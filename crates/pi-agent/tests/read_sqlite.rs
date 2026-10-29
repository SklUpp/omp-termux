//! Consumer-visible SQLite `read` contract: bare listings, schema samples,
//! primary-key/rowid lookups, structured queries, raw `?q=` reads, caps,
//! selector disambiguation, WAL sidecar initialization, read-only enforcement,
//! and cancellation. Drives the public [`ReadTool`] exactly like the agent loop
//! and the headless CLI do.

use std::{
	fs,
	path::{Path, PathBuf},
	time::{SystemTime, UNIX_EPOCH},
};

use pi_agent::{AgentContext, CancellationToken, ReadTool, Tool, ToolResult};
use rusqlite::Connection;
use serde_json::json;

struct TempDir(PathBuf);

impl TempDir {
	fn new(label: &str) -> Self {
		let nanos = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.map(|duration| duration.as_nanos())
			.unwrap_or_default();
		let path = std::env::temp_dir().join(format!(
			"omp-read-sqlite-{label}-{}-{nanos}",
			std::process::id()
		));
		fs::create_dir_all(&path).expect("create temp dir");
		Self(path)
	}

	fn path(&self) -> &Path {
		&self.0
	}

	fn file(&self, name: &str) -> PathBuf {
		self.0.join(name)
	}
}

impl Drop for TempDir {
	fn drop(&mut self) {
		let _ = fs::remove_dir_all(&self.0);
	}
}

fn create_connection(path: &Path) -> Connection {
	Connection::open(path).expect("open sqlite fixture")
}

/// One fixture with a single-column PK, a text PK in a `WITHOUT ROWID` table,
/// a composite PK, a rowid-only table, and a table large enough for the schema
/// continuation notice.
fn create_fixture(dir: &TempDir) -> PathBuf {
	let path = dir.file("sample.sqlite");
	let connection = create_connection(&path);
	connection
		.execute_batch(
			"CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT, score REAL);
			 INSERT INTO notes VALUES (1, 'alpha', 1.5), (2, 'beta', 2.5);
			 CREATE TABLE codes(code TEXT PRIMARY KEY, label TEXT) WITHOUT ROWID;
			 INSERT INTO codes VALUES ('a', 'A');
			 CREATE TABLE pairs(a INTEGER, b INTEGER, PRIMARY KEY(a, b));
			 INSERT INTO pairs VALUES (1, 2);
			 CREATE TABLE loose(value TEXT);
			 INSERT INTO loose VALUES ('first');
			 CREATE TABLE many(id INTEGER PRIMARY KEY);
			 INSERT INTO many(id) VALUES (1), (2), (3), (4), (5), (6), (7);",
		)
		.expect("create fixture");
	path
}

async fn read(dir: &TempDir, selector: &str) -> ToolResult {
	let tool = ReadTool::new();
	tool.execute(&json!({ "path": selector }), &AgentContext::new(dir.path().to_path_buf()))
		.await
		.expect("read executes")
}

fn selector(path: &Path, suffix: &str) -> String {
	format!("{}{suffix}", path.display())
}

/// Header plus data cells of the first rendered ASCII table, skipping the
/// divider row. Cells are trimmed; counts stay semantic, not prose.
fn ascii_table(content: &str) -> (Vec<String>, Vec<Vec<String>>) {
	let cell_split = |line: &str| -> Vec<String> {
		line.trim_matches('|')
			.split('|')
			.map(|cell| cell.trim().to_string())
			.collect()
	};
	let mut lines = content.lines().filter(|line| line.starts_with("| "));
	let header = cell_split(lines.next().expect("rendered table header"));
	let rows = lines.skip(1).map(cell_split).collect();
	(header, rows)
}

#[tokio::test]
async fn bare_database_path_lists_tables_with_row_counts() {
	let dir = TempDir::new("list");
	let path = create_fixture(&dir);

	let result = read(&dir, &path.display().to_string()).await;
	assert!(!result.is_error, "{}", result.content);
	assert!(!result.content.contains("sqlite_"), "{}", result.content);

	let connection = create_connection(&path);
	let fixture_counts = [("codes", 1), ("loose", 1), ("many", 7), ("notes", 2), ("pairs", 1)];
	let mut listed = 0usize;
	for (name, fixture_count) in fixture_counts {
		let expected: i64 = connection
			.query_row(&format!("SELECT COUNT(*) FROM \"{name}\""), [], |row| row.get(0))
			.expect("count fixture rows");
		assert_eq!(expected, fixture_count, "{name} fixture count");
		let line = result
			.content
			.lines()
			.find(|line| line.starts_with(&format!("{name} ")))
			.unwrap_or_else(|| panic!("missing table {name}: {}", result.content));
		let counts_match = line
			.split(|character: char| !character.is_ascii_digit())
			.any(|digits| digits == expected.to_string().as_str());
		assert!(counts_match, "{name} should report {expected} rows: {line}");
		listed += 1;
	}
	assert_eq!(listed, 5, "{}", result.content);

	let details = result.details.expect("details");
	assert_eq!(details["resolvedPath"], json!(path.display().to_string()));
}

