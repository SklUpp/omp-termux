//! Skill discovery from `<dir>/skills/*/SKILL.md`.
//!
//! Roots (`discovery/helpers.ts:376-433`, `discovery/builtin.ts:280-300`):
//! every ancestor of `cwd` up to `$HOME` contributing `<ancestor>/.omp/skills`,
//! then `~/.omp/agent/skills`. Closest definition wins on name collisions; the
//! whole body is handed to the model as context — no executable code.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::{
	Error, Result, dirs,
	yaml::{self, Value},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
	pub name: String,
	pub description: String,
	pub path: PathBuf,
	pub body: String,
}

pub fn load(cwd: &Path, no_skills: bool, patterns: &[String]) -> Result<Vec<Skill>> {
	if no_skills {
		return Ok(Vec::new());
	}
	let matcher = build_matcher(patterns)?;
	let mut found: BTreeMap<String, Skill> = BTreeMap::new();
	for dir in roots(cwd) {
		for skill in scan_dir(&dir)? {
			found.entry(skill.name.clone()).or_insert(skill);
		}
	}
	let mut skills: Vec<Skill> = found
		.into_values()
		.filter(|skill| matcher.as_ref().is_none_or(|set| matches(set, skill)))
		.collect();
	skills.sort_by(|left, right| {
		left
			.name
			.cmp(&right.name)
			.then_with(|| left.path.cmp(&right.path))
	});
	Ok(skills)
}

fn roots(cwd: &Path) -> Vec<PathBuf> {
	let mut roots = Vec::new();
	let home = dirs::home_dir();
	let mut current = Some(cwd.to_path_buf());
	while let Some(dir) = current {
		roots.push(dirs::project_config_dir(&dir).join("skills"));
		if home.as_ref().is_some_and(|home| home == &dir) {
			break;
		}
		current = dir.parent().map(Path::to_path_buf);
	}
	roots.push(dirs::agent_dir().join("skills"));
	roots
}

fn scan_dir(dir: &Path) -> Result<Vec<Skill>> {
	let entries = match fs::read_dir(dir) {
		Ok(entries) => entries,
		Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
		Err(err) => {
			return Err(Error::with_source(format!("cannot read skills dir {}", dir.display()), err));
		},
	};
	let mut skills = Vec::new();
	for entry in entries {
		let entry = entry?;
		let name = entry.file_name();
		if name.to_string_lossy().starts_with('.') {
			continue;
		}
		let path = entry.path();
		if !path.is_dir() {
			continue;
		}
		let skill_path = path.join("SKILL.md");
		if !skill_path.is_file() {
			continue;
		}
		let content = fs::read_to_string(&skill_path)
			.map_err(|err| Error::with_source(format!("cannot read {}", skill_path.display()), err))?;
		if let Some(skill) = parse_skill(&skill_path, &content) {
			skills.push(skill);
		}
	}
	Ok(skills)
}

fn parse_skill(path: &Path, content: &str) -> Option<Skill> {
	let (frontmatter, body) = split_frontmatter(content)?;
	let meta = if frontmatter.trim().is_empty() {
		Value::Null
	} else {
		yaml::parse(frontmatter).ok()?
	};
	if meta.get("enabled").and_then(Value::as_bool) == Some(false) {
		return None;
	}
	let directory_name = path.parent()?.file_name()?.to_string_lossy().into_owned();
	let name = meta
		.get("name")
		.and_then(Value::to_text)
		.map(|name| name.trim().to_string())
		.filter(|name| !name.is_empty())
		.unwrap_or(directory_name);
	let description = meta
		.get("description")
		.and_then(Value::to_text)
		.unwrap_or_default();
	// Mirrors `requireDescription: true` in the built-in provider: an
	// undescribed skill is invisible to the model anyway.
	if description.trim().is_empty() {
		return None;
	}
	Some(Skill { name, description, path: path.to_path_buf(), body: body.to_string() })
}

