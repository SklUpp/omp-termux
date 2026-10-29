//! The `eval` tool: one persistent single cell per call.
//!
//! State persists per (language, owner, session, cwd, interpreter). JS is only
//! started when the model explicitly asks for `language: "js"`; there is no
//! fallback language.

use std::{
	collections::HashMap,
	path::PathBuf,
	sync::{Arc, Mutex},
	time::{Duration, Instant},
};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::{
	state::{AgentContext, ToolResult},
	tools::{Tool, ToolError},
};

use super::{
	bridge::EVAL_TIMEOUT_PAUSE_OP,
	host::{ArtifactRef, EvalHost},
	idle::{IdleTimeout, combine_with},
	kernel::Kernel,
	output::{OutputSink, dimension_note, format_display_outputs_for_text},
	runtime::{EvalRuntime, background_wait, session_cwd},
	types::{
		AsyncJob, AsyncState, CellStatus, EvalCellResult, EvalDisplayOutput, EvalError, EvalImage,
		EvalLanguage, EvalStatusEvent, EvalToolDetails,
	},
	util,
};

/// Parent-supplied stable session identity. `session_id` is required; the rest
/// activate managed environment keys only when present.
#[derive(Debug, Clone)]
pub struct EvalSessionInfo {
	pub session_id: String,
	pub owner_id: Option<String>,
	pub session_file: Option<PathBuf>,
	pub artifacts_dir: Option<PathBuf>,
	pub local_roots: Vec<PathBuf>,
}

impl EvalSessionInfo {
	pub fn new(session_id: impl Into<String>) -> Self {
		Self {
			session_id: session_id.into(),
			owner_id: None,
			session_file: None,
			artifacts_dir: None,
			local_roots: Vec::new(),
		}
	}

	#[must_use]
	pub fn with_owner(mut self, owner: Option<String>) -> Self {
		self.owner_id = owner;
		self
	}

	#[must_use]
	pub fn with_session_file(mut self, path: Option<PathBuf>) -> Self {
		self.session_file = path;
		self
	}

	#[must_use]
	pub fn with_artifacts_dir(mut self, path: Option<PathBuf>) -> Self {
		self.artifacts_dir = path;
		self
	}

	#[must_use]
	pub fn with_local_roots(mut self, roots: Vec<PathBuf>) -> Self {
		self.local_roots = roots;
		self
	}
}

/// Native eval tool.
pub struct EvalTool {
	runtime: Arc<EvalRuntime>,
	host: Arc<dyn EvalHost>,
	session: EvalSessionInfo,
	fallback_results: Arc<Mutex<Vec<String>>>,
	description: String,
}

impl EvalTool {
	/// Required dependencies: no default/placeholder host is created.
	pub fn new(runtime: Arc<EvalRuntime>, host: Arc<dyn EvalHost>, session: EvalSessionInfo) -> Self {
		let enabled = runtime.settings().allowance().languages();
		let description = if enabled.is_empty() {
			"Eval is disabled for this session.".to_string()
		} else {
			format!(
				"Execute one persistent {} cell. State persists across calls per language; JS starts only when explicitly requested.",
				enabled
					.iter()
					.map(|language| language.display_name())
					.collect::<Vec<_>>()
					.join(" / ")
			)
		};
		Self {
			runtime,
			host,
			session,
			fallback_results: Arc::new(Mutex::new(Vec::new())),
			description,
		}
	}
}

#[derive(Debug, Clone)]
struct EvalParams {
	language: EvalLanguage,
	code: String,
	title: Option<String>,
	timeout_secs: Option<u64>,
	reset: bool,
}

fn parse_params(args: &Value) -> Result<EvalParams, ToolError> {
	let language_token = args
		.get("language")
		.and_then(Value::as_str)
		.ok_or_else(|| ToolError::InvalidArguments("missing `language` (expected py/js/rb/jl)".to_string()))?;
	let language = EvalLanguage::parse(language_token).ok_or_else(|| {
		ToolError::InvalidArguments(format!(
			"unknown eval language `{language_token}` (expected py/js/rb/jl)"
		))
	})?;
	let code = args
		.get("code")
		.and_then(Value::as_str)
		.ok_or_else(|| ToolError::InvalidArguments("missing string `code`".to_string()))?
		.to_string();
	let title = args.get("title").and_then(Value::as_str).map(str::to_string);
	let timeout_secs = match args.get("timeout") {
		None | Some(Value::Null) => None,
		Some(value) => Some(value.as_u64().ok_or_else(|| {
			ToolError::InvalidArguments("`timeout` must be a non-negative integer of seconds".to_string())
		})?),
	};
	let reset = args.get("reset").and_then(Value::as_bool).unwrap_or(false);
	Ok(EvalParams { language, code, title, timeout_secs, reset })
}