#[tokio::test]
async fn schema_reads_show_create_sql_samples_and_continuation() {
	let dir = TempDir::new("schema");
	let path = create_fixture(&dir);

	let result = read(&dir, &selector(&path, ":notes")).await;
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT, score REAL)"));
	assert!(result.content.contains("Sample rows:"));
	assert!(result.content.contains("alpha"));
	assert!(result.content.contains("| id"));

	let result = read(&dir, &selector(&path, ":many")).await;
	assert!(!result.is_error, "{}", result.content);
	assert!(
		result.content.contains(
			"[2 more rows; append :many?limit=20&offset=5 to the database path to continue]"
		),
		"{}",
		result.content
	);

	let result = read(&dir, &selector(&path, ":missing")).await;
	assert!(result.is_error);
	assert_eq!(result.content, "SQLite table 'missing' not found");
}

#[tokio::test]
async fn row_lookups_use_primary_key_rowid_and_report_composite_keys() {
	let dir = TempDir::new("row");
	let path = create_fixture(&dir);

	let result = read(&dir, &selector(&path, ":notes:2")).await;
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("body: beta"), "{}", result.content);
	assert!(!result.content.contains("alpha"), "{}", result.content);

	let result = read(&dir, &selector(&path, ":codes:a")).await;
	assert!(!result.is_error, "{}", result.content);
	assert_eq!(result.content, "code: a\nlabel: A");

	let result = read(&dir, &selector(&path, ":loose:1")).await;
	assert!(!result.is_error, "{}", result.content);
	assert_eq!(result.content, "value: first");

	let result = read(&dir, &selector(&path, ":pairs:1")).await;
	assert!(result.is_error);
	assert!(
		result.content.contains("has a composite primary key; use '?where=' instead"),
		"{}",
		result.content
	);

	let result = read(&dir, &selector(&path, ":notes:99")).await;
	assert!(!result.is_error);
	assert_eq!(result.content, "No row found in table 'notes' for key '99'.");
}

#[tokio::test]
async fn pagination_where_order_and_errors_follow_the_query_contract() {
	let dir = TempDir::new("query");
	let path = create_fixture(&dir);

	let result = read(&dir, &selector(&path, ":many?limit=1&offset=1")).await;
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("| 2"), "{}", result.content);
	assert!(
		result.content.contains(
			"[5 more rows; append :many?limit=1&offset=2 to the database path to continue]"
		),
		"{}",
		result.content
	);

	let result = read(&dir, &selector(&path, ":many?where=id%3E5")).await;
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("| 6"), "{}", result.content);
	assert!(result.content.contains("| 7"), "{}", result.content);
	assert!(!result.content.contains("| 1 |"), "{}", result.content);

	let result = read(&dir, &selector(&path, ":many?order=id:desc&limit=2")).await;
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("| 7"), "{}", result.content);
	assert!(result.content.contains("| 6"), "{}", result.content);

	let result = read(&dir, &selector(&path, ":many?limit=999")).await;
	assert!(!result.is_error, "{}", result.content);
	let (headers, rows) = ascii_table(&result.content);
	assert_eq!(headers, ["id"]);
	assert_eq!(rows.len(), 7, "{}", result.content);
	for (index, row) in rows.into_iter().enumerate() {
		assert_eq!(row, vec![(index + 1).to_string()], "{}", result.content);
	}

	for (suffix, message) in [
		(":many?limit=0", "SQLite limit must be a positive integer; got '0'"),
		(":many?offset=-1", "SQLite offset must be a non-negative integer; got '-1'"),
		(":many?bogus=1", "Unsupported SQLite query parameter 'bogus'"),
		(":many?order=missing", "SQLite order column 'missing' not found in table schema"),
		(":many?order=id:sideways", "SQLite order direction must be 'asc' or 'desc'; got 'sideways'"),
		(
			":many?where=1%3D1%3BDROP%20TABLE%20notes",
			"SQLite 'where' clause must not contain comments or statement terminators; use '?q=SELECT ...' for raw SQL",
		),
		(
			":many?where=1%3D1%20LIMIT%201",
			"SQLite 'where' clause must not contain LIMIT/OFFSET/UNION/INTERSECT/EXCEPT/ATTACH/DETACH/PRAGMA; use '?q=SELECT ...' for raw SQL",
		),
	] {
		let result = read(&dir, &selector(&path, suffix)).await;
		assert!(result.is_error, "{suffix} should fail");
		assert_eq!(result.content, message, "{suffix}");
	}

	let result = read(&dir, &selector(&path, "?q=SELECT%20count(*)%20AS%20n%20FROM%20notes")).await;
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("n"), "{}", result.content);
	assert!(result.content.contains("| 2"), "{}", result.content);

	let result = read(&dir, &selector(&path, "?q=SELECT%201&limit=2")).await;
	assert!(result.is_error);
	assert_eq!(
		result.content,
		"SQLite raw queries cannot be combined with table selectors or pagination"
	);

	let result = read(&dir, &selector(&path, ":notes:1?limit=2")).await;
	assert!(result.is_error);
	assert_eq!(result.content, "SQLite row lookups cannot be combined with query parameters");
}

