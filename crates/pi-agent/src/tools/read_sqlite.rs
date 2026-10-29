//! SQLite reads: bare database listings, `:table` schema/sample reads,
//! `:table:key` primary-key/rowid lookups, paginated structured queries, and
//! raw `?q=SELECT ...` reads.
//!
//! Mirrors `packages/coding-agent/src/tools/read-sqlite.ts` and
//! `sqlite-reader.ts`: path-candidate discovery (`db.sqlite:table?limit=2`),
//! selector parsing, query/order/where validation, 20/500/1000 row caps,
//! 120x40 ASCII table rendering with a vertical fallback, `sqlite_stat1`
//! row-count estimates, and WAL-sidecar initialization.
//!
//! Every connection is opened read-only and carries `PRAGMA query_only = ON`
//! plus a prepare-time authorizer that only permits reads and
//! `table_info`/`table_xinfo`, so a raw query cannot write, ATTACH/DETACH, or
//! flip pragmas that touch the database file. The single exception is the
//! upstream WAL initialization: a database whose header says WAL but whose
//! `-wal`/`-shm` sidecars are missing is opened read-write once so SQLite can
//! recreate them, exactly as `openSqliteReadConnection` does.
//!
//! ponytail: display width uses a compact East-Asian-width table instead of a
//! full UAX#11 crate (no new dependency); swap in a width crate if emoji ZWJ
//! sequences or rare combining scripts must measure exactly like the TUI.

use std::{
	collections::{HashMap, HashSet},
	fs::File,
	io::Read as _,
	path::{Path, PathBuf},
};

use rusqlite::{
	Connection, ErrorCode, OpenFlags, OptionalExtension,
	hooks::{AuthAction, AuthContext, Authorization},
	params,
	types::Value as SqlValue,
};
use serde_json::{Map, Value as JsonValue, json};
use tokio_util::sync::CancellationToken;

use super::read::{
	DEFAULT_MAX_BYTES, find_unique_workspace_suffix, format_bytes, is_not_found,
	meta_truncation_json, prepend_suffix_notice, resolve_path, truncate_head, truncation_json,
};
use crate::state::{AgentContext, ToolResult};

const SQLITE_MAGIC: [u8; 16] = *b"SQLite format 3\0";
const DEFAULT_QUERY_LIMIT: i64 = 20;
const DEFAULT_SCHEMA_SAMPLE_LIMIT: i64 = 5;
const MAX_QUERY_LIMIT: i64 = 500;
/// Raw `?q=` row cap.
const MAX_RAW_QUERY_ROWS: usize = 1000;
const MAX_RENDER_WIDTH: usize = 120;
const MAX_COLUMN_WIDTH: usize = 40;
/// Floor for every ASCII column so a shrunk cell still shows one glyph (`Fo…`).
const MIN_COLUMN_WIDTH: usize = 3;
/// `" | "` between columns.
const COLUMN_SEPARATOR_WIDTH: usize = 3;
/// Leading `"|"` + trailing `" |"` frame.
const TABLE_FRAME_WIDTH: usize = 1;
/// Listing cap (`applyListLimit(tables, { limit: 500 })`).
const LIST_LIMIT: usize = 500;
/// Rows scanned when counting a table for the listing.
const ROW_COUNT_PROBE_CAP: i64 = 50_000;
/// `DEFAULT_TAB_WIDTH` spaces, as `replaceTabs` in the TUI.
const TAB_SPACES: &str = "   ";
/// `DEFAULT_MAX_LINES`, used as the raw-query limit when no rows came back.
const DEFAULT_MAX_LINES: i64 = 3000;

const READ_ONLY_QUERY_ERROR: &str =
	"SQLite read queries must not modify or attach databases";
const COMMENT_OR_TERMINATOR_ERROR: &str =
	"SQLite 'where' clause must not contain comments or statement terminators; use '?q=SELECT ...' for raw SQL";
const FORBIDDEN_KEYWORD_ERROR: &str =
	"SQLite 'where' clause must not contain LIMIT/OFFSET/UNION/INTERSECT/EXCEPT/ATTACH/DETACH/PRAGMA; use '?q=SELECT ...' for raw SQL";
const FORBIDDEN_WHERE_KEYWORDS: &[&str] =
	&["limit", "offset", "union", "intersect", "except", "attach", "detach", "pragma"];

