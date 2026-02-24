use serde::{Deserialize, Serialize};

/// Channel identifier for stream data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamChannel {
    Stdout,
    Stderr,
}

/// A chunk of stream data (terminal output, command output, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamData {
    pub stream_id: String,
    pub channel: StreamChannel,
    /// Base64-encoded raw bytes.
    pub data: String,
}

/// Notification that a stream has ended.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEnd {
    pub stream_id: String,
    pub exit_code: i32,
}

/// Stream data for shell/terminal sessions (bidirectional).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellStreamData {
    pub shell_id: String,
    /// Base64-encoded terminal data.
    pub data: String,
}
