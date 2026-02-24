//! Interactive shell (PTY) session handler.
//!
//! Uses Unix PTY (openpty) to create pseudo-terminal sessions.
//! Each session spawns a login shell and streams I/O via base64-encoded
//! WebSocket messages through the gateway.

use agent_proto::{RpcResponse, ShellCloseRequest, ShellDataRequest, ShellOpenRequest, ShellResizeRequest};
use base64::Engine;
use dashmap::DashMap;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc;
use tracing::{debug, info};

/// Active shell session.
struct ShellSession {
    master_fd: i32,
    child_pid: u32,
    shutdown: Arc<AtomicBool>,
}

fn sessions() -> &'static DashMap<String, ShellSession> {
    static SESSIONS: OnceLock<DashMap<String, ShellSession>> = OnceLock::new();
    SESSIONS.get_or_init(DashMap::new)
}

/// Open a new PTY shell session.
pub async fn handle_shell_open(
    id: String,
    params: serde_json::Value,
    tx: mpsc::Sender<String>,
) -> RpcResponse {
    let req: ShellOpenRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid params: {e}")),
    };

    if sessions().contains_key(&req.shell_id) {
        return RpcResponse::error(id, -32001, format!("Shell already exists: {}", req.shell_id));
    }

    match create_pty_session(&req, tx).await {
        Ok(()) => RpcResponse::success(
            id,
            serde_json::json!({ "shell_id": req.shell_id, "ok": true }),
        ),
        Err(e) => RpcResponse::error(id, -32003, format!("Failed to create shell: {e}")),
    }
}

