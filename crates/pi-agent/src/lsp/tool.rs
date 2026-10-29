//! `lsp` tool adapter: schema, timeout/cancellation, action dispatch, and
//! model-facing text output.
//!
//! Port of `packages/coding-agent/src/lsp/tool.ts` plus a `format` action backed
//! by the same formatter used by write-through (`formatContent` upstream).

use std::{
	path::Path,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	time::Duration,
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
	state::{AgentContext, ToolResult},
	tools::{Tool, ToolError},
};

use super::{
	client::{
		self, clear_initialization_failure, get_active_clients, get_config,
		get_or_create_client_for, ensure_file_open, refresh_file, reload_server,
		shutdown_stale_clients, wait_for_diagnostics, wait_for_project_loaded, LspClient, LspError,
		LspSession, WaitDiagnosticsOptions,
	},
	config::{
		get_lsp_server_for_file, get_lsp_servers, get_lsp_servers_for_file, get_servers_for_file,
		is_method_not_found_error,
	},
	diagnostics::{
		self, BATCH_DIAGNOSTICS_WAIT_TIMEOUT_MS, SINGLE_DIAGNOSTICS_WAIT_TIMEOUT_MS,
	},
	edits::{apply_edits_then_rename, flatten_workspace_text_edits, ranges_overlap, sort_and_validate_text_edits, RenameReferenceEdit},
	linter,
	lspmux,
	types::{
		CodeAction, CodeActionContext, CodeActionOrCommand, Diagnostic, DocumentSymbol, LspParams,
		Position, Range, ServerConfig, SymbolInformation, TextEdit, WorkspaceEdit,
	},
	utils::{
		dedupe_workspace_symbols, file_to_uri, filter_workspace_symbols, format_code_action,
		format_diagnostic, format_diagnostics_summary, format_document_symbol,
		format_grouped_diagnostic_messages, format_location, format_path_relative_to_cwd,
		format_symbol_information, format_workspace_edit, normalize_location_result,
		resolve_diagnostic_targets, resolve_symbol_column, resolve_to_cwd, sort_diagnostics,
		uri_to_file,
	},
	workspace_diagnostics::run_workspace_diagnostics,
};

const DEFAULT_TIMEOUT_SECS: f64 = 20.0;
const MIN_TIMEOUT_SECS: f64 = 5.0;
const MAX_TIMEOUT_SECS: f64 = 300.0;
const MAX_GLOB_DIAGNOSTIC_TARGETS: usize = 20;
const WORKSPACE_SYMBOL_LIMIT: usize = 200;
const REFERENCES_RETRY_COUNT: usize = 2;
const REFERENCES_RETRY_DELAY_MS: u64 = 250;
const REFERENCE_CONTEXT_LIMIT: usize = 50;
const MAX_RENAME_PAIRS: usize = 1_000;
const PROJECT_INDEXED_ACTIONS: &[&str] = &[
	"definition",
	"type_definition",
	"implementation",
	"references",
	"rename",
	"hover",
];

const SCHEMA_DESCRIPTION: &str = "Query language servers for diagnostics, definitions, references, hover, symbols, rename, and other LSP operations.";

// =============================================================================
// Execution guard
// =============================================================================

struct RunGuard {
	cancel: CancellationToken,
	user_cancel: CancellationToken,
	timeout_secs: f64,
	timed_out: Arc<AtomicBool>,
	timer: tokio::task::JoinHandle<()>,
}

impl RunGuard {
	fn new(user_cancel: &CancellationToken, timeout_secs: f64) -> Self {
		let cancel = CancellationToken::new();
		let timed_out = Arc::new(AtomicBool::new(false));
		let timer_cancel = cancel.clone();
		let user = user_cancel.clone();
		let fired = timed_out.clone();
		let duration = Duration::from_secs_f64(timeout_secs);
		let timer = tokio::spawn(async move {
			tokio::select! {
				_ = user.cancelled() => timer_cancel.cancel(),
				_ = tokio::time::sleep(duration) => {
					fired.store(true, Ordering::SeqCst);
					timer_cancel.cancel();
				}
			}
		});
		Self {
			cancel,
			user_cancel: user_cancel.clone(),
			timeout_secs,
			timed_out,
			timer,
		}
	}

	fn abort_error(&self, action: &str, server: Option<&str>) -> ToolError {
		if self.timed_out.load(Ordering::SeqCst) && !self.user_cancel.is_cancelled() {
			let server = server.unwrap_or("language server");
			ToolError::ExecutionFailed(format!(
				"LSP {action} timed out after {}s on {server}. The server may still be indexing; try again or pass timeout=<larger>.",
				self.timeout_secs
			))
		} else {
			ToolError::Aborted(format!("LSP {action} aborted"))
		}
	}
}

impl Drop for RunGuard {
	fn drop(&mut self) {
		self.timer.abort();
	}
}

fn clamp_timeout(value: Option<f64>) -> f64 {
	value
		.filter(|value| value.is_finite())
		.unwrap_or(DEFAULT_TIMEOUT_SECS)
		.max(MIN_TIMEOUT_SECS)
		.min(MAX_TIMEOUT_SECS)
}

fn text_result(text: String, success: bool, details: Value) -> ToolResult {
	ToolResult {
		tool_call_id: String::new(),
		content: text,
		is_error: !success,
		terminate: false,
		details: Some(details),
	}
}

fn details(action: &str, success: bool, server: Option<&str>, params: &LspParams) -> Value {
	let mut value = json!({
		"action": action,
		"success": success,
	});
	if let Some(server) = server.filter(|server| !server.is_empty()) {
		value["serverName"] = Value::String(server.to_string());
	}
	if let Ok(request) = serde_json::to_value(params) {
		value["request"] = request;
	}
	value
}

fn lsp_error_text(error: &LspError) -> String {
	format!("LSP error: {error}")
}

// =============================================================================
// Tool
// =============================================================================

pub struct LspTool {
	/// Ownership scope over the process-wide client registry. Root turns share
	/// the runner's scope (`RunnerSession`), children get their own, so a
	/// teardown never closes a server another session still uses.
	session: Arc<LspSession>,
}

impl Default for LspTool {
	fn default() -> Self {
		Self::new()
	}
}

impl LspTool {
	/// Tool with the default `lsp.shared = true` transport preference.
	#[must_use]
	pub fn new() -> Self {
		Self { session: LspSession::new(true) }
	}

	/// Tool whose clients prefer (`true`) or avoid (`false`) the broker mux.
	///
	/// This is the `lsp.shared` integration seam: the parent maps the setting
	/// here instead of mutating the process environment. The preference flows
	/// per call through client lookup, diagnostics, formatting and stale-client
	/// cleanup, so concurrent sessions with different settings stay isolated.
	#[must_use]
	pub fn with_shared(shared_mux: bool) -> Self {
		Self { session: LspSession::new(shared_mux) }
	}

	/// Tool that joins an existing ownership scope.
	///
	/// The runner rebuilds its registry every prompt but must keep one LSP
	/// session across turns, exactly like snapshots and the `EditTool` state.
	#[must_use]
	pub fn with_session(session: Arc<LspSession>) -> Self {
		Self { session }
	}

	/// Ownership scope this tool claims clients under.
	#[must_use]
	pub fn session(&self) -> &Arc<LspSession> {
		&self.session
	}
}

#[async_trait]
impl Tool for LspTool {
	fn name(&self) -> &str {
		"lsp"
	}

