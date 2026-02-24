use agent_proto::{ExecRequest, ExecResponse, ExecStreamRequest, RpcRequest, RpcResponse, SystemInfoResponse};
use agent_proto::stream::{StreamChannel, StreamData, StreamEnd};
use std::time::Instant;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

/// Execute a command on this machine and return structured output.
pub async fn execute_command(id: String, request: ExecRequest) -> RpcResponse {
    let start = Instant::now();
    let timeout_ms = request.timeout_ms.unwrap_or(30_000);

    debug!(command = %request.command, sudo = ?request.sudo, "Executing command");

    // Build the command
    let mut cmd = if request.sudo.unwrap_or(false) {
        let mut c = Command::new("sudo");
        c.arg("-n") // non-interactive sudo
            .arg("sh")
            .arg("-c")
            .arg(&request.command);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(&request.command);
        c
    };

    // Set environment variables
    if let Some(ref env_vars) = request.env {
        for (k, v) in env_vars {
            cmd.env(k, v);
        }
    }

    // Execute with timeout
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        cmd.output(),
    )
    .await;

    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let exit_code = output.status.code().unwrap_or(-1);

            debug!(exit_code, duration_ms, "Command completed");

            RpcResponse::success(
                id,
                serde_json::to_value(ExecResponse {
                    stdout,
                    stderr,
                    exit_code,
                    duration_ms,
                })
                .unwrap_or_default(),
            )
        }
        Ok(Err(e)) => {
            error!("Command execution failed: {e}");
            RpcResponse::error(
                id,
                agent_proto::error_codes::COMMAND_FAILED,
                format!("Command execution failed: {e}"),
            )
        }
        Err(_) => {
            error!("Command timed out after {timeout_ms}ms");
            RpcResponse::error(
                id,
                agent_proto::error_codes::AGENT_TIMEOUT,
                format!("Command timed out after {timeout_ms}ms"),
            )
        }
    }
}

/// Handle system.info request — returns structured system information.
pub async fn handle_system_info(id: String) -> RpcResponse {
    let hostname = hostname::get()
        .map(|h: std::ffi::OsString| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".into());

    let os_version = sysinfo::System::os_version().unwrap_or_else(|| "unknown".into());
    let kernel = sysinfo::System::kernel_version().unwrap_or_else(|| "unknown".into());

    // Get uptime via sysinfo
    let uptime_secs = sysinfo::System::uptime();

    RpcResponse::success(
        id,
        serde_json::to_value(SystemInfoResponse {
            hostname,
            os_type: std::env::consts::OS.into(),
            os_version,
            kernel,
            arch: std::env::consts::ARCH.into(),
            uptime_secs,
        })
        .unwrap_or_default(),
    )
}

/// Handle exec.stream — run a command with real-time streaming output.
/// Sends StreamData notifications for stdout/stderr and StreamEnd when done.
pub async fn handle_exec_stream(
    id: String,
    params: serde_json::Value,
    tx: mpsc::Sender<String>,
) -> RpcResponse {
    let request: ExecStreamRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid params: {e}")),
    };

    let stream_id = request.stream_id.clone();

    debug!(command = %request.command, stream_id = %stream_id, "Starting streaming exec");

    // Build the command
    let mut cmd = if request.sudo.unwrap_or(false) {
        let mut c = Command::new("sudo");
        c.arg("-n").arg("sh").arg("-c").arg(&request.command);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(&request.command);
        c
    };

    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return RpcResponse::error(id, -1, format!("Failed to spawn command: {e}")),
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stream_id_clone = stream_id.clone();
    let tx_clone = tx.clone();

    // Spawn background task to read stdout/stderr and send as stream notifications
    tokio::spawn(async move {
        let mut handles = Vec::new();

        // Stdout reader
        if let Some(stdout) = stdout {
            let sid = stream_id_clone.clone();
            let tx = tx_clone.clone();
            handles.push(tokio::spawn(async move {
                let mut reader = tokio::io::BufReader::new(stdout);
                let mut buf = vec![0u8; 4096];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            let notification = RpcRequest::notification(
                                "stream.data",
                                serde_json::to_value(StreamData {
                                    stream_id: sid.clone(),
                                    channel: StreamChannel::Stdout,
                                    data: base64::Engine::encode(
                                        &base64::engine::general_purpose::STANDARD,
                                        &buf[..n],
                                    ),
                                })
                                .unwrap_or_default(),
                            );
                            if tx
                                .send(serde_json::to_string(&notification).unwrap_or_default())
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(e) => {
                            warn!("Error reading stdout: {e}");
                            break;
                        }
                    }
                }
            }));
        }

        // Stderr reader
        if let Some(stderr) = stderr {
            let sid = stream_id_clone.clone();
            let tx = tx_clone.clone();
            handles.push(tokio::spawn(async move {
                let mut reader = tokio::io::BufReader::new(stderr);
                let mut buf = vec![0u8; 4096];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            let notification = RpcRequest::notification(
                                "stream.data",
                                serde_json::to_value(StreamData {
                                    stream_id: sid.clone(),
                                    channel: StreamChannel::Stderr,
                                    data: base64::Engine::encode(
                                        &base64::engine::general_purpose::STANDARD,
                                        &buf[..n],
                                    ),
                                })
                                .unwrap_or_default(),
                            );
                            if tx
                                .send(serde_json::to_string(&notification).unwrap_or_default())
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(e) => {
                            warn!("Error reading stderr: {e}");
                            break;
                        }
                    }
                }
            }));
        }

        // Wait for all readers to finish
        for handle in handles {
            let _ = handle.await;
        }

        // Wait for child exit
        let exit_code = match child.wait().await {
            Ok(status) => status.code().unwrap_or(-1),
            Err(e) => {
                warn!("Error waiting for child: {e}");
                -1
            }
        };

        // Send stream end notification
        let end_notification = RpcRequest::notification(
            "stream.end",
            serde_json::to_value(StreamEnd {
                stream_id: stream_id_clone,
                exit_code,
            })
            .unwrap_or_default(),
        );
        let _ = tx_clone
            .send(serde_json::to_string(&end_notification).unwrap_or_default())
            .await;
    });

    // Return immediately — output will stream via notifications
    RpcResponse::success(
        id,
        serde_json::json!({ "streaming": true, "stream_id": stream_id }),
    )
}
