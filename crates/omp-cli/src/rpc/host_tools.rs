//! Host-tool bridge: RPC host tools exposed to the agent as native [`Tool`]s.
//!
//! The bridge mirrors `packages/coding-agent/src/modes/rpc/host-tools.ts`: the
//! host registers definitions, the adapter emits `host_tool_call`, and the
//! result arrives as a `host_tool_result` side-channel frame. Cancellation
//! emits `host_tool_cancel` and fails the call closed.

use std::{
	collections::HashMap,
	sync::{
		Arc, Mutex, MutexGuard,
		atomic::{AtomicU64, Ordering},
	},
};

use async_trait::async_trait;
use pi_agent::{
	state::{AgentContext, ToolResult},
	tools::{Tool, ToolError},
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{
	session::{FrameSink, Prerequisite},
	types::{RpcHostToolDefinition, RpcHostToolResult, RpcHostToolUpdate},
};

enum HostReply {
	Update(Value),
	Result(Value),
	Failed(String),
	Closed(String),
}

struct PendingCall {
	tx: mpsc::UnboundedSender<HostReply>,
}

/// Serializes host tool calls over an RPC frame sink.
pub struct HostToolBridge {
	sink:        Arc<dyn FrameSink>,
	definitions: Mutex<Vec<RpcHostToolDefinition>>,
	pending:     Mutex<HashMap<String, PendingCall>>,
	closed:      Mutex<Option<String>>,
	next_id:     AtomicU64,
}

impl std::fmt::Debug for HostToolBridge {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("HostToolBridge")
			.field("definitions", &self.definitions.lock().map(|d| d.len()).unwrap_or(0))
			.finish()
	}
}

/// Validate one batch of definitions exactly like `normalizeHostToolDefinitions`.
pub fn normalize_definitions(
	definitions: Vec<RpcHostToolDefinition>,
) -> Result<Vec<RpcHostToolDefinition>, String> {
	let mut normalized = Vec::with_capacity(definitions.len());
	for (index, definition) in definitions.into_iter().enumerate() {
		let name = definition.name.trim().to_string();
		if name.is_empty() {
			return Err(format!("Host tool at index {index} must provide a non-empty name"));
		}
		let description = definition.description.trim().to_string();
		if description.is_empty() {
			return Err(format!("Host tool \"{name}\" must provide a non-empty description"));
		}
		if !definition.parameters.is_object() {
			return Err(format!("Host tool \"{name}\" must provide a JSON Schema object"));
		}
		let label = definition
			.label
			.as_deref()
			.map(str::trim)
			.filter(|label| !label.is_empty())
			.unwrap_or(&name)
			.to_string();
		normalized.push(RpcHostToolDefinition {
			name,
			label: Some(label),
			description,
			parameters: definition.parameters,
			hidden: definition.hidden,
			load_mode: definition.load_mode,
		});
	}
	Ok(normalized)
}

impl HostToolBridge {
	#[must_use]
	pub fn new(sink: Arc<dyn FrameSink>) -> Self {
		Self {
			sink,
			definitions: Mutex::new(Vec::new()),
			pending: Mutex::new(HashMap::new()),
			closed: Mutex::new(None),
			next_id: AtomicU64::new(1),
		}
	}

	/// Replace the host tool set and return the adapters to mount.
	pub fn set_tools(
		self: &Arc<Self>,
		definitions: Vec<RpcHostToolDefinition>,
	) -> Result<Vec<Arc<dyn Tool>>, String> {
		let normalized = normalize_definitions(definitions)?;
		let mut unique = std::collections::BTreeSet::new();
		for definition in &normalized {
			if !unique.insert(definition.name.clone()) {
				return Err(format!("Duplicate host tool name: {}", definition.name));
			}
		}
		let mut store = self.lock_definitions();
		*store = normalized.clone();
		drop(store);
		Ok(normalized
			.into_iter()
			.map(|definition| {
				Arc::new(HostToolAdapter { definition, bridge: Arc::clone(self) }) as Arc<dyn Tool>
			})
			.collect())
	}

	/// Names currently registered, in registration order.
	#[must_use]
	pub fn tool_names(&self) -> Vec<String> {
		self
			.lock_definitions()
			.iter()
			.map(|definition| definition.name.clone())
			.collect()
	}

