//! Real CLI wiring for the eval host seams.
//!
//! The `pi-agent` eval tool owns the bridge and kernel protocol; this module
//! supplies the concrete host (session tools, provider completions, subagents,
//! settings, artifacts, background delivery) and a binder that mounts the tool
//! into root and child registries.

use std::{
	collections::BTreeSet,
	path::{Path, PathBuf},
	sync::{
		Arc, Weak,
		atomic::{AtomicU64, Ordering},
	},
	time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use pi_agent::{
	eval::{
		ArtifactRef, AutoBackground, DEFAULT_AUTO_BACKGROUND_THRESHOLD_MS,
		EVAL_DEFAULT_TIMEOUT_SECS, EvalAllowance, EvalFlagOverrides, EvalHost, EvalRuntime,
		EvalSessionInfo, EvalSettings, EvalTool, EvalToolBinder, Interpreters,
	},
	provider::{ModelProvider, ToolChoice},
	state::{AgentContext, Message, ToolCall, ToolResult},
	supervisor::{DelegationLimits, Supervisor},
	tools::{
		ToolError,
		registry::ToolRegistry,
		scheduler::ToolScheduler,
		task_catalog::ChildModelFactory,
	},
};
use serde_json::{Value, json};
use tokio::{
	fs,
	io::AsyncWriteExt,
	sync::Mutex,
};

/// Build eval settings from config layers plus `PI_PY`/`PI_JS`/`PI_RB`/`PI_JL`.
///
/// Environment booleans win whenever set, including `0`/`false`/empty (which
/// disable). Unset defers to `eval.*`; unset-and-unconfigured keeps upstream
/// defaults (py/js on, rb/jl off).
pub fn eval_settings(config: &crate::config::Config) -> EvalSettings {
	let raw = &config.eval_config;
	let mut allowance = EvalAllowance::default();
	for (language, value) in [
		(pi_agent::eval::EvalLanguage::Py, raw.python),
		(pi_agent::eval::EvalLanguage::Js, raw.js),
		(pi_agent::eval::EvalLanguage::Rb, raw.ruby),
		(pi_agent::eval::EvalLanguage::Jl, raw.julia),
	] {
		if let Some(value) = value {
			allowance.set(language, value);
		}
	}
	let overrides = EvalFlagOverrides::from_values(
		env_flag("PI_PY"),
		env_flag("PI_JS"),
		env_flag("PI_RB"),
		env_flag("PI_JL"),
	);
	let mut settings = EvalSettings::new(
		allowance,
		Interpreters::new(
			raw.python_interpreter.clone(),
			raw.ruby_interpreter.clone(),
			raw.julia_interpreter.clone(),
		),
	)
	.with_flag_overrides(overrides);
	if let Some(mode) = raw.python_kernel_mode {
		settings = settings.with_kernel_mode(mode);
	}
	if let Some(seconds) = config.max_tool_timeout_secs {
		settings = settings.with_timeouts(EVAL_DEFAULT_TIMEOUT_SECS, seconds);
	}
	// Upstream reads the threshold only after the enable gate; an unset
	// threshold keeps the 60s default instead of backgrounding immediately.
	if raw.auto_background_enabled == Some(true) {
		let threshold_ms = raw
			.auto_background_threshold_ms
			.unwrap_or(DEFAULT_AUTO_BACKGROUND_THRESHOLD_MS);
		settings = settings.with_auto_background(Some(AutoBackground { threshold_ms }));
	}
	settings
}

/// `None` = unset. A set variable is truthy only for the documented tokens.
fn env_flag(name: &str) -> Option<bool> {
	std::env::var(name).ok().map(|value| {
		matches!(
			value.trim(),
			"1" | "Y" | "y" | "TRUE" | "true" | "YES" | "yes" | "ON" | "on"
		)
	})
}

/// Session-scoped inputs shared by root and child mounts.
pub struct EvalMountContext {
	pub runtime:      Arc<EvalRuntime>,
	pub supervisor:   Option<Weak<Supervisor>>,
	pub provider:     Arc<dyn ModelProvider>,
	pub model_factory: Option<Arc<dyn ChildModelFactory>>,
	pub session_id:   String,
	pub session_file: Option<PathBuf>,
	pub artifacts_dir: Option<PathBuf>,
	pub limits:       DelegationLimits,
	/// Session goal/budget controller backing `budget()` and the `+Nk!` gate.
	/// `None` for a caller that has no goal runtime: the helpers fail closed
	/// instead of fabricating a snapshot.
	pub goal:         Option<Arc<crate::goal::GoalRuntime>>,
}

/// Mounts eval into a registry. Root uses `owner = None`; children use their
/// supervisor child id, which becomes a fresh eval session/owner.
pub struct CliEvalBinder {
	context: EvalMountContext,
}

impl CliEvalBinder {
	pub fn new(context: EvalMountContext) -> Self {
		Self { context }
	}

	pub fn context(&self) -> &EvalMountContext {
		&self.context
	}

	/// Mount one host. `provider` is the child loop's resolved provider when
	/// known; root and pre-resolution binds fall back to the session provider.
	fn bind_host(
		&self,
		registry: &mut ToolRegistry,
		owner: Option<&str>,
		provider: Option<Arc<dyn ModelProvider>>,
	) {
		let parent_tools = owner.map(|_| registry.names().into_iter().collect::<BTreeSet<_>>());
		let tools = bridge_tool_registry(registry);
		let session_id = match owner {
			Some(owner) => format!("{}:{owner}", self.context.session_id),
			None => self.context.session_id.clone(),
		};
		let host = Arc::new(CliEvalHost {
			supervisor: self.context.supervisor.clone(),
			provider: provider.unwrap_or_else(|| Arc::clone(&self.context.provider)),
			model_factory: self.context.model_factory.clone(),
			tools,
			parent_tools,
			owner: owner.map(str::to_string),
			limits: self.context.limits,
			artifacts_dir: self.context.artifacts_dir.clone(),
			next_artifact: Mutex::new(None),
			goal: self.context.goal.clone(),
		});
		let session = EvalSessionInfo::new(session_id)
			.with_owner(owner.map(str::to_string))
			.with_session_file(self.context.session_file.clone())
			.with_artifacts_dir(self.context.artifacts_dir.clone());
		registry.register(Arc::new(EvalTool::new(
			Arc::clone(&self.context.runtime),
			host,
			session,
		)));
	}
}

#[async_trait]
impl EvalToolBinder for CliEvalBinder {
	fn bind(&self, registry: &mut ToolRegistry, owner: Option<&str>) {
		self.bind_host(registry, owner, None);
	}

	fn bind_with_provider(
		&self,
		registry: &mut ToolRegistry,
		owner: Option<&str>,
		provider: Option<Arc<dyn ModelProvider>>,
	) {
		self.bind_host(registry, owner, provider);
	}

	async fn dispose_owner(&self, owner: &str) {
		self.context.runtime.dispose_owner(Some(owner)).await;
	}
}

/// Tools the host owns directly (`eval` re-enters this runtime; `task`/`hub`
/// belong to the supervisor). One source for the bridge exclusion and for the
/// reserved-name check that keeps host tool definitions from taking these.
pub const OWNED_TOOL_NAMES: [&str; 3] = ["eval", "task", "hub"];

/// Clone the session registry for bridged `tool.*` calls, dropping tools that
/// would close an ownership cycle back to this host (`eval` itself would
/// re-enter the same runtime/host from inside a running cell) or reach
/// supervisor-owned delegation (`task`/`hub`). Structured subagents remain
/// available through `agent()`, which owns reserve/depth/admission/disposal;
/// upstream bridges every enabled tool, so this is a deliberate narrowing.
fn bridge_tool_registry(registry: &ToolRegistry) -> ToolRegistry {
	let mut tools = registry.clone();
	for name in OWNED_TOOL_NAMES {
		tools.remove(name);
	}
	tools
}

/// Concrete [`EvalHost`].
pub struct CliEvalHost {
	supervisor:    Option<Weak<Supervisor>>,
	provider:      Arc<dyn ModelProvider>,
	model_factory: Option<Arc<dyn ChildModelFactory>>,
	tools:         ToolRegistry,
	parent_tools:  Option<BTreeSet<String>>,
	owner:         Option<String>,
	limits:        DelegationLimits,
	artifacts_dir: Option<PathBuf>,
	next_artifact: Mutex<Option<u64>>,
	goal:          Option<Arc<crate::goal::GoalRuntime>>,
}

impl CliEvalHost {
	fn supervisor(&self) -> Result<Arc<Supervisor>, String> {
		self
			.supervisor
			.as_ref()
			.and_then(Weak::upgrade)
			.ok_or_else(|| {
				"subagents are unavailable: this session has no supervisor".to_string()
			})
	}

	fn provider_for(&self, model: &str) -> Result<Arc<dyn ModelProvider>, String> {
		match model {
			"default" => Ok(Arc::clone(&self.provider)),
			"smol" | "slow" => {
				let factory = self.model_factory.as_ref().ok_or_else(|| {
					format!("model role `{model}` is not configured (no model catalog)")
				})?;
				let resolved = factory
					.resolve(model, None, None)
					.map_err(|error| format!("model role `{model}`: {error}"))?;
				Ok(resolved.provider)
			},
			other => Err(format!(
				"completion model must be `default`, `smol`, or `slow` (got `{other}`)"
			)),
		}
	}

	async fn write_artifact_file(&self, kind: &str, bytes: &[u8]) -> Result<Option<ArtifactRef>, String> {
		let Some(directory) = self.artifacts_dir.clone() else {
			return Ok(None);
		};
		fs::create_dir_all(&directory)
			.await
			.map_err(|error| format!("artifacts directory {}: {error}", directory.display()))?;
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			// Best effort: the parent session directory is private already.
			let _ = fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).await;
		}
		let kind = sanitize_kind(kind);
		let mut next = self.next_artifact.lock().await;
		if next.is_none() {
			*next = Some(scan_next_artifact_id(&directory).await?);
		}
		// Stage once under an exclusive, non-public sibling name. The public
		// `N.<kind>.log` name is only ever created by `hard_link`, so it can
		// never be observed empty or partial from this writer.
		let Some((staged, mut file)) = create_artifact_stage(&directory, &kind).await? else {
			return Err("could not create an exclusive artifact staging file".to_string());
		};
		let written = async {
			file.write_all(bytes).await?;
			file.flush().await?;
			file.sync_all().await?;
			Ok::<(), std::io::Error>(())
		}
		.await;
		drop(file);
		if let Err(error) = written {
			let _ = fs::remove_file(&staged).await;
			return Err(format!("stage {}: {error}", staged.display()));
		}
		match fs::metadata(&staged).await {
			Ok(metadata) if metadata.len() == bytes.len() as u64 => {},
			Ok(metadata) => {
				let _ = fs::remove_file(&staged).await;
				return Err(format!(
					"stage {}: wrote {} of {} bytes",
					staged.display(),
					metadata.len(),
					bytes.len()
				));
			},
			Err(error) => {
				let _ = fs::remove_file(&staged).await;
				return Err(format!("verify {}: {error}", staged.display()));
			},
		}
		loop {
			let id = (*next).expect("artifact id initialized");
			let path = directory.join(format!("{id}.{kind}.log"));
			match publish_staged_artifact(&staged, &path).await {
				Ok(true) => {
					let _ = fs::remove_file(&staged).await;
					*next = Some(id + 1);
					return Ok(Some(ArtifactRef {
						id: id.to_string(),
						path: Some(path.display().to_string()),
					}));
				},
				Ok(false) => {
					// Another writer already published this id; keep the staged
					// copy and retry the next id without touching their file.
					*next = Some(id + 1);
				},
				Err(error) => {
					let _ = fs::remove_file(&staged).await;
					return Err(format!("publish {}: {error}", path.display()));
				},
			}
		}
	}
}

