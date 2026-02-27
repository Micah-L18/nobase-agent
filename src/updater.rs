//! Self-update logic for the NoBase Agent.
//!
//! Downloads the latest binary from the backend, verifies its SHA-256 checksum,
//! atomically replaces the running binary, and restarts the systemd service.
//!
//! Used by:
//!   - `nobase update` CLI command
//!   - `self_update` RPC handler (backend-triggered push)
//!   - Automatic update check on reconnect to gateway

use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use tracing::{debug, info, warn};

const BINARY_PATH: &str = "/usr/local/bin/nobase";
const TMP_PATH: &str = "/tmp/nobase-update";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Result of an update check.
#[derive(Debug)]
pub enum UpdateResult {
    /// Binary already matches the latest checksum.
    AlreadyUpToDate,
    /// Binary was updated and the service is restarting.
    Updated {
        old_hash: String,
        new_hash: String,
    },
    /// backend_url is not configured — cannot check for updates.
    NotConfigured,
}

impl std::fmt::Display for UpdateResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyUpToDate => write!(f, "Already up-to-date (v{VERSION})"),
            Self::Updated { new_hash, .. } => write!(f, "Updated to {}", &new_hash[..12]),
            Self::NotConfigured => write!(f, "backend_url not configured, skipping update check"),
        }
    }
}

/// Check for and apply an update from the backend.
///
/// This is the core update logic shared between the CLI command, RPC handler,
/// and automatic reconnect check.
///
/// - `backend_url`: HTTP base URL of the backend (e.g. "http://192.168.0.247:3044")
/// - `restart_service`: Whether to run `systemctl restart nobase` after replacing
///   the binary. Set to `true` for RPC/reconnect (agent is running as a service),
///   `false` if the caller handles restart itself.
///
/// Returns `UpdateResult` describing what happened.
pub async fn check_and_update(
    backend_url: &str,
    restart_service: bool,
) -> Result<UpdateResult, Box<dyn std::error::Error + Send + Sync>> {
    if backend_url.is_empty() {
        return Ok(UpdateResult::NotConfigured);
    }

    let backend_url = backend_url.trim_end_matches('/');

    // Detect platform
    let arch = std::env::consts::ARCH;
    let rust_arch = match arch {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => return Err(format!("Unsupported architecture: {arch}").into()),
    };
    let platform = format!("linux-{rust_arch}");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    // Step 1: Fetch expected checksum from backend
    let checksum_url = format!("{backend_url}/api/agents/binary/{platform}/checksum");
    debug!("Fetching checksum from {checksum_url}");
    let checksum_resp = client.get(&checksum_url).send().await?;
    if checksum_resp.status() == reqwest::StatusCode::NOT_FOUND {
        // No binary available on the backend — skip update silently
        debug!("No binary available for {platform} on backend (404)");
        return Ok(UpdateResult::AlreadyUpToDate);
    }
    if !checksum_resp.status().is_success() {
        return Err(format!(
            "Failed to fetch checksum (HTTP {})",
            checksum_resp.status()
        )
        .into());
    }
    let checksum_json: serde_json::Value = checksum_resp.json().await?;
    let expected_sha256 = checksum_json["sha256"]
        .as_str()
        .ok_or("Invalid checksum response from backend")?
        .to_string();

    // Step 2: Hash current binary and compare
    let current_hash = if std::path::Path::new(BINARY_PATH).exists() {
        let current_bytes = tokio::fs::read(BINARY_PATH).await?;
        let mut hasher = Sha256::new();
        hasher.update(&current_bytes);
        format!("{:x}", hasher.finalize())
    } else {
        String::new()
    };

    if current_hash == expected_sha256 {
        debug!("Binary checksum matches — already up-to-date");
        return Ok(UpdateResult::AlreadyUpToDate);
    }

    info!(
        "Update available: current={} expected={}",
        &current_hash[..current_hash.len().min(12)],
        &expected_sha256[..expected_sha256.len().min(12)]
    );

    // Step 3: Download new binary
    let binary_url = format!("{backend_url}/api/agents/binary/{platform}");
    info!("Downloading new binary from {binary_url}");
    let binary_resp = client.get(&binary_url).send().await?;
    if !binary_resp.status().is_success() {
        return Err(
            format!("Failed to download binary (HTTP {})", binary_resp.status()).into(),
        );
    }
    let new_binary = binary_resp.bytes().await?;
    let size_mb = new_binary.len() as f64 / 1_048_576.0;
    info!("Downloaded {size_mb:.1} MB");

    // Step 4: Verify checksum
    let mut hasher = Sha256::new();
    hasher.update(&new_binary);
    let actual_sha256 = format!("{:x}", hasher.finalize());
    if actual_sha256 != expected_sha256 {
        return Err(format!(
            "Checksum mismatch! expected={expected_sha256} actual={actual_sha256}"
        )
        .into());
    }
    info!("SHA-256 verified");

    // Step 5: Atomic replace — write to tmp, set permissions, move over current
    // rename() fails across filesystems (EXDEV: /tmp → /usr/local/bin), so
    // we fall back to copy + remove when that happens.
    tokio::fs::write(TMP_PATH, &new_binary).await?;
    tokio::fs::set_permissions(TMP_PATH, std::fs::Permissions::from_mode(0o755)).await?;
    if tokio::fs::rename(TMP_PATH, BINARY_PATH).await.is_err() {
        tokio::fs::copy(TMP_PATH, BINARY_PATH).await?;
        tokio::fs::set_permissions(BINARY_PATH, std::fs::Permissions::from_mode(0o755)).await?;
        let _ = tokio::fs::remove_file(TMP_PATH).await;
    }
    info!("Binary replaced at {BINARY_PATH}");

    // Step 6: Restart service if requested
    if restart_service {
        info!("Restarting nobase service...");
        match tokio::process::Command::new("systemctl")
            .args(["restart", "nobase"])
            .status()
            .await
        {
            Ok(s) if s.success() => info!("Service restarted successfully"),
            Ok(s) => warn!("systemctl restart exited with {s}"),
            Err(e) => warn!("Could not restart service: {e}"),
        }
    }

    Ok(UpdateResult::Updated {
        old_hash: current_hash,
        new_hash: actual_sha256,
    })
}