/// Result of resolving a raw read path to an existing SQLite database.
pub(super) struct ResolvedSqlitePath {
	pub absolute_path:     PathBuf,
	pub sub_path:          String,
	pub query_string:      String,
	pub suffix_resolution: Option<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SqlitePathCandidate {
	sqlite_path:  String,
	sub_path:     String,
	query_string: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SqliteSelector {
	List,
	Schema { table: String, sample_limit: i64 },
	Row { table: String, key: String },
	Query {
		table:        String,
		limit:        i64,
		offset:       i64,
		order:        Option<String>,
		where_clause: Option<String>,
	},
	Raw { sql: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowCount {
	Exact(i64),
	Estimate(i64),
	AtLeast(i64),
}

#[derive(Clone, Debug)]
struct TableSummary {
	name:  String,
	count: RowCount,
}

#[derive(Clone, Debug)]
struct TableInfoRow {
	name:         String,
	column_type:  String,
	pk:           i64,
}

enum RowLookup {
	PrimaryKey { column: String, column_type: String },
	Rowid,
}

struct Page {
	columns:     Vec<String>,
	rows:        Vec<HashMap<String, SqlValue>>,
	total_count: i64,
}

#[derive(Debug)]
enum ReadError {
	Cancelled,
	Message(String),
}

impl std::fmt::Display for ReadError {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Cancelled => formatter.write_str("Read cancelled"),
			Self::Message(message) => formatter.write_str(message),
		}
	}
}

impl From<rusqlite::Error> for ReadError {
	fn from(error: rusqlite::Error) -> Self {
		ReadError::Message(sqlite_message(error))
	}
}

impl From<String> for ReadError {
	fn from(message: String) -> Self {
		ReadError::Message(message)
	}
}

impl From<&str> for ReadError {
	fn from(message: &str) -> Self {
		ReadError::Message(message.to_string())
	}
}

type ReadResult<T> = Result<T, ReadError>;

fn sqlite_message(error: rusqlite::Error) -> String {
	if let rusqlite::Error::SqliteFailure(inner, _) = &error {
		if inner.code == ErrorCode::AuthorizationForStatementDenied {
			return READ_ONLY_QUERY_ERROR.to_string();
		}
	}
	error.to_string()
}

// ---------------------------------------------------------------------------
// Path discovery
// ---------------------------------------------------------------------------

/// `parseSqlitePathCandidates`: every `.sqlite`/`.sqlite3`/`.db`/`.db3` prefix
/// followed by `:`, `?`, or the end of the input, longest path first.
fn parse_path_candidates(raw: &str) -> Vec<SqlitePathCandidate> {
	let normalized = raw.replace('\\', "/");
	let bytes = normalized.as_bytes();
	let mut candidates = Vec::new();
	let mut seen = HashSet::new();
	let mut index = 0usize;
	while index < bytes.len() {
		if bytes[index] != b'.' {
			index += 1;
			continue;
		}
		let Some(length) = sqlite_extension_length(&bytes[index..]) else {
			index += 1;
			continue;
		};
		let end = index + length;
		if end < bytes.len() && bytes[end] != b':' && bytes[end] != b'?' {
			index += 1;
			continue;
		}
		let sqlite_path = raw[..end].to_string();
		let (sub_path, query_string) = split_remainder(&normalized[end..]);
		if seen.insert((sqlite_path.clone(), sub_path.clone(), query_string.clone())) {
			candidates.push(SqlitePathCandidate { sqlite_path, sub_path, query_string });
		}
		// The global regex resumes after the match, not after the dot.
		index = end;
	}
	candidates.sort_by(|left, right| right.sqlite_path.len().cmp(&left.sqlite_path.len()));
	candidates
}

fn sqlite_extension_length(rest: &[u8]) -> Option<usize> {
	for extension in [&b".sqlite3"[..], &b".sqlite"[..], &b".db3"[..], &b".db"[..]] {
		if rest.len() >= extension.len() && rest[..extension.len()].eq_ignore_ascii_case(extension) {
			return Some(extension.len());
		}
	}
	None
}

fn split_remainder(remainder: &str) -> (String, String) {
	match remainder.find('?') {
		Some(index) => (
			remainder[..index].trim_start_matches(':').to_string(),
			remainder[index + 1..].to_string(),
		),
		None => (remainder.trim_start_matches(':').to_string(), String::new()),
	}
}

/// Resolve the raw authored path to a SQLite database, retrying a missing path
/// through the workspace suffix match. `Err` only for non-`ENOENT` stat errors
/// after a suffix match, matching upstream's rethrow.
pub(super) async fn resolve_sqlite_read_path(
	raw: &str,
	ctx: &AgentContext,
) -> Result<Option<ResolvedSqlitePath>, String> {
	for candidate in parse_path_candidates(raw) {
		let mut target = resolve_path(ctx, &candidate.sqlite_path);
		let mut suffix_resolution = None;
		match tokio::fs::metadata(&target).await {
			Ok(meta) => {
				if meta.is_dir() || !is_sqlite_file(&target) {
					continue;
				}
			},
			Err(error) => {
				if !is_not_found(&error) || is_remote_mount_path(&target, ctx) {
					continue;
				}
				let Some((found, display)) =
					find_unique_workspace_suffix(&candidate.sqlite_path, ctx).await
				else {
					continue;
				};
				match tokio::fs::metadata(&found).await {
					Ok(meta) => {
						if meta.is_dir() || !is_sqlite_file(&found) {
							continue;
						}
						target = found;
						suffix_resolution = Some((candidate.sqlite_path.clone(), display));
					},
					Err(retry) => {
						if !is_not_found(&retry) {
							return Err(retry.to_string());
						}
						continue;
					},
				}
			},
		}
		return Ok(Some(ResolvedSqlitePath {
			absolute_path: target,
			sub_path: candidate.sub_path,
			query_string: candidate.query_string,
			suffix_resolution,
		}));
	}
	Ok(None)
}

/// Skip the workspace suffix glob under the sshfs remote mount, like upstream.
fn is_remote_mount_path(path: &Path, ctx: &AgentContext) -> bool {
	let Some(home) = ctx.env.get("HOME").filter(|home| !home.is_empty()) else {
		return false;
	};
	path.starts_with(Path::new(home).join(".omp").join("remote"))
}

fn is_sqlite_file(path: &Path) -> bool {
	let Ok(mut file) = File::open(path) else {
		return false;
	};
	let mut magic = [0u8; 16];
	match file.read_exact(&mut magic) {
		Ok(()) => magic == SQLITE_MAGIC,
		Err(_) => false,
	}
}

// ---------------------------------------------------------------------------
// Tool entry point
// ---------------------------------------------------------------------------

/// Read a resolved SQLite target. Read failures surface as error tool results,
/// mirroring the rest of `read`.
pub(super) async fn read_sqlite(resolved: ResolvedSqlitePath, ctx: &AgentContext) -> ToolResult {
	if ctx.cancellation_token.is_cancelled() {
		return cancelled_result();
	}
	let token = ctx.cancellation_token.clone();
	let outcome = tokio::task::spawn_blocking(move || run(&resolved, &token)).await;
	match outcome {
		Ok(Ok((content, details))) => ToolResult {
			tool_call_id: String::new(),
			content,
			is_error: false,
			terminate: false,
			details: Some(details),
		},
		Ok(Err(ReadError::Cancelled)) => cancelled_result(),
		Ok(Err(ReadError::Message(message))) => error_result(message),
		Err(join) => error_result(format!("SQLite read failed: {join}")),
	}
}

fn cancelled_result() -> ToolResult {
	ToolResult {
		tool_call_id: String::new(),
		content:      "Read cancelled".to_string(),
		is_error:     true,
		terminate:    false,
		details:      None,
	}
}

fn error_result(message: String) -> ToolResult {
	ToolResult {
		tool_call_id: String::new(),
		content: message,
		is_error: true,
		terminate: false,
		details: None,
	}
}

fn run(resolved: &ResolvedSqlitePath, token: &CancellationToken) -> ReadResult<(String, JsonValue)> {
	check_cancelled(token)?;
	let selector = parse_selector(&resolved.sub_path, &resolved.query_string)?;
	let connection = open_read_connection(&resolved.absolute_path)?;
	check_cancelled(token)?;
	let mut details = base_details(resolved);
	let (content, notice_applied) = match &selector {
		SqliteSelector::List => {
			let tables = list_tables(&connection, ROW_COUNT_PROBE_CAP, token)?;
			if tables.len() >= LIST_LIMIT {
				if let Some(JsonValue::Object(meta)) = details.get_mut("meta") {
					meta.insert(
						"limits".to_string(),
						json!({ "resultLimit": { "reached": LIST_LIMIT, "suggestion": LIST_LIMIT * 2 } }),
					);
				}
			}
			let limited: Vec<TableSummary> = tables.iter().take(LIST_LIMIT).cloned().collect();
			let output = prepend_suffix_notice(
				render_table_list(&limited),
				resolved.suffix_resolution.as_ref(),
			);
			if output.len() as u64 > DEFAULT_MAX_BYTES {
				let truncation = truncate_head(&output, DEFAULT_MAX_BYTES);
				record_truncation(&mut details, &truncation);
				(truncation.content, true)
			} else {
				(output, true)
			}
		},
		SqliteSelector::Schema { table, sample_limit } => {
			let page = query_rows(&connection, table, *sample_limit, 0, None, None, token)?;
			let mut output =
				render_schema(&get_table_schema(&connection, table)?, &page.columns, &page.rows);
			let shown = page.rows.len() as i64;
			if shown < page.total_count {
				let remaining = page.total_count - shown;
				output.push_str(&format!(
					"\n[{remaining} more rows; append :{table}?limit={DEFAULT_QUERY_LIMIT}&offset={shown} to the database path to continue]"
				));
			}
			(output, false)
		},
		SqliteSelector::Row { table, key } => {
			let lookup = resolve_row_lookup(&connection, table)?;
			match get_row(&connection, table, &lookup, key)? {
				Some((columns, row)) => (render_row(&columns, &row), false),
				None => (format!("No row found in table '{table}' for key '{key}'."), false),
			}
		},
		SqliteSelector::Query { table, limit, offset, order, where_clause } => {
			let page = query_rows(
				&connection,
				table,
				*limit,
				*offset,
				order.as_deref(),
				where_clause.as_deref(),
				token,
			)?;
			(
				render_table(&page.columns, &page.rows, page.total_count, *offset, *limit, table),
				false,
			)
		},
		SqliteSelector::Raw { sql } => {
			let (columns, rows, truncated) = execute_read_query(&connection, sql, token)?;
			let limit = if rows.is_empty() { DEFAULT_MAX_LINES } else { rows.len() as i64 };
			let mut output = render_table(&columns, &rows, rows.len() as i64, 0, limit, "query");
			if truncated {
				output.push_str(&format!(
					"\n[Output capped at {MAX_RAW_QUERY_ROWS} rows; add a LIMIT/OFFSET clause to the query to page through more]"
				));
			}
			(output, false)
		},
	};
	let content = if notice_applied {
		content
	} else {
		prepend_suffix_notice(content, resolved.suffix_resolution.as_ref())
	};
	Ok((content, JsonValue::Object(details)))
}

fn base_details(resolved: &ResolvedSqlitePath) -> Map<String, JsonValue> {
	let path = resolved.absolute_path.display().to_string();
	let mut details = Map::new();
	details.insert("resolvedPath".to_string(), json!(path));
	if let Some((from, to)) = &resolved.suffix_resolution {
		details.insert("suffixResolution".to_string(), json!({ "from": from, "to": to }));
	}
	details.insert("meta".to_string(), json!({ "source": { "type": "path", "value": path } }));
	details
}

fn record_truncation(details: &mut Map<String, JsonValue>, truncation: &super::read::HeadTruncation) {
	details.insert(
		"truncation".to_string(),
		truncation_json(
			&truncation.content,
			true,
			Some(truncation.truncated_by),
			truncation.total_lines,
			truncation.total_bytes,
			truncation.output_lines,
			truncation.output_bytes,
			truncation.first_line_exceeds,
		),
	);
	let meta_truncation = meta_truncation_json(
		truncation.truncated_by,
		truncation.total_lines,
		truncation.total_bytes,
		truncation.output_lines,
		truncation.output_bytes,
		1,
		truncation.output_lines,
	);
	if let Some(JsonValue::Object(meta)) = details.get_mut("meta") {
		meta.insert("truncation".to_string(), meta_truncation);
	}
}

fn check_cancelled(token: &CancellationToken) -> ReadResult<()> {
	if token.is_cancelled() {
		return Err(ReadError::Cancelled);
	}
	Ok(())
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

fn open_read_connection(path: &Path) -> ReadResult<Connection> {
	if requires_wal_sidecar_initialization(path) {
		return configure(open_connection(path, true)?).map_err(ReadError::from);
	}
	let first = match open_connection(path, false) {
		Ok(connection) => configure(connection),
		Err(error) => Err(error),
	};
	match first {
		Ok(connection) => Ok(connection),
		Err(error) if is_cant_open(&error) => {
			configure(open_connection(path, true)?).map_err(ReadError::from)
		},
		Err(error) => Err(ReadError::from(error)),
	}
}

fn open_connection(path: &Path, read_write: bool) -> rusqlite::Result<Connection> {
	let mut flags = OpenFlags::SQLITE_OPEN_NO_MUTEX;
	flags |= if read_write {
		OpenFlags::SQLITE_OPEN_READ_WRITE
	} else {
		OpenFlags::SQLITE_OPEN_READ_ONLY
	};
	Connection::open_with_flags(path, flags)
}

/// `configureSqliteReadConnection`: query-only, 3s busy timeout, a schema
/// probe, then the read-only authorizer.
fn configure(connection: Connection) -> rusqlite::Result<Connection> {
	connection.execute_batch("PRAGMA query_only = ON; PRAGMA busy_timeout = 3000")?;
	{
		let mut statement = connection.prepare("SELECT name FROM sqlite_master LIMIT 1")?;
		let mut rows = statement.query([])?;
		rows.next()?;
	}
	connection.authorizer(Some(authorize));
	Ok(connection)
}

fn is_cant_open(error: &rusqlite::Error) -> bool {
	matches!(error, rusqlite::Error::SqliteFailure(inner, _) if inner.code == ErrorCode::CannotOpen)
}

fn authorize(context: AuthContext<'_>) -> Authorization {
	match context.action {
		AuthAction::Read { .. }
		| AuthAction::Select
		| AuthAction::Function { .. }
		| AuthAction::Recursive
		| AuthAction::Transaction { .. }
		| AuthAction::Savepoint { .. } => Authorization::Allow,
		AuthAction::Pragma { pragma_name, .. }
			if matches!(pragma_name.to_ascii_lowercase().as_str(), "table_info" | "table_xinfo") =>
		{
			Authorization::Allow
		},
		_ => Authorization::Deny,
	}
}

/// Header write/read versions are 2 for WAL; SQLite cannot read a WAL database
/// read-only when either sidecar is missing, so upstream opens it read-write
/// once to recreate them.
fn requires_wal_sidecar_initialization(path: &Path) -> bool {
	let Ok(mut file) = File::open(path) else {
		return false;
	};
	let mut header = [0u8; 20];
	if file.read_exact(&mut header).is_err() {
		return false;
	}
	if header[18] != 2 && header[19] != 2 {
		return false;
	}
	!sidecar_path(path, "-wal").exists() || !sidecar_path(path, "-shm").exists()
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
	let mut name = path.as_os_str().to_os_string();
	name.push(suffix);
	PathBuf::from(name)
}

// ---------------------------------------------------------------------------
// Selector parsing
// ---------------------------------------------------------------------------

fn parse_selector(sub_path: &str, query_string: &str) -> ReadResult<SqliteSelector> {
	let normalized = sub_path.trim_start_matches(':').trim();
	let params = QueryParams::parse(query_string);
	if let Some(raw_query) = params.get("q") {
		let other_keys = params.keys().any(|key| key != "q");
		if !normalized.is_empty() || other_keys {
			return Err(
				"SQLite raw queries cannot be combined with table selectors or pagination".into()
			);
		}
		if raw_query.trim().is_empty() {
			return Err("SQLite query parameter 'q' cannot be empty".into());
		}
		return Ok(SqliteSelector::Raw { sql: raw_query.to_string() });
	}

	if normalized.is_empty() {
		if params.size() > 0 {
			return Err(
				"SQLite query parameters require a table selector or q=SELECT...".into()
			);
		}
		return Ok(SqliteSelector::List);
	}

	let (table, key) = match normalized.find(':') {
		Some(index) => (&normalized[..index], Some(&normalized[index + 1..])),
		None => (normalized, None),
	};
	if table.is_empty() {
		return Err("SQLite selectors must include a table name".into());
	}

	if let Some(key) = key.filter(|key| !key.is_empty()) {
		if params.size() > 0 {
			return Err("SQLite row lookups cannot be combined with query parameters".into());
		}
		return Ok(SqliteSelector::Row { table: table.to_string(), key: key.to_string() });
	}

	let where_clause = validate_where_clause(params.get("where"))?;
	let order = params
		.get("order")
		.map(str::trim)
		.filter(|order| !order.is_empty())
		.map(str::to_string);
	let has_query_params =
		params.has("limit") || params.has("offset") || order.is_some() || where_clause.is_some();
	if has_query_params {
		for key_name in params.keys() {
			if !matches!(key_name, "limit" | "offset" | "order" | "where") {
				return Err(format!("Unsupported SQLite query parameter '{key_name}'").into());
			}
		}
		return Ok(SqliteSelector::Query {
			table: table.to_string(),
			limit: parse_limit(params.get("limit"), DEFAULT_QUERY_LIMIT)?,
			offset: parse_offset(params.get("offset"))?,
			order,
			where_clause,
		});
	}

	if let Some(key_name) = params.keys().next() {
		return Err(format!("Unsupported SQLite query parameter '{key_name}'").into());
	}

	Ok(SqliteSelector::Schema { table: table.to_string(), sample_limit: DEFAULT_SCHEMA_SAMPLE_LIMIT })
}

/// `URLSearchParams` subset: first-value lookup, ordering, and size.
struct QueryParams {
	pairs: Vec<(String, String)>,
}

impl QueryParams {
	fn parse(raw: &str) -> Self {
		let mut pairs = Vec::new();
		for piece in raw.split('&') {
			if piece.is_empty() {
				continue;
			}
			let (name, value) = match piece.find('=') {
				Some(index) => (&piece[..index], &piece[index + 1..]),
				None => (piece, ""),
			};
			pairs.push((percent_decode(name), percent_decode(value)));
		}
		Self { pairs }
	}

	fn get(&self, key: &str) -> Option<&str> {
		self.pairs.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str())
	}

	fn has(&self, key: &str) -> bool {
		self.pairs.iter().any(|(name, _)| name == key)
	}

	fn keys(&self) -> impl Iterator<Item = &str> {
		self.pairs.iter().map(|(name, _)| name.as_str())
	}

	fn size(&self) -> usize {
		self.pairs.len()
	}
}

fn percent_decode(input: &str) -> String {
	let bytes = input.as_bytes();
	let mut decoded = Vec::with_capacity(bytes.len());
	let mut index = 0usize;
	while index < bytes.len() {
		match bytes[index] {
			b'+' => {
				decoded.push(b' ');
				index += 1;
			},
			b'%' if index + 2 < bytes.len() => {
				let high = (bytes[index + 1] as char).to_digit(16);
				let low = (bytes[index + 2] as char).to_digit(16);
				match (high, low) {
					(Some(high), Some(low)) => {
						decoded.push((high * 16 + low) as u8);
						index += 3;
					},
					_ => {
						decoded.push(b'%');
						index += 1;
					},
				}
			},
			byte => {
				decoded.push(byte);
				index += 1;
			},
		}
	}
	String::from_utf8_lossy(&decoded).into_owned()
}

fn parse_limit(value: Option<&str>, fallback: i64) -> ReadResult<i64> {
	let Some(value) = value else {
		return Ok(fallback);
	};
	if value.trim().is_empty() {
		return Ok(fallback);
	}
	let Some(parsed) = js_parse_int(value).filter(|parsed| parsed.is_finite() && *parsed >= 1.0)
	else {
		return Err(format!("SQLite limit must be a positive integer; got '{value}'").into());
	};
	Ok(if parsed > MAX_QUERY_LIMIT as f64 { MAX_QUERY_LIMIT } else { parsed as i64 })
}

fn parse_offset(value: Option<&str>) -> ReadResult<i64> {
	let Some(value) = value else {
		return Ok(0);
	};
	if value.trim().is_empty() {
		return Ok(0);
	}
	let Some(parsed) = js_parse_int(value).filter(|parsed| parsed.is_finite() && *parsed >= 0.0)
	else {
		return Err(
			format!("SQLite offset must be a non-negative integer; got '{value}'").into()
		);
	};
	Ok(parsed.min(i64::MAX as f64) as i64)
}

/// `Number.parseInt(value, 10)`: optional whitespace/sign, then a digit prefix.
fn js_parse_int(value: &str) -> Option<f64> {
	let trimmed = value.trim_start();
	let (sign, rest) = match trimmed.as_bytes().first() {
		Some(b'-') => (-1.0, &trimmed[1..]),
		Some(b'+') => (1.0, &trimmed[1..]),
		_ => (1.0, trimmed),
	};
	let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
	if digits.is_empty() {
		return None;
	}
	digits.parse::<f64>().ok().map(|parsed| sign * parsed)
}

/// `Number(value)`: trimmed numeric literal, including hex/binary/octal and
/// the infinities.
fn js_number_literal(raw: &str) -> Option<f64> {
	let trimmed = raw.trim();
	if trimmed.is_empty() {
		return Some(0.0);
	}
	let lower = trimmed.to_ascii_lowercase();
	match lower.as_str() {
		"infinity" | "+infinity" => return Some(f64::INFINITY),
		"-infinity" => return Some(f64::NEG_INFINITY),
		_ => {},
	}
	if let Some(hex) = lower.strip_prefix("0x") {
		return i64::from_str_radix(hex, 16).ok().map(|value| value as f64);
	}
	if let Some(binary) = lower.strip_prefix("0b") {
		return i64::from_str_radix(binary, 2).ok().map(|value| value as f64);
	}
	if let Some(octal) = lower.strip_prefix("0o") {
		return i64::from_str_radix(octal, 8).ok().map(|value| value as f64);
	}
	trimmed.parse::<f64>().ok()
}

fn validate_where_clause(where_clause: Option<&str>) -> ReadResult<Option<String>> {
	let Some(raw) = where_clause else {
		return Ok(None);
	};
	let trimmed = raw.trim();
	if trimmed.is_empty() {
		return Ok(None);
	}
	if let Some(violation) = find_where_clause_violation(trimmed) {
		return Err(violation.into());
	}
	Ok(Some(trimmed.to_string()))
}

/// Reject comments, statement terminators, and pagination/attach/pragma
/// keywords outside string literals in a `where=` clause.
fn find_where_clause_violation(sql: &str) -> Option<&'static str> {
	let chars: Vec<char> = sql.chars().collect();
	let mut in_single_quote = false;
	let mut in_double_quote = false;
	let mut token_start: Option<usize> = None;
	let mut keyword_violation: Option<&'static str> = None;
	let mut index = 0usize;
	while index <= chars.len() {
		let current = chars.get(index).copied();
		let next = chars.get(index + 1).copied();

		if in_single_quote {
			if current == Some('\'') && next == Some('\'') {
				index += 2;
				continue;
			}
			if current == Some('\'') {
				in_single_quote = false;
			}
			index += 1;
			continue;
		}
		if in_double_quote {
			if current == Some('"') && next == Some('"') {
				index += 2;
				continue;
			}
			if current == Some('"') {
				in_double_quote = false;
			}
			index += 1;
			continue;
		}

		if current.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
			token_start.get_or_insert(index);
			index += 1;
			continue;
		}

		if let Some(start) = token_start.take() {
			if keyword_violation.is_none() {
				let token: String =
					chars[start..index].iter().collect::<String>().to_ascii_lowercase();
				if FORBIDDEN_WHERE_KEYWORDS.contains(&token.as_str()) {
					keyword_violation = Some(FORBIDDEN_KEYWORD_ERROR);
				}
			}
		}

		let Some(current) = current else {
			break;
		};
		if current == '\'' {
			in_single_quote = true;
			index += 1;
			continue;
		}
		if current == '"' {
			in_double_quote = true;
			index += 1;
			continue;
		}
		if current == ';' {
			return Some(COMMENT_OR_TERMINATOR_ERROR);
		}
		if (current == '-' && next == Some('-'))
			|| (current == '/' && next == Some('*'))
			|| (current == '*' && next == Some('/'))
		{
			return Some(COMMENT_OR_TERMINATOR_ERROR);
		}
		index += 1;
	}
	keyword_violation
}

