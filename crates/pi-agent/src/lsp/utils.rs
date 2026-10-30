//! URI conversion, path display, symbol resolution, and output formatting.
//!
//! Port of `packages/coding-agent/src/lsp/utils.ts`, minus theme-dependent
//! icons (the Rust tool emits plain text that stays stable in headless runs).

use std::{
	collections::HashSet,
	path::{Component, Path, PathBuf},
};

use serde_json::Value;

use super::types::{
	CodeAction, CodeActionOrCommand, Diagnostic, DocumentSymbol, Location, SymbolInformation,
	TextEdit, WorkspaceEdit,
};

// =============================================================================
// URI handling
// =============================================================================

/// Convert a local path to a percent-encoded `file://` URI.
#[must_use]
pub fn file_to_uri(path: &Path) -> String {
	let absolute = absolutize(path);
	let mut raw = absolute.to_string_lossy().to_string();
	if !raw.starts_with('/') {
		raw.insert(0, '/');
	}
	let mut encoded = String::with_capacity(raw.len() + 8);
	encoded.push_str("file://");
	for byte in raw.bytes() {
		match byte {
			b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
				encoded.push(char::from(byte));
			}
			_ => {
				const HEX: &[u8; 16] = b"0123456789ABCDEF";
				encoded.push('%');
				encoded.push(char::from(HEX[(byte >> 4) as usize]));
				encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
			}
		}
	}
	encoded
}

/// Lax conversion used for display and server-provided paths: tolerates
/// unencoded characters and non-file URIs.
#[must_use]
pub fn uri_to_file(uri: &str) -> PathBuf {
	if let Some(path) = file_uri_path(uri) {
		return path;
	}
	if !uri.starts_with("file://") {
		return PathBuf::from(uri);
	}
	let raw = uri.strip_prefix("file://").unwrap_or(uri);
	let raw = raw.strip_prefix("localhost").unwrap_or(raw);
	PathBuf::from(percent_decode(raw))
}

/// Strict conversion for filesystem mutations: only local `file://` URIs are
/// accepted and the decoded path must be absolute.
#[must_use]
pub fn file_uri_path(uri: &str) -> Option<PathBuf> {
	let raw = uri.strip_prefix("file://")?;
	let raw = raw.strip_prefix("localhost").unwrap_or(raw);
	if raw.is_empty() {
		return None;
	}
	let decoded = percent_decode(raw);
	let path = PathBuf::from(decoded);
	if path.is_absolute() {
		return Some(path);
	}
	// `file://C:/...` (no slash) is malformed; `file:///C:/...` is absolute on Windows.
	#[cfg(windows)]
	{
		let text = path.to_string_lossy();
		if text.len() > 2 && text.as_bytes()[1] == b':' {
			return Some(path);
		}
	}
	None
}

fn percent_decode(raw: &str) -> String {
	let bytes = raw.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut index = 0;
	while index < bytes.len() {
		if bytes[index] == b'%' && index + 2 < bytes.len() {
			let high = hex_value(bytes[index + 1]);
			let low = hex_value(bytes[index + 2]);
			if let (Some(high), Some(low)) = (high, low) {
				out.push((high << 4) | low);
				index += 3;
				continue;
			}
		}
		out.push(bytes[index]);
		index += 1;
	}
	String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
	match byte {
		b'0'..=b'9' => Some(byte - b'0'),
		b'a'..=b'f' => Some(byte - b'a' + 10),
		b'A'..=b'F' => Some(byte - b'A' + 10),
		_ => None,
	}
}

/// Canonical map key for a file URI: equivalent spellings collapse.
#[must_use]
pub fn uri_key(uri: &str) -> String {
	if let Some(path) = file_uri_path(uri) {
		return normalize_absolute(&path).to_string_lossy().into_owned();
	}
	uri.to_string()
}

// =============================================================================
// Paths
// =============================================================================

#[must_use]
pub fn absolutize(path: &Path) -> PathBuf {
	if path.is_absolute() {
		normalize_absolute(path)
	} else {
		std::env::current_dir()
			.map(|cwd| normalize_absolute(&cwd.join(path)))
			.unwrap_or_else(|_| path.to_path_buf())
	}
}

