//! Path resolution shared by the non-hashline edit engines.
//!
//! Mirrors upstream `resolveEditPath`: authored paths resolve against the
//! session cwd; when a path that must exist is missing, a unique workspace
//! suffix match is used. Internal URL schemes are refused with upstream's
//! protocol-handler error. Accepting them requires the parent host to supply
//! the `InternalUrlRouter` API from
//! `packages/coding-agent/src/internal-urls/router.ts`:
//! `can_handle(url)`, `resolve(url, ctx)` (`ResolveContext`: cwd, session file,
//! settings, abort signal, and `local://` per-session roots), `write(url,
//! content, ctx)` (`WriteContext`: cwd, signal, local roots, session `xd`
//! writer), and `get_handler(scheme).write` for the approval tier. The Rust
//! parent has no router or per-session context yet, so the engines still refuse
//! every internal scheme via [`ensure_no_internal_scheme`] and never invent a
//! local fallback.

use std::{
	collections::HashSet,
	path::{Component, Path, PathBuf},
};

use tokio_util::sync::CancellationToken;

use crate::tools::ToolError;

/// Internal schemes the TypeScript `edit` tool routes through
/// `InternalUrlRouter`/`resolvePlanPath`. This crate has no URL adapter.
const INTERNAL_SCHEMES: [&str; 9] = [
	"agent://",
	"artifact://",
	"skill://",
	"rule://",
	"security://",
	"local://",
	"mcp://",
	"ssh://",
	"vault://",
];

/// Directories skipped by the workspace suffix search.
const SKIP_DIRS: [&str; 8] = [".git", "node_modules", "target", ".venv", "venv", "dist", "build", ".cache"];
const SUFFIX_SEARCH_MAX_FILES: usize = 5000;

/// Scheme of a path when it is one of the internal URLs a host must route.
pub fn internal_url_scheme(path: &str) -> Option<&'static str> {
	let normalized = normalize_local_scheme(path.trim());
	INTERNAL_SCHEMES
		.iter()
		.copied()
		.find(|prefix| normalized.starts_with(prefix))
}

/// Refuse an internal URL before any filesystem call.
pub fn ensure_no_internal_scheme(path: &str) -> Result<(), ToolError> {
	if let Some(prefix) = internal_url_scheme(path) {
		return Err(ToolError::ExecutionFailed(format!(
			"Path \"{path}\" uses internal scheme \"{prefix}\" and must be resolved through the proper protocol handler, not as a filesystem path."
		)));
	}
	Ok(())
}

/// `local:/x` collapses to `local://x` under Node's path handling; match it so
/// the scheme check cannot be bypassed by one slash.
fn normalize_local_scheme(path: &str) -> String {
	if let Some(rest) = path.strip_prefix("local:/")
		&& !rest.starts_with('/')
	{
		return format!("local://{rest}");
	}
	path.to_string()
}

