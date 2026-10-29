//! The `ast_grep` tool: structural search over tree-sitter ASTs.
//!
//! Mirrors `packages/coding-agent/src/tools/ast-grep.ts`: one `pat` is compiled
//! for every language discovered among the candidates, matches are merged
//! across semicolon-delimited targets, sorted deterministically, then paged with
//! `skip` and the fixed 50-match limit. The result reports total matches,
//! distinct matching files, files searched, whether the window was truncated,
//! and per-file parse issues.
//!
//! Parsing, pattern compilation, and matching all come from `pi-ast`; directory
//! discovery comes from `pi-walker`. The tool is read-only and never writes.

use std::{
	collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet},
	path::{Path, PathBuf},
	sync::Arc,
};

use async_trait::async_trait;
use pi_ast::ops::{
	AstMatch, Pattern, SourceAst, collect_matches_in, compile_search_patterns,
	is_supported_file, parse_source, resolve_language, tree_has_parse_error,
};
use pi_walker::{
	CompiledWalkGlob, DirectoryErrorMode, EmptyRecheck, FollowLinks, WalkDetail, WalkError,
	WalkFilter, WalkOrder, WalkRequest,
};
use serde_json::{Value, json};

use super::{
	ParsedFindPattern, Tool, ToolError, extract_raw_paths, fix_unclosed_braces,
	format_path_relative_to_cwd, has_glob_path_chars, is_root_path, normalize_path,
	normalize_path_like_input, parse_find_pattern,
	read::{FileDisplayInputs, ReadSnapshotSink, normalize_snapshot_text},
	resolve_to_cwd, split_top_level_delimited, tool_result,
};
use crate::state::{AgentContext, ToolResult};

/// Upstream `DEFAULT_AST_LIMIT`.
const DEFAULT_AST_LIMIT: usize = 50;
/// Upstream `capParseErrors` default.
const MAX_PARSE_ERRORS: usize = 20;

/// Builtin structural search tool.
///
/// `sink` is the session hashline store seam, used only when the resolved
/// display mode selects hashline; `display` carries the session inputs
/// (`edit.mode`, whether `edit` is mounted, `readLineNumbers`). No inputs keeps
/// the historical plain `*LINE|text` output.
pub struct AstGrepTool {
	sink:    Option<Arc<dyn ReadSnapshotSink>>,
	display: Option<FileDisplayInputs>,
}

impl Default for AstGrepTool {
	fn default() -> Self {
		Self::new()
	}
}

impl AstGrepTool {
	pub fn new() -> Self {
		Self { sink: None, display: None }
	}

	/// Search with hashline tags available when the display mode selects them.
	pub fn with_snapshot_sink(sink: Arc<dyn ReadSnapshotSink>) -> Self {
		Self { sink: Some(sink), display: None }
	}

	/// Opt into resolver-driven display; without it the tool stays plain.
	pub fn with_display_inputs(mut self, display: FileDisplayInputs) -> Self {
		self.display = Some(display);
		self
	}

	/// Matches are always mutable and never `:raw`, so only the session inputs
	/// vary. Without explicit inputs the historical plain output is kept.
	fn hash_lines(&self) -> bool {
		match self.display {
			Some(inputs) => inputs.resolve(false, false).hash_lines,
			None => false,
		}
	}
}

#[async_trait]
impl Tool for AstGrepTool {
	fn name(&self) -> &str {
		"ast_grep"
	}

	fn description(&self) -> &str {
		"Search code with AST patterns (structural grep)"
	}

	fn schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"pat": {
					"type": "string",
					"description": "ast pattern"
				},
				"path": {
					"type": "string",
					"description": "file, directory, glob, or local file:// URL to search; pass several as a semicolon-delimited list (\"src; tests\"). Omitted -> searches the workspace root (\".\")"
				},
				"skip": {
					"type": "number",
					"description": "matches to skip"
				}
			},
			"required": ["pat"]
		})
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		run_ast_grep(self, args, ctx)
	}
}

pub(super) struct Target {
	pub(super) base:                 PathBuf,
	pub(super) glob:                 Option<String>,
	pub(super) is_file:              bool,
	pub(super) mentions_node_modules: bool,
}

#[derive(Clone)]
pub(super) struct Candidate {
	pub(super) absolute: PathBuf,
	pub(super) display:  String,
}

/// Deterministic match ordering, identical to upstream
/// `compareAstFindMatch`.
#[derive(Clone, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
struct MatchKey {
	path:         String,
	start_line:   usize,
	start_column: usize,
	end_line:     usize,
	end_column:   usize,
	byte_start:   usize,
	byte_end:     usize,
}

struct RetainedMatch {
	key:  MatchKey,
	text: String,
	meta: BTreeMap<String, String>,
}

impl Ord for RetainedMatch {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		self.key.cmp(&other.key)
	}
}

impl PartialOrd for RetainedMatch {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
		Some(self.cmp(other))
	}
}

impl Eq for RetainedMatch {}

impl PartialEq for RetainedMatch {
	fn eq(&self, other: &Self) -> bool {
		self.key == other.key
	}
}