#[must_use]
pub fn normalize_absolute(path: &Path) -> PathBuf {
	let mut out = PathBuf::new();
	for component in path.components() {
		match component {
			Component::CurDir => {}
			Component::ParentDir => {
				if !out.pop() {
					out.push("..");
				}
			}
			other => out.push(other.as_os_str()),
		}
	}
	out
}

/// Resolve a tool-supplied path: `~`, absolute, or relative to `cwd`.
#[must_use]
pub fn resolve_to_cwd(raw: &str, cwd: &Path) -> PathBuf {
	if raw == "/" {
		return cwd.to_path_buf();
	}
	let expanded = if let Some(rest) = raw.strip_prefix("~/") {
		std::env::var_os("HOME")
			.map(PathBuf::from)
			.map(|home| home.join(rest))
			.unwrap_or_else(|| PathBuf::from(raw))
	} else if raw == "~" {
		std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(raw))
	} else {
		PathBuf::from(raw)
	};
	if expanded.is_absolute() {
		normalize_absolute(&expanded)
	} else {
		normalize_absolute(&cwd.join(expanded))
	}
}

/// Display a path relative to `cwd`, or absolute when outside it.
#[must_use]
pub fn format_path_relative_to_cwd(path: &Path, cwd: &Path) -> String {
	let absolute = if path.is_absolute() { normalize_absolute(path) } else { normalize_absolute(&cwd.join(path)) };
	let root = normalize_absolute(cwd);
	if let Ok(relative) = absolute.strip_prefix(&root) {
		let display = path_to_posix(relative);
		if display.is_empty() { ".".to_string() } else { display }
	} else {
		path_to_posix(&absolute)
	}
}

#[must_use]
pub fn path_to_posix(path: &Path) -> String {
	let mut text = path.to_string_lossy().replace('\\', "/");
	if text.starts_with("//") {
		text = format!("/{}", text.trim_start_matches('/'));
	}
	text
}

#[must_use]
pub fn is_path_within(file: &Path, root: &Path) -> bool {
	let file = absolutize(file);
	let root = absolutize(root);
	file == root || file.starts_with(&root)
}

/// Map a path extension to an LSP language id. Servers override this via
/// `ServerConfig::language_id`.
#[must_use]
pub fn detect_language_id(path: &Path) -> String {
	let extension = path.extension().and_then(|value| value.to_str()).unwrap_or("").to_ascii_lowercase();
	let language = match extension.as_str() {
		"rs" => "rust",
		"ts" => "typescript",
		"tsx" => "typescriptreact",
		"js" => "javascript",
		"jsx" => "javascriptreact",
		"mjs" | "cjs" => "javascript",
		"py" | "pyi" => "python",
		"go" => "go",
		"c" => "c",
		"h" => "c",
		"cc" | "cpp" | "cxx" | "hpp" | "hxx" => "cpp",
		"m" => "objective-c",
		"mm" => "objective-cpp",
		"java" => "java",
		"kt" | "kts" => "kotlin",
		"scala" | "sc" | "sbt" => "scala",
		"swift" => "swift",
		"css" => "css",
		"scss" => "scss",
		"less" => "less",
		"html" | "htm" => "html",
		"json" => "json",
		"jsonc" => "jsonc",
		"yaml" | "yml" => "yaml",
		"toml" => "toml",
		"md" | "markdown" => "markdown",
		"sh" | "bash" | "zsh" => "shellscript",
		"lua" => "lua",
		"rb" | "rake" | "gemspec" => "ruby",
		"php" => "php",
		"cs" => "csharp",
		"zig" => "zig",
		"ex" | "exs" => "elixir",
		"heex" => "phoenix-heex",
		"erl" | "hrl" => "erlang",
		"hs" | "lhs" => "haskell",
		"ml" | "mli" => "ocaml",
		"tf" | "tfvars" => "terraform",
		"nix" => "nix",
		"vue" => "vue",
		"svelte" => "svelte",
		"astro" => "astro",
		"sql" => "sql",
		"xml" => "xml",
		"dockerfile" => "dockerfile",
		_ => return extension,
	};
	language.to_string()
}

// =============================================================================
// Diagnostics
// =============================================================================

#[must_use]
pub fn severity_to_string(severity: Option<u8>) -> &'static str {
	match severity.unwrap_or(1) {
		1 => "error",
		2 => "warning",
		3 => "info",
		4 => "hint",
		_ => "unknown",
	}
}

