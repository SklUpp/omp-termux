use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::mcp::config::validate_server_config;
use crate::mcp::error::McpError;
use crate::mcp::timeout::resolve_mcp_timeout_ms;
use crate::mcp::transports::http::HttpTransport;
use crate::mcp::transports::sse::LegacySseTransport;
use crate::mcp::transports::stdio::StdioTransport;
use crate::mcp::transports::McpTransport;
use crate::mcp::types::{
    MCPContent, MCPInitializeParams, MCPInitializeResult, MCPResource, MCPResourceReadParams,
    MCPResourceReadResult, MCPResourceTemplate, MCPResourceTemplatesListResult,
    MCPResourcesListResult, MCPServerCapabilities, MCPServerConfig, MCPServerInfo,
    MCPToolCallResult, MCPToolDefinition, MCPToolsListResult, MCP_PROTOCOL_VERSION,
};

// Conservative pagination bounds for untrusted servers; shared by resources
// and resource templates. 64 pages x 10k entries keeps a broken cursor from
// becoming unbounded memory/time while never limiting a real server.
const MAX_RESOURCE_PAGES: usize = 64;
const MAX_RESOURCE_ENTRIES: usize = 10_000;

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

/// JSON-RPC `-32601` ("Method not found"). Servers may advertise the
/// `resources` capability without implementing the optional
/// `resources/templates/list` method.
fn is_method_not_found_error(error: &McpError) -> bool {
    if let McpError::JsonRpc { code, .. } = error {
        if *code == -32601 {
            return true;
        }
    }
    let message = error.to_string().to_lowercase();
    message.contains("-32601") || message.contains("method not found")
}

impl McpConnection {
    fn request_timeout(&self) -> Option<Duration> {
        let timeout_ms = resolve_mcp_timeout_ms(self.config.timeout());
        if timeout_ms > 0 {
            Some(Duration::from_millis(timeout_ms))
        } else {
            None
        }
    }

    pub async fn reconnect(&self) -> Result<(), McpError> {
        let (new_transport, _) = create_and_init_transport(&self.config).await?;
        let mut t = self.transport.write().await;
        let old = std::mem::replace(&mut *t, new_transport);
        let _ = old.close().await;
        Ok(())
    }

    pub async fn list_tools(&self) -> Result<Vec<MCPToolDefinition>, McpError> {
        let timeout_dur = self.request_timeout();

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

    /// List resources, following `nextCursor` until exhausted.
    ///
    /// Returns `[]` when the server did not advertise the `resources`
    /// capability (no request is sent), matching the TS client. Reads are
    /// idempotent, but nothing is cached or replayed here: the TS side caches
    /// per connection, and a stale cache is worse than one request.
    pub async fn list_resources(&self) -> Result<Vec<MCPResource>, McpError> {
        if self.capabilities.resources.is_none() {
            return Ok(Vec::new());
        }
        let timeout_dur = self.request_timeout();
        let deadline = timeout_dur.map(|timeout| Instant::now() + timeout);
        let mut all_resources = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();
        let mut pages = 0usize;

        loop {
            pages += 1;
            if pages > MAX_RESOURCE_PAGES {
                return Err(McpError::Protocol(
                    "resources/list exceeded pagination bounds".to_string(),
                ));
            }
            let mut params = json!({});
            if let Some(c) = &cursor {
                params["cursor"] = json!(c);
            }
            let remaining = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(McpError::Timeout(
                            "resources/list pagination deadline exceeded".to_string(),
                        ));
                    }
                    Some(remaining)
                }
                None => None,
            };
            let transport = {
                let guard = self.transport.read().await;
                guard.clone()
            };
            let res_val = transport
                .request("resources/list", Some(params), remaining)
                .await?;
            let page: MCPResourcesListResult = serde_json::from_value(res_val).map_err(|e| {
                McpError::Protocol(format!("Invalid resources/list response: {}", e))
            })?;

            if page.resources.len() > MAX_RESOURCE_ENTRIES.saturating_sub(all_resources.len()) {
                return Err(McpError::Protocol(
                    "resources/list exceeded pagination bounds".to_string(),
                ));
            }
            all_resources.extend(page.resources);

