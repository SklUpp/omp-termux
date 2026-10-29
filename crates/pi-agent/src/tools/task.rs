use std::{
	collections::{BTreeSet, HashMap},
	sync::Arc,
	time::Instant,
};

use async_trait::async_trait;
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::{
	Tool, ToolError,
	registry::ToolRegistry,
	task_catalog::{
		AgentCatalog, AgentDefinition, AgentSource, ChildModel, ChildModelFactory, SchemaMode,
		Spawns, array_valued_labels, normalize_schema, thinking_to_effort, validate_output,
	},
};
use crate::{
	loop_module::{AgentError, AgentLoop, SteeringEvent},
	state::{AgentConfig, AgentContext, AgentState, Message, ToolResult, TurnResult},
	supervisor::{ChildGuard, Supervisor, cap_result_text},
};

#[derive(Clone)]
pub struct TaskTool {
	pub supervisor: Arc<Supervisor>,
	pub depth: i32,
	/// Agent id that owns the children this tool spawns (`None` at the root).
	pub parent_id: Option<String>,
	pub semaphore: Arc<Semaphore>,
	/// Explicit catalog for tests/embedders; otherwise the catalog installed by
	/// the CLI runtime is used.
	pub catalog: Option<Arc<AgentCatalog>>,
	/// Name of the agent that owns this tool instance, for self-spawn rejection.
	pub agent_name: Option<String>,
	/// That agent's spawn allowlist (`None` = session default `*`).
	pub spawns: Option<Spawns>,
	/// Effective tool allowlist inherited by descendants (privilege ceiling).
	pub parent_tools: Option<BTreeSet<String>>,
	/// Factory snapshot captured when this tool was created as a nested child;
	/// `None` for a root tool, which reads the current per-turn snapshot.
	pub model_factory: Option<Arc<dyn ChildModelFactory>>,
}

/// Immutable per-spawn snapshot shared by the task tool and detached background
/// work: catalog definitions/policy plus the exact model resolver in force when
/// the spawn started.
#[derive(Clone)]
struct ChildContext {
	supervisor: Arc<Supervisor>,
	depth: i32,
	parent_tools: Option<BTreeSet<String>>,
	catalog: Option<Arc<AgentCatalog>>,
	model_factory: Option<Arc<dyn ChildModelFactory>>,
}

impl ChildContext {
	fn resolve_model(
		&self,
		agent: &AgentDefinition,
		effort: Option<&str>,
	) -> Result<Option<ChildModel>, String> {
		match &self.catalog {
			Some(catalog) => catalog.resolve_provider(
				agent,
				effort,
				self.model_factory.as_deref(),
			),
			None if agent.model.is_empty() => Ok(None),
			None => Err(format!(
				"Agent '{}' selects model {:?} but no agent catalog is installed",
				agent.name, agent.model
			)),
		}
	}

	/// Build the child registry: intersect the agent's tool allowlist with the
	/// parent's effective allowlist, fail closed on tools this build cannot
	/// provide, and give a nested `task` tool its agent context plus this
	/// factory snapshot.
	fn prepare_registry(
		&self,
		child_id: &str,
		agent: &AgentDefinition,
		active_model: Option<&str>,
	) -> Result<(ToolRegistry, Option<BTreeSet<String>>), String> {
		let mut registry = self
			.supervisor
			.child_registry_unbound(self.depth + 1, Some(child_id));

		if let Some(tools) = &agent.tools {
			for name in tools {
				if registry.get(name).is_some() {
					continue;
				}
				// The supervisor omits `task` from the child registry at max
				// recursion depth; that is a depth limit, not a missing tool.
				if name == "task" {
					continue;
				}
				return Err(format!(
					"Agent '{}' requests tool '{name}', which this build does not implement; \
					 spawning it would silently drop that capability. Remove the tool or run \
					 without this agent.",
					agent.name
				));
			}
			if registry.get("yield").is_none() {
				return Err("framework 'yield' tool missing from child registry".to_string());
			}
		}

		let effective: Option<BTreeSet<String>> = match &agent.tools {
			Some(tools) => {
				let mut allowed: BTreeSet<String> = tools.iter().cloned().collect();
				// `yield` is the framework's mandatory return channel; prompts
				// cannot use it to grant any other tool.
				allowed.insert("yield".to_string());
				Some(match &self.parent_tools {
					Some(parent) => allowed.intersection(parent).cloned().collect(),
					None => allowed,
				})
			},
			// No agent allowlist: inherit the parent ceiling verbatim, including
			// `None` (unrestricted). Never materialize a concrete set here.
			None => self.parent_tools.clone(),
		};

		if let Some(allowed) = &effective {
			// Filter by registry keys (internal tool names), never by advertised
			// wire names: `edit` may advertise `apply_patch` while its allowlist
			// entry stays `edit`.
			for name in registry.names() {
				if !allowed.contains(&name) {
					registry.remove(&name);
				}
			}
		}

		if registry.get("task").is_some() {
			registry.remove("task");
			let mut nested = TaskTool::with_parent(
				Arc::clone(&self.supervisor),
				self.depth + 1,
				Some(child_id.to_string()),
			);
			nested.catalog = self.catalog.clone();
			nested.agent_name = Some(agent.name.clone());
			nested.spawns = agent.spawns.clone();
			nested.parent_tools = effective.clone();
			nested.model_factory = self.model_factory.clone();
			registry.register(Arc::new(nested));
		}

		self.supervisor
			.rebind_session_tools(&mut registry, active_model);

		Ok((registry, effective))
	}
}

impl TaskTool {
	pub fn new(supervisor: Arc<Supervisor>, depth: i32) -> Self {
		Self::with_parent(supervisor, depth, None)
	}

	pub fn with_parent(
		supervisor: Arc<Supervisor>,
		depth: i32,
		parent_id: Option<String>,
	) -> Self {
		let limits = supervisor.limits();
		let permits = if limits.max_concurrency == 0 {
			Semaphore::MAX_PERMITS
		} else {
			limits.max_concurrency
		};
		Self {
			supervisor,
			depth,
			parent_id,
			semaphore: Arc::new(Semaphore::new(permits)),
			catalog: None,
			agent_name: None,
			spawns: None,
			parent_tools: None,
			model_factory: None,
		}
	}

	pub fn with_semaphore(supervisor: Arc<Supervisor>, depth: i32, semaphore: Arc<Semaphore>) -> Self {
		Self {
			supervisor,
			depth,
			parent_id: None,
			semaphore,
			catalog: None,
			agent_name: None,
			spawns: None,
			parent_tools: None,
			model_factory: None,
		}
	}

	#[must_use]
	pub fn with_catalog(mut self, catalog: Arc<AgentCatalog>) -> Self {
		self.catalog = Some(catalog);
		self
	}

	#[must_use]
	pub fn with_agent_context(
		mut self,
		agent_name: String,
		spawns: Option<Spawns>,
		tools: Option<BTreeSet<String>>,
	) -> Self {
		self.agent_name = Some(agent_name);
		self.spawns = spawns;
		self.parent_tools = tools;
		self
	}

	#[must_use]
	pub fn with_model_factory(mut self, factory: Arc<dyn ChildModelFactory>) -> Self {
		self.model_factory = Some(factory);
		self
	}

	/// Snapshot the catalog and resolver for one invocation. A root tool reads
	/// the latest per-turn factory; a nested tool keeps the factory captured
	/// when it was created, so a later `/model` switch cannot change a running
	/// child's grandchildren.
	fn child_context(&self) -> ChildContext {
		let catalog = self.catalog();
		let model_factory = self
			.model_factory
			.clone()
			.or_else(|| catalog.as_ref().and_then(|catalog| catalog.model_factory()));
		ChildContext {
			supervisor: Arc::clone(&self.supervisor),
			depth: self.depth,
			parent_tools: self.parent_tools.clone(),
			catalog,
			model_factory,
		}
	}

	fn catalog(&self) -> Option<Arc<AgentCatalog>> {
		self
			.catalog
			.clone()
			.or_else(|| self.supervisor.agent_catalog())
	}

	/// Resolve the requested agent against the spawn policy and reject agent
	/// capabilities this runtime cannot honor.
	fn resolve_agent(&self, requested: Option<&str>) -> Result<AgentDefinition, ToolError> {
		let requested = requested.map(str::trim).filter(|name| !name.is_empty());
		let default_agent = self.spawns.as_ref().map_or("task", Spawns::default_agent);
		let name = requested.unwrap_or(default_agent);
		let Some(catalog) = self.catalog() else {
			// No catalog installed: preserve the original single-agent
			// behaviour, but still honor self-spawn/spawns restrictions.
			if self.agent_name.as_deref() == Some(name) {
				return Err(ToolError::InvalidArguments(format!(
					"Cannot spawn {name} agent from within itself (recursion prevention). Use a different agent type."
				)));
			}
			if let Some(spawns) = &self.spawns
				&& !spawns.allows(name)
			{
				return Err(ToolError::InvalidArguments(format!(
					"Cannot spawn '{name}'. Allowed: {}",
					spawns.error_text()
				)));
			}
			if name != "task" {
				return Err(ToolError::InvalidArguments(format!(
					"Unsupported agent '{name}': only 'task' is supported in this build"
				)));
			}
			return Ok(synthetic_task_agent());
		};
		let agent = catalog
			.validate_selection(name, self.agent_name.as_deref(), self.spawns.as_ref())
			.map_err(ToolError::InvalidArguments)?;
		check_agent_capabilities(agent)?;
		Ok(agent.clone())
	}

