//! Filesystem locations shared with the TypeScript runtime.
//!
//! Anchors: `packages/utils/src/dirs.ts` (`CONFIG_DIR_NAME = ".omp"`,
//! `MAIN_CONFIG_FILENAMES = ["config.yml", "config.yaml"]`, `getAgentDir()`),
//! so a Rust binary and the JS CLI read the same files.

use std::env;
use std::path::{Path, PathBuf};

/// Config directory name (mirrors `CONFIG_DIR_NAME`).
pub const CONFIG_DIR_NAME: &str = ".omp";

/// Canonical config filename first, legacy YAML fallback second.
pub const MAIN_CONFIG_FILENAMES: [&str; 2] = ["config.yml", "config.yaml"];

fn env_path(key: &str) -> Option<PathBuf> {
	env::var_os(key)
		.map(PathBuf::from)
		.filter(|path| !path.as_os_str().is_empty())
}

#[must_use]
pub fn home_dir() -> Option<PathBuf> {
	env_path("HOME").or_else(|| env_path("USERPROFILE"))
}

/// `$PI_CONFIG_DIR` else `~/.omp` (`dirs.ts:24,26-27`).
#[must_use]
pub fn config_root() -> PathBuf {
	env_path("PI_CONFIG_DIR").unwrap_or_else(|| {
		home_dir().map_or_else(|| PathBuf::from(CONFIG_DIR_NAME), |home| home.join(CONFIG_DIR_NAME))
	})
}

/// `$PI_CODING_AGENT_DIR` else `<config root>/agent` (`dirs.ts:567-570`).
#[must_use]
pub fn agent_dir() -> PathBuf {
	env_path("PI_CODING_AGENT_DIR").unwrap_or_else(|| config_root().join("agent"))
}

/// Project-local config directory (`<cwd>/.omp`).
#[must_use]
pub fn project_config_dir(cwd: &Path) -> PathBuf {
	cwd.join(CONFIG_DIR_NAME)
}

/// Shared install data directories, most specific first: `$OMP_SHARE_DIR`,
/// `$PREFIX/share/omp` (Termux/nix prefixes), `<exe>/../share/omp`,
/// `~/.local/share/omp`, `/usr/local/share/omp`, `/usr/share/omp`.
#[must_use]
pub fn share_dirs() -> Vec<PathBuf> {
	let mut dirs: Vec<PathBuf> = Vec::new();
	let mut push = |path: PathBuf| {
		if !dirs.contains(&path) {
			dirs.push(path);
		}
	};
	if let Some(dir) = env_path("OMP_SHARE_DIR") {
		push(dir);
	}
	if let Some(prefix) = env_path("PREFIX") {
		push(prefix.join("share/omp"));
	}
	if let Ok(exe) = env::current_exe()
		&& let Some(bin) = exe.parent()
		&& let Some(prefix) = bin.parent()
	{
		push(prefix.join("share/omp"));
	}
	if let Some(home) = home_dir() {
		push(home.join(".local/share/omp"));
	}
	push(PathBuf::from("/usr/local/share/omp"));
	push(PathBuf::from("/usr/share/omp"));
	dirs
}

/// Make `path` absolute against `base` without requiring it to exist.
#[must_use]
pub fn absolute(base: &Path, path: &Path) -> PathBuf {
	if path.is_absolute() {
		path.to_path_buf()
	} else {
		base.join(path)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn filenames_match_typescript() {
		assert_eq!(CONFIG_DIR_NAME, ".omp");
		assert_eq!(MAIN_CONFIG_FILENAMES, ["config.yml", "config.yaml"]);
	}

	#[test]
	fn relative_paths_resolve_against_base() {
		assert_eq!(absolute(Path::new("/work"), Path::new("sub/x")), PathBuf::from("/work/sub/x"));
		assert_eq!(absolute(Path::new("/work"), Path::new("/abs")), PathBuf::from("/abs"));
	}
}
