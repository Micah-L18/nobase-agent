use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::{info, warn};

/// Agent configuration loaded from TOML file.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentConfig {
    pub connection: ConnectionConfig,
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConnectionConfig {
    pub gateway_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub registration_token: String,
    #[serde(default)]
    pub server_id: String,
    /// Backend HTTP URL for self-updates (e.g. "http://192.168.0.247:3044")
    #[serde(default)]
    pub backend_url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HeartbeatConfig {
    #[serde(default = "default_heartbeat_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_reconnect_max_delay")]
    pub reconnect_max_delay_secs: u64,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            interval_secs: default_heartbeat_interval(),
            reconnect_max_delay_secs: default_reconnect_max_delay(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetricsConfig {
    /// How often to sample system metrics (default: 1s).
    #[serde(default = "default_collection_interval")]
    pub collection_interval_secs: u64,
    /// How often to push a batch of unsent metrics to the gateway (default: 5s).
    #[serde(default = "default_metrics_interval")]
    pub push_interval_secs: u64,
    #[serde(default = "default_true")]
    pub include_docker: bool,
    #[serde(default = "default_true")]
    pub include_gpu: bool,
    /// How long to retain buffered metrics on disk, in hours (default: 24).
    #[serde(default = "default_buffer_retention")]
    pub buffer_retention_hours: u64,
    /// Path to the local SQLite buffer database.
    #[serde(default = "default_buffer_path")]
    pub buffer_path: String,
    /// Maximum data points per backfill message on reconnect (default: 300).
    #[serde(default = "default_backfill_chunk")]
    pub backfill_chunk_size: u64,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            collection_interval_secs: default_collection_interval(),
            push_interval_secs: default_metrics_interval(),
            include_docker: true,
            include_gpu: true,
            buffer_retention_hours: default_buffer_retention(),
            buffer_path: default_buffer_path(),
            backfill_chunk_size: default_backfill_chunk(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SecurityConfig {
    #[serde(default)]
    pub allowed_commands: Vec<String>,
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    #[serde(default = "default_max_file_size")]
    pub max_file_size_mb: u64,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            allowed_commands: Vec::new(),
            allowed_paths: Vec::new(),
            max_file_size_mb: default_max_file_size(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    /// Output format: "text" (default) or "json"
    #[serde(default = "default_log_format")]
    pub format: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default = "default_log_max_size")]
    pub max_size_mb: u64,
    #[serde(default = "default_log_max_files")]
    pub max_files: u32,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: default_log_format(),
            file: None,
            max_size_mb: default_log_max_size(),
            max_files: default_log_max_files(),
        }
    }
}

// Default value functions
fn default_heartbeat_interval() -> u64 { 30 }
fn default_reconnect_max_delay() -> u64 { 300 }
fn default_collection_interval() -> u64 { 1 }
fn default_metrics_interval() -> u64 { 5 }
fn default_true() -> bool { true }
fn default_buffer_retention() -> u64 { 24 }
fn default_buffer_path() -> String { "/var/lib/nobase/metrics.db".into() }
fn default_backfill_chunk() -> u64 { 300 }
fn default_max_file_size() -> u64 { 100 }
fn default_log_level() -> String { "info".into() }
fn default_log_format() -> String { "text".into() }
fn default_log_max_size() -> u64 { 50 }
fn default_log_max_files() -> u32 { 3 }

impl AgentConfig {
    /// Load configuration from a TOML file.
    pub fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let path = Path::new(path);
        if !path.exists() {
            return Err(format!("Config file not found: {}", path.display()).into());
        }
        let content = std::fs::read_to_string(path)?;
        let config: AgentConfig = toml::from_str(&content)?;
        
        // Validate required fields
        if config.connection.gateway_url.is_empty() {
            return Err("connection.gateway_url is required".into());
        }
        if config.connection.api_key.is_empty() && config.connection.registration_token.is_empty() {
            return Err("Either connection.api_key or connection.registration_token is required".into());
        }

        Ok(config)
    }

    /// Check if this is a first-time registration (no API key yet).
    pub fn needs_registration(&self) -> bool {
        self.connection.api_key.is_empty()
    }

    /// Save configuration back to a TOML file (atomic write via temp + rename).
    pub fn save(&self, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        let content = toml::to_string_pretty(self)?;
        let path = Path::new(path);
        let tmp_path = path.with_extension("toml.tmp");
        std::fs::write(&tmp_path, &content)?;
        std::fs::rename(&tmp_path, path)?;
        info!("Config saved to {}", path.display());
        Ok(())
    }

    /// Update connection credentials after successful registration.
    pub fn set_credentials(&mut self, server_id: String, api_key: String) {
        self.connection.server_id = server_id;
        self.connection.api_key = api_key;
        // Clear the one-time registration token
        self.connection.registration_token.clear();
    }
}
