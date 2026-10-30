//! `issue://` and `pr://` internal URL handlers.
//!
//! Mirrors `packages/coding-agent/src/internal-urls/issue-pr-protocol.ts`,
//! `tools/gh-view.ts`, `tools/gh-pr-diff.ts`, `tools/gh-common.ts`, and the
//! sanctioned `gh` runner in `utils/github.ts` — minus the SQLite cache, which
//! this port has no store for yet, so every read is a fresh live fetch
//! (`formatFreshnessNote("miss")` = `Fetched live`).
//!
//! URL shapes: `scheme://`, `scheme://N`, `scheme://owner/repo`,
//! `scheme://owner/repo/N`, an optional dotted `host/` prefix on any of them,
//! `pr://N/diff[/all|/<i>]`, and `?state/&limit/&author/&label` on lists or
//! `?comments=0|false` on single items. The router hands over a URL without a
//! query split, so this module splits `?` and reconstructs
//! `rawHost`/`rawPathname` itself before applying the upstream parser.
//!
//! Read-only: no mutations, no auth flow, ambient `gh auth` only. `gh` runs as
//! argv (never a shell) under a non-interactive environment, a 5-minute
//! deadline, 8 MiB capped streams, and a fresh process group so cancellation
//! kills and reaps the whole tree. Failures surface the upstream
//! `formatGhFailure` strings verbatim.

use std::{
	io::ErrorKind,
	path::Path,
	process::{ExitStatus, Stdio},
	time::Duration,
};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio::{
	io::{AsyncRead, AsyncReadExt},
	process::{Child, Command},
	time::timeout,
};

use super::internal_urls::{InternalResource, InternalUrl, ProtocolHandler, ResolveContext};
use crate::CancellationToken;

/// `LIST_LIMIT_DEFAULT` / `LIST_LIMIT_MAX` (`issue-pr-protocol.ts:74-75`).
const LIST_LIMIT_DEFAULT: u32 = 30;
const LIST_LIMIT_MAX: u32 = 100;
/// `GH_COMMAND_TIMEOUT_MS` (`utils/github.ts:17`).
const GH_DEADLINE: Duration = Duration::from_secs(5 * 60);
/// `GH_OUTPUT_LIMIT_BYTES` / `GH_TRUNCATED_MARKER` (`utils/github.ts:19-20`).
const GH_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const GH_TRUNCATED_MARKER: &str = "\n[gh subprocess output truncated after 8 MiB]\n";
/// `FILE_PREVIEW_LIMIT` (`tools/gh-search.ts:37`).
const FILE_PREVIEW_LIMIT: usize = 50;
/// `PR_DIFF_FILES_PAGE_SIZE` / `PR_DIFF_FILES_MAX` (`tools/gh-pr-diff.ts:9-10`).
const PR_DIFF_FILES_PAGE_SIZE: usize = 100;
const PR_DIFF_FILES_MAX: u64 = 3000;
/// `REVIEW_COMMENTS_PAGE_SIZE` (`tools/gh-view.ts`).
const REVIEW_COMMENTS_PAGE_SIZE: usize = 100;
const GH_NOT_INSTALLED: &str =
	"GitHub CLI (gh) is not installed. Install it from https://cli.github.com/.";
const GH_INVALID_JSON: &str = "GitHub CLI returned invalid JSON output.";
const GH_EMPTY_OUTPUT: &str = "GitHub CLI returned empty output.";

/// `GH_NON_INTERACTIVE_ENV` (`utils/github.ts:21-30`) without the Bun-only
/// prompt shims this port cannot resolve: pagers off, every editor and
/// credential prompt rejected, `LC_ALL` dropped so `LC_MESSAGES=C` decides.
const GH_ENV: [(&str, &str); 9] = [
	("GH_PROMPT_DISABLED", "1"),
	("GIT_TERMINAL_PROMPT", "0"),
	("GIT_ASKPASS", "true"),
	("SSH_ASKPASS", "false"),
	("GIT_EDITOR", "true"),
	("VISUAL", "true"),
	("EDITOR", "true"),
	("GH_PAGER", "cat"),
	("LC_MESSAGES", "C"),
];

/// `GH_ISSUE_FIELDS` (`tools/gh-view.ts:37-49`).
const GH_ISSUE_FIELDS: [&str; 11] = [
	"author",
	"body",
	"comments",
	"createdAt",
	"labels",
	"number",
	"state",
	"stateReason",
	"title",
	"updatedAt",
	"url",
];
const GH_ISSUE_FIELDS_NO_COMMENTS: [&str; 10] = [
	"author",
	"body",
	"createdAt",
	"labels",
	"number",
	"state",
	"stateReason",
	"title",
	"updatedAt",
	"url",
];
/// `GH_PR_FIELDS` (`tools/gh-view.ts:114-132`).
const GH_PR_FIELDS: [&str; 17] = [
	"author",
	"baseRefName",
	"body",
	"comments",
	"createdAt",
	"files",
	"headRefName",
	"isDraft",
	"labels",
	"mergeStateStatus",
	"number",
	"reviews",
	"reviewDecision",
	"state",
	"title",
	"updatedAt",
	"url",
];
const GH_PR_FIELDS_NO_COMMENTS: [&str; 16] = [
	"author",
	"baseRefName",
	"body",
	"createdAt",
	"files",
	"headRefName",
	"isDraft",
	"labels",
	"mergeStateStatus",
	"number",
	"reviews",
	"reviewDecision",
	"state",
	"title",
	"updatedAt",
	"url",
];
/// `fetchAndRenderList` field lists (`issue-pr-protocol.ts:387-400`).
const ISSUE_LIST_FIELDS: [&str; 8] =
	["number", "title", "state", "author", "labels", "createdAt", "updatedAt", "url"];
const PR_LIST_FIELDS: [&str; 11] = [
	"number",
	"title",
	"state",
	"isDraft",
	"author",
	"baseRefName",
	"headRefName",
	"labels",
	"createdAt",
	"updatedAt",
	"url",
];

/// Parsed `pr://` diff variants (`ParsedPrDiff`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrDiffMode {
	/// Enumerate the changed files.
	List,
	/// Full unified diff.
	All,
	/// One file's unified section, 1-indexed.
	Slice(u64),
}

/// Parsed `issue://` / `pr://` URL (`Parsed` union).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GithubParsed {
	List {
		repo:   Option<String>,
		state:  String,
		limit:  u32,
		author: Option<String>,
		label:  Option<String>,
	},
	Single {
		repo:     Option<String>,
		number:   u64,
		comments: bool,
	},
	PrDiff {
		repo:   Option<String>,
		number: u64,
		mode:   PrDiffMode,
	},
}

/// `issue://` handler: listing and single-item views, read-only.
pub struct IssueUrlHandler;

/// `pr://` handler: listing, single-item views, and the diff family.
pub struct PrUrlHandler;

#[async_trait]
impl ProtocolHandler for IssueUrlHandler {
	fn scheme(&self) -> &str {
		"issue"
	}

	fn immutable(&self) -> bool {
		true
	}

	async fn resolve(
		&self,
		url: &InternalUrl,
		context: &ResolveContext,
	) -> Result<InternalResource, String> {
		if context.signal.is_cancelled() {
			return Err("aborted".to_string());
		}
		match parse_issue_pr_url(&url.href, "issue")? {
			GithubParsed::List { repo, state, limit, author, label } => fetch_and_render_list(
				"issue",
				&url.href,
				&context.cwd,
				&context.signal,
				repo,
				state,
				limit,
				author,
				label,
			)
			.await
			.map_err(|error| format!("issue:// listing failed: {error}")),
			GithubParsed::Single { repo, number, comments } => fetch_issue_view(
				&url.href,
				&context.cwd,
				&context.signal,
				repo,
				number,
				comments,
			)
			.await
			.map_err(|error| format!("issue:// resolution failed: {error}")),
			GithubParsed::PrDiff { .. } => {
				// The parser rejects `issue://…/diff`; this guard only keeps the
				// union honest, like the upstream belt-and-suspenders catch.
				Err("Invalid issue:// URL: unexpected variant 'pr-diff'".to_string())
			},
		}
	}
}

#[async_trait]
impl ProtocolHandler for PrUrlHandler {
	fn scheme(&self) -> &str {
		"pr"
	}

	fn immutable(&self) -> bool {
		true
	}

	async fn resolve(
		&self,
		url: &InternalUrl,
		context: &ResolveContext,
	) -> Result<InternalResource, String> {
		if context.signal.is_cancelled() {
			return Err("aborted".to_string());
		}
		match parse_issue_pr_url(&url.href, "pr")? {
			GithubParsed::List { repo, state, limit, author, label } => fetch_and_render_list(
				"pr",
				&url.href,
				&context.cwd,
				&context.signal,
				repo,
				state,
				limit,
				author,
				label,
			)
			.await
			.map_err(|error| format!("pr:// listing failed: {error}")),
			GithubParsed::PrDiff { repo, number, mode } => fetch_pr_diff_resource(
				&url.href,
				&context.cwd,
				&context.signal,
				repo,
				number,
				mode,
			)
			.await
			.map_err(|error| format!("pr:// diff resolution failed: {error}")),
			GithubParsed::Single { repo, number, comments } => {
				// Repo resolution failures stay unwrapped here: upstream throws
				// them before its single-item try block.
				let repo = match repo {
					Some(repo) => repo,
					None => resolve_default_repo(&context.cwd, &context.signal)
						.await
						.map_err(|message| {
							format!(
								"pr://{number} could not resolve a default repo from the current \
								 session: {message}\nUse pr://<owner>/<repo>/{number}."
							)
						})?,
				};
				fetch_pr_view(
					&url.href,
					&context.cwd,
					&context.signal,
					&repo,
					number,
					comments,
				)
				.await
				.map_err(|error| format!("pr:// resolution failed: {error}"))
			},
		}
	}
}