fn run_ast_grep(tool: &AstGrepTool, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
	if ctx.cancellation_token.is_cancelled() {
		return Ok(tool_result("AST grep cancelled", true));
	}

	let pattern = args
		.get("pat")
		.and_then(Value::as_str)
		.map(str::trim)
		.ok_or_else(|| ToolError::InvalidArguments("missing 'pat' parameter".to_string()))?;
	if pattern.is_empty() {
		return Err(ToolError::InvalidArguments(
			"`pat` must be a non-empty pattern".to_string(),
		));
	}
	let pattern = pattern.to_string();

	let skip = match args.get("skip") {
		None | Some(Value::Null) => 0usize,
		Some(value) => {
			let number = value.as_f64().ok_or_else(|| {
				ToolError::InvalidArguments("skip must be a non-negative number".to_string())
			})?;
			if !number.is_finite() || number < 0.0 {
				return Err(ToolError::InvalidArguments(
					"skip must be a non-negative number".to_string(),
				));
			}
			number.floor() as usize
		},
	};

	let (targets, missing, scope_path) = plan_targets(args, &ctx.cwd)?;

	let Some(candidates) = collect_candidates(&targets, ctx)? else {
		return Ok(tool_result("AST grep cancelled", true));
	};
	let files_searched = candidates.len();

	let capacity = skip.saturating_add(DEFAULT_AST_LIMIT).saturating_add(1);
	let mut retained: BinaryHeap<RetainedMatch> = BinaryHeap::new();
	let mut compiled: HashMap<&'static str, Result<Vec<Pattern>, String>> =
		HashMap::new();
	let mut parse_errors: Vec<String> = Vec::new();
	let mut files_with_matches: BTreeSet<String> = BTreeSet::new();
	let mut total_matches = 0usize;

	for candidate in &candidates {
		if ctx.cancellation_token.is_cancelled() {
			return Ok(tool_result("AST grep cancelled", true));
		}

		let language = match resolve_language(None, &candidate.absolute) {
			Ok(language) => language,
			Err(error) => {
				parse_errors.push(format!("{}: {error}", candidate.display));
				continue;
			},
		};

		let patterns = match compiled
			.entry(language.canonical_name())
			.or_insert_with(|| {
				compile_search_patterns(&pattern, language).map_err(|error| error.to_string())
			}) {
			Ok(patterns) => patterns,
			Err(error) => {
				parse_errors.push(format!("{pattern}: {}: {error}", candidate.display));
				continue;
			},
		};

		let source = match std::fs::read_to_string(&candidate.absolute) {
			Ok(source) => source,
			Err(error) => {
				parse_errors.push(format!("{pattern}: {}: {error}", candidate.display));
				continue;
			},
		};

		let ast = parse_source(&source, language);

		report_parse_error(&ast, &candidate.display, &mut parse_errors);

		let mut seen: HashSet<MatchKey> = HashSet::new();
		for matched in collect_deduped_matches(&ast, patterns.as_slice()) {
			if ctx.cancellation_token.is_cancelled() {
				return Ok(tool_result("AST grep cancelled", true));
			}
			let key = MatchKey {
				path:         candidate.display.clone(),
				start_line:   matched.line,
				start_column: matched.column,
				end_line:     matched.end_line,
				end_column:   matched.end_column,
				byte_start:   matched.byte_start,
				byte_end:     matched.byte_end,
			};
			if !seen.insert(key.clone()) {
				continue;
			}
			total_matches = total_matches.saturating_add(1);
			files_with_matches.insert(candidate.display.clone());
			if capacity == 0 {
				continue;
			}
			let candidate_match =
				RetainedMatch { key, text: matched.text, meta: matched.meta_variables };
			if retained.len() < capacity {
				retained.push(candidate_match);
			} else if retained.peek().is_some_and(|worst| candidate_match.key < worst.key) {
				retained.pop();
				retained.push(candidate_match);
			}
		}
	}

	let limit_reached = total_matches > skip.saturating_add(DEFAULT_AST_LIMIT);
	let sorted = retained.into_sorted_vec();
	let visible: Vec<RetainedMatch> =
		sorted.into_iter().skip(skip).take(DEFAULT_AST_LIMIT).collect();

	let mut matches_by_file: BTreeMap<String, Vec<&RetainedMatch>> = BTreeMap::new();
	for matched in &visible {
		matches_by_file.entry(matched.key.path.clone()).or_default().push(matched);
	}

	let parse_errors_total = parse_errors.len();
	let capped_parse_errors: Vec<String> =
		parse_errors.into_iter().take(MAX_PARSE_ERRORS).collect();

	let file_list: Vec<String> = matches_by_file.keys().cloned().collect();
	let file_matches: Vec<Value> = matches_by_file
		.iter()
		.map(|(path, matches)| json!({ "path": path, "count": matches.len() }))
		.collect();

	let mut details = json!({
		"matchCount": total_matches,
		"fileCount": files_with_matches.len(),
		"filesSearched": files_searched,
		"limitReached": limit_reached,
		"scopePath": scope_path,
		"searchPath": format_path_relative_to_cwd(&common_base(&targets), &ctx.cwd, false),
		"cwd": ctx.cwd.to_string_lossy().replace('\\', "/"),
		"files": file_list,
		"fileMatches": file_matches,
	});
	if !capped_parse_errors.is_empty() {
		if let Value::Object(map) = &mut details {
			map.insert("parseErrors".to_string(), json!(&capped_parse_errors));
			map.insert("parseErrorsTotal".to_string(), json!(parse_errors_total));
		}
	}

	if visible.is_empty() {
		let mut message = if total_matches == 0 {
			"No matches found".to_string()
		} else {
			format!(
				"No more results ({total_matches} matches total; skip={skip} is past the end)"
			)
		};
		append_parse_errors(&mut message, &capped_parse_errors, parse_errors_total);
		return Ok(ast_grep_result(message, details, false));
	}

	let grouped = targets.len() > 1 || targets.iter().any(|target| !target.is_file);

	// Hashline is per resolved display mode; a file that cannot be snapshotted
	// (over cap, unreadable, vanished) falls back to plain `|` lines.
	let absolute_by_display: BTreeMap<&str, &Path> = candidates
		.iter()
		.map(|candidate| (candidate.display.as_str(), candidate.absolute.as_path()))
		.collect();
	let sink = tool.sink.as_deref();
	let mut tags: BTreeMap<String, String> = BTreeMap::new();
	if tool.hash_lines() {
		if let Some(sink) = sink {
			for path in matches_by_file.keys() {
				let Some(absolute) = absolute_by_display.get(path.as_str()) else {
					continue;
				};
				let Ok(bytes) = std::fs::read(absolute) else {
					continue;
				};
				let text = normalize_snapshot_text(&bytes);
				if let Some(tag) = sink.record_file(absolute, &text) {
					tags.insert(path.clone(), tag);
				}
			}
		}
	}

	let mut output: Vec<String> = Vec::new();
	let mut display: Vec<String> = Vec::new();
	let render_file = |path: &str| -> RenderedMatches {
		let matches = matches_by_file.get(path).map(Vec::as_slice).unwrap_or(&[]);
		let hash_tag = tags.get(path).map(String::as_str);
		let rendered = render_matches(matches, hash_tag);
		if let (Some(sink), Some(tag), Some(absolute)) = (
			sink,
			hash_tag,
			absolute_by_display.get(path).copied(),
		) {
			if !rendered.seen.is_empty() {
				sink.record_seen_lines(absolute, tag, &rendered.seen);
			}
		}
		rendered
	};

	if grouped {
		let files: Vec<String> = matches_by_file.keys().cloned().collect();
		let rendered = format_grouped_files(&files, |path| {
			let file = render_file(path);
			GroupedSection {
				header_suffix: tags.get(path).map(|tag| format!("#{tag}")).unwrap_or_default(),
				skip:          file.model.is_empty(),
				model:         file.model,
				display:       file.display,
			}
		});
		output = rendered.model;
		display = rendered.display;
	} else {
		for path in matches_by_file.keys() {
			let file = render_file(path);
			if file.model.is_empty() {
				continue;
			}
			if !output.is_empty() {
				output.push(String::new());
				display.push(String::new());
			}
			if let (Some(sink), Some(tag), Some(absolute)) = (
				sink,
				tags.get(path),
				absolute_by_display.get(path.as_str()).copied(),
			) {
				output.push(sink.header(absolute, tag, &ctx.cwd));
			}
			output.extend(file.model);
			display.extend(file.display);
		}
	}
	if let Value::Object(map) = &mut details {
		map.insert("displayContent".to_string(), json!(display.join("\n")));
	}

	if limit_reached {
		output.push(String::new());
		output.push("Result limit reached; narrow path or increase limit.".to_string());
	}
	if !capped_parse_errors.is_empty() {
		output.push(String::new());
		output.push("Parse issues:".to_string());
		for error in &capped_parse_errors {
			output.push(format!("- {error}"));
		}
		if parse_errors_total > capped_parse_errors.len() {
			output.push(format!(
				"({} more parse issues)",
				parse_errors_total - capped_parse_errors.len()
			));
		}
	}
	if !missing.is_empty() {
		output.push(String::new());
		output.push(format!("Skipped missing paths: {}", missing.join(", ")));
	}

	Ok(ast_grep_result(output.join("\n"), details, false))
}