	fn description(&self) -> &str {
		SCHEMA_DESCRIPTION
	}

	fn schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"action": {
					"type": "string",
					"enum": [
						"diagnostics", "definition", "references", "hover", "symbols", "rename",
						"rename_file", "code_actions", "format", "type_definition",
						"implementation", "status", "reload", "capabilities", "request"
					],
					"description": "LSP action. Use `*` as file for workspace-scoped diagnostics/symbols/reload."
				},
				"file": {
					"type": "string",
					"description": "File path; a glob for diagnostics; `*` for workspace scope; source path for rename_file."
				},
				"line": {
					"type": "number",
					"minimum": 1,
					"description": "1-indexed line for position-based actions. Defaults to 1."
				},
				"symbol": {
					"type": "string",
					"description": "Substring used to resolve the column on `line`; supports `name#N` occurrence selectors."
				},
				"query": {
					"type": "string",
					"description": "Workspace symbol query, code-action selector, or LSP method name for action=request."
				},
				"new_name": {
					"type": "string",
					"description": "Required for rename and rename_file."
				},
				"apply": {
					"type": "boolean",
					"description": "For rename/rename_file apply unless false; for code_actions list unless true."
				},
				"timeout": {
					"type": "number",
					"minimum": 5,
					"maximum": 300,
					"description": "Seconds; default 20, clamped to 5..300."
				},
				"payload": {
					"type": "string",
					"description": "JSON string for action=request; overrides auto-built params."
				}
			},
			"required": ["action"]
		})
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let params: LspParams = serde_json::from_value(args.clone())
			.map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
		let action = params.action.trim().to_ascii_lowercase();
		let timeout_secs = clamp_timeout(params.timeout);
		let run = RunGuard::new(&ctx.cancellation_token, timeout_secs);
		let cwd = ctx.cwd.clone();

		match action.as_str() {
			"status" => {
				let config = get_config(&cwd);
				let mut lines = Vec::new();
				let started = get_active_clients();
				if config.is_empty() {
					lines.push("No language servers configured for this project".to_string());
				} else {
					let labelled: Vec<String> = config
						.names()
						.iter()
						.map(|name| {
							let server_config = config.get(name).expect("name from config");
							let started_client = started
								.iter()
								.find(|client| client.name == server_config.command);
							match started_client {
								Some(client) => format!("{name} ({})", client.status.as_str()),
								None => format!("{name} (configured, not started)"),
							}
						})
						.collect();
					lines.push(format!("Language servers: {}", labelled.join(", ")));
					lines.push(
						"  note: 'configured, not started' means the binary resolves on PATH but no request has spawned it yet; 'ready' means a client process is live for this cwd."
							.to_string(),
					);
				}
				let lspmux_state = lspmux::detect_lspmux().await;
				if lspmux_state.available {
					let path = lspmux_state
						.binary_path
						.as_ref()
						.map(|path| path.to_string_lossy().into_owned())
						.unwrap_or_default();
					lines.push(format!(
						"lspmux: {} ({path})",
						if lspmux_state.running { "running" } else { "available, not running" }
					));
				}
				Ok(text_result(lines.join("\n"), true, details(&action, true, None, &params)))
			}
			"diagnostics" => {
				self.diagnostics(&params, &cwd, &run, &action).await
			}
			"rename_file" => self.rename_file(&params, &cwd, &run, &action).await,
			"format" => self.format(&params, &cwd, &run, &action).await,
			"capabilities" => self.capabilities(&params, &cwd, &run, &action).await,
			"request" => self.request(&params, &cwd, &run, &action).await,
			"symbols" if params.file.as_deref() == Some("*") || params.file.is_none() => {
				self.workspace_symbols(&params, &cwd, &run, &action).await
			}
			"reload" if params.file.as_deref().is_none_or(|file| file == "*") => {
				self.workspace_reload(&params, &cwd, &run, &action).await
			}
			_ => self.single_file(&params, &cwd, &run, &action).await,
		}
	}
}

impl LspTool {
	async fn diagnostics(
		&self,
		params: &LspParams,
		cwd: &Path,
		run: &RunGuard,
		action: &str,
	) -> Result<ToolResult, ToolError> {
		let Some(file) = params.file.as_deref() else {
			return Ok(text_result(
				"Error: file parameter required. Use `*` for workspace-wide diagnostics or a path/glob for specific files.".to_string(),
				false,
				details(action, false, None, params),
			));
		};
		if file == "*" {
			let result = run_workspace_diagnostics(cwd, &run.cancel).await;
			return Ok(text_result(
				format!("Workspace diagnostics ({}):\n{}", result.project_type.description, result.output),
				true,
				details(action, true, None, params),
			));
		}

		let (targets, truncated) =
			resolve_diagnostic_targets(file, cwd, MAX_GLOB_DIAGNOSTIC_TARGETS)
				.await
				.map_err(|error| ToolError::ExecutionFailed(error))?;
		if targets.is_empty() {
			return Ok(text_result(
				format!("No files matched pattern: {file}"),
				true,
				details(action, true, None, params),
			));
		}
		let detailed = targets.len() > 1 || truncated;
		let wait_timeout = if detailed {
			BATCH_DIAGNOSTICS_WAIT_TIMEOUT_MS
		} else {
			SINGLE_DIAGNOSTICS_WAIT_TIMEOUT_MS
		}
		.min(run.timeout_secs as u64 * 1000);
		let config = get_config(cwd);
		let mut results = Vec::new();
		let mut all_servers = Vec::new();
		let mut attempts = 0usize;
		let mut successes = 0usize;
		if truncated {
			results.push(format!(
				"Pattern matched more than {MAX_GLOB_DIAGNOSTIC_TARGETS} files; showing first {MAX_GLOB_DIAGNOSTIC_TARGETS}. Narrow the glob or use workspace diagnostics."
			));
		}

		for target in &targets {
			if run.cancel.is_cancelled() {
				return Err(run.abort_error(action, None));
			}
			let resolved = resolve_to_cwd(target, cwd);
			let servers = get_servers_for_file(&config, &resolved);
			if servers.is_empty() {
				results.push(format!("{target}: No language server found"));
				continue;
			}
			let uri = file_to_uri(&resolved);
			let rel_path = format_path_relative_to_cwd(&resolved, cwd);
			let mut all_diagnostics: Vec<Diagnostic> = Vec::new();
			let mut failed_servers = Vec::new();
			let mut succeeded_servers = 0usize;

			for (server_name, server_config) in &servers {
				attempts += 1;
				all_servers.push(server_name.clone());
				let outcome = self
					.diagnostics_for_server(
						server_name,
						server_config,
						&resolved,
						&uri,
						wait_timeout,
						cwd,
						run,
					)
					.await;
				match outcome {
					Ok(diagnostics) => {
						all_diagnostics.extend(diagnostics);
						succeeded_servers += 1;
						successes += 1;
					}
					Err(LspError::Cancelled) => {
						if run.cancel.is_cancelled() {
							return Err(run.abort_error(action, Some(server_name)));
						}
						failed_servers.push(server_name.clone());
					}
					Err(_) => failed_servers.push(server_name.clone()),
				}
			}

			let mut seen = std::collections::HashSet::new();
			let mut unique = Vec::new();
			for diagnostic in all_diagnostics {
				if seen.insert(super::utils::diagnostic_key(&diagnostic)) {
					unique.push(diagnostic);
				}
			}
			sort_diagnostics(&mut unique);

			if !detailed {
				if succeeded_servers == 0 {
					return Ok(text_result(
						format!(
							"{rel_path}: all language servers failed ({})",
							failed_servers.join(", ")
						),
						false,
						details(action, false, Some(&all_servers.join(", ")), params),
					));
				}
				if unique.is_empty() {
					let mut text = "OK".to_string();
					if !failed_servers.is_empty() {
						text.push_str(&format!("\nsome servers failed: {}", failed_servers.join(", ")));
					}
					return Ok(text_result(
						text,
						true,
						details(action, true, Some(&all_servers.join(", ")), params),
					));
				}
				let summary = format_diagnostics_summary(&unique);
				let formatted: Vec<String> = unique
					.iter()
					.map(|diagnostic| format_diagnostic(diagnostic, &rel_path))
					.collect();
				let mut output = format!(
					"{summary}:\n{}",
					format_grouped_diagnostic_messages(&formatted)
				);
				if !failed_servers.is_empty() {
					output.push_str(&format!("\nsome servers failed: {}", failed_servers.join(", ")));
				}
				return Ok(text_result(
					output,
					true,
					details(action, true, Some(&all_servers.join(", ")), params),
				));
			}

			if unique.is_empty() {
				if succeeded_servers == 0 {
					results.push(format!(
						"{rel_path}: all language servers failed ({})",
						failed_servers.join(", ")
					));
				} else {
					results.push(format!("{rel_path}: no issues"));
					if !failed_servers.is_empty() {
						results.push(format!(
							"{rel_path}: some servers failed ({})",
							failed_servers.join(", ")
						));
					}
				}
			} else {
				results.push(format!("{rel_path}: {}", format_diagnostics_summary(&unique)));
				let formatted: Vec<String> = unique
					.iter()
					.map(|diagnostic| format_diagnostic(diagnostic, &rel_path))
					.collect();
				results.push(format_grouped_diagnostic_messages(&formatted));
				if !failed_servers.is_empty() {
					results.push(format!(
						"{rel_path}: some servers failed ({})",
						failed_servers.join(", ")
					));
				}
			}
		}

		let all_failed = attempts > 0 && successes == 0;
		let server_label = all_servers.join(", ");
		Ok(text_result(
			results.join("\n"),
			!all_failed,
			details(action, !all_failed, Some(&server_label), params),
		))
	}

