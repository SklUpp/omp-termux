//! Concrete per-session snapshot store for hashline section tags.
//!
//! A tag is `xxh32(normalized full text) & 0xffff`; any read of byte-identical
//! content mints the same tag, so a follow-up edit validates as long as the
//! live file still hashes to it. The store keeps a bounded history of full-file
//! versions per canonical path so in-session edit chains can name the version
//! they anchored against, plus the 1-indexed lines a producer displayed.

use std::{
	collections::{BTreeSet, HashMap},
	path::Path,
	sync::Mutex,
};

use super::hash::compute_file_hash;

/// Files above this size never mint a tag: line-anchored editing of
/// multi-megabyte files is out of scope under the full-content model.
pub const SNAPSHOT_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Distinct paths retained at once (LRU).
const MAX_PATHS: usize = 256;
/// Full-file versions retained per path (oldest dropped first).
const MAX_VERSIONS_PER_PATH: usize = 4;
/// Global ceiling on retained text across all paths.
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;

/// One observed full-file version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
	/// Canonical path this version belongs to.
	pub path: String,
	/// Full normalized (LF, no BOM) file text as observed.
	pub text: String,
	/// Content-derived tag for [`Snapshot::text`].
	pub hash: String,
	/// 1-indexed lines a producer actually displayed. `None` means no
	/// provenance was recorded, which disables the seen-line guard.
	pub seen_lines: Option<BTreeSet<u32>>,
}

#[derive(Debug, Default)]
struct Inner {
	/// Per-path version history, newest first.
	versions: HashMap<String, Vec<Snapshot>>,
	/// Canonical paths, least-recently-used first.
	recency: Vec<String>,
}

impl Inner {
	fn touch(&mut self, path: &str) {
		self.recency.retain(|entry| entry != path);
		self.recency.push(path.to_string());
	}

	fn total_bytes(&self) -> usize {
		self.versions
			.values()
			.flat_map(|history| history.iter())
			.map(|snapshot| snapshot.text.len())
			.sum()
	}

	fn evict(&mut self) {
		while self.versions.len() > MAX_PATHS || self.total_bytes() > MAX_TOTAL_BYTES {
			if self.recency.len() <= 1 {
				break;
			}
			let Some(lru) = self.recency.first().cloned() else { break };
			self.recency.remove(0);
			self.versions.remove(&lru);
		}
	}
}

/// In-memory, LRU-bounded snapshot store. Cheap to share: all state sits behind
/// one mutex and every lookup returns a clone.
#[derive(Debug, Default)]
pub struct SnapshotStore {
	inner: Mutex<Inner>,
}

impl SnapshotStore {
	pub fn new() -> Self {
		Self::default()
	}

	/// Record `text` (normalized full file) for `path` and return its tag.
	/// Returns `None` when the text exceeds [`SNAPSHOT_MAX_BYTES`].
	///
	/// Recording byte-identical content again refreshes recency and unions any
	/// newly displayed lines; distinct texts that collide on the 16-bit tag are
	/// kept as separate versions because the tag is only an index.
	pub fn record(&self, path: &str, text: &str, seen_lines: Option<&[u32]>) -> Option<String> {
		if text.len() > SNAPSHOT_MAX_BYTES {
			return None;
		}
		let hash = compute_file_hash(text);
		let mut inner = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
		inner.touch(path);
		let mut refreshed = false;
		{
			let history = inner.versions.entry(path.to_string()).or_default();
			if let Some(index) = history
				.iter()
				.position(|snapshot| snapshot.hash == hash && snapshot.text == text)
			{
				merge_seen_lines(&mut history[index], seen_lines);
				let existing = history.remove(index);
				history.insert(0, existing);
				refreshed = true;
			}
		}
		if !refreshed {
			let mut snapshot = Snapshot {
				path: path.to_string(),
				text: text.to_string(),
				hash: hash.clone(),
				seen_lines: None,
			};
			merge_seen_lines(&mut snapshot, seen_lines);
			let history = inner.versions.entry(path.to_string()).or_default();
			history.insert(0, snapshot);
			history.truncate(MAX_VERSIONS_PER_PATH);
		}
		inner.evict();
		Some(hash)
	}

	/// Merge displayed lines into the version tagged `hash`.
	pub fn record_seen_lines(&self, path: &str, hash: &str, lines: &[u32]) {
		let mut inner = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
		let Some(history) = inner.versions.get_mut(path) else { return };
		let Some(snapshot) = history.iter_mut().find(|snapshot| snapshot.hash == hash) else {
			return;
		};
		merge_seen_lines(snapshot, Some(lines));
	}

	/// Most-recently recorded version for `path`.
	pub fn head(&self, path: &str) -> Option<Snapshot> {
		let inner = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
		inner.versions.get(path)?.first().cloned()
	}

	/// Retained version for `path` whose tag equals `hash`.
	pub fn by_hash(&self, path: &str, hash: &str) -> Option<Snapshot> {
		let inner = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
		inner
			.versions
			.get(path)?
			.iter()
			.find(|snapshot| snapshot.hash == hash)
			.cloned()
	}

	/// Retained version for `path` whose text equals `text`.
	pub fn by_content(&self, path: &str, text: &str) -> Option<Snapshot> {
		let inner = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
		inner
			.versions
			.get(path)?
			.iter()
			.find(|snapshot| snapshot.text == text)
			.cloned()
	}

	/// Drop the version history for one path.
	pub fn invalidate(&self, path: &str) {
		let mut inner = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
		inner.versions.remove(path);
		inner.recency.retain(|entry| entry != path);
	}

	/// Move retained history from `from` to `to` so tags minted from reads of
	/// the source path stay valid at the destination.
	pub fn relocate(&self, from: &str, to: &str) {
		let mut inner = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
		let Some(mut moved) = inner.versions.remove(from) else { return };
		inner.recency.retain(|entry| entry != from);
		for snapshot in &mut moved {
			snapshot.path = to.to_string();
		}
		let history = inner.versions.entry(to.to_string()).or_default();
		let mut merged: Vec<Snapshot> = Vec::new();
		for snapshot in moved.into_iter().chain(history.drain(..)) {
			if merged.iter().any(|seen| seen.hash == snapshot.hash) {
				continue;
			}
			merged.push(snapshot);
		}
		merged.truncate(MAX_VERSIONS_PER_PATH);
		*history = merged;
		inner.touch(to);
		inner.evict();
	}

	/// Drop every version history.
	pub fn clear(&self) {
		let mut inner = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
		inner.versions.clear();
		inner.recency.clear();
	}
}

fn merge_seen_lines(snapshot: &mut Snapshot, lines: Option<&[u32]>) {
	let Some(lines) = lines else { return };
	if lines.is_empty() {
		return;
	}
	let seen = snapshot.seen_lines.get_or_insert_with(BTreeSet::new);
	for line in lines {
		seen.insert(*line);
	}
}

/// Canonicalize a path into the stable key the store uses: symlink-resolved
/// absolute path, falling back to the resolved parent plus file name, then to
/// the input. Non-existent paths (creates) still land on one stable key.
pub fn canonical_key(path: &Path) -> String {
	if let Ok(resolved) = std::fs::canonicalize(path) {
		return resolved.to_string_lossy().into_owned();
	}
	if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
		if let Ok(resolved_parent) = std::fs::canonicalize(parent) {
			return resolved_parent.join(name).to_string_lossy().into_owned();
		}
	}
	path.to_string_lossy().into_owned()
}

/// Section header line (`[path#TAG]`) for a displayed path.
pub fn format_header(display_path: &str, tag: &str) -> String {
	format!("[{display_path}#{tag}]")
}
