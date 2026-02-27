use crate::config::AgentConfig;
use agent_proto::{ExecRequest, HeartbeatResponse, RotateKeyRequest, RpcRequest, RpcResponse};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info, warn};

/// Shared config context for handlers that need to modify agent state.
pub struct DispatchContext {
    pub config: Arc<RwLock<AgentConfig>>,
    pub config_path: String,
}

/// Dispatch an incoming RPC request to the appropriate handler.
/// The `tx` sender is passed through for handlers that need to send
/// ongoing notifications (e.g., shell data, stream output).
/// The `ctx` provides access to agent config for operations like key rotation.
pub async fn dispatch_request(
    request: RpcRequest,
    tx: mpsc::Sender<String>,
    ctx: Arc<DispatchContext>,
) -> RpcResponse {
    let id = request.id.clone().unwrap_or_default();
    let method = request.method.as_str();

    debug!("Dispatching RPC: {method}");

    match method {
        // ─── Command execution ──────────────────────────────────────────
        "exec" => handle_exec(id, request.params).await,

        // ─── Heartbeat / ping ───────────────────────────────────────────
        "agent.heartbeat" | "heartbeat" | "ping" => handle_ping(id).await,

        // ─── System info ────────────────────────────────────────────────
        "system.info" => super::handlers::exec::handle_system_info(id).await,

        // ─── Interactive shell (PTY) ────────────────────────────────────
        "shell.open" => super::handlers::shell::handle_shell_open(id, request.params, tx).await,
        "shell.data" => super::handlers::shell::handle_shell_data(id, request.params),
        "shell.resize" => super::handlers::shell::handle_shell_resize(id, request.params),
        "shell.close" => super::handlers::shell::handle_shell_close(id, request.params),

        // ─── File system operations ─────────────────────────────────────
        "fs.list" => super::handlers::fs::handle_fs_list(id, request.params).await,
        "fs.read" => super::handlers::fs::handle_fs_read(id, request.params).await,
        "fs.write" => super::handlers::fs::handle_fs_write(id, request.params).await,
        "fs.stat" => super::handlers::fs::handle_fs_stat(id, request.params).await,
        "fs.mkdir" => super::handlers::fs::handle_fs_mkdir(id, request.params).await,
        "fs.delete" => super::handlers::fs::handle_fs_delete(id, request.params).await,
        "fs.rename" => super::handlers::fs::handle_fs_rename(id, request.params).await,
        "fs.search" => super::handlers::fs::handle_fs_search(id, request.params).await,

        // ─── Chunked file transfer ──────────────────────────────────────
        "fs.upload.start" => super::handlers::fs::handle_fs_upload_start(id, request.params).await,
        "fs.upload.chunk" => super::handlers::fs::handle_fs_upload_chunk(id, request.params).await,
        "fs.upload.end" => super::handlers::fs::handle_fs_upload_end(id, request.params).await,
        "fs.download" => super::handlers::fs::handle_fs_download(id, request.params, tx).await,

        // ─── Metrics ────────────────────────────────────────────────────
        "metrics.collect" => super::handlers::metrics::handle_metrics_collect(id).await,

        // ─── Streaming command execution ─────────────────────────────────
        "exec.stream" => super::handlers::exec::handle_exec_stream(id, request.params, tx).await,

        // ─── Docker operations ──────────────────────────────────────────
        "docker.list" => super::handlers::docker::handle_docker_list(id, request.params).await,
        "docker.stats" => super::handlers::docker::handle_docker_stats(id, request.params).await,
        "docker.inspect" => super::handlers::docker::handle_docker_inspect(id, request.params).await,
        "docker.logs" => super::handlers::docker::handle_docker_logs(id, request.params, tx).await,

        // ─── Agent management ───────────────────────────────────────────
        "rotate_key" => handle_rotate_key(id, request.params, ctx).await,
        "self_update" => handle_self_update(id, ctx).await,

        _ => {
            warn!("Unknown method: {method}");
            RpcResponse::error(id, -32601, format!("Method not found: {method}"))
        }
    }
}

/// Handle command execution.
async fn handle_exec(id: String, params: serde_json::Value) -> RpcResponse {
    let request: ExecRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => {
            return RpcResponse::error(id, -32602, format!("Invalid params: {e}"));
        }
    };

    super::handlers::exec::execute_command(id, request).await
}

/// Handle ping/heartbeat.
async fn handle_ping(id: String) -> RpcResponse {
    RpcResponse::success(
        id,
        serde_json::to_value(HeartbeatResponse {
            ok: true,
            timestamp: chrono::Utc::now().to_rfc3339(),
        })
        .unwrap_or_default(),
    )
}

/// Handle API key rotation — update config and save atomically.
async fn handle_rotate_key(
    id: String,
    params: serde_json::Value,
    ctx: Arc<DispatchContext>,
) -> RpcResponse {
    let req: RotateKeyRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => {
            return RpcResponse::error(id, -32602, format!("Invalid params: {e}"));
        }
    };

    let mut config = ctx.config.write().await;
    config.connection.api_key = req.new_api_key;

    if let Err(e) = config.save(&ctx.config_path) {
        return RpcResponse::error(id, -32000, format!("Failed to save new key: {e}"));
    }

    info!("API key rotated and saved to config");
    RpcResponse::success(
        id,
        serde_json::json!({ "ok": true }),
    )
}

/// Handle self-update RPC: check for a new binary on the backend and apply it.
///
/// The agent downloads the latest binary, verifies its checksum, replaces itself,
/// and restarts the systemd service. The RPC response is sent *before* the restart
/// happens, so the caller should expect the connection to drop shortly after.
async fn handle_self_update(id: String, ctx: Arc<DispatchContext>) -> RpcResponse {
    let backend_url = {
        let config = ctx.config.read().await;
        config.connection.backend_url.clone()
    };

    match crate::updater::check_and_update(&backend_url, true).await {
        Ok(result) => {
            let status = match &result {
                crate::updater::UpdateResult::AlreadyUpToDate => "up_to_date",
                crate::updater::UpdateResult::Updated { .. } => "updated",
                crate::updater::UpdateResult::NotConfigured => "not_configured",
            };
            info!("Self-update RPC result: {result}");
            RpcResponse::success(
                id,
                serde_json::json!({
                    "status": status,
                    "message": result.to_string(),
                }),
            )
        }
        Err(e) => {
            error!("Self-update RPC failed: {e}");
            RpcResponse::error(id, -32000, format!("Update failed: {e}"))
        }
    }
}
