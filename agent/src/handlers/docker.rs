use agent_proto::{
    ContainerStats, DockerInspectRequest, DockerLogsRequest, DockerStatsRequest,
    DockerStatsResponse, RpcRequest, RpcResponse,
};
use agent_proto::stream::{StreamChannel, StreamData, StreamEnd};
use bollard::Docker;
use bollard::container::{ListContainersOptions, InspectContainerOptions, LogsOptions, StatsOptions};
use futures_util::StreamExt;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Connect to the local Docker daemon.
fn connect_docker() -> Result<Docker, String> {
    Docker::connect_with_local_defaults()
        .map_err(|e| format!("Failed to connect to Docker: {e}"))
}

/// Handle docker.stats — get resource usage stats for containers.
pub async fn handle_docker_stats(id: String, params: serde_json::Value) -> RpcResponse {
    let request: DockerStatsRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid params: {e}")),
    };

    let docker = match connect_docker() {
        Ok(d) => d,
        Err(e) => return RpcResponse::error(id, -1, e),
    };

    // If no container IDs specified, get stats for all running containers
    let container_ids = if request.container_ids.is_empty() {
        // List all running containers
        let mut filters = HashMap::new();
        filters.insert("status".to_string(), vec!["running".to_string()]);
        let options = ListContainersOptions {
            filters,
            ..Default::default()
        };
        match docker.list_containers(Some(options)).await {
            Ok(containers) => containers
                .iter()
                .filter_map(|c| c.id.clone())
                .collect::<Vec<_>>(),
            Err(e) => return RpcResponse::error(id, -1, format!("Failed to list containers: {e}")),
        }
    } else {
        request.container_ids
    };

    let mut stats_results = Vec::new();

    for container_id in &container_ids {
        let options = StatsOptions {
            stream: false,
            one_shot: true,
        };

        let mut stream = docker.stats(container_id, Some(options));

        if let Some(Ok(stats)) = stream.next().await {
            // Calculate CPU usage percentage
            let cpu_delta = stats.cpu_stats.cpu_usage.total_usage as f64
                - stats.precpu_stats.cpu_usage.total_usage as f64;
            let system_delta = stats.cpu_stats.system_cpu_usage.unwrap_or(0) as f64
                - stats.precpu_stats.system_cpu_usage.unwrap_or(0) as f64;
            let num_cpus = stats
                .cpu_stats
                .online_cpus
                .unwrap_or(1) as f64;
            let cpu_percent = if system_delta > 0.0 {
                (cpu_delta / system_delta) * num_cpus * 100.0
            } else {
                0.0
            };

            // Memory stats
            let mem_usage = stats.memory_stats.usage.unwrap_or(0) as f64 / 1_048_576.0;
            let mem_limit = stats.memory_stats.limit.unwrap_or(1) as f64 / 1_048_576.0;
            let mem_percent = if mem_limit > 0.0 {
                (mem_usage / mem_limit) * 100.0
            } else {
                0.0
            };

            // Network stats
            let (rx_bytes, tx_bytes) = stats
                .networks
                .as_ref()
                .map(|nets| {
                    nets.values().fold((0u64, 0u64), |(rx, tx), n| {
                        (rx + n.rx_bytes, tx + n.tx_bytes)
                    })
                })
                .unwrap_or((0, 0));

            // Block I/O stats
            let (block_read, block_write) = stats
                .blkio_stats
                .io_service_bytes_recursive
                .as_ref()
                .map(|entries| {
                    entries.iter().fold((0u64, 0u64), |(r, w), entry| {
                        match entry.op.as_str() {
                            "read" | "Read" => (r + entry.value, w),
                            "write" | "Write" => (r, w + entry.value),
                            _ => (r, w),
                        }
                    })
                })
                .unwrap_or((0, 0));

            // Get container name
            let name = if stats.name.is_empty() {
                container_id.chars().take(12).collect()
            } else {
                stats.name.trim_start_matches('/').to_string()
            };

            stats_results.push(ContainerStats {
                id: container_id.chars().take(12).collect(),
                name,
                cpu_percent: (cpu_percent * 100.0).round() / 100.0,
                memory_usage_mb: (mem_usage * 100.0).round() / 100.0,
                memory_limit_mb: (mem_limit * 100.0).round() / 100.0,
                memory_percent: (mem_percent * 100.0).round() / 100.0,
                network_rx_mb: (rx_bytes as f64 / 1_048_576.0 * 100.0).round() / 100.0,
                network_tx_mb: (tx_bytes as f64 / 1_048_576.0 * 100.0).round() / 100.0,
                block_read_mb: (block_read as f64 / 1_048_576.0 * 100.0).round() / 100.0,
                block_write_mb: (block_write as f64 / 1_048_576.0 * 100.0).round() / 100.0,
                pids: stats.pids_stats.current.unwrap_or(0) as u32,
            });
        }
    }

    let response = DockerStatsResponse {
        containers: stats_results,
    };

    RpcResponse::success(id, serde_json::to_value(response).unwrap_or_default())
}