	/// Resolve an inbound `host_tool_result`; `false` when the id is unknown.
	pub fn handle_result(&self, frame: RpcHostToolResult) -> bool {
		let Some(pending) = self.lock_pending().remove(&frame.id) else {
			return false;
		};
		if frame.is_error {
			let text = text_content(&frame.result);
			let message = if text.is_empty() {
				"Host tool execution failed".to_string()
			} else {
				text
			};
			let _ = pending.tx.send(HostReply::Failed(message));
		} else {
			let _ = pending.tx.send(HostReply::Result(frame.result));
		}
		true
	}

	/// Resolve an inbound `host_tool_update`; `false` when the id is unknown.
	pub fn handle_update(&self, frame: RpcHostToolUpdate) -> bool {
		let pending = self.lock_pending();
		let Some(call) = pending.get(&frame.id) else {
			return false;
		};
		call.tx.send(HostReply::Update(frame.partial_result)).is_ok()
	}

	/// Fail every pending call and every future call (peer disconnected).
	pub fn close(&self, message: &str) {
		let mut closed = self.lock_closed();
		if closed.is_none() {
			*closed = Some(message.to_string());
		}
		drop(closed);
		let pending: Vec<PendingCall> =
			std::mem::take(&mut *self.lock_pending()).into_values().collect();
		for call in pending {
			let _ = call.tx.send(HostReply::Closed(message.to_string()));
		}
	}

	fn lock_definitions(&self) -> MutexGuard<'_, Vec<RpcHostToolDefinition>> {
		self.definitions.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	fn lock_pending(&self) -> MutexGuard<'_, HashMap<String, PendingCall>> {
		self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	fn lock_closed(&self) -> MutexGuard<'_, Option<String>> {
		self.closed.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}
}

/// Extract newline-joined text blocks from an `AgentToolResult`.
fn text_content(result: &Value) -> String {
	result
		.get("content")
		.and_then(Value::as_array)
		.map(|items| {
			items
				.iter()
				.filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
				.filter_map(|item| item.get("text").and_then(Value::as_str))
				.collect::<Vec<_>>()
				.join("\n")
		})
		.unwrap_or_default()
		.trim()
		.to_string()
}

/// Native tool adapter for one host definition.
pub struct HostToolAdapter {
	definition: RpcHostToolDefinition,
	bridge:     Arc<HostToolBridge>,
}

impl HostToolAdapter {
	/// Build the adapter from a shared bridge handle.
	#[must_use]
	pub fn new(definition: RpcHostToolDefinition, bridge: Arc<HostToolBridge>) -> Self {
		Self { definition, bridge }
	}
}

#[async_trait]
impl Tool for HostToolAdapter {
	fn name(&self) -> &str {
		&self.definition.name
	}

	fn description(&self) -> &str {
		&self.definition.description
	}

	fn schema(&self) -> Value {
		self.definition.parameters.clone()
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		self.execute_with_id("", args, ctx).await
	}
}

