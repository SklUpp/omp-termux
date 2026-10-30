//! The `eval` tool: one persistent single cell per call.
//!
//! State persists per (language, owner, session, cwd, interpreter). JS is only
//! started when the model explicitly asks for `language: "js"`; there is no
//! fallback language.

use std::{
	collections::{BTreeMap, HashMap},
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
	host::EvalHost,
	idle::{CellControl, IdleTimeout},
	output::{LIVE_TAIL_BYTES, OutputSink, dimension_note, format_display_outputs_for_text},
	runtime::{EvalRuntime, KernelMode, background_wait, session_cwd},
	types::{
		AsyncJob, AsyncState, CellStatus, EvalBackendId, EvalCellResult, EvalDisplayOutput, EvalError,
		EvalImage, EvalLanguage, EvalStatusEvent, EvalToolDetails,
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
	/// Scheme (without `://`) to on-disk root, e.g. `{"local": "/path"}`. Sent to
	/// subprocess preludes as JSON so `read("local://x")`/`write("local://x")`
	/// resolve under the same root.
	pub local_roots: BTreeMap<String, PathBuf>,
}

impl EvalSessionInfo {
	pub fn new(session_id: impl Into<String>) -> Self {
		Self {
			session_id: session_id.into(),
			owner_id: None,
			session_file: None,
			artifacts_dir: None,
			local_roots: BTreeMap::new(),
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
	pub fn with_local_roots(mut self, roots: impl IntoIterator<Item = (String, PathBuf)>) -> Self {
		self.local_roots = roots.into_iter().collect();
		self
	}

	#[must_use]
	pub fn with_local_root(mut self, scheme: impl Into<String>, path: impl Into<PathBuf>) -> Self {
		self.local_roots.insert(scheme.into(), path.into());
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

fn apply_status(events: &Arc<Mutex<Vec<EvalStatusEvent>>>, event: EvalStatusEvent) {
	// Watchdog control is executed bridge-side before the host await; these
	// synthetic ops never render or persist.
	if event.op == EVAL_TIMEOUT_PAUSE_OP || event.op == super::bridge::EVAL_TIMEOUT_RESUME_OP {
		return;
	}
	let mut events = events.lock().unwrap_or_else(|poison| poison.into_inner());
	EvalStatusEvent::upsert(&mut events, event);
}

/// Append streamed output to the bounded live preview shared with the
/// auto-background snapshot.
fn push_preview(preview: &Arc<Mutex<String>>, chunk: &str) {
	let mut preview = preview.lock().unwrap_or_else(|poison| poison.into_inner());
	if preview.len() >= LIVE_TAIL_BYTES {
		return;
	}
	let room = LIVE_TAIL_BYTES - preview.len();
	let mut end = chunk.len().min(room);
	while end > 0 && !chunk.is_char_boundary(end) {
		end -= 1;
	}
	preview.push_str(&chunk[..end]);
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
		let roots: BTreeMap<&str, String> = session
			.local_roots
			.iter()
			.map(|(scheme, root)| (scheme.as_str(), root.display().to_string()))
			.collect();
		if let Ok(encoded) = serde_json::to_string(&roots) {
			env.insert("PI_EVAL_LOCAL_ROOTS".to_string(), encoded);
		}
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
	/// Bounded live tail shared with the auto-background start snapshot.
	preview: Arc<Mutex<String>>,
}

impl CellJob {
	/// Cancellation that lands before the kernel accepts code is still a
	/// cancelled cell result (upstream `createCancelledPythonResult`), not a
	/// tool error: foreground and background deliveries agree on the
	/// `Command aborted` annotation.
	fn aborted_result(&self, started: Instant) -> ToolResult {
		let output = "Command aborted".to_string();
		let cell = EvalCellResult {
			index: 0,
			title: self.title.clone(),
			code: self.code.clone(),
			language: Some(EvalBackendId::of(self.language)),
			output: output.clone(),
			status: CellStatus::Error,
			duration_ms: Some(started.elapsed().as_millis() as u64),
			exit_code: Some(1),
			status_events: None,
			has_markdown: None,
		};
		let details = EvalToolDetails {
			cells: Some(vec![cell]),
			is_error: Some(true),
			language: Some(EvalBackendId::of(self.language)),
			languages: Some(vec![EvalBackendId::of(self.language)]),
			..EvalToolDetails::default()
		};
		ToolResult {
			tool_call_id: String::new(),
			content: output,
			is_error: true,
			terminate: false,
			details: Some(serde_json::to_value(details).unwrap_or(Value::Null)),
		}
	}

	async fn run(self) -> Result<ToolResult, ToolError> {
		let started = Instant::now();
		if self.ctx.cancellation_token.is_cancelled() {
			return Ok(self.aborted_result(started));
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
		// Discovery is bounded by the cell budget and the caller's real token.
		// Julia readiness draws the same cell budget as its execution; every other
		// backend keeps probe/startup outside the cell watchdog.
		let cell_timeout_ms = idle_secs.filter(|seconds| *seconds > 0).map(|seconds| seconds * 1000);
		// One anchor for the whole cell: a dead-kernel retry gets the remainder.
		let cell_deadline = cell_timeout_ms.map(|budget| started + Duration::from_millis(budget));
		let probe_timeout_ms = settings.probe_timeout_ms();
		let probe_bound_ms = match (cell_timeout_ms, probe_timeout_ms) {
			(Some(cell), 0) => Some(cell),
			(Some(cell), probe) => Some(cell.min(probe)),
			(None, 0) => None,
			(None, probe) => Some(probe),
		};
		let acquired = self
			.runtime
			.kernel_for_session(
				self.language,
				self.session.owner_id.as_deref(),
				&self.session.session_id,
				&cwd,
				&self.ctx.env,
				self.reset,
				probe_bound_ms,
				cell_deadline,
				self.ctx.cancellation_token.clone(),
			)
			.await;
		let acquired = match acquired {
			Ok(acquired) => acquired,
			Err(EvalError::Aborted(_)) => return Ok(self.aborted_result(started)),
			Err(error) => return Err(tool_error(error)),
		};
		let mut kernel = acquired;
		let per_call = matches!(settings.kernel_mode(), KernelMode::PerCall);

		// Julia readiness is the interpreter boot itself and draws on the cell
		// budget, so its execution must fit in the remainder. Probing backends keep
		// the existing contract: probe and startup are not charged to the cell.
		let idle = match (self.language, cell_deadline) {
			(EvalLanguage::Jl, Some(deadline)) => {
				let remaining = deadline.saturating_duration_since(Instant::now());
				Some(IdleTimeout::new(remaining.max(Duration::from_millis(1))))
			},
			_ => idle_secs
				.filter(|seconds| *seconds > 0)
				.map(|seconds| IdleTimeout::new(Duration::from_secs(seconds))),
		};
		let control = CellControl::new(self.ctx.cancellation_token.clone(), idle);
		let cancellation = control.kernel_token();
		let run_id = format!("eval-{}-{}", self.language.token(), &util::random_token()[..8]);
		let managed = managed_env(
			&self.session,
			&self.runtime.bridge().endpoint().url,
			&self.runtime.bridge().endpoint().token,
			self.runtime.bridge_session_id(),
		);
		let status_events = Arc::new(Mutex::new(Vec::new()));
		let status_rx = self.runtime.bridge().register(
			self.runtime.bridge_session_id(),
			&run_id,
			Arc::clone(&self.host),
			self.ctx.clone(),
			format!("{} prelude", self.language.runtime_name()),
			self.ctx.cancellation_token.clone(),
			Some(control.clone()),
		);
		let mut drain = spawn_status_drain(status_rx, Arc::clone(&status_events));

		let mut sink = OutputSink::new(settings.spill_bytes(), settings.keep_artifacts());
		let mut display_outputs: Vec<EvalDisplayOutput> = Vec::new();
		let mut json_outputs: Vec<Value> = Vec::new();
		let mut images: Vec<EvalImage> = Vec::new();
		let mut image_notes: Vec<String> = Vec::new();
		let mut has_markdown = false;

		let events_for_display = Arc::clone(&status_events);
		let cancelled_by_us =
			|| self.ctx.cancellation_token.is_cancelled() || control.timed_out();
		// One bounded recovery attempt, and only when the kernel died on its own:
		// never replay after a turn/timeout cancellation (the first run may have
		// produced side effects before its delivery became uncertain).
		let mut retried = false;
		let mut original_cancelled: Option<super::types::KernelExecuteResult> = None;
		let mut result = loop {
			let attempt_result = {
				let on_chunk = |chunk: &str| {
					sink.push(chunk);
					push_preview(&self.preview, chunk);
				};
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
						apply_status(&events_for_display, event);
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
			let retryable = !retried
				&& !kernel.is_alive()
				&& !cancelled_by_us()
				&& match &attempt_result {
					Ok(result) => result.cancelled,
					Err(_) => true,
				};
			if !retryable {
				break attempt_result;
			}
			retried = true;
			original_cancelled = attempt_result.as_ref().ok().filter(|result| result.cancelled).cloned();
			match self
				.runtime
				.kernel_for_session(
					self.language,
					self.session.owner_id.as_deref(),
					&self.session.session_id,
					&cwd,
					&self.ctx.env,
					true,
					probe_bound_ms,
					cell_deadline,
					self.ctx.cancellation_token.clone(),
				)
				.await
			{
				Ok(replacement) => {
					kernel = replacement;
					if self.ctx.cancellation_token.is_cancelled() {
						break attempt_result;
					}
				},
				Err(error) => {
					if cancelled_by_us() {
						break attempt_result;
					}
					break Err(error);
				},
			}
		};

		let timed_out = control.timed_out();
		if timed_out
			&& let Ok(retry) = &result
			&& retry.cancelled
			&& let Some(original) = original_cancelled
		{
			result = Ok(original);
		}
		control.dispose();
		self.runtime.bridge().unregister(self.runtime.bridge_session_id(), &run_id);
		if tokio::time::timeout(Duration::from_millis(250), &mut drain).await.is_err() {
			drain.abort();
		}
		if per_call {
			kernel.shutdown().await;
		}

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

		let mut artifact = None;
		let mut artifact_error = None;
		if sink.truncated() && let Some(bytes) = sink.artifact() {
			match self.host.write_artifact("eval", bytes).await {
				Ok(Some(published)) => artifact = Some((published.id, published.path)),
				// An absent session artifacts directory legitimately disables
				// publication; the elided bytes are simply unrecoverable.
				Ok(None) => {},
				// A real publish refusal (e.g. Android `link(2)` EACCES) means
				// the elided bytes have no artifact at all. Report it instead of
				// presenting the truncated output as complete.
				Err(error) => {
					is_error = true;
					artifact_error = Some(format!("artifact unavailable: {error}"));
				},
			}
		}
		// The provider only sees `content`; a details-only notice is invisible to
		// the model, so surface the denial before the preserved cell text.
		if let Some(error) = &artifact_error {
			cell_output = format!("{error}\n{cell_output}");
		}
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
			language: Some(EvalBackendId::of(self.language)),
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
			language: Some(EvalBackendId::of(self.language)),
			languages: Some(vec![EvalBackendId::of(self.language)]),
			notice: artifact_error,
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
) -> tokio::task::JoinHandle<()> {
	tokio::spawn(async move {
		while let Some(event) = receiver.recv().await {
			apply_status(&events, event);
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
		let preview = Arc::new(Mutex::new(String::new()));
		let job = CellJob {
			runtime: Arc::clone(&self.runtime),
			host: Arc::clone(&self.host),
			session: self.session.clone(),
			language: params.language,
			code: params.code.clone(),
			title: params.title.clone(),
			timeout_secs: params.timeout_secs,
			reset: params.reset,
			ctx: ctx.clone(),
			preview: Arc::clone(&preview),
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

		if let Some(wait_ms) = wait_ms {
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
					let preview_text = preview
						.lock()
						.unwrap_or_else(|poison| poison.into_inner())
						.clone();
					let backend = EvalBackendId::of(params.language);
					let details = EvalToolDetails {
						cells: Some(vec![EvalCellResult {
							index: 0,
							title: params.title.clone(),
							code: params.code.clone(),
							language: Some(backend),
							output: preview_text.clone(),
							status: CellStatus::Running,
							duration_ms: None,
							exit_code: None,
							status_events: None,
							has_markdown: None,
						}]),
						language: Some(backend),
						languages: Some(vec![backend]),
						notice: None,
						r#async: Some(AsyncJob {
							state: AsyncState::Running,
							job_id: job_id.clone(),
							kind: "eval".to_string(),
						}),
						..EvalToolDetails::default()
					};
					let mut lines = Vec::new();
					let trimmed = preview_text.trim_end();
					if !trimmed.is_empty() {
						lines.push(trimmed.to_string());
						lines.push(String::new());
					}
					lines.push(format!(
						"Backgrounded as job {job_id}; result will be delivered automatically."
					));
					return Ok(ToolResult {
						tool_call_id: String::new(),
						content: lines.join("\n"),
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
				// Keep a pre-existing (artifact denial) notice visible.
				let notice = match object.get("notice").and_then(Value::as_str) {
					Some(existing) => format!("{existing}\n{notice}"),
					None => notice,
				};
				object.insert("notice".to_string(), Value::String(notice));
			}
		}
		Ok(result)
	}
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
	fn local_roots_serialize_as_scheme_map() {
		let session = EvalSessionInfo::new("s").with_local_roots([
			("local".to_string(), PathBuf::from("/tmp/a")),
			("work".to_string(), PathBuf::from("/tmp/b")),
		]);
		let env = managed_env(&session, "http://127.0.0.1:1", "t", "bs");
		let encoded = env.get("PI_EVAL_LOCAL_ROOTS").expect("local roots env");
		let value: Value = serde_json::from_str(encoded).expect("json map");
		assert_eq!(value["local"], "/tmp/a");
		assert_eq!(value["work"], "/tmp/b");
	}

	#[test]
	fn managed_env_omits_absent_optional_keys() {
		let env = managed_env(&EvalSessionInfo::new("s"), "http://127.0.0.1:1", "t", "bs");
		assert_eq!(env.get("PI_TOOL_BRIDGE_SESSION").map(String::as_str), Some("bs"));
		assert!(!env.contains_key("PI_SESSION_FILE"));
		assert!(!env.contains_key("PI_ARTIFACTS_DIR"));
	}
}