/// Expand a leading `~` to `$HOME` and strip wrapping quotes.
pub fn expand_path(raw: &str) -> String {
	let trimmed = raw.trim();
	let unquoted = if trimmed.len() > 1
		&& ((trimmed.starts_with('"') && trimmed.ends_with('"'))
			|| (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
	{
		&trimmed[1..trimmed.len() - 1]
	} else {
		trimmed
	};
	if unquoted == "~" {
		return home_dir().to_string_lossy().into_owned();
	}
	if let Some(rest) = unquoted.strip_prefix("~/") {
		return home_dir().join(rest).to_string_lossy().into_owned();
	}
	unquoted.to_string()
}

fn home_dir() -> PathBuf {
	std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// Resolve `path` against `cwd` and normalize `.`/`..` lexically.
pub fn resolve_to_cwd(path: &str, cwd: &Path) -> PathBuf {
	let expanded = expand_path(path);
	let candidate = Path::new(&expanded);
	let joined = if candidate.is_absolute() { candidate.to_path_buf() } else { cwd.join(candidate) };
	lexical_normalize(&joined)
}

/// Collapse `.`/`..` without touching the filesystem (a symlinked parent must
/// not change the authored target).
pub fn lexical_normalize(path: &Path) -> PathBuf {
	let mut out = PathBuf::new();
	for component in path.components() {
		match component {
			Component::CurDir => {},
			Component::ParentDir => {
				if !out.pop() {
					out.push(component.as_os_str());
				}
			},
			other => out.push(other.as_os_str()),
		}
	}
	out
}

/// Resolve the authored path for an edit call.
///
/// `must_exist` mirrors upstream: create paths are not suffix-searched.
/// Returns `(absolute, display)` where `display` keeps the authored spelling
/// when it resolves, and the discovered relative path otherwise.
pub async fn resolve_edit_path(
	authored: &str,
	cwd: &Path,
	must_exist: bool,
	token: &CancellationToken,
) -> Result<(PathBuf, String), ToolError> {
	ensure_no_internal_scheme(authored)?;
	if authored.trim().is_empty() {
		return Err(ToolError::InvalidArguments("path must not be empty".to_string()));
	}
	let absolute = resolve_to_cwd(authored, cwd);
	if !must_exist || tokio::fs::metadata(&absolute).await.is_ok() {
		return Ok((absolute, authored.trim().to_string()));
	}
	if token.is_cancelled() {
		return Err(ToolError::Aborted("edit cancelled".to_string()));
	}
	if let Some(relative) = unique_workspace_suffix(authored, cwd, token).await? {
		let absolute = resolve_to_cwd(&relative, cwd);
		return Ok((absolute, relative));
	}
	Ok((absolute, authored.trim().to_string()))
}

/// Find a single workspace file whose path ends with the authored suffix.
async fn unique_workspace_suffix(
	authored: &str,
	cwd: &Path,
	token: &CancellationToken,
) -> Result<Option<String>, ToolError> {
	let suffix = expand_path(authored).replace('\\', "/");
	let suffix = suffix.trim_start_matches("./").to_string();
	if suffix.is_empty() || Path::new(&suffix).is_absolute() {
		return Ok(None);
	}
	let root = cwd.to_path_buf();
	let search_token = token.clone();
	let found = tokio::task::spawn_blocking(move || walk_suffix(&root, &suffix, &search_token))
		.await
		.map_err(|err| ToolError::ExecutionFailed(format!("workspace suffix search failed: {err}")))?;
	Ok(found)
}

fn walk_suffix(root: &Path, suffix: &str, token: &CancellationToken) -> Option<String> {
	let mut stack = vec![root.to_path_buf()];
	let mut seen = HashSet::new();
	let mut files = 0usize;
	let mut found: Option<String> = None;
	while let Some(dir) = stack.pop() {
		if token.is_cancelled() || files >= SUFFIX_SEARCH_MAX_FILES {
			break;
		}
		let Ok(entries) = std::fs::read_dir(&dir) else { continue };
		for entry in entries.flatten() {
			if token.is_cancelled() {
				break;
			}
			let path = entry.path();
			let Ok(file_type) = entry.file_type() else { continue };
			if file_type.is_dir() {
				let name = entry.file_name();
				let name = name.to_string_lossy().to_string();
				if SKIP_DIRS.contains(&name.as_str()) || name.starts_with('.') {
					continue;
				}
				stack.push(path);
				continue;
			}
			if !file_type.is_file() {
				continue;
			}
			files += 1;
			let Ok(relative) = path.strip_prefix(root) else { continue };
			let relative = relative.to_string_lossy().replace('\\', "/");
			if relative.ends_with(suffix) {
				if !seen.insert(relative.clone()) || found.is_some() {
					// A second match makes the suffix ambiguous.
					return None;
				}
				found = Some(relative);
			}
		}
	}
	found
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn internal_schemes_are_refused() {
		assert!(ensure_no_internal_scheme("artifact://out").is_err());
		assert!(ensure_no_internal_scheme("local:/tmp/x").is_err());
		assert!(ensure_no_internal_scheme("ssh://host/file").is_err());
		assert!(ensure_no_internal_scheme("src/main.rs").is_ok());
	}

	#[test]
	fn lexical_resolution() {
		let cwd = Path::new("/work/tree");
		assert_eq!(resolve_to_cwd("./a/../b.rs", cwd), PathBuf::from("/work/tree/b.rs"));
		assert_eq!(resolve_to_cwd("/abs/x.rs", cwd), PathBuf::from("/abs/x.rs"));
	}
}