#[tokio::test]
async fn raw_queries_read_but_never_write_attach_or_flip_pragmas() {
	let dir = TempDir::new("readonly");
	let path = create_fixture(&dir);
	let before = fs::read(&path).expect("read fixture bytes");

	for query in [
		"?q=DELETE%20FROM%20notes",
		"?q=DELETE+FROM+notes",
		"?q=CREATE%20TABLE%20evil(x%20INTEGER)",
		"?q=ATTACH%20DATABASE%20'evil.db'%20AS%20evil",
		"?q=PRAGMA%20journal_mode%3DWAL",
		"?q=SELECT%20%3F%20AS%20x",
	] {
		let result = read(&dir, &selector(&path, query)).await;
		assert!(result.is_error, "{query} must be rejected: {}", result.content);
	}
	assert_eq!(fs::read(&path).expect("read fixture bytes"), before, "database mutated");

	let result = read(&dir, &selector(&path, ":notes")).await;
	assert!(!result.is_error, "{}", result.content);
	assert_eq!(fs::read(&path).expect("read fixture bytes"), before, "database mutated");
}

#[tokio::test]
async fn raw_queries_cap_rows_and_render_wide_tables_vertically() {
	let dir = TempDir::new("caps");
	let path = dir.file("caps.sqlite");
	{
		let connection = create_connection(&path);
		connection
			.execute_batch("CREATE TABLE big(id INTEGER PRIMARY KEY);")
			.expect("create big");
		{
			let transaction = connection.unchecked_transaction().expect("transaction");
			{
				let mut statement = transaction
					.prepare("INSERT INTO big(id) VALUES (?1)")
					.expect("prepare insert");
				for id in 1..=1001 {
					statement.execute([id]).expect("insert row");
				}
			}
			transaction.commit().expect("commit");
		}
		connection
			.execute_batch(&format!(
				"CREATE TABLE wide({});",
				(0..20)
					.map(|index| format!("c{index} INTEGER"))
					.collect::<Vec<_>>()
					.join(", ")
			))
			.expect("create wide");
		connection
			.execute_batch("INSERT INTO wide VALUES (0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19)")
			.expect("insert wide");
	}

	let result = read(&dir, &selector(&path, "?q=SELECT%20*%20FROM%20big")).await;
	assert!(!result.is_error, "{}", result.content);
	assert!(
		result
			.content
			.contains("[Output capped at 1000 rows; add a LIMIT/OFFSET clause to the query to page through more]"),
		"{}",
		result.content
	);

	let result = read(&dir, &selector(&path, ":wide")).await;
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("── Row 1 ──"), "{}", result.content);
	assert!(result.content.contains("c0 "), "{}", result.content);
}

#[tokio::test]
async fn colon_selectors_disambiguate_from_literal_colon_filenames() {
	let dir = TempDir::new("literal");
	let path = create_fixture(&dir);
	fs::write(dir.file("lit.db:notes"), "plain text").expect("write literal colon file");

	let result = read(&dir, &selector(&path, ":notes?limit=1")).await;
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("alpha"), "{}", result.content);
	assert!(!result.content.contains("beta"), "{}", result.content);

	let result = read(&dir, "lit.db:notes").await;
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("plain text"), "{}", result.content);
	assert!(!result.content.contains("alpha"), "literal colon filename was parsed as a SQLite selector");
}