// ---------------------------------------------------------------------------
// SQL metadata and reads
// ---------------------------------------------------------------------------

fn quote_identifier(identifier: &str) -> String {
	format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn get_table_master_row(connection: &Connection, table: &str) -> ReadResult<Option<String>> {
	let mut statement = connection.prepare(
		"SELECT name, sql FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name = ?1",
	)?;
	let mut rows = statement.query(params![table])?;
	match rows.next()? {
		Some(row) => Ok(row.get::<_, Option<String>>(1)?),
		None => Err(format!("SQLite table '{table}' not found").into()),
	}
}

fn get_table_info_rows(connection: &Connection, table: &str) -> ReadResult<Vec<TableInfoRow>> {
	get_table_master_row(connection, table)?;
	let sql = format!("PRAGMA table_info({})", quote_identifier(table));
	let mut statement = connection.prepare(&sql)?;
	let mut rows = statement.query([])?;
	let mut columns = Vec::new();
	while let Some(row) = rows.next()? {
		columns.push(TableInfoRow {
			name:        row.get::<_, String>(1)?,
			column_type: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
			pk:          row.get::<_, i64>(5)?,
		});
	}
	Ok(columns)
}

fn get_table_columns(connection: &Connection, table: &str) -> ReadResult<Vec<String>> {
	Ok(get_table_info_rows(connection, table)?.into_iter().map(|column| column.name).collect())
}

fn get_primary_key_columns(connection: &Connection, table: &str) -> ReadResult<Vec<TableInfoRow>> {
	let mut primary_keys: Vec<TableInfoRow> = get_table_info_rows(connection, table)?
		.into_iter()
		.filter(|column| column.pk > 0)
		.collect();
	primary_keys.sort_by_key(|column| column.pk);
	Ok(primary_keys)
}

fn get_table_schema(connection: &Connection, table: &str) -> ReadResult<String> {
	match get_table_master_row(connection, table)? {
		Some(sql) => Ok(sql),
		None => Err(format!("SQLite schema for table '{table}' is unavailable").into()),
	}
}

fn is_without_rowid(sql: &str) -> bool {
	let tokens: Vec<&str> = sql
		.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
		.filter(|token| !token.is_empty())
		.collect();
	tokens.windows(2).any(|pair| {
		pair[0].eq_ignore_ascii_case("without") && pair[1].eq_ignore_ascii_case("rowid")
	})
}

fn resolve_row_lookup(connection: &Connection, table: &str) -> ReadResult<RowLookup> {
	let primary_keys = get_primary_key_columns(connection, table)?;
	match primary_keys.len() {
		0 => {
			let schema = get_table_schema(connection, table)?;
			if is_without_rowid(&schema) {
				return Err(format!(
					"SQLite table '{table}' does not expose ROWID; use '?where=' instead"
				)
				.into());
			}
			Ok(RowLookup::Rowid)
		},
		1 => {
			let column = primary_keys[0].clone();
			Ok(RowLookup::PrimaryKey { column: column.name, column_type: column.column_type })
		},
		_ => Err(format!(
			"SQLite table '{table}' has a composite primary key; use '?where=' instead"
		)
		.into()),
	}
}

fn get_row(
	connection: &Connection,
	table: &str,
	lookup: &RowLookup,
	key: &str,
) -> ReadResult<Option<(Vec<String>, HashMap<String, SqlValue>)>> {
	let (sql, binding) = match lookup {
		RowLookup::PrimaryKey { column, column_type } => (
			format!(
				"SELECT * FROM {} WHERE {} = ?1 LIMIT 1",
				quote_identifier(table),
				quote_identifier(column)
			),
			lookup_binding(key, column_type)?,
		),
		RowLookup::Rowid => (
			format!("SELECT * FROM {} WHERE rowid = ?1 LIMIT 1", quote_identifier(table)),
			SqlValue::Integer(coerce_integer_key(key, "SQLite ROWID")?),
		),
	};
	let columns = get_table_columns(connection, table)?;
	let mut statement = connection.prepare(&sql)?;
	let mut rows = statement.query(params![binding])?;
	match rows.next()? {
		Some(row) => Ok(Some((columns.clone(), read_row(row, &columns)?))),
		None => Ok(None),
	}
}

fn lookup_binding(key: &str, column_type: &str) -> ReadResult<SqlValue> {
	let normalized = column_type.trim().to_ascii_uppercase();
	if normalized.contains("INT") {
		return Ok(SqlValue::Integer(coerce_integer_key(
			key,
			&format!("Primary key '{key}'"),
		)?));
	}
	if normalized.contains("REAL") || normalized.contains("FLOA") || normalized.contains("DOUB") {
		if let Some(parsed) = js_number_literal(key).filter(|parsed| parsed.is_finite()) {
			return Ok(SqlValue::Real(parsed));
		}
	}
	Ok(SqlValue::Text(key.to_string()))
}

fn coerce_integer_key(key: &str, label: &str) -> ReadResult<i64> {
	let trimmed = key.trim();
	let digits = trimmed.strip_prefix('-').unwrap_or(trimmed);
	if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
		return Err(format!("{label} must be an integer; got '{key}'").into());
	}
	trimmed
		.parse::<i64>()
		.map_err(|_| ReadError::Message(format!("{label} must be an integer; got '{key}'")))
}

