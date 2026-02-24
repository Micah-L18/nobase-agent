//! File system operation handlers.
//!
//! Implements fs.list, fs.read, fs.write, fs.stat, fs.mkdir, fs.delete,
//! fs.rename, fs.search, fs.upload.*, and fs.download — replacing SFTP operations over SSH.
//!
//! Supports optional `sudo` mode for privileged operations (shells out to commands).

use agent_proto::*;
use agent_proto::stream::{StreamChannel, StreamData, StreamEnd};
use base64::Engine;
use dashmap::DashMap;
use sha2::{Digest, Sha256};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::LazyLock;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{debug, warn};

const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024; // 100 MB default limit
const DOWNLOAD_CHUNK_SIZE: usize = 64 * 1024; // 64 KB chunks for download streaming

/// In-flight upload transfers, keyed by transfer_id.
struct UploadTransfer {
    path: String,
    file: tokio::fs::File,
    total_size: u64,
    received: u64,
    hasher: Sha256,
    sudo: bool,
}

static UPLOADS: LazyLock<DashMap<String, UploadTransfer>> = LazyLock::new(DashMap::new);

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Convert a Unix mode to a permission string like "rwxr-xr-x".
fn mode_to_string(mode: u32) -> String {
    let mut s = String::with_capacity(10);
    // File type prefix
    let ft = mode & libc::S_IFMT as u32;
    if ft == libc::S_IFDIR as u32 {
        s.push('d');
    } else if ft == libc::S_IFLNK as u32 {
        s.push('l');
    } else {
        s.push('-');
    }
    // User
    s.push(if mode & 0o400 != 0 { 'r' } else { '-' });
    s.push(if mode & 0o200 != 0 { 'w' } else { '-' });
    s.push(if mode & 0o100 != 0 { 'x' } else { '-' });
    // Group
    s.push(if mode & 0o040 != 0 { 'r' } else { '-' });
    s.push(if mode & 0o020 != 0 { 'w' } else { '-' });
    s.push(if mode & 0o010 != 0 { 'x' } else { '-' });
    // Others
    s.push(if mode & 0o004 != 0 { 'r' } else { '-' });
    s.push(if mode & 0o002 != 0 { 'w' } else { '-' });
    s.push(if mode & 0o001 != 0 { 'x' } else { '-' });
    s
}

/// Convert a UID to a username (or stringify the UID).
fn uid_to_name(uid: u32) -> String {
    unsafe {
        let pw = libc::getpwuid(uid);
        if !pw.is_null() {
            std::ffi::CStr::from_ptr((*pw).pw_name)
                .to_string_lossy()
                .into_owned()
        } else {
            uid.to_string()
        }
    }
}

/// Convert a GID to a group name (or stringify the GID).
fn gid_to_name(gid: u32) -> String {
    unsafe {
        let gr = libc::getgrgid(gid);
        if !gr.is_null() {
            std::ffi::CStr::from_ptr((*gr).gr_name)
                .to_string_lossy()
                .into_owned()
        } else {
            gid.to_string()
        }
    }
}

/// Build an FsEntry from metadata and file name.
fn metadata_to_entry(name: &str, meta: &std::fs::Metadata) -> FsEntry {
    let ft = meta.file_type();
    let entry_type = if ft.is_dir() {
        FsEntryType::Directory
    } else if ft.is_symlink() {
        FsEntryType::Symlink
    } else {
        FsEntryType::File
    };

    let modified = meta
        .modified()
        .ok()
        .and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| {
                    chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                        .map(|dt| dt.to_rfc3339())
                        .unwrap_or_default()
                })
        })
        .unwrap_or_default();

    FsEntry {
        name: name.to_string(),
        entry_type,
        size: meta.len(),
        permissions: mode_to_string(meta.mode()),
        modified,
        owner: uid_to_name(meta.uid()),
        group: gid_to_name(meta.gid()),
    }
}

// ─── Handlers ───────────────────────────────────────────────────────────────

