//! Cumulative expansion budget shared by parsing, clipboard expansion, apply,
//! and the prepared-section state of one `edit` call.
//!
//! Every charge happens *before* the allocation it pays for, so a small input
//! cannot make the tool clone or materialize gigabytes (e.g. one multi-MiB
//! register pasted a thousand times).

/// Resolved edits one call may prepare across all its sections.
pub const MAX_BATCH_EXPANDED_EDITS: usize = 200_000;
/// Prepared text/register bytes one call may retain across its sections.
pub const MAX_BATCH_BYTES: usize = 24 * 1024 * 1024;
/// Named registers retained by one edit-tool session.
pub const MAX_SESSION_REGISTER_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct Budget {
	edits: usize,
	bytes: usize,
}

impl Budget {
	pub fn charge_edits(&mut self, edits: usize) -> Result<(), String> {
		self.edits = self.edits.saturating_add(edits);
		if self.edits > MAX_BATCH_EXPANDED_EDITS {
			return Err(format!(
				"this call expands to more than {MAX_BATCH_EXPANDED_EDITS} edits across its sections; \
				 split it into smaller edit calls"
			));
		}
		Ok(())
	}

	pub fn charge_bytes(&mut self, bytes: usize) -> Result<(), String> {
		self.bytes = self.bytes.saturating_add(bytes);
		if self.bytes > MAX_BATCH_BYTES {
			return Err(format!(
				"this call would materialize more than {} MiB of file/register text across its \
				 sections; split it into smaller edit calls",
				MAX_BATCH_BYTES / (1024 * 1024)
			));
		}
		Ok(())
	}
}