fn resolve_order_clause(order: Option<&str>, columns: &[String]) -> ReadResult<String> {
	let Some(order) = order else {
		return Ok(String::new());
	};
	let trimmed = order.trim();
	if trimmed.is_empty() {
		return Ok(String::new());
	}
	let (column, direction) = match trimmed.rfind(':') {
		Some(index) => (&trimmed[..index], trimmed[index + 1..].trim().to_ascii_lowercase()),
		None => (trimmed, "asc".to_string()),
	};
	if !columns.iter().any(|candidate| candidate == column) {
		return Err(format!("SQLite order column '{column}' not found in table schema").into());
	}
	if direction != "asc" && direction != "desc" {
		return Err(format!("SQLite order direction must be 'asc' or 'desc'; got '{direction}'")
			.into());
	}
	Ok(format!(" ORDER BY {} {}", quote_identifier(column), direction.to_ascii_uppercase()))
}

fn query_rows(
	connection: &Connection,
	table: &str,
	limit: i64,
	offset: i64,
	order: Option<&str>,
	where_clause: Option<&str>,
	token: &CancellationToken,
) -> ReadResult<Page> {
	let columns = get_table_columns(connection, table)?;
	let where_sql = match validate_where_clause(where_clause)? {
		Some(validated) => format!(" WHERE {validated}"),
		None => String::new(),
	};
	let order_sql = resolve_order_clause(order, &columns)?;
	let count_sql =
		format!("SELECT COUNT(*) AS count FROM {}{where_sql}", quote_identifier(table));
	let total_count: i64 = connection.query_row(&count_sql, [], |row| row.get(0))?;
	let select_sql = format!(
		"SELECT * FROM {}{where_sql}{order_sql} LIMIT ?1 OFFSET ?2",
		quote_identifier(table)
	);
	let mut statement = connection.prepare(&select_sql)?;
	if statement.parameter_count() != 2 {
		return Err(
			"SQLite where clause changed the expected pagination parameters; use q=SELECT ... for raw SQL"
				.into(),
		);
	}
	let mut rows = statement.query(params![limit, offset])?;
	let mut collected = Vec::new();
	while let Some(row) = rows.next()? {
		check_cancelled(token)?;
		collected.push(read_row(row, &columns)?);
	}
	Ok(Page { columns, rows: collected, total_count })
}