	fn build_item(
		&self,
		obj: &serde_json::Map<String, Value>,
		context: Option<String>,
		defaults: &ItemDefaults,
	) -> Result<TaskItemSpec, ToolError> {
		check_unsupported_fields(obj)?;
		let requested_agent = match obj.get("agent") {
			None | Some(Value::Null) => None,
			Some(Value::String(name)) => Some(name.as_str()),
			Some(_) => {
				return Err(ToolError::InvalidArguments(
					"'agent' must be a string".to_string(),
				));
			},
		};
		let mut agent = self.resolve_agent(requested_agent)?;
		let task = obj
			.get("task")
			.and_then(Value::as_str)
			.ok_or_else(|| ToolError::InvalidArguments("Missing 'task' in tasks item".to_string()))?
			.to_string();
		let name = if let Some(n) = obj.get("name").and_then(Value::as_str) {
			validate_name(n)?;
			n.to_string()
		} else {
			format!("task-{}", self.supervisor.next_seq())
		};
		let output_schema = if obj.contains_key("outputSchema") {
			parse_schema(obj.get("outputSchema"))?
		} else {
			defaults.output_schema.clone()
		};
		let schema_mode = if obj.contains_key("schemaMode") {
			parse_schema_mode(obj.get("schemaMode"))?.unwrap_or_default()
		} else {
			defaults.schema_mode
		};
		let effort = if obj.contains_key("effort") {
			parse_effort(obj.get("effort"))?
		} else {
			defaults.effort.clone()
		};
		// Agent output schemas accept the same JSON-string/`true` forms as the
		// caller's `outputSchema`; normalize before validating or using it.
		let mut schema_error = None;
		if let Some(raw) = &agent.output {
			match normalize_schema(raw) {
				Ok(schema) => agent.output = Some(schema),
				Err(error) => {
					if schema_mode == SchemaMode::Strict {
						return Err(ToolError::InvalidArguments(format!(
							"Agent '{}' output schema is invalid: {error}",
							agent.name
						)));
					}
					agent.output = None;
					schema_error = Some(format!(
						"agent '{}' output schema ignored: {error}",
						agent.name
					));
				},
			}
		}
		Ok(TaskItemSpec {
			name,
			agent,
			task,
			context,
			output_schema,
			schema_mode,
			schema_error,
			effort,
		})
	}