/// Split a `---` frontmatter block from the markdown body.
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
	let rest = content
		.strip_prefix("---\n")
		.or_else(|| content.strip_prefix("---\r\n"))?;
	let mut offset = 0;
	for line in rest.split_inclusive('\n') {
		if line.trim_end() == "---" {
			return Some((&rest[..offset], &rest[offset + line.len()..]));
		}
		offset += line.len();
	}
	None
}

fn build_matcher(patterns: &[String]) -> Result<Option<GlobSet>> {
	let patterns: Vec<&String> = patterns
		.iter()
		.filter(|pattern| !pattern.trim().is_empty())
		.collect();
	if patterns.is_empty() {
		return Ok(None);
	}
	let mut builder = GlobSetBuilder::new();
	for pattern in patterns {
		let glob = Glob::new(pattern.trim())
			.map_err(|err| Error::new(format!("invalid --skills pattern `{pattern}`: {err}")))?;
		builder.add(glob);
	}
	builder
		.build()
		.map(Some)
		.map_err(|err| Error::new(format!("invalid --skills patterns: {err}")))
}

fn matches(set: &GlobSet, skill: &Skill) -> bool {
	set.is_match(&skill.name) || set.is_match(&skill.path)
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
			"omp-cli-skills-{}-{}",
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

	struct EnvGuard {
		agent: Option<std::ffi::OsString>,
		config: Option<std::ffi::OsString>,
	}

	impl EnvGuard {
		fn isolate(root: &Path) -> Self {
			let guard = Self {
				agent: env::var_os("PI_CODING_AGENT_DIR"),
				config: env::var_os("PI_CONFIG_DIR"),
			};
			// SAFETY: callers hold the crate-wide env lock.
			unsafe {
				env::set_var("PI_CONFIG_DIR", root);
				env::set_var("PI_CODING_AGENT_DIR", root.join("agent"));
			}
			guard
		}
	}

	impl Drop for EnvGuard {
		fn drop(&mut self) {
			// SAFETY: restoring what this guard replaced.
			unsafe {
				match &self.agent {
					Some(value) => env::set_var("PI_CODING_AGENT_DIR", value),
					None => env::remove_var("PI_CODING_AGENT_DIR"),
				}
				match &self.config {
					Some(value) => env::set_var("PI_CONFIG_DIR", value),
					None => env::remove_var("PI_CONFIG_DIR"),
				}
			}
		}
	}

	fn skill(name: &str, description: &str, body: &str) -> String {
		format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n")
	}

	#[test]
	fn discovers_project_and_user_skills_with_filters() {
		let _env = crate::TEST_ENV_LOCK.lock().expect("env lock");
		let dir = temp_dir();
		let _guard = EnvGuard::isolate(&dir);
		write(
			&dir.join(".omp/skills/review/SKILL.md"),
			&skill("review", "Code review", "# Review\nbody"),
		);
		write(&dir.join("agent/skills/test/SKILL.md"), &skill("test", "Run tests", "# Test"));
		write(
			&dir.join("agent/skills/undescribed/SKILL.md"),
			"---\nname: undescribed\n---\nno description\n",
		);
		write(
			&dir.join("agent/skills/disabled/SKILL.md"),
			"---\ndescription: off\nenabled: false\n---\nx\n",
		);

		let all = load(&dir, false, &[]).expect("skills");
		let names: Vec<&str> = all.iter().map(|skill| skill.name.as_str()).collect();
		assert_eq!(names, ["review", "test"]);
		assert!(all[0].body.contains("body"));

		let filtered = load(&dir, false, &["rev*".to_string()]).expect("filtered");
		assert_eq!(filtered.len(), 1);
		assert_eq!(filtered[0].name, "review");

		assert!(load(&dir, true, &[]).expect("disabled").is_empty());
		fs::remove_dir_all(dir).ok();
	}
}
