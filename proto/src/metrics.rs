use serde::{Deserialize, Serialize};

/// Structured metrics collected by the agent.
/// Replaces the 17 sequential SSH commands currently used by the backend collector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsPayload {
    pub hostname: String,
    pub os: OsInfo,
    pub uptime_secs: u64,
    pub cpu: CpuMetrics,
    pub memory: MemoryMetrics,
    pub disks: Vec<DiskMetrics>,
    pub network: NetworkMetrics,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docker: Option<DockerMetrics>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OsInfo {
    pub name: String,
    pub version: String,
    pub kernel: String,
    pub arch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuMetrics {
    pub model: String,
    pub cores: u32,
    pub usage_percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_celsius: Option<f64>,
    pub mhz: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryMetrics {
    pub total_mb: u64,
    pub used_mb: u64,
    pub available_mb: u64,
    pub swap_total_mb: u64,
    pub swap_used_mb: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed_mhz: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskMetrics {
    pub mount: String,
    pub filesystem: String,
    pub total_gb: f64,
    pub used_gb: f64,
    pub available_gb: f64,
    pub usage_percent: f64,
    pub device: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkMetrics {
    /// Primary network interface name (e.g. "eth0", "ens3")
    #[serde(default)]
    pub interface: String,
    /// Download rate in bytes per second (computed delta between snapshots)
    pub rx_bytes_per_sec: u64,
    /// Upload rate in bytes per second (computed delta between snapshots)
    pub tx_bytes_per_sec: u64,
    /// Total bytes received since boot (cumulative)
    pub rx_total_bytes: u64,
    /// Total bytes transmitted since boot (cumulative)
    pub tx_total_bytes: u64,
    pub latency_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuMetrics {
    pub name: String,
    pub memory_total_mb: u64,
    pub memory_used_mb: u64,
    pub usage_percent: f64,
    pub temperature_celsius: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerMetrics {
    pub installed: bool,
    pub version: String,
    pub containers_total: u32,
    pub containers_running: u32,
}

/// Request to collect metrics (no params needed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsCollectRequest {}

/// Wrapper for metrics push notification from agent (single snapshot, legacy).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsPushNotification {
    pub server_id: String,
    pub metrics: MetricsPayload,
    pub timestamp: String,
}

/// A single timestamped metrics data point within a batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsDataPoint {
    pub metrics: MetricsPayload,
    pub timestamp: String,
}

/// Batch metrics push notification from agent.
///
/// Sent every `push_interval_secs` containing the latest unsent data points.
/// On reconnect, `is_backfill = true` signals that the data is historical
/// and the backend should use the provided timestamps rather than `now()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsBatchPushNotification {
    pub server_id: String,
    pub data_points: Vec<MetricsDataPoint>,
    #[serde(default)]
    pub is_backfill: bool,
}
