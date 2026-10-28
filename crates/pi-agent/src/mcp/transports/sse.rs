use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::mcp::error::McpError;
use crate::mcp::timeout::resolve_mcp_timeout_ms;
use crate::mcp::transports::header_policy::{merge_mcp_headers, without_header};
use crate::mcp::transports::McpTransport;
use crate::mcp::types::{MCPSseServerConfig, RequestIdFormat};

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

struct PendingGuard<'a> {
    map: &'a Mutex<HashMap<String, oneshot::Sender<Result<Value, McpError>>>>,
    id_key: &'a str,
    active: bool,
}

impl<'a> Drop for PendingGuard<'a> {
    fn drop(&mut self) {
        if self.active {
            let mut guard = self.map.lock().unwrap();
            guard.remove(self.id_key);
        }
    }
}

struct InnerSseTransport {
    client: reqwest::Client,
    url: String,
    configured_headers: HashMap<String, String>,
    endpoint_url: RwLock<Option<String>>,
    pending_requests: Mutex<HashMap<String, oneshot::Sender<Result<Value, McpError>>>>,
    next_id: AtomicU64,
    request_id_format: RequestIdFormat,
    header_policy: Option<String>,
    timeout_ms: u64,
    sse_handle: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for InnerSseTransport {
    fn drop(&mut self) {
        if let Some(handle) = self.sse_handle.lock().unwrap().take() {
            handle.abort();
        }
        let mut ep = self.endpoint_url.write().unwrap();
        *ep = None;
        let mut map = self.pending_requests.lock().unwrap();
        for (_, tx) in map.drain() {
            let _ = tx.send(Err(McpError::Closed));
        }
    }
}

#[derive(Clone)]
pub struct LegacySseTransport {
    inner: Arc<InnerSseTransport>,
}

impl LegacySseTransport {
    pub fn new(config: &MCPSseServerConfig) -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();

        let timeout_ms = resolve_mcp_timeout_ms(config.timeout);

        Self {
            inner: Arc::new(InnerSseTransport {
                client,
                url: config.url.clone(),
                configured_headers: config.headers.clone(),
                endpoint_url: RwLock::new(None),
                pending_requests: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                request_id_format: config.request_id_format.unwrap_or(RequestIdFormat::Number),
                header_policy: config.header_policy.clone(),
                timeout_ms,
                sse_handle: Mutex::new(None),
            }),
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

    async fn get_with_redirect_policy(
        &self,
        target_initial: &str,
        generated_headers: &HashMap<String, String>,
        overall_timeout: Option<Duration>,
    ) -> Result<reqwest::Response, McpError> {
        let start_time = Instant::now();
        let configured_clean = without_header(&self.inner.configured_headers, "mcp-protocol-version");
        let mut target_url = target_initial.to_string();
        let mut hops = 0;
        let is_origin_locked = self.inner.header_policy.as_deref() == Some("origin-locked");

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

            let is_same_origin = Self::check_origin(&self.inner.url, &target_url);
            let send_configured = if is_same_origin {
                &configured_clean
            } else {
                &HashMap::new()
            };

            let mut active_generated = generated_headers.clone();
            if !is_same_origin {
                // Security: Strip session credentials on cross-origin redirects
                active_generated.remove("mcp-session-id");
                active_generated.remove("authorization");
            }

            let merged_headers = merge_mcp_headers(&active_generated, send_configured);
            let mut req_builder = self.inner.client.get(&target_url);

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
                    McpError::Timeout(format!("Legacy SSE GET timed out: {}", e))
                } else {
                    McpError::Transport(format!("Legacy SSE GET failed: {}", e))
                }
            })?;