pub fn sort_diagnostics(diagnostics: &mut [Diagnostic]) {
	diagnostics.sort_by(|a, b| {
		let severity = a.severity.unwrap_or(1).cmp(&b.severity.unwrap_or(1));
		if severity.is_ne() {
			return severity;
		}
		let line = a.range.start.line.cmp(&b.range.start.line);
		if line.is_ne() {
			return line;
		}
		let character = a.range.start.character.cmp(&b.range.start.character);
		if character.is_ne() {
			return character;
		}
		a.message.cmp(&b.message)
	});
}

#[must_use]
pub fn diagnostic_key(diagnostic: &Diagnostic) -> String {
	format!(
		"{}:{}:{}:{}:{}",
		diagnostic.range.start.line,
		diagnostic.range.start.character,
		diagnostic.range.end.line,
		diagnostic.range.end.character,
		diagnostic.message
	)
}

fn strip_diagnostic_noise(message: &str) -> String {
	message
		.lines()
		.filter(|line| {
			let trimmed = line.trim();
			!trimmed.starts_with("for further information visit")
				&& !(trimmed.starts_with("http://") || trimmed.starts_with("https://"))
		})
		.collect::<Vec<_>>()
		.join("\n")
		.trim()
		.to_string()
}

#[must_use]
pub fn format_diagnostic(diagnostic: &Diagnostic, file_path: &str) -> String {
	let severity = severity_to_string(diagnostic.severity);
	let line = diagnostic.range.start.line + 1;
	let column = diagnostic.range.start.character + 1;
	let source = diagnostic
		.source
		.as_ref()
		.map(|source| format!("[{source}] "))
		.unwrap_or_default();
	let code = diagnostic
		.code
		.as_ref()
		.map(|code| match code {
			Value::String(text) => format!(" ({text})"),
			Value::Number(number) => format!(" ({number})"),
			other => format!(" ({other})"),
		})
		.unwrap_or_default();
	let message = strip_diagnostic_noise(&diagnostic.message);
	format!("{file_path}:{line}:{column} [{severity}] {source}{message}{code}")
}

/// Group `path:line:col [severity] ...` lines under a `# path` header. The
/// upstream renderer folds common directory prefixes; this keeps one header per
/// file, which is deterministic and lossless for model consumption.
#[must_use]
pub fn format_grouped_diagnostic_messages(messages: &[String]) -> String {
	let mut groups: Vec<(String, Vec<String>)> = Vec::new();
	let mut ungrouped = Vec::new();
	for message in messages {
		let Some((file, rest)) = split_diagnostic_message(message) else {
			ungrouped.push(message.clone());
			continue;
		};
		if let Some((_, lines)) = groups.iter_mut().find(|(path, _)| path == &file) {
			lines.push(format!("  {rest}"));
		} else {
			groups.push((file, vec![format!("  {rest}")]));
		}
	}
	let mut lines = Vec::new();
	for (file, diagnostics) in &groups {
		lines.push(format!("# {file}"));
		lines.extend(diagnostics.iter().cloned());
	}
	if !ungrouped.is_empty() {
		if !lines.is_empty() {
			lines.push(String::new());
		}
		lines.extend(ungrouped);
	}
	lines.join("\n")
}

fn split_diagnostic_message(message: &str) -> Option<(String, String)> {
	// Split on the first `:digits:digits ` boundary after the path.
	let bytes = message.as_bytes();
	let mut index = 0;
	while index < bytes.len() {
		if bytes[index] == b':' {
			let line_end = bytes[index + 1..].iter().position(|byte| !byte.is_ascii_digit())?;
			if line_end == 0 {
				index += 1;
				continue;
			}
			let after_line = index + 1 + line_end;
			if bytes.get(after_line) == Some(&b':') {
				let column_end = bytes[after_line + 1..]
					.iter()
					.position(|byte| !byte.is_ascii_digit())
					.unwrap_or(bytes.len() - after_line - 1);
				if column_end > 0 {
					let file = message[..index].replace('\\', "/");
					// TS keeps `line:column` in the message body; only the path
					// prefix before the first colon is stripped.
					let rest = message[index + 1..].trim_start().to_string();
					return Some((file, rest));
				}
			}
		}
		index += 1;
	}
	None
}

