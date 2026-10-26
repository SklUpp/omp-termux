use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::mcp::error::McpError;
use crate::mcp::timeout::resolve_mcp_timeout_ms;
use crate::mcp::transports::header_policy::{merge_mcp_headers, without_header};
use crate::mcp::transports::McpTransport;
use crate::mcp::types::{MCPHttpServerConfig, RequestIdFormat};

pub const MAX_MCP_FRAME_SIZE: usize = 16 * 1024 * 1024; // 16 MiB bound to prevent memory exhaustion

async fn read_bounded_body(mut response: reqwest::Response, max_bytes: usize) -> Result<String, McpError> {
    let mut buf = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| McpError::Transport(e.to_string()))? {
        if buf.len() + chunk.len() > max_bytes {
            return Err(McpError::Transport(format!(
                "Response body exceeded maximum bound of {} bytes",
                max_bytes
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&buf).to_string())
}

#[derive(Clone)]
pub struct HttpTransport {
    client: reqwest::Client,
    url: String,
    configured_headers: HashMap<String, String>,
    session_id: Arc<RwLock<Option<String>>>,
    protocol_version: Arc<RwLock<Option<String>>>,
    header_policy: Option<String>,
    next_id: Arc<AtomicU64>,
    request_id_format: RequestIdFormat,
    timeout_ms: u64,
}

impl HttpTransport {
    pub fn new(config: &MCPHttpServerConfig) -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();

        let timeout_ms = resolve_mcp_timeout_ms(config.timeout);

        Self {
            client,
            url: config.url.clone(),
            configured_headers: config.headers.clone(),
            session_id: Arc::new(RwLock::new(None)),
            protocol_version: Arc::new(RwLock::new(None)),
            header_policy: config.header_policy.clone(),
            next_id: Arc::new(AtomicU64::new(1)),
            request_id_format: config.request_id_format.unwrap_or(RequestIdFormat::Number),
            timeout_ms,
        }
    }

    fn check_origin(url1: &str, url2: &str) -> bool {
        let parse1 = reqwest::Url::parse(url1);
        let parse2 = reqwest::Url::parse(url2);
        match (parse1, parse2) {
            (Ok(u1), Ok(u2)) => u1.origin() == u2.origin(),
            _ => false,
        }
    }

    pub async fn post_with_redirect_policy(
        &self,
        target_initial: &str,
        body: &Value,
        generated_headers: &HashMap<String, String>,
        overall_timeout: Option<Duration>,
    ) -> Result<reqwest::Response, McpError> {
        let start_time = Instant::now();
        let configured_clean = without_header(&self.configured_headers, "mcp-protocol-version");
        let mut target_url = target_initial.to_string();
        let mut hops = 0;
        let is_origin_locked = self.header_policy.as_deref() == Some("origin-locked");

        loop {
            let hop_timeout = match overall_timeout {
                Some(total) => {
                    let elapsed = start_time.elapsed();
                    if elapsed >= total {
                        return Err(McpError::Timeout(format!(
                            "Request timed out during redirects after {:?}",
                            total
                        )));
                    }
                    Some(total - elapsed)
                }
                None => None,
            };

            let is_same_origin = Self::check_origin(&self.url, &target_url);
            let send_configured = if is_same_origin {
                &configured_clean
            } else {
                &HashMap::new()
            };

            let mut active_generated = generated_headers.clone();
            if !is_same_origin {
                // Security: Strip session-bound credentials on cross-origin redirects
                active_generated.remove("mcp-session-id");
                active_generated.remove("authorization");
            }

            let merged_headers = merge_mcp_headers(&active_generated, send_configured);
            let mut req_builder = self.client.post(&target_url).json(body);

            let mut header_map = HeaderMap::new();
            for (k, v) in merged_headers {
                if let (Ok(hname), Ok(hval)) = (
                    HeaderName::from_bytes(k.as_bytes()),
                    HeaderValue::from_str(&v),
                ) {
                    header_map.insert(hname, hval);
                }
            }
            req_builder = req_builder.headers(header_map);

            if let Some(dur) = hop_timeout {
                req_builder = req_builder.timeout(dur);
            }

            let resp = req_builder.send().await.map_err(|e| {
                if e.is_timeout() {
                    McpError::Timeout(format!("HTTP request timed out: {}", e))
                } else {
                    McpError::Transport(format!("HTTP request failed: {}", e))
                }
            })?;

            let status = resp.status();
            if status.is_redirection() {
                hops += 1;
                if hops > 5 {
                    return Err(McpError::Transport("Too many redirects (max 5)".to_string()));
                }

                if is_origin_locked && (status.as_u16() == 301 || status.as_u16() == 302 || status.as_u16() == 303) {
                    return Err(McpError::Transport(format!(
                        "Refusing non-method-preserving redirect {} for POST",
                        status.as_u16()
                    )));
                }

                if let Some(loc) = resp.headers().get("location") {
                    let loc_str = loc.to_str().map_err(|_| {
                        McpError::Transport("Invalid redirect location header".to_string())
                    })?;
                    target_url = match reqwest::Url::parse(loc_str) {
                        Ok(u) => u.to_string(),
                        Err(_) => match reqwest::Url::parse(&target_url) {
                            Ok(base) => base.join(loc_str).map_err(|_| {
                                McpError::Transport("Failed to resolve relative redirect".to_string())
                            })?.to_string(),
                            Err(_) => {
                                return Err(McpError::Transport("Invalid base URL for redirect".to_string()))
                            }
                        },
                    };

                    if is_origin_locked && !Self::check_origin(&self.url, &target_url) {
                        return Err(McpError::Transport(format!(
                            "Origin-locked policy forbids cross-origin redirect to: {}",
                            target_url
                        )));
                    }

                    continue;
                } else {
                    return Err(McpError::Transport("Redirect without location header".to_string()));
                }
            }

            return Ok(resp);
        }
    }

    pub async fn send_server_reply(&self, resp: Value) -> Result<(), McpError> {
        let mut generated_headers = HashMap::new();
        generated_headers.insert("content-type".to_string(), "application/json".to_string());
        if let Some(sid) = self.session_id.read().unwrap().as_ref() {
            generated_headers.insert("mcp-session-id".to_string(), sid.clone());
        }
        if let Some(pv) = self.protocol_version.read().unwrap().as_ref() {
            generated_headers.insert("mcp-protocol-version".to_string(), pv.clone());
        }
        let _ = self
            .post_with_redirect_policy(&self.url, &resp, &generated_headers, Some(Duration::from_secs(10)))
            .await;
        Ok(())
    }
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Option<Duration>,
    ) -> Result<Value, McpError> {
        let id_val = match self.request_id_format {
            RequestIdFormat::Number => json!(self.next_id.fetch_add(1, Ordering::SeqCst)),
            RequestIdFormat::String => {
                json!(format!("req_{}", self.next_id.fetch_add(1, Ordering::SeqCst)))
            }
        };

        let req_body = json!({
            "jsonrpc": "2.0",
            "id": id_val,
            "method": method,
            "params": params.unwrap_or_else(|| json!({}))
        });

        let start_time = Instant::now();

        let mut generated_headers = HashMap::new();
        generated_headers.insert("content-type".to_string(), "application/json".to_string());
        generated_headers.insert(
            "accept".to_string(),
            "application/json, text/event-stream".to_string(),
        );

        if let Some(sid) = self.session_id.read().unwrap().as_ref() {
            generated_headers.insert("mcp-session-id".to_string(), sid.clone());
        }
        if let Some(pv) = self.protocol_version.read().unwrap().as_ref() {
            generated_headers.insert("mcp-protocol-version".to_string(), pv.clone());
        }

        let remaining_timeout = timeout.map(|d| {
            let elapsed = start_time.elapsed();
            if elapsed >= d {
                Duration::from_millis(1)
            } else {
                d - elapsed
            }
        });

        let response = self
            .post_with_redirect_policy(&self.url, &req_body, &generated_headers, remaining_timeout)
            .await?;

        if let Some(sid_val) = response.headers().get("mcp-session-id") {
            if let Ok(s) = sid_val.to_str() {
                let mut sid = self.session_id.write().unwrap();
                *sid = Some(s.to_string());
            }
        }

        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if !status.is_success() {
            let body = read_bounded_body(response, MAX_MCP_FRAME_SIZE).await.unwrap_or_default();
            return Err(McpError::Http {
                status: status.as_u16(),
                message: body,
            });
        }

        if content_type.contains("text/event-stream") {
            let transport_self = self.clone();
            let read_sse_future = async move {
                let mut stream = response.bytes_stream();
                let mut buffer: Vec<u8> = Vec::new();
                let mut sse_data = String::new();

                while let Some(chunk_res) = stream.next().await {
                    let chunk = chunk_res.map_err(|e| {
                        if e.is_timeout() || e.to_string().to_lowercase().contains("timed out") {
                            McpError::Timeout(format!("SSE stream timed out: {}", e))
                        } else {
                            McpError::AcceptedStreamError(format!("SSE stream disconnected: {}", e))
                        }
                    })?;
                    buffer.extend_from_slice(&chunk);

                    if buffer.len() > MAX_MCP_FRAME_SIZE || sse_data.len() > MAX_MCP_FRAME_SIZE {
                        return Err(McpError::Transport(format!(
                            "SSE buffer exceeded maximum bound of {} bytes",
                            MAX_MCP_FRAME_SIZE
                        )));
                    }

                    while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                        let line_bytes = buffer[..pos].to_vec();
                        buffer.drain(..=pos);
                        let line = String::from_utf8_lossy(&line_bytes).trim_end_matches('\r').to_string();

                        if line.is_empty() {
                            if !sse_data.is_empty() {
                                let data_trimmed = sse_data.trim();
                                if data_trimmed != "[DONE]" {
                                    if let Ok(parsed) = serde_json::from_str::<Value>(data_trimmed) {
                                        let match_id = parsed.get("id");

                                        // Server-to-client request in SSE stream: preserve typed Value ID and send reply with transport policy in background
                                        if let (Some(id), Some(srv_method)) = (match_id, parsed.get("method").and_then(|v| v.as_str())) {
                                            let resp = match srv_method {
                                                "ping" => json!({
                                                    "jsonrpc": "2.0",
                                                    "id": id,
                                                    "result": {}
                                                }),
                                                "roots/list" => {
                                                    let cwd = std::env::current_dir()
                                                        .map(|p| p.to_string_lossy().to_string())
                                                        .unwrap_or_else(|_| ".".to_string());
                                                    json!({
                                                        "jsonrpc": "2.0",
                                                        "id": id,
                                                        "result": {
                                                            "roots": [{
                                                                "uri": format!("file://{}", cwd),
                                                                "name": "workspace"
                                                            }]
                                                        }
                                                    })
                                                }
                                                _ => json!({
                                                    "jsonrpc": "2.0",
                                                    "id": id,
                                                    "error": {
                                                        "code": -32601,
                                                        "message": format!("Method not found: {}", srv_method)
                                                    }
                                                }),
                                            };
                                            let reply_transport = transport_self.clone();
                                            tokio::spawn(async move {
                                                let _ = reply_transport.send_server_reply(resp).await;
                                            });
                                            sse_data.clear();
                                            continue;
                                        }

                                        // Typed Value equality (no type-erasure between 1 and "1")
                                        if match_id == Some(&id_val) {
                                            if let Some(err) = parsed.get("error") {
                                                let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
                                                let message = err
                                                    .get("message")
                                                    .and_then(|m| m.as_str())
                                                    .unwrap_or("JSON-RPC error")
                                                    .to_string();
                                                let data = err.get("data").cloned();
                                                return Err(McpError::JsonRpc { code, message, data });
                                            }
                                            return Ok(parsed.get("result").cloned().unwrap_or(Value::Null));
                                        }
                                    }
                                }
                                sse_data.clear();
                            }
                            continue;
                        }

                        if let Some(d) = line.strip_prefix("data:") {
                            if !sse_data.is_empty() {
                                sse_data.push('\n');
                            }
                            sse_data.push_str(d.trim());
                            if sse_data.len() > MAX_MCP_FRAME_SIZE {
                                return Err(McpError::Transport(format!(
                                    "SSE event data exceeded maximum bound of {} bytes",
                                    MAX_MCP_FRAME_SIZE
                                )));
                            }
                        }
                    }
                }

                Err(McpError::AcceptedStreamError(
                    "SSE stream ended without matching response ID".to_string(),
                ))
            };

            let remaining_for_stream = timeout.map(|d| {
                let elapsed = start_time.elapsed();
                if elapsed >= d {
                    Duration::from_millis(1)
                } else {
                    d - elapsed
                }
            });

            if let Some(dur) = remaining_for_stream {
                match tokio::time::timeout(dur, read_sse_future).await {
                    Ok(res) => res,
                    Err(_) => Err(McpError::Timeout(format!(
                        "HTTP SSE stream timed out after {:?}",
                        dur
                    ))),
                }
            } else {
                read_sse_future.await
            }
        } else {
            let full_text = read_bounded_body(response, MAX_MCP_FRAME_SIZE).await.map_err(|e| {
                McpError::AcceptedStreamError(format!("Failed to read accepted HTTP JSON response body: {}", e))
            })?;
            let parsed: Value = serde_json::from_str(&full_text).map_err(|e| {
                McpError::Protocol(format!("Failed to parse JSON response: {}", e))
            })?;

            let resp_id = parsed.get("id");
            if resp_id != Some(&id_val) {
                return Err(McpError::Protocol(format!(
                    "Mismatched JSON-RPC response ID: expected {:?}, got {:?}",
                    id_val, resp_id
                )));
            }

            if let Some(err) = parsed.get("error") {
                let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
                let message = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("JSON-RPC error")
                    .to_string();
                let data = err.get("data").cloned();
                return Err(McpError::JsonRpc { code, message, data });
            }

            Ok(parsed.get("result").cloned().unwrap_or(Value::Null))
        }
    }

    async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params.unwrap_or_else(|| json!({}))
        });

        let mut generated_headers = HashMap::new();
        generated_headers.insert("content-type".to_string(), "application/json".to_string());
        if let Some(sid) = self.session_id.read().unwrap().as_ref() {
            generated_headers.insert("mcp-session-id".to_string(), sid.clone());
        }
        if let Some(pv) = self.protocol_version.read().unwrap().as_ref() {
            generated_headers.insert("mcp-protocol-version".to_string(), pv.clone());
        }

        let timeout_dur = if self.timeout_ms > 0 {
            Some(Duration::from_millis(self.timeout_ms))
        } else {
            None
        };

        let resp = self
            .post_with_redirect_policy(&self.url, &notification, &generated_headers, timeout_dur)
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            return Err(McpError::Http {
                status: status.as_u16(),
                message: read_bounded_body(resp, MAX_MCP_FRAME_SIZE).await.unwrap_or_default(),
            });
        }

        Ok(())
    }

    async fn close(&self) -> Result<(), McpError> {
        Ok(())
    }

    fn set_protocol_version(&self, version: &str) {
        let mut pv = self.protocol_version.write().unwrap();
        *pv = Some(version.to_string());
    }
}