impl HostToolAdapter {
	async fn execute_with_id(
		&self,
		tool_call_id: &str,
		args: &Value,
		ctx: &AgentContext,
	) -> Result<ToolResult, ToolError> {
		let definition = &self.definition;
		if ctx.cancellation_token.is_cancelled() {
			return Err(ToolError::Aborted(format!(
				"Host tool \"{}\" was aborted",
				definition.name
			)));
		}
		if let Some(message) = self.bridge.lock_closed().clone() {
			return Err(ToolError::ExecutionFailed(message));
		}

		let id = format!(
			"host-{}-{}",
			std::process::id(),
			self.bridge.next_id.fetch_add(1, Ordering::Relaxed)
		);
		let (tx, mut rx) = mpsc::unbounded_channel();
		self
			.bridge
			.lock_pending()
			.insert(id.clone(), PendingCall { tx });
		self.bridge.sink.send(json!({
			"type": "host_tool_call",
			"id": id,
			"toolCallId": tool_call_id,
			"toolName": definition.name,
			"arguments": args,
		}));

		loop {
			let outcome = tokio::select! {
				reply = rx.recv() => reply,
				() = ctx.cancellation_token.cancelled() => {
					self.bridge.lock_pending().remove(&id);
					self.bridge.sink.send(json!({
						"type": "host_tool_cancel",
						"id": format!("host-cancel-{}", self.bridge.next_id.fetch_add(1, Ordering::Relaxed)),
						"targetId": id,
					}));
					return Err(ToolError::Aborted(format!(
						"Host tool \"{}\" was aborted",
						definition.name
					)));
				},
			};

			match outcome {
				// Native `Tool::execute` has no progress callback, so an update is
				// acknowledged and dropped instead of ending the wait early.
				Some(HostReply::Update(_)) => continue,
				Some(HostReply::Result(result)) => {
					let text = text_content(&result);
					return Ok(ToolResult {
						tool_call_id: tool_call_id.to_string(),
						content: text,
						is_error: false,
						terminate: false,
						details: Some(result),
					});
				},
				Some(HostReply::Failed(message)) => return Err(ToolError::ExecutionFailed(message)),
				Some(HostReply::Closed(message)) => return Err(ToolError::ExecutionFailed(message)),
				None => {
					self.bridge.lock_pending().remove(&id);
					return Err(ToolError::ExecutionFailed(format!(
						"Host tool \"{}\" channel closed before a result arrived",
						definition.name
					)));
				},
			}
		}
	}
}

/// The native seam host tools still need: `RunRequest` carries no extra tools.
pub fn mount_prerequisite() -> Prerequisite {
	Prerequisite::new(
		"host_tools_not_mounted",
		"host tools were validated and stored, but this build cannot mount them into the per-turn \
		 registry: RunRequest exposes no extra-tools field (native seam: RunRequest { extra_tools: \
		 Vec<Arc<dyn Tool>> } registered after build_registry in runtime::PiAgentRunner::run)",
	)
}

#[cfg(test)]
mod tests {
	use std::sync::mpsc;

	use pi_agent::CancellationToken;

	use super::*;

	#[derive(Default)]
	struct TestSink {
		tx: Mutex<Option<mpsc::Sender<Value>>>,
	}

	impl FrameSink for TestSink {
		fn send(&self, frame: Value) {
			if let Some(tx) = self.tx.lock().expect("sink").as_ref() {
				let _ = tx.send(frame);
			}
		}
	}

	fn definition(name: &str) -> RpcHostToolDefinition {
		RpcHostToolDefinition {
			name: name.to_string(),
			label: None,
			description: format!("{name} tool"),
			parameters: json!({ "type": "object", "properties": {} }),
			hidden: false,
			load_mode: None,
		}
	}

	#[test]
	fn validation_matches_upstream_messages() {
		let error = normalize_definitions(vec![RpcHostToolDefinition {
			name: "  ".into(),
			..definition("x")
		}])
		.expect_err("empty name");
		assert_eq!(error, "Host tool at index 0 must provide a non-empty name");

		let error = normalize_definitions(vec![RpcHostToolDefinition {
			description: " ".into(),
			..definition("web")
		}])
		.expect_err("empty description");
		assert_eq!(error, "Host tool \"web\" must provide a non-empty description");

		let error = normalize_definitions(vec![RpcHostToolDefinition {
			parameters: json!([]),
			..definition("web")
		}])
		.expect_err("bad schema");
		assert_eq!(error, "Host tool \"web\" must provide a JSON Schema object");

		let normalized = normalize_definitions(vec![definition(" web ")]).expect("valid");
		assert_eq!(normalized[0].name, "web");
		assert_eq!(normalized[0].label.as_deref(), Some("web"));
	}

	#[test]
	fn duplicate_names_are_rejected() {
		let sink = Arc::new(TestSink::default());
		let bridge = Arc::new(HostToolBridge::new(sink));
		let error = match bridge.set_tools(vec![definition("a"), definition("a")]) {
			Ok(_) => panic!("duplicate host tool names must be rejected"),
			Err(error) => error,
		};
		assert_eq!(error, "Duplicate host tool name: a");
	}

