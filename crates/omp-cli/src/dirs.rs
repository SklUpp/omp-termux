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

/// `$PI_CONFIG_DIR` else `~/.omp`, before profile relocation.
#[must_use]
pub fn base_config_root() -> PathBuf {
	env_path("PI_CONFIG_DIR").unwrap_or_else(|| {
		home_dir().map_or_else(|| PathBuf::from(CONFIG_DIR_NAME), |home| home.join(CONFIG_DIR_NAME))
	})
}

/// Config root for a named profile: `<base>/profiles/<name>` (`dirs.ts`).
#[must_use]
pub fn profile_config_root(base: &Path, profile: &str) -> PathBuf {
	base.join("profiles").join(profile)
}

/// Agent directory for a named profile: `<base>/profiles/<name>/agent`.
#[must_use]
pub fn profile_agent_dir(profile: &str) -> PathBuf {
	profile_agent_dir_in(&base_config_root(), profile)
}

fn profile_agent_dir_in(base: &Path, profile: &str) -> PathBuf {
	profile_config_root(base, profile).join("agent")
}

/// Active config root: the profile root when a profile is selected, else the
/// base root. Mirrors `getConfigRootDir()`.
#[must_use]
pub fn config_root() -> PathBuf {
	let base = base_config_root();
	match crate::profile::active_from_env() {
		Some(profile) => profile_config_root(&base, &profile),
		None => base,
	}
}

/// `$PI_CODING_AGENT_DIR` else `<config root>/agent` (`dirs.ts:567-570`). A
/// named profile always derives its own agent dir and ignores the explicit
/// override, matching `DirResolver`. In default mode a value that is merely a
/// profile-derived path inherited from a parent `setProfile` (it equals
/// `<base>/profiles/<PI_PROFILE>/agent`) is also ignored, so an explicit
/// `OMP_PROFILE=""` really selects the default agent dir.
#[must_use]
pub fn agent_dir() -> PathBuf {
	agent_dir_from(crate::profile::active_from_env().as_deref(), env_path("PI_CODING_AGENT_DIR").as_deref())
}

fn agent_dir_from(profile: Option<&str>, override_dir: Option<&Path>) -> PathBuf {
	let base = base_config_root();
	if let Some(profile) = profile {
		return profile_agent_dir_in(&base, profile);
	}
	match override_dir {
		Some(dir) if !is_profile_derived(dir, &base) => dir.to_path_buf(),
		_ => base.join("agent"),
	}
}

/// `resolvePreProfileAgentDir`: a `PI_CODING_AGENT_DIR` equal to the agent dir
/// of the (bypassed) `PI_PROFILE` is profile-derived, not a user override.
fn is_profile_derived(dir: &Path, base: &Path) -> bool {
	let Some(legacy) = env::var("PI_PROFILE").ok().and_then(|raw| crate::profile::normalize(Some(raw.as_str())).ok().flatten())
	else {
		return false;
	};
	profile_agent_dir_in(base, &legacy) == dir
}

/// XDG category for an agent subdirectory (`dirs.ts` `XdgCategory`). Only the
/// categories consumed by the headless runtime exist here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentDirCategory {
	Data,
	State,
}

impl AgentDirCategory {
	fn env_var(self) -> &'static str {
		match self {
			Self::Data => "XDG_DATA_HOME",
			Self::State => "XDG_STATE_HOME",
		}
	}
}

/// Agent subdirectory honoring the DirResolver XDG redirect. Mirrors
/// `agentSubdir(undefined, subdir, category)`: without an XDG redirect the path
/// is `<agent dir>/<subdir>`; with one it is flat under `$XDG_*_HOME/omp`.
#[must_use]
pub fn agent_subdir(subdir: &str, category: AgentDirCategory) -> PathBuf {
	agent_category_root(category).join(subdir)
}

fn agent_category_root(category: AgentDirCategory) -> PathBuf {
	xdg_agent_root(category).unwrap_or_else(agent_dir)
}

/// `DirResolver` XDG pin rules: Linux/macOS only, only when the agent dir is its
/// default (no explicit `PI_CODING_AGENT_DIR` override), and — for a named
/// profile — only when that category already contains `omp/profiles/<name>`.
/// The base `omp` root is never consulted for a named profile, so a profile
/// cannot silently move to XDG once the base app dir appears.
fn xdg_agent_root(category: AgentDirCategory) -> Option<PathBuf> {
	if !cfg!(any(target_os = "linux", target_os = "macos")) {
		return None;
	}
	let base = base_config_root();
	let profile = crate::profile::active_from_env();
	let profile_name = profile.as_deref();
	let default_agent = match profile_name {
		Some(name) => profile_agent_dir_in(&base, name),
		None => base.join("agent"),
	};
	let override_dir = env_path("PI_CODING_AGENT_DIR");
	if agent_dir_from(profile_name, override_dir.as_deref()) != default_agent {
		return None;
	}
	let app_root = env_path(category.env_var())?.join("omp");
	let candidate = match profile_name {
		Some(name) => app_root.join("profiles").join(name),
		None => app_root,
	};
	candidate.exists().then_some(candidate)
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

	#[test]
	fn profile_layout_nests_under_the_config_root() {
		let base = Path::new("/home/u/.omp");
		assert_eq!(profile_config_root(base, "work"), PathBuf::from("/home/u/.omp/profiles/work"));
		assert_eq!(
			profile_config_root(base, "work").join("agent"),
			PathBuf::from("/home/u/.omp/profiles/work/agent")
		);
	}
}