	async fn run_single_task(
		&self,
		index: usize,
		item: TaskItemSpec,
		ctx: &AgentContext,
		child_ctx: &ChildContext,
	) -> SingleResult {
		let start = Instant::now();
		let child_name = item.name.clone();
		let agent_name = item.agent.name.clone();
		let agent_source = item.agent.source.as_str().to_string();
		let Some((child_id, child_cancel)) = self.supervisor.reserve_child(
			&child_name,
			index,
			&agent_name,
			&item.task,
			self.parent_id.as_deref(),
		) else {
			return SingleResult {
				index,
				id: child_name.clone(),
				name: child_name,
				agent: agent_name.clone(),
				agent_source: agent_source.clone(),
				task: item.task,
				exit_code: 1,
				output: "(no output)".to_string(),
				stderr: String::new(),
				truncated: false,
				duration_ms: 0,
				tokens: 0,
				requests: 0,
				error: Some(format!(
					"result capacity exhausted ({} outstanding); wait for or consume finished jobs first",
					crate::supervisor::MAX_OUTSTANDING_RESULTS
				)),
				aborted: None,
				abort_reason: None,
				extracted_tool_data: None,
				structured_output: None,
				resolved_model: None,
				model_role: None,
			};
		};
		let mut reservation = ReservationGuard::new(
			self.supervisor.clone(),
			child_id.clone(),
			child_cancel.clone(),
		);

		let _permit = match tokio::select! {
			biased;
			_ = child_cancel.cancelled() => Err("cancelled by hub"),
			_ = ctx.cancellation_token.cancelled() => Err("aborted by parent"),
			permit = self.semaphore.acquire() => permit.map_err(|_| "aborted by parent"),
		} {
			Ok(permit) => permit,
			Err(reason) => {
				self.supervisor.mark_aborted(&child_id, reason);
				self.supervisor.unregister_active_child(&child_id);
				return SingleResult {
					index,
					id: child_id,
					name: child_name,
					agent: agent_name.clone(),
					agent_source: agent_source.clone(),
					task: item.task,
					exit_code: 1,
					output: "(no output)".to_string(),
					stderr: String::new(),
					truncated: false,
					duration_ms: start.elapsed().as_millis() as u64,
					tokens: 0,
					requests: 0,
					error: None,
					aborted: Some(true),
					abort_reason: Some(reason.to_string()),
					extracted_tool_data: None,
				structured_output: None,
				resolved_model: None,
				model_role: None,
				};
			}
		};

		// A permit can be granted at the same moment the reservation is cancelled.
		if child_cancel.is_cancelled() {
			self.supervisor.mark_aborted(&child_id, "cancelled by hub");
			self.supervisor.unregister_active_child(&child_id);
			return SingleResult {
				index,
				id: child_id,
				name: child_name,
				agent: agent_name.clone(),
				agent_source: agent_source.clone(),
				task: item.task,
				exit_code: 1,
				output: "(no output)".to_string(),
				stderr: String::new(),
				truncated: false,
				duration_ms: start.elapsed().as_millis() as u64,
				tokens: 0,
				requests: 0,
				error: None,
				aborted: Some(true),
				abort_reason: Some("cancelled by hub".to_string()),
				extracted_tool_data: None,
				structured_output: None,
				resolved_model: None,
				model_role: None,
			};
		}

		let child_model = match child_ctx.resolve_model(&item.agent, item.effort.as_deref()) {
			Ok(model) => model,
			Err(message) => {
				reservation.disarm();
				self.supervisor.mark_failed(
					&child_id,
					start.elapsed().as_millis() as u64,
					&message,
				);
				self.supervisor.unregister_active_child(&child_id);
				self.supervisor.release_outstanding(&child_id);
				return single_failure(
					index,
					child_id,
					child_name,
					agent_name,
					agent_source,
					item.task,
					message,
					start.elapsed().as_millis() as u64,
					None,
					None,
				);
			},
		};
		let resolved_model = child_model.as_ref().map(|model| model.model.clone());
		let model_role = child_model.as_ref().and_then(|model| model.role.clone());
		let child_provider = child_model.map_or_else(
			|| self.supervisor.provider(),
			|model| model.provider,
		);
		let (child_registry, _effective_tools) =
			match child_ctx.prepare_registry(&child_id, &item.agent, resolved_model.as_deref()) {
				Ok(prepared) => prepared,
				Err(message) => {
					reservation.disarm();
					self.supervisor.mark_failed(
						&child_id,
						start.elapsed().as_millis() as u64,
						&message,
					);
					self.supervisor.unregister_active_child(&child_id);
					self.supervisor.release_outstanding(&child_id);
					return single_failure(
						index,
						child_id,
						child_name,
						agent_name,
						agent_source,
						item.task,
						message,
						start.elapsed().as_millis() as u64,
						None,
						None,
					);
				},
			};
		let prompt_schema = item.output_schema.clone().or_else(|| {
			if item.schema_error.is_none() {
				item.agent.output.clone()
			} else {
				None
			}
		});
		let child_config = AgentConfig {
			max_turns: 20,
			context_limit: 100_000,
			system_prompt: child_system_prompt(&item.agent, prompt_schema.as_ref()),
			cwd: ctx.cwd.clone(),
		};

		let child_approver = self.supervisor.child_approver();

		let (steering_tx, steering_rx) = tokio::sync::mpsc::channel(16);
		let assignment = if let Some(context) = &item.context {
			format!("{context}\n\n{}", item.task)
		} else {
			item.task.clone()
		};
		let full_prompt = format!("Complete assignment thoroughly:\n\n{assignment}");

		let child_cancel_for_loop = child_cancel.clone();
		let mut task_handle = tokio::spawn(async move {
			let child_loop = match child_approver {
				Some(approver) => AgentLoop::with_approver(
					child_config,
					child_provider,
					child_registry,
					approver,
				),
				None => AgentLoop::new(child_config, child_provider, child_registry),
			};
			let mut child_state = AgentState::default();
			child_loop
				.run_dialog_message_with_cancel(
					Message::user(full_prompt),
					&mut child_state,
					Some(steering_rx),
					None,
					child_cancel_for_loop,
				)
				.await
		});

		if !self.supervisor.register_active_child(
			&child_id,
			steering_tx.clone(),
			task_handle.abort_handle(),
			child_cancel.clone(),
		) {
			// The reservation was cancelled between the permit check and this
			// registration; never let the already-spawned loop run unowned.
			child_cancel.cancel();
			task_handle.abort();
			let _ = task_handle.await;
			self.supervisor.mark_aborted(&child_id, "cancelled by hub");
			self.supervisor.unregister_active_child(&child_id);
			return SingleResult {
				index,
				id: child_id,
				name: child_name,
				agent: agent_name.clone(),
				agent_source: agent_source.clone(),
				task: item.task,
				exit_code: 1,
				output: "(no output)".to_string(),
				stderr: String::new(),
				truncated: false,
				duration_ms: start.elapsed().as_millis() as u64,
				tokens: 0,
				requests: 0,
				error: None,
				aborted: Some(true),
				abort_reason: Some("cancelled by hub".to_string()),
				extracted_tool_data: None,
				structured_output: None,
				resolved_model: None,
				model_role: None,
			};
		}

		let mut guard = ChildGuard::new(
			task_handle.abort_handle(),
			steering_tx.clone(),
			self.supervisor.clone(),
			child_id.clone(),
			child_cancel.clone(),
		);
		// The running task is now owned by `ChildGuard`; no await occurred since
		// `register_active_child`, so nothing can drop this future in the gap.
		reservation.disarm();

		let outcome = tokio::select! {
			_ = ctx.cancellation_token.cancelled() => {
				child_cancel.cancel();
				let _ = steering_tx.try_send(SteeringEvent::Interrupt);
				None
			}
			res = &mut task_handle => Some(join_dialog_result(res)),
		};
		// Await outside the select so only one branch borrows the handle.
		let outcome = match outcome {
			Some(outcome) => outcome,
			None => match tokio::time::timeout(CHILD_CANCEL_GRACE, &mut task_handle).await {
				Ok(joined) => join_dialog_result(joined),
				Err(_) => {
					guard.handle.abort();
					let _ = task_handle.await;
					Err(AgentError::Interrupted)
				},
			},
		};

		guard.defuse();
		self.supervisor.unregister_active_child(&child_id);

		let duration_ms = start.elapsed().as_millis() as u64;
		let outstanding_id = child_id.clone();

		let result = match outcome {
			Ok(turn_results) => {
				let requests = turn_results.len();
				let base_output = turn_results
					.iter()
					.rev()
					.find_map(|turn| {
						let trimmed = turn.message.content.trim();
						if trimmed.is_empty() {
							None
						} else {
							Some(trimmed.to_string())
						}
					})
					.unwrap_or_else(|| "(no output)".to_string());

				let mut extracted_tool_data = None;
				for turn in &turn_results {
					for tr in &turn.tool_results {
						if tr.terminate {
							if let Ok(val) = serde_json::from_str::<Value>(&tr.content) {
								let mut map = HashMap::new();
								map.insert("yield".to_string(), vec![val]);
								extracted_tool_data = Some(map);
							}
							break;
						}
					}
					if extracted_tool_data.is_some() {
						break;
					}
				}

				let mut is_aborted = false;
				let mut abort_reason = None;
				if let Some(map) = &extracted_tool_data
					&& let Some(yields) = map.get("yield")
				{
					for y in yields {
						if y.get("status").and_then(Value::as_str) == Some("aborted") {
							is_aborted = true;
							if let Some(err) = y.get("error").and_then(Value::as_str) {
								abort_reason = Some(err.to_string());
							}
						}
					}
				}

				let finalized = finalize_output(&item, &turn_results, &base_output, is_aborted);
				let (output, output_truncated) = cap_result_text(&finalized.output);
				let exit_code = if is_aborted { 1 } else { finalized.exit_code };

				if is_aborted {
					self.supervisor.mark_aborted(
						&child_id,
						abort_reason.as_deref().unwrap_or("aborted by yield"),
					);
				} else if finalized.failed {
					self
						.supervisor
						.mark_failed(&child_id, duration_ms, finalized.stderr.trim());
				} else {
					// A child that yields carries its real result in the yielded
					// data, not in assistant text. Surface that in hub snapshots.
					let record_output = finalized
						.structured
						.as_ref()
						.and_then(|structured| structured.data.as_ref())
						.map(Value::to_string)
						.or_else(|| {
							extracted_tool_data
								.as_ref()
								.and_then(|map| map.get("yield"))
								.and_then(|yields| yields.first())
								.and_then(|yielded| yielded.get("data"))
								.map(Value::to_string)
						})
						.unwrap_or_else(|| output.clone());
					self
						.supervisor
						.mark_completed(&child_id, duration_ms, &record_output);
				}

				SingleResult {
					index,
					id: child_id,
					name: child_name,
					agent: agent_name.clone(),
					agent_source: agent_source.clone(),
					task: item.task,
					exit_code,
					output,
					stderr: finalized.stderr,
					truncated: output_truncated,
					duration_ms,
					tokens: 0,
					requests,
					error: None,
					aborted: if is_aborted { Some(true) } else { None },
					abort_reason,
					extracted_tool_data,
					structured_output: finalized.structured,
					resolved_model,
					model_role,
				}
			},
			Err(AgentError::Interrupted) => {
				self.supervisor.mark_aborted(&child_id, "aborted by parent");
				SingleResult {
					index,
					id: child_id,
					name: child_name,
					agent: agent_name.clone(),
					agent_source: agent_source.clone(),
					task: item.task,
					exit_code: 1,
					output: "(no output)".to_string(),
					stderr: String::new(),
					truncated: false,
					duration_ms,
					tokens: 0,
					requests: 0,
					error: None,
					aborted: Some(true),
					abort_reason: Some("aborted by parent".to_string()),
					extracted_tool_data: None,
				structured_output: None,
				resolved_model: None,
				model_role: None,
				}
			},
			Err(err) => {
				let err_msg = err.to_string();
				self.supervisor.mark_failed(&child_id, duration_ms, &err_msg);
				SingleResult {
					index,
					id: child_id,
					name: child_name,
					agent: agent_name.clone(),
					agent_source: agent_source.clone(),
					task: item.task,
					exit_code: 1,
					output: "(no output)".to_string(),
					stderr: err_msg.clone(),
					truncated: false,
					duration_ms,
					tokens: 0,
					requests: 0,
					error: Some(err_msg),
					aborted: None,
					abort_reason: None,
					extracted_tool_data: None,
				structured_output: None,
				resolved_model: None,
				model_role: None,
				}
			},
		};
		self.supervisor.release_outstanding(&outstanding_id);
		result
	}

	/// Reserve and detach one background child. The returned id is the real
	/// supervisor identity and doubles as the job id, so `hub jobs/cancel/wait`
	/// address it immediately, before the child acquires a permit or runs.
	fn spawn_background_task(
		&self,
		index: usize,
		item: TaskItemSpec,
		ctx: &AgentContext,
		child_ctx: ChildContext,
	) -> Result<String, String> {
		let child_name = item.name.clone();
		let agent_name = item.agent.name.clone();
		let item_for_settle = item.clone();
		let Some((child_id, child_cancel)) = self.supervisor.reserve_child(
			&child_name,
			index,
			&agent_name,
			&item.task,
			self.parent_id.as_deref(),
		) else {
			return Err(format!(
				"{child_name}: result capacity exhausted ({} outstanding); wait for or consume finished jobs first",
				crate::supervisor::MAX_OUTSTANDING_RESULTS
			));
		};
		let (steering_tx, steering_rx) = tokio::sync::mpsc::channel(16);
		let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();
		let supervisor = self.supervisor.clone();
		// Per-`TaskTool` (per-session) permits: nested spawns use their own
		// semaphore, so a parent holding a permit while waiting for descendants
		// cannot deadlock when every parent permit is occupied.
		let semaphore = self.semaphore.clone();
		let cwd = ctx.cwd.clone();
		let task_id = child_id.clone();
		let cancel = child_cancel.clone();
		let mut lifecycle = SettlementGuard::new(supervisor.clone(), task_id.clone());

		let handle = tokio::spawn(async move {
			// Wait until the reservation is registered as active, so an early
			// cancel can abort this task before it ever reaches the model.
			if start_rx.await.is_err() {
				supervisor.mark_aborted(&task_id, "cancelled before start");
				supervisor.unregister_active_child(&task_id);
				return;
			}
			let start = Instant::now();
			let permit = tokio::select! {
				biased;
				_ = cancel.cancelled() => None,
				permit = semaphore.clone().acquire_owned() => permit.ok(),
			};
			let Some(_permit) = permit else {
				supervisor.mark_aborted(&task_id, "cancelled by hub");
				supervisor.unregister_active_child(&task_id);
				return;
			};
			if cancel.is_cancelled() {
				supervisor.mark_aborted(&task_id, "cancelled by hub");
				supervisor.unregister_active_child(&task_id);
				return;
			}
			let outcome = run_background_child(
				supervisor.clone(),
				task_id.clone(),
				item,
				cwd,
				steering_rx,
				cancel.clone(),
				child_ctx,
			)
			.await;
			supervisor.unregister_active_child(&task_id);
			settle_background_result(supervisor, task_id, start, outcome, item_for_settle);
			lifecycle.disarm();
		});

		if !self.supervisor.register_active_child(
			&child_id,
			steering_tx,
			handle.abort_handle(),
			child_cancel.clone(),
		) {
			child_cancel.cancel();
			handle.abort();
			self.supervisor.mark_aborted(&child_id, "cancelled by hub");
			self.supervisor.unregister_active_child(&child_id);
			return Err(format!("{child_name}: cancelled before start"));
		}
		let _ = start_tx.send(());
		Ok(child_id)
	}
}