	#[test]
	fn host_call_round_trips_through_frames() {
		let (tx, rx) = mpsc::channel();
		let sink = Arc::new(TestSink { tx: Mutex::new(Some(tx)) });
		let bridge = Arc::new(HostToolBridge::new(sink));
		let adapter = HostToolAdapter::new(definition("web"), Arc::clone(&bridge));

		let runtime = tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.build()
			.expect("runtime");
		let ctx = AgentContext::new(std::env::temp_dir());
		let task = runtime.spawn(async move { adapter.execute_with_id("t1", &json!({ "q": "x" }), &ctx).await });

		let call = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("call frame");
		assert_eq!(call["type"], json!("host_tool_call"));
		assert_eq!(call["toolCallId"], json!("t1"));
		assert_eq!(call["toolName"], json!("web"));
		assert_eq!(call["arguments"], json!({ "q": "x" }));
		let id = call["id"].as_str().expect("id").to_string();

		assert!(bridge.handle_result(RpcHostToolResult {
			id,
			result: json!({ "content": [{ "type": "text", "text": "hello" }], "details": { "n": 1 } }),
			is_error: false,
		}));

		let result = runtime.block_on(task).expect("join").expect("tool result");
		assert_eq!(result.content, "hello");
		assert!(!result.is_error);
		assert_eq!(result.details.expect("details")["details"]["n"], json!(1));
	}

	#[test]
	fn host_error_and_unknown_ids_are_handled() {
		let (tx, rx) = mpsc::channel();
		let sink = Arc::new(TestSink { tx: Mutex::new(Some(tx)) });
		let bridge = Arc::new(HostToolBridge::new(sink));
		assert!(!bridge.handle_result(RpcHostToolResult {
			id: "missing".into(),
			result: json!({ "content": [] }),
			is_error: false,
		}));

		let adapter = HostToolAdapter::new(definition("web"), Arc::clone(&bridge));
		let runtime = tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.build()
			.expect("runtime");
		let ctx = AgentContext::new(std::env::temp_dir());
		let task = runtime.spawn(async move { adapter.execute_with_id("t2", &json!({}), &ctx).await });
		let call = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("call");
		let id = call["id"].as_str().expect("id").to_string();
		assert!(bridge.handle_result(RpcHostToolResult {
			id,
			result: json!({ "content": [{ "type": "text", "text": "boom" }] }),
			is_error: true,
		}));
		let error = runtime.block_on(task).expect("join").expect_err("error");
		assert_eq!(error.to_string(), "Execution failed: boom");
	}

	#[test]
	fn cancellation_emits_cancel_frame_and_fails_closed() {
		let (tx, rx) = mpsc::channel();
		let sink = Arc::new(TestSink { tx: Mutex::new(Some(tx)) });
		let bridge = Arc::new(HostToolBridge::new(sink));
		let adapter = HostToolAdapter::new(definition("web"), Arc::clone(&bridge));
		let runtime = tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.build()
			.expect("runtime");
		let token = CancellationToken::new();
		let mut ctx = AgentContext::new(std::env::temp_dir());
		ctx.cancellation_token = token.clone();
		let task = runtime.spawn(async move { adapter.execute_with_id("t3", &json!({}), &ctx).await });
		let _call = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("call");
		token.cancel();
		let error = runtime.block_on(task).expect("join").expect_err("aborted");
		assert!(error.to_string().contains("aborted"));
		let cancel = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("cancel frame");
		assert_eq!(cancel["type"], json!("host_tool_cancel"));
	}

	#[test]
	fn close_fails_pending_and_future_calls() {
		let (tx, rx) = mpsc::channel();
		let sink = Arc::new(TestSink { tx: Mutex::new(Some(tx)) });
		let bridge = Arc::new(HostToolBridge::new(sink));
		let adapter = HostToolAdapter::new(definition("web"), Arc::clone(&bridge));
		let runtime = tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.build()
			.expect("runtime");
		let ctx = AgentContext::new(std::env::temp_dir());
		let task = runtime.spawn(async move { adapter.execute_with_id("t4", &json!({}), &ctx).await });
		let _call = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("call");
		bridge.close("RPC client disconnected");
		let error = runtime.block_on(task).expect("join").expect_err("closed");
		assert_eq!(error.to_string(), "Execution failed: RPC client disconnected");
		assert_eq!(bridge.lock_closed().clone().as_deref(), Some("RPC client disconnected"));
	}
}