/// Handle docker.inspect — get detailed container information.
pub async fn handle_docker_inspect(id: String, params: serde_json::Value) -> RpcResponse {
    let request: DockerInspectRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid params: {e}")),
    };

    let docker = match connect_docker() {
        Ok(d) => d,
        Err(e) => return RpcResponse::error(id, -1, e),
    };

    match docker
        .inspect_container(&request.container_id, None::<InspectContainerOptions>)
        .await
    {
        Ok(info) => {
            let json = serde_json::to_value(info).unwrap_or_default();
            RpcResponse::success(id, json)
        }
        Err(e) => RpcResponse::error(id, -1, format!("Failed to inspect container: {e}")),
    }
}

/// Handle docker.logs — stream container logs back via notifications.
pub async fn handle_docker_logs(
    id: String,
    params: serde_json::Value,
    tx: mpsc::Sender<String>,
) -> RpcResponse {
    let request: DockerLogsRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, -32602, format!("Invalid params: {e}")),
    };

    let docker = match connect_docker() {
        Ok(d) => d,
        Err(e) => return RpcResponse::error(id, -1, e),
    };

    let stream_id = request.stream_id.clone();
    let follow = request.follow.unwrap_or(false);
    let tail = request.tail.map(|t| t.to_string()).unwrap_or_else(|| "all".to_string());

    let options = LogsOptions::<String> {
        stdout: true,
        stderr: true,
        follow,
        tail,
        ..Default::default()
    };

    // For following logs, spawn a background task
    if follow {
        let container_id = request.container_id.clone();
        let stream_id_clone = stream_id.clone();
        let tx_clone = tx.clone();

        tokio::spawn(async move {
            let mut log_stream = docker.logs(&container_id, Some(options));

            while let Some(result) = log_stream.next().await {
                match result {
                    Ok(output) => {
                        let (channel, data) = match output {
                            bollard::container::LogOutput::StdOut { message } => {
                                (StreamChannel::Stdout, message)
                            }
                            bollard::container::LogOutput::StdErr { message } => {
                                (StreamChannel::Stderr, message)
                            }
                            _ => continue,
                        };

                        let notification = RpcRequest::notification(
                            "stream.data",
                            serde_json::to_value(StreamData {
                                stream_id: stream_id_clone.clone(),
                                channel,
                                data: base64::Engine::encode(
                                    &base64::engine::general_purpose::STANDARD,
                                    &data,
                                ),
                            })
                            .unwrap_or_default(),
                        );

                        if tx_clone
                            .send(serde_json::to_string(&notification).unwrap_or_default())
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("Docker logs error: {e}");
                        break;
                    }
                }
            }

            // Send stream end
            let end_notification = RpcRequest::notification(
                "stream.end",
                serde_json::to_value(StreamEnd {
                    stream_id: stream_id_clone,
                    exit_code: 0,
                })
                .unwrap_or_default(),
            );
            let _ = tx_clone
                .send(serde_json::to_string(&end_notification).unwrap_or_default())
                .await;
        });

        // Return immediately — logs will stream via notifications
        RpcResponse::success(
            id,
            serde_json::json!({ "streaming": true, "stream_id": stream_id }),
        )
    } else {
        // Non-follow: collect all logs and return
        let mut log_stream = docker.logs(&request.container_id, Some(options));
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();

        while let Some(result) = log_stream.next().await {
            match result {
                Ok(bollard::container::LogOutput::StdOut { message }) => {
                    stdout_buf.extend_from_slice(&message);
                }
                Ok(bollard::container::LogOutput::StdErr { message }) => {
                    stderr_buf.extend_from_slice(&message);
                }
                Ok(_) => {}
                Err(e) => {
                    warn!("Docker logs error: {e}");
                    break;
                }
            }
        }

        RpcResponse::success(
            id,
            serde_json::json!({
                "stdout": String::from_utf8_lossy(&stdout_buf),
                "stderr": String::from_utf8_lossy(&stderr_buf),
            }),
        )
    }
}