#[async_trait]
impl EvalHost for CliEvalHost {
	async fn call_tool(
		&self,
		name: &str,
		args: Value,
		ctx: &AgentContext,
	) -> Result<ToolResult, ToolError> {
		if OWNED_TOOL_NAMES.contains(&name) {
			return Err(ToolError::InvalidArguments(format!(
				"`{name}` is not callable from eval; use agent() for subagents"
			)));
		}
		if self.tools.get(name).is_none() {
			return Err(ToolError::InvalidArguments(format!(
				"unknown tool `{name}` in this session"
			)));
		}
		let call = ToolCall { id: "eval-bridge".to_string(), name: name.to_string(), arguments: args };
		let scheduler = ToolScheduler::new(self.tools.clone());
		let execution = scheduler.execute_batch(std::slice::from_ref(&call), ctx);
		let mut results = tokio::select! {
			_ = ctx.cancellation_token.cancelled() => {
				return Err(ToolError::Aborted(format!("tool `{name}` call cancelled")));
			},
			results = execution => results,
		};
		results.pop().ok_or_else(|| {
			ToolError::ExecutionFailed(format!("tool `{name}` produced no result"))
		})
	}

	async fn completion(&self, args: Value, ctx: &AgentContext) -> Result<Value, String> {
		let prompt = args
			.get("prompt")
			.and_then(Value::as_str)
			.ok_or_else(|| "completion() requires a string `prompt`".to_string())?;
		let system = match args.get("system") {
			None | Some(Value::Null) => None,
			Some(Value::String(value)) => Some(value.as_str()),
			Some(_) => return Err("completion() `system` must be a string".to_string()),
		};
		let schema = args.get("schema").filter(|value| !value.is_null());
		if let Some(schema) = schema
			&& !schema.is_object()
		{
			return Err("completion() `schema` must be an object".to_string());
		}
		let model = match args.get("model") {
			None | Some(Value::Null) => "default",
			Some(Value::String(value)) => value.as_str(),
			Some(_) => return Err("completion() `model` must be a string".to_string()),
		};
		// Cancellation law: never dispatch after a pre-cancelled context, and let
		// cancellation win over an instantly-ready provider.
		if ctx.cancellation_token.is_cancelled() {
			return Err("completion() cancelled".to_string());
		}
		let provider = self.provider_for(model)?;
		let messages = completion_messages(prompt, system, schema)?;
		let completion = provider.complete(&messages, &[], &ToolChoice::Auto);
		let message = tokio::select! {
			biased;
			_ = ctx.cancellation_token.cancelled() => {
				return Err("completion() cancelled".to_string());
			},
			result = completion => result.map_err(|error| format!("completion failed: {error}"))?,
		};
		if ctx.cancellation_token.is_cancelled() {
			return Err("completion() cancelled".to_string());
		}
		let text = completion_text(&message, schema.is_some())?;
		Ok(json!({ "text": text }))
	}

