//! Native hashline contract: content tags, bounded snapshots, the patch
//! grammar, clipboard registers, block anchors, and the `edit` tool.
//!
//! One [`SnapshotStore`] per agent session is shared by `read` (via
//! [`NativeSnapshotSink`]) and `edit`; the store is injected, never global. A
//! session's tags are content-derived, so what the store scopes is read
//! provenance (which lines this session displayed) and `CUT` registers — a
//! child registry must rebind its own store so neither leaks between agents.

pub mod apply;
pub mod budget;
pub mod block;
pub mod clipboard;
pub mod edit;
pub mod hash;
pub mod mismatch;
pub mod parser;
pub mod recovery;
pub mod syntax;
pub mod snapshot;

use std::{path::Path, sync::Arc};

pub use edit::execute;
pub use snapshot::{Snapshot, SnapshotStore, canonical_key, format_header};

/// Read-side sink state: the session store this agent's reads and edits share.
#[derive(Debug, Clone)]
pub struct NativeSnapshotSink {
	store: Arc<SnapshotStore>,
}

impl NativeSnapshotSink {
	pub fn new(store: Arc<SnapshotStore>) -> Self {
		Self { store }
	}

	/// Record the full normalized text a read observed; returns its 4-hex tag
	/// (`None` above the 4 MiB snapshot cap).
	pub fn record_file(&self, absolute_path: &Path, full_text: &str) -> Option<String> {
		self.store.record(&canonical_key(absolute_path), full_text, None)
	}

	/// Attach the 1-indexed source lines the read actually displayed.
	pub fn record_seen_lines(&self, absolute_path: &Path, tag: &str, lines: &[u32]) {
		if lines.is_empty() {
			return;
		}
		self
			.store
			.record_seen_lines(&canonical_key(absolute_path), tag, lines);
	}

	/// Section header line (`[path#TAG]`), with the path shown relative to
	/// `cwd` when it lives underneath it.
	pub fn header(&self, absolute_path: &Path, tag: &str, cwd: &Path) -> String {
		format_header(&display_path(absolute_path, cwd), tag)
	}
}

/// Present a read path relative to the session cwd (`/` separators).
pub fn display_path(path: &Path, cwd: &Path) -> String {
	match path.strip_prefix(cwd) {
		Ok(relative) => {
			let text = relative.to_string_lossy().replace('\\', "/");
			if text.is_empty() { ".".to_string() } else { text }
		}
		Err(_) => path.to_string_lossy().into_owned(),
	}
}