#[must_use]
pub fn format_diagnostics_summary(diagnostics: &[Diagnostic]) -> String {
	let mut counts = [0usize; 5];
	for diagnostic in diagnostics {
		let index = usize::from(diagnostic.severity.unwrap_or(1)).min(4);
		counts[index] += 1;
	}
	let mut parts = Vec::new();
	for (count, name) in counts.iter().skip(1).zip(["error", "warning", "info", "hint"]) {
		if *count > 0 {
			parts.push(format!("{count} {name}(s)"));
		}
	}
	if parts.is_empty() { "no issues".to_string() } else { parts.join(", ") }
}

// =============================================================================
// Location and edit formatting
// =============================================================================

/// Normalize a definition/reference result into plain locations. Supports
/// `Location`, `Location[]`, `LocationLink`, and `LocationLink[]`.
#[must_use]
pub fn normalize_location_result(value: Value) -> Vec<Location> {
	let items = match value {
		Value::Null => return Vec::new(),
		Value::Array(items) => items,
		other => vec![other],
	};
	let mut locations = Vec::new();
	for item in items {
		let Some(object) = item.as_object() else {
			continue;
		};
		if object.contains_key("uri") {
			if let Ok(location) = serde_json::from_value::<Location>(item) {
				locations.push(location);
			}
		} else if object.contains_key("targetUri") {
			if let Ok(link) = serde_json::from_value::<super::types::LocationLink>(item) {
				locations.push(Location { uri: link.target_uri, range: link.target_selection_range });
			}
		}
	}
	locations
}

#[must_use]
pub fn format_location(location: &Location, cwd: &Path) -> String {
	let file = format_path_relative_to_cwd(&uri_to_file(&location.uri), cwd);
	format!(
		"{file}:{}:{}",
		location.range.start.line + 1,
		location.range.start.character + 1
	)
}

#[must_use]
pub fn format_workspace_edit(edit: &WorkspaceEdit, cwd: &Path) -> Vec<String> {
	let mut results = Vec::new();
	if let Some(changes) = &edit.changes {
		for (uri, edits) in changes {
			let file = format_path_relative_to_cwd(&uri_to_file(uri), cwd);
			let suffix = if edits.len() > 1 { "s" } else { "" };
			results.push(format!("{file}: {} edit{suffix}", edits.len()));
		}
	}
	if let Some(changes) = &edit.document_changes {
		for change in changes {
			match change {
				super::types::WorkspaceDocumentChange::Text(text) => {
					let file = format_path_relative_to_cwd(&uri_to_file(&text.text_document.uri), cwd);
					let suffix = if text.edits.len() > 1 { "s" } else { "" };
					results.push(format!("{file}: {} edit{suffix}", text.edits.len()));
				}
				super::types::WorkspaceDocumentChange::Resource(resource) => match resource {
					super::types::ResourceOperation::Create { uri, .. } => {
						results.push(format!("CREATE: {}", format_path_relative_to_cwd(&uri_to_file(uri), cwd)));
					}
					super::types::ResourceOperation::Rename { old_uri, new_uri, .. } => {
						results.push(format!(
							"RENAME: {} -> {}",
							format_path_relative_to_cwd(&uri_to_file(old_uri), cwd),
							format_path_relative_to_cwd(&uri_to_file(new_uri), cwd)
						));
					}
					super::types::ResourceOperation::Delete { uri, .. } => {
						results.push(format!("DELETE: {}", format_path_relative_to_cwd(&uri_to_file(uri), cwd)));
					}
				},
			}
		}
	}
	results
}

#[must_use]
pub fn format_text_edit(edit: &TextEdit, max_length: usize) -> String {
	let position = format!("{}:{}", edit.range.start.line + 1, edit.range.start.character + 1);
	let preview = if edit.new_text.chars().count() > max_length {
		format!(
			"{}…",
			edit.new_text.chars().take(max_length).collect::<String>().replace('\n', "\\n")
		)
	} else {
		edit.new_text.replace('\n', "\\n")
	};
	format!("line {position} \"{preview}\"")
}