/// One file's match lines: the model text plus the TUI code-frame view.
struct RenderedMatches {
	model: Vec<String>,
	display: Vec<String>,
	seen: Vec<u32>,
}

/// Port of `match-line-format.ts` + the ast-grep renderer: under a hashline
/// tag lines use `*LINE:text`; otherwise the display-only `*LINE|text`.
/// Continuation lines keep a space marker so line numbers align. Frames are
/// mode independent.
fn render_matches(matches: &[&RetainedMatch], hash_tag: Option<&str>) -> RenderedMatches {
	let line_number_width = matches
		.iter()
		.map(|matched| {
			let count = matched.text.split('\n').count();
			let end_line = matched.key.start_line + count - 1;
			matched.key.start_line.to_string().len().max(end_line.to_string().len())
		})
		.max()
		.unwrap_or(0);
	let hash = hash_tag.is_some();
	let mut rendered = RenderedMatches { model: Vec::new(), display: Vec::new(), seen: Vec::new() };
	for matched in matches {
		for (offset, line) in matched.text.split('\n').enumerate() {
			let line_number = matched.key.start_line + offset;
			let is_match = offset == 0;
			let marker = if is_match { '*' } else { ' ' };
			let separator = if hash { ':' } else { '|' };
			rendered.model.push(format!("{marker}{line_number}{separator}{line}"));
			rendered.display.push(code_frame_line(
				marker,
				line_number,
				line,
				line_number_width,
			));
			if let Ok(number) = u32::try_from(line_number) {
				rendered.seen.push(number);
			}
		}
		if !matched.meta.is_empty() {
			let serialized = matched
				.meta
				.iter()
				.map(|(key, value)| format!("{key}={value}"))
				.collect::<Vec<_>>()
				.join(", ");
			rendered.model.push(format!("  meta: {serialized}"));
			rendered.display.push(format!("  meta: {serialized}"));
		}
	}
	rendered
}

/// `-LINE│text` / `*LINE│text` with the gutter padded to the widest line
/// number (`formatCodeFrameLine` in `render-utils.ts`).
pub(super) fn code_frame_line(marker: char, line: usize, content: &str, line_number_width: usize) -> String {
	let gutter = format!("{marker}{line}");
	let padded = pad_start(&gutter, line_number_width + 1);
	format!("{padded}│{content}")
}

pub(super) fn pad_start(text: &str, width: usize) -> String {
	let length = text.chars().count();
	if length >= width {
		text.to_string()
	} else {
		format!("{}{text}", " ".repeat(width - length))
	}
}

/// One file's contribution to [`format_grouped_files`].
pub(super) struct GroupedSection {
	/// Extra text appended to the `## name` header, e.g. `#TAG` or
	/// ` (1 replacement)`.
	pub header_suffix: String,
	pub model:         Vec<String>,
	pub display:       Vec<String>,
	/// When true the file and its header are omitted entirely.
	pub skip:          bool,
}

pub(super) struct GroupedOutput {
	pub model:   Vec<String>,
	pub display: Vec<String>,
}

/// Port of `formatGroupedFiles` in `grouped-file-output.ts`: one `#` per
/// nesting level, single-child directory chains folded into one header, a
/// blank line before every directory header and root-level file.
pub(super) fn format_grouped_files<F>(files: &[String], mut render: F) -> GroupedOutput
where
	F: FnMut(&str) -> GroupedSection,
{
	let mut sections: BTreeMap<String, GroupedSection> = BTreeMap::new();
	let mut included: Vec<&str> = Vec::new();
	for path in files {
		if sections.contains_key(path) {
			continue;
		}
		let section = render(path);
		if section.skip {
			continue;
		}
		included.push(path.as_str());
		sections.insert(path.clone(), section);
	}
	let tree = build_path_tree(included);
	let mut output = GroupedOutput { model: Vec::new(), display: Vec::new() };
	let mut emitted = false;
	walk_path_tree(&tree, 0, &mut emitted, &sections, &mut output);
	output
}