/// Build a child loop with fresh read/edit state, its own result sink, and its
/// own peer inbox. Runs detached from the parent turn so REPL turns do not kill
/// a background job.
async fn run_background_child(
	supervisor: Arc<Supervisor>,
	child_id: String,
	item: TaskItemSpec,
	cwd: std::path::PathBuf,
	steering_rx: tokio::sync::mpsc::Receiver<SteeringEvent>,
	cancel: CancellationToken,
	child_ctx: ChildContext,
) -> Result<Vec<TurnResult>, AgentError> {
	let child_model = child_ctx
		.resolve_model(&item.agent, item.effort.as_deref())
		.map_err(AgentError::Internal)?;
	let child_active_model = child_model.as_ref().map(|model| model.model.clone());
	let child_provider = child_model.map_or_else(
		|| supervisor.provider(),
		|model| model.provider,
	);
	let (child_registry, _effective_tools) = child_ctx
		.prepare_registry(&child_id, &item.agent, child_active_model.as_deref())
		.map_err(AgentError::Internal)?;
	let prompt_schema = item.output_schema.clone().or_else(|| {
		if item.schema_error.is_none() {
			item.agent.output.clone()
		} else {
			None
		}
	});
	let child_config = AgentConfig {
		max_turns: 20,
		context_limit: 100_000,
		system_prompt: child_system_prompt(&item.agent, prompt_schema.as_ref()),
		cwd,
	};
	let child_approver = supervisor.child_approver();
	let assignment = if let Some(context) = &item.context {
		format!("{context}\n\n{}", item.task)
	} else {
		item.task.clone()
	};
	let full_prompt = format!("Complete assignment thoroughly:\n\n{assignment}");
	let child_loop = match child_approver {
		Some(approver) => {
			AgentLoop::with_approver(child_config, child_provider, child_registry, approver)
		},
		None => AgentLoop::new(child_config, child_provider, child_registry),
	};
	let child_loop = child_loop
		.with_background_results(supervisor.owner_results_sink(Some(&child_id)))
		.with_peer_inbox(supervisor.mailbox(), child_id.clone());
	let mut child_state = AgentState::default();
	child_loop
		.run_dialog_message_with_cancel(
			Message::user(full_prompt),
			&mut child_state,
			Some(steering_rx),
			None,
			cancel,
		)
		.await
}

/// Summarize a finished child: final assistant text, abort flag/reason, and a
/// terminal `yield` payload when one exists.
fn summarize_turns(turns: &[TurnResult]) -> (String, bool, Option<String>) {
	let output = turns
		.iter()
		.rev()
		.find_map(|turn| {
			let trimmed = turn.message.content.trim();
			if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
		})
		.unwrap_or_else(|| "(no output)".to_string());
	let mut aborted = false;
	let mut reason = None;
	for turn in turns {
		for result in &turn.tool_results {
			if result.terminate {
				if let Ok(value) = serde_json::from_str::<Value>(&result.content)
					&& value.get("status").and_then(Value::as_str) == Some("aborted")
				{
					aborted = true;
					reason = value
						.get("error")
						.and_then(Value::as_str)
						.map(str::to_string);
				}
			}
		}
	}
	(output, aborted, reason)
}

fn settle_background_result(
	supervisor: Arc<Supervisor>,
	id: String,
	start: Instant,
	outcome: Result<Vec<TurnResult>, AgentError>,
	item: TaskItemSpec,
) {
	let duration_ms = start.elapsed().as_millis() as u64;
	let agent = item.agent.name.clone();
	match outcome {
		Ok(turns) => {
			let (base, aborted, reason) = summarize_turns(&turns);
			if aborted {
				supervisor.mark_aborted(
					&id,
					reason.as_deref().unwrap_or("aborted by yield"),
				);
				return;
			}
			let finalized = finalize_output(&item, &turns, &base, false);
			let (output, _) = cap_result_text(&finalized.output);
			if finalized.failed {
				let envelope = task_result_envelope(
					&id,
					&agent,
					"failed (schema_violation)",
					duration_ms,
					&output,
				);
				supervisor.mark_failed(&id, duration_ms, &envelope);
				supervisor.enqueue_delivery(&id, envelope);
				return;
			}
			let output = if finalized.stderr.is_empty() {
				output
			} else {
				format!("{}{output}", finalized.stderr)
			};
			let envelope = task_result_envelope(&id, &agent, "completed", duration_ms, &output);
			supervisor.mark_completed(&id, duration_ms, &envelope);
			supervisor.enqueue_delivery(&id, envelope);
		},
		Err(AgentError::Interrupted) => {
			supervisor.mark_aborted(&id, "aborted");
		},
		Err(error) => {
			let message = error.to_string();
			let envelope =
				task_result_envelope(&id, &agent, "failed (exit 1)", duration_ms, &message);
			supervisor.mark_failed(&id, duration_ms, &envelope);
			supervisor.enqueue_delivery(&id, envelope);
		},
	}
}

/// `<task-result>` envelope from `prompts/tools/task-summary.md`. No
/// `agent://`/`history://` pointer is emitted: this build has no transcript
/// resolver, so a link would be fake.
fn task_result_envelope(
	id: &str,
	agent: &str,
	status: &str,
	duration_ms: u64,
	output: &str,
) -> String {
	format!(
		"<task-result id=\"{id}\" agent=\"{agent}\" status=\"{status}\" duration=\"{}\">\n<output>\n{output}\n</output>\n</task-result>",
		format_duration_ms(duration_ms)
	)
}

fn format_duration_ms(milliseconds: u64) -> String {
	if milliseconds < 1_000 {
		return format!("{milliseconds}ms");
	}
	let seconds = milliseconds as f64 / 1_000.0;
	if seconds < 60.0 {
		let rounded = (seconds * 10.0).round() / 10.0;
		if rounded.fract() == 0.0 {
			format!("{}s", rounded as u64)
		} else {
			format!("{rounded:.1}s")
		}
	} else {
		let minutes = (seconds / 60.0).floor() as u64;
		let rest = (seconds - minutes as f64 * 60.0).round() as u64;
		format!("{minutes}m {rest}s")
	}
}

fn coordination_hint(ids: &[String]) -> String {
	let dm = if ids.len() == 1 {
		format!(
			"DM `{}` via `hub` send to coordinate while it runs; use `hub` only to inspect (`jobs`), wait, or cancel a stuck task.",
			ids[0]
		)
	} else {
		"DM these ids via `hub` send to coordinate while they run; use `hub` only to inspect (`jobs`), wait, or cancel a stuck task by id.".to_string()
	};
	format!("{dm}\n{}", TASK_ASYNC_CONTRACT)
}

/// Upstream `prompts/tools/task-async-contract.md`, verbatim.
const TASK_ASYNC_CONTRACT: &str = "No polling needed.\n\nSettled-job inspection: `hub jobs` | `hub wait` delivers its snapshot → no duplicate `async-result`.\n\n`completed`: subagent yielded successfully; claimed artifacts unverified.";

/// Owns a queued reservation until the spawned `ChildGuard` takes over. If the
/// batch future is dropped while waiting for a semaphore permit, this drop still
/// settles the record and releases the reservation instead of leaking a
/// `Running` id forever.
struct ReservationGuard {
	supervisor: Arc<Supervisor>,
	id:         String,
	token:      tokio_util::sync::CancellationToken,
	armed:      bool,
}

impl ReservationGuard {
	fn new(
		supervisor: Arc<Supervisor>,
		id: String,
		token: tokio_util::sync::CancellationToken,
	) -> Self {
		Self { supervisor, id, token, armed: true }
	}

	/// Called only after `ChildGuard` owns the running task; no await may sit
	/// between registration and this handoff.
	fn disarm(&mut self) {
		self.armed = false;
	}
}

impl Drop for ReservationGuard {
	fn drop(&mut self) {
		if !self.armed {
			return;
		}
		self.token.cancel();
		self.supervisor.mark_aborted(&self.id, "aborted by parent");
		self.supervisor.unregister_active_child(&self.id);
	}
}

/// Lifecycle guard for a detached background child. A panic inside the child
/// future unwinds past the normal unregister/settle path; this guard then
/// settles the record as aborted and frees the admission slot (unless a body
/// was already queued). Disarmed only after normal settlement.
struct SettlementGuard {
	supervisor: Arc<Supervisor>,
	id:         String,
	armed:      bool,
}