fn execute_read_query(
	connection: &Connection,
	sql: &str,
	token: &CancellationToken,
) -> ReadResult<(Vec<String>, Vec<HashMap<String, SqlValue>>, bool)> {
	let mut statement = connection.prepare(sql)?;
	if statement.parameter_count() > 0 {
		return Err("SQLite raw queries do not support bound parameters".into());
	}
	if !statement.readonly() {
		return Err(READ_ONLY_QUERY_ERROR.into());
	}
	let columns: Vec<String> =
		statement.column_names().into_iter().map(str::to_string).collect();
	let mut rows = statement.query([])?;
	let mut collected = Vec::new();
	let mut truncated = false;
	while let Some(row) = rows.next()? {
		check_cancelled(token)?;
		if collected.len() >= MAX_RAW_QUERY_ROWS {
			truncated = true;
			break;
		}
		collected.push(read_row(row, &columns)?);
	}
	Ok((columns, collected, truncated))
}

fn read_row(
	row: &rusqlite::Row<'_>,
	columns: &[String],
) -> ReadResult<HashMap<String, SqlValue>> {
	let mut mapped = HashMap::with_capacity(columns.len());
	for (index, column) in columns.iter().enumerate() {
		mapped.insert(column.clone(), row.get::<_, SqlValue>(index)?);
	}
	Ok(mapped)
}