fn tool_error(error: EvalError) -> ToolError {
	match error {
		EvalError::Unavailable(message) | EvalError::Kernel(message) | EvalError::Bridge(message) => {
			ToolError::ExecutionFailed(message)
		},
		EvalError::InvalidArguments(message) | EvalError::UnknownLanguage(message) => {
			ToolError::InvalidArguments(message)
		},
		EvalError::Aborted(message) => ToolError::Aborted(message),
		EvalError::Io(error) => ToolError::ExecutionFailed(error.to_string()),
	}
}

fn apply_status(
	events: &Arc<Mutex<Vec<EvalStatusEvent>>>,
	idle: Option<&IdleTimeout>,
	event: EvalStatusEvent,
) {
	if event.op == EVAL_TIMEOUT_PAUSE_OP {
		if let Some(idle) = idle {
			idle.pause();
		}
		return;
	}
	if event.op == super::bridge::EVAL_TIMEOUT_RESUME_OP {
		if let Some(idle) = idle {
			idle.resume();
		}
		return;
	}
	let mut events = events.lock().unwrap_or_else(|poison| poison.into_inner());
	EvalStatusEvent::upsert(&mut events, event);
}

fn managed_env(
	session: &EvalSessionInfo,
	bridge_url: &str,
	bridge_token: &str,
	bridge_session: &str,
) -> HashMap<String, String> {
	let mut env = HashMap::new();
	env.insert("PI_TOOL_BRIDGE_URL".to_string(), bridge_url.to_string());
	env.insert("PI_TOOL_BRIDGE_TOKEN".to_string(), bridge_token.to_string());
	env.insert("PI_TOOL_BRIDGE_SESSION".to_string(), bridge_session.to_string());
	if let Some(path) = &session.session_file {
		env.insert("PI_SESSION_FILE".to_string(), path.display().to_string());
	}
	if let Some(path) = &session.artifacts_dir {
		env.insert("PI_ARTIFACTS_DIR".to_string(), path.display().to_string());
	}
	if !session.local_roots.is_empty() {
		let joined = session
			.local_roots
			.iter()
			.map(|root| root.display().to_string())
			.collect::<Vec<_>>()
			.join(local_roots_delimiter());
		env.insert("PI_EVAL_LOCAL_ROOTS".to_string(), joined);
	}
	env
}

struct CellJob {
	runtime: Arc<EvalRuntime>,
	host: Arc<dyn EvalHost>,
	session: EvalSessionInfo,
	language: EvalLanguage,
	code: String,
	title: Option<String>,
	timeout_secs: Option<u64>,
	reset: bool,
	ctx: AgentContext,
}

