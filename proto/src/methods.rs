use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─── Registration ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRegisterRequest {
    pub registration_token: String,
    pub hostname: String,
    pub os_type: String,
    pub os_version: String,
    pub agent_version: String,
    pub arch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRegisterResponse {
    pub server_id: String,
    pub api_key: String,
}

// ─── Reconnection ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentReconnectRequest {
    pub api_key: String,
    pub server_id: String,
    pub hostname: String,
    pub agent_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentReconnectResponse {
    pub ok: bool,
}

// ─── Command Execution ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecRequest {
    pub command: String,
    #[serde(default)]
    pub sudo: Option<bool>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResponse {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub duration_ms: u64,
}

// ─── Streaming Command ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecStreamRequest {
    pub command: String,
    #[serde(default)]
    pub sudo: Option<bool>,
    pub stream_id: String,
}

// ─── Interactive Shell ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellOpenRequest {
    pub shell_id: String,
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellOpenResponse {
    pub shell_id: String,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellDataRequest {
    pub shell_id: String,
    /// Base64-encoded terminal input.
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellResizeRequest {
    pub shell_id: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellCloseRequest {
    pub shell_id: String,
}

// ─── File Operations ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsListRequest {
    pub path: String,
    #[serde(default)]
    pub sudo: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsListResponse {
    pub entries: Vec<FsEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsEntry {
    pub name: String,
    pub entry_type: FsEntryType,
    pub size: u64,
    pub permissions: String,
    pub modified: String,
    pub owner: String,
    pub group: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsEntryType {
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsReadRequest {
    pub path: String,
    #[serde(default)]
    pub encoding: Option<FsEncoding>,
    #[serde(default)]
    pub sudo: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsReadResponse {
    pub content: String,
    pub encoding: FsEncoding,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsEncoding {
    Utf8,
    Base64,
}

impl Default for FsEncoding {
    fn default() -> Self {
        Self::Utf8
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsWriteRequest {
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub encoding: Option<FsEncoding>,
    #[serde(default)]
    pub sudo: Option<bool>,
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsStatRequest {
    pub path: String,
    #[serde(default)]
    pub sudo: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsStatResponse {
    pub entry: FsEntry,
    pub exists: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsMkdirRequest {
    pub path: String,
    #[serde(default)]
    pub recursive: Option<bool>,
    #[serde(default)]
    pub sudo: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsDeleteRequest {
    pub path: String,
    #[serde(default)]
    pub recursive: Option<bool>,
    #[serde(default)]
    pub sudo: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsRenameRequest {
    pub old_path: String,
    pub new_path: String,
    #[serde(default)]
    pub sudo: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsSearchRequest {
    pub path: String,
    pub pattern: String,
    #[serde(default)]
    pub max_depth: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsSearchResponse {
    pub matches: Vec<String>,
}

// ─── Chunked File Transfer ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsUploadStartRequest {
    pub transfer_id: String,
    pub path: String,
    pub total_size: u64,
    #[serde(default)]
    pub sudo: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsUploadChunkRequest {
    pub transfer_id: String,
    pub offset: u64,
    /// Base64-encoded chunk data (~64KB chunks).
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsUploadEndRequest {
    pub transfer_id: String,
    pub checksum_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsDownloadRequest {
    pub transfer_id: String,
    pub path: String,
    #[serde(default)]
    pub sudo: Option<bool>,
}

// ─── Docker Operations ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerStatsRequest {
    #[serde(default)]
    pub container_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerStatsResponse {
    pub containers: Vec<ContainerStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerStats {
    pub id: String,
    pub name: String,
    pub cpu_percent: f64,
    pub memory_usage_mb: f64,
    pub memory_limit_mb: f64,
    pub memory_percent: f64,
    pub network_rx_mb: f64,
    pub network_tx_mb: f64,
    pub block_read_mb: f64,
    pub block_write_mb: f64,
    pub pids: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerInspectRequest {
    pub container_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerLogsRequest {
    pub container_id: String,
    pub stream_id: String,
    #[serde(default)]
    pub tail: Option<u32>,
    #[serde(default)]
    pub follow: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerListRequest {
    /// If true, include stopped containers. Default: false (running only).
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerListResponse {
    pub containers: Vec<DockerContainerSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerContainerSummary {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: String,
    pub status: String,
    pub created: i64,
    pub ports: Vec<DockerPortMapping>,
    pub networks: Vec<String>,
    pub labels: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerPortMapping {
    pub container_port: u16,
    pub host_port: Option<u16>,
    pub protocol: String,
    pub host_ip: Option<String>,
}

// ─── System Info ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfoRequest {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfoResponse {
    pub hostname: String,
    pub os_type: String,
    pub os_version: String,
    pub kernel: String,
    pub arch: String,
    pub uptime_secs: u64,
}

// ─── Heartbeat ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    pub ok: bool,
    pub timestamp: String,
}

// ─── Gateway ↔ Backend messages ─────────────────────────────────────────────

/// Backend → Gateway: execute an RPC on a specific agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayAgentRequest {
    pub server_id: String,
    pub method: String,
    pub params: serde_json::Value,
}

/// Gateway → Backend: agent connection status change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatusNotification {
    pub server_id: String,
    pub status: AgentConnectionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentConnectionStatus {
    Connected,
    Disconnected,
}

/// Gateway → Backend: list connected agents response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub server_id: String,
    pub hostname: String,
    pub agent_version: String,
    pub os_type: String,
    pub connected_at: String,
    pub last_heartbeat: String,
}

/// Gateway → Backend: validate registration token request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidateTokenRequest {
    pub registration_token: String,
    pub agent_info: AgentRegisterRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidateTokenResponse {
    pub valid: bool,
    pub server_id: Option<String>,
    pub api_key: Option<String>,
}

// ─── API Key Rotation ────────────────────────────────────────────────────────

/// Backend → Agent (via gateway): rotate the agent's API key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RotateKeyRequest {
    pub new_api_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RotateKeyResponse {
    pub ok: bool,
}