	async fn diagnostics_for_server(
		&self,
		server_name: &str,
		server_config: &ServerConfig,
		absolute_path: &Path,
		uri: &str,
		wait_timeout_ms: u64,
		cwd: &Path,
		run: &RunGuard,
	) -> Result<Vec<Diagnostic>, LspError> {
		if server_config.is_custom_linter() {
			let result = linter::lint(server_name, server_config, cwd, absolute_path, &run.cancel)
				.await
				.map_err(LspError::Message);
			if run.cancel.is_cancelled() {
				return Err(LspError::Cancelled);
			}
			return result;
		}
		let client = get_or_create_client_for(&self.session, server_config, cwd, None, &run.cancel).await?;
		if server_config.is_project_aware() {
			wait_for_project_loaded(&client, &run.cancel, Some(absolute_path)).await;
		}
		let min_version = client.diagnostics_version();
		refresh_file(&client, absolute_path, &run.cancel).await?;
		let expected_version = client.open_file_version(uri);
		wait_for_diagnostics(
			&client,
			uri,
			WaitDiagnosticsOptions {
				timeout: Duration::from_millis(wait_timeout_ms),
				min_version: Some(min_version),
				expected_document_version: expected_version,
				..WaitDiagnosticsOptions::default()
			},
			&run.cancel,
		)
		.await
	}

	async fn single_file(
		&self,
		params: &LspParams,
		cwd: &Path,
		run: &RunGuard,
		action: &str,
	) -> Result<ToolResult, ToolError> {
		let normalized = if action == "actions" { "code_actions" } else { action };
		let Some(file) = params.file.as_deref() else {
			return Ok(text_result(
				"Error: file parameter required. Use `*` for workspace scope where supported.".to_string(),
				false,
				details(normalized, false, None, params),
			));
		};
		if file == "*" {
			return Ok(text_result(
				"No language server found for this action".to_string(),
				false,
				details(normalized, false, None, params),
			));
		}
		let resolved = resolve_to_cwd(file, cwd);
		let config = get_config(cwd);
		let Some((server_name, server_config)) = get_lsp_server_for_file(&config, &resolved) else {
			return Ok(text_result(
				"No language server found for this action".to_string(),
				false,
				details(normalized, false, None, params),
			));
		};
		if normalized == "reload" {
			clear_initialization_failure(&server_config, cwd, self.session.shared_mux());
		}

		let client = match get_or_create_client_for(&self.session, &server_config, cwd, None, &run.cancel).await {
			Ok(client) => client,
			Err(LspError::Cancelled) => return Err(run.abort_error(normalized, Some(&server_name))),
			Err(error) => {
				return Ok(text_result(
					lsp_error_text(&error),
					false,
					details(normalized, false, Some(&server_name), params),
				));
			}
		};
		if let Err(LspError::Cancelled) = ensure_file_open(&client, &resolved, &run.cancel).await {
			return Err(run.abort_error(normalized, Some(&server_name)));
		}
		let needs_project_index = PROJECT_INDEXED_ACTIONS.contains(&normalized)
			&& server_config.is_project_aware();
		if needs_project_index {
			wait_for_project_loaded(&client, &run.cancel, Some(&resolved)).await;
		}

		if params.line.is_some()
			&& params.symbol.as_deref().is_none_or(str::is_empty)
			&& matches!(normalized, "references" | "rename" | "definition")
			&& server_config.is_project_aware()
		{
			return Ok(text_result(
				format!("symbol is required for project-aware {normalized}; pass symbol=<name>, optionally symbol#N for repeated occurrences"),
				false,
				details(normalized, false, Some(&server_name), params),
			));
		}

		let line = params.line.unwrap_or(1);
		let character = match resolve_symbol_column(&resolved, line, params.symbol.as_deref()).await {
			Ok(character) => character,
			Err(error) => {
				return Ok(text_result(
					error,
					false,
					details(normalized, false, Some(&server_name), params),
				));
			}
		};
		let uri = file_to_uri(&resolved);
		let position = Position { line: line.saturating_sub(1), character };

		let outcome = self
			.dispatch_single(
				normalized,
				params,
				&client,
				&server_name,
				&server_config,
				&resolved,
				&uri,
				position,
				cwd,
				run,
			)
			.await;

		match outcome {
			Ok(output) => Ok(text_result(
				output,
				true,
				details(normalized, true, Some(&server_name), params),
			)),
			Err(LspError::Cancelled) => Err(run.abort_error(normalized, Some(&server_name))),
			Err(error) => Ok(text_result(
				lsp_error_text(&error),
				false,
				details(normalized, false, Some(&server_name), params),
			)),
		}
	}

