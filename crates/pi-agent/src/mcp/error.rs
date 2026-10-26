use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("OAuth authentication requires credential storage (step 5): {0}")]
    OAuthUnsupported(String),

    #[error("Transport error: {0}")]
    Transport(String),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("JSON-RPC error {code}: {message}")]
    JsonRpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },

    #[error("Timeout error: {0}")]
    Timeout(String),

    #[error("HTTP error {status}: {message}")]
    Http {
        status: u16,
        message: String,
    },

    #[error("Tool error: {0}")]
    Tool(String),

    #[error("Connection closed")]
    Closed,

    #[error("Accepted stream error (non-replayable mutating call): {0}")]
    AcceptedStreamError(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Json(#[from] serde_json::Error),
}
