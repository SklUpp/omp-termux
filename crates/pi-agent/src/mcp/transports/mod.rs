use async_trait::async_trait;
use serde_json::Value;
use std::time::Duration;
use crate::mcp::error::McpError;

pub mod header_policy;
pub mod http;
pub mod sse;
pub mod stdio;

#[async_trait]
pub trait McpTransport: Send + Sync {
    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Option<Duration>,
    ) -> Result<Value, McpError>;

    async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError>;

    async fn close(&self) -> Result<(), McpError>;

    fn set_protocol_version(&self, _version: &str) {}
}