	#[allow(clippy::too_many_arguments)]
	async fn dispatch_single(
		&self,
		action: &str,
		params: &LspParams,
		client: &Arc<LspClient>,
		_server_name: &str,
		server_config: &ServerConfig,
		file: &Path,
		uri: &str,
		position: Position,
		cwd: &Path,
		run: &RunGuard,
	) -> Result<String, LspError> {
		let path_params = json!({ "textDocument": { "uri": uri }, "position": position });
		match action {
			"definition" | "type_definition" | "implementation" => {
				let method = match action {
					"definition" => "textDocument/definition",
					"type_definition" => "textDocument/typeDefinition",
					_ => "textDocument/implementation",
				};
				let result = client
					.send_request(method, path_params, &run.cancel, None)
					.await?;
				let locations = normalize_location_result(result);
				let noun = match action {
					"definition" => "definition",
					"type_definition" => "type definition",
					_ => "implementation",
				};
				if locations.is_empty() {
					return Ok(format!("No {noun} found"));
				}
				let lines = futures::future::join_all(
					locations
						.iter()
						.map(|location| diagnostics::format_location_with_context(location, cwd)),
				)
				.await;
				Ok(format!(
					"Found {} {noun}(s):\n{}",
					locations.len(),
					lines.join("\n")
				))
			}
			"references" => {
				let mut result: Vec<super::types::Location> = Vec::new();
				for attempt in 0..=REFERENCES_RETRY_COUNT {
					let response = client
						.send_request(
							"textDocument/references",
							json!({
								"textDocument": { "uri": uri },
								"position": position,
								"context": { "includeDeclaration": true }
							}),
							&run.cancel,
							None,
						)
						.await?;
					result = normalize_location_result(response);
					if !server_config.is_project_aware() || attempt == REFERENCES_RETRY_COUNT {
						break;
					}
					if !result.is_empty()
						&& !is_only_queried_declaration(&result, uri, position)
					{
						break;
					}
					wait_for_project_loaded(client, &run.cancel, None).await;
					tokio::select! {
						_ = run.cancel.cancelled() => return Err(LspError::Cancelled),
						_ = tokio::time::sleep(Duration::from_millis(REFERENCES_RETRY_DELAY_MS)) => {}
					}
				}
				if result.is_empty() {
					return Ok("No references found".to_string());
				}
				let contextual_count = result.len().min(REFERENCE_CONTEXT_LIMIT);
				let contextual_lines = futures::future::join_all(
					result[..contextual_count]
						.iter()
						.map(|location| diagnostics::format_location_with_context(location, cwd)),
				)
				.await;
				let plain_lines: Vec<String> = result[contextual_count..]
					.iter()
					.map(|location| format!("  {}", format_location(location, cwd)))
					.collect();
				let mut lines = contextual_lines;
				if !plain_lines.is_empty() {
					lines.push(format!(
						"  ... {} additional reference(s) shown without context",
						plain_lines.len()
					));
					lines.extend(plain_lines);
				}
				Ok(format!("Found {} reference(s):\n{}", result.len(), lines.join("\n")))
			}
			"hover" => {
				let result = client
					.send_request("textDocument/hover", path_params, &run.cancel, None)
					.await?;
				let hover: Option<super::types::Hover> = serde_json::from_value(result).ok();
				match hover {
					Some(hover) => {
						let text = super::utils::extract_hover_text(&hover.contents);
						Ok(if text.is_empty() { "No hover information".to_string() } else { text })
					}
					None => Ok("No hover information".to_string()),
				}
			}
			"code_actions" => {
				let diagnostics = client
					.diagnostics_for(uri)
					.map(|published| published.diagnostics)
					.unwrap_or_default();
				let only = if params.apply != Some(true) {
					params.query.as_deref().filter(|query| !query.is_empty()).map(|query| vec![query.to_string()])
				} else {
					None
				};
				let context = CodeActionContext {
					diagnostics,
					only,
					trigger_kind: 1,
				};
				let result = client
					.send_request(
						"textDocument/codeAction",
						json!({
							"textDocument": { "uri": uri },
							"range": { "start": position, "end": position },
							"context": context
						}),
						&run.cancel,
						None,
					)
					.await?;
				let actions: Vec<Value> = result.as_array().cloned().unwrap_or_default();
				if actions.is_empty() {
					return Ok("No code actions available".to_string());
				}
				if params.apply == Some(true) {
					let Some(query) = params.query.as_deref().map(str::trim).filter(|query| !query.is_empty()) else {
						return Ok("Error: query parameter required when apply=true for code_actions".to_string());
					};
					let selected = if query.chars().all(|ch| ch.is_ascii_digit()) {
						query.parse::<usize>().ok().and_then(|index| actions.get(index).cloned())
					} else {
						actions.iter().find(|action| {
							action
								.get("title")
								.and_then(Value::as_str)
								.is_some_and(|title| title.to_ascii_lowercase().contains(&query.to_ascii_lowercase()))
						}).cloned()
					};
					let Some(selected) = selected else {
						let lines: Vec<String> = actions
							.iter()
							.enumerate()
							.filter_map(|(index, value)| {
								serde_json::from_value::<CodeActionOrCommand>(value.clone())
									.ok()
									.map(|action| format!("  {}", format_code_action(&action, index)))
							})
							.collect();
						return Ok(format!(
							"No code action matches \"{query}\". Available actions:\n{}",
							lines.join("\n")
						));
					};
					let title = selected
						.get("title")
						.and_then(Value::as_str)
						.unwrap_or("code action")
						.to_string();
					let applied = apply_code_action(client, selected, &run.cancel).await?;
					let Some(applied) = applied else {
						return Ok(format!(
							"Action \"{title}\" has no workspace edit or command to apply"
						));
					};
					let mut lines = Vec::new();
					if !applied.edits.is_empty() {
						lines.push("  Workspace edit:".to_string());
						lines.extend(applied.edits.iter().map(|edit| format!("    {edit}")));
					}
					if !applied.commands.is_empty() {
						lines.push("  Executed command(s):".to_string());
						lines.extend(applied.commands.iter().map(|command| format!("    {command}")));
					}
					return Ok(format!("Applied \"{}\":\n{}", applied.title, lines.join("\n")));
				}
				let lines: Vec<String> = actions
					.iter()
					.enumerate()
					.filter_map(|(index, value)| {
						serde_json::from_value::<CodeActionOrCommand>(value.clone())
							.ok()
							.map(|action| format!("  {}", format_code_action(&action, index)))
					})
					.collect();
				Ok(format!("{} code action(s):\n{}", actions.len(), lines.join("\n")))
			}
			"symbols" => {
				let result = client
					.send_request(
						"textDocument/documentSymbol",
						json!({ "textDocument": { "uri": uri } }),
						&run.cancel,
						None,
					)
					.await?;
				let items = result.as_array().cloned().unwrap_or_default();
				if items.is_empty() {
					return Ok("No symbols found".to_string());
				}
				let rel_path = format_path_relative_to_cwd(file, cwd);
				if items[0].get("selectionRange").is_some() {
					let symbols: Vec<DocumentSymbol> = items
						.iter()
						.filter_map(|item| serde_json::from_value(item.clone()).ok())
						.collect();
					let lines: Vec<String> = symbols
						.iter()
						.flat_map(|symbol| format_document_symbol(symbol, 0))
						.collect();
					Ok(format!("Symbols in {rel_path}:\n{}", lines.join("\n")))
				} else {
					let symbols: Vec<SymbolInformation> = items
						.iter()
						.filter_map(|item| serde_json::from_value(item.clone()).ok())
						.collect();
					let lines: Vec<String> = symbols
						.iter()
						.map(|symbol| {
							format!(
								"{} @ line {}",
								symbol.name,
								symbol.location.range.start.line + 1
							)
						})
						.collect();
					Ok(format!("Symbols in {rel_path}:\n{}", lines.join("\n")))
				}
			}
			"rename" => {
				let Some(new_name) = params.new_name.as_deref().filter(|name| !name.is_empty()) else {
					return Ok("Error: new_name parameter required for rename".to_string());
				};
				let result = client
					.send_request(
						"textDocument/rename",
						json!({
							"textDocument": { "uri": uri },
							"position": position,
							"newName": new_name
						}),
						&run.cancel,
						None,
					)
					.await?;
				let edit: Option<WorkspaceEdit> = serde_json::from_value(result).ok();
				let Some(edit) = edit else {
					return Ok("Rename returned no edits".to_string());
				};
				if params.apply == Some(false) {
					let preview = format_workspace_edit(&edit, cwd);
					return Ok(format!(
						"Rename preview:\n{}",
						preview.iter().map(|line| format!("  {line}")).collect::<Vec<_>>().join("\n")
					));
				}
				let applied = client::apply_workspace_edit_with_lsp(&edit, cwd, &run.cancel)
					.await
					.map_err(LspError::Message)?;
				Ok(format!(
					"Applied rename:\n{}",
					applied.iter().map(|line| format!("  {line}")).collect::<Vec<_>>().join("\n")
				))
			}
			"reload" => reload_server(client, _server_name, &run.cancel).await,
			"format" => {
				// Routed through `LspTool::format`; kept unreachable so the match
				// stays exhaustive if the dispatch table changes.
				Ok("Formatting is handled before server dispatch".to_string())
			}
			other => Ok(format!("Unknown action: {other}")),
		}
	}