/// List directory contents.
pub async fn handle_fs_list(id: String, params: serde_json::Value) -> RpcResponse {
    let req: FsListRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid params: {e}")),
    };

    debug!(path = %req.path, sudo = ?req.sudo, "fs.list");

    // Sudo mode: use `ls` command
    if req.sudo.unwrap_or(false) {
        return handle_fs_list_sudo(id, &req.path).await;
    }

    let path = Path::new(&req.path);
    let mut entries = Vec::new();

    let mut dir = match fs::read_dir(path).await {
        Ok(d) => d,
        Err(e) => {
            return RpcResponse::error(
                id,
                error_codes::COMMAND_FAILED,
                format!("Cannot read directory '{}': {e}", req.path),
            )
        }
    };

    while let Ok(Some(entry)) = dir.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        // Use symlink_metadata to not follow symlinks
        match entry.metadata().await {
            Ok(meta) => entries.push(metadata_to_entry(&name, &meta)),
            Err(e) => {
                debug!("Skipping entry {name}: {e}");
            }
        }
    }

    // Sort: directories first, then alphabetically
    entries.sort_by(|a, b| {
        let a_dir = matches!(a.entry_type, FsEntryType::Directory);
        let b_dir = matches!(b.entry_type, FsEntryType::Directory);
        b_dir.cmp(&a_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    RpcResponse::success(
        id,
        serde_json::to_value(FsListResponse { entries }).unwrap_or_default(),
    )
}

/// fs.list with sudo (parses `ls -la` output).
async fn handle_fs_list_sudo(id: String, path: &str) -> RpcResponse {
    let output = tokio::process::Command::new("sudo")
        .arg("ls")
        .arg("-la")
        .arg("--time-style=full-iso")
        .arg(path)
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let mut entries = Vec::new();

            for line in stdout.lines().skip(1) {
                // skip "total N" line
                let parts: Vec<&str> = line.splitn(9, char::is_whitespace).collect();
                if parts.len() < 9 {
                    continue;
                }
                let name = parts[8].to_string();
                if name == "." || name == ".." {
                    continue;
                }

                let perms = parts[0].to_string();
                let entry_type = if perms.starts_with('d') {
                    FsEntryType::Directory
                } else if perms.starts_with('l') {
                    FsEntryType::Symlink
                } else {
                    FsEntryType::File
                };

                entries.push(FsEntry {
                    name,
                    entry_type,
                    size: parts[4].parse().unwrap_or(0),
                    permissions: perms,
                    modified: format!("{} {}", parts[5], parts[6]),
                    owner: parts[2].to_string(),
                    group: parts[3].to_string(),
                });
            }

            RpcResponse::success(
                id,
                serde_json::to_value(FsListResponse { entries }).unwrap_or_default(),
            )
        }
        Ok(out) => RpcResponse::error(
            id,
            error_codes::COMMAND_FAILED,
            format!("ls failed: {}", String::from_utf8_lossy(&out.stderr)),
        ),
        Err(e) => RpcResponse::error(id, error_codes::COMMAND_FAILED, format!("exec failed: {e}")),
    }
}