async fn create_pty_session(
    req: &ShellOpenRequest,
    tx: mpsc::Sender<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Set up initial window size
    let ws = libc::winsize {
        ws_row: req.rows,
        ws_col: req.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    // Open PTY pair
    let pty = nix::pty::openpty(Some(&ws), None)?;
    let master_raw = pty.master.into_raw_fd();
    let slave_raw = pty.slave.into_raw_fd();

    // Spawn login shell with slave as stdio
    let mut cmd = tokio::process::Command::new("/bin/bash");
    cmd.arg("-l");

    unsafe {
        let slave = slave_raw;
        cmd.stdin(Stdio::from_raw_fd(libc::dup(slave)));
        cmd.stdout(Stdio::from_raw_fd(libc::dup(slave)));
        cmd.stderr(Stdio::from_raw_fd(libc::dup(slave)));
        cmd.pre_exec(move || {
            // Create new session
            nix::unistd::setsid()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            // Set controlling terminal
            if libc::ioctl(slave, libc::TIOCSCTTY as _, 0i32) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    // Set environment variables
    if let Some(ref env) = req.env {
        for (k, v) in env {
            cmd.env(k, v);
        }
    }
    cmd.env("TERM", "xterm-256color");

    let mut child = cmd.spawn()?;
    let child_pid = child.id().unwrap_or(0);

    // Close slave fd in parent
    unsafe {
        libc::close(slave_raw);
    }

    // Spawn task to reap child process (prevents zombies)
    tokio::spawn(async move {
        let _ = child.wait().await;
        debug!("Shell child process {child_pid} exited");
    });

    // Dup master fd for the read thread
    let read_fd = unsafe { libc::dup(master_raw) };
    if read_fd < 0 {
        unsafe {
            libc::close(master_raw);
        }
        return Err("Failed to dup master fd".into());
    }

    let shell_id = req.shell_id.clone();
    let shell_id_clone = shell_id.clone();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();

    // Spawn a dedicated OS thread for blocking PTY reads.
    // Uses poll() with timeouts to allow periodic shutdown checks.
    std::thread::spawn(move || {
        let engine = base64::engine::general_purpose::STANDARD;
        let mut buf = [0u8; 8192];

        loop {
            if shutdown_clone.load(Ordering::Relaxed) {
                break;
            }

            // Poll for readable data with 500ms timeout
            let mut pollfd = libc::pollfd {
                fd: read_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ret = unsafe { libc::poll(&mut pollfd, 1, 500) };

            if ret < 0 {
                // poll error
                break;
            }
            if ret == 0 {
                // Timeout — loop around to check shutdown
                continue;
            }
            if pollfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                // PTY closed
                break;
            }

            let n = unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n <= 0 {
                break;
            }

            let data = engine.encode(&buf[..n as usize]);
            let notification = agent_proto::RpcRequest::notification(
                "shell.data",
                serde_json::json!({
                    "shell_id": shell_id_clone,
                    "data": data,
                }),
            );
            if let Ok(msg) = serde_json::to_string(&notification) {
                if tx.blocking_send(msg).is_err() {
                    break;
                }
            }
        }

        // Clean up read fd and remove session
        unsafe {
            libc::close(read_fd);
        }
        sessions().remove(&shell_id_clone);
        info!("Shell read loop ended: {shell_id_clone}");
    });

    sessions().insert(
        shell_id.clone(),
        ShellSession {
            master_fd: master_raw,
            child_pid,
            shutdown,
        },
    );

    info!(shell_id = %shell_id, pid = child_pid, "Shell session opened");
    Ok(())
}

/// Write data to a shell session (stdin).
pub fn handle_shell_data(id: String, params: serde_json::Value) -> RpcResponse {
    let req: ShellDataRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid params: {e}")),
    };

    let session = match sessions().get(&req.shell_id) {
        Some(s) => s,
        None => {
            return RpcResponse::error(
                id,
                -32001,
                format!("Shell not found: {}", req.shell_id),
            )
        }
    };

    let data = match base64::engine::general_purpose::STANDARD.decode(&req.data) {
        Ok(d) => d,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid base64: {e}")),
    };

    let n = unsafe { libc::write(session.master_fd, data.as_ptr() as *const _, data.len()) };
    if n < 0 {
        return RpcResponse::error(
            id,
            agent_proto::error_codes::COMMAND_FAILED,
            format!("Write failed: {}", std::io::Error::last_os_error()),
        );
    }

    RpcResponse::success(id, serde_json::json!({ "ok": true }))
}

/// Resize a shell session's terminal window.
pub fn handle_shell_resize(id: String, params: serde_json::Value) -> RpcResponse {
    let req: ShellResizeRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid params: {e}")),
    };

    let session = match sessions().get(&req.shell_id) {
        Some(s) => s,
        None => {
            return RpcResponse::error(
                id,
                -32001,
                format!("Shell not found: {}", req.shell_id),
            )
        }
    };

    let ws = libc::winsize {
        ws_row: req.rows,
        ws_col: req.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let ret = unsafe { libc::ioctl(session.master_fd, libc::TIOCSWINSZ as _, &ws) };
    if ret < 0 {
        return RpcResponse::error(
            id,
            agent_proto::error_codes::COMMAND_FAILED,
            format!("Resize failed: {}", std::io::Error::last_os_error()),
        );
    }

    RpcResponse::success(id, serde_json::json!({ "ok": true }))
}

/// Close a shell session.
pub fn handle_shell_close(id: String, params: serde_json::Value) -> RpcResponse {
    let req: ShellCloseRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid params: {e}")),
    };

    if let Some((_, session)) = sessions().remove(&req.shell_id) {
        // Signal the read thread to stop
        session.shutdown.store(true, Ordering::Relaxed);

        // Kill the child process
        if session.child_pid > 0 {
            unsafe {
                libc::kill(session.child_pid as i32, libc::SIGTERM);
            }
        }

        // Close master fd (read thread has its own dup'd fd)
        unsafe {
            libc::close(session.master_fd);
        }

        info!(shell_id = %req.shell_id, "Shell session closed");
        RpcResponse::success(id, serde_json::json!({ "ok": true }))
    } else {
        RpcResponse::error(
            id,
            -32001,
            format!("Shell not found: {}", req.shell_id),
        )
    }
}