	async fn format(
		&self,
		params: &LspParams,
		cwd: &Path,
		run: &RunGuard,
		action: &str,
	) -> Result<ToolResult, ToolError> {
		let Some(file) = params.file.as_deref().filter(|file| *file != "*") else {
			return Ok(text_result(
				"Error: file parameter required for format".to_string(),
				false,
				details(action, false, None, params),
			));
		};
		let resolved = resolve_to_cwd(file, cwd);
		if tokio::fs::metadata(&resolved).await.is_err() {
			return Ok(text_result(
				format!(
					"Error: file does not exist: {}",
					format_path_relative_to_cwd(&resolved, cwd)
				),
				false,
				details(action, false, None, params),
			));
		}
		let config = get_config(cwd);
		let servers = get_servers_for_file(&config, &resolved);
		let (status, _formatted) = diagnostics::format_file(&resolved, cwd, &servers, &run.cancel, &self.session)
			.await
			.map_err(ToolError::ExecutionFailed)?;
		if run.cancel.is_cancelled() {
			return Err(run.abort_error(action, None));
		}
		let server_label = servers.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(", ");
		match status {
			diagnostics::FormatResultStatus::Formatted => Ok(text_result(
				format!("Formatted {}", format_path_relative_to_cwd(&resolved, cwd)),
				true,
				details(action, true, Some(&server_label), params),
			)),
			diagnostics::FormatResultStatus::Unchanged => Ok(text_result(
				format!("{} is already formatted", format_path_relative_to_cwd(&resolved, cwd)),
				true,
				details(action, true, Some(&server_label), params),
			)),
			diagnostics::FormatResultStatus::Unsupported => Ok(text_result(
				format!(
					"No formatter available for {}",
					format_path_relative_to_cwd(&resolved, cwd)
				),
				true,
				details(action, true, Some(&server_label), params),
			)),
			diagnostics::FormatResultStatus::Failed => Ok(text_result(
				format!("Formatting failed for {}", format_path_relative_to_cwd(&resolved, cwd)),
				false,
				details(action, false, Some(&server_label), params),
			)),
		}
	}

