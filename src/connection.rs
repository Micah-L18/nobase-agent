use crate::config::AgentConfig;
use crate::dispatcher::{dispatch_request, DispatchContext};
use crate::metrics_buffer::MetricsBuffer;
use agent_proto::{
    AgentReconnectRequest, AgentRegisterRequest, HeartbeatRequest,
    MetricsBatchPushNotification, MetricsDataPoint,
    RpcMessage, RpcRequest, RpcResponse,
};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{self, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

/// Run the main connection loop. Reconnects automatically with exponential backoff.
///
/// The metrics collection task runs *outside* this loop so that it keeps
/// writing to the local SQLite buffer even while the agent is disconnected.
pub async fn run_connection_loop(config: AgentConfig, config_path: String) -> Result<(), Box<dyn std::error::Error>> {
    let config = Arc::new(RwLock::new(config));
    let max_backoff = config.read().await.heartbeat.reconnect_max_delay_secs;

    // ── Initialise the disk-backed metrics buffer ────────────────────────
    let buffer_path = config.read().await.metrics.buffer_path.clone();
    let metrics_buffer = Arc::new(
        MetricsBuffer::new(&buffer_path)
            .map_err(|e| format!("Failed to open metrics buffer at {buffer_path}: {e}"))?,
    );

    // Log how many unsent rows we have on startup (i.e. from previous runs)
    match metrics_buffer.unsent_count() {
        Ok(n) if n > 0 => info!("Metrics buffer has {n} unsent data points from previous session"),
        Ok(_) => {}
        Err(e) => warn!("Could not read unsent count: {e}"),
    }

    // ── Spawn the 1-second collection task (lives across reconnects) ────
    let collection_interval = config.read().await.metrics.collection_interval_secs;
    let retention_hours = config.read().await.metrics.buffer_retention_hours;
    let buf_collect = metrics_buffer.clone();
    let _collection_handle = tokio::spawn(async move {
        // Persistent System instance for efficient CPU delta tracking
        let mut sys = sysinfo::System::new();
        sys.refresh_cpu_all();
        sys.refresh_memory();

        let mut interval = time::interval(Duration::from_secs(collection_interval));
        let mut cleanup_tick: u64 = 0;

        loop {
            interval.tick().await;

            sys.refresh_cpu_all();
            sys.refresh_memory();

            let metrics = crate::handlers::metrics::collect_metrics_from_system(&sys);
            let ts = chrono::Utc::now().to_rfc3339();

            // Write to local SQLite buffer (blocking I/O in spawn_blocking)
            let buf = buf_collect.clone();
            let ts2 = ts.clone();
            if let Err(e) = tokio::task::spawn_blocking(move || buf.store(&ts2, &metrics)).await {
                warn!("Metrics store task panicked: {e}");
            }

            // Periodic cleanup every 5 minutes (300 ticks at 1s)
            cleanup_tick += 1;
            if cleanup_tick % 300 == 0 {
                let buf = buf_collect.clone();
                let rh = retention_hours;
                tokio::task::spawn_blocking(move || {
                    let _ = buf.cleanup(rh);
                    let _ = buf.cleanup_sent();
                })
                .await
                .ok();
            }
        }
    });

    // ── Connection loop with exponential backoff ────────────────────────
    let mut backoff_secs: u64 = 1;

    loop {
        match connect_and_run(config.clone(), &config_path, metrics_buffer.clone()).await {
            Ok(()) => {
                info!("Connection closed gracefully");
                break;
            }
            Err(e) => {
                error!("Connection error: {e}");
                warn!("Reconnecting in {backoff_secs}s...");
                time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(max_backoff);
            }
        }
    }

    Ok(())
}

/// Connect to gateway and run the message loop.
async fn connect_and_run(
    config: Arc<RwLock<AgentConfig>>,
    config_path: &str,
    metrics_buffer: Arc<MetricsBuffer>,
) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = config.read().await;
    let url = cfg.connection.gateway_url.clone();
    let needs_registration = cfg.needs_registration();
    let registration_token = cfg.connection.registration_token.clone();
    let api_key = cfg.connection.api_key.clone();
    let server_id = cfg.connection.server_id.clone();
    let heartbeat_interval = cfg.heartbeat.interval_secs;
    let push_interval = cfg.metrics.push_interval_secs;
    let backfill_chunk_size = cfg.metrics.backfill_chunk_size as usize;
    drop(cfg); // Release read lock

    info!("Connecting to gateway: {url}");

    let request = http::Request::builder()
        .uri(&url)
        .header("Host", extract_host(&url))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tokio_tungstenite::tungstenite::handshake::client::generate_key(),
        )
        .body(())?;

    let (ws_stream, _response) = connect_async(request).await?;
    info!("Connected to gateway");

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // Channel for sending messages back through the WS
    let (tx, mut rx) = mpsc::channel::<String>(256);

    // Register or reconnect
    if needs_registration {
        let hostname = gethostname();
        let register = RpcRequest::new(
            uuid::Uuid::new_v4().to_string(),
            "agent.register",
            serde_json::to_value(AgentRegisterRequest {
                registration_token,
                hostname: hostname.clone(),
                os_type: std::env::consts::OS.into(),
                os_version: get_os_version(),
                agent_version: env!("CARGO_PKG_VERSION").into(),
                arch: std::env::consts::ARCH.into(),
            })?,
        );
        let msg = serde_json::to_string(&register)?;
        ws_sender.send(Message::Text(msg.into())).await?;
        info!("Sent registration request");
    } else {
        let reconnect = RpcRequest::new(
            uuid::Uuid::new_v4().to_string(),
            "agent.reconnect",
            serde_json::to_value(AgentReconnectRequest {
                api_key,
                server_id,
                hostname: gethostname(),
                agent_version: env!("CARGO_PKG_VERSION").into(),
            })?,
        );
        let msg = serde_json::to_string(&reconnect)?;
        ws_sender.send(Message::Text(msg.into())).await?;
        info!("Sent reconnect request");
    }

    // Spawn heartbeat task
    let heartbeat_tx = tx.clone();
    let heartbeat_handle = tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(heartbeat_interval));
        loop {
            interval.tick().await;
            let hb = RpcRequest::notification(
                "agent.heartbeat",
                serde_json::to_value(HeartbeatRequest {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                })
                .unwrap_or_default(),
            );
            if let Ok(msg) = serde_json::to_string(&hb) {
                if heartbeat_tx.send(msg).await.is_err() {
                    break;
                }
            }
        }
    });

    // Spawn one-shot update check — runs once after connection stabilises.
    // If a new binary is available, downloads it, replaces the running binary,
    // and restarts the service (which will cause this connection to drop and
    // re-establish with the new version).
    let update_config = config.clone();
    tokio::spawn(async move {
        // Wait for the connection to stabilise before checking
        tokio::time::sleep(Duration::from_secs(5)).await;

        let backend_url = {
            let cfg = update_config.read().await;
            cfg.connection.backend_url.clone()
        };

        match crate::updater::check_and_update(&backend_url, true).await {
            Ok(crate::updater::UpdateResult::AlreadyUpToDate) => {
                debug!("Update check: already up-to-date");
            }
            Ok(crate::updater::UpdateResult::Updated { new_hash, .. }) => {
                info!("Update check: updated to {}, service restarting...", &new_hash[..12]);
            }
            Ok(crate::updater::UpdateResult::NotConfigured) => {
                debug!("Update check: backend_url not configured, skipping");
            }
            Err(e) => {
                warn!("Update check failed: {e}");
            }
        }
    });

    // Spawn metrics push task — drains unsent rows from the local buffer
    // and sends them as batches to the gateway every `push_interval` seconds.
    let metrics_tx = tx.clone();
    let push_buf = metrics_buffer.clone();
    let metrics_handle = tokio::spawn(async move {
        // Wait for connection to stabilize before first push
        tokio::time::sleep(Duration::from_secs(2)).await;

        // ── Backfill: drain all previously-unsent data points ───────────
        info!("Starting metrics backfill...");
        loop {
            let buf = push_buf.clone();
            let chunk = backfill_chunk_size;
            let rows = match tokio::task::spawn_blocking(move || buf.get_unsent(chunk)).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    warn!("Backfill read error: {e}");
                    break;
                }
                Err(e) => {
                    warn!("Backfill task panicked: {e}");
                    break;
                }
            };

            if rows.is_empty() {
                info!("Metrics backfill complete — no unsent data remaining");
                break;
            }

            let count = rows.len();
            let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
            let data_points: Vec<MetricsDataPoint> = rows
                .into_iter()
                .map(|r| MetricsDataPoint {
                    metrics: r.metrics,
                    timestamp: r.timestamp,
                })
                .collect();

            let notification = RpcRequest::notification(
                "metrics.batch_push",
                serde_json::to_value(MetricsBatchPushNotification {
                    server_id: String::new(), // Gateway fills in
                    data_points,
                    is_backfill: true,
                })
                .unwrap_or_default(),
            );
            if let Ok(msg) = serde_json::to_string(&notification) {
                if metrics_tx.send(msg).await.is_err() {
                    warn!("Backfill: send channel closed, will retry on next connection");
                    break;
                }
            }

            // Mark as sent
            let buf = push_buf.clone();
            let _ = tokio::task::spawn_blocking(move || buf.mark_sent(&ids)).await;

            info!("Backfill: sent {count} data points");

            // Small delay between chunks to avoid overwhelming the gateway
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // ── Steady-state: push latest unsent data every push_interval ───
        let mut interval = time::interval(Duration::from_secs(push_interval));
        loop {
            interval.tick().await;

            let buf = push_buf.clone();
            // Fetch up to push_interval worth of 1-second samples
            let limit = (push_interval as usize).max(5);
            let rows = match tokio::task::spawn_blocking(move || buf.get_unsent(limit)).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    warn!("Metrics push read error: {e}");
                    continue;
                }
                Err(e) => {
                    warn!("Metrics push task panicked: {e}");
                    continue;
                }
            };

            if rows.is_empty() {
                continue;
            }

            let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
            let data_points: Vec<MetricsDataPoint> = rows
                .into_iter()
                .map(|r| MetricsDataPoint {
                    metrics: r.metrics,
                    timestamp: r.timestamp,
                })
                .collect();

            let notification = RpcRequest::notification(
                "metrics.batch_push",
                serde_json::to_value(MetricsBatchPushNotification {
                    server_id: String::new(),
                    data_points,
                    is_backfill: false,
                })
                .unwrap_or_default(),
            );
            if let Ok(msg) = serde_json::to_string(&notification) {
                if metrics_tx.send(msg).await.is_err() {
                    break; // Channel closed — connection dead
                }
            }

            // Mark as sent after successful channel send
            let buf = push_buf.clone();
            let _ = tokio::task::spawn_blocking(move || buf.mark_sent(&ids)).await;
        }
    });

    // Spawn sender task — drains the tx channel and sends over WS
    let sender_handle = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_sender.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    // Create dispatch context for handlers that need config access
    let dispatch_ctx = Arc::new(DispatchContext {
        config: config.clone(),
        config_path: config_path.to_string(),
    });

    // Main receive loop
    while let Some(msg_result) = ws_receiver.next().await {
        match msg_result {
            Ok(Message::Text(text)) => {
                let text_str: &str = &text;
                debug!("Received: {}", &text_str[..text_str.len().min(200)]);

                match serde_json::from_str::<RpcMessage>(text_str) {
                    Ok(RpcMessage::Request(request)) => {
                        // Incoming RPC request from gateway — dispatch to handler
                        let tx_clone = tx.clone();
                        let ctx_clone = dispatch_ctx.clone();
                        tokio::spawn(async move {
                            let response = dispatch_request(request, tx_clone.clone(), ctx_clone).await;
                            if let Ok(msg) = serde_json::to_string(&response) {
                                let _ = tx_clone.send(msg).await;
                            }
                        });
                    }
                    Ok(RpcMessage::Response(response)) => {
                        // Response to our request (registration, reconnect, etc.)
                        handle_response(response, config.clone(), config_path).await;
                    }
                    Err(e) => {
                        warn!("Failed to parse message: {e}");
                    }
                }
            }
            Ok(Message::Ping(_data)) => {
                // WebSocket pings are handled at the protocol level by tungstenite
            }
            Ok(Message::Close(_)) => {
                info!("Gateway closed connection");
                break;
            }
            Err(e) => {
                error!("WebSocket error: {e}");
                break;
            }
            _ => {}
        }
    }

    // Cleanup
    heartbeat_handle.abort();
    metrics_handle.abort();
    sender_handle.abort();

    Err("Connection lost".into())
}

