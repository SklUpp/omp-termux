//! Stored-session scan for `session/list`, `_omp/sessions/listAll`,
//! `_omp/projects/list` and `_omp/chats/byCwd`.
//!
//! Ported from `session/session-listing.ts` (`scanSessionFile`, `list`, `listAll`)
//! and the ACP wrappers (`acp-agent.ts:1134-1171, 2212-2215`): the title slot
//! (first fixed-width line) wins over the header title, entries sort by mtime
//! descending, and `session/list` pages by numeric cursor offset in 50-item
//! windows. Parsers are the native ones (`parse_title_slot`, `SessionHeader`);
//! nothing here writes.

use std::{
	fs,
	io::{BufRead, BufReader},
	path::{Path, PathBuf},
	time::SystemTime,
};

use serde_json::{Value, json};

use crate::session::{
	SessionHeader, SessionOptions, compute_default_session_dir, parse_title_slot,
};

/// Window for `session/list`.
pub const SESSION_PAGE_SIZE: usize = 50;

/// One stored log, as far as ACP listing uses it.
#[derive(Debug, Clone)]
pub struct StoredSession {
	pub id: String,
	pub cwd: String,
	pub title: Option<String>,
	pub path: PathBuf,
	pub modified: SystemTime,
	pub message_count: usize,
	pub size: u64,
}

/// Scan every session directory under the configured root (or only `cwd`'s
/// directory when one is supplied), newest first.
#[must_use]
pub fn list_sessions(cwd: Option<&Path>, sessions_root: Option<&Path>) -> Vec<StoredSession> {
	let root = sessions_root
		.map(Path::to_path_buf)
		.unwrap_or_else(SessionOptions::default_sessions_root);
	let dirs: Vec<PathBuf> = match cwd {
		Some(cwd) => vec![compute_default_session_dir(cwd, Some(&root))],
		None => match fs::read_dir(&root) {
			Ok(entries) => entries
				.flatten()
				.map(|entry| entry.path())
				.filter(|path| path.is_dir())
				.collect(),
			Err(_) => Vec::new(),
		},
	};
	let mut sessions: Vec<StoredSession> = Vec::new();
	for dir in dirs {
		let Ok(entries) = fs::read_dir(&dir) else {
			continue;
		};
		for entry in entries.flatten() {
			let path = entry.path();
			if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
				continue;
			}
			if let Some(session) = scan_session_file(&path) {
				sessions.push(session);
			}
		}
	}
	sessions.sort_by(|left, right| {
		right
			.modified
			.cmp(&left.modified)
			.then_with(|| left.path.cmp(&right.path))
	});
	sessions
}

/// Find one session by id, preferring the `cwd` directory and falling back to
/// the global scan (TS `#findStoredSession`; ids are globally unique).
#[must_use]
pub fn find_session(id: &str, cwd: &Path, sessions_root: Option<&Path>) -> Option<StoredSession> {
	list_sessions(Some(cwd), sessions_root)
		.into_iter()
		.find(|session| session.id == id)
		.or_else(|| {
			list_sessions(None, sessions_root)
				.into_iter()
				.find(|session| session.id == id)
		})
}

/// One page plus the next cursor (`None` on the last page).
#[must_use]
pub fn page(
	sessions: &[StoredSession],
	cursor: Option<&str>,
) -> Result<(Vec<StoredSession>, Option<String>), String> {
	let offset = match cursor {
		None => 0,
		Some(cursor) => cursor
			.parse::<usize>()
			.map_err(|_| format!("Invalid ACP session cursor: {cursor}"))?,
	};
	let paged = sessions
		.iter()
		.skip(offset)
		.take(SESSION_PAGE_SIZE)
		.cloned()
		.collect::<Vec<_>>();
	let next = offset + paged.len();
	Ok((paged, (next < sessions.len()).then(|| next.to_string())))
}

/// ACP `SessionInfo` shape (`acp-agent.ts:2042-2050`).
#[must_use]
pub fn session_info(session: &StoredSession) -> Value {
	let mut info = json!({
		"sessionId": session.id,
		"cwd": session.cwd,
		"updatedAt": crate::output::iso8601(session.modified),
		"_meta": {
			"messageCount": session.message_count,
			"size": session.size,
		},
	});
	if let Some(title) = &session.title {
		info["title"] = json!(title);
	}
	info
}

/// `_omp/projects/list` buckets, most recent activity first.
#[must_use]
pub fn projects(sessions: &[StoredSession]) -> (Vec<Value>, usize) {
	let mut buckets: std::collections::BTreeMap<String, (usize, u64, String)> =
		std::collections::BTreeMap::new();
	for session in sessions {
		if session.cwd.is_empty() {
			continue;
		}
		let modified = session
			.modified
			.duration_since(SystemTime::UNIX_EPOCH)
			.unwrap_or_default()
			.as_millis() as u64;
		let entry = buckets
			.entry(session.cwd.clone())
			.or_insert((0, 0, String::new()));
		entry.0 += 1;
		if modified > entry.1 {
			entry.1 = modified;
			entry.2 = session.title.clone().unwrap_or_default();
		}
	}
	let mut projects: Vec<Value> = buckets
		.into_iter()
		.map(|(cwd, (session_count, last_activity, last_title))| {
			json!({
				"cwd": cwd,
				"sessionCount": session_count,
				"lastActivityAt": last_activity,
				"lastTitle": last_title,
			})
		})
		.collect();
	projects.sort_by(|left, right| {
		right["lastActivityAt"]
			.as_u64()
			.cmp(&left["lastActivityAt"].as_u64())
	});
	(projects, sessions.len())
}

fn scan_session_file(path: &Path) -> Option<StoredSession> {
	let file = fs::File::open(path).ok()?;
	let metadata = file.metadata().ok()?;
	let modified = metadata.modified().ok()?;
	let reader = BufReader::new(file);
	let mut title_slot = None;
	let mut header: Option<SessionHeader> = None;
	let mut message_count = 0usize;
	for (index, line) in reader.lines().enumerate() {
		let line = line.ok()?;
		if index == 0 {
			title_slot = parse_title_slot(&line);
			continue;
		}
		if index == 1 {
			header = serde_json::from_str::<SessionHeader>(&line).ok();
			continue;
		}
		if line.trim_start().starts_with("{\"type\":\"message\"") {
			message_count += 1;
		}
	}
	let header = header?;
	let title = title_slot
		.filter(|slot| !slot.title.trim().is_empty())
		.map(|slot| slot.title)
		.or_else(|| {
			header
				.title
				.clone()
				.filter(|title| !title.trim().is_empty())
		});
	Some(StoredSession {
		id: header.id,
		cwd: header.cwd,
		title,
		path: path.to_path_buf(),
		modified,
		message_count,
		size: metadata.len(),
	})
}