impl CellJob {
	async fn run(self) -> Result<ToolResult, ToolError> {
		let started = Instant::now();
		if self.ctx.cancellation_token.is_cancelled() {
			return Err(ToolError::Aborted("eval call cancelled".to_string()));
		}
		let settings = self.runtime.settings();
		if !settings.allowance().enabled(self.language) {
			return Err(ToolError::ExecutionFailed(
				super::runtime::EvalSettings::disabled_message(self.language),
			));
		}
		let cwd = session_cwd(&self.ctx.cwd, &self.ctx.env);
		let idle_secs = Some(match self.timeout_secs {
			Some(0) => 0,
			Some(requested) => settings.resolve_timeout_secs(Some(requested)),
			None => settings.resolve_timeout_secs(None),
		});
		let idle = idle_secs
			.filter(|seconds| *seconds > 0)
			.map(|seconds| IdleTimeout::new(Duration::from_secs(seconds)));
		let combined = match &idle {
			Some(idle) => {
				let (token, handle) = combine_with(self.ctx.cancellation_token.clone(), idle.token());
				Some((token, handle))
			},
			None => None,
		};
		let cancellation = combined
			.as_ref()
			.map_or_else(|| self.ctx.cancellation_token.clone(), |(token, _)| token.clone());
		let run_id = format!("eval-{}-{}", self.language.token(), &util::random_token()[..8]);
		let managed = managed_env(
			&self.session,
			&self.runtime.bridge().endpoint().url,
			&self.runtime.bridge().endpoint().token,
			self.runtime.bridge_session_id(),
		);
		let status_rx = self.runtime.bridge().register(
			self.runtime.bridge_session_id(),
			&run_id,
			Arc::clone(&self.host),
			self.ctx.clone(),
			format!("{} prelude", self.language.runtime_name()),
			cancellation.clone(),
		);
		let status_events = Arc::new(Mutex::new(Vec::new()));
		let drain = spawn_status_drain(status_rx, Arc::clone(&status_events), idle.clone());

		let acquired = self
			.runtime
			.kernel_for_session(
				self.language,
				self.session.owner_id.as_deref(),
				&self.session.session_id,
				&cwd,
				&self.ctx.env,
				self.reset,
				cancellation.clone(),
			)
			.await;
		let kernel: Arc<Kernel> = match acquired {
			Ok(kernel) => kernel,
			Err(error) => {
				self.runtime.bridge().unregister(self.runtime.bridge_session_id(), &run_id);
				drain.abort();
				if let Some((_, handle)) = combined {
					handle.abort();
				}
				idle.as_ref().inspect(|idle| idle.dispose());
				return Err(tool_error(error));
			},
		};

		let mut sink = OutputSink::new(settings.spill_bytes(), settings.keep_artifacts());
		let mut display_outputs: Vec<EvalDisplayOutput> = Vec::new();
		let mut json_outputs: Vec<Value> = Vec::new();
		let mut images: Vec<EvalImage> = Vec::new();
		let mut image_notes: Vec<String> = Vec::new();
		let mut has_markdown = false;

		let events_for_display = Arc::clone(&status_events);
		let idle_for_display = idle.clone();
		let result = {
			let on_chunk = |chunk: &str| sink.push(chunk);
			let on_display = |output: EvalDisplayOutput| match output {
				EvalDisplayOutput::Json(value) => {
					json_outputs.push(value.clone());
					display_outputs.push(EvalDisplayOutput::Json(value));
				},
				EvalDisplayOutput::Image { data, mime_type } => {
					let index = images.len() + 1;
					if let Some(note) = dimension_note(index, &mime_type, &data) {
						image_notes.push(note);
					}
					images.push(EvalImage::new(data.clone(), mime_type.clone()));
					display_outputs.push(EvalDisplayOutput::Image { data, mime_type });
				},
				EvalDisplayOutput::Markdown { text } => {
					has_markdown = true;
					display_outputs.push(EvalDisplayOutput::Markdown { text });
				},
				EvalDisplayOutput::Status(event) => {
					apply_status(&events_for_display, idle_for_display.as_ref(), event);
				},
			};
			kernel
				.execute(
					&self.code,
					super::types::KernelExecuteOptions {
						run_id: run_id.clone(),
						cwd: &cwd,
						env: &managed,
						silent: false,
						store_history: true,
						cancellation: cancellation.clone(),
						on_chunk: Some(Box::new(on_chunk)),
						on_display: Some(Box::new(on_display)),
					},
				)
				.await
		};

		self.runtime.bridge().unregister(self.runtime.bridge_session_id(), &run_id);
		drain.abort();
		if let Some((_, handle)) = combined {
			handle.abort();
		}
		let timed_out = idle.as_ref().is_some_and(IdleTimeout::timed_out);
		idle.as_ref().inspect(|idle| idle.dispose());

		let mut is_error = true;
		let stdout = match &result {
			Ok(result) => {
				is_error = result.status == super::types::KernelStatus::Error
					|| result.cancelled
					|| result.kernel_killed
					|| timed_out;
				sink.finish_text()
			},
			Err(_) => sink.finish_text(),
		};

		if timed_out || result.as_ref().is_ok_and(|result| result.cancelled) {
			is_error = true;
		}

		let display_text = format_display_outputs_for_text(&display_outputs);
		let image_text = image_notes.join("\n");
		let visible = match (display_text.is_empty(), image_text.is_empty()) {
			(false, false) => format!("{display_text}\n\n{image_text}"),
			(false, true) => display_text,
			(true, false) => image_text,
			(true, true) => String::new(),
		};
		let trimmed = stdout.trim();
		let mut cell_output = match (trimmed.is_empty(), visible.is_empty()) {
			(false, false) => format!("{trimmed}\n\n{visible}"),
			(false, true) => trimmed.to_string(),
			(true, false) => visible,
			(true, true) => {
				if images.is_empty() {
					match &result {
						Ok(result) if result.kernel_killed => "Kernel exited unexpectedly".to_string(),
						Ok(result) if result.cancelled => "Command aborted".to_string(),
						Ok(result) if result.error.is_some() => result
							.error
							.as_ref()
							.map_or_else(|| "Eval failed".to_string(), |error| {
								if error.value.is_empty() {
									error.name.clone()
								} else {
									error.value.clone()
								}
							}),
						Err(error) => error.to_string(),
						_ => "(no output)".to_string(),
					}
				} else {
					format!(
						"(displayed {} image{}; no text output)",
						images.len(),
						if images.len() == 1 { "" } else { "s" }
					)
				}
			},
		};
		if timed_out {
			let note = format!("Command aborted (timeout after {}s)", idle_secs.unwrap_or(0));
			cell_output = if cell_output == "Command aborted" || cell_output == "(no output)" {
				note
			} else {
				format!("{note}\n{cell_output}")
			};
		}

		let artifact = if sink.truncated() {
			match sink.artifact() {
				Some(bytes) => self
					.host
					.write_artifact("eval", bytes)
					.await
					.ok()
					.flatten()
					.map(|artifact: ArtifactRef| (artifact.id, artifact.path)),
				None => None,
			}
		} else {
			None
		};
		let meta = sink.meta(artifact);
		let events = status_events
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.clone();
		let cell_status = if is_error { CellStatus::Error } else { CellStatus::Complete };
		let cell = EvalCellResult {
			index: 0,
			title: self.title.clone(),
			code: self.code.clone(),
			language: Some(self.language),
			output: cell_output.clone(),
			status: cell_status,
			duration_ms: Some(started.elapsed().as_millis() as u64),
			exit_code: Some(if is_error { 1 } else { 0 }),
			status_events: (!events.is_empty()).then(|| events.clone()),
			has_markdown: has_markdown.then_some(true),
		};
		let details = EvalToolDetails {
			cells: Some(vec![cell]),
			json_outputs: (!json_outputs.is_empty()).then_some(json_outputs),
			images: (!images.is_empty()).then_some(images),
			status_events: (!events.is_empty()).then_some(events),
			is_error: is_error.then_some(true),
			meta: Some(meta),
			language: Some(self.language),
			languages: Some(vec![self.language]),
			notice: None,
			r#async: None,
		};
		Ok(ToolResult {
			tool_call_id: String::new(),
			content: cell_output,
			is_error,
			terminate: false,
			details: Some(serde_json::to_value(details).unwrap_or(Value::Null)),
		})
	}
}