#[must_use]
pub fn format_code_action(action: &CodeActionOrCommand, index: usize) -> String {
	match action {
		CodeActionOrCommand::Command(command) => format!("{index}: [action] {}", command.title),
		CodeActionOrCommand::Action(action) => {
			let kind = action.kind.as_deref().unwrap_or("action");
			let preferred = if action.is_preferred.unwrap_or(false) { " (preferred)" } else { "" };
			let disabled = action
				.disabled
				.as_ref()
				.map(|disabled| format!(" (disabled: {})", disabled.reason))
				.unwrap_or_default();
			format!("{index}: [{kind}] {}{preferred}{disabled}", action.title)
		}
	}
}

// =============================================================================
// Symbols
// =============================================================================

#[must_use]
pub fn symbol_kind_name(kind: u8) -> &'static str {
	match kind {
		1 => "File",
		2 => "Module",
		3 => "Namespace",
		4 => "Package",
		5 => "Class",
		6 => "Method",
		7 => "Property",
		8 => "Field",
		9 => "Constructor",
		10 => "Enum",
		11 => "Interface",
		12 => "Function",
		13 => "Variable",
		14 => "Constant",
		15 => "String",
		16 => "Number",
		17 => "Boolean",
		18 => "Array",
		19 => "Object",
		20 => "Key",
		21 => "Null",
		22 => "EnumMember",
		23 => "Struct",
		24 => "Event",
		25 => "Operator",
		26 => "TypeParameter",
		_ => "Unknown",
	}
}

#[must_use]
pub fn format_document_symbol(symbol: &DocumentSymbol, indent: usize) -> Vec<String> {
	let prefix = "  ".repeat(indent);
	let detail = symbol.detail.as_ref().map(|detail| format!(" {detail}")).unwrap_or_default();
	let mut lines = vec![format!(
		"{prefix}{} {}{detail} @ line {}",
		symbol_kind_name(symbol.kind),
		symbol.name,
		symbol.range.start.line + 1
	)];
	if let Some(children) = &symbol.children {
		for child in children {
			lines.extend(format_document_symbol(child, indent + 1));
		}
	}
	lines
}

#[must_use]
pub fn format_symbol_information(symbol: &SymbolInformation, cwd: &Path) -> String {
	let container = symbol
		.container_name
		.as_ref()
		.map(|name| format!(" ({name})"))
		.unwrap_or_default();
	format!(
		"{} {}{container} @ {}",
		symbol_kind_name(symbol.kind),
		symbol.name,
		format_location(&symbol.location, cwd)
	)
}

#[must_use]
pub fn filter_workspace_symbols(symbols: Vec<SymbolInformation>, query: &str) -> Vec<SymbolInformation> {
	let needle = query.trim().to_ascii_lowercase();
	if needle.is_empty() {
		return symbols;
	}
	symbols
		.into_iter()
		.filter(|symbol| {
			symbol.name.to_ascii_lowercase().contains(&needle)
				|| symbol
					.container_name
					.as_ref()
					.is_some_and(|container| container.to_ascii_lowercase().contains(&needle))
				|| uri_to_file(&symbol.location.uri)
					.to_string_lossy()
					.to_ascii_lowercase()
					.contains(&needle)
		})
		.collect()
}

#[must_use]
pub fn dedupe_workspace_symbols(symbols: Vec<SymbolInformation>) -> Vec<SymbolInformation> {
	let mut seen = HashSet::new();
	let mut unique = Vec::new();
	for symbol in symbols {
		let key = format!(
			"{}:{}:{}:{}:{}:{}",
			symbol.name,
			symbol.container_name.as_deref().unwrap_or(""),
			symbol.kind,
			symbol.location.uri,
			symbol.location.range.start.line,
			symbol.location.range.start.character
		);
		if seen.insert(key) {
			unique.push(symbol);
		}
	}
	unique
}

// =============================================================================
// Symbol column resolution and context
// =============================================================================

fn first_non_whitespace_column(line: &str) -> u32 {
	let byte_index = line.char_indices().find(|(_, ch)| !ch.is_whitespace()).map(|(index, _)| index).unwrap_or(0);
	line[..byte_index].encode_utf16().count() as u32
}

fn is_identifier_char(ch: char) -> bool {
	ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'
}

