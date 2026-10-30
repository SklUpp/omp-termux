//! ACP method dispatch and the stdio entry point.
//!
//! Ported from `modes/acp/acp-agent.ts` method table and
//! `utils/src/acp/connection.ts:240-275`. The transport owns framing and
//! writer ordering; this module owns session lookup, the `_omp/*` extensions
//! and close/EOF teardown. `session/cancel` is signalled from the reader hook
//! (`notify_control`) *and* through normal dispatch, so a wedged handler queue
//! cannot hide a cancel; the hook never consumes the frame and never writes.

use std::{
	io::{self, BufRead, Write},
	sync::{Arc, Mutex, OnceLock, PoisonError},
	time::Duration,
};

use serde_json::{Value, json};

use super::{
	capabilities,
	protocol::{self, ClientCapabilities, INTERNAL_ERROR, RequestError},
	session::{
		AcpRecipe, AcpRuntime, AcpSession, SessionManager, absolute_cwd, extensions_payload,
		list_payload, usage_payload,
	},
	session_list,
	transport::{Dispatcher, Transport},
};

/// Live ACP connection. One per process.
pub struct AcpAgent {
	manager: Arc<SessionManager>,
	caps: Mutex<ClientCapabilities>,
	transport: OnceLock<Transport>,
	runtime: OnceLock<AcpRuntime>,
}

impl AcpAgent {
	fn new() -> Arc<Self> {
		Arc::new(Self {
			manager: Arc::new(SessionManager::new()),
			caps: Mutex::new(ClientCapabilities::default()),
			transport: OnceLock::new(),
			runtime: OnceLock::new(),
		})
	}

	#[must_use]
	pub fn manager(&self) -> &Arc<SessionManager> {
		&self.manager
	}

	#[must_use]
	pub fn transport(&self) -> Option<&Transport> {
		self.transport.get()
	}

	fn runtime(&self) -> &AcpRuntime {
		self
			.runtime
			.get()
			.expect("ACP runtime installed before dispatch")
	}

	fn caps(&self) -> ClientCapabilities {
		self
			.caps
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.clone()
	}

	fn session(&self, params: Option<&Value>) -> Result<Arc<AcpSession>, RequestError> {
		let id = params
			.and_then(|params| params.get("sessionId"))
			.and_then(Value::as_str)
			.ok_or_else(RequestError::invalid_params)?;
		self
			.manager
			.get(id)
			.ok_or_else(|| RequestError::new(INTERNAL_ERROR, format!("Unsupported ACP session: {id}")))
	}