	async fn agent(&self, args: Value, ctx: &AgentContext) -> Result<Value, String> {
		let supervisor = self.supervisor()?;
		if !supervisor.task_enabled() {
			return Err("subagents are disabled for this session".to_string());
		}
		// TS `runEvalAgent` hard gate: a `+Nk!` ceiling refuses to spawn once the
		// turn's spend reached it. Advisory ceilings never block.
		if let Some(goal) = &self.goal {
			let snapshot = goal.budget_snapshot();
			if snapshot.hard
				&& let Some(total) = snapshot.total
				&& snapshot.spent >= total
			{
				return Err(format!(
					"agent() blocked: turn token budget exhausted ({}/{total} output tokens). \
					 Raise or drop the +Nk! ceiling to continue.",
					snapshot.spent
				));
			}
		}
		let tool = supervisor.eval_task_tool(self.owner.as_deref(), self.parent_tools.clone());
		// No outer cancellation race: `run_single_task` already selects on this
		// token, cancels the child, and disposes its eval owner. Dropping the
		// future here would abort the child without that cleanup.
		let result = tool.run_eval_agent(&args, ctx).await?;
		// Child output is part of this turn's spend once, from the reported
		// usage the delegation result carries (absent -> nothing added).
		if let Some(goal) = &self.goal
			&& let Some(output) = result
				.get("details")
				.and_then(|details| details.get("usage"))
				.and_then(|usage| usage.get("output"))
				.and_then(Value::as_u64)
		{
			goal.record_eval_subagent_output(output);
		}
		Ok(result)
	}