	async fn rename_file(
		&self,
		params: &LspParams,
		cwd: &Path,
		run: &RunGuard,
		action: &str,
	) -> Result<ToolResult, ToolError> {
		let (Some(file), Some(new_name)) = (params.file.as_deref(), params.new_name.as_deref()) else {
			return Ok(text_result(
				"Error: rename_file requires both `file` (source path) and `new_name` (destination path)".to_string(),
				false,
				details(action, false, None, params),
			));
		};
		let source = resolve_to_cwd(file, cwd);
		let dest = resolve_to_cwd(new_name, cwd);
		if source == dest {
			return Ok(text_result(
				"Error: source and destination paths are identical".to_string(),
				false,
				details(action, false, None, params),
			));
		}
		let source_metadata = match tokio::fs::metadata(&source).await {
			Ok(metadata) => metadata,
			Err(error) => {
				let source_label = format_path_relative_to_cwd(&source, cwd);
				let message = if error.kind() == std::io::ErrorKind::NotFound {
					format!("Error: source path does not exist: {source_label}")
				} else {
					format!("Error: cannot read source path {source_label}: {error}")
				};
				return Ok(text_result(message, false, details(action, false, None, params)));
			}
		};
		match tokio::fs::symlink_metadata(&dest).await {
			Ok(_) => {
				return Ok(text_result(
					format!(
						"Error: destination already exists: {}",
						format_path_relative_to_cwd(&dest, cwd)
					),
					false,
					details(action, false, None, params),
				));
			}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
			Err(error) => {
				return Ok(text_result(
					format!(
						"Error: cannot read destination path {}: {error}",
						format_path_relative_to_cwd(&dest, cwd)
					),
					false,
					details(action, false, None, params),
				));
			}
		}
		let (pairs, exceeded) = enumerate_rename_pairs(&source, &dest, source_metadata.is_dir()).await?;
		if exceeded {
			return Ok(text_result(
				format!(
					"Error: rename touches more than {MAX_RENAME_PAIRS} files; move the directory with the filesystem and re-index instead."
				),
				false,
				details(action, false, None, params),
			));
		}
		if pairs.is_empty() {
			return Ok(text_result(
				"Error: no files to rename".to_string(),
				false,
				details(action, false, None, params),
			));
		}
		let config = get_config(cwd);
		let mut relevant_paths = vec![source.clone(), dest.clone()];
		for (old_uri, new_uri) in &pairs {
			relevant_paths.push(uri_to_file(old_uri));
			relevant_paths.push(uri_to_file(new_uri));
		}
		let mut relevant_names = std::collections::HashSet::new();
		for path in &relevant_paths {
			for (name, _) in get_lsp_servers_for_file(&config, path) {
				relevant_names.insert(name);
			}
		}
		let servers: Vec<(String, ServerConfig)> = get_lsp_servers(&config)
			.into_iter()
			.filter(|(name, _)| relevant_names.contains(name))
			.collect();
		let lsp_params = json!({
			"files": pairs.iter().map(|(old, new)| json!({ "oldUri": old, "newUri": new })).collect::<Vec<_>>()
		});
		let mut per_server_edits: Vec<(String, WorkspaceEdit)> = Vec::new();
		let mut server_notes = Vec::new();
		let mut responding = Vec::new();
		let mut hard_failures = Vec::new();

		for (server_name, server_config) in &servers {
			let client = match get_or_create_client_for(&self.session, server_config, cwd, None, &run.cancel).await {
				Ok(client) => client,
				Err(LspError::Cancelled) => return Err(run.abort_error(action, Some(server_name))),
				Err(error) => {
					server_notes.push(format!("  {server_name}: {error}"));
					continue;
				}
			};
			if server_config.is_project_aware() {
				wait_for_project_loaded(&client, &run.cancel, Some(&source)).await;
			}
			match client
				.send_request("workspace/willRenameFiles", lsp_params.clone(), &run.cancel, None)
				.await
			{
				Ok(result) => {
					responding.push(server_name.clone());
					match serde_json::from_value::<WorkspaceEdit>(result) {
						Ok(edit) if !edit.is_empty() => per_server_edits.push((server_name.clone(), edit)),
						_ => {}
					}
				}
				Err(LspError::Cancelled) => return Err(run.abort_error(action, Some(server_name))),
				Err(error) => {
					if !is_method_not_found_error(&error.message()) {
						server_notes.push(format!("  {server_name}: {error}"));
						hard_failures.push(server_name.clone());
					}
				}
			}
		}

		let source_label = format_path_relative_to_cwd(&source, cwd);
		let dest_label = format_path_relative_to_cwd(&dest, cwd);
		let file_count_label = if source_metadata.is_dir() {
			format!("{} file(s) under {source_label}", pairs.len())
		} else {
			source_label.clone()
		};
		let should_apply = params.apply != Some(false);
		if !should_apply {
			let mut lines = vec![format!("Rename preview: {file_count_label} → {dest_label}")];
			if per_server_edits.is_empty() {
				lines.push("  No LSP edits would be applied".to_string());
			}
			for (server_name, edit) in &per_server_edits {
				let edits = format_workspace_edit(edit, cwd);
				if edits.is_empty() {
					continue;
				}
				lines.push(format!("  {server_name}:"));
				lines.extend(edits.iter().map(|line| format!("    {line}")));
			}
			if !server_notes.is_empty() {
				lines.push("  Server notes:".to_string());
				lines.extend(server_notes);
			}
			return Ok(text_result(
				lines.join("\n"),
				true,
				details(action, true, Some(&responding.join(", ")), params),
			));
		}

		if !hard_failures.is_empty() {
			let mut lines = vec![format!(
				"Error: aborted rename; workspace/willRenameFiles failed on {}, so semantic references would not be updated. No files were moved.",
				hard_failures.join(", ")
			)];
			lines.push("  Server notes:".to_string());
			lines.extend(server_notes);
			return Ok(text_result(
				lines.join("\n"),
				false,
				details(action, false, Some(&responding.join(", ")), params),
			));
		}

		let mut summary = Vec::new();
		let server_configs: std::collections::HashMap<String, ServerConfig> =
			servers.iter().cloned().collect();
		struct Bucket {
			primary_server: String,
			edits: Vec<TextEdit>,
			discarded: usize,
			conflict_servers: Vec<String>,
		}
		let mut buckets: std::collections::HashMap<String, Bucket> = std::collections::HashMap::new();
		for (server_name, edit) in &per_server_edits {
			let incoming_primary = server_configs
				.get(server_name)
				.is_some_and(ServerConfig::is_project_aware);
			for (uri, edits) in flatten_workspace_text_edits(edit) {
				let Some(existing) = buckets.get_mut(&uri) else {
					buckets.insert(
						uri,
						Bucket {
							primary_server: server_name.clone(),
							edits,
							discarded: 0,
							conflict_servers: Vec::new(),
						},
					);
					continue;
				};
				let existing_primary = server_configs
					.get(&existing.primary_server)
					.is_some_and(ServerConfig::is_project_aware);
				if incoming_primary && !existing_primary {
					let mut kept_old = Vec::new();
					let mut discarded_old = 0;
					for old_edit in &existing.edits {
						if edits.iter().any(|new_edit| ranges_overlap(new_edit.range, old_edit.range)) {
							discarded_old += 1;
						} else {
							kept_old.push(old_edit.clone());
						}
					}
					if discarded_old > 0 {
						push_unique(&mut existing.conflict_servers, &existing.primary_server);
					}
					existing.discarded += discarded_old;
					existing.primary_server = server_name.clone();
					let mut combined = edits;
					combined.extend(kept_old);
					existing.edits = combined;
				} else {
					let mut discarded_new = 0;
					for new_edit in edits {
						if existing
							.edits
							.iter()
							.any(|accepted| ranges_overlap(accepted.range, new_edit.range))
						{
							discarded_new += 1;
						} else {
							existing.edits.push(new_edit);
						}
					}
					if discarded_new > 0 {
						push_unique(&mut existing.conflict_servers, server_name);
						existing.discarded += discarded_new;
					}
				}
			}
		}
		for (uri, bucket) in &buckets {
			sort_and_validate_text_edits(&bucket.edits).map_err(ToolError::ExecutionFailed)?;
			let path = uri_to_file(uri);
			let rel = format_path_relative_to_cwd(&path, cwd);
			summary.push(format!(
				"  {}: applied {} edit(s) to {rel}",
				bucket.primary_server,
				bucket.edits.len()
			));
			if bucket.discarded > 0 {
				summary.push(format!(
					"    note: discarded {} overlapping edit(s) from {} (kept {})",
					bucket.discarded,
					bucket.conflict_servers.join(", "),
					bucket.primary_server
				));
			}
		}
		let references: Vec<RenameReferenceEdit> = buckets
			.into_iter()
			.map(|(uri, bucket)| RenameReferenceEdit { file_path: uri_to_file(&uri), edits: bucket.edits })
			.collect();
		apply_edits_then_rename(&references, &source, &dest)
			.await
			.map_err(ToolError::ExecutionFailed)?;
		summary.push(format!("  Renamed {source_label} → {dest_label}"));

		for (server_name, server_config) in &servers {
			let client = match get_or_create_client_for(&self.session, server_config, cwd, None, &run.cancel).await {
				Ok(client) => client,
				Err(error) => {
					server_notes.push(format!("  {server_name}: {error}"));
					continue;
				}
			};
			for (old_uri, _) in &pairs {
				if client.has_open_file(old_uri) {
					let _ = client
						.send_notification(
							"textDocument/didClose",
							Some(json!({ "textDocument": { "uri": old_uri } })),
							&run.cancel,
						)
						.await;
					client.close_file(old_uri);
				}
			}
			if let Err(error) = client
				.send_notification(
					"workspace/didRenameFiles",
					Some(lsp_params.clone()),
					&run.cancel,
				)
				.await
			{
				server_notes.push(format!("  {server_name}: {error}"));
			}
		}
		if !server_notes.is_empty() {
			summary.push("  Server notes:".to_string());
			summary.extend(server_notes);
		}
		Ok(text_result(
			format!("Renamed {file_count_label} → {dest_label}\n{}", summary.join("\n")),
			true,
			details(action, true, Some(&responding.join(", ")), params),
		))
	}