impl SettlementGuard {
	fn new(supervisor: Arc<Supervisor>, id: String) -> Self {
		Self { supervisor, id, armed: true }
	}

	fn disarm(&mut self) {
		self.armed = false;
	}
}

impl Drop for SettlementGuard {
	fn drop(&mut self) {
		if !self.armed {
			return;
		}
		if !self.supervisor.mark_aborted(&self.id, "panicked before settlement") {
			self.supervisor.release_outstanding_if_undelivered(&self.id);
		}
		self.supervisor.unregister_active_child(&self.id);
	}
}

/// How long a cancelled child may take to unwind cooperative tool cleanup
/// (the shell's own graceful-kill window) before it is hard-aborted.
const CHILD_CANCEL_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

fn join_dialog_result(
	joined: Result<Result<Vec<crate::state::TurnResult>, AgentError>, tokio::task::JoinError>,
) -> Result<Vec<crate::state::TurnResult>, AgentError> {
	match joined {
		Ok(dialog_res) => dialog_res,
		Err(join_err) => {
			if join_err.is_cancelled() {
				Err(AgentError::Interrupted)
			} else {
				Err(AgentError::Internal(join_err.to_string()))
			}
		},
	}
}

struct ItemDefaults {
	output_schema: Option<Value>,
	schema_mode: SchemaMode,
	effort: Option<String>,
}

fn parse_schema(value: Option<&Value>) -> Result<Option<Value>, ToolError> {
	match value {
		None | Some(Value::Null) => Ok(None),
		Some(schema) => normalize_schema(schema)
			.map(Some)
			.map_err(|error| ToolError::InvalidArguments(format!("Invalid 'outputSchema': {error}"))),
	}
}

fn parse_schema_mode(value: Option<&Value>) -> Result<Option<SchemaMode>, ToolError> {
	let Some(value) = value else {
		return Ok(None);
	};
	let Some(text) = value.as_str() else {
		return Err(ToolError::InvalidArguments(
			"'schemaMode' must be \"permissive\" or \"strict\"".to_string(),
		));
	};
	SchemaMode::parse(text).map(Some).ok_or_else(|| {
		ToolError::InvalidArguments(format!(
			"Invalid 'schemaMode' '{text}': expected \"permissive\" or \"strict\""
		))
	})
}

fn parse_effort(value: Option<&Value>) -> Result<Option<String>, ToolError> {
	let Some(value) = value else {
		return Ok(None);
	};
	let Some(text) = value.as_str() else {
		return Err(ToolError::InvalidArguments(
			"'effort' must be a string".to_string(),
		));
	};
	thinking_to_effort(text)
		.map(|effort| effort.map(str::to_string))
		.map_err(|error| ToolError::InvalidArguments(format!("Invalid 'effort': {error}")))
}

fn check_unsupported_fields(obj: &serde_json::Map<String, Value>) -> Result<(), ToolError> {
	match obj.get("isolated") {
		None | Some(Value::Bool(false)) => Ok(()),
		Some(Value::Bool(true)) => Err(ToolError::InvalidArguments(
			"'isolated' is not supported in this build: there is no git worktree create/apply/cleanup \
			 implementation; run without 'isolated' and apply child changes manually"
				.to_string(),
		)),
		Some(_) => Err(ToolError::InvalidArguments(
			"'isolated' must be a boolean".to_string(),
		)),
	}
}

fn validate_name(name: &str) -> Result<(), ToolError> {
	if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
		Ok(())
	} else {
		Err(ToolError::InvalidArguments(format!(
			"Invalid name '{name}': must match [A-Za-z0-9_-]+"
		)))
	}
}

fn synthetic_task_agent() -> AgentDefinition {
	AgentDefinition {
		name: "task".to_string(),
		description: "General-purpose subagent with full capabilities".to_string(),
		tools: None,
		spawns: None,
		model: Vec::new(),
		output: None,
		thinking_level: None,
		blocking: false,
		read_summarize: None,
		prewalk: None,
		advisor: None,
		autoload_skills: Vec::new(),
		system_prompt: String::new(),
		source: AgentSource::Bundled,
		file_path: "embedded:task.md".to_string(),
	}
}

fn check_agent_capabilities(agent: &AgentDefinition) -> Result<(), ToolError> {
	if agent.prewalk.is_some() {
		return Err(ToolError::InvalidArguments(format!(
			"Agent '{}' requests prewalk hand-off, which this build does not implement",
			agent.name
		)));
	}
	if agent.advisor.is_some() {
		return Err(ToolError::InvalidArguments(format!(
			"Agent '{}' requests an advisor model, which this build does not implement",
			agent.name
		)));
	}
	if !agent.autoload_skills.is_empty() {
		return Err(ToolError::InvalidArguments(format!(
			"Agent '{}' requests autoloadSkills, which this build does not implement",
			agent.name
		)));
	}
	if agent.read_summarize == Some(true) {
		return Err(ToolError::InvalidArguments(format!(
			"Agent '{}' requests read summarization, which this build does not implement",
			agent.name
		)));
	}
	Ok(())
}

fn child_system_prompt(agent: &AgentDefinition, schema: Option<&Value>) -> Option<String> {
	let body = agent.system_prompt.trim();
	if body.is_empty() {
		return None;
	}
	let mut prompt = body.to_string();
	if let Some(schema) = schema
		&& !matches!(schema, Value::Bool(true))
	{
		let rendered = serde_json::to_string_pretty(schema)
			.unwrap_or_else(|_| schema.to_string());
		prompt.push_str(
			"\n\n§ Structured output\nYour terminal `yield` MUST put the complete result \
			 object in `result.data`, matching this schema:\n```json\n",
		);
		prompt.push_str(&rendered);
		prompt.push_str("\n```");
	}
	Some(prompt)
}

struct AssembledYield {
	data: Option<Value>,
	raw_text: bool,
}

struct FinalizedOutput {
	output: String,
	stderr: String,
	exit_code: i32,
	structured: Option<StructuredOutput>,
	failed: bool,
}

fn collect_yields(turns: &[TurnResult]) -> Vec<Value> {
	let mut names: HashMap<&str, &str> = HashMap::new();
	for turn in turns {
		for call in &turn.message.tool_calls {
			names.insert(call.id.as_str(), call.name.as_str());
		}
	}
	let mut yields = Vec::new();
	for turn in turns {
		for result in &turn.tool_results {
			if names.get(result.tool_call_id.as_str()) == Some(&"yield")
				&& let Ok(value) = serde_json::from_str::<Value>(&result.content)
			{
				yields.push(value);
			}
		}
	}
	yields
}

/// Assemble incremental `type:[label]` yields into one object and return the
/// terminal payload (or the final assistant text when the terminal yield has no
/// data). Mirrors the TypeScript assembly for the fields this build supports.
fn assemble_yields(
	yields: &[Value],
	last_text: &str,
	array_labels: &BTreeSet<String>,
) -> AssembledYield {
	let terminal = yields.iter().rev().find(|item| {
		!item
			.get("type")
			.and_then(Value::as_array)
			.is_some_and(|labels| !labels.is_empty())
	});
	let mut sections = serde_json::Map::new();
	let mut counts: HashMap<String, usize> = HashMap::new();
	for item in yields {
		if item.get("status").and_then(Value::as_str) == Some("aborted") {
			continue;
		}
		let Some(labels) = item.get("type").and_then(Value::as_array) else {
			continue;
		};
		if labels.is_empty() {
			continue;
		}
		for label in labels.iter().filter_map(Value::as_str).map(str::trim) {
			if label.is_empty() {
				continue;
			}
			let data = item
				.get("data")
				.cloned()
				.unwrap_or_else(|| Value::String(last_text.to_string()));
			let force_array = array_labels.contains(label);
			let count = counts.entry(label.to_string()).or_insert(0);
			match (*count, force_array) {
				(0, true) => {
					sections.insert(label.to_string(), Value::Array(vec![data]));
				},
				(0, false) => {
					sections.insert(label.to_string(), data);
				},
				(_, _) => match sections.get_mut(label) {
					Some(Value::Array(items)) => items.push(data),
					Some(existing) => {
						let previous = existing.take();
						*existing = Value::Array(vec![previous, data]);
					},
					None => {},
				},
			}
			*count += 1;
		}
	}
	if let Some(terminal) = terminal
		&& let Some(data) = terminal.get("data")
	{
		return AssembledYield {
			data: Some(data.clone()),
			raw_text: false,
		};
	}
	if !sections.is_empty() {
		return AssembledYield {
			data: Some(Value::Object(sections)),
			raw_text: false,
		};
	}
	if terminal.is_some() {
		return AssembledYield {
			data: Some(Value::String(last_text.to_string())),
			raw_text: true,
		};
	}
	AssembledYield {
		data: None,
		raw_text: false,
	}
}