#[tokio::test]
async fn wal_database_reads_after_missing_sidecars_are_reinitialized() {
	let dir = TempDir::new("wal");
	let path = dir.file("wal.sqlite");
	{
		let connection = create_connection(&path);
		connection
			.execute_batch(
				"PRAGMA journal_mode = WAL;
				 CREATE TABLE events(id INTEGER PRIMARY KEY, payload TEXT);
				 INSERT INTO events VALUES (1, 'wal-row');",
			)
			.expect("create wal fixture");
		connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").expect("checkpoint");
	}
	let _ = fs::remove_file(format!("{}-wal", path.display()));
	let _ = fs::remove_file(format!("{}-shm", path.display()));

	let result = read(&dir, &selector(&path, ":events")).await;
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("wal-row"), "{}", result.content);
}

#[tokio::test]
async fn missing_path_resolves_through_the_workspace_suffix_match() {
	let dir = TempDir::new("suffix");
	let nested = dir.file("nested");
	fs::create_dir_all(&nested).expect("create nested");
	let path = nested.join("sample.sqlite");
	{
		let connection = create_connection(&path);
		connection
			.execute_batch("CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT); INSERT INTO notes VALUES (1, 'alpha');")
			.expect("create nested fixture");
	}

	let result = read(&dir, "sample.sqlite:notes").await;
	assert!(!result.is_error, "{}", result.content);
	assert!(
		result
			.content
			.contains("[Path 'sample.sqlite' not found; resolved to 'nested/sample.sqlite' via suffix match]"),
		"{}",
		result.content
	);
	assert!(result.content.contains("alpha"), "{}", result.content);
}

#[tokio::test]
async fn cancelled_context_short_circuits_before_touching_the_database() {
	let dir = TempDir::new("cancel");
	let path = create_fixture(&dir);
	let token = CancellationToken::new();
	token.cancel();

	let tool = ReadTool::new();
	let result = tool
		.execute(
			&json!({ "path": path.display().to_string() }),
			&AgentContext::with_cancellation(dir.path().to_path_buf(), token),
		)
		.await
		.expect("read executes");
	assert!(result.is_error);
	assert_eq!(result.content, "Read cancelled");
}

#[tokio::test]
async fn table_listing_stops_at_500_and_reports_the_limit() {
	let dir = TempDir::new("limit");
	let path = dir.file("many-tables.sqlite");
	{
		let connection = create_connection(&path);
		connection
			.execute_batch(
				&(0..505)
					.map(|index| format!("CREATE TABLE t{index}(x INTEGER);"))
					.collect::<Vec<_>>()
					.join("\n"),
			)
			.expect("create tables");
	}

	let result = read(&dir, &path.display().to_string()).await;
	assert!(!result.is_error, "{}", result.content);
	assert_eq!(result.content.lines().count(), 500, "{}", result.content);
	let details = result.details.expect("details");
	assert_eq!(details["meta"]["limits"]["resultLimit"]["reached"], json!(500));
	assert_eq!(details["meta"]["limits"]["resultLimit"]["suggestion"], json!(1000));
}

#[tokio::test]
async fn generated_columns_render_under_their_own_headers() {
	let dir = TempDir::new("generated");
	let path = dir.file("generated.sqlite");
	{
		let connection = create_connection(&path);
		connection
			.execute_batch(
				"CREATE TABLE gen(
					id INTEGER PRIMARY KEY,
					a INTEGER,
					doubled INTEGER GENERATED ALWAYS AS (a * 2) STORED,
					tripled INTEGER GENERATED ALWAYS AS (a * 3) VIRTUAL,
					label TEXT
				 );
				 INSERT INTO gen(id, a, label) VALUES (1, 7, 'x');",
			)
			.expect("create generated fixture");
	}

	let result = read(&dir, &selector(&path, ":gen:1")).await;
	assert!(!result.is_error, "{}", result.content);
	let rendered: Vec<(String, String)> = result
		.content
		.lines()
		.filter_map(|line| line.split_once(": "))
		.map(|(column, value)| {
			(column.trim().to_string(), value.trim().to_string())
		})
		.collect();
	let expected_rows = vec![
		("id".to_string(), "1".to_string()),
		("a".to_string(), "7".to_string()),
		("doubled".to_string(), "14".to_string()),
		("tripled".to_string(), "21".to_string()),
		("label".to_string(), "x".to_string()),
	];
	assert_eq!(rendered, expected_rows, "{}", result.content);

	let result = read(&dir, &selector(&path, ":gen?limit=1")).await;
	assert!(!result.is_error, "{}", result.content);
	let (headers, rows) = ascii_table(&result.content);
	assert_eq!(headers, ["id", "a", "doubled", "tripled", "label"]);
	assert_eq!(rows, vec![vec!["1", "7", "14", "21", "x"]]);

	let result = read(&dir, &selector(&path, ":gen")).await;
	assert!(!result.is_error, "{}", result.content);
	let (headers, rows) = ascii_table(&result.content);
	assert_eq!(headers, ["id", "a", "doubled", "tripled", "label"]);
	assert_eq!(rows, vec![vec!["1", "7", "14", "21", "x"]]);

	let result = read(&dir, &selector(&path, "?q=SELECT%20*%20FROM%20gen")).await;
	assert!(!result.is_error, "{}", result.content);
	let (headers, rows) = ascii_table(&result.content);
	assert_eq!(headers, ["id", "a", "doubled", "tripled", "label"]);
	assert_eq!(rows, vec![vec!["1", "7", "14", "21", "x"]]);
}

