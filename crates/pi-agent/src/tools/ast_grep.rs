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
};

use async_trait::async_trait;
use pi_ast::{
	SupportLang,
	ops::{
		AstMatch, Pattern, collect_matches, compile_search_patterns, has_parse_error,
		is_supported_file, resolve_language,
	},
};
use pi_walker::{
	CompiledWalkGlob, DirectoryErrorMode, EmptyRecheck, FollowLinks, WalkDetail, WalkError,
	WalkFilter, WalkOrder, WalkRequest,
};
use serde_json::{Value, json};

use super::{
	ParsedFindPattern, Tool, ToolError, extract_raw_paths, fix_unclosed_braces,
	format_path_relative_to_cwd, is_root_path, normalize_path, normalize_path_like_input,
	parse_find_pattern, resolve_to_cwd, split_top_level_delimited, tool_result,
};
use crate::state::{AgentContext, ToolResult};

/// Upstream `DEFAULT_AST_LIMIT`.
const DEFAULT_AST_LIMIT: usize = 50;
/// Upstream `capParseErrors` default.
const MAX_PARSE_ERRORS: usize = 20;

/// Builtin structural search tool.
pub struct AstGrepTool;

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
					"description": "file, directory, glob, or internal URL to search; pass several as a semicolon-delimited list (\"src; tests\"). Omitted -> searches the workspace root (\".\")"
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
		run_ast_grep(args, ctx)
	}
}

struct Target {
	base:                 PathBuf,
	glob:                 Option<String>,
	is_file:              bool,
	mentions_node_modules: bool,
}

struct Candidate {
	absolute: PathBuf,
	display:  String,
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

fn run_ast_grep(args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
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

		report_parse_error(&source, language, &candidate.display, &mut parse_errors);

		let mut seen: HashSet<MatchKey> = HashSet::new();
		for matched in collect_deduped_matches(&source, language, patterns.as_slice()) {
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
	let mut output: Vec<String> = Vec::new();
	for (index, (path, matches)) in matches_by_file.iter().enumerate() {
		if grouped {
			if index > 0 {
				output.push(String::new());
			}
			output.push(format!("# {path}"));
		}
		for matched in matches {
			let lines: Vec<&str> = matched.text.split('\n').collect();
			for (offset, line) in lines.iter().enumerate() {
				let line_number = matched.key.start_line + offset;
				let marker = if offset == 0 { '*' } else { ' ' };
				output.push(format!("{marker}{line_number}|{line}"));
			}
			if !matched.meta.is_empty() {
				let serialized = matched
					.meta
					.iter()
					.map(|(key, value)| format!("{key}={value}"))
					.collect::<Vec<_>>()
					.join(", ");
				output.push(format!("  meta: {serialized}"));
			}
		}
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

/// `compile_search_patterns` adds Rust's contextual fallback on top of the
/// primary pattern. Both can describe the same source span (a call and its
/// expression statement), so later patterns only contribute matches whose
/// start position the primary pattern did not already cover. Exact duplicate
/// ranges are still filtered by the caller.
fn collect_deduped_matches(
	source: &str,
	language: SupportLang,
	patterns: &[Pattern],
) -> Vec<AstMatch> {
	let Some((first, rest)) = patterns.split_first() else {
		return Vec::new();
	};
	let mut matches = collect_matches(source, language, std::slice::from_ref(first));
	let mut starts: HashSet<(usize, usize)> =
		matches.iter().map(|matched| (matched.line, matched.column)).collect();
	for pattern in rest {
		for matched in collect_matches(source, language, std::slice::from_ref(pattern)) {
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

/// Detect tree-sitter error nodes without adding a second parser: the shared
/// parse cache owns the only direct `Parser` call.
fn report_parse_error(
	source: &str,
	language: SupportLang,
	display: &str,
	errors: &mut Vec<String>,
) {
	match has_parse_error(source, language) {
		Ok(true) => errors.push(format!(
			"{display}: parse error (syntax tree contains error nodes)"
		)),
		Ok(false) => {},
		Err(error) => errors.push(format!("{display}: {error}")),
	}
}

fn plan_targets(
	args: &Value,
	cwd: &Path,
) -> Result<(Vec<Target>, Vec<String>, String), ToolError> {
	let mut expanded: Vec<String> = Vec::new();
	for raw in extract_raw_paths(args) {
		let normalized = normalize_path_like_input(&raw);
		if normalized.is_empty() {
			return Err(ToolError::InvalidArguments(
				"`path` must contain non-empty globs or paths".to_string(),
			));
		}
		let literal = resolve_to_cwd(&normalized, cwd);
		if literal.exists() {
			expanded.push(normalized);
		} else {
			expanded.extend(split_top_level_delimited(&normalized));
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

	let is_single = expanded.len() == 1;
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

		if is_single {
			return Err(ToolError::InvalidArguments(format!("Path not found: {item}")));
		}

		let ParsedFindPattern { base_path, glob_pattern, has_glob } = parse_find_pattern(item);
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

fn collect_candidates(
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

fn common_base(targets: &[Target]) -> PathBuf {
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

	#[tokio::test]
	async fn reports_captures_paging_and_parse_errors() {
		let dir = temp_dir("behavior");
		fs::write(dir.join("one.rs"), calls(30, 0)).expect("write one");
		fs::write(dir.join("two.rs"), calls(30, 30)).expect("write two");
		fs::write(dir.join("broken.rs"), "fn main( {\n}\n").expect("write broken");

		let tool = AstGrepTool;
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
}