            let status = resp.status();
            if status.is_redirection() {
                hops += 1;
                if hops > 5 {
                    return Err(McpError::Transport("Too many redirects (max 5)".to_string()));
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

                    if is_origin_locked && !Self::check_origin(&self.inner.url, &target_url) {
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

    pub async fn post_with_redirect_policy(
        &self,
        target_initial: &str,
        body: &Value,
        generated_headers: &HashMap<String, String>,
        overall_timeout: Option<Duration>,
    ) -> Result<reqwest::Response, McpError> {
        let start_time = Instant::now();
        let configured_clean = without_header(&self.inner.configured_headers, "mcp-protocol-version");
        let mut target_url = target_initial.to_string();
        let mut hops = 0;
        let is_origin_locked = self.inner.header_policy.as_deref() == Some("origin-locked");

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

            let is_same_origin = Self::check_origin(&self.inner.url, &target_url);
            let send_configured = if is_same_origin {
                &configured_clean
            } else {
                &HashMap::new()
            };

            let mut active_generated = generated_headers.clone();
            if !is_same_origin {
                // Security: Strip session credentials on cross-origin redirects
                active_generated.remove("mcp-session-id");
                active_generated.remove("authorization");
            }

            let merged_headers = merge_mcp_headers(&active_generated, send_configured);
            let mut req_builder = self.inner.client.post(&target_url).json(body);

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
                    McpError::Timeout(format!("Legacy SSE POST timed out: {}", e))
                } else {
                    McpError::Transport(format!("Legacy SSE POST failed: {}", e))
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

                    if is_origin_locked && !Self::check_origin(&self.inner.url, &target_url) {
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
        let endpoint = match self.inner.endpoint_url.read().unwrap().as_ref() {
            Some(ep) => ep.clone(),
            None => return Err(McpError::Closed),
        };
        let mut generated_headers = HashMap::new();
        generated_headers.insert("content-type".to_string(), "application/json".to_string());
        let _ = self
            .post_with_redirect_policy(&endpoint, &resp, &generated_headers, Some(Duration::from_secs(10)))
            .await;
        Ok(())
    }

    pub async fn connect(&self) -> Result<(), McpError> {
        if self.inner.endpoint_url.read().unwrap().is_some() {
            return Ok(());
        }

        let timeout_dur = if self.inner.timeout_ms > 0 {
            Some(Duration::from_millis(self.inner.timeout_ms))
        } else {
            None
        };
        let connect_fut = async {
            let mut generated_headers = HashMap::new();
            generated_headers.insert("accept".to_string(), "text/event-stream".to_string());

            let resp = self
                .get_with_redirect_policy(&self.inner.url, &generated_headers, timeout_dur)
                .await?;

            if !resp.status().is_success() {
                return Err(McpError::Http {
                    status: resp.status().as_u16(),
                    message: read_bounded_body(resp, MAX_MCP_FRAME_SIZE).await.unwrap_or_default(),
                });
            }

            let mut stream = resp.bytes_stream();
            let mut buffer: Vec<u8> = Vec::new();
            let mut endpoint_str: Option<String> = None;

            let mut current_event = String::new();
            let mut current_data = String::new();

            while let Some(chunk_res) = stream.next().await {
                let chunk = chunk_res.map_err(|e| {
                    if e.is_timeout()
                        || e.to_string().to_lowercase().contains("timed out")
                        || e.to_string().contains("error decoding response body")
                    {
                        McpError::Timeout(format!("SSE stream timed out: {}", e))
                    } else {
                        McpError::Transport(format!("SSE stream error: {}", e))
                    }
                })?;
                buffer.extend_from_slice(&chunk);

                if buffer.len() > MAX_MCP_FRAME_SIZE {
                    return Err(McpError::Transport(format!(
                        "SSE buffer exceeded maximum bound of {} bytes",
                        MAX_MCP_FRAME_SIZE
                    )));
                }

                let mut found = false;
                while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                    let line_bytes = buffer[..pos].to_vec();
                    buffer.drain(..=pos);
                    let line = String::from_utf8_lossy(&line_bytes).trim_end_matches('\r').to_string();

                    if line.is_empty() {
                        if current_event == "endpoint" && !current_data.is_empty() {
                            endpoint_str = Some(current_data.trim().to_string());
                            found = true;
                            break;
                        }
                        current_event.clear();
                        current_data.clear();
                        continue;
                    }

                    if let Some(ev) = line.strip_prefix("event:") {
                        current_event = ev.trim().to_string();
                    } else if let Some(d) = line.strip_prefix("data:") {
                        if !current_data.is_empty() {
                            current_data.push('\n');
                        }
                        current_data.push_str(d.trim());
                        if current_data.len() > MAX_MCP_FRAME_SIZE {
                            return Err(McpError::Transport(format!(
                                "SSE event data exceeded maximum bound of {} bytes",
                                MAX_MCP_FRAME_SIZE
                            )));
                        }
                    }
                }

                if found {
                    break;
                }
            }

            let raw_endpoint = endpoint_str.ok_or_else(|| {
                McpError::Transport("SSE stream ended before receiving 'endpoint' event".to_string())
            })?;

            Ok::<_, McpError>((raw_endpoint, stream, buffer))
        };

        let (raw_endpoint, mut stream, mut buffer) = match timeout_dur {
            Some(d) => tokio::time::timeout(d, connect_fut)
                .await
                .map_err(|_| McpError::Timeout("Legacy SSE connection timed out waiting for endpoint event".to_string()))??,
            None => connect_fut.await?,
        };

        let base_url = reqwest::Url::parse(&self.inner.url)
            .map_err(|e| McpError::Transport(format!("Invalid configured URL: {}", e)))?;
        let resolved_endpoint = base_url
            .join(&raw_endpoint)
            .map_err(|e| McpError::Transport(format!("Invalid endpoint URL: {}", e)))?;

        if resolved_endpoint.origin() != base_url.origin() {
            return Err(McpError::Transport(format!(
                "Legacy SSE endpoint origin mismatch: {} vs {}",
                resolved_endpoint.origin().ascii_serialization(),
                base_url.origin().ascii_serialization()
            )));
        }

        let endpoint_final = resolved_endpoint.to_string();
        {
            let mut ep = self.inner.endpoint_url.write().unwrap();
            *ep = Some(endpoint_final.clone());
        }

        let weak_inner = Arc::downgrade(&self.inner);
        let handle = tokio::spawn(async move {
            let mut sse_event = String::new();
            let mut sse_data = String::new();

            while let Some(chunk_res) = stream.next().await {
                let chunk = match chunk_res {
                    Ok(c) => c,
                    Err(_) => break,
                };
                buffer.extend_from_slice(&chunk);

                if buffer.len() > MAX_MCP_FRAME_SIZE || sse_data.len() > MAX_MCP_FRAME_SIZE {
                    break;
                }

                while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                    let line_bytes = buffer[..pos].to_vec();
                    buffer.drain(..=pos);
                    let line = String::from_utf8_lossy(&line_bytes).trim_end_matches('\r').to_string();

                    if line.is_empty() {
                        if !sse_data.is_empty() {
                            let data_trimmed = sse_data.trim();
                            if let Ok(parsed) = serde_json::from_str::<Value>(data_trimmed) {
                                let id_val = parsed.get("id");
                                let method_val = parsed.get("method").and_then(|v| v.as_str());

                                if let (Some(id), Some(method)) = (id_val, method_val) {
                                    let resp = match method {
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
                                                "message": format!("Method not found: {}", method)
                                            }
                                        }),
                                    };
                                    if let Some(inner) = weak_inner.upgrade() {
                                        let reply_transport = LegacySseTransport { inner };
                                        tokio::spawn(async move {
                                            let _ = reply_transport.send_server_reply(resp).await;
                                        });
                                    }
                                } else if let Some(id) = id_val {
                                    if let Some(inner) = weak_inner.upgrade() {
                                        let id_key = serde_json::to_string(id).unwrap_or_default();
                                        let tx = {
                                            let mut map = inner.pending_requests.lock().unwrap();
                                            map.remove(&id_key)
                                        };
                                        if let Some(sender) = tx {
                                            if let Some(err) = parsed.get("error") {
                                                let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
                                                let message = err
                                                    .get("message")
                                                    .and_then(|m| m.as_str())
                                                    .unwrap_or("JSON-RPC error")
                                                    .to_string();
                                                let data = err.get("data").cloned();
                                                let _ = sender.send(Err(McpError::JsonRpc { code, message, data }));
                                            } else {
                                                let _ = sender.send(Ok(parsed.get("result").cloned().unwrap_or(Value::Null)));
                                            }
                                        }
                                    }
                                }
                            }
                            sse_data.clear();
                        }
                        sse_event.clear();
                        continue;
                    }

                    if let Some(ev) = line.strip_prefix("event:") {
                        sse_event = ev.trim().to_string();
                    } else if let Some(d) = line.strip_prefix("data:") {
                        if !sse_data.is_empty() {
                            sse_data.push('\n');
                        }
                        sse_data.push_str(d.trim());
                        if sse_data.len() > MAX_MCP_FRAME_SIZE {
                            break;
                        }
                    }
                }
            }

            if let Some(inner) = weak_inner.upgrade() {
                let mut ep = inner.endpoint_url.write().unwrap();
                *ep = None;
                let mut map = inner.pending_requests.lock().unwrap();
                // The POSTs behind these pending requests were already accepted
                // when the event stream died: delivery is unverified. Requests
                // that never left the client still see `McpError::Closed`.
                for (_, tx) in map.drain() {
                    let _ = tx.send(Err(McpError::Transport(
                        "Connection lost while awaiting response".to_string(),
                    )));
                }
            }
        });