fn load_row_estimates(connection: &Connection) -> ReadResult<HashMap<String, i64>> {
	let mut estimates = HashMap::new();
	let has_stat1: Option<String> = connection
		.query_row(
			"SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'sqlite_stat1'",
			[],
			|row| row.get(0),
		)
		.optional()?;
	if has_stat1.is_none() {
		return Ok(estimates);
	}
	let mut statement = connection.prepare("SELECT tbl, stat FROM sqlite_stat1")?;
	let mut rows = statement.query([])?;
	while let Some(row) = rows.next()? {
		let table: String = row.get(0)?;
		let stat: Option<String> = row.get(1)?;
		let Some(stat) = stat else {
			continue;
		};
		let Some(rows_estimate) = parse_leading_int(&stat) else {
			continue;
		};
		let previous = estimates.get(&table).copied();
		if previous.is_none_or(|previous| rows_estimate > previous) {
			estimates.insert(table, rows_estimate);
		}
	}
	Ok(estimates)
}

fn parse_leading_int(value: &str) -> Option<i64> {
	let trimmed = value.trim_start();
	let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
	if digits.is_empty() {
		return None;
	}
	digits.parse::<i64>().ok()
}

fn probe_row_count(
	connection: &Connection,
	table: &str,
	cap: i64,
	token: &CancellationToken,
) -> ReadResult<RowCount> {
	check_cancelled(token)?;
	let sql = format!(
		"SELECT COUNT(*) AS count FROM (SELECT 1 FROM {} LIMIT {})",
		quote_identifier(table),
		cap + 1
	);
	let counted: i64 = connection.query_row(&sql, [], |row| row.get(0))?;
	Ok(if counted > cap { RowCount::AtLeast(cap) } else { RowCount::Exact(counted) })
}