	fn dispatch_request(
		&self,
		method: &str,
		params: Option<&Value>,
		notification: bool,
	) -> Result<Value, RequestError> {
		match method {
			"initialize" => {
				let caps = params
					.and_then(|params| params.get("clientCapabilities"))
					.cloned()
					.map(|value| serde_json::from_value::<ClientCapabilities>(value).unwrap_or_default())
					.unwrap_or_default();
				*self.caps.lock().unwrap_or_else(PoisonError::into_inner) = caps.clone();
				Ok(capabilities::initialize_result(&caps))
			},
			"authenticate" => {
				let method_id = params
					.and_then(|params| params.get("methodId"))
					.and_then(Value::as_str)
					.ok_or_else(RequestError::invalid_params)?;
				capabilities::authenticate(method_id, &self.caps())
			},
			"session/new" => {
				let cwd = params
					.and_then(|params| params.get("cwd"))
					.and_then(Value::as_str)
					.ok_or_else(RequestError::invalid_params)?;
				let cwd = absolute_cwd(cwd)?;
				let mcp = mcp_servers(params)?;
				let session = self.runtime().new_session(cwd, &self.caps(), mcp)?;
				let response = session.protocol_response();
				self.manager.insert(Arc::clone(&session));
				session.schedule_bootstrap();
				Ok(response)
			},
			"session/load" => {
				let id = session_id(params)?;
				let cwd = absolute_cwd(cwd_param(params)?)?;
				let mcp = mcp_servers(params)?;
				let session = match self.manager.get(&id) {
					Some(session) => {
						if session.cwd() != cwd.as_path() {
							return Err(RequestError::new(
								INTERNAL_ERROR,
								format!(
									"ACP session {id} is already loaded for {}, not {}",
									session.cwd().display(),
									cwd.display()
								),
							));
						}
						session.set_mcp_servers(mcp);
						session
					},
					None => {
						let session = self.runtime().load_session(&id, cwd, &self.caps(), mcp)?;
						self.manager.insert(Arc::clone(&session));
						session
					},
				};
				session.replay_history();
				let response = json!({
					"configOptions": session.config_options(),
					"modes": session.mode_state(),
				});
				session.schedule_bootstrap();
				Ok(response)
			},
			"session/list" => {
				let cwd = params
					.and_then(|params| params.get("cwd"))
					.and_then(Value::as_str)
					.filter(|cwd| !cwd.is_empty())
					.map(absolute_cwd)
					.transpose()?;
				let cursor = params
					.and_then(|params| params.get("cursor"))
					.and_then(Value::as_str);
				list_payload(&self.manager, cwd.as_deref(), cursor, &self.runtime().sessions_root())
			},
			"session/resume" => {
				let id = session_id(params)?;
				let cwd = absolute_cwd(cwd_param(params)?)?;
				let mcp = mcp_servers(params)?;
				let session = match self.manager.get(&id) {
					Some(session) => {
						if session.cwd() != cwd.as_path() {
							return Err(RequestError::new(
								INTERNAL_ERROR,
								format!(
									"ACP session {id} is already loaded for {}, not {}",
									session.cwd().display(),
									cwd.display()
								),
							));
						}
						session.set_mcp_servers(mcp);
						session
					},
					None => {
						let session = self.runtime().load_session(&id, cwd, &self.caps(), mcp)?;
						self.manager.insert(Arc::clone(&session));
						session
					},
				};
				let response = json!({
					"configOptions": session.config_options(),
					"modes": session.mode_state(),
				});
				session.schedule_bootstrap();
				Ok(response)
			},
			"session/fork" => {
				let id = session_id(params)?;
				let cwd = absolute_cwd(cwd_param(params)?)?;
				let mcp = mcp_servers(params)?;
				let source = match self.manager.get(&id) {
					Some(session) => session,
					None => {
						let session =
							self.runtime().load_session(&id, cwd.clone(), &self.caps(), mcp.clone())?;
						session
					},
				};
				if source.is_prompt_in_flight() {
					return Err(RequestError::new(
						INTERNAL_ERROR,
						format!("ACP session fork is unavailable while a prompt is in progress: {id}"),
					));
				}
				let forked = self.runtime().fork_session(&source, cwd, &self.caps(), mcp)?;
				let response = json!({
					"sessionId": forked.id(),
					"configOptions": forked.config_options(),
					"modes": forked.mode_state(),
				});
				self.manager.insert(Arc::clone(&forked));
				forked.schedule_bootstrap();
				Ok(response)
			},
			"session/close" => {
				let id = session_id(params)?;
				if let Some(session) = self.manager.remove(&id) {
					session.close("session/close")?;
				}
				Ok(json!({}))
			},
			"session/set_mode" => {
				let session = self.session(params)?;
				let mode_id = params
					.and_then(|params| params.get("modeId"))
					.and_then(Value::as_str)
					.ok_or_else(RequestError::invalid_params)?;
				session.set_mode(mode_id)?;
				Ok(json!({}))
			},
			"session/set_config_option" => {
				let session = self.session(params)?;
				let config_id = params
					.and_then(|params| params.get("configId"))
					.and_then(Value::as_str)
					.ok_or_else(RequestError::invalid_params)?;
				let value = params
					.and_then(|params| params.get("value"))
					.ok_or_else(RequestError::invalid_params)?;
				session.set_config_option(config_id, value)
			},
			"session/prompt" => {
				let session = self.session(params)?;
				let prompt = params
					.and_then(|params| params.get("prompt"))
					.ok_or_else(RequestError::invalid_params)?;
				session.prompt(prompt)
			},
			"session/cancel" => {
				if let Ok(session) = self.session(params) {
					session.signal_cancel();
				}
				Ok(json!({}))
			},
			method if method.starts_with("_omp/") || method == "speech.models.list" => {
				if notification {
					return Ok(json!({}));
				}
				self.ext_method(method, params)
			},
			_ if notification => Ok(json!({})),
			_ => Err(RequestError::method_not_found(method)),
		}
	}