	async fn budget(&self, _args: Value, ctx: &AgentContext) -> Result<Value, String> {
		if ctx.cancellation_token.is_cancelled() {
			return Err("budget() cancelled".to_string());
		}
		let Some(goal) = &self.goal else {
			return Err(
				"Goal Mode token budget is unavailable: this build has no Goal Mode token \
				 accounting source, so eval will not fabricate a snapshot"
					.to_string(),
			);
		};
		// Precedence lives on the runtime: `+Nk` window, enabled goal budget,
		// then the advisory output delta. `spent` is always this session's spend.
		let snapshot = goal.budget_snapshot();
		Ok(json!({
			"total": snapshot.total,
			"spent": snapshot.spent,
			"hard": snapshot.hard,
		}))
	}

	fn concurrency(&self, _args: &Value) -> Result<Value, String> {
		let limit = self
			.supervisor
			.as_ref()
			.and_then(Weak::upgrade)
			.map_or(self.limits.max_concurrency, |supervisor| {
				supervisor.limits().max_concurrency
			});
		Ok(json!({ "limit": limit }))
	}

	async fn write_artifact(
		&self,
		kind: &str,
		bytes: &[u8],
	) -> Result<Option<ArtifactRef>, String> {
		CliEvalHost::write_artifact_file(self, kind, bytes).await
	}

	fn enqueue_background_result(&self, job_id: &str, text: String) -> bool {
		let Some(supervisor) = self.supervisor.as_ref().and_then(Weak::upgrade) else {
			return false;
		};
		if !supervisor.background_ready() {
			return false;
		}
		let text = format!("Eval background job `{job_id}` finished:\n{text}");
		supervisor.enqueue_owner_result(self.owner.as_deref(), job_id, text)
	}
}

fn completion_messages(
	prompt: &str,
	system: Option<&str>,
	schema: Option<&Value>,
) -> Result<Vec<Message>, String> {
	if prompt.trim().is_empty() {
		return Err("completion() `prompt` must not be empty".to_string());
	}
	let mut system_text = system.unwrap_or_default().to_string();
	if let Some(schema) = schema {
		if !system_text.is_empty() {
			system_text.push_str("\n\n");
		}
		system_text.push_str(
			"Respond with only a single JSON value (no prose, no Markdown fences) that \
			 validates against this JSON Schema:\n",
		);
		system_text.push_str(
			&serde_json::to_string(schema)
				.map_err(|error| format!("completion() schema encoding: {error}"))?,
		);
	}
	let mut messages = Vec::with_capacity(2);
	if !system_text.is_empty() {
		messages.push(Message::system(system_text));
	}
	messages.push(Message::user(prompt));
	Ok(messages)
}

fn completion_text(message: &Message, schema: bool) -> Result<String, String> {
	let text = message.content.trim().to_string();
	if text.is_empty() {
		return Err("completion() returned an empty response".to_string());
	}
	if !schema {
		return Ok(text);
	}
	let unfenced = text
		.strip_prefix("```json")
		.or_else(|| text.strip_prefix("```"))
		.map(|inner| inner.strip_suffix("```").unwrap_or(inner).trim())
		.unwrap_or(&text);
	let parsed: Value = serde_json::from_str(unfenced)
		.map_err(|error| format!("completion() did not return JSON: {error}"))?;
	Ok(parsed.to_string())
}

