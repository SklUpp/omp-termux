use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use crate::mcp::config::validate_server_config;
use crate::mcp::error::McpError;
use crate::mcp::timeout::resolve_mcp_timeout_ms;
use crate::mcp::transports::http::HttpTransport;
use crate::mcp::transports::sse::LegacySseTransport;
use crate::mcp::transports::stdio::StdioTransport;
use crate::mcp::transports::McpTransport;
use crate::mcp::types::{
    MCPContent, MCPInitializeParams, MCPInitializeResult, MCPServerCapabilities, MCPServerConfig,
    MCPServerInfo, MCPToolCallResult, MCPToolDefinition, MCPToolsListResult, MCP_PROTOCOL_VERSION,
};

pub fn is_retriable_connection_error(error: &McpError) -> bool {
    match error {
        McpError::AcceptedStreamError(_) => false,
        McpError::Http { status, .. } => *status == 404 || *status == 502 || *status == 503,
        McpError::Transport(msg) | McpError::Timeout(msg) | McpError::Tool(msg) => {
            let lower = msg.to_lowercase();
            lower.contains("econnrefused")
                || lower.contains("econnreset")
                || lower.contains("epipe")
                || lower.contains("enetunreach")
                || lower.contains("ehostunreach")
                || lower.contains("fetch failed")
                || lower.contains("transport not connected")
                || lower.contains("transport closed")
                || lower.contains("network error")
                || lower.contains("connection refused")
                || lower.contains("broken pipe")
                || lower.contains("connection reset")
        }
        McpError::Io(io_err) => {
            use std::io::ErrorKind;
            matches!(
                io_err.kind(),
                ErrorKind::ConnectionRefused
                    | ErrorKind::ConnectionReset
                    | ErrorKind::BrokenPipe
                    | ErrorKind::NotConnected
            )
        }
        McpError::Closed => true,
        _ => false,
    }
}

/// `tools/call` failures that leave delivery unverified: the request was on the
/// wire (or a gateway saw it and answered 502/503) when the result was lost, so a
/// replay could execute a mutating call twice. [`McpError::Closed`] is excluded —
/// transports report it only when the request never left the client.
pub fn is_unverified_delivery(error: &McpError) -> bool {
    matches!(
        error,
        McpError::Transport(_) | McpError::Http { status: 502 | 503, .. }
    )
}

pub struct McpConnection {
    pub name: String,
    pub config: MCPServerConfig,
    pub transport: Arc<RwLock<Arc<dyn McpTransport>>>,
    pub server_info: MCPServerInfo,
    pub capabilities: MCPServerCapabilities,
    pub instructions: Option<String>,
}

impl std::fmt::Debug for McpConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConnection")
            .field("name", &self.name)
            .field("config", &self.config)
            .field("server_info", &self.server_info)
            .field("capabilities", &self.capabilities)
            .field("instructions", &self.instructions)
            .finish()
    }
}

async fn create_and_init_transport(
    config: &MCPServerConfig,
) -> Result<(Arc<dyn McpTransport>, MCPInitializeResult), McpError> {
    let transport: Arc<dyn McpTransport> = match config {
        MCPServerConfig::Stdio(s) => Arc::new(StdioTransport::spawn(s).await?),
        MCPServerConfig::Http(h) => Arc::new(HttpTransport::new(h)),
        MCPServerConfig::Sse(s) => {
            let sse = LegacySseTransport::new(s);
            sse.connect().await?;
            Arc::new(sse)
        }
    };

    let timeout_ms = resolve_mcp_timeout_ms(config.timeout());
    let timeout_dur = if timeout_ms > 0 {
        Some(Duration::from_millis(timeout_ms))
    } else {
        None
    };

    let init_params = MCPInitializeParams {
        protocol_version: MCP_PROTOCOL_VERSION.to_string(),
        capabilities: Default::default(),
        client_info: crate::mcp::types::MCPClientInfo {
            name: "omp".to_string(),
            version: "1.0.0".to_string(),
        },
    };

    let init_res_val = transport
        .request(
            "initialize",
            Some(serde_json::to_value(&init_params)?),
            timeout_dur,
        )
        .await?;

    let init_result: MCPInitializeResult = serde_json::from_value(init_res_val)
        .map_err(|e| McpError::Protocol(format!("Invalid initialize response: {}", e)))?;

    transport.set_protocol_version(&init_result.protocol_version);

    // Propagate notification errors (P1 from reviewer)
    transport
        .notify("notifications/initialized", Some(json!({})))
        .await?;

    Ok((transport, init_result))
}