            match page.next_cursor {
                Some(next) if !next.is_empty() => {
                    if !seen.insert(next.clone()) {
                        return Err(McpError::Protocol(
                            "resources/list returned a repeated cursor".to_string(),
                        ));
                    }
                    cursor = Some(next);
                }
                _ => break,
            }
        }

        Ok(all_resources)
    }

    /// List resource templates, following `nextCursor` until exhausted.
    ///
    /// A server MAY advertise `resources` without implementing the optional
    /// `resources/templates/list` method and answer JSON-RPC `-32601`. That is
    /// treated as "no templates" so it cannot discard the server's concrete
    /// resources; every other error propagates.
    pub async fn list_resource_templates(&self) -> Result<Vec<MCPResourceTemplate>, McpError> {
        if self.capabilities.resources.is_none() {
            return Ok(Vec::new());
        }
        let timeout_dur = self.request_timeout();
        let deadline = timeout_dur.map(|timeout| Instant::now() + timeout);
        let mut all_templates = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();
        let mut pages = 0usize;

        loop {
            pages += 1;
            if pages > MAX_RESOURCE_PAGES {
                return Err(McpError::Protocol(
                    "resources/templates/list exceeded pagination bounds".to_string(),
                ));
            }
            let mut params = json!({});
            if let Some(c) = &cursor {
                params["cursor"] = json!(c);
            }
            let remaining = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(McpError::Timeout(
                            "resources/templates/list pagination deadline exceeded".to_string(),
                        ));
                    }
                    Some(remaining)
                }
                None => None,
            };
            let transport = {
                let guard = self.transport.read().await;
                guard.clone()
            };
            let res_val = transport
                .request("resources/templates/list", Some(params), remaining)
                .await;
            let res_val = match res_val {
                Ok(v) => v,
                Err(e) if is_method_not_found_error(&e) => return Ok(Vec::new()),
                Err(e) => return Err(e),
            };
            let page: MCPResourceTemplatesListResult =
                serde_json::from_value(res_val).map_err(|e| {
                    McpError::Protocol(format!(
                        "Invalid resources/templates/list response: {}",
                        e
                    ))
                })?;

            if page.resource_templates.len()
                > MAX_RESOURCE_ENTRIES.saturating_sub(all_templates.len())
            {
                return Err(McpError::Protocol(
                    "resources/templates/list exceeded pagination bounds".to_string(),
                ));
            }
            all_templates.extend(page.resource_templates);

            match page.next_cursor {
                Some(next) if !next.is_empty() => {
                    if !seen.insert(next.clone()) {
                        return Err(McpError::Protocol(
                            "resources/templates/list returned a repeated cursor".to_string(),
                        ));
                    }
                    cursor = Some(next);
                }
                _ => break,
            }
        }

        Ok(all_templates)
    }

    /// Read a resource by URI. The URI is sent byte-for-byte: MCP resource URIs
    /// are opaque and must not be normalized (colons, query strings, and
    /// fragments are all significant). Failures, including JSON-RPC errors,
    /// are surfaced unchanged — no synthetic content is ever produced.
    pub async fn read_resource(&self, uri: &str) -> Result<MCPResourceReadResult, McpError> {
        let timeout_dur = self.request_timeout();
        let params = serde_json::to_value(MCPResourceReadParams { uri: uri.to_string() })?;

        let transport = {
            let guard = self.transport.read().await;
            guard.clone()
        };
        let res_val = transport
            .request("resources/read", Some(params), timeout_dur)
            .await?;

        serde_json::from_value(res_val)
            .map_err(|e| McpError::Protocol(format!("Invalid resources/read response: {}", e)))
    }

    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<MCPToolCallResult, McpError> {
        let timeout_dur = self.request_timeout();

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
        unregister_active_session(&self.name);
        let guard = self.transport.read().await;
        guard.close().await
    }
}

/// Process-wide live MCP connections for `mcp://` URI resolution.
///
/// Mirrors TS `MCPManager.#connections` / `MCPManager.instance()`: the URI
/// handler is stateless and looks sessions up here. Runtime code registers a
/// connection after `connect_to_server` and unregisters on disconnect; this
/// module never connects on its own.
static ACTIVE_SESSIONS: LazyLock<Mutex<BTreeMap<String, Arc<McpConnection>>>> =
	LazyLock::new(|| Mutex::new(BTreeMap::new()));

fn lock_sessions() -> std::sync::MutexGuard<'static, BTreeMap<String, Arc<McpConnection>>> {
	ACTIVE_SESSIONS
		.lock()
		.unwrap_or_else(|poison| poison.into_inner())
}

/// Install or replace one live connection under its server name.
pub fn register_active_session(connection: Arc<McpConnection>) {
	let name = connection.name.clone();
	lock_sessions().insert(name, connection);
}

/// Drop one live connection by server name.
pub fn unregister_active_session(name: &str) -> Option<Arc<McpConnection>> {
	lock_sessions().remove(name)
}

/// Look up one live connection by server name.
#[must_use]
pub fn get_active_session(name: &str) -> Option<Arc<McpConnection>> {
	lock_sessions().get(name).cloned()
}

/// Sorted names of every live MCP server.
#[must_use]
pub fn list_active_sessions() -> Vec<String> {
	lock_sessions().keys().cloned().collect()
}

/// Clear every live connection. Test-only.
#[cfg(test)]
pub fn clear_active_sessions() {
	lock_sessions().clear();
}