/// Port of `buildPathTree` in `packages/utils/src/path-tree.ts`: first-seen
/// child order, file names deduped per node, URL-like entries kept whole.
#[derive(Default)]
struct PathTreeNode {
	files:   Vec<(String, String)>,
	subdirs: Vec<(String, PathTreeNode)>,
}

fn build_path_tree<'a>(paths: impl IntoIterator<Item = &'a str>) -> PathTreeNode {
	let mut root = PathTreeNode::default();
	for raw in paths {
		let normalized = raw.replace('\\', "/");
		if is_url_like_path(&normalized) {
			add_tree_file(&mut root, &normalized, raw);
			continue;
		}
		let trimmed = normalized.trim_end_matches('/');
		if trimmed.is_empty() {
			continue;
		}
		let segments: Vec<&str> = trimmed.split('/').collect();
		let mut node = &mut root;
		for segment in &segments[..segments.len() - 1] {
			let index = match node.subdirs.iter().position(|(name, _)| name == segment) {
				Some(index) => index,
				None => {
					node.subdirs.push(((*segment).to_string(), PathTreeNode::default()));
					node.subdirs.len() - 1
				},
			};
			node = &mut node.subdirs[index].1;
		}
		add_tree_file(node, segments[segments.len() - 1], raw);
	}
	root
}

fn add_tree_file(node: &mut PathTreeNode, name: &str, path: &str) {
	if node.files.iter().any(|(existing, _)| existing == name) {
		return;
	}
	node.files.push((name.to_string(), path.to_string()));
}

fn is_url_like_path(path: &str) -> bool {
	let Some((scheme, _)) = path.split_once("://") else {
		return false;
	};
	let mut chars = scheme.chars();
	matches!(chars.next(), Some(first) if first.is_ascii_alphabetic())
		&& chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
}

fn walk_path_tree(
	node: &PathTreeNode,
	depth: usize,
	emitted: &mut bool,
	sections: &BTreeMap<String, GroupedSection>,
	output: &mut GroupedOutput,
) {
	for (file_name, path) in &node.files {
		let Some(section) = sections.get(path) else {
			continue;
		};
		if *emitted && depth == 0 {
			output.model.push(String::new());
			output.display.push(String::new());
		}
		*emitted = true;
		let header = format!("{} {file_name}{}", "#".repeat(depth + 1), section.header_suffix);
		output.model.push(header.clone());
		output.model.extend(section.model.iter().cloned());
		output.display.push(header);
		if section.display.is_empty() {
			output.display.extend(section.model.iter().cloned());
		} else {
			output.display.extend(section.display.iter().cloned());
		}
	}
	for (dir_name, subdir) in &node.subdirs {
		let mut dir_node = subdir;
		let mut parts = vec![dir_name.clone()];
		while dir_node.files.is_empty() && dir_node.subdirs.len() == 1 {
			let only = &dir_node.subdirs[0];
			parts.push(only.0.clone());
			dir_node = &only.1;
		}
		if *emitted {
			output.model.push(String::new());
			output.display.push(String::new());
		}
		*emitted = true;
		let header = format!("{} {}/", "#".repeat(depth + 1), parts.join("/"));
		output.model.push(header.clone());
		output.display.push(header);
		walk_path_tree(dir_node, depth + 1, emitted, sections, output);
	}
}

/// `compile_search_patterns` adds Rust's contextual fallback on top of the
/// primary pattern. Both can describe the same source span (a call and its
/// expression statement), so later patterns only contribute matches whose
/// start position the primary pattern did not already cover. Exact duplicate
/// ranges are still filtered by the caller.
fn collect_deduped_matches(ast: &SourceAst, patterns: &[Pattern]) -> Vec<AstMatch> {
	let Some((first, rest)) = patterns.split_first() else {
		return Vec::new();
	};
	let mut matches = collect_matches_in(ast, std::slice::from_ref(first));
	let mut starts: HashSet<(usize, usize)> =
		matches.iter().map(|matched| (matched.line, matched.column)).collect();
	for pattern in rest {
		for matched in collect_matches_in(ast, std::slice::from_ref(pattern)) {
			if starts.insert((matched.line, matched.column)) {
				matches.push(matched);
			}
		}
	}
	matches
}

fn ast_grep_result(content: String, details: Value, is_error: bool) -> ToolResult {
	ToolResult {
		tool_call_id: String::new(),
		content,
		is_error,
		terminate: false,
		details: Some(details),
	}
}

fn append_parse_errors(message: &mut String, errors: &[String], total: usize) {
	if errors.is_empty() {
		return;
	}
	message.push_str("\nParse issues:");
	for error in errors {
		message.push_str("\n- ");
		message.push_str(error);
	}
	if total > errors.len() {
		message.push_str(&format!("\n({} more parse issues)", total - errors.len()));
	}
}

/// Detect tree-sitter error nodes on the tree the matcher already parsed: one
/// parse per file, and no cache entry is written for a tree nobody reuses.
fn report_parse_error(ast: &SourceAst, display: &str, errors: &mut Vec<String>) {
	if tree_has_parse_error(ast) {
		errors.push(format!(
			"{display}: parse error (syntax tree contains error nodes)"
		));
	}
}

pub(super) fn plan_targets(
	args: &Value,
	cwd: &Path,
) -> Result<(Vec<Target>, Vec<String>, String), ToolError> {
	plan_targets_with(args, cwd, false)
}

/// Plan targets with per-entry `parseSearchPath` semantics: every expanded
/// glob keeps its leading segment (`*.ts` stays top-level). `ast_edit` uses
/// this for its `paths` array so each entry keeps single-path semantics.
pub(super) fn plan_targets_single(
	args: &Value,
	cwd: &Path,
) -> Result<(Vec<Target>, Vec<String>, String), ToolError> {
	plan_targets_with(args, cwd, true)
}