// ────────────────────────────────────────────────────────────────────────────
// Parsing (`issue-pr-protocol.ts:100-335`)
// ────────────────────────────────────────────────────────────────────────────

/// `parsePositiveDecimalInt` (`gh-common.ts:180-190`): digit-only, JS-safe,
/// greater than zero. Rejects `1e2`, `0x10`, `12.0`, `+1`, `-1`.
pub(crate) fn parse_positive_int(raw: &str) -> Option<u64> {
	if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
		return None;
	}
	let value: u64 = raw.parse().ok()?;
	// `Number.isSafeInteger` ceiling.
	(value > 0 && value <= 9_007_199_254_740_991).then_some(value)
}

/// `parseRepoRef` (`gh-common.ts:87-94`): split `[HOST/]OWNER/REPO`; any other
/// shape stays an opaque slug. The host is only split off when the string has
/// exactly three segments.
pub(crate) fn parse_repo_ref(raw: &str) -> (Option<String>, String) {
	let Some(first) = raw.find('/') else {
		return (None, raw.to_string());
	};
	let Some(second) = raw[first + 1..].find('/').map(|index| first + 1 + index) else {
		return (None, raw.to_string());
	};
	if raw[second + 1..].contains('/') {
		return (None, raw.to_string());
	}
	(Some(raw[..first].to_string()), raw[first + 1..].to_string())
}

/// `formatRepoRef` (`gh-common.ts:97-99`).
fn format_repo_ref(host: Option<&str>, slug: &str) -> String {
	match host {
		Some(host) => format!("{host}/{slug}"),
		None => slug.to_string(),
	}
}

/// Parse one `issue://` / `pr://` URL from its verbatim href.
///
/// `scheme` is the caller's scheme, matching upstream's explicit parameter —
/// it decides which shapes and errors apply, not the href's own prefix.
pub(crate) fn parse_issue_pr_url(href: &str, scheme: &str) -> Result<GithubParsed, String> {
	let rest = href.split_once("://").map_or("", |(_, rest)| rest);
	// The router keeps the whole tail in `path`; upstream's `URL` parser drops
	// the fragment before `searchParams` and `pathname` are read.
	let rest = rest.split_once('#').map_or(rest, |(before, _)| before);
	let (head, query) = rest.split_once('?').unwrap_or((rest, ""));
	let (raw_host, raw_path) = head.split_once('/').unwrap_or((head, ""));
	let mut parts = split_segments(raw_path, scheme)?;

	// A dotted first segment can only be a host. A single-label host is only
	// recognizable from the item number's position, so it is accepted in the
	// numbered form alone (upstream keeps the bad-number error otherwise).
	let dotted_host = raw_host.contains('.');
	if dotted_host && parts.len() < 2 {
		return Err(format!(
			"Invalid {scheme}:// URL. Expected {scheme}://<host>/<owner>/<repo> or \
			 {scheme}://<host>/<owner>/<repo>/<number>"
		));
	}
	let host_prefixed = if dotted_host {
		parts.len() >= 2
	} else {
		parts.len() >= 3 && parts.get(2).is_some_and(|part| parse_positive_int(part).is_some())
	};
	let mut repo_host: Option<String> = None;
	let mut host = raw_host.to_string();
	if host_prefixed {
		repo_host = Some(host);
		host = parts.first().cloned().unwrap_or_default();
		parts.remove(0);
	}

	let mut repo: Option<String> = None;
	let number_part: String;
	let mut diff_parts: Vec<String> = Vec::new();

	if host.is_empty() && parts.is_empty() {
		return parse_list_options(query, scheme, None);
	}
	if !host.is_empty() && parts.is_empty() {
		// `scheme://N` (numeric) or a host-only URL with no repo segment.
		number_part = host;
	} else if parts.first().map(String::as_str) == Some("diff") && parse_positive_int(&host).is_some()
	{
		// `<scheme>://N/diff[/<sub>]`; restricted to numeric hosts so
		// `<scheme>://owner/diff` stays the repo-scoped listing.
		number_part = host;
		diff_parts = parts;
	} else if !host.is_empty() && parts.len() == 1 {
		let repo = Some(format_repo_ref(repo_host.as_deref(), &format!("{host}/{}", parts[0])));
		return parse_list_options(query, scheme, repo);
	} else if !host.is_empty() && parts.len() >= 2 {
		repo = Some(format_repo_ref(repo_host.as_deref(), &format!("{host}/{}", parts[0])));
		number_part = parts[1].clone();
		diff_parts = parts[2..].to_vec();
	} else {
		return Err(format!(
			"Invalid {scheme}:// URL. Expected {scheme}://, {scheme}://<number>, \
			 {scheme}://<owner>/<repo>, or {scheme}://<owner>/<repo>/<number>"
		));
	}

	// Reject unrecognized trailing segments before parsing the number so
	// `issue://owner/repo/foo/bar` surfaces as an invalid URL rather than a
	// misleading invalid number.
	if !diff_parts.is_empty() {
		if scheme == "issue" {
			return Err("Invalid issue:// URL. Issue views do not have a diff; use \
			            pr://<owner>/<repo>/<n>/diff for pull requests."
				.to_string());
		}
		if diff_parts[0] != "diff" || diff_parts.len() > 2 {
			return Err("Invalid pr:// URL. Expected pr://<n>, pr://<n>/diff, pr://<n>/diff/all, \
			            or pr://<n>/diff/<i>"
				.to_string());
		}
	}

	let number = parse_positive_int(&number_part).ok_or_else(|| {
		let shown = if number_part.is_empty() { "(missing)" } else { number_part.as_str() };
		format!("Invalid {scheme}:// number: {shown}")
	})?;

	if diff_parts.is_empty() {
		let comments = match query_param(query, "comments") {
			None => true,
			Some(raw) => !(raw == "0" || raw.eq_ignore_ascii_case("false")),
		};
		return Ok(GithubParsed::Single { repo, number, comments });
	}

	if diff_parts.len() == 1 {
		return Ok(GithubParsed::PrDiff { repo, number, mode: PrDiffMode::List });
	}
	let sub = &diff_parts[1];
	if sub == "all" {
		return Ok(GithubParsed::PrDiff { repo, number, mode: PrDiffMode::All });
	}
	let index = parse_positive_int(sub).ok_or_else(|| {
		format!(
			"Invalid pr:// diff sub-path '{sub}'. Use 'all' or a 1-indexed file number."
		)
	})?;
	Ok(GithubParsed::PrDiff { repo, number, mode: PrDiffMode::Slice(index) })
}

fn parse_list_options(
	query: &str,
	scheme: &str,
	repo: Option<String>,
) -> Result<GithubParsed, String> {
	let allowed: &[&str] = if scheme == "pr" {
		&["open", "closed", "merged", "all"]
	} else {
		&["open", "closed", "all"]
	};
	let state = match query_param(query, "state") {
		// Reject instead of silently falling back to "open": a typo would
		// otherwise be indistinguishable from "no matches".
		Some(raw) if !allowed.contains(&raw.as_str()) => {
			return Err(format!(
				"Invalid {scheme}:// list state '{raw}'. Expected one of: {}.",
				allowed.join(", ")
			));
		},
		Some(raw) => raw,
		None => "open".to_string(),
	};
	let limit = match query_param(query, "limit") {
		None => LIST_LIMIT_DEFAULT,
		Some(raw) => {
			let parsed = parse_positive_int(&raw).ok_or_else(|| {
				format!(
					"Invalid {scheme}:// list limit '{raw}'. Expected a positive integer (max \
					 {LIST_LIMIT_MAX})."
				)
			})?;
			u32::try_from(parsed.min(u64::from(LIST_LIMIT_MAX))).unwrap_or(LIST_LIMIT_MAX)
		},
	};
	Ok(GithubParsed::List {
		repo,
		state,
		limit,
		author: query_param(query, "author"),
		label:  query_param(query, "label"),
	})
}

/// Percent-encoded path segments. Segments are validated after decoding but
/// kept raw, exactly like upstream: `parts.push(seg)` pushes the encoded text.
fn split_segments(raw_path: &str, scheme: &str) -> Result<Vec<String>, String> {
	let stripped = raw_path.strip_prefix('/').unwrap_or(raw_path);
	let mut parts = Vec::new();
	if stripped.is_empty() {
		return Ok(parts);
	}
	for segment in stripped.split('/') {
		let decoded = percent_decode(segment).ok_or_else(|| {
			format!("Invalid {scheme}:// URL: empty or unsafe path segment")
		})?;
		if decoded.is_empty() || decoded == "." || decoded == ".." {
			return Err(format!("Invalid {scheme}:// URL: empty or unsafe path segment"));
		}
		parts.push(segment.to_string());
	}
	Ok(parts)
}

/// Strict `%XX` decoding; `None` on a malformed escape or non-UTF-8 result.
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