/// Read a file.
pub async fn handle_fs_read(id: String, params: serde_json::Value) -> RpcResponse {
    let req: FsReadRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid params: {e}")),
    };

    debug!(path = %req.path, "fs.read");

    let encoding = req.encoding.unwrap_or_default();

    // Sudo mode
    if req.sudo.unwrap_or(false) {
        return handle_fs_read_sudo(id, &req.path, encoding).await;
    }

    // Check file size first
    let meta = match fs::metadata(&req.path).await {
        Ok(m) => m,
        Err(e) => {
            return RpcResponse::error(
                id,
                error_codes::COMMAND_FAILED,
                format!("Cannot stat '{}': {e}", req.path),
            )
        }
    };

    if meta.len() > MAX_FILE_SIZE {
        return RpcResponse::error(
            id,
            error_codes::COMMAND_FAILED,
            format!("File too large: {} bytes (max {})", meta.len(), MAX_FILE_SIZE),
        );
    }

    let data = match fs::read(&req.path).await {
        Ok(d) => d,
        Err(e) => {
            return RpcResponse::error(
                id,
                error_codes::COMMAND_FAILED,
                format!("Cannot read '{}': {e}", req.path),
            )
        }
    };

    let (content, used_encoding) = match encoding {
        FsEncoding::Utf8 => {
            // Try UTF-8, fall back to base64
            match String::from_utf8(data.clone()) {
                Ok(s) => (s, FsEncoding::Utf8),
                Err(_) => (
                    base64::engine::general_purpose::STANDARD.encode(&data),
                    FsEncoding::Base64,
                ),
            }
        }
        FsEncoding::Base64 => (
            base64::engine::general_purpose::STANDARD.encode(&data),
            FsEncoding::Base64,
        ),
    };

    RpcResponse::success(
        id,
        serde_json::to_value(FsReadResponse {
            content,
            encoding: used_encoding,
            size: meta.len(),
        })
        .unwrap_or_default(),
    )
}

/// fs.read with sudo.
async fn handle_fs_read_sudo(id: String, path: &str, encoding: FsEncoding) -> RpcResponse {
    let output = tokio::process::Command::new("sudo")
        .arg("cat")
        .arg(path)
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => {
            let size = out.stdout.len() as u64;
            let (content, used_encoding) = match encoding {
                FsEncoding::Utf8 => match String::from_utf8(out.stdout.clone()) {
                    Ok(s) => (s, FsEncoding::Utf8),
                    Err(_) => (
                        base64::engine::general_purpose::STANDARD.encode(&out.stdout),
                        FsEncoding::Base64,
                    ),
                },
                FsEncoding::Base64 => (
                    base64::engine::general_purpose::STANDARD.encode(&out.stdout),
                    FsEncoding::Base64,
                ),
            };

            RpcResponse::success(
                id,
                serde_json::to_value(FsReadResponse {
                    content,
                    encoding: used_encoding,
                    size,
                })
                .unwrap_or_default(),
            )
        }
        Ok(out) => RpcResponse::error(
            id,
            error_codes::COMMAND_FAILED,
            format!("sudo cat failed: {}", String::from_utf8_lossy(&out.stderr)),
        ),
        Err(e) => RpcResponse::error(id, error_codes::COMMAND_FAILED, format!("exec failed: {e}")),
    }
}

/// Write a file.
pub async fn handle_fs_write(id: String, params: serde_json::Value) -> RpcResponse {
    let req: FsWriteRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid params: {e}")),
    };

    debug!(path = %req.path, "fs.write");

    let encoding = req.encoding.unwrap_or_default();
    let data = match encoding {
        FsEncoding::Utf8 => req.content.into_bytes(),
        FsEncoding::Base64 => match base64::engine::general_purpose::STANDARD.decode(&req.content) {
            Ok(d) => d,
            Err(e) => return RpcResponse::error(id, -32602, format!("Invalid base64: {e}")),
        },
    };

    if data.len() as u64 > MAX_FILE_SIZE {
        return RpcResponse::error(
            id,
            error_codes::COMMAND_FAILED,
            format!("Data too large: {} bytes (max {})", data.len(), MAX_FILE_SIZE),
        );
    }

    // Sudo mode: write via tee
    if req.sudo.unwrap_or(false) {
        let mut child = match tokio::process::Command::new("sudo")
            .arg("tee")
            .arg(&req.path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                return RpcResponse::error(
                    id,
                    error_codes::COMMAND_FAILED,
                    format!("Failed to spawn sudo tee: {e}"),
                )
            }
        };

        if let Some(ref mut stdin) = child.stdin {
            use tokio::io::AsyncWriteExt;
            if let Err(e) = stdin.write_all(&data).await {
                return RpcResponse::error(
                    id,
                    error_codes::COMMAND_FAILED,
                    format!("Write failed: {e}"),
                );
            }
        }
        drop(child.stdin.take());

        let status = child.wait().await;
        if let Ok(s) = &status {
            if !s.success() {
                return RpcResponse::error(
                    id,
                    error_codes::COMMAND_FAILED,
                    format!("sudo tee exited with code {}", s.code().unwrap_or(-1)),
                );
            }
        }

        // Set file mode if specified
        if let Some(ref mode_str) = req.mode {
            let _ = tokio::process::Command::new("sudo")
                .arg("chmod")
                .arg(mode_str)
                .arg(&req.path)
                .status()
                .await;
        }

        return RpcResponse::success(id, serde_json::json!({ "ok": true, "size": data.len() }));
    }

    // Normal write
    if let Err(e) = fs::write(&req.path, &data).await {
        return RpcResponse::error(
            id,
            error_codes::COMMAND_FAILED,
            format!("Cannot write '{}': {e}", req.path),
        );
    }

    // Set file mode if specified
    if let Some(ref mode_str) = req.mode {
        if let Ok(mode) = u32::from_str_radix(mode_str, 8) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&req.path, std::fs::Permissions::from_mode(mode)).await;
        }
    }

    RpcResponse::success(id, serde_json::json!({ "ok": true, "size": data.len() }))
}