fn plan_targets_with(
	args: &Value,
	cwd: &Path,
	single_glob_semantics: bool,
) -> Result<(Vec<Target>, Vec<String>, String), ToolError> {
	let mut normalized_inputs: Vec<(String, Vec<String>)> = Vec::new();
	for raw in extract_raw_paths(args) {
		let normalized = normalize_path_like_input(&raw);
		if normalized.is_empty() {
			return Err(ToolError::InvalidArguments(
				"`path` must contain non-empty globs or paths".to_string(),
			));
		}
		// Resolve/validate every entry before any filesystem probe: this port
		// has no internal-URL or remote resolver, so a scheme must never
		// degrade into a relative path, and a mixed list (`"src; ssh://box/x"`)
		// fails as a whole even when `src` exists.
		let parts = split_top_level_delimited(&normalized)
			.into_iter()
			.map(|part| {
				if is_non_local_scheme(&part) {
					Err(ToolError::InvalidArguments(format!(
						"Unsupported target: {part} (only local filesystem paths are supported)"
					)))
				} else {
					file_url_to_path(&part).unwrap_or(Ok(part))
						.map_err(ToolError::InvalidArguments)
				}
			})
			.collect::<Result<Vec<_>, _>>()?;
		normalized_inputs.push((normalized, parts));
	}

	let mut expanded: Vec<String> = Vec::new();
	for (normalized, parts) in normalized_inputs {
		let literal = resolve_to_cwd(&normalized, cwd);
		if literal.exists() {
			expanded.push(normalized);
		} else {
			expanded.extend(parts);
		}
	}
	if expanded.is_empty() {
		return Err(ToolError::InvalidArguments(
			"`path` must contain non-empty globs or paths".to_string(),
		));
	}
	if expanded.iter().any(|item| item.chars().all(|character| character == '/')) {
		return Err(ToolError::InvalidArguments(
			"Searching from root directory '/' is not allowed".to_string(),
		));
	}

	let scope_path = expanded
		.iter()
		.map(|item| {
			let resolved = resolve_to_cwd(item, cwd);
			format_path_relative_to_cwd(&resolved, cwd, item.ends_with('/'))
		})
		.collect::<Vec<_>>()
		.join(", ");

	let is_single = single_glob_semantics || expanded.len() == 1;
	let mut targets: Vec<Target> = Vec::new();
	let mut missing: Vec<String> = Vec::new();

	for item in &expanded {
		let literal = resolve_to_cwd(item, cwd);
		if is_root_path(&literal) {
			return Err(ToolError::InvalidArguments(
				"Searching from root directory '/' is not allowed".to_string(),
			));
		}
		if literal.exists() {
			let metadata = std::fs::metadata(&literal).map_err(|error| {
				ToolError::ExecutionFailed(format!("Failed to inspect {}: {error}", literal.display()))
			})?;
			if metadata.is_file() {
				targets.push(Target {
					base: literal,
					glob: None,
					is_file: true,
					mentions_node_modules: false,
				});
			} else if metadata.is_dir() {
				targets.push(Target {
					base: literal,
					glob: None,
					is_file: false,
					mentions_node_modules: false,
				});
			} else {
				return Err(ToolError::InvalidArguments(format!(
					"Path must be a regular file or directory: {item}"
				)));
			}
			continue;
		}

		// Single targets mirror upstream `parseSearchPath`: a leading glob stays
		// as written (`*.rs` is top-level only). Multi-target lists keep
		// `parse_find_pattern`, which makes such a glob recursive.
		let parsed = if is_single {
			parse_single_target(item)
		} else {
			parse_find_pattern(item)
		};
		let ParsedFindPattern { base_path, glob_pattern, has_glob } = parsed;
		let base = resolve_to_cwd(&base_path, cwd);
		if !base.exists() {
			missing.push(item.clone());
			continue;
		}
		if is_root_path(&base) {
			return Err(ToolError::InvalidArguments(
				"Searching from root directory '/' is not allowed".to_string(),
			));
		}
		if !has_glob || !base.is_dir() {
			missing.push(item.clone());
			continue;
		}
		let mentions_node_modules = glob_pattern.contains("node_modules");
		targets.push(Target {
			base,
			glob: Some(fix_unclosed_braces(glob_pattern)),
			is_file: false,
			mentions_node_modules,
		});
	}

	if targets.is_empty() {
		return Err(ToolError::InvalidArguments(format!(
			"Path not found: {}",
			missing.join(", ")
		)));
	}

	Ok((targets, missing, scope_path))
}

/// Single-target glob parsing, mirroring upstream `parseSearchPath`: a leading
/// glob is used as written, so `*.rs` matches only the search root.
/// Multi-target lists keep [`parse_find_pattern`], which widens such globs
/// recursively.
fn parse_single_target(item: &str) -> ParsedFindPattern {
	let normalized = item.replace('\\', "/");
	let segments: Vec<&str> = normalized.split('/').collect();
	let Some(index) = segments
		.iter()
		.position(|segment| has_glob_path_chars(segment))
	else {
		return ParsedFindPattern {
			base_path:    normalized,
			glob_pattern: "**/*".to_string(),
			has_glob:     false,
		};
	};
	if index == 0 {
		return ParsedFindPattern {
			base_path:    ".".to_string(),
			glob_pattern: normalized,
			has_glob:     true,
		};
	}
	let raw_base = segments[..index].join("/");
	// Same convention as `parse_find_pattern`: an absolute glob whose literal
	// prefix is empty (`/*.rs`) has base `/`, so the root guard rejects it
	// instead of silently searching the cwd.
	let base_path = if raw_base.is_empty() {
		if normalized.starts_with('/') {
			"/".to_string()
		} else {
			".".to_string()
		}
	} else {
		raw_base
	};
	ParsedFindPattern {
		base_path,
		glob_pattern: segments[index..].join("/"),
		has_glob: true,
	}
}

/// Map a local `file://` target to its filesystem path.
///
/// `None` when `item` is not a file URL. `Some(Err(..))` for file URLs this
/// port cannot map locally: a non-local host, a missing absolute path, or a
/// malformed percent escape. `file:///abs` and `file://localhost/abs` decode;
/// percent escapes are decoded as UTF-8, matching upstream `fileURLToPath`.
fn file_url_to_path(item: &str) -> Option<Result<String, String>> {
	let (scheme, rest) = item.split_once("://")?;
	if !scheme.eq_ignore_ascii_case("file") {
		return None;
	}
	let (authority, path) = match rest.split_once('/') {
		Some((authority, tail)) => (authority, format!("/{tail}")),
		None => (rest, String::new()),
	};
	if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
		return Some(Err(format!(
			"Unsupported file URL host in {item} (only local file:/// paths are supported)"
		)));
	}
	if path.is_empty() {
		return Some(Err(format!("file URL has no absolute path: {item}")));
	}
	Some(
		percent_decode(&path)
			.ok_or_else(|| format!("Invalid percent escape in file URL: {item}")),
	)
}