	async fn capabilities(
		&self,
		params: &LspParams,
		cwd: &Path,
		run: &RunGuard,
		action: &str,
	) -> Result<ToolResult, ToolError> {
		let config = get_config(cwd);
		let server_list: Vec<(String, ServerConfig)> = match params.file.as_deref() {
			Some(file) if file != "*" => {
				let resolved = resolve_to_cwd(file, cwd);
				get_lsp_servers_for_file(&config, &resolved)
			}
			_ => get_lsp_servers(&config),
		};
		if server_list.is_empty() {
			return Ok(text_result(
				if params.file.as_deref().is_some_and(|file| file != "*") {
					"No language server found for this file".to_string()
				} else {
					"No language servers configured".to_string()
				},
				false,
				details(action, false, None, params),
			));
		}
		let mut sections = Vec::new();
		let mut responding = Vec::new();
		for (server_name, server_config) in server_list {
			if run.cancel.is_cancelled() {
				return Err(run.abort_error(action, Some(&server_name)));
			}
			match get_or_create_client_for(&self.session, &server_config, cwd, None, &run.cancel).await {
				Ok(client) => {
					responding.push(server_name.clone());
					let capabilities = client.server_capabilities().unwrap_or_else(|| json!({}));
					let pretty = serde_json::to_string_pretty(&capabilities).unwrap_or_else(|_| "{}".to_string());
					sections.push(format!("{server_name}:"));
					sections.push(
						format!("  capabilities: {}", pretty.split('\n').collect::<Vec<_>>().join("\n  ")),
					);
				}
				Err(LspError::Cancelled) => return Err(run.abort_error(action, Some(&server_name))),
				Err(error) => sections.push(format!("{server_name}: failed to start ({error})")),
			}
		}
		Ok(text_result(
			sections.join("\n"),
			true,
			details(action, true, Some(&responding.join(", ")), params),
		))
	}

	async fn request(
		&self,
		params: &LspParams,
		cwd: &Path,
		run: &RunGuard,
		action: &str,
	) -> Result<ToolResult, ToolError> {
		let Some(method) = params.query.as_deref().map(str::trim).filter(|method| !method.is_empty()) else {
			return Ok(text_result(
				"Error: action=request requires `query` to specify the LSP method name (e.g., 'rust-analyzer/expandMacro')".to_string(),
				false,
				details(action, false, None, params),
			));
		};
		let config = get_config(cwd);
		let resolved_target = params
			.file
			.as_deref()
			.filter(|file| *file != "*")
			.map(|file| resolve_to_cwd(file, cwd));
		let chosen = match &resolved_target {
			Some(target) => match get_lsp_server_for_file(&config, target) {
				Some(server) => server,
				None => {
					return Ok(text_result(
						"No language server found for this file".to_string(),
						false,
						details(action, false, None, params),
					));
				}
			},
			None => match get_lsp_servers(&config).into_iter().next() {
				Some(server) => server,
				None => {
					return Ok(text_result(
						"No language servers configured".to_string(),
						false,
						details(action, false, None, params),
					));
				}
			},
		};
		let (chosen_name, chosen_config) = chosen;
		let request_params = if let Some(payload) = &params.payload {
			match serde_json::from_str::<Value>(payload) {
				Ok(value) => value,
				Err(error) => {
					return Ok(text_result(
						format!("Error: invalid JSON in payload: {error}"),
						false,
						details(action, false, Some(&chosen_name), params),
					));
				}
			}
		} else if let Some(target) = &resolved_target {
			let uri = file_to_uri(target);
			match params.line {
				Some(line) => {
					let character = match resolve_symbol_column(target, line, params.symbol.as_deref()).await {
						Ok(character) => character,
						Err(error) => {
							return Ok(text_result(
								error,
								false,
								details(action, false, Some(&chosen_name), params),
							));
						}
					};
					json!({ "textDocument": { "uri": uri }, "position": { "line": line.saturating_sub(1), "character": character } })
				}
				None => json!({ "textDocument": { "uri": uri } }),
			}
		} else {
			json!({})
		};

		let client = match get_or_create_client_for(&self.session, &chosen_config, cwd, None, &run.cancel).await {
			Ok(client) => client,
			Err(LspError::Cancelled) => return Err(run.abort_error(action, Some(&chosen_name))),
			Err(error) => {
				return Ok(text_result(
					lsp_error_text(&error),
					false,
					details(action, false, Some(&chosen_name), params),
				));
			}
		};
		if let Some(target) = &resolved_target {
			if let Err(LspError::Cancelled) = ensure_file_open(&client, target, &run.cancel).await {
				return Err(run.abort_error(action, Some(&chosen_name)));
			}
		}
		match client
			.send_request(method, request_params.clone(), &run.cancel, None)
			.await
		{
			Ok(result) => {
				let formatted = match result {
					Value::Null => "null".to_string(),
					Value::String(text) => text,
					other => serde_json::to_string_pretty(&other).unwrap_or_else(|_| other.to_string()),
				};
				Ok(text_result(
					format!("{chosen_name} ← {method}:\n{formatted}"),
					true,
					details(action, true, Some(&chosen_name), params),
				))
			}
			Err(LspError::Cancelled) => Err(run.abort_error(action, Some(&chosen_name))),
			Err(error) => {
				let preview = serde_json::to_string(&request_params).unwrap_or_default();
				let preview = if preview.chars().count() > 400 {
					format!("{}...", preview.chars().take(397).collect::<String>())
				} else {
					preview
				};
				Ok(text_result(
					format!(
						"LSP error from {chosen_name} on {method}: {error}\n  params: {preview}"
					),
					false,
					details(action, false, Some(&chosen_name), params),
				))
			}
		}
	}

	async fn workspace_symbols(
		&self,
		params: &LspParams,
		cwd: &Path,
		run: &RunGuard,
		action: &str,
	) -> Result<ToolResult, ToolError> {
		let Some(query) = params.query.as_deref().map(str::trim).filter(|query| !query.is_empty()) else {
			return Ok(text_result(
				"Error: query parameter required for workspace symbol search".to_string(),
				false,
				details(action, false, None, params),
			));
		};
		let config = get_config(cwd);
		let servers: Vec<(String, ServerConfig)> = get_lsp_servers(&config);
		if servers.is_empty() {
			return Ok(text_result(
				"No language server found for this action".to_string(),
				false,
				details(action, false, None, params),
			));
		}
		let mut aggregated: Vec<SymbolInformation> = Vec::new();
		let mut responding = Vec::new();
		let mut failures = Vec::new();
		for (server_name, server_config) in &servers {
			if run.cancel.is_cancelled() {
				return Err(run.abort_error(action, Some(server_name)));
			}
			let client = match get_or_create_client_for(&self.session, server_config, cwd, None, &run.cancel).await {
				Ok(client) => client,
				Err(LspError::Cancelled) => return Err(run.abort_error(action, Some(server_name))),
				Err(error) => {
					failures.push(format!("  {server_name}: {error}"));
					continue;
				}
			};
			match client
				.send_request("workspace/symbol", json!({ "query": query }), &run.cancel, None)
				.await
			{
				Ok(result) => {
					responding.push(server_name.clone());
					let symbols: Vec<SymbolInformation> = result
						.as_array()
						.map(|items| {
							items
								.iter()
								.filter_map(|item| serde_json::from_value(item.clone()).ok())
								.collect()
						})
						.unwrap_or_default();
					aggregated.extend(filter_workspace_symbols(symbols, query));
				}
				Err(LspError::Cancelled) => return Err(run.abort_error(action, Some(server_name))),
				Err(error) => failures.push(format!("  {server_name}: {error}")),
			}
		}
		let failure_section = if failures.is_empty() {
			String::new()
		} else {
			format!("\nServer failures:\n{}", failures.join("\n"))
		};
		if responding.is_empty() {
			return Ok(text_result(
				format!("Workspace symbol search failed: all language servers failed{failure_section}"),
				false,
				details(action, false, None, params),
			));
		}
		let deduped = dedupe_workspace_symbols(aggregated);
		if deduped.is_empty() {
			return Ok(text_result(
				format!("No symbols matching \"{query}\"{failure_section}"),
				true,
				details(action, true, Some(&responding.join(", ")), params),
			));
		}
		let limited: Vec<&SymbolInformation> = deduped.iter().take(WORKSPACE_SYMBOL_LIMIT).collect();
		let lines: Vec<String> = limited
			.iter()
			.map(|symbol| format!("  {}", format_symbol_information(symbol, cwd)))
			.collect();
		let truncation = if deduped.len() > WORKSPACE_SYMBOL_LIMIT {
			format!(
				"\n[…{} symbols elided…]",
				deduped.len() - WORKSPACE_SYMBOL_LIMIT
			)
		} else {
			String::new()
		};
		Ok(text_result(
			format!(
				"Found {} symbol(s) matching \"{query}\":\n{}{truncation}{failure_section}",
				deduped.len(),
				lines.join("\n")
			),
			true,
			details(action, true, Some(&responding.join(", ")), params),
		))
	}

