//! Diagnostics waiting/pulling, formatting, and per-file aggregation.
//!
//! Port of `packages/coding-agent/src/lsp/diagnostics.ts` and
//! `format-options.ts`. Each server pipeline is bounded by a wall-clock budget
//! so a wedged server cannot hold an edit or diagnostics request forever.

use std::{collections::HashSet, path::Path, time::Duration};

use futures::future::join_all;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{
	editorconfig::get_editor_config_formatting,
	client::{
		self, ensure_file_open, get_or_create_client, wait_for_diagnostics,
		wait_for_project_loaded, LspClient, LspError, WaitDiagnosticsOptions,
	},
	config::has_root_marker_ancestor,
	linter,
	types::{Diagnostic, ServerConfig, TextEdit},
	edits::apply_text_edits_to_string as apply_text_edits_string,
	utils::{
		diagnostic_key, file_to_uri, format_diagnostic, format_diagnostics_summary,
		format_path_relative_to_cwd, sort_diagnostics,
	},
};

pub const SINGLE_DIAGNOSTICS_WAIT_TIMEOUT_MS: u64 = 3_000;
pub const BATCH_DIAGNOSTICS_WAIT_TIMEOUT_MS: u64 = 400;
pub const DIAGNOSTICS_PIPELINE_GRACE_MS: u64 = 10_000;
pub const DIAGNOSTIC_MESSAGE_LIMIT: usize = 50;
const FALLBACK_TAB_SIZE: u32 = 2;
const FALLBACK_INSERT_SPACES: bool = true;

const ORPHAN_TYPESCRIPT_CODES: &[i64] = &[1375, 1378, 2307, 2580, 2591, 2792, 2867];

// =============================================================================
// Versions
// =============================================================================

#[derive(Debug, Clone, Default)]
pub struct ServerVersionMap(pub std::collections::HashMap<String, u64>);

impl ServerVersionMap {
	#[must_use]
	pub fn get(&self, name: &str) -> Option<u64> {
		self.0.get(name).copied()
	}
}

#[derive(Debug, Clone, Default)]
pub struct OpenVersionMap(pub std::collections::HashMap<String, i64>);

impl OpenVersionMap {
	#[must_use]
	pub fn get(&self, name: &str) -> Option<i64> {
		self.0.get(name).copied()
	}
}

pub async fn capture_diagnostic_versions(
	cwd: &Path,
	servers: &[(String, ServerConfig)],
	init_timeout: Option<Duration>,
	cancel: &CancellationToken,
	shared_mux: bool,
) -> ServerVersionMap {
	let mut versions = ServerVersionMap::default();
	let futures = servers.iter().filter_map(|(name, config)| {
		if config.is_custom_linter() {
			return None;
		}
		let name = name.clone();
		let config = config.clone();
		let cwd = cwd.to_path_buf();
		let cancel = cancel.clone();
		Some(async move {
			if let Ok(client) = get_or_create_client(&config, &cwd, init_timeout, &cancel, shared_mux).await {
				Some((name, client.diagnostics_version()))
			} else {
				None
			}
		})
	});
	for entry in join_all(futures).await.into_iter().flatten() {
		versions.0.insert(entry.0, entry.1);
	}
	versions
}

pub async fn capture_open_file_versions(
	absolute_path: &Path,
	cwd: &Path,
	servers: &[(String, ServerConfig)],
	cancel: &CancellationToken,
	shared_mux: bool,
) -> OpenVersionMap {
	let uri = file_to_uri(absolute_path);
	let futures = servers.iter().filter_map(|(name, config)| {
		if config.is_custom_linter() {
			return None;
		}
		let name = name.clone();
		let config = config.clone();
		let cwd = cwd.to_path_buf();
		let cancel = cancel.clone();
		let uri = uri.clone();
		Some(async move {
			let client = get_or_create_client(&config, &cwd, None, &cancel, shared_mux).await.ok()?;
			let version = client.open_file_version(&uri)?;
			Some((name, version))
		})
	});
	let mut versions = OpenVersionMap::default();
	for entry in join_all(futures).await.into_iter().flatten() {
		versions.0.insert(entry.0, entry.1);
	}
	versions
}

// =============================================================================
// Diagnostics
// =============================================================================

#[derive(Debug, Clone)]
pub struct FileDiagnosticsResult {
	pub server: String,
	pub messages: Vec<String>,
	pub summary: String,
	pub errored: bool,
	pub formatter: Option<FormatResultStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatResultStatus {
	Unchanged,
	Formatted,
	Failed,
	Unsupported,
}

impl FormatResultStatus {
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Unchanged => "unchanged",
			Self::Formatted => "formatted",
			Self::Failed => "failed",
			Self::Unsupported => "unsupported",
		}
	}
}