	fn ext_method(&self, method: &str, params: Option<&Value>) -> Result<Value, RequestError> {
		let root = self.runtime().sessions_root();
		match method {
			"speech.models.list" => Ok(capabilities::speech_models_catalog()),
			"_omp/sessions/listAll" => {
				let limit = params
					.and_then(|params| params.get("limit"))
					.and_then(Value::as_u64)
					.map(|limit| limit.clamp(1, 5_000) as usize)
					.unwrap_or(1_000);
				let all = session_list::list_sessions(None, Some(&root));
				let total = all.len();
				let sessions: Vec<Value> = all
					.iter()
					.take(limit)
					.map(session_list::session_info)
					.collect();
				Ok(json!({ "sessions": sessions, "total": total }))
			},
			"_omp/projects/list" => {
				let all = session_list::list_sessions(None, Some(&root));
				let (projects, total_sessions) = session_list::projects(&all);
				Ok(json!({ "projects": projects, "totalSessions": total_sessions }))
			},
			"_omp/chats/byCwd" => {
				let cwd = params
					.and_then(|params| params.get("cwd"))
					.and_then(Value::as_str)
					.ok_or_else(|| RequestError::new(INTERNAL_ERROR, "cwd required"))?;
				let limit = params
					.and_then(|params| params.get("limit"))
					.and_then(Value::as_u64)
					.map(|limit| limit.clamp(1, 500) as usize)
					.unwrap_or(100);
				let all = session_list::list_sessions(None, Some(&root));
				let sessions: Vec<Value> = all
					.iter()
					.filter(|session| session.cwd == cwd)
					.take(limit)
					.map(session_list::session_info)
					.collect();
				Ok(json!({ "sessions": sessions }))
			},
			"_omp/usage" => Ok(usage_payload()),
			"_omp/extensions" => Ok(extensions_payload()),
			"_omp/extensions/toggle" => {
				let provider_id = params
					.and_then(|params| params.get("providerId"))
					.and_then(Value::as_str)
					.ok_or_else(|| RequestError::new(INTERNAL_ERROR, "providerId required"))?;
				Err(capabilities::extension_toggle_error(provider_id))
			},
			other => Err(capabilities::unknown_ext_method(other)),
		}
	}
}

fn session_id(params: Option<&Value>) -> Result<String, RequestError> {
	params
		.and_then(|params| params.get("sessionId"))
		.and_then(Value::as_str)
		.map(str::to_string)
		.ok_or_else(RequestError::invalid_params)
}

fn cwd_param(params: Option<&Value>) -> Result<&str, RequestError> {
	params
		.and_then(|params| params.get("cwd"))
		.and_then(Value::as_str)
		.ok_or_else(RequestError::invalid_params)
}

/// Parse ACP `mcpServers` into the native discovery shape.
///
/// Accepts exactly the transports the native manager launches (`stdio`, `http`,
/// `sse`); any other `type` fails, matching the upstream defensive reject.
/// Validation and env expansion reuse [`crate::config::parse_and_validate_mcp_server`],
/// so client-supplied servers pass the same schema gate as configured ones.
fn mcp_servers(
	params: Option<&Value>,
) -> Result<Vec<crate::config::DiscoveredMcpServer>, RequestError> {
	let Some(servers) = params
		.and_then(|params| params.get("mcpServers"))
		.and_then(Value::as_array)
	else {
		return Ok(Vec::new());
	};
	let mut parsed = Vec::with_capacity(servers.len());
	let mut seen = std::collections::BTreeSet::new();
	for server in servers {
		let name = server
			.get("name")
			.and_then(Value::as_str)
			.ok_or_else(|| RequestError::new(INTERNAL_ERROR, "mcpServers entry requires a name"))?;
		if !crate::config::is_valid_mcp_server_name(name) {
			return Err(RequestError::new(
				INTERNAL_ERROR,
				format!("Invalid MCP server name: {name}"),
			));
		}
		if !seen.insert(name.to_string()) {
			return Err(RequestError::new(
				INTERNAL_ERROR,
				format!("Duplicate MCP server name: {name}"),
			));
		}
		let config = crate::config::parse_and_validate_mcp_server(name, server)
			.map_err(|message| RequestError::new(INTERNAL_ERROR, message))?;
		parsed.push(crate::config::DiscoveredMcpServer {
			name: name.to_string(),
			config,
			source_path: std::path::PathBuf::from("<acp>"),
		});
	}
	Ok(parsed)
}

