use serde::{Deserialize, Serialize};

/// JSON-RPC 2.0 request envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    /// Unique request ID. Present for requests, absent for notifications.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// RPC method name (e.g. "exec", "fs.list", "shell.open").
    pub method: String,
    /// Method-specific parameters.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// JSON-RPC 2.0 success response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: String,
    /// The result payload (present on success).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Error payload (present on failure).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Well-known error codes.
pub mod error_codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;

    // Application-defined errors
    pub const AGENT_OFFLINE: i32 = -32001;
    pub const AGENT_TIMEOUT: i32 = -32002;
    pub const COMMAND_FAILED: i32 = -32003;
    pub const AUTH_FAILED: i32 = -32004;
    pub const REGISTRATION_FAILED: i32 = -32005;
}

impl RpcRequest {
    /// Create a new JSON-RPC 2.0 request.
    pub fn new(id: impl Into<String>, method: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(id.into()),
            method: method.into(),
            params,
        }
    }

    /// Create a notification (no response expected).
    pub fn notification(method: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: None,
            method: method.into(),
            params,
        }
    }
}

impl RpcResponse {
    /// Create a success response.
    pub fn success(id: impl Into<String>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: id.into(),
            result: Some(result),
            error: None,
        }
    }

    /// Create an error response.
    pub fn error(id: impl Into<String>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: id.into(),
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }

    /// Create an error response with additional data.
    pub fn error_with_data(
        id: impl Into<String>,
        code: i32,
        message: impl Into<String>,
        data: serde_json::Value,
    ) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: id.into(),
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: Some(data),
            }),
        }
    }

    /// Check if this response is an error.
    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }
}

/// Unified message type — can be either a request or response.
/// Used for parsing incoming WS messages before we know the type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcMessage {
    Request(RpcRequest),
    Response(RpcResponse),
}