/// Get file/directory stats.
pub async fn handle_fs_stat(id: String, params: serde_json::Value) -> RpcResponse {
    let req: FsStatRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid params: {e}")),
    };

    debug!(path = %req.path, "fs.stat");

    // Sudo mode
    if req.sudo.unwrap_or(false) {
        let output = tokio::process::Command::new("sudo")
            .arg("stat")
            .arg("--format=%F|%s|%a|%U|%G|%Y")
            .arg(&req.path)
            .output()
            .await;

        return match output {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let parts: Vec<&str> = stdout.trim().split('|').collect();
                if parts.len() >= 6 {
                    let entry_type = if parts[0].contains("directory") {
                        FsEntryType::Directory
                    } else if parts[0].contains("link") {
                        FsEntryType::Symlink
                    } else {
                        FsEntryType::File
                    };

                    let name = Path::new(&req.path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();

                    RpcResponse::success(
                        id,
                        serde_json::to_value(FsStatResponse {
                            exists: true,
                            entry: FsEntry {
                                name,
                                entry_type,
                                size: parts[1].parse().unwrap_or(0),
                                permissions: parts[2].to_string(),
                                modified: parts[5].to_string(),
                                owner: parts[3].to_string(),
                                group: parts[4].to_string(),
                            },
                        })
                        .unwrap_or_default(),
                    )
                } else {
                    RpcResponse::error(id, error_codes::COMMAND_FAILED, "Failed to parse stat output")
                }
            }
            Ok(_) => RpcResponse::success(
                id,
                serde_json::to_value(FsStatResponse {
                    exists: false,
                    entry: FsEntry {
                        name: String::new(),
                        entry_type: FsEntryType::File,
                        size: 0,
                        permissions: String::new(),
                        modified: String::new(),
                        owner: String::new(),
                        group: String::new(),
                    },
                })
                .unwrap_or_default(),
            ),
            Err(e) => {
                RpcResponse::error(id, error_codes::COMMAND_FAILED, format!("exec failed: {e}"))
            }
        };
    }

    let path = Path::new(&req.path);
    match fs::symlink_metadata(path).await {
        Ok(meta) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            RpcResponse::success(
                id,
                serde_json::to_value(FsStatResponse {
                    exists: true,
                    entry: metadata_to_entry(&name, &meta),
                })
                .unwrap_or_default(),
            )
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => RpcResponse::success(
            id,
            serde_json::to_value(FsStatResponse {
                exists: false,
                entry: FsEntry {
                    name: String::new(),
                    entry_type: FsEntryType::File,
                    size: 0,
                    permissions: String::new(),
                    modified: String::new(),
                    owner: String::new(),
                    group: String::new(),
                },
            })
            .unwrap_or_default(),
        ),
        Err(e) => RpcResponse::error(
            id,
            error_codes::COMMAND_FAILED,
            format!("Cannot stat '{}': {e}", req.path),
        ),
    }
}