pub struct GetDiagnosticsOptions<'a> {
	pub min_versions: Option<&'a ServerVersionMap>,
	pub expected_document_versions: Option<&'a OpenVersionMap>,
	pub timeout_ms: Option<u64>,
	pub pipeline_budget_ms: Option<u64>,
}

impl Default for GetDiagnosticsOptions<'_> {
	fn default() -> Self {
		Self {
			min_versions: None,
			expected_document_versions: None,
			timeout_ms: None,
			pipeline_budget_ms: None,
		}
	}
}

pub async fn get_diagnostics_for_file(
	absolute_path: &Path,
	cwd: &Path,
	servers: &[(String, ServerConfig)],
	options: GetDiagnosticsOptions<'_>,
	cancel: &CancellationToken,
	shared_mux: bool,
) -> Option<FileDiagnosticsResult> {
	if servers.is_empty() {
		return None;
	}
	let wait_budget = Duration::from_millis(
		options.timeout_ms.unwrap_or(SINGLE_DIAGNOSTICS_WAIT_TIMEOUT_MS),
	);
	let pipeline_budget = Duration::from_millis(
		options
			.pipeline_budget_ms
			.unwrap_or(wait_budget.as_millis() as u64 + DIAGNOSTICS_PIPELINE_GRACE_MS),
	);
	let uri = file_to_uri(absolute_path);
	let rel_path = format_path_relative_to_cwd(absolute_path, cwd);

	let futures = servers.iter().map(|(server_name, server_config)| {
		let server_name = server_name.clone();
		let server_config = server_config.clone();
		let cwd = cwd.to_path_buf();
		let absolute_path = absolute_path.to_path_buf();
		let uri = uri.clone();
		let rel_path = rel_path.clone();
		let cancel = cancel.clone();
		let shared_mux = shared_mux;
		let min_version = options.min_versions.and_then(|versions| versions.get(&server_name));
		let expected_version = options
			.expected_document_versions
			.and_then(|versions| versions.get(&server_name));
		async move {
			let pipeline = async {
				if server_config.is_custom_linter() {
					return match linter::lint(&server_name, &server_config, &cwd, &absolute_path, &cancel).await {
						Ok(diagnostics) => Some((server_name, server_config, diagnostics, None)),
						Err(error) => Some((server_name, server_config, Vec::<Diagnostic>::new(), Some(error))),
					};
				}
				let client = get_or_create_client(
					&server_config,
					&cwd,
					Some(pipeline_budget),
					&cancel,
					shared_mux,
				)
				.await
				.ok()?;
				ensure_file_open(&client, &absolute_path, &cancel).await.ok()?;
				if server_config.is_project_aware() {
					wait_for_project_loaded(&client, &cancel, Some(&absolute_path)).await;
				}
				let diagnostics = wait_for_diagnostics(
					&client,
					&uri,
					WaitDiagnosticsOptions {
						timeout: wait_budget,
						min_version,
						expected_document_version: expected_version,
						..WaitDiagnosticsOptions::default()
					},
					&cancel,
				)
				.await
				.ok()?;
				Some((server_name, server_config, diagnostics, None))
			};
			let _ = rel_path;
			tokio::time::timeout(pipeline_budget, pipeline).await.ok().flatten()
		}
	});
	let results: Vec<Option<(String, ServerConfig, Vec<Diagnostic>, Option<String>)>> = join_all(futures).await;

	let mut all_diagnostics = Vec::new();
	let mut server_names = Vec::new();
	let mut failures = Vec::new();
	for result in results.into_iter().flatten() {
		let (server_name, server_config, diagnostics, failure) = result;
		server_names.push(server_name.clone());
		if let Some(error) = failure {
			failures.push(format!("{server_name}: {error}"));
		}
		all_diagnostics.extend(filter_orphan_project_diagnostics(
			absolute_path,
			&server_name,
			&server_config,
			diagnostics,
		));
	}

	if server_names.is_empty() {
		return None;
	}
	if all_diagnostics.is_empty() && failures.is_empty() {
		return Some(FileDiagnosticsResult {
			server: server_names.join(", "),
			messages: Vec::new(),
			summary: "OK".to_string(),
			errored: false,
			formatter: None,
		});
	}

	let mut seen = HashSet::new();
	let mut unique = Vec::new();
	for diagnostic in all_diagnostics {
		if seen.insert(diagnostic_key(&diagnostic)) {
			unique.push(diagnostic);
		}
	}
	sort_diagnostics(&mut unique);
	let failure_count = failures.len();
	let mut messages = failures;
	let remaining = DIAGNOSTIC_MESSAGE_LIMIT.saturating_sub(messages.len());
	messages.extend(
		unique
			.iter()
			.map(|diagnostic| format_diagnostic(diagnostic, &rel_path))
			.take(remaining),
	);
	let messages = messages.into_iter().take(DIAGNOSTIC_MESSAGE_LIMIT).collect();
	let mut summary = format_diagnostics_summary(&unique);
	if failure_count > 0 {
		summary = format!("{summary}; {failure_count} server(s) failed");
	}
	let errored = failure_count > 0 || unique.iter().any(|diagnostic| diagnostic.severity == Some(1));
	Some(FileDiagnosticsResult {
		server: server_names.join(", "),
		messages,
		summary,
		errored,
		formatter: None,
	})
}