pub async fn connect_to_server(
    name: &str,
    config: MCPServerConfig,
) -> Result<McpConnection, McpError> {
    if let Err(errors) = validate_server_config(name, &config) {
        return Err(McpError::Config(errors.join("; ")));
    }

    let (transport, init_result) = create_and_init_transport(&config).await?;

    Ok(McpConnection {
        name: name.to_string(),
        config,
        transport: Arc::new(RwLock::new(transport)),
        server_info: init_result.server_info,
        capabilities: init_result.capabilities,
        instructions: init_result.instructions,
    })
}

impl McpConnection {
    pub async fn reconnect(&self) -> Result<(), McpError> {
        let (new_transport, _) = create_and_init_transport(&self.config).await?;
        let mut t = self.transport.write().await;
        let old = std::mem::replace(&mut *t, new_transport);
        let _ = old.close().await;
        Ok(())
    }

    pub async fn list_tools(&self) -> Result<Vec<MCPToolDefinition>, McpError> {
        let timeout_ms = resolve_mcp_timeout_ms(self.config.timeout());
        let timeout_dur = if timeout_ms > 0 {
            Some(Duration::from_millis(timeout_ms))
        } else {
            None
        };

        let mut all_tools = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let mut params = json!({});
            if let Some(c) = &cursor {
                params["cursor"] = json!(c);
            }

            let transport = {
                let guard = self.transport.read().await;
                guard.clone()
            };

            let call_res = transport
                .request("tools/list", Some(params.clone()), timeout_dur)
                .await;

            let res_val = match call_res {
                Ok(v) => v,
                // Read-only tools/list is idempotent, so reconnecting and
                // retrying once is safe even when the previous attempt was
                // already on the wire.
                Err(e) if is_retriable_connection_error(&e) || is_unverified_delivery(&e) => {
                    self.reconnect().await?;
                    let new_transport = {
                        let guard = self.transport.read().await;
                        guard.clone()
                    };
                    new_transport
                        .request("tools/list", Some(params), timeout_dur)
                        .await?
                }
                Err(e) => return Err(e),
            };
            let page: MCPToolsListResult = serde_json::from_value(res_val)
                .map_err(|e| McpError::Protocol(format!("Invalid tools/list response: {}", e)))?;

            all_tools.extend(page.tools);

            if let Some(next) = page.next_cursor {
                if !next.is_empty() {
                    cursor = Some(next);
                    continue;
                }
            }
            break;
        }

        Ok(all_tools)
    }

    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<MCPToolCallResult, McpError> {
        let timeout_ms = resolve_mcp_timeout_ms(self.config.timeout());
        let timeout_dur = if timeout_ms > 0 {
            Some(Duration::from_millis(timeout_ms))
        } else {
            None
        };

        let params = json!({
            "name": name,
            "arguments": arguments,
        });

        let transport = {
            let guard = self.transport.read().await;
            guard.clone()
        };

        let call_res = transport
            .request("tools/call", Some(params), timeout_dur)
            .await;

        let res_val = match call_res {
            Ok(v) => v,
            // The server accepted this call and its result stream then died
            // (streamable HTTP reports that as `AcceptedStreamError`): execution
            // is uncertain. Surface it unchanged and never replay.
            Err(e @ McpError::AcceptedStreamError(_)) => return Err(e),
            // The request was already on the wire when the connection failed:
            // delivery, and therefore execution, cannot be verified. Reconnect
            // for future calls, but NEVER transparently replay a potentially
            // mutating tool call!
            Err(e) if is_unverified_delivery(&e) => {
                let _ = self.reconnect().await;
                return Err(McpError::Transport(format!(
                    "Connection lost during tool call '{}' (cannot verify if mutation executed; refusing replay): {}",
                    name, e
                )));
            }
            // Nothing was sent (`McpError::Closed`), or the call is known not to
            // have run (protocol/JSON-RPC/timeout): report the real error as-is.
            Err(e) => return Err(e),
        };

        let mut tool_result: MCPToolCallResult = serde_json::from_value(res_val)
            .map_err(|e| McpError::Protocol(format!("Invalid tools/call response: {}", e)))?;

        if tool_result.is_error == Some(true) {
            if let Some(first) = tool_result.content.first_mut() {
                match first {
                    MCPContent::Text { text } => {
                        if !text.starts_with("Error:") {
                            *text = format!("Error: {}", text);
                        }
                    }
                    _ => {
                        tool_result
                            .content
                            .insert(0, MCPContent::Text { text: "Error:".to_string() });
                    }
                }
            } else {
                tool_result.content.push(MCPContent::Text {
                    text: "Error: tool execution failed".to_string(),
                });
            }
        }

        Ok(tool_result)
    }

    pub async fn close(&self) -> Result<(), McpError> {
        let guard = self.transport.read().await;
        guard.close().await
    }
}