fn percent_decode(value: &str) -> Option<String> {
	let bytes = value.as_bytes();
	let mut decoded = Vec::with_capacity(bytes.len());
	let mut index = 0;
	while index < bytes.len() {
		if bytes[index] == b'%' {
			let hex = bytes.get(index + 1..index + 3)?;
			if !hex.iter().all(u8::is_ascii_hexdigit) {
				return None;
			}
			let byte = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
			decoded.push(byte);
			index += 3;
		} else {
			decoded.push(bytes[index]);
			index += 1;
		}
	}
	String::from_utf8(decoded).ok()
}

/// Whether a target uses a URL scheme this port cannot resolve.
///
/// `file://` is a local path and is resolved by [`file_url_to_path`]; every
/// other scheme (`ssh://`, `local://`, `skill://`, `https://`, ...) is
/// unsupported here because it has no local backing path in this port.
/// Single-letter schemes are treated as Windows drive paths, not URLs.
fn is_non_local_scheme(item: &str) -> bool {
	let Some((scheme, _)) = item.split_once("://") else {
		return false;
	};
	let bytes = scheme.as_bytes();
	bytes.len() >= 2
		&& bytes[0].is_ascii_alphabetic()
		&& bytes
			.iter()
			.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'.' | b'-'))
		&& !scheme.eq_ignore_ascii_case("file")
}

pub(super) fn collect_candidates(
	targets: &[Target],
	ctx: &AgentContext,
) -> Result<Option<Vec<Candidate>>, ToolError> {
	let mut candidates: Vec<Candidate> = Vec::new();
	let mut seen: HashSet<PathBuf> = HashSet::new();

	for target in targets {
		if ctx.cancellation_token.is_cancelled() {
			return Ok(None);
		}

		if target.is_file {
			if !is_supported_file(&target.base, None) {
				let message = resolve_language(None, &target.base)
					.err()
					.map(|error| error.to_string())
					.unwrap_or_else(|| {
						format!("Unsupported file type: {}", target.base.display())
					});
				return Err(ToolError::InvalidArguments(message));
			}
			let absolute = normalize_path(&target.base);
			if seen.insert(absolute.clone()) {
				candidates.push(Candidate {
					display: format_path_relative_to_cwd(&absolute, &ctx.cwd, false),
					absolute,
				});
			}
			continue;
		}

		let mut filter =
			WalkFilter::files_only().node_modules_unless_mentioned(target.mentions_node_modules);
		if let Some(glob) = target.glob.as_deref() {
			let compiled = CompiledWalkGlob::new([glob.to_string()]).map_err(|error| {
				ToolError::InvalidArguments(format!("Invalid glob pattern: {error}"))
			})?;
			filter = filter.glob(compiled);
		}

		let request = WalkRequest::new(target.base.clone())
			.hidden(true)
			.gitignore(true)
			.skip_git(true)
			.follow_links(FollowLinks::Never)
			.detail(WalkDetail::Minimal)
			.order(WalkOrder::Path)
			.emit_root(false)
			.depth(1, usize::MAX)
			.directory_errors(DirectoryErrorMode::SkipSkippable)
			.cache(true)
			.empty_recheck(EmptyRecheck::Configured)
			.filter(filter);

		let heartbeat = || {
			if ctx.cancellation_token.is_cancelled() {
				Err("AST grep cancelled".to_string())
			} else {
				Ok(())
			}
		};

		let entries = match request.collect_file_candidates_with_heartbeat(heartbeat) {
			Ok(entries) => entries,
			Err(WalkError::Interrupted(_)) => return Ok(None),
			Err(error) => {
				if ctx.cancellation_token.is_cancelled() {
					return Ok(None);
				}
				return Err(ToolError::ExecutionFailed(format!(
					"AST grep walk error: {error}"
				)));
			},
		};

		for entry in entries {
			if !is_supported_file(&entry.path, None) {
				continue;
			}
			let absolute = normalize_path(&entry.path);
			if !seen.insert(absolute) {
				continue;
			}
			candidates.push(Candidate {
				display: format_path_relative_to_cwd(&entry.path, &ctx.cwd, false),
				absolute: entry.path,
			});
		}
	}

	candidates.sort_by(|left, right| {
		left
			.display
			.cmp(&right.display)
			.then_with(|| left.absolute.cmp(&right.absolute))
	});
	Ok(Some(candidates))
}

pub(super) fn common_base(targets: &[Target]) -> PathBuf {
	let Some(first) = targets.first() else {
		return PathBuf::from(".");
	};
	let mut base = first.base.clone();
	for target in &targets[1..] {
		while !target.base.starts_with(&base) {
			if !base.pop() {
				return PathBuf::from("/");
			}
		}
	}
	base
}

#[cfg(test)]
mod tests {
	use std::{
		fs,
		time::{SystemTime, UNIX_EPOCH},
	};

	use serde_json::json;

	use crate::tools::Tool as _;

	use super::*;

	fn temp_dir(label: &str) -> PathBuf {
		let nanos = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.map(|duration| duration.as_nanos())
			.unwrap_or_default();
		let path = std::env::temp_dir().join(format!(
			"omp-ast-grep-{label}-{}-{nanos}",
			std::process::id()
		));
		fs::create_dir_all(&path).expect("create temp dir");
		path
	}

	fn calls(count: usize, start: usize) -> String {
		let mut source = String::from("fn main() {\n");
		for index in start..start + count {
			source.push_str(&format!("    foo(a{index});\n"));
		}
		source.push_str("}\n");
		source
	}