/// Create a directory.
pub async fn handle_fs_mkdir(id: String, params: serde_json::Value) -> RpcResponse {
    let req: FsMkdirRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid params: {e}")),
    };

    debug!(path = %req.path, "fs.mkdir");

    if req.sudo.unwrap_or(false) {
        let mut cmd = tokio::process::Command::new("sudo");
        cmd.arg("mkdir");
        if req.recursive.unwrap_or(false) {
            cmd.arg("-p");
        }
        cmd.arg(&req.path);

        match cmd.status().await {
            Ok(s) if s.success() => {
                RpcResponse::success(id, serde_json::json!({ "ok": true }))
            }
            Ok(s) => RpcResponse::error(
                id,
                error_codes::COMMAND_FAILED,
                format!("mkdir failed (exit {})", s.code().unwrap_or(-1)),
            ),
            Err(e) => {
                RpcResponse::error(id, error_codes::COMMAND_FAILED, format!("exec failed: {e}"))
            }
        }
    } else {
        let result = if req.recursive.unwrap_or(false) {
            fs::create_dir_all(&req.path).await
        } else {
            fs::create_dir(&req.path).await
        };

        match result {
            Ok(()) => RpcResponse::success(id, serde_json::json!({ "ok": true })),
            Err(e) => RpcResponse::error(
                id,
                error_codes::COMMAND_FAILED,
                format!("Cannot create '{}': {e}", req.path),
            ),
        }
    }
}

/// Delete a file or directory.
pub async fn handle_fs_delete(id: String, params: serde_json::Value) -> RpcResponse {
    let req: FsDeleteRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid params: {e}")),
    };

    debug!(path = %req.path, recursive = ?req.recursive, "fs.delete");

    if req.sudo.unwrap_or(false) {
        let mut cmd = tokio::process::Command::new("sudo");
        cmd.arg("rm");
        if req.recursive.unwrap_or(false) {
            cmd.arg("-rf");
        } else {
            cmd.arg("-f");
        }
        cmd.arg(&req.path);

        match cmd.status().await {
            Ok(s) if s.success() => {
                RpcResponse::success(id, serde_json::json!({ "ok": true }))
            }
            Ok(s) => RpcResponse::error(
                id,
                error_codes::COMMAND_FAILED,
                format!("rm failed (exit {})", s.code().unwrap_or(-1)),
            ),
            Err(e) => {
                RpcResponse::error(id, error_codes::COMMAND_FAILED, format!("exec failed: {e}"))
            }
        }
    } else {
        let path = Path::new(&req.path);
        let meta = fs::symlink_metadata(path).await;

        let result = match meta {
            Ok(m) if m.is_dir() && req.recursive.unwrap_or(false) => {
                fs::remove_dir_all(path).await
            }
            Ok(m) if m.is_dir() => fs::remove_dir(path).await,
            Ok(_) => fs::remove_file(path).await,
            Err(e) => Err(e),
        };

        match result {
            Ok(()) => RpcResponse::success(id, serde_json::json!({ "ok": true })),
            Err(e) => RpcResponse::error(
                id,
                error_codes::COMMAND_FAILED,
                format!("Cannot delete '{}': {e}", req.path),
            ),
        }
    }
}

/// Rename/move a file or directory.
pub async fn handle_fs_rename(id: String, params: serde_json::Value) -> RpcResponse {
    let req: FsRenameRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid params: {e}")),
    };

    debug!(from = %req.old_path, to = %req.new_path, "fs.rename");

    if req.sudo.unwrap_or(false) {
        let status = tokio::process::Command::new("sudo")
            .arg("mv")
            .arg(&req.old_path)
            .arg(&req.new_path)
            .status()
            .await;

        match status {
            Ok(s) if s.success() => {
                RpcResponse::success(id, serde_json::json!({ "ok": true }))
            }
            Ok(s) => RpcResponse::error(
                id,
                error_codes::COMMAND_FAILED,
                format!("mv failed (exit {})", s.code().unwrap_or(-1)),
            ),
            Err(e) => {
                RpcResponse::error(id, error_codes::COMMAND_FAILED, format!("exec failed: {e}"))
            }
        }
    } else {
        match fs::rename(&req.old_path, &req.new_path).await {
            Ok(()) => RpcResponse::success(id, serde_json::json!({ "ok": true })),
            Err(e) => RpcResponse::error(
                id,
                error_codes::COMMAND_FAILED,
                format!("Cannot rename '{}' → '{}': {e}", req.old_path, req.new_path),
            ),
        }
    }
}