	async fn workspace_reload(
		&self,
		params: &LspParams,
		cwd: &Path,
		run: &RunGuard,
		action: &str,
	) -> Result<ToolResult, ToolError> {
		client::invalidate_config(cwd);
		let config = get_config(cwd);
		let servers: Vec<(String, ServerConfig)> = get_lsp_servers(&config);
		let server_configs: Vec<ServerConfig> = servers.iter().map(|(_, server)| server.clone()).collect();
		let stopped = shutdown_stale_clients(cwd, &server_configs, &run.cancel, self.session.shared_mux()).await;
		let stopped = match stopped {
			Ok(stopped) => stopped,
			Err(LspError::Cancelled) => return Err(run.abort_error(action, None)),
			Err(error) => return Ok(text_result(lsp_error_text(&error), false, details(action, false, None, params))),
		};
		if servers.is_empty() {
			return Ok(text_result(
				"No language server found for this action".to_string(),
				false,
				details(action, false, None, params),
			));
		}
		let mut outputs = Vec::new();
		if !stopped.is_empty() {
			outputs.push(format!(
				"Stopped {} server(s) with superseded configuration: {}",
				stopped.len(),
				stopped.join(", ")
			));
		}
		for (server_name, server_config) in servers {
			if run.cancel.is_cancelled() {
				return Err(run.abort_error(action, Some(&server_name)));
			}
			clear_initialization_failure(&server_config, cwd, self.session.shared_mux());
			match get_or_create_client_for(&self.session, &server_config, cwd, None, &run.cancel).await {
				Ok(client) => match reload_server(&client, &server_name, &run.cancel).await {
					Ok(line) => outputs.push(line),
					Err(LspError::Cancelled) => return Err(run.abort_error(action, Some(&server_name))),
					Err(error) => outputs.push(format!("Failed to reload {server_name}: {error}")),
				},
				Err(LspError::Cancelled) => return Err(run.abort_error(action, Some(&server_name))),
				Err(error) => outputs.push(format!("Failed to reload {server_name}: {error}")),
			}
		}
		Ok(text_result(
			outputs.join("\n"),
			true,
			details(action, true, None, params),
		))
	}
}

// =============================================================================
// Rename helpers
// =============================================================================

async fn enumerate_rename_pairs(
	source: &Path,
	dest: &Path,
	source_is_directory: bool,
) -> Result<(Vec<(String, String)>, bool), ToolError> {
	if !source_is_directory {
		return Ok((
			vec![(file_to_uri(source), file_to_uri(dest))],
			false,
		));
	}
	let mut pairs = Vec::new();
	let mut stack = vec![source.to_path_buf()];
	while let Some(dir) = stack.pop() {
		let mut entries = tokio::fs::read_dir(&dir)
			.await
			.map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
		while let Some(entry) = entries
			.next_entry()
			.await
			.map_err(|error| ToolError::ExecutionFailed(error.to_string()))?
		{
			let file_type = entry
				.file_type()
				.await
				.map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
			if file_type.is_dir() {
				stack.push(entry.path());
				continue;
			}
			if !file_type.is_file() {
				continue;
			}
			if pairs.len() >= MAX_RENAME_PAIRS {
				return Ok((pairs, true));
			}
			let old_path = entry.path();
			let relative = old_path
				.strip_prefix(source)
				.map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
			pairs.push((file_to_uri(&old_path), file_to_uri(&dest.join(relative))));
		}
	}
	pairs.sort_by(|left, right| left.0.cmp(&right.0));
	Ok((pairs, false))
}

fn push_unique(values: &mut Vec<String>, value: &str) {
	if !values.iter().any(|existing| existing == value) {
		values.push(value.to_string());
	}
}

fn is_only_queried_declaration(
	locations: &[super::types::Location],
	uri: &str,
	position: Position,
) -> bool {
	if locations.len() != 1 {
		return false;
	}
	let location = &locations[0];
	if location.uri != uri {
		return false;
	}
	let range = location.range;
	range_contains_position(range, position)
}

fn range_contains_position(range: Range, position: Position) -> bool {
	let after_start = position.line > range.start.line
		|| (position.line == range.start.line && position.character >= range.start.character);
	let before_end = position.line < range.end.line
		|| (position.line == range.end.line && position.character <= range.end.character);
	after_start && before_end
}

// =============================================================================
// Code action application
// =============================================================================

struct AppliedCodeAction {
	title: String,
	edits: Vec<String>,
	commands: Vec<String>,
}

async fn apply_code_action(
	client: &Arc<LspClient>,
	action: Value,
	cancel: &CancellationToken,
) -> Result<Option<AppliedCodeAction>, LspError> {
	let parsed: CodeActionOrCommand =
		serde_json::from_value(action.clone()).map_err(|error| LspError::Message(error.to_string()))?;
	match parsed {
		CodeActionOrCommand::Command(command) => {
			execute_command(client, &command.command, command.arguments, cancel).await?;
			return Ok(Some(AppliedCodeAction {
				title: command.title,
				edits: Vec::new(),
				commands: vec![command.command],
			}));
		}
		CodeActionOrCommand::Action(mut code_action) => {
			if code_action.edit.is_none() {
				if let Ok(resolved) = client
					.send_request("codeAction/resolve", action, cancel, None)
					.await
				{
					if let Ok(resolved) = serde_json::from_value::<CodeAction>(resolved) {
						code_action = resolved;
					}
				}
			}
			let edits = match &code_action.edit {
				Some(edit) if !edit.is_empty() => {
					client::apply_workspace_edit_with_lsp(edit, &client.cwd, cancel)
						.await
						.map_err(LspError::Message)?
				}
				_ => Vec::new(),
			};
			let mut commands = Vec::new();
			if let Some(command) = &code_action.command {
				execute_command(client, &command.command, command.arguments.clone(), cancel).await?;
				commands.push(command.command.clone());
			}
			if edits.is_empty() && commands.is_empty() {
				return Ok(None);
			}
			Ok(Some(AppliedCodeAction { title: code_action.title, edits, commands }))
		}
	}
}

async fn execute_command(
	client: &Arc<LspClient>,
	command: &str,
	arguments: Option<Vec<Value>>,
	cancel: &CancellationToken,
) -> Result<(), LspError> {
	client
		.send_request(
			"workspace/executeCommand",
			json!({ "command": command, "arguments": arguments.unwrap_or_default() }),
			cancel,
			None,
		)
		.await
		.map(|_| ())
}