pub const LOCATION_CONTEXT_LINES: u32 = 1;

pub async fn format_location_with_context(
	location: &super::types::Location,
	cwd: &Path,
) -> String {
	let header = format!("  {}", super::utils::format_location(location, cwd));
	let context = super::utils::read_location_context(
		&super::utils::uri_to_file(&location.uri),
		location.range.start.line + 1,
		LOCATION_CONTEXT_LINES,
	)
	.await;
	if context.is_empty() {
		return header;
	}
	format!(
		"{header}\n{}",
		context.iter().map(|line| format!("    {line}")).collect::<Vec<_>>().join("\n")
	)
}

/// True when the file lives under a Rust workspace marker.
#[must_use]
pub fn has_rust_workspace_ancestor(file_path: &Path) -> bool {
	let mut dir = file_path.parent().map(Path::to_path_buf);
	while let Some(current) = dir {
		if current.join("Cargo.toml").exists() || current.join("rust-analyzer.toml").exists() {
			return true;
		}
		dir = current.parent().map(Path::to_path_buf);
		if current.parent().is_none() {
			return false;
		}
	}
	false
}

fn filter_orphan_project_diagnostics(
	absolute_path: &Path,
	server_name: &str,
	server_config: &ServerConfig,
	diagnostics: Vec<Diagnostic>,
) -> Vec<Diagnostic> {
	if server_config.root_markers.is_empty()
		|| has_root_marker_ancestor(absolute_path, &server_config.root_markers)
	{
		return diagnostics;
	}
	diagnostics
		.into_iter()
		.filter(|diagnostic| !is_typescript_project_diagnostic(server_name, diagnostic))
		.collect()
}

fn diagnostic_code_number(diagnostic: &Diagnostic) -> Option<i64> {
	match diagnostic.code.as_ref()? {
		serde_json::Value::Number(number) => number.as_i64(),
		serde_json::Value::String(text) => text.parse::<i64>().ok(),
		_ => None,
	}
}

fn is_typescript_project_diagnostic(server_name: &str, diagnostic: &Diagnostic) -> bool {
	if diagnostic.source.as_deref() != Some("typescript")
		&& !server_name.to_ascii_lowercase().contains("typescript")
	{
		return false;
	}
	diagnostic_code_number(diagnostic)
		.is_some_and(|code| ORPHAN_TYPESCRIPT_CODES.contains(&code))
}

// =============================================================================
// Formatting
// =============================================================================

#[derive(Debug, Clone)]
pub struct FormatContentResult {
	pub content: String,
	pub failed: bool,
	pub unsupported: bool,
}