/// Search for files matching a pattern using `find`.
pub async fn handle_fs_search(id: String, params: serde_json::Value) -> RpcResponse {
    let req: FsSearchRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid params: {e}")),
    };

    debug!(path = %req.path, pattern = %req.pattern, "fs.search");

    let mut cmd = tokio::process::Command::new("find");
    cmd.arg(&req.path);

    if let Some(depth) = req.max_depth {
        cmd.arg("-maxdepth").arg(depth.to_string());
    }

    cmd.arg("-name").arg(&req.pattern);

    match cmd.output().await {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let matches: Vec<String> = stdout
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| l.to_string())
                .collect();

            RpcResponse::success(
                id,
                serde_json::to_value(FsSearchResponse { matches }).unwrap_or_default(),
            )
        }
        Ok(out) => {
            // find returns non-zero for permission errors but may still have results
            let stdout = String::from_utf8_lossy(&out.stdout);
            let matches: Vec<String> = stdout
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| l.to_string())
                .collect();

            if !matches.is_empty() {
                RpcResponse::success(
                    id,
                    serde_json::to_value(FsSearchResponse { matches }).unwrap_or_default(),
                )
            } else {
                RpcResponse::error(
                    id,
                    error_codes::COMMAND_FAILED,
                    format!("find failed: {}", String::from_utf8_lossy(&out.stderr)),
                )
            }
        }
        Err(e) => {
            RpcResponse::error(id, error_codes::COMMAND_FAILED, format!("exec failed: {e}"))
        }
    }
}

// ─── Chunked Upload Handlers ────────────────────────────────────────────────

/// Handle `fs.upload.start` — begin a chunked file upload.
/// Creates the target file and stores the transfer state.
pub async fn handle_fs_upload_start(id: String, params: serde_json::Value) -> RpcResponse {
    let req: FsUploadStartRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid params: {e}")),
    };

    let sudo = req.sudo.unwrap_or(false);

    debug!(transfer_id = %req.transfer_id, path = %req.path, size = req.total_size, "Upload start");

    // If sudo mode, write to a temp file first then move with sudo
    let write_path = if sudo {
        format!("/tmp/.qd-upload-{}", req.transfer_id)
    } else {
        req.path.clone()
    };

    // Ensure parent directory exists
    if let Some(parent) = Path::new(&write_path).parent() {
        if !parent.exists() {
            if let Err(e) = fs::create_dir_all(parent).await {
                return RpcResponse::error(id, error_codes::COMMAND_FAILED,
                    format!("Cannot create parent directory: {e}"));
            }
        }
    }

    let file = match fs::File::create(&write_path).await {
        Ok(f) => f,
        Err(e) => return RpcResponse::error(id, error_codes::COMMAND_FAILED,
            format!("Cannot create file {}: {e}", write_path)),
    };

    let transfer = UploadTransfer {
        path: req.path,
        file,
        total_size: req.total_size,
        received: 0,
        hasher: Sha256::new(),
        sudo,
    };

    UPLOADS.insert(req.transfer_id.clone(), transfer);

    RpcResponse::success(id, serde_json::json!({ "ok": true, "transfer_id": req.transfer_id }))
}