fn render_assembled(assembled: &AssembledYield, fallback: &str) -> String {
	match &assembled.data {
		Some(Value::String(text)) if assembled.raw_text => text.clone(),
		Some(data) => {
			serde_json::to_string_pretty(data).unwrap_or_else(|_| fallback.to_string())
		},
		None => fallback.to_string(),
	}
}

fn finalize_output(
	item: &TaskItemSpec,
	turns: &[TurnResult],
	base: &str,
	aborted: bool,
) -> FinalizedOutput {
	if aborted {
		return FinalizedOutput {
			output: base.to_string(),
			stderr: String::new(),
			exit_code: 1,
			structured: None,
			failed: true,
		};
	}
	let caller_schema = item.output_schema.clone();
	let agent_schema = if item.schema_error.is_none() {
		item.agent.output.clone()
	} else {
		None
	};
	let source = if caller_schema.is_some() {
		"caller"
	} else if agent_schema.is_some() {
		"agent"
	} else {
		"none"
	};
	let schema = caller_schema.or(agent_schema);
	let labels = schema.as_ref().map(array_valued_labels).unwrap_or_default();
	let assembled = assemble_yields(&collect_yields(turns), base, &labels);

	let Some(schema) = schema.filter(|schema| !matches!(schema, Value::Bool(true))) else {
		let structured = (source != "none").then(|| StructuredOutput {
			source: source.to_string(),
			mode: item.schema_mode.as_str().to_string(),
			status: "valid".to_string(),
			data: assembled.data.clone(),
			error: None,
		});
		return FinalizedOutput {
			output: render_assembled(&assembled, base),
			stderr: item
				.schema_error
				.as_ref()
				.map(|error| format!("schema_warning: {error}\n"))
				.unwrap_or_default(),
			exit_code: 0,
			structured,
			failed: false,
		};
	};

	let Some(data) = assembled.data.clone() else {
		let message = "schema_violation: no structured result (agent did not yield result.data and its final message was not JSON)".to_string();
		let structured = StructuredOutput {
			source: source.to_string(),
			mode: item.schema_mode.as_str().to_string(),
			status: "invalid".to_string(),
			data: None,
			error: Some(message.clone()),
		};
		return if item.schema_mode == SchemaMode::Strict {
			let payload = json!({
				"error": "schema_violation",
				"message": message,
				"missingRequired": [],
				"data": Value::Null,
			});
			FinalizedOutput {
				output: serde_json::to_string_pretty(&payload).unwrap_or_else(|_| base.to_string()),
				stderr: message,
				exit_code: 1,
				structured: Some(structured),
				failed: true,
			}
		} else {
			FinalizedOutput {
				output: base.to_string(),
				stderr: format!("schema_warning: {message}\n"),
				exit_code: 0,
				structured: Some(structured),
				failed: false,
			}
		};
	};

	match validate_output(&schema, &data) {
		Ok(()) => FinalizedOutput {
			output: render_assembled(&assembled, base),
			stderr: String::new(),
			exit_code: 0,
			structured: Some(StructuredOutput {
				source: source.to_string(),
				mode: item.schema_mode.as_str().to_string(),
				status: "valid".to_string(),
				data: Some(data),
				error: None,
			}),
			failed: false,
		},
		Err(failure) => {
			let message = format!("schema_violation: {}", failure.message);
			let structured = StructuredOutput {
				source: source.to_string(),
				mode: item.schema_mode.as_str().to_string(),
				status: "invalid".to_string(),
				data: Some(data.clone()),
				error: Some(message.clone()),
			};
			if item.schema_mode == SchemaMode::Strict {
				let payload = json!({
					"error": "schema_violation",
					"message": failure.message,
					"missingRequired": failure.missing_required,
					"data": data,
				});
				FinalizedOutput {
					output: serde_json::to_string_pretty(&payload)
						.unwrap_or_else(|_| base.to_string()),
					stderr: message,
					exit_code: 1,
					structured: Some(structured),
					failed: true,
				}
			} else {
				FinalizedOutput {
					output: render_assembled(&assembled, base),
					stderr: format!("schema_warning: {message}\n"),
					exit_code: 0,
					structured: Some(structured),
					failed: false,
				}
			}
		},
	}
}

#[allow(clippy::too_many_arguments)]
fn single_failure(
	index: usize,
	id: String,
	name: String,
	agent: String,
	agent_source: String,
	task: String,
	message: String,
	duration_ms: u64,
	aborted: Option<bool>,
	abort_reason: Option<String>,
) -> SingleResult {
	let is_abort = aborted == Some(true);
	SingleResult {
		index,
		id,
		name,
		agent,
		agent_source,
		task,
		exit_code: 1,
		output: "(no output)".to_string(),
		stderr: if is_abort { String::new() } else { message.clone() },
		truncated: false,
		duration_ms,
		tokens: 0,
		requests: 0,
		error: if is_abort { None } else { Some(message) },
		aborted,
		abort_reason,
		extracted_tool_data: None,
		structured_output: None,
		resolved_model: None,
		model_role: None,
	}
}