fn spawn_status_drain(
	mut receiver: mpsc::UnboundedReceiver<EvalStatusEvent>,
	events: Arc<Mutex<Vec<EvalStatusEvent>>>,
	idle: Option<IdleTimeout>,
) -> tokio::task::JoinHandle<()> {
	tokio::spawn(async move {
		while let Some(event) = receiver.recv().await {
			apply_status(&events, idle.as_ref(), event);
		}
	})
}

#[async_trait]
impl Tool for EvalTool {
	fn name(&self) -> &str {
		"eval"
	}

	fn description(&self) -> &str {
		&self.description
	}

	fn schema(&self) -> Value {
		let enabled = self.runtime.settings().allowance().languages();
		let advertised: Vec<&str> = if enabled.is_empty() {
			EvalLanguage::ALL.iter().map(|language| language.token()).collect()
		} else {
			enabled.iter().map(|language| language.token()).collect()
		};
		serde_json::json!({
			"type": "object",
			"properties": {
				"language": {
					"type": "string",
					"enum": advertised,
					"description": format!(
						"Runtime: {}. JS starts only when explicitly requested.",
						enabled
							.iter()
							.map(|language| language.display_name())
							.collect::<Vec<_>>()
							.join(" / ")
					)
				},
				"code": {
					"type": "string",
					"description": "Cell source. Python/Ruby/Julia/JS top-level await supported; globals persist."
				},
				"title": {
					"type": "string",
					"description": "Short label shown in the transcript (e.g. \"load config\")."
				},
				"timeout": {
					"type": "integer",
					"minimum": 0,
					"description": "Cell work budget in seconds; host bridge calls pause it. 0 disables it."
				},
				"reset": {
					"type": "boolean",
					"description": "Wipe this language's kernel before running. Other languages are untouched."
				}
			},
			"required": ["language", "code"],
			"additionalProperties": false
		})
	}