fn find_symbol_match_indexes(line: &str, symbol: &str, case_insensitive: bool) -> Vec<u32> {
	if symbol.is_empty() {
		return Vec::new();
	}
	let haystack = if case_insensitive { line.to_ascii_lowercase() } else { line.to_string() };
	let needle = if case_insensitive { symbol.to_ascii_lowercase() } else { symbol.to_string() };
	let require_boundary = {
		let mut chars = symbol.chars();
		let first = chars.next();
		first.is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_' || ch == '$')
			&& chars.all(is_identifier_char)
	};
	let mut indexes = Vec::new();
	let mut cursor = 0;
	while cursor <= haystack.len() {
		let Some(found) = haystack[cursor..].find(&needle) else {
			break;
		};
		let index = cursor + found;
		if require_boundary {
			let before = haystack[..index].chars().next_back();
			let after = haystack[index + needle.len()..].chars().next();
			if before.is_some_and(is_identifier_char) || after.is_some_and(is_identifier_char) {
				// Advance one whole character: the rejected match may start on a
				// multi-byte symbol, so `index + 1` could split the same char.
				cursor = index + haystack[index..].chars().next().map_or(1, char::len_utf8);
				continue;
			}
		}
		// Column is measured in UTF-16 code units by LSP. `haystack` shares char
		// boundaries with `line` (ASCII-only folding), so slicing the haystack
		// keeps the conversion valid for non-ASCII prefixes.
		let column = haystack[..index].encode_utf16().count() as u32;
		indexes.push(column);
		cursor = index + needle.len();
	}
	indexes
}

fn parse_symbol_spec(spec: &str) -> (String, usize) {
	if let Some((symbol, occurrence)) = spec.rsplit_once('#') {
		if !symbol.is_empty() {
			if let Ok(occurrence) = occurrence.parse::<usize>() {
				return (symbol.to_string(), occurrence.max(1));
			}
		}
	}
	(spec.to_string(), 1)
}

/// Resolve a 0-based character offset on `line` (1-indexed) for `symbol`.
pub async fn resolve_symbol_column(
	file_path: &Path,
	line: u32,
	symbol: Option<&str>,
) -> Result<u32, String> {
	let line_number = line.max(1);
	let file_text = tokio::fs::read_to_string(file_path)
		.await
		.map_err(|error| {
			if error.kind() == std::io::ErrorKind::NotFound {
				format!("File not found: {}", file_path.display())
			} else {
				error.to_string()
			}
		})?;
	let target_line = file_text.split('\n').nth(line_number as usize - 1).unwrap_or("");
	let Some(symbol) = symbol.filter(|symbol| !symbol.is_empty()) else {
		return Ok(first_non_whitespace_column(target_line));
	};
	let (symbol, occurrence) = parse_symbol_spec(symbol);
	let exact = find_symbol_match_indexes(target_line, &symbol, false);
	let matches = if exact.is_empty() {
		find_symbol_match_indexes(target_line, &symbol, true)
	} else {
		exact
	};
	if matches.is_empty() {
		return Err(format!("Symbol \"{symbol}\" not found on line {line_number}"));
	}
	if occurrence > matches.len() {
		return Err(format!(
			"Symbol \"{symbol}\" occurrence {occurrence} is out of bounds on line {line_number} (found {})",
			matches.len()
		));
	}
	Ok(matches[occurrence - 1])
}

/// Read `target_line` plus `context_lines` above/below, each prefixed with its
/// 1-indexed line number.
pub async fn read_location_context(
	file_path: &Path,
	line: u32,
	context_lines: u32,
) -> Vec<String> {
	let Ok(text) = tokio::fs::read_to_string(file_path).await else {
		return Vec::new();
	};
	let lines: Vec<&str> = text.split('\n').collect();
	if lines.is_empty() {
		return Vec::new();
	}
	let target = line.max(1) as usize;
	let start = target.saturating_sub(context_lines as usize).max(1);
	let end = (target + context_lines as usize).min(lines.len());
	(start..=end)
		.map(|current| format!("{current}: {}", lines[current - 1]))
		.collect()
}

// =============================================================================
// Hover
// =============================================================================

#[must_use]
pub fn extract_hover_text(contents: &Value) -> String {
	match contents {
		Value::String(text) => text.clone(),
		Value::Array(items) => items
			.iter()
			.map(extract_hover_text)
			.collect::<Vec<_>>()
			.join("\n\n"),
		Value::Object(object) => {
			if let Some(Value::String(value)) = object.get("value") {
				value.clone()
			} else if let Some(Value::String(kind)) = object.get("kind") {
				kind.clone()
			} else {
				contents.to_string()
			}
		}
		other => other.to_string(),
	}
}