fn list_tables(
	connection: &Connection,
	probe_cap: i64,
	token: &CancellationToken,
) -> ReadResult<Vec<TableSummary>> {
	let names = {
		let mut statement = connection.prepare(
			"SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name COLLATE NOCASE",
		)?;
		let mut rows = statement.query([])?;
		let mut names = Vec::new();
		while let Some(row) = rows.next()? {
			names.push(row.get::<_, String>(0)?);
		}
		names
	};
	let estimates = load_row_estimates(connection)?;
	let mut tables = Vec::with_capacity(names.len());
	for name in names {
		check_cancelled(token)?;
		let count = match estimates.get(&name) {
			Some(estimate) if *estimate > probe_cap => RowCount::Estimate(*estimate),
			_ => probe_row_count(connection, &name, probe_cap, token)?,
		};
		tables.push(TableSummary { name, count });
	}
	Ok(tables)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn sanitize_cell(value: &str) -> String {
	value.replace('\t', TAB_SPACES).replace("\r\n", "\\n").replace('\n', "\\n")
}

fn stringify_value(value: &SqlValue) -> String {
	match value {
		SqlValue::Null => "NULL".to_string(),
		SqlValue::Integer(number) => number.to_string(),
		SqlValue::Real(number) => js_number(*number),
		SqlValue::Text(text) => text.clone(),
		SqlValue::Blob(blob) => format!("<BLOB {}>", format_bytes(blob.len() as u64)),
	}
}

/// `String(number)` for finite reals: plain decimal in the same range as JS,
/// exponential (with a signed exponent) outside it.
fn js_number(value: f64) -> String {
	if value == 0.0 {
		return "0".to_string();
	}
	if !value.is_finite() {
		return if value.is_nan() {
			"NaN".to_string()
		} else if value > 0.0 {
			"Infinity".to_string()
		} else {
			"-Infinity".to_string()
		};
	}
	let magnitude = value.abs();
	if (1e-6..1e21).contains(&magnitude) {
		return format!("{value}");
	}
	let exponential = format!("{value:e}");
	match exponential.split_once('e') {
		Some((mantissa, exponent)) if exponent.starts_with('-') => {
			format!("{mantissa}e{exponent}")
		},
		Some((mantissa, exponent)) => format!("{mantissa}e+{exponent}"),
		None => exponential,
	}
}

fn display_width(text: &str) -> usize {
	text.chars().map(char_width).sum()
}

fn char_width(character: char) -> usize {
	let code = character as u32;
	if code == 0 || code < 0x20 || (0x7f..0xa0).contains(&code) {
		return 0;
	}
	if is_zero_width(code) {
		return 0;
	}
	if is_wide(code) { 2 } else { 1 }
}

fn is_zero_width(code: u32) -> bool {
	matches!(code,
		0x0300..=0x036f
		| 0x0483..=0x0489
		| 0x0591..=0x05bd
		| 0x0610..=0x061a
		| 0x064b..=0x065f
		| 0x0670
		| 0x06d6..=0x06dc
		| 0x0730..=0x074a
		| 0x07a6..=0x07b0
		| 0x0816..=0x082d
		| 0x08d3..=0x08e1
		| 0x08e3..=0x0903
		| 0x093a..=0x093c
		| 0x0941..=0x0948
		| 0x094d
		| 0x0951..=0x0957
		| 0x0962..=0x0963
		| 0x1ab0..=0x1aff
		| 0x1dc0..=0x1dff
		| 0x200b..=0x200f
		| 0x202a..=0x202e
		| 0x2060..=0x2064
		| 0x20d0..=0x20f0
		| 0xfe00..=0xfe0f
		| 0xfe20..=0xfe2f
		| 0xfeff
		| 0xe0100..=0xe01ef)
}

fn is_wide(code: u32) -> bool {
	matches!(code,
		0x1100..=0x115f
		| 0x2e80..=0x303e
		| 0x3041..=0x33ff
		| 0x3400..=0x4dbf
		| 0x4e00..=0x9fff
		| 0xa000..=0xa4cf
		| 0xa960..=0xa97f
		| 0xac00..=0xd7a3
		| 0xf900..=0xfaff
		| 0xfe10..=0xfe19
		| 0xfe30..=0xfe6f
		| 0xff00..=0xff60
		| 0xffe0..=0xffe6
		| 0x1b000..=0x1b2ff
		| 0x1f300..=0x1f64f
		| 0x1f680..=0x1f6ff
		| 0x1f900..=0x1f9ff
		| 0x20000..=0x3fffd)
}

fn truncate_to_width(text: &str, max_width: usize) -> String {
	if display_width(text) <= max_width {
		return text.to_string();
	}
	if max_width == 0 {
		return String::new();
	}
	let target = max_width - 1;
	let mut output = String::new();
	let mut width = 0usize;
	for character in text.chars() {
		let character_width = char_width(character);
		if width + character_width > target {
			break;
		}
		output.push(character);
		width += character_width;
	}
	output.push('…');
	output
}

fn pad_cell(value: &str, width: usize) -> String {
	let truncated = truncate_to_width(&sanitize_cell(value), width.max(MIN_COLUMN_WIDTH));
	let visible = display_width(&truncated);
	if visible >= width {
		truncated
	} else {
		format!("{truncated}{}", " ".repeat(width - visible))
	}
}

fn table_fits_at_minimum(column_count: usize) -> bool {
	MIN_COLUMN_WIDTH * column_count + COLUMN_SEPARATOR_WIDTH * column_count + TABLE_FRAME_WIDTH
		<= MAX_RENDER_WIDTH
}

fn build_vertical_blocks(columns: &[String], rows: &[HashMap<String, SqlValue>]) -> String {
	if rows.is_empty() {
		return "(no rows)".to_string();
	}
	let mut name_width = MIN_COLUMN_WIDTH;
	for column in columns {
		name_width = name_width.max(display_width(&sanitize_cell(column)));
	}
	let name_width = name_width.min(MAX_COLUMN_WIDTH);
	rows.iter()
		.enumerate()
		.map(|(index, row)| {
			let mut block = vec![format!("── Row {} ──", index + 1)];
			for column in columns {
				let name = pad_cell(column, name_width);
				let value = sanitize_cell(&stringify_value(
					row.get(column).unwrap_or(&SqlValue::Null),
				));
				block.push(truncate_to_width(&format!("{name}: {value}"), MAX_RENDER_WIDTH));
			}
			block.join("\n")
		})
		.collect::<Vec<_>>()
		.join("\n\n")
}

fn build_ascii_table(columns: &[String], rows: &[HashMap<String, SqlValue>]) -> String {
	if columns.is_empty() {
		return if rows.is_empty() {
			"(no rows)".to_string()
		} else {
			"(rows returned without named columns)".to_string()
		};
	}
	if !table_fits_at_minimum(columns.len()) {
		return build_vertical_blocks(columns, rows);
	}

	let mut widths: Vec<usize> = columns
		.iter()
		.map(|column| {
			MIN_COLUMN_WIDTH.max(MAX_COLUMN_WIDTH.min(display_width(&sanitize_cell(column))))
		})
		.collect();
	for row in rows {
		for (index, column) in columns.iter().enumerate() {
			let value = stringify_value(row.get(column).unwrap_or(&SqlValue::Null));
			let cell_width = display_width(&sanitize_cell(&value));
			widths[index] = widths[index].max(MAX_COLUMN_WIDTH.min(cell_width));
		}
	}

	let overhead = columns.len() * COLUMN_SEPARATOR_WIDTH + TABLE_FRAME_WIDTH;
	let mut total_width = widths.iter().sum::<usize>() + overhead;
	while total_width > MAX_RENDER_WIDTH {
		let mut widest_index = None;
		let mut widest_width = MIN_COLUMN_WIDTH;
		for (index, width) in widths.iter().enumerate() {
			if *width > widest_width {
				widest_index = Some(index);
				widest_width = *width;
			}
		}
		let Some(index) = widest_index else {
			break;
		};
		widths[index] = MIN_COLUMN_WIDTH.max(widths[index] - 1);
		total_width = widths.iter().sum::<usize>() + overhead;
	}

	let header = format!(
		"| {} |",
		columns
			.iter()
			.enumerate()
			.map(|(index, column)| pad_cell(column, widths[index]))
			.collect::<Vec<_>>()
			.join(" | ")
	);
	let divider = format!(
		"| {} |",
		widths
			.iter()
			.map(|width| "-".repeat((*width).max(MIN_COLUMN_WIDTH)))
			.collect::<Vec<_>>()
			.join(" | ")
	);
	let mut lines = vec![header, divider];

	if rows.is_empty() {
		lines.push("(no rows)".to_string());
	} else {
		for row in rows {
			let cells: Vec<String> = columns
				.iter()
				.enumerate()
				.map(|(index, column)| {
					pad_cell(
						&stringify_value(row.get(column).unwrap_or(&SqlValue::Null)),
						widths[index],
					)
				})
				.collect();
			lines.push(format!("| {} |", cells.join(" | ")));
		}
	}

	lines
		.iter()
		.map(|line| truncate_to_width(&line.replace('\t', TAB_SPACES), MAX_RENDER_WIDTH))
		.collect::<Vec<_>>()
		.join("\n")
}

fn render_table_list(tables: &[TableSummary]) -> String {
	if tables.is_empty() {
		return "(no tables)".to_string();
	}
	tables
		.iter()
		.map(|table| {
			truncate_to_width(
				&format!("{} ({})", table.name, format_row_count(&table.count))
					.replace('\t', TAB_SPACES),
				MAX_RENDER_WIDTH,
			)
		})
		.collect::<Vec<_>>()
		.join("\n")
}

fn format_row_count(count: &RowCount) -> String {
	match count {
		RowCount::Exact(rows) => format!("{rows} rows"),
		RowCount::Estimate(rows) => format!("~{rows} rows"),
		RowCount::AtLeast(rows) => format!("{rows}+ rows"),
	}
}

fn render_schema(
	create_sql: &str,
	columns: &[String],
	rows: &[HashMap<String, SqlValue>],
) -> String {
	let schema_lines: Vec<String> = create_sql
		.replace('\t', TAB_SPACES)
		.split('\n')
		.map(|line| truncate_to_width(line, MAX_RENDER_WIDTH))
		.collect();
	[schema_lines.join("\n"), String::new(), "Sample rows:".to_string(), build_ascii_table(columns, rows)]
		.join("\n")
}

fn render_row(columns: &[String], row: &HashMap<String, SqlValue>) -> String {
	if columns.is_empty() {
		return "(no columns)".to_string();
	}
	columns
		.iter()
		.map(|column| {
			let value = stringify_value(row.get(column).unwrap_or(&SqlValue::Null));
			truncate_to_width(
				&format!("{column}: {value}").replace('\t', TAB_SPACES),
				MAX_RENDER_WIDTH,
			)
		})
		.collect::<Vec<_>>()
		.join("\n")
}

fn render_table(
	columns: &[String],
	rows: &[HashMap<String, SqlValue>],
	total_count: i64,
	offset: i64,
	limit: i64,
	table: &str,
) -> String {
	let mut parts = vec![build_ascii_table(columns, rows)];
	let shown = total_count.min(offset + rows.len() as i64);
	if shown < total_count {
		let remaining = total_count - shown;
		let next_offset = offset + rows.len() as i64;
		parts.push(truncate_to_width(
			&format!(
				"[{remaining} more rows; append :{table}?limit={limit}&offset={next_offset} to the database path to continue]"
			)
			.replace('\t', TAB_SPACES),
			MAX_RENDER_WIDTH,
		));
	}
	parts.join("\n")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn path_candidates_are_longest_first_and_skip_non_sqlite_suffixes() {
		let candidates = parse_path_candidates("data.sqlite:notes?limit=2");
		assert_eq!(candidates.len(), 1);
		assert_eq!(candidates[0].sqlite_path, "data.sqlite");
		assert_eq!(candidates[0].sub_path, "notes");
		assert_eq!(candidates[0].query_string, "limit=2");

		let candidates = parse_path_candidates("x.sqlite:notes.db:2");
		assert_eq!(
			candidates
				.iter()
				.map(|candidate| candidate.sqlite_path.as_str())
				.collect::<Vec<_>>(),
			vec!["x.sqlite:notes.db", "x.sqlite"]
		);
		assert_eq!(candidates[0].sub_path, "2");
		assert_eq!(candidates[1].sub_path, "notes.db:2");

		assert!(parse_path_candidates("data.sqlite.txt:5").is_empty());
		assert!(parse_path_candidates("notes.dbx").is_empty());
	}

	#[test]
	fn percent_decoding_matches_url_search_params() {
		assert_eq!(percent_decode("a+b%20c"), "a b c");
		assert_eq!(percent_decode("100%"), "100%");
		assert_eq!(percent_decode("%zz"), "%zz");
		assert_eq!(percent_decode("%C3%A9"), "é");
	}

	#[test]
	fn selector_kinds_match_the_upstream_contract() {
		assert_eq!(parse_selector("", "").unwrap(), SqliteSelector::List);
		assert_eq!(
			parse_selector("notes", "").unwrap(),
			SqliteSelector::Schema { table: "notes".into(), sample_limit: 5 }
		);
		assert_eq!(
			parse_selector(":notes:2", "").unwrap(),
			SqliteSelector::Row { table: "notes".into(), key: "2".into() }
		);
		assert!(matches!(
			parse_selector("notes", "limit=2&offset=3&order=id:desc&where=body%3D'x'").unwrap(),
			SqliteSelector::Query { limit: 2, offset: 3, .. }
		));
		assert!(matches!(
			parse_selector("", "q=SELECT%201").unwrap(),
			SqliteSelector::Raw { .. }
		));

		assert_eq!(
			parse_selector("", "limit=2").unwrap_err().to_string(),
			"SQLite query parameters require a table selector or q=SELECT..."
		);
		assert_eq!(
			parse_selector("", "q=").unwrap_err().to_string(),
			"SQLite query parameter 'q' cannot be empty"
		);
		assert_eq!(
			parse_selector("notes", "q=SELECT%201").unwrap_err().to_string(),
			"SQLite raw queries cannot be combined with table selectors or pagination"
		);
		assert_eq!(
			parse_selector("notes:1", "limit=2").unwrap_err().to_string(),
			"SQLite row lookups cannot be combined with query parameters"
		);
		assert_eq!(
			parse_selector("notes", "bogus=1").unwrap_err().to_string(),
			"Unsupported SQLite query parameter 'bogus'"
		);
	}

	#[test]
	fn limits_and_offsets_follow_parse_int_and_cap_at_500() {
		assert_eq!(parse_limit(None, DEFAULT_QUERY_LIMIT).unwrap(), 20);
		assert_eq!(parse_limit(Some(""), DEFAULT_QUERY_LIMIT).unwrap(), 20);
		assert_eq!(parse_limit(Some("999"), DEFAULT_QUERY_LIMIT).unwrap(), 500);
		assert_eq!(parse_limit(Some("12junk"), DEFAULT_QUERY_LIMIT).unwrap(), 12);
		assert!(parse_limit(Some("0"), DEFAULT_QUERY_LIMIT).is_err());
		assert!(parse_limit(Some("-3"), DEFAULT_QUERY_LIMIT).is_err());
		assert!(parse_limit(Some("abc"), DEFAULT_QUERY_LIMIT).is_err());
		assert!(parse_limit(Some("9".repeat(400).as_str()), DEFAULT_QUERY_LIMIT).is_err());
		assert_eq!(parse_offset(None).unwrap(), 0);
		assert_eq!(parse_offset(Some("7")).unwrap(), 7);
		assert!(parse_offset(Some("-1")).is_err());
	}

	#[test]
	fn where_guard_rejects_control_syntax_outside_quotes() {
		assert_eq!(validate_where_clause(Some(" body = 'x;y' ")).unwrap().as_deref(), Some("body = 'x;y'"));
		assert_eq!(validate_where_clause(Some("body = 'limit'")).unwrap().as_deref(), Some("body = 'limit'"));
		assert_eq!(
			validate_where_clause(Some("1=1; DROP TABLE notes")).unwrap_err().to_string(),
			COMMENT_OR_TERMINATOR_ERROR
		);
		assert_eq!(
			validate_where_clause(Some("1=1 -- x")).unwrap_err().to_string(),
			COMMENT_OR_TERMINATOR_ERROR
		);
		assert_eq!(
			validate_where_clause(Some("1=1 LIMIT 1")).unwrap_err().to_string(),
			FORBIDDEN_KEYWORD_ERROR
		);
		assert_eq!(
			validate_where_clause(Some("x IN (SELECT 1 UNION SELECT 2)")).unwrap_err().to_string(),
			FORBIDDEN_KEYWORD_ERROR
		);
	}

	#[test]
	fn width_and_truncation_match_terminal_cells() {
		assert_eq!(display_width("abc"), 3);
		assert_eq!(display_width("中a"), 3);
		assert_eq!(display_width("e\u{301}"), 1);
		assert_eq!(truncate_to_width("abcdef", 4), "abc…");
		assert_eq!(truncate_to_width("abc", 3), "abc");
		assert_eq!(truncate_to_width("abc", 1), "…");
		assert_eq!(truncate_to_width("中文", 3), "中…");
	}

	#[test]
	fn js_number_matches_string_number_for_common_values() {
		assert_eq!(js_number(1.0), "1");
		assert_eq!(js_number(0.1), "0.1");
		assert_eq!(js_number(1e21), "1e+21");
		assert_eq!(js_number(1.5e-7), "1.5e-7");
		assert_eq!(js_number(-0.0), "0");
	}

	#[test]
	fn ascii_tables_fall_back_to_vertical_blocks_past_19_columns() {
		let columns: Vec<String> = (0..20).map(|index| format!("c{index}")).collect();
		let mut row = HashMap::new();
		for column in &columns {
			row.insert(column.clone(), SqlValue::Integer(7));
		}
		let rendered = build_ascii_table(&columns, &[row]);
		assert!(rendered.starts_with("── Row 1 ──\n"), "{rendered}");
		assert!(rendered.contains("c0 : 7"), "{rendered}");
	}
}