/// Handle `fs.upload.chunk` — receive a chunk of data for an in-progress upload.
pub async fn handle_fs_upload_chunk(id: String, params: serde_json::Value) -> RpcResponse {
    let req: FsUploadChunkRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid params: {e}")),
    };

    let data = match base64::engine::general_purpose::STANDARD.decode(&req.data) {
        Ok(d) => d,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid base64 data: {e}")),
    };

    let mut transfer = match UPLOADS.get_mut(&req.transfer_id) {
        Some(t) => t,
        None => return RpcResponse::error(id, error_codes::COMMAND_FAILED,
            format!("No active upload for transfer_id: {}", req.transfer_id)),
    };

    // Seek to offset and write
    if let Err(e) = transfer.file.seek(std::io::SeekFrom::Start(req.offset)).await {
        return RpcResponse::error(id, error_codes::COMMAND_FAILED, format!("Seek error: {e}"));
    }

    if let Err(e) = transfer.file.write_all(&data).await {
        return RpcResponse::error(id, error_codes::COMMAND_FAILED, format!("Write error: {e}"));
    }

    transfer.hasher.update(&data);
    transfer.received += data.len() as u64;

    RpcResponse::success(id, serde_json::json!({
        "ok": true,
        "received": transfer.received,
        "total": transfer.total_size,
    }))
}

/// Handle `fs.upload.end` — finalize a chunked upload, verify checksum.
pub async fn handle_fs_upload_end(id: String, params: serde_json::Value) -> RpcResponse {
    let req: FsUploadEndRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid params: {e}")),
    };

    let (_, mut transfer) = match UPLOADS.remove(&req.transfer_id) {
        Some(t) => t,
        None => return RpcResponse::error(id, error_codes::COMMAND_FAILED,
            format!("No active upload for transfer_id: {}", req.transfer_id)),
    };

    // Flush and sync file
    if let Err(e) = transfer.file.flush().await {
        return RpcResponse::error(id, error_codes::COMMAND_FAILED, format!("Flush error: {e}"));
    }
    if let Err(e) = transfer.file.sync_all().await {
        return RpcResponse::error(id, error_codes::COMMAND_FAILED, format!("Sync error: {e}"));
    }
    drop(transfer.file);

    // Verify checksum
    let actual_hash = format!("{:x}", transfer.hasher.finalize());
    if actual_hash != req.checksum_sha256 {
        // Clean up the file on checksum mismatch
        if transfer.sudo {
            let tmp = format!("/tmp/.qd-upload-{}", req.transfer_id);
            let _ = fs::remove_file(&tmp).await;
        } else {
            let _ = fs::remove_file(&transfer.path).await;
        }
        return RpcResponse::error(id, error_codes::COMMAND_FAILED,
            format!("Checksum mismatch: expected {}, got {}", req.checksum_sha256, actual_hash));
    }

    // If sudo mode, move from tmp to final path
    if transfer.sudo {
        let tmp = format!("/tmp/.qd-upload-{}", req.transfer_id);
        let output = tokio::process::Command::new("sudo")
            .arg("-n").arg("mv").arg(&tmp).arg(&transfer.path)
            .output().await;

        match output {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let _ = fs::remove_file(&tmp).await;
                return RpcResponse::error(id, error_codes::COMMAND_FAILED,
                    format!("sudo mv failed: {}", String::from_utf8_lossy(&o.stderr)));
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp).await;
                return RpcResponse::error(id, error_codes::COMMAND_FAILED,
                    format!("Failed to run sudo mv: {e}"));
            }
        }
    }

    debug!(path = %transfer.path, size = transfer.received, "Upload complete");

    RpcResponse::success(id, serde_json::json!({
        "ok": true,
        "path": transfer.path,
        "size": transfer.received,
        "checksum": actual_hash,
    }))
}

// ─── Chunked Download Handler ───────────────────────────────────────────────