/// Handle a response to one of our outgoing requests.
async fn handle_response(response: RpcResponse, config: Arc<RwLock<AgentConfig>>, config_path: &str) {
    if response.is_error() {
        if let Some(ref err) = response.error {
            error!(
                "RPC error (id={}): [{}] {}",
                response.id, err.code, err.message
            );
        }
    } else if let Some(ref result) = response.result {
        // Check if this is a registration response
        if let Ok(reg) = serde_json::from_value::<agent_proto::AgentRegisterResponse>(result.clone())
        {
            info!(
                server_id = %reg.server_id,
                "Registration successful! API key received."
            );

            // Persist API key + server_id to config file
            let mut cfg = config.write().await;
            cfg.set_credentials(reg.server_id, reg.api_key);
            match cfg.save(config_path) {
                Ok(()) => info!("Credentials persisted to config file"),
                Err(e) => warn!("Failed to persist credentials: {e} — agent will need re-registration on restart"),
            }
        } else {
            debug!("Response (id={}): {:?}", response.id, result);
        }
    }
}

/// Get the system hostname.
fn gethostname() -> String {
    hostname::get()
        .map(|h: std::ffi::OsString| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

/// Get OS version string.
fn get_os_version() -> String {
    sysinfo::System::os_version()
        .unwrap_or_else(|| "unknown".into())
}

/// Extract host from a URL for the Host header.
fn extract_host(url: &str) -> String {
    url.replace("wss://", "")
        .replace("ws://", "")
        .split('/')
        .next()
        .unwrap_or("localhost")
        .to_string()
}