	fn assert_invalid_arguments(error: ToolError, needle: &str) {
		match error {
			ToolError::InvalidArguments(message) => {
				assert!(message.contains(needle), "{message}");
			},
			other => panic!("expected invalid arguments containing {needle}, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn reports_captures_paging_and_parse_errors() {
		let dir = temp_dir("behavior");
		fs::write(dir.join("one.rs"), calls(30, 0)).expect("write one");
		fs::write(dir.join("two.rs"), calls(30, 30)).expect("write two");
		fs::write(dir.join("broken.rs"), "fn main( {\n}\n").expect("write broken");

		let tool = AstGrepTool::new();
		let ctx = AgentContext::new(dir.clone());

		let paged = tool
			.execute(
				&json!({ "pat": "foo($A)", "path": "one.rs;two.rs", "skip": 5 }),
				&ctx,
			)
			.await
			.expect("ast_grep executes");
		assert!(!paged.is_error, "{}", paged.content);
		let details = paged.details.as_ref().expect("details");
		assert_eq!(details["matchCount"].as_u64(), Some(60));
		assert_eq!(details["fileCount"].as_u64(), Some(2));
		assert_eq!(details["filesSearched"].as_u64(), Some(2));
		assert_eq!(details["limitReached"].as_bool(), Some(true));
		assert!(paged.content.contains("meta: A=a5"), "{}", paged.content);
		assert!(!paged.content.contains("meta: A=a55"), "{}", paged.content);

		let whole = tool
			.execute(&json!({ "pat": "foo($A)", "path": "." }), &ctx)
			.await
			.expect("ast_grep executes");
		assert!(!whole.is_error, "{}", whole.content);
		let details = whole.details.as_ref().expect("details");
		assert_eq!(details["matchCount"].as_u64(), Some(60));
		assert_eq!(details["filesSearched"].as_u64(), Some(3));
		assert!(
			details["parseErrors"]
				.as_array()
				.is_some_and(|errors| !errors.is_empty()),
			"{}",
			whole.content
		);
	}

	#[tokio::test]
	async fn single_glob_target_searches_its_literal_base() {
		let dir = temp_dir("single-glob");
		fs::create_dir_all(dir.join("src/nested")).expect("create src");
		fs::write(dir.join("src/a.rs"), calls(1, 0)).expect("write a");
		fs::write(dir.join("src/nested/b.rs"), calls(1, 10)).expect("write b");
		fs::write(dir.join("top.rs"), calls(1, 20)).expect("write top");

		let tool = AstGrepTool::new();
		let ctx = AgentContext::new(dir.clone());

		let result = tool
			.execute(&json!({ "pat": "foo($A)", "path": "src/**/*.rs" }), &ctx)
			.await
			.expect("single glob executes");
		assert!(!result.is_error, "{}", result.content);
		let details = result.details.as_ref().expect("details");
		assert_eq!(details["matchCount"].as_u64(), Some(2), "{}", result.content);
		assert_eq!(details["filesSearched"].as_u64(), Some(2));
		assert!(!result.content.contains("top.rs"), "{}", result.content);

		// A single literal that really is absent still reports it verbatim.
		let error = tool
			.execute(&json!({ "pat": "foo($A)", "path": "missing.rs" }), &ctx)
			.await
			.expect_err("missing literal rejects");
		assert_invalid_arguments(error, "Path not found: missing.rs");
	}

	#[tokio::test]
	async fn single_target_glob_stays_at_the_search_root() {
		let dir = temp_dir("single-root-glob");
		fs::create_dir_all(dir.join("nested")).expect("create nested");
		fs::write(dir.join("root.rs"), calls(1, 0)).expect("write root");
		fs::write(dir.join("nested/inner.rs"), calls(1, 10)).expect("write inner");

		let tool = AstGrepTool::new();
		let ctx = AgentContext::new(dir.clone());

		let single = tool
			.execute(&json!({ "pat": "foo($A)", "path": "*.rs" }), &ctx)
			.await
			.expect("single glob executes");
		assert!(!single.is_error, "{}", single.content);
		let details = single.details.as_ref().expect("details");
		assert_eq!(details["matchCount"].as_u64(), Some(1), "{}", single.content);
		assert_eq!(details["filesSearched"].as_u64(), Some(1));
		assert!(!single.content.contains("inner.rs"), "{}", single.content);

		// Multi-target lists keep the recursive convention.
		let multi = tool
			.execute(&json!({ "pat": "foo($A)", "path": "*.rs; nested" }), &ctx)
			.await
			.expect("multi glob executes");
		assert!(!multi.is_error, "{}", multi.content);
		let details = multi.details.as_ref().expect("details");
		assert_eq!(details["matchCount"].as_u64(), Some(2), "{}", multi.content);
		assert_eq!(details["filesSearched"].as_u64(), Some(2));
	}

	#[tokio::test]
	async fn absolute_glob_targets_hit_the_root_guard() {
		let dir = temp_dir("root-guard");
		fs::write(dir.join("root.rs"), calls(1, 0)).expect("write root");

		let tool = AstGrepTool::new();
		let ctx = AgentContext::new(dir.clone());

		// An absolute glob with an empty literal prefix has base `/`, exactly
		// like `parse_find_pattern`; it must fail the root guard instead of
		// silently searching the cwd (where `root.rs` would match).
		for path in ["/*.rs", "/*", "/{a,b}/*.rs", "//*.rs"] {
			let error = tool
				.execute(&json!({ "pat": "foo($A)", "path": path }), &ctx)
				.await
				.expect_err("absolute root glob rejects");
			assert_invalid_arguments(error, "root directory '/'");
		}
	}

	#[tokio::test]
	async fn file_url_targets_decode_locally_and_reject_hosts() {
		let dir = temp_dir("file-url");
		let spaced = dir.join("file with space.rs");
		fs::write(&spaced, calls(1, 0)).expect("write spaced");
		fs::write(dir.join("one.rs"), calls(2, 10)).expect("write one");

		let tool = AstGrepTool::new();
		let ctx = AgentContext::new(dir.clone());

		let encoded_path = spaced.to_string_lossy().replace(' ', "%20");
		for url in [
			format!("file://{encoded_path}"),
			format!("file://localhost{encoded_path}"),
		] {
			let result = tool
				.execute(&json!({ "pat": "foo($A)", "path": url }), &ctx)
				.await
				.expect("file URL executes");
			assert!(!result.is_error, "{}", result.content);
			let details = result.details.as_ref().expect("details");
			assert_eq!(details["matchCount"].as_u64(), Some(1), "{}", result.content);
			assert_eq!(details["filesSearched"].as_u64(), Some(1));
		}

		// A file URL mixed with a plain path searches both.
		let mixed = format!("file://{encoded_path}; one.rs");
		let result = tool
			.execute(&json!({ "pat": "foo($A)", "path": mixed }), &ctx)
			.await
			.expect("mixed file URL executes");
		assert!(!result.is_error, "{}", result.content);
		let details = result.details.as_ref().expect("details");
		assert_eq!(details["matchCount"].as_u64(), Some(3), "{}", result.content);
		assert_eq!(details["fileCount"].as_u64(), Some(2));

		// Non-local authorities are rejected, including inside a mixed list.
		for path in [
			"file://remote.example/tmp/x.rs",
			"one.rs; file://remote.example/tmp/x.rs",
		] {
			let error = tool
				.execute(&json!({ "pat": "foo($A)", "path": path }), &ctx)
				.await
				.expect_err("remote file URL rejects");
			assert_invalid_arguments(error, "Unsupported file URL host");
		}
	}

	#[tokio::test]
	async fn non_local_schemes_reject_before_any_search() {
		let dir = temp_dir("schemes");
		fs::write(dir.join("one.rs"), calls(1, 0)).expect("write one");

		let tool = AstGrepTool::new();
		let ctx = AgentContext::new(dir.clone());

		for path in [
			"ssh://box/repo",
			"local://name.md",
			"skill://name",
			"https://example.com/x.rs",
		] {
			let error = tool
				.execute(&json!({ "pat": "foo($A)", "path": path }), &ctx)
				.await
				.expect_err("scheme target rejects");
			assert_invalid_arguments(error, "Unsupported target");
		}

		// Mixed lists fail as a whole: `one.rs` exists and has a match, so a
		// partial search would have succeeded instead of returning this error.
		for path in ["one.rs; ssh://box/repo", "ssh://box/repo; one.rs"] {
			let error = tool
				.execute(&json!({ "pat": "foo($A)", "path": path }), &ctx)
				.await
				.expect_err("mixed scheme target rejects");
			assert_invalid_arguments(error, "Unsupported target");
		}
		let error = tool
			.execute(
				&json!({ "pat": "foo($A)", "path": ["one.rs", "local://name.md"] }),
				&ctx,
			)
			.await
			.expect_err("mixed scheme array rejects");
		assert_invalid_arguments(error, "Unsupported target");
	}

	#[tokio::test]
	async fn parse_errors_and_matches_come_from_the_same_scan() {
		let dir = temp_dir("parse-and-match");
		// Unclosed block: the tree carries an error node and still contains the
		// call, so a correct single-parse scan reports both.
		fs::write(dir.join("half.rs"), "fn main() {\n    foo(a1);\n").expect("write half");

		let tool = AstGrepTool::new();
		let ctx = AgentContext::new(dir.clone());

		let result = tool
			.execute(&json!({ "pat": "foo($A)", "path": "half.rs" }), &ctx)
			.await
			.expect("ast_grep executes");
		assert!(!result.is_error, "{}", result.content);
		let details = result.details.as_ref().expect("details");
		assert_eq!(details["matchCount"].as_u64(), Some(1), "{}", result.content);
		assert_eq!(details["filesSearched"].as_u64(), Some(1));
		assert!(
			details["parseErrors"]
				.as_array()
				.is_some_and(|errors| !errors.is_empty()),
			"{}",
			result.content
		);
		assert!(result.content.contains("meta: A=a1"), "{}", result.content);
	}

	#[derive(Default)]
	struct TestSink {
		files: std::sync::Mutex<Vec<(PathBuf, String)>>,
	}

	impl ReadSnapshotSink for TestSink {
		fn record_file(&self, absolute_path: &Path, full_text: &str) -> Option<String> {
			self
				.files
				.lock()
				.expect("files")
				.push((absolute_path.to_path_buf(), full_text.to_string()));
			Some("0A1B".to_string())
		}

		fn record_seen_lines(&self, _absolute_path: &Path, _tag: &str, _lines: &[u32]) {}

		fn header(&self, absolute_path: &Path, tag: &str, cwd: &Path) -> String {
			let relative = absolute_path.strip_prefix(cwd).unwrap_or(absolute_path);
			format!("[{}#{tag}]", relative.to_string_lossy().replace('\\', "/"))
		}
	}

	#[tokio::test]
	async fn hashline_display_tags_files_and_uses_colon_lines() {
		use crate::tools::edit_variants::mode::EditMode;

		let dir = temp_dir("hashline-display");
		fs::write(dir.join("one.rs"), calls(1, 0)).expect("write one");
		let sink = Arc::new(TestSink::default());
		let tool = AstGrepTool::with_snapshot_sink(sink.clone())
			.with_display_inputs(FileDisplayInputs::new(EditMode::Hashline, true, false));
		let ctx = AgentContext::new(dir.clone());

		let result = tool
			.execute(&json!({ "pat": "foo($A)", "path": "one.rs" }), &ctx)
			.await
			.expect("ast_grep executes");
		assert!(!result.is_error, "{}", result.content);
		assert!(result.content.starts_with("[one.rs#0A1B]\n"), "{}", result.content);
		assert!(result.content.contains("*2:    foo(a0);"), "{}", result.content);
		assert!(!result.content.contains("*2|"), "{}", result.content);
		assert_eq!(sink.files.lock().expect("files").len(), 1);
		let details = result.details.as_ref().expect("details");
		assert!(
			details["displayContent"]
				.as_str()
				.is_some_and(|display| display.contains("*2│    foo(a0);")),
			"{}",
			result.content
		);

		// No display inputs keeps the historical plain separator.
		let plain = AstGrepTool::new();
		let result = plain
			.execute(&json!({ "pat": "foo($A)", "path": "one.rs" }), &ctx)
			.await
			.expect("ast_grep executes");
		assert!(result.content.contains("*2|    foo(a0);"), "{}", result.content);
		assert!(!result.content.contains("[one.rs#"), "{}", result.content);
	}
}
