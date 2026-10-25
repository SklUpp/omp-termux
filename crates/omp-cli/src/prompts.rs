//! Prompt resolution from files.
//!
//! Order (CLI input first, most specific file next):
//! `--system-prompt` > `<cwd>/.omp/SYSTEM.md` > `~/.omp/agent/SYSTEM.md` >
//! `share/omp/system-prompt.md`. `APPEND_SYSTEM.md` is additive on the same
//! roots; `PERSONALITY.md`, `RULES.md` and `AGENTS.md` contribute context
//! files. A CLI value is a file path when that file exists (unless it contains
//! a newline, which makes it a literal prompt) — `resolvePromptInput`
//! semantics from `packages/coding-agent/src/system-prompt.ts`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::{Result, dirs};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
	pub path: PathBuf,
	pub content: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prompts {
	/// Fully resolved system prompt, if any source provided one.
	pub system: Option<String>,
	/// Additive append-system prompt.
	pub append: Option<String>,
	/// `PERSONALITY.md` / `RULES.md` / `AGENTS.md` contents, project first.
	pub files: Vec<ContextFile>,
}

/// CLI prompt input: a literal when it contains a newline, otherwise file
/// contents when readable, otherwise the literal string.
#[must_use]
pub fn resolve_input(input: &str) -> String {
	if input.contains('\n') {
		return input.to_string();
	}
	fs::read_to_string(input).unwrap_or_else(|_| input.to_string())
}

pub fn resolve(cli_system: Option<&str>, cli_append: Option<&str>, cwd: &Path) -> Result<Prompts> {
	let project = dirs::project_config_dir(cwd);
	let agent = dirs::agent_dir();

	let system = match cli_system {
		Some(input) => Some(resolve_input(input)),
		None => {
			let mut candidates = vec![project.join("SYSTEM.md"), agent.join("SYSTEM.md")];
			candidates.extend(
				dirs::share_dirs()
					.into_iter()
					.map(|dir| dir.join("system-prompt.md")),
			);
			first_file(candidates)
		},
	};

	let append = match cli_append {
		Some(input) => Some(resolve_input(input)),
		None => first_file([project.join("APPEND_SYSTEM.md"), agent.join("APPEND_SYSTEM.md")]),
	};

	let mut files = Vec::new();
	for name in ["PERSONALITY.md", "RULES.md", "AGENTS.md"] {
		for path in [project.join(name), agent.join(name)] {
			if let Ok(content) = fs::read_to_string(&path)
				&& !content.trim().is_empty()
			{
				files.push(ContextFile { path, content });
			}
		}
	}
	Ok(Prompts { system, append, files })
}

/// First readable non-empty file among `candidates`.
fn first_file(candidates: impl IntoIterator<Item = PathBuf>) -> Option<String> {
	for path in candidates {
		if let Ok(content) = fs::read_to_string(&path)
			&& !content.trim().is_empty()
		{
			return Some(content);
		}
	}
	None
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::env;
	use std::io::Write as _;
	use std::sync::atomic::{AtomicU32, Ordering};

	static COUNTER: AtomicU32 = AtomicU32::new(0);

	fn temp_dir() -> PathBuf {
		let unique = format!(
			"omp-cli-prompts-{}-{}",
			std::process::id(),
			COUNTER.fetch_add(1, Ordering::Relaxed)
		);
		let dir = env::temp_dir().join(unique);
		fs::create_dir_all(&dir).expect("temp dir");
		dir
	}

	fn write(path: &Path, text: &str) {
		fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
		let mut file = fs::File::create(path).expect("create");
		file.write_all(text.as_bytes()).expect("write");
	}

	#[test]
	fn cli_input_with_newline_is_literal() {
		assert_eq!(resolve_input("line one\nline two"), "line one\nline two");
	}

	#[test]
	fn cli_input_reads_existing_file_else_literal() {
		let dir = temp_dir();
		let path = dir.join("prompt.md");
		write(&path, "from file");
		assert_eq!(resolve_input(path.to_str().expect("utf8")), "from file");
		assert_eq!(resolve_input("not a file"), "not a file");
		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn project_files_win_over_agent_files_and_append_is_additive() {
		let _env = crate::TEST_ENV_LOCK.lock().expect("env lock");
		let dir = temp_dir();
		let project = dir.join(".omp");
		let agent = dir.join("agent");
		// Isolate the real user config: the agent dir is derived from env.
		// SAFETY: single-threaded unit test; restored below.
		let previous_agent = env::var_os("PI_CODING_AGENT_DIR");
		let previous_config = env::var_os("PI_CONFIG_DIR");
		unsafe {
			env::set_var("PI_CONFIG_DIR", &dir);
			env::set_var("PI_CODING_AGENT_DIR", &agent);
		}
		write(&project.join("SYSTEM.md"), "project system");
		write(&agent.join("SYSTEM.md"), "agent system");
		write(&agent.join("APPEND_SYSTEM.md"), "append tail");
		write(&project.join("AGENTS.md"), "project agents");
		write(&agent.join("AGENTS.md"), "agent agents");

		let prompts = resolve(None, None, &dir).expect("resolve");
		assert_eq!(prompts.system.as_deref(), Some("project system"));
		assert_eq!(prompts.append.as_deref(), Some("append tail"));
		let contents: Vec<&str> = prompts
			.files
			.iter()
			.map(|file| file.content.as_str())
			.collect();
		assert_eq!(contents, ["project agents", "agent agents"]);

		fs::remove_dir_all(&dir).ok();
		unsafe {
			match previous_agent {
				Some(value) => env::set_var("PI_CODING_AGENT_DIR", value),
				None => env::remove_var("PI_CODING_AGENT_DIR"),
			}
			match previous_config {
				Some(value) => env::set_var("PI_CONFIG_DIR", value),
				None => env::remove_var("PI_CONFIG_DIR"),
			}
		}
	}
}