/// `URLSearchParams.get`: first match, `+` as space, invalid escapes kept.
fn query_param(query: &str, key: &str) -> Option<String> {
	for pair in query.split('&') {
		if pair.is_empty() {
			continue;
		}
		let (raw_key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
		if decode_query_component(raw_key) == key {
			return Some(decode_query_component(raw_value));
		}
	}
	None
}

fn decode_query_component(value: &str) -> String {
	let bytes = value.as_bytes();
	let mut decoded = Vec::with_capacity(bytes.len());
	let mut index = 0;
	while index < bytes.len() {
		match bytes[index] {
			b'+' => {
				decoded.push(b' ');
				index += 1;
			},
			b'%' if index + 2 < bytes.len()
				&& bytes[index + 1].is_ascii_hexdigit()
				&& bytes[index + 2].is_ascii_hexdigit() =>
			{
				let byte = u8::from_str_radix(&value[index + 1..index + 3], 16).unwrap_or(b'%');
				decoded.push(byte);
				index += 3;
			},
			byte => {
				decoded.push(byte);
				index += 1;
			},
		}
	}
	String::from_utf8_lossy(&decoded).into_owned()
}

// ────────────────────────────────────────────────────────────────────────────
// Rendering (`tools/gh-view.ts`, `tools/gh-common.ts`, `issue-pr-protocol.ts`)
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct GhUser {
	login: Option<String>,
	name:  Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct GhLabel {
	name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct GhComment {
	author:       Option<GhUser>,
	body:         Option<String>,
	created_at:   Option<String>,
	url:          Option<String>,
	is_minimized: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct GhIssueView {
	author:       Option<GhUser>,
	body:         Option<String>,
	comments:     Option<Vec<GhComment>>,
	created_at:   Option<String>,
	labels:       Option<Vec<GhLabel>>,
	number:       Option<u64>,
	state:        Option<String>,
	state_reason: Option<String>,
	title:        Option<String>,
	updated_at:   Option<String>,
	url:          Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct GhFile {
	path:         Option<String>,
	additions:    Option<u64>,
	deletions:    Option<u64>,
	change_type:  Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct GhCommit {
	oid: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct GhReview {
	author:       Option<GhUser>,
	body:         Option<String>,
	commit:       Option<GhCommit>,
	state:        Option<String>,
	submitted_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct GhReviewCommentApi {
	body:            Option<String>,
	created_at:      Option<String>,
	html_url:        Option<String>,
	id:              Option<u64>,
	in_reply_to_id:  Option<u64>,
	line:            Option<u64>,
	original_line:   Option<u64>,
	path:            Option<String>,
	side:            Option<String>,
	user:            Option<GhUser>,
}

#[derive(Debug, Clone, Default)]
struct GhReviewComment {
	author:          Option<GhUser>,
	body:            Option<String>,
	created_at:      Option<String>,
	in_reply_to_id:  Option<u64>,
	line:            Option<u64>,
	original_line:   Option<u64>,
	path:            Option<String>,
	side:            Option<String>,
	url:             Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct GhPrView {
	author:           Option<GhUser>,
	base_ref_name:    Option<String>,
	body:             Option<String>,
	comments:         Option<Vec<GhComment>>,
	created_at:       Option<String>,
	files:            Option<Vec<GhFile>>,
	head_ref_name:    Option<String>,
	is_draft:         Option<bool>,
	labels:           Option<Vec<GhLabel>>,
	merge_state_status: Option<String>,
	number:           Option<u64>,
	reviews:          Option<Vec<GhReview>>,
	review_decision:  Option<String>,
	state:            Option<String>,
	title:            Option<String>,
	updated_at:       Option<String>,
	url:              Option<String>,
	#[serde(skip)]
	review_comments:  Option<Vec<GhReviewComment>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ListItem {
	number:     Option<u64>,
	title:      Option<String>,
	state:      Option<String>,
	author:     Option<GhUser>,
	labels:     Option<Vec<GhLabel>>,
	created_at: Option<String>,
	updated_at: Option<String>,
	is_draft:   Option<bool>,
}

fn normalize_text(value: Option<&str>) -> String {
	value
		.unwrap_or("")
		.replace("\r\n", "\n")
		.replace('\r', "\n")
		.replace('\t', "    ")
		.trim()
		.to_string()
}

fn format_author(author: Option<&GhUser>) -> Option<String> {
	let Some(author) = author else {
		return None;
	};
	if let Some(login) = author.login.as_deref().filter(|login| !login.is_empty()) {
		return Some(format!("@{login}"));
	}
	author.name.as_deref().filter(|name| !name.is_empty()).map(str::to_string)
}

fn format_labels(labels: Option<&Vec<GhLabel>>) -> Option<String> {
	let names: Vec<&str> = labels
		.map(|labels| {
			labels
				.iter()
				.filter_map(|label| label.name.as_deref())
				.filter(|name| !name.is_empty())
				.collect::<Vec<_>>()
		})
		.unwrap_or_default();
	(!names.is_empty()).then(|| names.join(", "))
}

fn push_line<T: std::fmt::Display>(lines: &mut Vec<String>, label: &str, value: Option<T>) {
	let Some(value) = value else {
		return;
	};
	let value = value.to_string();
	if value.is_empty() {
		return;
	}
	lines.push(format!("{label}: {value}"));
}

fn format_comments_section(comments: Option<&Vec<GhComment>>) -> Option<Vec<String>> {
	let comments = comments.filter(|comments| !comments.is_empty())?;
	let visible: Vec<&GhComment> = comments
		.iter()
		.filter(|comment| comment.is_minimized != Some(true))
		.collect();
	let hidden_count = comments.len() - visible.len();
	let mut lines = vec!["## Comments".to_string(), String::new()];
	if visible.is_empty() {
		lines.push(format!(
			"No visible comments. Minimized comments omitted: {hidden_count}."
		));
		return Some(lines);
	}
	lines[0] = format!("## Comments ({})", visible.len());
	for comment in visible {
		let author = format_author(comment.author.as_ref()).unwrap_or_else(|| "unknown".to_string());
		let created = comment.created_at.as_deref().map_or(String::new(), |created| {
			format!(" · {created}")
		});
		lines.push(format!("### {author}{created}"));
		lines.push(String::new());
		let body = normalize_text(comment.body.as_deref());
		lines.push(if body.is_empty() { "No comment body.".to_string() } else { body });
		if let Some(url) = comment.url.as_deref() {
			lines.push(String::new());
			lines.push(format!("URL: {url}"));
		}
		lines.push(String::new());
	}
	if hidden_count > 0 {
		lines.push(format!("Minimized comments omitted: {hidden_count}."));
	}
	Some(lines)
}

fn format_reviews_section(reviews: Option<&Vec<GhReview>>) -> Option<Vec<String>> {
	let reviews = reviews.filter(|reviews| !reviews.is_empty())?;
	let mut lines = vec![format!("## Reviews ({})", reviews.len()), String::new()];
	for review in reviews {
		let author = format_author(review.author.as_ref()).unwrap_or_else(|| "unknown".to_string());
		let submitted = review
			.submitted_at
			.as_deref()
			.map_or(String::new(), |submitted| format!(" - {submitted}"));
		let state = review
			.state
			.as_deref()
			.map_or(String::new(), |state| format!(" [{state}]"));
		lines.push(format!("### {author}{submitted}{state}"));
		if let Some(oid) = review
			.commit
			.as_ref()
			.and_then(|commit| commit.oid.as_deref())
			.filter(|oid| !oid.is_empty())
		{
			lines.push(String::new());
			lines.push(format!("Commit: {}", &oid[..oid.len().min(12)]));
		}
		lines.push(String::new());
		let body = normalize_text(review.body.as_deref());
		lines.push(if body.is_empty() { "No review body.".to_string() } else { body });
		lines.push(String::new());
	}
	Some(lines)
}

fn review_comment_location(comment: &GhReviewComment) -> Option<String> {
	let path = comment.path.as_deref()?;
	Some(match comment.line.or(comment.original_line) {
		Some(line) => format!("{path}:{line}"),
		None => path.to_string(),
	})
}

fn format_review_comments_section(comments: Option<&Vec<GhReviewComment>>) -> Option<Vec<String>> {
	let comments = comments.filter(|comments| !comments.is_empty())?;
	let mut lines = vec![format!("## Review Comments ({})", comments.len()), String::new()];
	for comment in comments {
		let author = format_author(comment.author.as_ref()).unwrap_or_else(|| "unknown".to_string());
		let created = comment
			.created_at
			.as_deref()
			.map_or(String::new(), |created| format!(" · {created}"));
		lines.push(format!("### {author}{created}"));
		lines.push(String::new());
		push_line(&mut lines, "Location", review_comment_location(comment));
		push_line(&mut lines, "Side", comment.side.clone());
		push_line(&mut lines, "Reply to", comment.in_reply_to_id);
		push_line(&mut lines, "URL", comment.url.clone());
		lines.push(String::new());
		let body = normalize_text(comment.body.as_deref());
		lines.push(if body.is_empty() { "No review comment body.".to_string() } else { body });
		lines.push(String::new());
	}
	Some(lines)
}

fn format_pr_files(files: Option<&Vec<GhFile>>) -> Vec<String> {
	let Some(files) = files.filter(|files| !files.is_empty()) else {
		return Vec::new();
	};
	let mut lines = vec![format!("## Files ({})", files.len()), String::new()];
	for file in files.iter().take(FILE_PREVIEW_LIMIT) {
		let path = file.path.as_deref().unwrap_or("(unknown file)");
		let change_type = file.change_type.as_deref().unwrap_or("CHANGED");
		lines.push(format!(
			"- {path} [{change_type}] (+{} -{})",
			file.additions.unwrap_or(0),
			file.deletions.unwrap_or(0)
		));
	}
	if files.len() > FILE_PREVIEW_LIMIT {
		lines.push(format!("[…{} files elided…]", files.len() - FILE_PREVIEW_LIMIT));
	}
	lines
}

fn format_issue_view(data: &GhIssueView, identifier: &str, include_comments: bool) -> String {
	let mut lines = Vec::new();
	let number = data
		.number
		.map_or_else(|| identifier.to_string(), |number| number.to_string());
	let title = data.title.clone().unwrap_or_else(|| "Untitled".to_string());
	lines.push(format!("# Issue #{number}: {title}"));
	lines.push(String::new());
	push_line(&mut lines, "State", data.state.clone());
	push_line(&mut lines, "State reason", data.state_reason.clone());
	push_line(&mut lines, "Author", format_author(data.author.as_ref()));
	push_line(&mut lines, "Created", data.created_at.clone());
	push_line(&mut lines, "Updated", data.updated_at.clone());
	push_line(&mut lines, "Labels", format_labels(data.labels.as_ref()));
	push_line(&mut lines, "URL", data.url.clone());
	lines.push(String::new());
	lines.push("## Body".to_string());
	lines.push(String::new());
	let body = normalize_text(data.body.as_deref());
	lines.push(if body.is_empty() { "No description provided.".to_string() } else { body });
	if include_comments {
		if let Some(section) = format_comments_section(data.comments.as_ref()) {
			lines.push(String::new());
			lines.extend(section);
		}
	}
	lines.join("\n").trim().to_string()
}

fn format_pr_view(data: &GhPrView, identifier: &str, include_comments: bool) -> String {
	let mut lines = Vec::new();
	let number = data
		.number
		.map_or_else(|| identifier.to_string(), |number| number.to_string());
	let title = data.title.clone().unwrap_or_else(|| "Untitled".to_string());
	lines.push(format!("# Pull Request #{number}: {title}"));
	lines.push(String::new());
	push_line(&mut lines, "State", data.state.clone());
	push_line(&mut lines, "Draft", data.is_draft);
	push_line(&mut lines, "Author", format_author(data.author.as_ref()));
	push_line(&mut lines, "Base", data.base_ref_name.clone());
	push_line(&mut lines, "Head", data.head_ref_name.clone());
	push_line(&mut lines, "Review decision", data.review_decision.clone());
	push_line(&mut lines, "Merge state", data.merge_state_status.clone());
	push_line(&mut lines, "Created", data.created_at.clone());
	push_line(&mut lines, "Updated", data.updated_at.clone());
	push_line(&mut lines, "Labels", format_labels(data.labels.as_ref()));
	push_line(&mut lines, "URL", data.url.clone());
	lines.push(String::new());
	lines.push("## Body".to_string());
	lines.push(String::new());
	let body = normalize_text(data.body.as_deref());
	lines.push(if body.is_empty() { "No description provided.".to_string() } else { body });

	let file_section = format_pr_files(data.files.as_ref());
	if !file_section.is_empty() {
		lines.push(String::new());
		lines.extend(file_section);
	}
	if include_comments {
		if let Some(section) = format_reviews_section(data.reviews.as_ref()) {
			lines.push(String::new());
			lines.extend(section);
		}
		if let Some(section) = format_review_comments_section(data.review_comments.as_ref()) {
			lines.push(String::new());
			lines.extend(section);
		}
		if let Some(section) = format_comments_section(data.comments.as_ref()) {
			lines.push(String::new());
			lines.extend(section);
		}
	}
	lines.join("\n").trim().to_string()
}

fn format_list_item(scheme: &str, repo: &str, item: &ListItem) -> String {
	let number = item
		.number
		.map_or_else(|| "?".to_string(), |number| number.to_string());
	let title = item.title.clone().unwrap_or_else(|| "(no title)".to_string());
	let state = item
		.state
		.as_deref()
		.map(str::to_lowercase)
		.unwrap_or_else(|| "?".to_string());
	let author = item
		.author
		.as_ref()
		.and_then(|author| author.login.clone())
		.filter(|login| !login.is_empty())
		.unwrap_or_else(|| "?".to_string());
	let updated = item
		.updated_at
		.clone()
		.or_else(|| item.created_at.clone())
		.unwrap_or_default();
	let draft = if scheme == "pr" && item.is_draft == Some(true) { " [draft]" } else { "" };
	let labels = item
		.labels
		.as_ref()
		.map(|labels| {
			labels
				.iter()
				.filter_map(|label| label.name.as_deref())
				.filter(|name| !name.is_empty())
				.collect::<Vec<_>>()
				.join(", ")
		})
		.unwrap_or_default();
	let label_suffix = if labels.is_empty() { String::new() } else { format!("  labels: {labels}") };
	let item_url = if number == "?" {
		format!("{scheme}://{repo}")
	} else {
		format!("{scheme}://{repo}/{number}")
	};
	format!(
		"- [{state}{draft}] #{number}  @{author}  {updated}\n    {title}{label_suffix}\n    {item_url}"
	)
}

// ────────────────────────────────────────────────────────────────────────────
// Fetch + render (`issue-pr-protocol.ts:339-620`)
// ────────────────────────────────────────────────────────────────────────────

#[allow(
	clippy::too_many_arguments,
	reason = "one argument per parsed list option keeps the upstream shape readable"
)]
async fn fetch_and_render_list(
	scheme: &str,
	href: &str,
	cwd: &Path,
	signal: &CancellationToken,
	repo: Option<String>,
	state: String,
	limit: u32,
	author: Option<String>,
	label: Option<String>,
) -> Result<InternalResource, String> {
	let resolved_repo = match repo {
		Some(repo) => repo,
		None => resolve_default_repo(cwd, signal).await.map_err(|message| {
			format!(
				"{scheme}:// could not resolve a default repo from the current session: \
				 {message}\nUse {scheme}://<owner>/<repo> instead."
			)
		})?,
	};
	let fields: &[&str] = if scheme == "issue" { &ISSUE_LIST_FIELDS } else { &PR_LIST_FIELDS };
	let mut args = vec![
		scheme.to_string(),
		"list".to_string(),
		"--repo".to_string(),
		resolved_repo.clone(),
		"--state".to_string(),
		state.clone(),
		"--limit".to_string(),
		limit.to_string(),
		"--json".to_string(),
		fields.join(","),
	];
	if let Some(author) = author {
		args.push("--author".to_string());
		args.push(author);
	}
	if let Some(label) = label {
		args.push("--label".to_string());
		args.push(label);
	}
	let raw = if scheme == "issue" {
		issue_json_with_state_reason_fallback(cwd, &args, signal, true).await?
	} else {
		run_gh_json(cwd, &args, signal, true).await?
	};
	let items: Vec<ListItem> = serde_json::from_str(&raw).map_err(|_| GH_INVALID_JSON.to_string())?;
	let noun = if scheme == "issue" { "Issues" } else { "Pull Requests" };
	let header =
		format!("# {noun} in {resolved_repo} ({state}, up to {limit})");
	let body = if items.is_empty() {
		"_No matches._".to_string()
	} else {
		items
			.iter()
			.map(|item| format_list_item(scheme, &resolved_repo, item))
			.collect::<Vec<_>>()
			.join("\n\n")
	};
	let footer = format!(
		"\n\n---\nRead a specific item: `{scheme}://{resolved_repo}/<N>` (or `{scheme}://<N>` for \
		 the current repo)."
	);
	let rendered = format!("{header}\n\n{body}{footer}");
	Ok(text_resource(
		href,
		rendered,
		"text/markdown",
		vec![format!("Live listing for {resolved_repo}")],
	))
}

async fn fetch_issue_view(
	href: &str,
	cwd: &Path,
	signal: &CancellationToken,
	repo: Option<String>,
	number: u64,
	comments: bool,
) -> Result<InternalResource, String> {
	let identifier = number.to_string();
	// Best-effort default repo: a resolution failure falls through to a direct
	// fetch so `gh` produces its own error instead of a friendlier masked one.
	let repo = match repo {
		Some(repo) => Some(repo),
		None => resolve_default_repo(cwd, signal).await.ok(),
	};
	let mut args = vec!["issue".to_string(), "view".to_string(), identifier.clone()];
	append_repo_flag(&mut args, repo.as_deref(), &identifier);
	let fields: &[&str] = if comments { &GH_ISSUE_FIELDS } else { &GH_ISSUE_FIELDS_NO_COMMENTS };
	args.push("--json".to_string());
	args.push(fields.join(","));
	let raw = issue_json_with_state_reason_fallback(cwd, &args, signal, repo.is_some()).await?;
	let data: GhIssueView =
		serde_json::from_str(&raw).map_err(|_| GH_INVALID_JSON.to_string())?;
	let rendered = format_issue_view(&data, &identifier, comments);
	let mut notes = vec!["Fetched live".to_string()];
	if !comments {
		notes.push("Comments disabled".to_string());
	}
	Ok(text_resource(href, rendered, "text/markdown", notes))
}

async fn fetch_pr_view(
	href: &str,
	cwd: &Path,
	signal: &CancellationToken,
	repo: &str,
	number: u64,
	comments: bool,
) -> Result<InternalResource, String> {
	let identifier = number.to_string();
	let mut args = vec!["pr".to_string(), "view".to_string(), identifier.clone()];
	append_repo_flag(&mut args, Some(repo), &identifier);
	let fields: &[&str] = if comments { &GH_PR_FIELDS } else { &GH_PR_FIELDS_NO_COMMENTS };
	args.push("--json".to_string());
	args.push(fields.join(","));
	let raw = run_gh_json(cwd, &args, signal, true).await?;
	let mut data: GhPrView =
		serde_json::from_str(&raw).map_err(|_| GH_INVALID_JSON.to_string())?;
	if comments {
		if let Some(pr_number) = data.number {
			data.review_comments = Some(fetch_pr_review_comments(cwd, repo, pr_number, signal).await?);
		}
	}
	let rendered = format_pr_view(&data, &identifier, comments);
	let mut notes = vec!["Fetched live".to_string()];
	if !comments {
		notes.push("Comments disabled".to_string());
	}
	notes.push(format!("Diff: pr://{repo}/{number}/diff"));
	Ok(text_resource(href, rendered, "text/markdown", notes))
}

async fn fetch_pr_review_comments(
	cwd: &Path,
	repo: &str,
	number: u64,
	signal: &CancellationToken,
) -> Result<Vec<GhReviewComment>, String> {
	let (host, slug) = parse_repo_ref(repo);
	let mut review_comments = Vec::new();
	let mut page: u64 = 1;
	loop {
		let mut args = vec!["api".to_string()];
		if let Some(host) = host.as_deref() {
			args.push("--hostname".to_string());
			args.push(host.to_string());
		}
		args.extend([
			"--method".to_string(),
			"GET".to_string(),
			format!("/repos/{slug}/pulls/{number}/comments"),
			"-F".to_string(),
			format!("per_page={REVIEW_COMMENTS_PAGE_SIZE}"),
			"-F".to_string(),
			format!("page={page}"),
		]);
		let raw = run_gh_json(cwd, &args, signal, true).await?;
		let response: Vec<GhReviewCommentApi> =
			serde_json::from_str(&raw).map_err(|_| GH_INVALID_JSON.to_string())?;
		let page_len = response.len();
		review_comments.extend(response.into_iter().filter_map(normalize_review_comment));
		// Compare the raw page length: a dropped malformed item must not end
		// pagination early and silently lose the remaining pages.
		if page_len < REVIEW_COMMENTS_PAGE_SIZE {
			break;
		}
		page += 1;
	}
	Ok(review_comments)
}

fn normalize_review_comment(comment: GhReviewCommentApi) -> Option<GhReviewComment> {
	comment.id?;
	Some(GhReviewComment {
		author:         comment.user,
		body:           comment.body,
		created_at:     comment.created_at,
		in_reply_to_id: comment.in_reply_to_id,
		line:           comment.line,
		original_line:  comment.original_line,
		path:           comment.path,
		side:           comment.side,
		url:            comment.html_url,
	})
}

async fn fetch_pr_diff_resource(
	href: &str,
	cwd: &Path,
	signal: &CancellationToken,
	repo: Option<String>,
	number: u64,
	mode: PrDiffMode,
) -> Result<InternalResource, String> {
	let repo = match repo {
		Some(repo) => repo,
		None => resolve_default_repo(cwd, signal).await.map_err(|message| {
			format!(
				"pr://{number}/diff could not resolve a default repo from the current session: \
				 {message}\nUse pr://<owner>/<repo>/{number}/diff."
			)
		})?,
	};
	let unified = fetch_pr_diff(cwd, &repo, number, signal).await?;
	let files = parse_pr_unified_diff(&unified);
	let file_count = files.len();
	let plural = if file_count == 1 { "" } else { "s" };
	match mode {
		PrDiffMode::All => Ok(text_resource(
			href,
			unified,
			"text/plain",
			vec![
				"Fetched live".to_string(),
				format!("Full diff for pr://{repo}/{number} ({file_count} file{plural})"),
			],
		)),
		PrDiffMode::Slice(index) => {
			if index < 1 || index > file_count as u64 {
				return Err(format!(
					"pr://{repo}/{number}/diff/{index} is out of range; PR has {file_count} \
					 file{plural}. Use pr://{repo}/{number}/diff to list available indices."
				));
			}
			let Some(file) = files.get((index - 1) as usize) else {
				return Err(format!(
					"pr://{repo}/{number}/diff/{index} resolved to a missing slice (parser bug)."
				));
			};
			let content = unified
				.get(file.start_offset..file.end_offset)
				.map(str::to_string)
				.unwrap_or_default();
			Ok(text_resource(
				href,
				content,
				"text/plain",
				vec![
					"Fetched live".to_string(),
					format!("Showing file {index}/{file_count}: {}", file.path),
					format!("Read all: pr://{repo}/{number}/diff/all"),
				],
			))
		},
		PrDiffMode::List => {
			let header = format!(
				"# Pull Request Diff: {repo}#{number} ({file_count} file{plural})"
			);
			let body = if files.is_empty() {
				"_No file changes._".to_string()
			} else {
				files
					.iter()
					.enumerate()
					.map(|(index, file)| format_file_line(index + 1, file, &repo, number))
					.collect::<Vec<_>>()
					.join("\n\n")
			};
			let footer = format!(
				"\n\n---\nRead all: `pr://{repo}/{number}/diff/all`. Each file is also available as \
				 `pr://{repo}/{number}/diff/<i>`."
			);
			Ok(text_resource(
				href,
				format!("{header}\n\n{body}{footer}"),
				"text/markdown",
				vec![
					"Fetched live".to_string(),
					format!("File listing for pr://{repo}/{number}"),
				],
			))
		},
	}
}

fn text_resource(
	href: &str,
	content: String,
	content_type: &str,
	notes: Vec<String>,
) -> InternalResource {
	InternalResource {
		size: content.len(),
		url: href.to_string(),
		content,
		content_type: content_type.to_string(),
		source_path: None,
		notes: Some(notes),
		immutable: true,
		is_directory: false,
	}
}

fn append_repo_flag(args: &mut Vec<String>, repo: Option<&str>, identifier: &str) {
	// A full URL identifier already names host, repo, and number.
	let Some(repo) = repo else {
		return;
	};
	if identifier.starts_with("https://") {
		return;
	}
	args.push("--repo".to_string());
	args.push(repo.to_string());
}

// ────────────────────────────────────────────────────────────────────────────
// `gh` runner (`utils/github.ts:42-180`)
// ────────────────────────────────────────────────────────────────────────────

struct GhRun {
	status: ExitStatus,
	stdout: String,
	stderr: String,
}

/// Run `gh` argv in `cwd`: non-interactive env, stdin closed, both streams
/// capped at 8 MiB, 5-minute deadline, and a fresh process group so cancel or
/// timeout kills and reaps the whole tree.
async fn run_gh(
	cwd: &Path,
	args: &[String],
	signal: &CancellationToken,
) -> Result<GhRun, String> {
	if signal.is_cancelled() {
		return Err("aborted".to_string());
	}
	let mut command = Command::new("gh");
	command
		.args(args)
		.current_dir(cwd)
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.kill_on_drop(true)
		.env_remove("LC_ALL")
		.envs(GH_ENV);
	#[cfg(unix)]
	command.process_group(0);
	let mut child = command.spawn().map_err(|error| {
		if error.kind() == ErrorKind::NotFound {
			GH_NOT_INSTALLED.to_string()
		} else {
			format!("Failed to start GitHub CLI: {error}")
		}
	})?;
	let pid = child.id();
	let stdout = child.stdout.take().ok_or("Failed to capture GitHub CLI output.")?;
	let stderr = child.stderr.take().ok_or("Failed to capture GitHub CLI output.")?;
	let out = tokio::spawn(read_capped(stdout));
	let err = tokio::spawn(read_capped(stderr));
	let waited = tokio::select! {
		() = signal.cancelled() => None,
		result = timeout(GH_DEADLINE, child.wait()) => Some(result),
	};
	let status = match waited {
		None => {
			kill_tree(&mut child, pid).await;
			let _ = out.await;
			let _ = err.await;
			return Err("aborted".to_string());
		},
		Some(Ok(Ok(status))) => status,
		Some(Ok(Err(error))) => {
			kill_tree(&mut child, pid).await;
			let _ = out.await;
			let _ = err.await;
			return Err(format!("GitHub CLI command failed: {error}"));
		},
		Some(Err(_)) => {
			kill_tree(&mut child, pid).await;
			let _ = out.await;
			let _ = err.await;
			return Err(format!("GitHub CLI command timed out: gh {}", args.join(" ")));
		},
	};
	let stdout = out
		.await
		.map_err(|error| format!("Failed to capture GitHub CLI output: {error}"))??;
	let stderr = err
		.await
		.map_err(|error| format!("Failed to capture GitHub CLI output: {error}"))??;
	if signal.is_cancelled() {
		return Err("aborted".to_string());
	}
	Ok(GhRun { status, stdout, stderr })
}

async fn read_capped<R: AsyncRead + Unpin>(mut reader: R) -> std::io::Result<String> {
	let mut captured = Vec::new();
	let mut total = 0usize;
	let mut buffer = vec![0u8; 64 * 1024];
	loop {
		let read = reader.read(&mut buffer).await?;
		if read == 0 {
			break;
		}
		total += read;
		if captured.len() < GH_OUTPUT_LIMIT {
			let take = (GH_OUTPUT_LIMIT - captured.len()).min(read);
			captured.extend_from_slice(&buffer[..take]);
		}
	}
	let mut text = String::from_utf8_lossy(&captured).into_owned();
	if total > GH_OUTPUT_LIMIT {
		text.push_str(GH_TRUNCATED_MARKER);
	}
	Ok(text)
}

async fn kill_tree(child: &mut Child, pid: Option<u32>) {
	#[cfg(unix)]
	if let Some(pid) = pid.filter(|pid| *pid > 1) {
		// SAFETY: signalling the process group we created; the negative pid is
		// guarded against the catastrophic 0/1 targets.
		unsafe {
			libc::kill(-(pid as i32), libc::SIGKILL);
		}
	}
	let _ = child.start_kill();
	let _ = child.wait().await;
}

/// Run `gh`, treat a non-zero exit as [`map_gh_failure`], require non-empty
/// valid JSON, and return the JSON text.
pub(crate) async fn run_gh_json(
	cwd: &Path,
	args: &[String],
	signal: &CancellationToken,
	repo_provided: bool,
) -> Result<String, String> {
	let run = run_gh(cwd, args, signal).await?;
	if !run.status.success() {
		return Err(map_gh_failure(args, &run.stdout, &run.stderr, repo_provided));
	}
	let stdout = run.stdout.trim();
	if stdout.is_empty() {
		return Err(GH_EMPTY_OUTPUT.to_string());
	}
	serde_json::from_str::<Value>(stdout).map_err(|_| GH_INVALID_JSON.to_string())?;
	Ok(stdout.to_string())
}

async fn run_gh_text(
	cwd: &Path,
	args: &[String],
	signal: &CancellationToken,
	repo_provided: bool,
	trim: bool,
) -> Result<String, String> {
	let run = run_gh(cwd, args, signal).await?;
	if !run.status.success() {
		return Err(map_gh_failure(args, &run.stdout, &run.stderr, repo_provided));
	}
	Ok(if trim { run.stdout.trim().to_string() } else { run.stdout })
}

/// `formatGhFailure` (`utils/github.ts:53-67`), verbatim.
pub(crate) fn map_gh_failure(
	args: &[String],
	stdout: &str,
	stderr: &str,
	repo_provided: bool,
) -> String {
	let raw = if stderr.trim().is_empty() { stdout } else { stderr };
	let message = raw.trim();
	if message.contains("gh auth login") || message.contains("not logged into any GitHub hosts") {
		return "GitHub CLI is not authenticated. Run `gh auth login`.".to_string();
	}
	if !repo_provided
		&& (message.contains("not a git repository")
			|| message.contains("no git remotes found")
			|| message.contains("unable to determine current repository"))
	{
		return "GitHub repository context is unavailable. Pass `repo` explicitly or run the tool \
		        inside a GitHub checkout."
			.to_string();
	}
	if !message.is_empty() {
		return message.to_string();
	}
	format!("GitHub CLI command failed: gh {}", args.join(" "))
}

fn gh_json_error_names_field(error: &str, field: &str) -> bool {
	error.contains("Unknown JSON field")
		&& (error.contains(&format!("\"{field}\""))
			|| error.contains(&format!("'{field}'"))
			|| error.contains(field))
}

fn drop_json_field(args: &[String], field: &str) -> Option<Vec<String>> {
	let mut next = args.to_vec();
	let json_index = next.iter().position(|arg| arg == "--json")?;
	let fields = next.get(json_index + 1)?;
	let split_fields: Vec<&str> = fields.split(',').collect();
	let kept: Vec<&str> = split_fields.iter().copied().filter(|candidate| *candidate != field).collect();
	if kept.len() == split_fields.len() {
		return None;
	}
	next[json_index + 1] = kept.join(",");
	Some(next)
}

/// `githubIssueJsonWithStateReasonFallback` (`gh-view.ts:66-80`): retry
/// without the optional `stateReason` field on older `gh` releases.
async fn issue_json_with_state_reason_fallback(
	cwd: &Path,
	args: &[String],
	signal: &CancellationToken,
	repo_provided: bool,
) -> Result<String, String> {
	match run_gh_json(cwd, args, signal, repo_provided).await {
		Err(error) if gh_json_error_names_field(&error, "stateReason") => {
			let Some(retry_args) = drop_json_field(args, "stateReason") else {
				return Err(error);
			};
			run_gh_json(cwd, &retry_args, signal, repo_provided).await
		},
		other => other,
	}
}

/// `defaultGhHost` (`gh-common.ts:198-200`), read from the child's inherited
/// environment.
fn default_gh_host() -> String {
	std::env::var("GH_HOST").map_or_else(|_| "github.com".to_string(), |host| host.to_lowercase())
}

/// `repoFromUrl` (`gh-common.ts:110-119`): keep the host unless `gh` would
/// have assumed it, so a bare identity is never redirected by `GH_HOST`.
fn repo_from_url(value: &str) -> Option<String> {
	let rest = value
		.trim()
		.strip_prefix("https://")
		.or_else(|| value.trim().strip_prefix("http://"))?;
	let mut segments = rest.split('/');
	let host = segments.next()?;
	let owner = segments.next()?;
	let repo = segments.next()?;
	if host.is_empty() || owner.is_empty() || repo.is_empty() {
		return None;
	}
	let repo = &repo[..repo.find(['?', '#']).unwrap_or(repo.len())];
	if repo.is_empty() {
		return None;
	}
	let host = host.to_lowercase();
	let slug = format!("{owner}/{repo}");
	Some(if host == default_gh_host() { slug } else { format!("{host}/{slug}") })
}

/// `resolveRepoFromCwd` (`gh-common.ts:224-231`): the checkout's
/// `[HOST/]OWNER/REPO`, host included so an enterprise checkout is never
/// looked up on github.com.
async fn resolve_default_repo(cwd: &Path, signal: &CancellationToken) -> Result<String, String> {
	let args: Vec<String> = ["repo", "view", "--json", "url", "-q", ".url"]
		.iter()
		.map(|arg| (*arg).to_string())
		.collect();
	let url = run_gh_text(cwd, &args, signal, false, true).await?;
	if url.trim().is_empty() {
		return Err("repo must not be empty".to_string());
	}
	repo_from_url(&url)
		.ok_or_else(|| format!("GitHub CLI returned an unrecognized repository URL: {url}"))
}

// ────────────────────────────────────────────────────────────────────────────
// PR diff (`tools/gh-pr-diff.ts`)
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangeType {
	Modified,
	Added,
	Deleted,
	Renamed,
	Binary,
}

impl ChangeType {
	fn as_str(self) -> &'static str {
		match self {
			Self::Modified => "modified",
			Self::Added => "added",
			Self::Deleted => "deleted",
			Self::Renamed => "renamed",
			Self::Binary => "binary",
		}
	}
}

#[derive(Debug, Clone)]
struct PrDiffFile {
	path:         String,
	additions:    u64,
	deletions:    u64,
	change_type:  ChangeType,
	old_path:     Option<String>,
	start_offset: usize,
	end_offset:   usize,
}

fn format_file_line(index: usize, file: &PrDiffFile, repo: &str, number: u64) -> String {
	let stats = if file.change_type == ChangeType::Binary {
		"(binary)".to_string()
	} else {
		format!("+{} -{}", file.additions, file.deletions)
	};
	let rename = file
		.old_path
		.as_deref()
		.map_or(String::new(), |old| format!("  (renamed from {old})"));
	format!(
		"{index}. {}  {stats}  [{}]{rename}\n   pr://{repo}/{number}/diff/{index}",
		file.path,
		file.change_type.as_str()
	)
}

async fn fetch_pr_diff(
	cwd: &Path,
	repo: &str,
	number: u64,
	signal: &CancellationToken,
) -> Result<String, String> {
	let mut args = vec![
		"pr".to_string(),
		"diff".to_string(),
		number.to_string(),
		"--color".to_string(),
		"never".to_string(),
	];
	append_repo_flag(&mut args, Some(repo), &number.to_string());
	match run_gh_text(cwd, &args, signal, true, false).await {
		Ok(text) => Ok(text),
		Err(error) if is_pr_diff_too_large_error(&error) => {
			fetch_pr_diff_via_files_api(cwd, repo, number, signal).await
		},
		Err(error) => Err(error),
	}
}

/// `isPrDiffTooLargeError` (`gh-pr-diff.ts:316-324`): GitHub rejects the
/// aggregate diff with HTTP 406 past 20,000 lines.
fn is_pr_diff_too_large_error(message: &str) -> bool {
	message.contains("HTTP 406")
		|| message.to_lowercase().contains("exceeded the maximum number of lines")
		|| message.contains("too_large")
}

/// `fetchPrDiffViaFilesApi` (`gh-pr-diff.ts:374-430`): rebuild a synthetic
/// unified diff from the per-file endpoint when the aggregate diff is too big.
async fn fetch_pr_diff_via_files_api(
	cwd: &Path,
	repo: &str,
	number: u64,
	signal: &CancellationToken,
) -> Result<String, String> {
	let (host, slug) = parse_repo_ref(repo);
	let mut pull_args = vec!["api".to_string()];
	if let Some(host) = host.as_deref() {
		pull_args.push("--hostname".to_string());
		pull_args.push(host.to_string());
	}
	pull_args.extend([
		"--method".to_string(),
		"GET".to_string(),
		format!("/repos/{slug}/pulls/{number}"),
	]);
	let raw = run_gh_json(cwd, &pull_args, signal, true).await?;
	let pull: Value = serde_json::from_str(&raw).map_err(|_| GH_INVALID_JSON.to_string())?;
	let changed_files = pull.get("changed_files").and_then(Value::as_u64).unwrap_or(0);
	if changed_files > PR_DIFF_FILES_MAX {
		return Err(format!(
			"Pull request changes {changed_files} files, exceeding GitHub's {PR_DIFF_FILES_MAX}-file \
			 limit for the per-file diff API."
		));
	}
	let mut sections = Vec::new();
	let mut page: u64 = 1;
	loop {
		let mut args = vec!["api".to_string()];
		if let Some(host) = host.as_deref() {
			args.push("--hostname".to_string());
			args.push(host.to_string());
		}
		args.extend([
			"--method".to_string(),
			"GET".to_string(),
			format!("/repos/{slug}/pulls/{number}/files"),
			"-F".to_string(),
			format!("per_page={PR_DIFF_FILES_PAGE_SIZE}"),
			"-F".to_string(),
			format!("page={page}"),
		]);
		let raw = run_gh_json(cwd, &args, signal, true).await?;
		let response: Vec<GhFileApi> =
			serde_json::from_str(&raw).map_err(|_| GH_INVALID_JSON.to_string())?;
		let page_len = response.len();
		for file in response {
			if let Some(section) = build_synthetic_diff_section(&file) {
				sections.push(section);
			}
		}
		if page_len < PR_DIFF_FILES_PAGE_SIZE {
			break;
		}
		page += 1;
	}
	// Trailing newline mirrors `gh pr diff` so downstream parsers splitting on
	// `^diff --git ` see identical boundaries.
	Ok(if sections.is_empty() { String::new() } else { format!("{}\n", sections.join("\n")) })
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct GhFileApi {
	filename:          Option<String>,
	previous_filename: Option<String>,
	status:            Option<String>,
	additions:         Option<u64>,
	deletions:         Option<u64>,
	patch:             Option<String>,
}

fn build_synthetic_diff_section(file: &GhFileApi) -> Option<String> {
	let new_path = file.filename.as_deref()?;
	let status = file.status.as_deref().unwrap_or("modified");
	let old_path = file.previous_filename.as_deref().unwrap_or(new_path);
	let old_diff_path = format_synthetic_diff_path("a/", old_path);
	let new_diff_path = format_synthetic_diff_path("b/", new_path);
	let mut lines = vec![format!("diff --git {old_diff_path} {new_diff_path}")];
	match status {
		"added" => lines.push("new file mode 100644".to_string()),
		"removed" => lines.push("deleted file mode 100644".to_string()),
		_ if status == "renamed" || file.previous_filename.is_some() => {
			lines.push(format!("rename from {old_path}"));
			lines.push(format!("rename to {new_path}"));
		},
		_ => {},
	}
	match file.patch.as_deref().filter(|patch| !patch.is_empty()) {
		Some(patch) => {
			lines.push(if status == "added" {
				"--- /dev/null".to_string()
			} else {
				format!("--- {old_diff_path}")
			});
			lines.push(if status == "removed" {
				"+++ /dev/null".to_string()
			} else {
				format!("+++ {new_diff_path}")
			});
			lines.push(patch.to_string());
		},
		None => lines.push(format!(
			"* patch unavailable (binary or too large); additions {}, deletions {}",
			file.additions.unwrap_or(0),
			file.deletions.unwrap_or(0)
		)),
	}
	Some(lines.join("\n"))
}

/// `formatSyntheticDiffPath` (`gh-pr-diff.ts:326-356`).
fn format_synthetic_diff_path(prefix: &str, path: &str) -> String {
	let prefixed = format!("{prefix}{path}");
	if !prefixed.chars().any(|ch| ch.is_control() || ch.is_whitespace() || ch == '"' || ch == '\\') {
		return prefixed;
	}
	let mut escaped = String::new();
	for ch in prefixed.chars() {
		match ch {
			'\\' => escaped.push_str("\\\\"),
			'"' => escaped.push_str("\\\""),
			'\n' => escaped.push_str("\\n"),
			'\r' => escaped.push_str("\\r"),
			'\t' => escaped.push_str("\\t"),
			other if (other as u32) < 32 => {
				escaped.push_str(&format!("\\{:03o}", other as u32));
			},
			other => escaped.push(other),
		}
	}
	format!("\"{escaped}\"")
}

fn parse_pr_unified_diff(text: &str) -> Vec<PrDiffFile> {
	let mut starts = Vec::new();
	if text.starts_with("diff --git ") {
		starts.push(0);
	}
	let mut from = 0;
	while let Some(relative) = text[from..].find("\ndiff --git ") {
		let index = from + relative + 1;
		starts.push(index);
		from = index;
	}
	let mut files = Vec::with_capacity(starts.len());
	for (position, start) in starts.iter().copied().enumerate() {
		let end = starts.get(position + 1).copied().unwrap_or(text.len());
		files.push(parse_pr_diff_section(&text[start..end], start, end));
	}
	files
}

fn parse_pr_diff_section(section: &str, start_offset: usize, end_offset: usize) -> PrDiffFile {
	let mut lines = section.split('\n');
	let header = lines.next().unwrap_or("");
	let (mut old_path, mut new_path) = parse_pr_diff_header_paths(header);
	let mut change_type = ChangeType::Modified;
	let mut is_binary = false;
	let mut additions = 0u64;
	let mut deletions = 0u64;
	let mut in_hunk = false;
	for line in lines {
		if line.starts_with("new file mode") {
			change_type = ChangeType::Added;
			continue;
		}
		if line.starts_with("deleted file mode") {
			change_type = ChangeType::Deleted;
			continue;
		}
		if let Some(rest) = line.strip_prefix("rename from ") {
			change_type = ChangeType::Renamed;
			old_path = Some(rest.to_string());
			continue;
		}
		if let Some(rest) = line.strip_prefix("rename to ") {
			new_path = Some(rest.to_string());
			continue;
		}
		if line.starts_with("Binary files ") && line.ends_with(" differ") {
			is_binary = true;
			continue;
		}
		if line.starts_with("@@ ") {
			in_hunk = true;
			continue;
		}
		if !in_hunk && is_pr_diff_file_header_line(line) {
			continue;
		}
		if line.starts_with('+') {
			additions += 1;
		} else if line.starts_with('-') {
			deletions += 1;
		}
	}
	if is_binary {
		if change_type == ChangeType::Modified {
			change_type = ChangeType::Binary;
		}
		additions = 0;
		deletions = 0;
	}
	let display_path = if change_type == ChangeType::Deleted {
		old_path.clone().or_else(|| new_path.clone())
	} else {
		new_path.clone().or_else(|| old_path.clone())
	}
	.unwrap_or_else(|| "(unknown)".to_string());
	let old_path = old_path.filter(|old| *old != display_path);
	PrDiffFile {
		path: display_path,
		additions,
		deletions,
		change_type,
		old_path,
		start_offset,
		end_offset,
	}
}

fn is_pr_diff_file_header_line(line: &str) -> bool {
	line == "--- /dev/null"
		|| line == "+++ /dev/null"
		|| line.starts_with("--- a/")
		|| line.starts_with("+++ b/")
		|| line.starts_with("--- \"a/")
		|| line.starts_with("+++ \"b/")
}

fn strip_diff_path_prefix(value: &str, prefix: &str) -> Option<String> {
	value.strip_prefix(prefix).map(str::to_string)
}

fn parse_pr_diff_header_paths(header: &str) -> (Option<String>, Option<String>) {
	let Some(trail) = header.strip_prefix("diff --git ") else {
		return (None, None);
	};
	if trail.starts_with('"') {
		let Some((old_token, next_index)) = parse_diff_quoted_token(trail, 0) else {
			return (None, None);
		};
		let Some((new_token, _)) = parse_diff_header_token(trail, next_index) else {
			return (None, None);
		};
		return (
			strip_diff_path_prefix(&old_token, "a/"),
			strip_diff_path_prefix(&new_token, "b/"),
		);
	}
	if trail.starts_with("a/") {
		if let Some(index) = trail.find(" b/").filter(|index| *index > 0) {
			return (
				Some(trail[2..index].to_string()),
				Some(trail[index + 3..].to_string()),
			);
		}
	}
	(None, None)
}

fn parse_diff_header_token(text: &str, start_index: usize) -> Option<(String, usize)> {
	let mut start = start_index;
	while text[start..].starts_with(' ') {
		start += 1;
	}
	if start >= text.len() {
		return None;
	}
	if let Some(quoted) = parse_diff_quoted_token(text, start) {
		return Some(quoted);
	}
	match text[start..].find(' ') {
		Some(relative) => {
			let end = start + relative;
			Some((text[start..end].to_string(), end))
		},
		None => Some((text[start..].to_string(), text.len())),
	}
}

fn parse_diff_quoted_token(text: &str, start_index: usize) -> Option<(String, usize)> {
	if text.as_bytes().get(start_index) != Some(&b'"') {
		return None;
	}
	let mut value = String::new();
	let mut index = start_index + 1;
	while index < text.len() {
		let ch = text[index..].chars().next()?;
		if ch == '"' {
			return Some((value, index + 1));
		}
		if ch != '\\' {
			value.push(ch);
			index += ch.len_utf8();
			continue;
		}
		let (escaped, next_index) = parse_diff_quoted_escape(text, index);
		value.push_str(&escaped);
		index = next_index;
	}
	None
}

fn parse_diff_quoted_escape(text: &str, slash_index: usize) -> (String, usize) {
	let Some(next) = text[slash_index + 1..].chars().next() else {
		return ("\\".to_string(), slash_index + 1);
	};
	if ('0'..='7').contains(&next) {
		let start = slash_index + 1;
		let mut end = start;
		while end < text.len() && end < slash_index + 4 {
			let Some(digit) = text[end..].chars().next() else {
				break;
			};
			if !('0'..='7').contains(&digit) {
				break;
			}
			end += digit.len_utf8();
		}
		let value = u32::from_str_radix(&text[start..end], 8)
			.ok()
			.and_then(char::from_u32)
			.map_or_else(String::new, |ch| ch.to_string());
		return (value, end);
	}
	let value = match next {
		'a' => "\x07".to_string(),
		'b' => "\x08".to_string(),
		'f' => "\x0c".to_string(),
		'n' => "\n".to_string(),
		'r' => "\r".to_string(),
		't' => "\t".to_string(),
		'v' => "\x0b".to_string(),
		other => other.to_string(),
	};
	(value, slash_index + 1 + next.len_utf8())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn issue(href: &str) -> Result<GithubParsed, String> {
		parse_issue_pr_url(href, "issue")
	}

	fn pr(href: &str) -> Result<GithubParsed, String> {
		parse_issue_pr_url(href, "pr")
	}

	fn single(repo: Option<&str>, number: u64, comments: bool) -> GithubParsed {
		GithubParsed::Single { repo: repo.map(str::to_string), number, comments }
	}

	fn list(repo: Option<&str>, state: &str, limit: u32) -> GithubParsed {
		GithubParsed::List {
			repo:   repo.map(str::to_string),
			state:  state.to_string(),
			limit,
			author: None,
			label:  None,
		}
	}

	#[test]
	fn parses_every_url_shape() {
		assert_eq!(issue("issue://").unwrap(), list(None, "open", 30));
		assert_eq!(issue("issue://123").unwrap(), single(None, 123, true));
		assert_eq!(issue("issue://owner/repo").unwrap(), list(Some("owner/repo"), "open", 30));
		assert_eq!(
			issue("issue://ghe.example.com/owner/repo").unwrap(),
			list(Some("ghe.example.com/owner/repo"), "open", 30)
		);
		assert_eq!(issue("issue://o/r/123?comments=0").unwrap(), single(Some("o/r"), 123, false));
		assert_eq!(issue("issue://o/r/123?comments=FALSE").unwrap(), single(Some("o/r"), 123, false));
		assert_eq!(pr("pr://").unwrap(), list(None, "open", 30));
		assert_eq!(pr("pr://o/r").unwrap(), list(Some("o/r"), "open", 30));
		assert_eq!(
			pr("pr://5/diff").unwrap(),
			GithubParsed::PrDiff { repo: None, number: 5, mode: PrDiffMode::List }
		);
		assert_eq!(
			pr("pr://o/r/5/diff/all").unwrap(),
			GithubParsed::PrDiff { repo: Some("o/r".into()), number: 5, mode: PrDiffMode::All }
		);
		assert_eq!(
			pr("pr://o/r/5/diff/2").unwrap(),
			GithubParsed::PrDiff { repo: Some("o/r".into()), number: 5, mode: PrDiffMode::Slice(2) }
		);
		assert_eq!(
			pr("pr://ghe.example.com/o/r/5").unwrap(),
			single(Some("ghe.example.com/o/r"), 5, true)
		);
		// A single-label host is only recognizable in the numbered form.
		assert_eq!(pr("pr://ghe/o/r/5").unwrap(), single(Some("ghe/o/r"), 5, true));
		assert!(pr("pr://ghe/o/r").is_err());
	}

	#[test]
	fn rejects_unsafe_or_unknown_shapes() {
		assert_eq!(
			issue("issue://o/r/foo").unwrap_err(),
			"Invalid issue:// number: foo"
		);
		assert_eq!(
			issue("issue://o/r/1/diff").unwrap_err(),
			"Invalid issue:// URL. Issue views do not have a diff; use \
			 pr://<owner>/<repo>/<n>/diff for pull requests."
		);
		assert_eq!(
			pr("pr://o/r/5/diff/0").unwrap_err(),
			"Invalid pr:// diff sub-path '0'. Use 'all' or a 1-indexed file number."
		);
		assert!(pr("pr://o/r/5/diff/x").is_err());
		assert_eq!(
			pr("pr://o/r/%2e%2e").unwrap_err(),
			"Invalid pr:// URL: empty or unsafe path segment"
		);
		assert_eq!(
			pr("pr://o//77").unwrap_err(),
			"Invalid pr:// URL: empty or unsafe path segment"
		);
		assert!(pr("pr://o/r/5/extra/segment").is_err());
	}

	#[test]
	fn validates_list_options() {
		assert_eq!(
			issue("issue://o/r?state=bogus").unwrap_err(),
			"Invalid issue:// list state 'bogus'. Expected one of: open, closed, all."
		);
		assert_eq!(
			issue("issue://o/r?limit=999").unwrap(),
			list(Some("o/r"), "open", 100)
		);
		assert_eq!(
			issue("issue://o/r?limit=abc").unwrap_err(),
			"Invalid issue:// list limit 'abc'. Expected a positive integer (max 100)."
		);
		assert_eq!(
			pr("pr://o/r?state=merged&limit=7&author=octocat&label=bug").unwrap(),
			GithubParsed::List {
				repo:   Some("o/r".into()),
				state:  "merged".into(),
				limit:  7,
				author: Some("octocat".into()),
				label:  Some("bug".into()),
			}
		);
	}

	#[test]
	fn parses_positive_decimal_only() {
		assert_eq!(parse_positive_int("12"), Some(12));
		assert_eq!(parse_positive_int("0"), None);
		assert_eq!(parse_positive_int("-1"), None);
		assert_eq!(parse_positive_int("+1"), None);
		assert_eq!(parse_positive_int("1e2"), None);
		assert_eq!(parse_positive_int("0x10"), None);
		assert_eq!(parse_positive_int("12.0"), None);
		assert_eq!(parse_positive_int(""), None);
		assert_eq!(parse_positive_int("9007199254740992"), None);
	}

	#[test]
	fn maps_gh_failures_verbatim() {
		let args = vec!["issue".to_string(), "view".to_string()];
		assert_eq!(
			map_gh_failure(&args, "", "not logged into any GitHub hosts", true),
			"GitHub CLI is not authenticated. Run `gh auth login`."
		);
		assert_eq!(
			map_gh_failure(&args, "", "fatal: not a git repository", false),
			"GitHub repository context is unavailable. Pass `repo` explicitly or run the tool \
			 inside a GitHub checkout."
		);
		assert_eq!(
			map_gh_failure(&args, "", "fatal: not a git repository", true),
			"fatal: not a git repository"
		);
		assert_eq!(
			map_gh_failure(&args, "boom", "  ", true),
			"boom"
		);
		assert_eq!(
			map_gh_failure(&args, "", "", true),
			"GitHub CLI command failed: gh issue view"
		);
	}

	#[test]
	fn parses_diff_sections_and_offsets() {
		let unified = "diff --git a/src/a.rs b/src/a.rs\n\
		               index 1..2 100644\n\
		               --- a/src/a.rs\n\
		               +++ b/src/a.rs\n\
		               @@ -1 +1 @@\n\
		               -old\n\
		               +new\n\
		               diff --git a/img.png b/img.png\n\
		               new file mode 100644\n\
		               Binary files /dev/null and b/img.png differ\n";
		let files = parse_pr_unified_diff(unified);
		assert_eq!(files.len(), 2);
		assert_eq!(files[0].path, "src/a.rs");
		assert_eq!(files[0].change_type, ChangeType::Modified);
		assert_eq!(files[0].additions, 1);
		assert_eq!(files[0].deletions, 1);
		assert_eq!(files[0].start_offset, 0);
		assert_eq!(
			&unified[files[0].start_offset..files[0].end_offset],
			"diff --git a/src/a.rs b/src/a.rs\n\
			 index 1..2 100644\n\
			 --- a/src/a.rs\n\
			 +++ b/src/a.rs\n\
			 @@ -1 +1 @@\n\
			 -old\n\
			 +new\n"
		);
		assert_eq!(files[1].path, "img.png");
		assert_eq!(files[1].change_type, ChangeType::Added);
		assert_eq!(files[1].end_offset, unified.len());
		assert!(parse_pr_unified_diff("").is_empty());
		assert!(parse_pr_unified_diff("no diff here").is_empty());
	}

	#[test]
	fn parses_rename_and_quoted_paths() {
		let rename = "diff --git a/old.rs b/new.rs\nrename from old.rs\nrename to new.rs\n";
		let files = parse_pr_unified_diff(rename);
		assert_eq!(files[0].path, "new.rs");
		assert_eq!(files[0].old_path.as_deref(), Some("old.rs"));
		assert_eq!(files[0].change_type, ChangeType::Renamed);

		let quoted = "diff --git \"a/a b/c\" \"b/a b/d\"\n";
		let files = parse_pr_unified_diff(quoted);
		assert_eq!(files[0].path, "a b/d");
		assert_eq!(files[0].old_path.as_deref(), Some("a b/c"));
	}

	#[test]
	fn detects_only_aggregate_diff_limit() {
		assert!(is_pr_diff_too_large_error("HTTP 406: exceeded the maximum number of lines"));
		assert!(is_pr_diff_too_large_error("too_large"));
		assert!(!is_pr_diff_too_large_error("HTTP 401: Bad credentials"));
	}

	#[test]
	fn cancelled_resolve_never_spawns_gh() {
		let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
		runtime.block_on(async {
			let signal = CancellationToken::new();
			signal.cancel();
			let context = ResolveContext {
				cwd: std::env::temp_dir(),
				signal,
				..ResolveContext::default()
			};
			let url = InternalUrl::parse("issue://o/r/1").unwrap();
			assert_eq!(IssueUrlHandler.resolve(&url, &context).await.unwrap_err(), "aborted");
			let url = InternalUrl::parse("pr://o/r/1/diff").unwrap();
			assert_eq!(PrUrlHandler.resolve(&url, &context).await.unwrap_err(), "aborted");
		});
	}
}