// =============================================================================
// Glob expansion
// =============================================================================

#[must_use]
pub fn has_glob_pattern(value: &str) -> bool {
	value.contains('*') || value.contains('?') || value.contains('[') || value.contains('{')
}

/// Expand a glob relative to `cwd`, returning at most `max_matches` relative
/// paths and whether more matches existed.
pub async fn collect_glob_matches(
	pattern: &str,
	cwd: &Path,
	max_matches: usize,
) -> Result<(Vec<String>, bool), String> {
	let limit = max_matches.max(1);
	let pattern = pattern.to_string();
	let root = cwd.to_path_buf();
	tokio::task::spawn_blocking(move || {
		let glob = pi_walker::CompiledWalkGlob::new([pattern]).map_err(|error| error.to_string())?;
		let request = pi_walker::WalkRequest::new(root.clone())
			.hidden(true)
			.gitignore(true)
			.skip_git(true)
			.skip_node_modules(false)
			.follow_links(pi_walker::FollowLinks::Never)
			.detail(pi_walker::WalkDetail::Minimal)
			.order(pi_walker::WalkOrder::Path)
			.emit_root(false)
			.depth(1, usize::MAX)
			.directory_errors(pi_walker::DirectoryErrorMode::SkipSkippable)
			.filter(pi_walker::WalkFilter::files_only().glob(glob))
			.limit(limit + 1);
		let outcome = request.collect().map_err(|error| error.to_string())?;
		let mut matches: Vec<String> = outcome.entries.into_iter().map(|entry| entry.path).collect();
		let truncated = matches.len() > limit;
		matches.truncate(limit);
		Ok((matches, truncated))
	})
	.await
	.map_err(|error| error.to_string())?
}

/// Resolve a diagnostics target: a concrete path is returned unchanged; a glob
/// is expanded to files relative to `cwd`.
pub async fn resolve_diagnostic_targets(
	file: &str,
	cwd: &Path,
	max_matches: usize,
) -> Result<(Vec<String>, bool), String> {
	if !has_glob_pattern(file) {
		return Ok((vec![file.to_string()], false));
	}
	let resolved = resolve_to_cwd(file, cwd);
	if tokio::fs::metadata(&resolved).await.is_ok_and(|metadata| metadata.is_file()) {
		return Ok((vec![file.to_string()], false));
	}
	collect_glob_matches(file, cwd, max_matches).await
}

/// Convenience for tests and callers that only need a `CodeAction` title.
#[must_use]
pub fn code_action_title(action: &CodeAction) -> &str {
	&action.title
}

#[cfg(test)]
mod tests {
	use super::find_symbol_match_indexes;

	fn utf16_prefix(line: &str, needle: &str) -> u32 {
		let index = line.find(needle).expect("needle present");
		utf16_at(line, index)
	}

	fn utf16_at(line: &str, index: usize) -> u32 {
		u32::try_from(line[..index].encode_utf16().count()).expect("column fits u32")
	}

	#[test]
	fn symbol_column_counts_utf16_after_non_ascii_prefix() {
		let line = "// héllo wörld target = target";
		let second = line.rfind("target").expect("second match");
		let indexes = find_symbol_match_indexes(line, "target", false);
		assert_eq!(indexes, vec![utf16_prefix(line, "target"), utf16_at(line, second)]);
	}

	#[test]
	fn symbol_column_handles_case_insensitive_non_ascii_prefix() {
		let line = "äö Alpha alpha";
		let indexes = find_symbol_match_indexes(line, "alpha", true);
		assert_eq!(indexes, vec![utf16_prefix(line, "Alpha"), utf16_prefix(line, "alpha")]);
	}

	#[test]
	fn symbol_scan_never_slices_inside_multibyte_chars() {
		assert_eq!(find_symbol_match_indexes("é x", "x", false), vec![2]);
		// A word-boundary rejection must advance a whole char, not one byte.
		assert_eq!(find_symbol_match_indexes("éa a", "a", false), vec![1, 3]);
	}
}
