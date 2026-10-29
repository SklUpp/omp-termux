//! Atomic per-file operations for the edit engines.
//!
//! Every commit re-checks the bytes it read before publishing, writes through
//! the shared temp-file+rename helper (symlink-safe, mode-preserving), and
//! leaves the destination untouched when anything fails. Moves are a
//! non-overwriting write of the destination followed by the source unlink;
//! delete and move are only reachable from an explicit mode operation.

use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::tools::{MAX_EDIT_BYTES, ToolError, read_capped, write_atomic_with_mode};

/// Read an existing UTF-8 file under the edit size cap.
pub async fn read_text(path: &Path, display: &str) -> Result<String, ToolError> {
	let meta = tokio::fs::metadata(path)
		.await
		.map_err(|err| map_read_error(err, display))?;
	if !meta.is_file() {
		return Err(ToolError::ExecutionFailed(format!("{display} is not a regular file")));
	}
	let (bytes, size) = read_capped(path, MAX_EDIT_BYTES)
		.await
		.map_err(|err| map_read_error(err, display))?;
	if size > bytes.len() as u64 {
		return Err(ToolError::ExecutionFailed(format!(
			"{display} is {size} bytes, the edit limit is {MAX_EDIT_BYTES}"
		)));
	}
	String::from_utf8(bytes)
		.map_err(|_| ToolError::ExecutionFailed(format!("{display} is not valid UTF-8")))
}

fn map_read_error(err: std::io::Error, display: &str) -> ToolError {
	if err.kind() == std::io::ErrorKind::NotFound {
		return ToolError::ExecutionFailed(format!("File not found: {display}"));
	}
	ToolError::ExecutionFailed(format!("Failed to read {display}: {err}"))
}

/// Publish `content` to `path` after verifying the file still holds `expected`.
///
/// `expected` is the exact bytes read when the edit was prepared, so a file
/// changed by another process is not silently overwritten.
pub async fn commit_text(
	path: &Path,
	display: &str,
	expected: &str,
	content: &str,
	token: &CancellationToken,
	mode: Option<u32>,
) -> Result<(), ToolError> {
	if token.is_cancelled() {
		return Err(ToolError::Aborted(format!("Edit to {display} cancelled")));
	}
	let current = read_text(path, display).await?;
	if current != expected {
		return Err(ToolError::ExecutionFailed(format!(
			"{display} changed after the edit was prepared; re-read it and re-issue the edit"
		)));
	}
	write_atomic_with_mode(path, content.as_bytes(), token, mode)
		.await
		.map_err(|err| map_write_error(err, display, token))
}

/// Create a file. `exclusive` refuses an existing destination.
pub async fn create_text(
	path: &Path,
	display: &str,
	content: &str,
	token: &CancellationToken,
	exclusive: bool,
) -> Result<(), ToolError> {
	if exclusive && tokio::fs::symlink_metadata(path).await.is_ok() {
		return Err(ToolError::ExecutionFailed(format!(
			"Cannot create {display}: file already exists. Use an update operation to modify it in place."
		)));
	}
	if let Some(parent) = path.parent()
		&& !parent.as_os_str().is_empty()
	{
		tokio::fs::create_dir_all(parent)
			.await
			.map_err(|err| ToolError::ExecutionFailed(format!("Failed to create {display}: {err}")))?;
	}
	write_atomic_with_mode(path, content.as_bytes(), token, None)
		.await
		.map_err(|err| map_write_error(err, display, token))
}

fn map_write_error(err: std::io::Error, display: &str, token: &CancellationToken) -> ToolError {
	if err.kind() == std::io::ErrorKind::Interrupted || token.is_cancelled() {
		return ToolError::Aborted(format!("Edit to {display} cancelled"));
	}
	ToolError::ExecutionFailed(format!("Failed to write {display}: {err}"))
}

/// Delete one regular file or symlink (never a directory).
pub async fn delete_file(path: &Path, display: &str, token: &CancellationToken) -> Result<(), ToolError> {
	if token.is_cancelled() {
		return Err(ToolError::Aborted(format!("Delete of {display} cancelled")));
	}
	let meta = tokio::fs::symlink_metadata(path)
		.await
		.map_err(|err| map_read_error(err, display))?;
	let file_type = meta.file_type();
	if !file_type.is_file() && !file_type.is_symlink() {
		return Err(ToolError::ExecutionFailed(format!("{display} is not a regular file")));
	}
	tokio::fs::remove_file(path)
		.await
		.map_err(|err| ToolError::ExecutionFailed(format!("Failed to delete {display}: {err}")))
}

/// Non-overwriting move: destination must not exist, mode is carried over.
pub async fn move_file(
	source: &Path,
	source_display: &str,
	dest: &Path,
	dest_display: &str,
	expected: &str,
	final_content: &str,
	token: &CancellationToken,
) -> Result<(), ToolError> {
	if source == dest {
		return Err(ToolError::ExecutionFailed("rename path is the same as source path".to_string()));
	}
	if tokio::fs::symlink_metadata(dest).await.is_ok() {
		return Err(ToolError::ExecutionFailed(format!(
			"Cannot rename {source_display} to {dest_display}: destination already exists."
		)));
	}
	let current = read_text(source, source_display).await?;
	if current != expected {
		return Err(ToolError::ExecutionFailed(format!(
			"{source_display} changed after the edit was prepared; re-read it and re-issue the edit"
		)));
	}
	let mode = file_mode(source).await;
	if let Some(parent) = dest.parent()
		&& !parent.as_os_str().is_empty()
	{
		tokio::fs::create_dir_all(parent)
			.await
			.map_err(|err| ToolError::ExecutionFailed(format!("Failed to create {dest_display}: {err}")))?;
	}
	write_atomic_with_mode(dest, final_content.as_bytes(), token, mode)
		.await
		.map_err(|err| map_write_error(err, dest_display, token))?;
	if let Err(err) = tokio::fs::remove_file(source).await {
		return Err(ToolError::ExecutionFailed(format!(
			"Wrote {dest_display} but failed to remove {source_display}: {err} (partial move)"
		)));
	}
	Ok(())
}

#[cfg(unix)]
pub async fn file_mode(path: &Path) -> Option<u32> {
	use std::os::unix::fs::PermissionsExt;
	tokio::fs::metadata(path).await.ok().map(|meta| meta.permissions().mode())
}

#[cfg(not(unix))]
pub async fn file_mode(_path: &Path) -> Option<u32> {
	None
}

/// Ensure the target's parent exists before a write.
pub async fn ensure_parent(path: &Path, display: &str) -> Result<(), ToolError> {
	if let Some(parent) = path.parent()
		&& !parent.as_os_str().is_empty()
	{
		tokio::fs::create_dir_all(parent)
			.await
			.map_err(|err| ToolError::ExecutionFailed(format!("Failed to create {display}: {err}")))?;
	}
	Ok(())
}
