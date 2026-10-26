use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::mcp::error::McpError;
use crate::mcp::transports::McpTransport;
use crate::mcp::types::{MCPStdioServerConfig, RequestIdFormat};

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

pub struct StdioTransport {
    child: Arc<Mutex<Option<Child>>>,
    stdin_tx: mpsc::Sender<String>,
    pending_requests: Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, McpError>>>>>,
    next_id: AtomicU64,
    request_id_format: RequestIdFormat,
    read_handle: Mutex<Option<JoinHandle<()>>>,
    closed: Arc<AtomicBool>,
}

impl StdioTransport {
    pub async fn spawn(config: &MCPStdioServerConfig) -> Result<Self, McpError> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args);

        if let Some(cwd) = &config.cwd {
            cmd.current_dir(cwd);
        }

        for (k, v) in &config.env {
            cmd.env(k, v);
        }

        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::null());

        let mut child = cmd.spawn().map_err(|e| {
            McpError::Transport(format!("Failed to spawn stdio process '{}': {}", config.command, e))
        })?;

        let stdout = child.stdout.take().ok_or_else(|| {
            McpError::Transport("Failed to capture stdio stdout".to_string())
        })?;
        let mut stdin = child.stdin.take().ok_or_else(|| {
            McpError::Transport("Failed to capture stdio stdin".to_string())
        })?;

        let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(128);
        let pending_requests: Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, McpError>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));

        let stdin_tx_reply = stdin_tx.clone();
        tokio::spawn(async move {
            while let Some(line) = stdin_rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        let pending_clone = Arc::clone(&pending_requests);
        let closed_reader = Arc::clone(&closed);
        let read_handle = tokio::spawn(async move {
            const MAX_MCP_FRAME_SIZE: usize = 16 * 1024 * 1024;
            use tokio::io::AsyncReadExt;
            let mut reader = stdout;
            let mut line_bytes = Vec::new();
            let mut chunk = [0u8; 8192];

            'outer: loop {
                let mut hit_newline = false;
                let mut full_line = Vec::new();
                let mut scanned_len = 0;

                while !hit_newline {
                    if let Some(pos) = line_bytes[scanned_len..].iter().position(|&b| b == b'\n') {
                        let pos = scanned_len + pos;
                        full_line = line_bytes[..pos].to_vec();
                        line_bytes.drain(..=pos);
                        hit_newline = true;
                        break;
                    }
                    scanned_len = line_bytes.len();

                    match reader.read(&mut chunk).await {
                        Ok(0) => {
                            if !line_bytes.is_empty() {
                                full_line = std::mem::take(&mut line_bytes);
                                hit_newline = true;
                                break;
                            }
                            break 'outer;
                        }
                        Ok(n) => {
                            if line_bytes.len() + n > MAX_MCP_FRAME_SIZE {
                                // Strictly drop connection before memory can grow
                                break 'outer;
                            }
                            line_bytes.extend_from_slice(&chunk[..n]);
                        }
                        Err(_) => break 'outer,
                    }
                }

                if !hit_newline {
                    break 'outer;
                }

                let line_str = String::from_utf8_lossy(&full_line);
                let trimmed = line_str.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let parsed: Value = match serde_json::from_str(trimmed) {
                    Ok(val) => val,
                    Err(_) => continue,
                };
                let id_val = parsed.get("id");
                let method_val = parsed.get("method").and_then(|v| v.as_str());

                if let Some(method) = method_val {
                    if let Some(id) = id_val {
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
                        let _ = stdin_tx_reply.send(format!("{}\n", resp)).await;
                    }
                    continue;
                }

                if let Some(id) = id_val {
                    let id_key = serde_json::to_string(id).unwrap_or_default();

                    let sender = {
                        let mut map = pending_clone.lock().unwrap();
                        map.remove(&id_key)
                    };

                    if let Some(tx) = sender {
                        if let Some(err) = parsed.get("error") {
                            let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
                            let message = err
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("Unknown JSON-RPC error")
                                .to_string();
                            let data = err.get("data").cloned();
                            let _ = tx.send(Err(McpError::JsonRpc { code, message, data }));
                        } else if let Some(result) = parsed.get("result") {
                            let _ = tx.send(Ok(result.clone()));
                        } else {
                            let _ = tx.send(Ok(Value::Null));
                        }
                    }
                }
            }

            closed_reader.store(true, Ordering::Release);
            let mut map = pending_clone.lock().unwrap();
            for (_, tx) in map.drain() {
                let _ = tx.send(Err(McpError::Closed));
            }
        });

        Ok(Self {
            child: Arc::new(Mutex::new(Some(child))),
            stdin_tx,
            pending_requests,
            next_id: AtomicU64::new(1),
            request_id_format: config.request_id_format.unwrap_or(RequestIdFormat::Number),
            read_handle: Mutex::new(Some(read_handle)),
            closed,
        })
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Option<Duration>,
    ) -> Result<Value, McpError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(McpError::Closed);
        }
        let id_num = self.next_id.fetch_add(1, Ordering::SeqCst);
        let id_val = match self.request_id_format {
            RequestIdFormat::Number => json!(id_num),
            RequestIdFormat::String => json!(format!("req_{}", id_num)),
        };
        let id_key = serde_json::to_string(&id_val).unwrap_or_else(|_| id_num.to_string());

        let req = json!({
            "jsonrpc": "2.0",
            "id": id_val,
            "method": method,
            "params": params.unwrap_or_else(|| json!({}))
        });

        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.pending_requests.lock().unwrap();
            map.insert(id_key.clone(), tx);
            if self.closed.load(Ordering::Acquire) {
                map.remove(&id_key);
                return Err(McpError::Closed);
            }
        }

        let mut guard = PendingGuard {
            map: &self.pending_requests,
            id_key: &id_key,
            active: true,
        };

        let stdin_tx = self.stdin_tx.clone();
        let send_and_recv = async move {
            let line = format!("{}\n", req);
            stdin_tx
                .send(line)
                .await
                .map_err(|e| McpError::Transport(format!("Failed to write to stdin: {}", e)))?;
            rx.await.map_err(|_| McpError::Closed)?
        };

        let res = if let Some(dur) = timeout {
            match tokio::time::timeout(dur, send_and_recv).await {
                Ok(res) => res,
                Err(_) => Err(McpError::Timeout(format!("Request '{}' timed out after {:?}", method, dur))),
            }
        } else {
            send_and_recv.await
        };

        if res.is_ok() {
            guard.active = false;
        }
        res
    }

    async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(McpError::Closed);
        }
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params.unwrap_or_else(|| json!({}))
        });
        let line = format!("{}\n", notification);
        self.stdin_tx
            .send(line)
            .await
            .map_err(|e| McpError::Transport(format!("Failed to send notification: {}", e)))?;
        Ok(())
    }

    async fn close(&self) -> Result<(), McpError> {
        self.closed.store(true, Ordering::Release);
        if let Some(handle) = self.read_handle.lock().unwrap().take() {
            handle.abort();
        }

        let child_opt = {
            let mut child_guard = self.child.lock().unwrap();
            child_guard.take()
        };
        if let Some(mut child) = child_opt {
            let _ = child.kill().await;
        }

        let mut map = self.pending_requests.lock().unwrap();
        for (_, tx) in map.drain() {
            let _ = tx.send(Err(McpError::Closed));
        }

        Ok(())
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        if let Some(handle) = self.read_handle.lock().unwrap().take() {
            handle.abort();
        }
        let mut child_guard = self.child.lock().unwrap();
        if let Some(mut child) = child_guard.take() {
            let _ = child.start_kill();
        }
        let mut map = self.pending_requests.lock().unwrap();
        for (_, tx) in map.drain() {
            let _ = tx.send(Err(McpError::Closed));
        }
    }
}