#[derive(Clone)]
struct TaskItemSpec {
	name: String,
	agent: AgentDefinition,
	task: String,
	context: Option<String>,
	/// Normalized caller schema (already validated).
	output_schema: Option<Value>,
	schema_mode: SchemaMode,
	/// Non-fatal agent-schema parse error in permissive mode.
	schema_error: Option<String>,
	effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredOutput {
	pub source: String,
	pub mode: String,
	pub status: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub data: Option<Value>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SingleResult {
	pub index: usize,
	/// Supervisor-generated identity, used for `hub cancel`.
	pub id: String,
	/// Model-supplied display label (may repeat across children).
	pub name: String,
	pub agent: String,
	pub agent_source: String,
	pub task: String,
	pub exit_code: i32,
	pub output: String,
	pub stderr: String,
	pub truncated: bool,
	pub duration_ms: u64,
	pub tokens: u64,
	pub requests: usize,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub error: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub aborted: Option<bool>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub abort_reason: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub extracted_tool_data: Option<HashMap<String, Vec<Value>>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub structured_output: Option<StructuredOutput>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub resolved_model: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub model_role: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsyncTaskState {
	pub state: String,
	pub job_id: String,
	#[serde(rename = "type")]
	pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnReceipt {
	pub id: String,
	pub job_id: String,
	pub blocking: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskToolDetails {
	pub project_agents_dir: Option<String>,
	pub results: Vec<SingleResult>,
	pub total_duration_ms: u64,
	/// Model-facing spawn summary for a background call.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub message: Option<String>,
	/// Present only for a background call that returned before the jobs ran.
	#[serde(rename = "async", skip_serializing_if = "Option::is_none")]
	pub async_state: Option<AsyncTaskState>,
	/// Per-item receipts: background ids stay visible after the call returns,
	/// while blocking ids reconcile with `results`.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub spawns: Vec<SpawnReceipt>,
}

#[async_trait]
impl Tool for TaskTool {
	fn name(&self) -> &str {
		"task"
	}

	fn description(&self) -> &str {
		"Spawn one or more subagents to perform tasks."
	}

	fn schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"task": { "type": "string", "description": "The task instructions for a single agent." },
				"name": { "type": "string", "description": "Stable agent name." },
				"agent": { "type": "string", "description": "Agent type to spawn." },
				"context": { "type": "string", "description": "Shared background context for batch tasks." },
				"outputSchema": {
					"description": "JSON-Schema/JTD object, JSON string, or true. Child result.data is validated against it; unsupported keywords are rejected."
				},
				"schemaMode": {
					"type": "string",
					"enum": ["permissive", "strict"],
					"description": "strict fails the child on a schema violation; permissive keeps the result with a warning."
				},
				"effort": {
					"type": "string",
					"description": "Reasoning effort override: auto, off, minimal, low, medium, high, xhigh, max (unique prefixes from 2 chars)."
				},
				"isolated": {
					"type": "boolean",
					"description": "Not supported in this build; an explicit true is rejected instead of silently ignored."
				},
				"tasks": {
					"type": "array",
					"description": "Batch of subagent tasks.",
					"items": {
						"type": "object",
						"properties": {
							"name": { "type": "string" },
							"agent": { "type": "string" },
							"task": { "type": "string" },
							"outputSchema": {
								"description": "Per-item override of outputSchema."
							},
							"schemaMode": {
								"type": "string",
								"enum": ["permissive", "strict"]
							},
							"effort": { "type": "string" },
							"isolated": { "type": "boolean" }
						},
						"required": ["task"]
					}
				}
			}
		})
	}

	fn is_exclusive(&self) -> bool {
		false
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let limits = self.supervisor.limits();
		let catalog = self.catalog();
		let max_depth = catalog
			.as_ref()
			.and_then(|catalog| catalog.max_recursion_depth)
			.unwrap_or(limits.max_depth);
		if max_depth >= 0 && self.depth >= max_depth {
			return Err(ToolError::ExecutionFailed(format!(
				"Cannot spawn another agent at task depth {}; maximum depth is {max_depth}.",
				self.depth
			)));
		}

		let obj = args
			.as_object()
			.ok_or_else(|| ToolError::InvalidArguments("Arguments must be an object".to_string()))?;

		check_unsupported_fields(obj)?;
		let defaults = ItemDefaults {
			output_schema: parse_schema(obj.get("outputSchema"))?,
			schema_mode: parse_schema_mode(obj.get("schemaMode"))?.unwrap_or_default(),
			effort: parse_effort(obj.get("effort"))?,
		};
		let child_ctx = self.child_context();

		let items = if obj.contains_key("tasks") {
			let tasks_arr = obj
				.get("tasks")
				.and_then(Value::as_array)
				.ok_or_else(|| ToolError::InvalidArguments("'tasks' must be an array".to_string()))?;

			if tasks_arr.is_empty() {
				return Err(ToolError::InvalidArguments(
					"'tasks' array cannot be empty".to_string(),
				));
			}

			let context = obj.get("context").and_then(Value::as_str).map(|s| s.to_string());
			let mut items = Vec::with_capacity(tasks_arr.len());
			for item_val in tasks_arr {
				let item_obj = item_val.as_object().ok_or_else(|| {
					ToolError::InvalidArguments("Each item in 'tasks' must be an object".to_string())
				})?;
				items.push(self.build_item(item_obj, context.clone(), &defaults)?);
			}
			items
		} else {
			let context = obj.get("context").and_then(Value::as_str).map(|s| s.to_string());
			vec![self.build_item(obj, context, &defaults)?]
		};
		// Bound the batch before allocating records or spawning any child: an
		// oversized batch is rejected as a whole with an explicit capacity error
		// instead of silently queueing an unbounded backlog of result bodies.
		let capacity = self.supervisor.outstanding_capacity();
		if items.len() > capacity {
			return Err(ToolError::ExecutionFailed(format!(
				"task result capacity exhausted: {} item(s) requested, {} slot(s) available; wait for or consume finished jobs first",
				items.len(),
				capacity
			)));
		}

		if self.supervisor.background_ready() {
			let mut async_items = Vec::new();
			let mut sync_items = Vec::new();
			for (index, item) in items.into_iter().enumerate() {
				if item.agent.blocking {
					sync_items.push((index, item));
				} else {
					async_items.push((index, item));
				}
			}
			let mut started = Vec::new();
			let mut failures = Vec::new();
			for (index, item) in async_items {
				match self.spawn_background_task(index, item, ctx, child_ctx.clone()) {
					Ok(id) => started.push(id),
					Err(error) => failures.push(error),
				}
			}

			if started.is_empty() && sync_items.is_empty() {
				let message = if failures.is_empty() {
					"Failed to start background task job".to_string()
				} else {
					format!(
						"Failed to start background task job{}: {}",
						if failures.len() == 1 { "" } else { "s" },
						failures.join("; ")
					)
				};
				return Ok(ToolResult {
					tool_call_id: String::new(),
					content: message,
					is_error: false,
					terminate: false,
					details: None,
				});
			}

			let schedule_failure_summary = if failures.is_empty() {
				String::new()
			} else {
				format!(
					" Failed to schedule {} spawn{}: {}.",
					failures.len(),
					if failures.len() == 1 { "" } else { "s" },
					failures.join("; ")
				)
			};
			let coordination_hint = coordination_hint(&started);
			if sync_items.is_empty() {
				let content = if started.len() == 1 {
					format!(
						"Spawned agent `{}` (job `{}`). Its result auto-delivers on yield unless a settled `hub jobs`/`wait` snapshot consumes it first. {coordination_hint}",
						started[0], started[0]
					)
				} else {
					let listing = started
						.iter()
						.map(|id| format!("- `{id}` (job `{id}`)"))
						.collect::<Vec<_>>()
						.join("\n");
					format!(
						"Spawned {} background agents.{} Each result auto-delivers on yield unless a settled `hub jobs`/`wait` snapshot consumes it first.\n{listing}\n{coordination_hint}",
						started.len(),
						schedule_failure_summary
					)
				};
				let details = TaskToolDetails {
					project_agents_dir: catalog
						.as_ref()
						.and_then(|catalog| catalog.project_agents_dir.clone()),
					results: Vec::new(),
					total_duration_ms: 0,
					message: Some(content),
					async_state: Some(AsyncTaskState {
						state: "running".to_string(),
						job_id: started[0].clone(),
						kind: "task".to_string(),
					}),
					spawns: started
						.iter()
						.map(|id| SpawnReceipt {
							id: id.clone(),
							job_id: id.clone(),
							blocking: false,
						})
						.collect(),
				};
				let serialized = serde_json::to_string(&details).map_err(|e| {
					ToolError::ExecutionFailed(format!("Failed to serialize TaskToolDetails: {e}"))
				})?;
				return Ok(ToolResult {
					tool_call_id: String::new(),
					content: serialized,
					is_error: false,
					terminate: false,
					details: None,
				});
			}

			// Mixed call: async jobs already run detached; the blocking subset
			// gates this call's return and lands inline in `results`. Both sets
			// keep their receipts so post-return delivery never loses a spawn.
			let start_total = Instant::now();
			let futures = sync_items
				.into_iter()
				.map(|(index, item)| self.run_single_task(index, item, ctx, &child_ctx));
			let results = join_all(futures).await;
			let total_duration_ms = start_total.elapsed().as_millis() as u64;
			let mut spawns = started
				.iter()
				.map(|id| SpawnReceipt {
					id: id.clone(),
					job_id: id.clone(),
					blocking: false,
				})
				.collect::<Vec<_>>();
			spawns.extend(results.iter().map(|result| SpawnReceipt {
				id: result.id.clone(),
				job_id: result.id.clone(),
				blocking: true,
			}));
			let mut message = format!(
				"Ran {} blocking agent{} inline.",
				results.len(),
				if results.len() == 1 { "" } else { "s" }
			);
			if !started.is_empty() {
				let listing = started
					.iter()
					.map(|id| format!("- `{id}` (job `{id}`)"))
					.collect::<Vec<_>>()
					.join("\n");
				message = format!(
					"Spawned {} background agent{}.{} Each result auto-delivers on yield unless a settled `hub jobs`/`wait` snapshot consumes it first.\n{listing}\n{coordination_hint}\n{message}",
					started.len(),
					if started.len() == 1 { "" } else { "s" },
					schedule_failure_summary
				);
			} else if !schedule_failure_summary.is_empty() {
				message.push_str(&schedule_failure_summary);
			}
			let details = TaskToolDetails {
				project_agents_dir: catalog
					.as_ref()
					.and_then(|catalog| catalog.project_agents_dir.clone()),
				results,
				total_duration_ms,
				message: Some(message),
				async_state: started.first().map(|job_id| AsyncTaskState {
					state: "running".to_string(),
					job_id: job_id.clone(),
					kind: "task".to_string(),
				}),
				spawns,
			};
			let serialized = serde_json::to_string(&details).map_err(|e| {
				ToolError::ExecutionFailed(format!("Failed to serialize TaskToolDetails: {e}"))
			})?;
			return Ok(ToolResult {
				tool_call_id: String::new(),
				content: serialized,
				is_error: false,
				terminate: false,
				details: None,
			});
		}

		let start_total = Instant::now();
		let futures = items
			.into_iter()
			.enumerate()
			.map(|(index, item)| self.run_single_task(index, item, ctx, &child_ctx));

		let results = join_all(futures).await;
		let total_duration_ms = start_total.elapsed().as_millis() as u64;

		let spawns = results
			.iter()
			.map(|result| SpawnReceipt {
				id: result.id.clone(),
				job_id: result.id.clone(),
				blocking: true,
			})
			.collect();
		let details = TaskToolDetails {
			project_agents_dir: catalog
				.as_ref()
				.and_then(|catalog| catalog.project_agents_dir.clone()),
			results,
			total_duration_ms,
			message: None,
			async_state: None,
			spawns,
		};

		let content = serde_json::to_string(&details).map_err(|e| {
			ToolError::ExecutionFailed(format!("Failed to serialize TaskToolDetails: {e}"))
		})?;

		Ok(ToolResult {
			tool_call_id: String::new(),
			content,
			is_error: false,
			terminate: false,
			details: None,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use async_trait::async_trait;
	use crate::{
		provider::{ChunkStream, ModelProvider, ProviderError, ToolChoice},
		supervisor::DelegationLimits,
	};

	struct NullProvider;

	#[async_trait]
	impl ModelProvider for NullProvider {
		async fn complete(
			&self,
			_messages: &[Message],
			_tools: &[serde_json::Value],
			_tool_choice: &ToolChoice,
		) -> std::result::Result<Message, ProviderError> {
			Err(ProviderError::Network("unused in catalog tests".to_string()))
		}

		async fn stream(
			&self,
			_messages: &[Message],
			_tools: &[serde_json::Value],
			_tool_choice: &ToolChoice,
		) -> std::result::Result<ChunkStream, ProviderError> {
			Err(ProviderError::Network("unused in catalog tests".to_string()))
		}
	}

	struct NamedFactory(&'static str);

	impl ChildModelFactory for NamedFactory {
		fn can_resolve(&self, _model: &str) -> bool {
			true
		}

		fn resolve(
			&self,
			_model: &str,
			_effort: Option<&str>,
			_fallback_effort: Option<&str>,
		) -> std::result::Result<ChildModel, String> {
			Ok(ChildModel {
				provider: Arc::new(NullProvider),
				model: self.0.to_string(),
				role: None,
			})
		}
	}

	fn model_agent() -> AgentDefinition {
		AgentDefinition {
			name: "task".to_string(),
			description: "test".to_string(),
			tools: None,
			spawns: None,
			model: vec!["@task".to_string()],
			output: None,
			thinking_level: None,
			blocking: false,
			read_summarize: None,
			prewalk: None,
			advisor: None,
			autoload_skills: Vec::new(),
			system_prompt: String::new(),
			source: AgentSource::Bundled,
			file_path: "embedded:task.md".to_string(),
		}
	}

	/// A child spawned before `/model` keeps resolving grandchildren through the
	/// factory snapshot taken at spawn; a fresh root spawn uses the new factory.
	/// The catalog (definitions + policy) is shared, so this pins the capture
	/// boundary between stable definitions and the per-turn model resolver.
	#[test]
	fn grandchild_keeps_spawn_factory_after_root_model_update() {
		let supervisor = Supervisor::with_selection(
			ToolRegistry::new(),
			Arc::new(NullProvider),
			None,
			DelegationLimits::default(),
			true,
			false,
		);
		let catalog = Arc::new(
			AgentCatalog::new(Vec::new(), None)
				.with_model_factory(Arc::new(NamedFactory("old-model"))),
		);
		supervisor.set_agent_catalog(Arc::clone(&catalog));

		let root = TaskTool::new(Arc::clone(&supervisor), 0).with_catalog(Arc::clone(&catalog));
		let captured = root.child_context().model_factory.expect("spawn captures factory");

		// Root `/model` switch refreshes the shared catalog's resolver only.
		catalog.set_model_factory(Arc::new(NamedFactory("new-model")));

		let nested = TaskTool::new(Arc::clone(&supervisor), 1)
			.with_catalog(Arc::clone(&catalog))
			.with_model_factory(captured);
		let old_model = nested
			.child_context()
			.resolve_model(&model_agent(), None)
			.expect("old child resolves")
			.expect("model");
		assert_eq!(old_model.model, "old-model");

		let fresh = TaskTool::new(supervisor, 0).with_catalog(catalog);
		let new_model = fresh
			.child_context()
			.resolve_model(&model_agent(), None)
			.expect("new root spawn resolves")
			.expect("model");
		assert_eq!(new_model.model, "new-model");
	}

	fn named_agent(name: &str, spawns: Option<Spawns>) -> AgentDefinition {
		let mut agent = model_agent();
		agent.name = name.to_string();
		agent.model = Vec::new();
		agent.spawns = spawns;
		agent
	}

	#[test]
	fn spawn_policy_rejects_self_spawn_spawns_and_disabled_agents() {
		let supervisor = Supervisor::with_selection(
			ToolRegistry::new(),
			Arc::new(NullProvider),
			None,
			DelegationLimits::default(),
			true,
			false,
		);
		let catalog = Arc::new(
			AgentCatalog::new(
				vec![
					named_agent("task", None),
					named_agent("scout", Some(Spawns::Only(vec!["task".to_string()]))),
					named_agent("blocked", None),
				],
				None,
			)
			.with_policy(
				vec!["blocked".to_string()],
				std::collections::BTreeMap::new(),
				None,
				None,
			),
		);
		supervisor.set_agent_catalog(Arc::clone(&catalog));
		let root = TaskTool::new(supervisor, 0).with_catalog(catalog);

		let self_spawn = root
			.clone()
			.with_agent_context(
				"scout".to_string(),
				Some(Spawns::Only(vec!["task".to_string()])),
				None,
			)
			.resolve_agent(Some("scout"))
			.expect_err("self spawn");
		assert!(self_spawn.to_string().contains("recursion prevention"));

		let forbidden = root
			.clone()
			.with_agent_context("scout".to_string(), Some(Spawns::Only(Vec::new())), None)
			.resolve_agent(Some("task"))
			.expect_err("spawn not allowed");
		assert!(forbidden.to_string().contains("Cannot spawn"));

		let disabled = root
			.clone()
			.resolve_agent(Some("blocked"))
			.expect_err("disabled");
		assert!(disabled.to_string().contains("disabled in settings"));

		let unknown = root
			.clone()
			.resolve_agent(Some("nope"))
			.expect_err("unknown");
		assert!(unknown.to_string().contains("Unknown agent"));

		assert_eq!(root.resolve_agent(Some("scout")).expect("allowed").name, "scout");
	}

	#[test]
	fn missing_agent_tools_fail_closed_before_spawn() {
		let supervisor = Supervisor::with_selection(
			ToolRegistry::with_builtins(),
			Arc::new(NullProvider),
			None,
			DelegationLimits::default(),
			true,
			false,
		);
		let context = ChildContext {
			supervisor,
			depth: 0,
			parent_tools: None,
			catalog: None,
			model_factory: None,
		};
		let mut scout = named_agent("scout", None);
		scout.tools = Some(vec![
			"read".to_string(),
			"grep".to_string(),
			"glob".to_string(),
			"web_search".to_string(),
			"yield".to_string(),
		]);
		let error = context
			.prepare_registry("scout-1", &scout, None)
			.err()
			.expect("missing web_search");
		assert!(error.contains("web_search"));
		assert!(error.contains("does not implement"));

		let mut supported = scout.clone();
		supported.tools = Some(vec!["read".to_string(), "grep".to_string(), "yield".to_string()]);
		let (registry, _tools) = context
			.prepare_registry("scout-2", &supported, None)
			.expect("supported tools");
		assert!(registry.get("read").is_some());

		let mut typo = scout;
		typo.tools = Some(vec!["read".to_string(), "fill".to_string(), "yield".to_string()]);
		let error = context.prepare_registry("typo-1", &typo, None).err().expect("unknown");
		assert!(error.contains("requests tool 'fill'"));
	}

	#[tokio::test]
	async fn preflight_failures_release_admission_slots() {
		let supervisor = Supervisor::with_selection(
			ToolRegistry::with_builtins(),
			Arc::new(NullProvider),
			None,
			DelegationLimits::default(),
			true,
			false,
		);
		let mut missing_tool = named_agent("missing-tool", None);
		missing_tool.tools = Some(vec!["does-not-exist".to_string(), "yield".to_string()]);
		missing_tool.blocking = true;
		let mut missing_model = named_agent("missing-model", None);
		missing_model.model = vec!["@no-factory".to_string()];
		missing_model.tools = Some(vec!["yield".to_string()]);
		missing_model.blocking = true;
		let catalog = Arc::new(AgentCatalog::new(
			vec![missing_tool, missing_model],
			None,
		));
		let tool = TaskTool::new(Arc::clone(&supervisor), 0).with_catalog(catalog);
		let ctx = AgentContext::new(std::env::temp_dir());
		let before = supervisor.outstanding_capacity();
		for index in 0..(crate::supervisor::MAX_OUTSTANDING_RESULTS + 2) {
			for agent in ["missing-tool", "missing-model"] {
				let result = tool
					.execute(
						&serde_json::json!({ "agent": agent, "task": format!("probe {index}") }),
						&ctx,
					)
					.await
					.expect("tool envelope");
				assert!(!result.content.is_empty());
				assert_eq!(
					supervisor.outstanding_capacity(),
					before,
					"preflight failure leaked an admission slot"
				);
			}
		}
	}

	#[test]
	fn unsupported_isolation_and_agent_capabilities_are_explicit() {
		assert!(check_unsupported_fields(
			serde_json::json!({ "isolated": true }).as_object().expect("object")
		)
		.is_err());
		assert!(check_unsupported_fields(
			serde_json::json!({ "isolated": false }).as_object().expect("object")
		)
		.is_ok());
		let mut agent = named_agent("handoff", None);
		agent.prewalk = Some(crate::tools::task_catalog::HandoffTarget::Default);
		let error = check_agent_capabilities(&agent).expect_err("prewalk");
		assert!(error.to_string().contains("prewalk"));
	}

	#[test]
	fn unrestricted_parent_ceiling_stays_none() {
		let supervisor = Supervisor::with_selection(
			ToolRegistry::with_builtins(),
			Arc::new(NullProvider),
			None,
			DelegationLimits::default(),
			true,
			false,
		);
		let tool = TaskTool::new(supervisor, 0);
		let context = tool.child_context();
		let mut agent = model_agent();
		agent.tools = None;
		let (_registry, effective) = context
			.prepare_registry("task-1", &agent, None)
			.expect("registry");
		assert!(effective.is_none(), "None ceiling must not expand to a set");
	}
}