fn sanitize_kind(kind: &str) -> String {
	let sanitized: String = kind
		.chars()
		.filter(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
		.take(64)
		.collect();
	if sanitized.is_empty() {
		"artifact".to_string()
	} else {
		sanitized
	}
}

/// Scan existing `N.<kind>.log` files and return the next free numeric prefix.
/// Staging names never end in `.log`, so an interrupted publication cannot
/// block or impersonate a published artifact.
async fn scan_next_artifact_id(directory: &Path) -> Result<u64, String> {
	let mut next = 0;
	let mut entries = fs::read_dir(directory)
		.await
		.map_err(|error| format!("read {}: {error}", directory.display()))?;
	while let Some(entry) = entries
		.next_entry()
		.await
		.map_err(|error| format!("read {}: {error}", directory.display()))?
	{
		let name = entry.file_name();
		let Some(name) = name.to_str() else {
			continue;
		};
		if !name.ends_with(".log") {
			continue;
		}
		if let Some(prefix) = name.split('.').next()
			&& let Ok(id) = prefix.parse::<u64>()
		{
			next = next.max(id.saturating_add(1));
		}
	}
	Ok(next)
}

/// Create a complete, exclusively named staging sibling for an artifact.
/// Returns `None` only if every unique candidate was taken (never retried into
/// another writer's file).
async fn create_artifact_stage(
	directory: &Path,
	kind: &str,
) -> Result<Option<(PathBuf, fs::File)>, String> {
	static SEQUENCE: AtomicU64 = AtomicU64::new(0);
	for _ in 0..16 {
		let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
		let nanos = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.map(|duration| duration.as_nanos())
			.unwrap_or(0);
		let path = directory.join(format!(
			".{kind}.log.stage-{}-{nanos}-{sequence}",
			std::process::id()
		));
		match fs::OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(&path)
			.await
		{
			Ok(file) => return Ok(Some((path, file))),
			Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {},
			Err(error) => {
				return Err(format!("stage {}: {error}", path.display()));
			},
		}
	}
	Ok(None)
}

/// Publish an already complete staging file under `final_path` without
/// replacing an existing artifact. The hard link is atomic and no-clobber:
/// `Ok(false)` means another writer owns this id and the caller must retry
/// the next id, leaving the existing file untouched.
async fn publish_staged_artifact(staged: &Path, final_path: &Path) -> Result<bool, String> {
	match fs::hard_link(staged, final_path).await {
		Ok(()) => Ok(true),
		Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
		Err(error) => Err(error.to_string()),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use pi_agent::{CancellationToken, tools::Tool};
	use tokio::sync::Notify;

	fn temp_dir(prefix: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"{prefix}-{}-{}",
			std::process::id(),
			std::time::SystemTime::now()
				.duration_since(std::time::UNIX_EPOCH)
				.unwrap()
				.as_nanos()
		));
		std::fs::create_dir_all(&dir).expect("temp dir");
		dir
	}

	fn host(artifacts_dir: Option<PathBuf>) -> CliEvalHost {
		CliEvalHost {
			supervisor: None,
			provider: Arc::new(UnusedProvider),
			model_factory: None,
			tools: ToolRegistry::new(),
			parent_tools: None,
			owner: None,
			limits: DelegationLimits::default(),
			artifacts_dir,
			next_artifact: Mutex::new(None),
			goal: None,
		}
	}

	struct UnusedProvider;

	#[async_trait]
	impl ModelProvider for UnusedProvider {
		async fn complete(
			&self,
			_messages: &[Message],
			_tools: &[Value],
			_tool_choice: &ToolChoice,
		) -> Result<Message, pi_agent::provider::ProviderError> {
			Err(pi_agent::provider::ProviderError::Protocol(
				"test provider is not callable".to_string(),
			))
		}

		async fn stream(
			&self,
			_messages: &[Message],
			_tools: &[Value],
			_tool_choice: &ToolChoice,
		) -> Result<pi_agent::provider::ChunkStream, pi_agent::provider::ProviderError> {
			Err(pi_agent::provider::ProviderError::Protocol(
				"test provider cannot stream".to_string(),
			))
		}
	}

	#[test]
	fn env_flags_win_and_unset_defers() {
		let _env = crate::TEST_ENV_LOCK.lock().expect("env lock");
		let mut config = crate::config::Config::default();
		config.eval_config.python = Some(true);
		config.eval_config.ruby = Some(true);
		// SAFETY: guarded by TEST_ENV_LOCK.
		unsafe {
			std::env::set_var("PI_PY", "0");
			std::env::set_var("PI_RB", "yes");
			std::env::remove_var("PI_JS");
			std::env::remove_var("PI_JL");
		}
		let settings = eval_settings(&config);
		assert!(!settings.allowance().python, "set 0 disables over eval.py=true");
		assert!(settings.allowance().ruby, "set 1 enables over eval.rb=true");
		assert!(settings.allowance().js, "unset keeps default");
		assert!(!settings.allowance().julia);
		// SAFETY: guarded by TEST_ENV_LOCK.
		unsafe {
			std::env::set_var("PI_PY", "false");
			std::env::set_var("PI_JS", "");
		}
		let settings = eval_settings(&config);
		assert!(!settings.allowance().python);
		assert!(!settings.allowance().js);
		unsafe {
			std::env::remove_var("PI_PY");
			std::env::remove_var("PI_JS");
			std::env::remove_var("PI_RB");
		}
	}

	#[test]
	fn settings_carry_kernel_mode_interpreters_and_timeout_cap() {
		let mut config = crate::config::Config::default();
		config.eval_config.python_kernel_mode = Some(pi_agent::eval::KernelMode::PerCall);
		config.eval_config.python_interpreter = Some("/opt/python".to_string());
		config.max_tool_timeout_secs = Some(7);
		let settings = eval_settings(&config);
		assert_eq!(settings.kernel_mode(), pi_agent::eval::KernelMode::PerCall);
		assert_eq!(settings.interpreters().python.as_deref(), Some("/opt/python"));
		assert_eq!(settings.resolve_timeout_secs(None), 7);
		assert_eq!(settings.resolve_timeout_secs(Some(99)), 7);
	}

	#[test]
	fn auto_background_settings_follow_the_enable_gate_and_threshold_default() {
		let mut config = crate::config::Config::default();
		// Disabled and threshold-only both keep auto-background off; a stray
		// threshold must never enable it.
		assert_eq!(eval_settings(&config).auto_background(), None);
		config.eval_config.auto_background_threshold_ms = Some(250);
		assert_eq!(eval_settings(&config).auto_background(), None);

		// Enabled without a threshold keeps the upstream 60s default.
		config.eval_config.auto_background_threshold_ms = None;
		config.eval_config.auto_background_enabled = Some(true);
		assert_eq!(
			eval_settings(&config).auto_background(),
			Some(AutoBackground { threshold_ms: DEFAULT_AUTO_BACKGROUND_THRESHOLD_MS })
		);

		// Explicit values, including the immediate-background boundary, pass
		// through unchanged.
		config.eval_config.auto_background_threshold_ms = Some(0);
		assert_eq!(
			eval_settings(&config).auto_background(),
			Some(AutoBackground { threshold_ms: 0 })
		);
		config.eval_config.auto_background_threshold_ms = Some(5_000);
		assert_eq!(
			eval_settings(&config).auto_background(),
			Some(AutoBackground { threshold_ms: 5_000 })
		);

		// `false` stays off even when a threshold is configured.
		config.eval_config.auto_background_enabled = Some(false);
		assert_eq!(eval_settings(&config).auto_background(), None);
	}

	#[test]
	fn completion_schema_is_strict_json_and_strips_fences() {
		let schema = json!({"type": "object", "properties": {"answer": {"type": "string"}}});
		let messages = completion_messages("question", Some("be terse"), Some(&schema))
			.expect("messages");
		assert_eq!(messages.len(), 2);
		assert!(messages[0].content.contains("JSON Schema"));
		assert!(messages[0].content.contains("be terse"));
		assert_eq!(messages[1].content, "question");
		assert!(completion_messages("", None, None).is_err());

		let plain = Message::assistant("  hello  ");
		assert_eq!(completion_text(&plain, false).unwrap(), "hello");
		let fenced = Message::assistant("```json\n{\"answer\":\"ok\"}\n```");
		assert_eq!(
			completion_text(&fenced, true).unwrap(),
			"{\"answer\":\"ok\"}"
		);
		assert!(completion_text(&Message::assistant("not json"), true).is_err());
		assert!(completion_text(&Message::assistant("   "), false).is_err());
	}

	#[tokio::test]
	async fn artifacts_are_atomic_unique_and_session_scoped() {
		let root = temp_dir("omp-eval-artifacts");
		let directory = root.join("session");
		std::fs::create_dir_all(&directory).expect("session dir");
		let session_host = Arc::new(host(Some(directory.clone())));
		// Pre-existing artifact must never be overwritten and drives the scan.
		std::fs::write(directory.join("4.eval.log"), b"old").expect("seed artifact");
		let (first, second) = tokio::join!(
			session_host.write_artifact("eval", b"first"),
			session_host.write_artifact("eval", b"second")
		);
		let first = first.expect("first").expect("available");
		let second = second.expect("second").expect("available");
		assert_ne!(first.id, second.id);
		assert!(
			first.id.parse::<u64>().unwrap_or(0) >= 5 && second.id.parse::<u64>().unwrap_or(0) >= 5
		);
		assert_eq!(std::fs::read(directory.join("4.eval.log")).unwrap(), b"old");
		// Publication stages a complete sibling and hard-links it into place:
		// each public artifact holds its whole payload and nothing staged
		// survives a successful publish.
		let first_path = directory.join(format!("{}.eval.log", first.id));
		let second_path = directory.join(format!("{}.eval.log", second.id));
		assert_eq!(std::fs::read(&first_path).unwrap(), b"first");
		assert_eq!(std::fs::read(&second_path).unwrap(), b"second");
		for entry in std::fs::read_dir(&directory).expect("artifact dir") {
			let name = entry.expect("entry").file_name().to_string_lossy().into_owned();
			assert!(
				!name.contains(".stage-") && !name.contains(".tmp-"),
				"staging file left behind: {name}"
			);
		}
		let kind = sanitize_kind("../bad kind");
		assert_eq!(kind, "badkind");
		assert_eq!(scan_next_artifact_id(&directory).await.unwrap(), 7);

		let no_session = host(None);
		assert_eq!(no_session.write_artifact("eval", b"x").await.unwrap(), None);
		std::fs::remove_dir_all(&root).expect("cleanup");
	}

	#[tokio::test]
	async fn staged_artifact_publication_is_exclusive_and_never_public_early() {
		let root = temp_dir("omp-eval-artifact-publish");
		let directory = root.join("session");
		std::fs::create_dir_all(&directory).expect("session dir");

		// Staging names never look like artifacts, so an interrupted write can
		// never expose an empty `N.<kind>.log`.
		let (staged, staged_file) = create_artifact_stage(&directory, "eval")
			.await
			.expect("stage")
			.expect("exclusive name");
		drop(staged_file);
		std::fs::write(&staged, b"fresh").expect("stage content");
		assert!(
			!staged
				.file_name()
				.unwrap()
				.to_string_lossy()
				.ends_with(".log"),
			"staging path must not be a public artifact name"
		);

		// An id another writer already published is never replaced; the staged
		// copy is preserved for the caller's retry/cleanup.
		let final_path = directory.join("3.eval.log");
		std::fs::write(&final_path, b"seeded").expect("seed artifact");
		assert!(
			!publish_staged_artifact(&staged, &final_path)
				.await
				.expect("publish attempt")
		);
		assert_eq!(std::fs::read(&final_path).unwrap(), b"seeded");
		assert_eq!(std::fs::read(&staged).unwrap(), b"fresh");

		// A free id appears only as the complete staged payload.
		std::fs::remove_file(&final_path).expect("clear id");
		assert!(
			publish_staged_artifact(&staged, &final_path)
				.await
				.expect("publish")
		);
		assert_eq!(std::fs::read(&final_path).unwrap(), b"fresh");
		std::fs::remove_dir_all(&root).expect("cleanup");
	}

	#[test]
	fn background_delivery_requires_sink_and_owner_route() {
		let host = host(None);
		assert!(!host.enqueue_background_result("job", "done".to_string()));
	}

	struct EvalStub;

	#[async_trait]
	impl Tool for EvalStub {
		fn name(&self) -> &str {
			"eval"
		}

		fn description(&self) -> &str {
			"test eval stub"
		}

		fn schema(&self) -> Value {
			json!({"type": "object", "properties": {}})
		}

		async fn execute(&self, _args: &Value, _ctx: &AgentContext) -> Result<ToolResult, ToolError> {
			Err(ToolError::ExecutionFailed("stub".to_string()))
		}
	}

	struct PendingProvider;

	#[async_trait]
	impl ModelProvider for PendingProvider {
		async fn complete(
			&self,
			_messages: &[Message],
			_tools: &[Value],
			_tool_choice: &ToolChoice,
		) -> Result<Message, pi_agent::provider::ProviderError> {
			std::future::pending().await
		}

		async fn stream(
			&self,
			_messages: &[Message],
			_tools: &[Value],
			_tool_choice: &ToolChoice,
		) -> Result<pi_agent::provider::ChunkStream, pi_agent::provider::ProviderError> {
			Err(pi_agent::provider::ProviderError::Protocol(
				"test provider cannot stream".to_string(),
			))
		}
	}

	#[derive(Default)]
	struct DisposeRecorder {
		disposed: Mutex<Vec<String>>,
		bound:     Notify,
	}

	#[async_trait]
	impl EvalToolBinder for DisposeRecorder {
		fn bind(&self, registry: &mut ToolRegistry, _owner: Option<&str>) {
			registry.register(Arc::new(EvalStub));
		}

		fn bind_with_provider(
			&self,
			registry: &mut ToolRegistry,
			owner: Option<&str>,
			_provider: Option<Arc<dyn ModelProvider>>,
		) {
			self.bind(registry, owner);
			// Signalled once the child's eval host is mounted, so the test
			// cancels after bind and not before it.
			self.bound.notify_one();
		}

		async fn dispose_owner(&self, owner: &str) {
			self.disposed.lock().await.push(owner.to_string());
		}
	}

	/// Cancellation must unwind through `run_eval_agent`, not drop the future
	/// on a host-side `select!`: only the run path disposes the child's eval
	/// session/kernels.
	#[tokio::test]
	async fn cancelled_agent_still_disposes_the_child_eval_owner() {
		let supervisor = Supervisor::with_selection(
			ToolRegistry::new(),
			Arc::new(PendingProvider),
			None,
			DelegationLimits::default(),
			true,
			false,
		);
		let recorder = Arc::new(DisposeRecorder::default());
		supervisor.set_eval_binder(Some(recorder.clone() as Arc<dyn EvalToolBinder>));
		let mut eval_host = host(None);
		eval_host.supervisor = Some(Arc::downgrade(&supervisor));
		let eval_host = Arc::new(eval_host);

		let context = AgentContext::with_cancellation(
			PathBuf::from("."),
			CancellationToken::new(),
		);
		let token = context.cancellation_token.clone();
		let pending = {
			let eval_host = Arc::clone(&eval_host);
			let context = context.clone();
			tokio::spawn(async move { eval_host.agent(json!({"prompt": "work"}), &context).await })
		};
		recorder.bound.notified().await;
		token.cancel();
		let result = pending.await.expect("join");
		assert!(result.is_err(), "cancelled agent() must fail: {result:?}");
		let disposed = recorder.disposed.lock().await;
		assert_eq!(
			disposed.len(),
			1,
			"cancelled agent() must dispose exactly its eval owner"
		);
	}

	struct JsonProvider;

	#[async_trait]
	impl ModelProvider for JsonProvider {
		async fn complete(
			&self,
			_messages: &[Message],
			_tools: &[Value],
			_tool_choice: &ToolChoice,
		) -> Result<Message, pi_agent::provider::ProviderError> {
			Ok(Message::assistant("```json\n{\"ok\":true}\n```"))
		}

		async fn stream(
			&self,
			_messages: &[Message],
			_tools: &[Value],
			_tool_choice: &ToolChoice,
		) -> Result<pi_agent::provider::ChunkStream, pi_agent::provider::ProviderError> {
			Err(pi_agent::provider::ProviderError::Protocol(
				"test provider cannot stream".to_string(),
			))
		}
	}

	#[tokio::test]
	async fn completion_uses_provider_and_validates_schema_output() {
		let mut host = host(None);
		host.provider = Arc::new(JsonProvider);
		let ctx = AgentContext::new(PathBuf::from("."));
		let value = host
			.completion(
				json!({
					"prompt": "answer",
					"system": "be terse",
					"schema": {"type": "object", "properties": {"ok": {"type": "boolean"}}},
				}),
				&ctx,
			)
			.await
			.expect("completion");
		assert_eq!(value["text"], "{\"ok\":true}");

		let cancelled = AgentContext::with_cancellation(
			PathBuf::from("."),
			tokio_util::sync::CancellationToken::new(),
		);
		cancelled.cancellation_token.cancel();
		let error = host
			.completion(json!({"prompt": "answer"}), &cancelled)
			.await
			.expect_err("cancelled completion");
		assert!(error.contains("cancelled"));
	}

	#[test]
	fn bridge_registry_drops_reentrant_tools() {
		let mut registry = ToolRegistry::new();
		registry.register(Arc::new(pi_agent::tools::ThinkTool::new()));
		registry.register(Arc::new(pi_agent::tools::YieldTool::new()));
		let bridged = bridge_tool_registry(&registry);
		assert!(bridged.get("think").is_some());
		assert!(bridged.get("yield").is_some());
		assert!(bridged.get("eval").is_none());
	}

	/// Counts child eval binds; a blocked `agent()` must never reach one.
	struct BindRecorder {
		binds: Arc<std::sync::atomic::AtomicUsize>,
	}

	#[async_trait]
	impl EvalToolBinder for BindRecorder {
		fn bind(&self, _registry: &mut ToolRegistry, _owner: Option<&str>) {
			self.binds.fetch_add(1, Ordering::SeqCst);
		}
	}

	fn supervisored_host(goal: Arc<crate::goal::GoalRuntime>) -> (CliEvalHost, Arc<Supervisor>) {
		let supervisor = Supervisor::with_selection(
			ToolRegistry::new(),
			Arc::new(UnusedProvider),
			None,
			DelegationLimits::default(),
			true,
			false,
		);
		let mut eval_host = host(None);
		eval_host.supervisor = Some(Arc::downgrade(&supervisor));
		eval_host.goal = Some(goal);
		(eval_host, supervisor)
	}

	#[tokio::test]
	async fn hard_turn_budget_reports_and_blocks_eval_agent_before_spawn() {
		let goal = Arc::new(crate::goal::GoalRuntime::new());
		goal.begin_turn(Some("do work +1k!"), crate::goal::TokenCounters::default());
		goal.record_eval_subagent_output(1000);
		let (eval_host, supervisor) = supervisored_host(Arc::clone(&goal));
		let binds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
		let binder: Arc<dyn EvalToolBinder> = Arc::new(BindRecorder { binds: Arc::clone(&binds) });
		supervisor.set_eval_binder(Some(binder));
		let ctx = AgentContext::new(PathBuf::from("."));

		let budget = eval_host.budget(Value::Null, &ctx).await.expect("budget");
		assert_eq!(budget, json!({"total": 1000, "spent": 1000, "hard": true}));

		// The gate runs before argument validation and spawning: a bogus key
		// must not leak a different error, and no child eval owner may bind.
		let error = eval_host
			.agent(json!({"prompt": "work", "bogus": true}), &ctx)
			.await
			.expect_err("hard budget must block agent()");
		assert_eq!(
			error,
			"agent() blocked: turn token budget exhausted (1000/1000 output tokens). \
			 Raise or drop the +Nk! ceiling to continue."
		);
		assert_eq!(binds.load(Ordering::SeqCst), 0, "blocked agent() must not spawn a child");
	}

	#[tokio::test]
	async fn advisory_turn_budget_reports_soft_and_never_blocks_eval_agent() {
		let goal = Arc::new(crate::goal::GoalRuntime::new());
		goal.begin_turn(Some("do work +1k"), crate::goal::TokenCounters::default());
		goal.record_eval_subagent_output(1500);
		let (eval_host, _supervisor) = supervisored_host(goal);
		let ctx = AgentContext::new(PathBuf::from("."));

		let budget = eval_host.budget(Value::Null, &ctx).await.expect("budget");
		assert_eq!(budget, json!({"total": 1000, "spent": 1500, "hard": false}));

		// Spend over an advisory ceiling passes the gate and reaches
		// `run_eval_agent`'s own validation.
		let error = eval_host
			.agent(json!({"prompt": "work", "bogus": true}), &ctx)
			.await
			.expect_err("invalid argument");
		assert!(error.contains("unknown argument"), "advisory spend must not block: {error}");
	}
}