        {
            let mut sh = self.inner.sse_handle.lock().unwrap();
            *sh = Some(handle);
        }

        Ok(())
    }
}

#[async_trait]
impl McpTransport for LegacySseTransport {
    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Option<Duration>,
    ) -> Result<Value, McpError> {
        let maybe_ep = self.inner.endpoint_url.read().unwrap().clone();
        let endpoint = match maybe_ep {
            Some(ep) => ep,
            None => {
                self.connect().await?;
                match self.inner.endpoint_url.read().unwrap().clone() {
                    Some(ep) => ep,
                    None => return Err(McpError::Closed),
                }
            }
        };

        let id_val = match self.inner.request_id_format {
            RequestIdFormat::Number => json!(self.inner.next_id.fetch_add(1, Ordering::SeqCst)),
            RequestIdFormat::String => {
                json!(format!("req_{}", self.inner.next_id.fetch_add(1, Ordering::SeqCst)))
            }
        };
        let id_key = serde_json::to_string(&id_val).map_err(McpError::Json)?;

        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.inner.pending_requests.lock().unwrap();
            map.insert(id_key.clone(), tx);
        }

        let mut guard = PendingGuard {
            map: &self.inner.pending_requests,
            id_key: &id_key,
            active: true,
        };

        let req_body = json!({
            "jsonrpc": "2.0",
            "id": id_val,
            "method": method,
            "params": params.unwrap_or_else(|| json!({}))
        });

        let mut generated_headers = HashMap::new();
        generated_headers.insert("content-type".to_string(), "application/json".to_string());

        let post_res = self
            .post_with_redirect_policy(&endpoint, &req_body, &generated_headers, timeout)
            .await;

        let resp = match post_res {
            Ok(r) => r,
            Err(e) => return Err(e),
        };

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = read_bounded_body(resp, MAX_MCP_FRAME_SIZE).await.unwrap_or_default();
            return Err(McpError::Http {
                status,
                message: body,
            });
        }

        let dur = match timeout {
            Some(d) => d,
            None => {
                return rx
                    .await
                    .map_err(|_| McpError::Transport("Connection lost while awaiting response".to_string()))?
                    .map(|v| {
                        guard.active = false;
                        v
                    });
            }
        };

        match tokio::time::timeout(dur, rx).await {
            Ok(res) => res
                .map_err(|_| McpError::Transport("Connection lost while awaiting response".to_string()))?
                .map(|v| {
                    guard.active = false;
                    v
                }),
            Err(_) => Err(McpError::Timeout(format!(
                "Legacy SSE request '{}' timed out after {:?}",
                method, dur
            ))),
        }
    }
    async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        let maybe_ep = self.inner.endpoint_url.read().unwrap().clone();
        let endpoint = match maybe_ep {
            Some(ep) => ep,
            None => {
                self.connect().await?;
                match self.inner.endpoint_url.read().unwrap().clone() {
                    Some(ep) => ep,
                    None => return Err(McpError::Closed),
                }
            }
        };

        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params.unwrap_or_else(|| json!({}))
        });

        let mut generated_headers = HashMap::new();
        generated_headers.insert("content-type".to_string(), "application/json".to_string());

        let timeout_dur = if self.inner.timeout_ms > 0 {
            Some(Duration::from_millis(self.inner.timeout_ms))
        } else {
            None
        };

        let resp = self
            .post_with_redirect_policy(&endpoint, &notification, &generated_headers, timeout_dur)
            .await?;

        if !resp.status().is_success() {
            return Err(McpError::Http {
                status: resp.status().as_u16(),
                message: read_bounded_body(resp, MAX_MCP_FRAME_SIZE).await.unwrap_or_default(),
            });
        }

        Ok(())
    }

    async fn close(&self) -> Result<(), McpError> {
        if let Some(handle) = self.inner.sse_handle.lock().unwrap().take() {
            handle.abort();
        }
        {
            let mut ep = self.inner.endpoint_url.write().unwrap();
            *ep = None;
        }
        let mut map = self.inner.pending_requests.lock().unwrap();
        for (_, tx) in map.drain() {
            let _ = tx.send(Err(McpError::Closed));
        }
        Ok(())
    }
}