/// Format `content` using the first applicable custom linter or LSP formatter.
pub async fn format_content(
	absolute_path: &Path,
	content: &str,
	cwd: &Path,
	servers: &[(String, ServerConfig)],
	cancel: &CancellationToken,
	shared_mux: bool,
) -> FormatContentResult {
	if servers.is_empty() {
		return FormatContentResult { content: content.to_string(), failed: false, unsupported: true };
	}
	let mut had_failure = false;
	for (server_name, server_config) in servers {
		if cancel.is_cancelled() {
			break;
		}
		if server_config.is_custom_linter() {
			match linter::format(server_name, server_config, cwd, absolute_path, content, cancel).await {
				Ok(Some(formatted)) => {
					return FormatContentResult { content: formatted, failed: false, unsupported: false };
				}
				Ok(None) => continue,
				Err(_) => {
					had_failure = true;
					continue;
				}
			}
		}
		let client = match get_or_create_client(server_config, cwd, None, cancel, shared_mux).await {
			Ok(client) => client,
			Err(LspError::Cancelled) => break,
			Err(_) => {
				had_failure = true;
				continue;
			}
		};
		let supports_formatting = client
			.server_capabilities()
			.and_then(|capabilities| capabilities.get("documentFormattingProvider").cloned())
			.is_some_and(|provider| !provider.is_null() && provider != serde_json::Value::Bool(false));
		if !supports_formatting {
			continue;
		}
		if sync_content_best_effort(&client, absolute_path, content, cancel).await.is_err() {
			had_failure = true;
			continue;
		}
		let edits = client
			.send_request(
				"textDocument/formatting",
				json!({
					"textDocument": { "uri": file_to_uri(absolute_path) },
					"options": resolve_format_options(absolute_path, content),
				}),
				cancel,
				None,
			)
			.await;
		match edits {
			Ok(value) => {
				let parsed: Vec<TextEdit> = serde_json::from_value(value).unwrap_or_default();
				if parsed.is_empty() {
					return FormatContentResult { content: content.to_string(), failed: false, unsupported: false };
				}
				match apply_text_edits_string(content, &parsed) {
					Ok(formatted) => {
						return FormatContentResult { content: formatted, failed: false, unsupported: false };
					}
					Err(_) => {
						had_failure = true;
					}
				}
			}
			Err(LspError::Cancelled) => break,
			Err(_) => had_failure = true,
		}
	}
	FormatContentResult {
		content: content.to_string(),
		failed: had_failure,
		unsupported: !had_failure,
	}
}

async fn sync_content_best_effort(
	client: &std::sync::Arc<LspClient>,
	absolute_path: &Path,
	content: &str,
	cancel: &CancellationToken,
) -> Result<(), LspError> {
	client::sync_content(client, absolute_path, content, cancel).await
}

/// Format a file on disk, writing back only when the formatter changed it.
pub async fn format_file(
	absolute_path: &Path,
	cwd: &Path,
	servers: &[(String, ServerConfig)],
	cancel: &CancellationToken,
	shared_mux: bool,
) -> Result<(FormatResultStatus, String), String> {
	let original = tokio::fs::read_to_string(absolute_path)
		.await
		.map_err(|error| error.to_string())?;
	let result = format_content(absolute_path, &original, cwd, servers, cancel, shared_mux).await;
	if result.failed {
		return Ok((FormatResultStatus::Failed, original));
	}
	if result.unsupported {
		return Ok((FormatResultStatus::Unsupported, original));
	}
	if result.content == original {
		return Ok((FormatResultStatus::Unchanged, result.content));
	}
	crate::lsp::edits::write_text_atomic(absolute_path, &result.content).await?;
	Ok((FormatResultStatus::Formatted, result.content))
}

// =============================================================================
// Formatting options
// =============================================================================

fn gcd(mut a: u32, mut b: u32) -> u32 {
	while b != 0 {
		let temp = b;
		b = a % b;
		a = temp;
	}
	a
}

#[must_use]
pub fn detect_indent_from_content(content: &str) -> (Option<u32>, Option<bool>) {
	if content.is_empty() {
		return (None, None);
	}
	let mut insert_spaces: Option<bool> = None;
	let mut unit = 0u32;
	for line in content.split('\n') {
		if line.is_empty() || line.trim().is_empty() {
			continue;
		}
		let first = line.as_bytes()[0];
		if first != b' ' && first != b'\t' {
			continue;
		}
		if insert_spaces.is_none() {
			insert_spaces = Some(first == b' ');
		}
		if first == b'\t' {
			continue;
		}
		let mut spaces = 0u32;
		for byte in line.as_bytes() {
			if *byte != b' ' {
				break;
			}
			spaces += 1;
		}
		if spaces == 0 {
			continue;
		}
		unit = if unit == 0 { spaces } else { gcd(unit, spaces) };
	}
	let tab_size = if insert_spaces == Some(true) && unit > 0 { Some(unit) } else { None };
	(tab_size, insert_spaces)
}

/// Formatting options payload. Precedence matches upstream: `.editorconfig`
/// chain, then content sniffing, then the 2-space/spaces fallback.
#[must_use]
pub fn resolve_format_options(file_path: &Path, content: &str) -> serde_json::Value {
	let (detected_tab, detected_spaces) = detect_indent_from_content(content);
	let editorconfig = get_editor_config_formatting(file_path);
	json!({
		"tabSize": editorconfig.tab_size.or(detected_tab).unwrap_or(FALLBACK_TAB_SIZE),
		"insertSpaces": editorconfig.insert_spaces.or(detected_spaces).unwrap_or(FALLBACK_INSERT_SPACES),
		"trimTrailingWhitespace": true,
		"insertFinalNewline": true,
		"trimFinalNewlines": true,
	})
}