	fn is_exclusive(&self) -> bool {
		true
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let params = parse_params(args)?;
		let job = CellJob {
			runtime: Arc::clone(&self.runtime),
			host: Arc::clone(&self.host),
			session: self.session.clone(),
			language: params.language,
			code: params.code,
			title: params.title,
			timeout_secs: params.timeout_secs,
			reset: params.reset,
			ctx: ctx.clone(),
		};
		let settings = self.runtime.settings();
		let cell_timeout_ms = match params.timeout_secs {
			Some(0) => None,
			Some(seconds) => Some(settings.resolve_timeout_secs(Some(seconds)) * 1000),
			None => Some(settings.resolve_timeout_secs(None) * 1000),
		};
		let wait_ms = settings
			.auto_background()
			.map(|config| background_wait(config.threshold_ms, cell_timeout_ms));

		if let Some(wait_ms) = wait_ms
			&& cell_timeout_ms.is_none_or(|timeout| wait_ms < timeout)
		{
			let job_id = format!("eval-bg-{}", &util::random_token()[..10]);
			let mut handle = tokio::spawn(job.run());
			match tokio::time::timeout(Duration::from_millis(wait_ms), &mut handle).await {
				Ok(joined) => {
					return joined.map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
				},
				Err(_) => {
					let host = Arc::clone(&self.host);
					let fallback = Arc::clone(&self.fallback_results);
					let delivery_id = job_id.clone();
					tokio::spawn(async move {
						let text = match handle.await {
							Ok(Ok(result)) => {
								if result.is_error {
									format!("[error] {}", result.content)
								} else {
									result.content
								}
							},
							Ok(Err(error)) => format!("Eval background job failed: {error}"),
							Err(error) => format!("Eval background job failed: {error}"),
						};
						if !host.enqueue_background_result(&delivery_id, text.clone()) {
							fallback
								.lock()
								.unwrap_or_else(|poison| poison.into_inner())
								.push(format!("Eval background job `{delivery_id}` finished:\n{text}"));
						}
					});
					let notice = format!(
						"Eval cell exceeded the {wait_ms}ms foreground window and continues in the background (job `{job_id}`)."
					);
					let details = EvalToolDetails {
						language: Some(params.language),
						languages: Some(vec![params.language]),
						notice: Some(notice.clone()),
						r#async: Some(AsyncJob {
							state: AsyncState::Running,
							job_id: job_id.clone(),
							kind: "eval-cell".to_string(),
						}),
						..EvalToolDetails::default()
					};
					return Ok(ToolResult {
						tool_call_id: String::new(),
						content: notice,
						is_error: false,
						terminate: false,
						details: Some(serde_json::to_value(details).unwrap_or(Value::Null)),
					});
				},
			}
		}

		let mut result = job.run().await?;
		let pending = {
			let mut fallback = self.fallback_results.lock().unwrap_or_else(|poison| poison.into_inner());
			std::mem::take(&mut *fallback)
		};
		if !pending.is_empty() {
			let notice = pending.join("\n\n");
			result.content = format!("{notice}\n\n{}", result.content);
			if let Some(details) = result.details.as_mut()
				&& let Some(object) = details.as_object_mut()
			{
				object.insert("notice".to_string(), Value::String(notice));
			}
		}
		Ok(result)
	}
}

/// Path delimiter used for `PI_EVAL_LOCAL_ROOTS`.
pub fn local_roots_delimiter() -> &'static str {
	":"
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn missing_language_is_rejected_instead_of_defaulting_to_js() {
		let error = parse_params(&json!({"code": "1+1"})).expect_err("language required");
		assert!(error.to_string().contains("language"));
	}

	#[test]
	fn unknown_language_lists_tokens() {
		let error = parse_params(&json!({"language": "ts", "code": "1"})).expect_err("unknown");
		assert!(error.to_string().contains("py/js/rb/jl"));
	}

	#[test]
	fn timeout_accepts_zero_and_rejects_negative() {
		assert_eq!(
			parse_params(&json!({"language": "py", "code": "", "timeout": 0}))
				.expect("zero")
				.timeout_secs,
			Some(0)
		);
		assert!(parse_params(&json!({"language": "py", "code": "", "timeout": -1})).is_err());
	}

	#[test]
	fn managed_env_omits_absent_optional_keys() {
		let env = managed_env(&EvalSessionInfo::new("s"), "http://127.0.0.1:1", "t", "bs");
		assert_eq!(env.get("PI_TOOL_BRIDGE_SESSION").map(String::as_str), Some("bs"));
		assert!(!env.contains_key("PI_SESSION_FILE"));
		assert!(!env.contains_key("PI_ARTIFACTS_DIR"));
	}
}