#[tokio::test]
async fn invalid_utf8_text_cells_are_replaced_without_panicking() {
	let dir = TempDir::new("utf8");
	let path = dir.file("bytes.sqlite");
	{
		let connection = create_connection(&path);
		connection
			.execute_batch(
				"CREATE TABLE raw(id INTEGER PRIMARY KEY, body TEXT);
				 INSERT INTO raw(id, body) VALUES (1, CAST(x'80' AS TEXT));",
			)
			.expect("create raw fixture");
	}

	for suffix in ["?q=SELECT%20body%20FROM%20raw", ":raw", ":raw:1"] {
		let result = read(&dir, &selector(&path, suffix)).await;
		assert!(!result.is_error, "{suffix}: {}", result.content);
		assert!(result.content.contains('\u{FFFD}'), "{suffix}: {}", result.content);
	}
}

#[tokio::test]
async fn read_only_pragma_introspection_works_but_writes_stay_blocked() {
	let dir = TempDir::new("pragma");
	let path = dir.file("pragma.sqlite");
	{
		let connection = create_connection(&path);
		connection
			.execute_batch(
				"CREATE TABLE parent(id INTEGER PRIMARY KEY);
				 CREATE TABLE child(id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id));
				 CREATE INDEX child_parent ON child(parent_id);
				 INSERT INTO parent VALUES (1);
				 INSERT INTO child VALUES (1, 1);",
			)
			.expect("create pragma fixture");
	}
	let before = fs::read(&path).expect("read fixture bytes");

	for (query, expected) in [
		("?q=PRAGMA%20index_list('child')", "child_parent"),
		("?q=PRAGMA%20index_info('child_parent')", "parent_id"),
		("?q=PRAGMA%20index_xinfo('child_parent')", "parent_id"),
		("?q=PRAGMA%20foreign_key_list('child')", "parent"),
		("?q=PRAGMA%20integrity_check(5)", "ok"),
		("?q=PRAGMA%20quick_check(5)", "ok"),
		("?q=PRAGMA%20table_info('child')", "parent_id"),
		("?q=PRAGMA%20table_xinfo('child')", "parent_id"),
		("?q=PRAGMA%20table_list", "child"),
		("?q=PRAGMA%20database_list", "main"),
		("?q=PRAGMA%20collation_list", "BINARY"),
		("?q=PRAGMA%20user_version", "user_version"),
	] {
		let result = read(&dir, &selector(&path, query)).await;
		assert!(!result.is_error, "{query}: {}", result.content);
		assert!(result.content.contains(expected), "{query}: {}", result.content);
	}
	let checked = read(&dir, &selector(&path, "?q=PRAGMA%20foreign_key_check('child')")).await;
	assert!(!checked.is_error, "{}", checked.content);
	assert!(ascii_table(&checked.content).1.is_empty(), "unexpected foreign-key violations");

	for query in [
		"?q=PRAGMA%20journal_mode%3DWAL",
		"?q=PRAGMA%20user_version%3D5",
		"?q=PRAGMA%20page_size%3D8192",
		"?q=PRAGMA%20schema_version%3D7",
		"?q=PRAGMA%20query_only",
	] {
		let result = read(&dir, &selector(&path, query)).await;
		assert!(result.is_error, "{query} must be rejected: {}", result.content);
	}
	assert_eq!(fs::read(&path).expect("read fixture bytes"), before, "database mutated");
}