/// Transport-facing dispatcher. Holds the agent weakly so the transport's
/// `Arc<dyn Dispatcher>` does not keep the agent (and its transport) alive in a
/// reference cycle.
struct DispatcherHandle {
	agent: std::sync::Weak<AcpAgent>,
}

impl Dispatcher for DispatcherHandle {
	fn dispatch(
		&self,
		method: &str,
		params: Option<&Value>,
		notification: bool,
	) -> Result<Value, RequestError> {
		match self.agent.upgrade() {
			Some(agent) => Dispatcher::dispatch(&*agent, method, params, notification),
			None => Err(RequestError::internal_error().with_detail("ACP agent dropped")),
		}
	}

	fn after_response(&self, method: &str, params: Option<&Value>) {
		if let Some(agent) = self.agent.upgrade() {
			Dispatcher::after_response(&*agent, method, params);
		}
	}

	fn notify_control(&self, method: &str, params: Option<&Value>, notification: bool) -> bool {
		match self.agent.upgrade() {
			Some(agent) => Dispatcher::notify_control(&*agent, method, params, notification),
			None => false,
		}
	}

	fn on_closed(&self, reason: &str) {
		if let Some(agent) = self.agent.upgrade() {
			Dispatcher::on_closed(&*agent, reason);
		}
	}
}

impl Dispatcher for AcpAgent {
	fn after_response(&self, method: &str, params: Option<&Value>) {
		if method != "session/prompt" {
			return;
		}
		if let Ok(session) = self.session(params) {
			session.start_pending_continuation();
		}
	}

	fn dispatch(
		&self,
		method: &str,
		params: Option<&Value>,
		notification: bool,
	) -> Result<Value, RequestError> {
		if self.runtime.get().is_none() {
			return Err(RequestError::internal_error().with_detail("ACP runtime not started"));
		}
		self.dispatch_request(method, params, notification)
	}

	/// Reader-thread hook. Cancellation is signalled immediately and the frame
	/// still reaches normal dispatch, so a wedged or saturated handler queue
	/// cannot hide a `session/cancel`.
	fn notify_control(&self, method: &str, params: Option<&Value>, notification: bool) -> bool {
		if method == "session/cancel" {
			if let Some(session_id) = params
				.and_then(|params| params.get("sessionId"))
				.and_then(Value::as_str)
				&& let Some(session) = self.manager.get(session_id)
			{
				session.signal_cancel();
			}
			// Notification form still reaches normal dispatch for any
			// non-reader cleanup; the request form is already answered `{}` by
			// the transport and must not occupy a handler slot.
			return !notification;
		}
		false
	}

	/// Exactly once after pending replies were rejected. Signal-only: no
	/// reverse calls, no writes, no writer join.
	fn on_closed(&self, reason: &str) {
		self.manager.shutdown_all(reason);
	}
}

/// Run the ACP server over stdio until EOF, then flush within `flush_budget`.
/// Returns the close reason the transport recorded.
pub fn run<R: BufRead, W: Write + Send + 'static>(
	recipe: AcpRecipe,
	reader: R,
	writer: W,
	flush_budget: Duration,
) -> io::Result<Option<String>> {
	let agent = AcpAgent::new();
	let dispatcher = Arc::new(DispatcherHandle { agent: Arc::downgrade(&agent) });
	let transport = Transport::new(writer, dispatcher)?;
	let _ = agent.transport.set(transport.clone());
	let _ = agent
		.runtime
		.set(AcpRuntime::new(recipe, transport.clone()));
	transport.serve(reader);
	let outcome = transport.finish(flush_budget);
	agent
		.manager
		.shutdown_all(outcome.reason.as_deref().unwrap_or("ACP connection closed"));
	Ok(outcome.reason)
}