/// Handle `fs.download` — stream a file back in chunks via stream.data notifications.
pub async fn handle_fs_download(
    id: String,
    params: serde_json::Value,
    tx: mpsc::Sender<String>,
) -> RpcResponse {
    let req: FsDownloadRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid params: {e}")),
    };

    let sudo = req.sudo.unwrap_or(false);
    let transfer_id = req.transfer_id.clone();
    let path = req.path.clone();

    debug!(transfer_id = %transfer_id, path = %path, "Download start");

    // Get file size first
    let file_size: u64;
    if sudo {
        let output = tokio::process::Command::new("sudo")
            .arg("-n").arg("stat").arg("--format=%s").arg(&path)
            .output().await;
        match output {
            Ok(o) if o.status.success() => {
                file_size = String::from_utf8_lossy(&o.stdout)
                    .trim().parse().unwrap_or(0);
            }
            Ok(o) => return RpcResponse::error(id, error_codes::COMMAND_FAILED,
                format!("stat failed: {}", String::from_utf8_lossy(&o.stderr))),
            Err(e) => return RpcResponse::error(id, error_codes::COMMAND_FAILED,
                format!("Failed to stat file: {e}")),
        }
    } else {
        match fs::metadata(&path).await {
            Ok(m) => file_size = m.len(),
            Err(e) => return RpcResponse::error(id, error_codes::COMMAND_FAILED,
                format!("File not found: {e}")),
        }
    }

    // Spawn background task to stream the file
    let tx_clone = tx;
    let transfer_id_clone = transfer_id.clone();
    tokio::spawn(async move {
        let result = if sudo {
            stream_file_sudo(&path, &transfer_id_clone, &tx_clone).await
        } else {
            stream_file_direct(&path, &transfer_id_clone, &tx_clone).await
        };

        let exit_code = match result {
            Ok(_) => 0,
            Err(e) => {
                warn!("Download stream error: {e}");
                -1
            }
        };

        let end = RpcRequest::notification(
            "stream.end",
            serde_json::to_value(StreamEnd {
                stream_id: transfer_id_clone,
                exit_code,
            }).unwrap_or_default(),
        );
        let _ = tx_clone.send(serde_json::to_string(&end).unwrap_or_default()).await;
    });

    RpcResponse::success(id, serde_json::json!({
        "streaming": true,
        "transfer_id": transfer_id,
        "size": file_size,
    }))
}

/// Stream file content directly (no sudo).
async fn stream_file_direct(
    path: &str,
    transfer_id: &str,
    tx: &mpsc::Sender<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut file = fs::File::open(path).await?;
    let mut buf = vec![0u8; DOWNLOAD_CHUNK_SIZE];

    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 { break; }

        let notification = RpcRequest::notification(
            "stream.data",
            serde_json::to_value(StreamData {
                stream_id: transfer_id.to_string(),
                channel: StreamChannel::Stdout,
                data: base64::engine::general_purpose::STANDARD.encode(&buf[..n]),
            }).unwrap_or_default(),
        );
        tx.send(serde_json::to_string(&notification)?).await
            .map_err(|e| format!("Send error: {e}"))?;
    }
    Ok(())
}

/// Stream file content via `sudo cat` for privileged reads.
async fn stream_file_sudo(
    path: &str,
    transfer_id: &str,
    tx: &mpsc::Sender<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut child = tokio::process::Command::new("sudo")
        .arg("-n").arg("cat").arg(path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let mut stdout = child.stdout.take()
        .ok_or("Failed to capture stdout")?;
    let mut buf = vec![0u8; DOWNLOAD_CHUNK_SIZE];

    loop {
        let n = stdout.read(&mut buf).await?;
        if n == 0 { break; }

        let notification = RpcRequest::notification(
            "stream.data",
            serde_json::to_value(StreamData {
                stream_id: transfer_id.to_string(),
                channel: StreamChannel::Stdout,
                data: base64::engine::general_purpose::STANDARD.encode(&buf[..n]),
            }).unwrap_or_default(),
        );
        tx.send(serde_json::to_string(&notification)?).await
            .map_err(|e| format!("Send error: {e}"))?;
    }

    let status = child.wait().await?;
    if !status.success() {
        return Err(format!("sudo cat exited with code {}", status.code().unwrap_or(-1)).into());
    }
    Ok(())
}
