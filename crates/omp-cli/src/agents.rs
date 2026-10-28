//! Agent discovery, frontmatter parsing, and `omp agents unpack`.
//!
//! Sources use the OMP-native directories only: project `.omp/agents`, user
//! `<agent-dir>/agents`, configured extension roots (`<root>/agents`), then the
//! seven bundled prompts embedded from the repository prompt assets. Precedence
//! is first-wins by agent name. There is no plugin marketplace loader in this
//! native runtime, so no plugin agents are discovered.

use std::{
	collections::BTreeSet,
	fs,
	path::{Path, PathBuf},
};

use pi_agent::tools::task_catalog::{
	AgentDefinition, AgentSource, HandoffTarget, Spawns,
};
use serde::Serialize;

use crate::{Error, Result, dirs, yaml};

const SCOUT_MD: &str = include_str!("../../../packages/coding-agent/src/prompts/agents/scout.md");
const DESIGNER_MD: &str =
	include_str!("../../../packages/coding-agent/src/prompts/agents/designer.md");
const REVIEWER_MD: &str =
	include_str!("../../../packages/coding-agent/src/prompts/agents/reviewer.md");
const SECURITY_REVIEWER_MD: &str =
	include_str!("../../../packages/coding-agent/src/prompts/agents/security-reviewer.md");
const LIBRARIAN_MD: &str =
	include_str!("../../../packages/coding-agent/src/prompts/agents/librarian.md");
const TASK_BODY: &str = include_str!("../../../packages/coding-agent/src/prompts/agents/task.md");

/// Discovery result for a runner session.
pub struct AgentDiscovery {
	pub agents: Vec<AgentDefinition>,
	pub project_agents_dir: Option<String>,
}

/// Load the shipped bundled agents from the actual prompt assets.
pub fn bundled_agents() -> Result<Vec<AgentDefinition>> {
	let task = format!(
		"---\nname: \"task\"\ndescription: \"General-purpose subagent with full capabilities for \
		 delegated multi-step tasks\"\nspawns: \"*\"\nmodel: \"@task\"\nthinking-level: \
		 \"auto\"\n---\n{TASK_BODY}"
	);
	let sonic = format!(
		"---\nname: \"sonic\"\ndescription: \"Low-reasoning agent for strictly mechanical updates \
		 or data collection only\"\nmodel: \"@smol\"\nthinking-level: \"medium\"\n---\n{TASK_BODY}"
	);
	Ok(vec![
		parse_agent("embedded:scout.md", SCOUT_MD, AgentSource::Bundled)?,
		parse_agent("embedded:designer.md", DESIGNER_MD, AgentSource::Bundled)?,
		parse_agent("embedded:reviewer.md", REVIEWER_MD, AgentSource::Bundled)?,
		parse_agent(
			"embedded:security-reviewer.md",
			SECURITY_REVIEWER_MD,
			AgentSource::Bundled,
		)?,
		parse_agent(
			"embedded:librarian.md",
			LIBRARIAN_MD,
			AgentSource::Bundled,
		)?,
		parse_agent("embedded:task.md", &task, AgentSource::Bundled)?,
		parse_agent("embedded:sonic.md", &sonic, AgentSource::Bundled)?,
	])
}

/// Discover project, user, extension, then bundled agents. Later sources never
/// override an earlier definition with the same name.
pub fn discover_agents(
	cwd: &Path,
	extension_roots: &[PathBuf],
) -> Result<AgentDiscovery> {
	let mut agents = Vec::new();
	let mut seen = BTreeSet::new();
	let mut project_agents_dir = None;

	if let Some(dir) = nearest_project_agents_dir(cwd) {
		project_agents_dir = Some(dir.display().to_string());
		append_dir(&dir, AgentSource::Project, &mut agents, &mut seen)?;
	}

	let user_dir = dirs::agent_dir().join("agents");
	if user_dir.is_dir() {
		append_dir(&user_dir, AgentSource::User, &mut agents, &mut seen)?;
	}

	for root in extension_roots {
		let dir = dirs::absolute(cwd, root).join("agents");
		if dir.is_dir() {
			append_dir(&dir, AgentSource::Extension, &mut agents, &mut seen)?;
		}
	}

	for agent in bundled_agents()? {
		if seen.insert(agent.name.clone()) {
			agents.push(agent);
		}
	}

	Ok(AgentDiscovery { agents, project_agents_dir })
}

/// Walk up from `cwd` and return the nearest `.omp/agents` directory.
fn nearest_project_agents_dir(cwd: &Path) -> Option<PathBuf> {
	let mut dir = Some(cwd);
	while let Some(current) = dir {
		let candidate = dirs::project_config_dir(current).join("agents");
		if candidate.is_dir() {
			return Some(candidate);
		}
		dir = current.parent();
	}
	None
}

fn append_dir(
	dir: &Path,
	source: AgentSource,
	agents: &mut Vec<AgentDefinition>,
	seen: &mut BTreeSet<String>,
) -> Result<()> {
	let mut paths = Vec::new();
	for entry in fs::read_dir(dir)
		.map_err(|error| Error::with_source(format!("cannot read {}", dir.display()), error))?
	{
		let entry = entry
			.map_err(|error| Error::with_source(format!("cannot read {}", dir.display()), error))?;
		let path = entry.path();
		if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
			continue;
		}
		paths.push(path);
	}
	paths.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
	for path in paths {
		let content = match fs::read_to_string(&path) {
			Ok(content) => content,
			Err(error) => {
				eprintln!("warning: cannot read agent {}: {error}", path.display());
				continue;
			},
		};
		match parse_agent(&path.display().to_string(), &content, source) {
			Ok(agent) => {
				if seen.insert(agent.name.clone()) {
					agents.push(agent);
				}
			},
			Err(error) => eprintln!("warning: skipped agent {}: {error}", path.display()),
		}
	}
	Ok(())
}

/// Parse agent markdown: YAML frontmatter, kebab-case key normalization, and
/// the same defaults as the TypeScript loader (`yield` is mandatory when tools
/// are restricted; `spawns` is inferred from a requested `task` tool).
pub fn parse_agent(
	file_path: &str,
	content: &str,
	source: AgentSource,
) -> Result<AgentDefinition> {
	let (meta, body) = split_frontmatter(content)?;
	let meta = camelize_keys(&meta);
	let name = required_text(&meta, "name", file_path)?;
	let description = required_text(&meta, "description", file_path)?;
	let mut tools = parse_list(meta.get("tools"));
	if let Some(tools) = &mut tools {
		for tool in tools.iter_mut() {
			*tool = normalize_tool_name(tool);
		}
		// TypeScript keeps an explicit empty list as "no tools"; only a
		// non-empty allowlist gains the mandatory `yield` return channel.
		if !tools.is_empty() && !tools.iter().any(|tool| tool == "yield") {
			tools.push("yield".to_string());
		}
	}
	let spawns = parse_spawns(meta.get("spawns")).or_else(|| {
		tools
			.as_ref()
			.filter(|tools| tools.iter().any(|tool| tool == "task"))
			.map(|_| Spawns::All)
	});
	let model = parse_list(meta.get("model")).unwrap_or_default();
	let output = meta.get("output").map(yaml_to_json);
	let thinking_level = meta
		.get("thinkingLevel")
		.and_then(yaml::Value::to_text)
		.or_else(|| meta.get("thinking").and_then(yaml::Value::to_text));
	let blocking = parse_bool(meta.get("blocking")).unwrap_or(false);
	let read_summarize = parse_bool(meta.get("readSummarize"));
	let prewalk = parse_handoff(meta.get("prewalk"));
	let advisor = parse_handoff(meta.get("advisor"));
	let autoload_skills = parse_list(meta.get("autoloadSkills")).unwrap_or_default();
	Ok(AgentDefinition {
		name,
		description,
		tools,
		spawns,
		model,
		output,
		thinking_level,
		blocking,
		read_summarize,
		prewalk,
		advisor,
		autoload_skills,
		system_prompt: body.trim().to_string(),
		source,
		file_path: file_path.to_string(),
	})
}

fn split_frontmatter(content: &str) -> Result<(yaml::Value, &str)> {
	let content = content.strip_prefix('\u{feff}').unwrap_or(content);
	let Some(first_line_end) = content.find('\n') else {
		return Err(Error::new("agent markdown has no frontmatter"));
	};
	let first_line = content[..first_line_end].trim_end_matches('\r');
	if first_line != "---" {
		return Err(Error::new("agent markdown must start with a '---' frontmatter block"));
	}
	let mut offset = first_line_end + 1;
	let mut yaml_end = None;
	let mut body_start = None;
	for line in content[offset..].split_inclusive('\n') {
		let trimmed = line.trim_end_matches(['\r', '\n']).trim();
		if trimmed == "---" {
			yaml_end = Some(offset);
			body_start = Some(offset + line.len());
			break;
		}
		offset += line.len();
	}
	let (Some(yaml_end), Some(body_start)) = (yaml_end, body_start) else {
		return Err(Error::new("agent frontmatter is not closed with '---'"));
	};
	let source = &content[first_line_end + 1..yaml_end];
	let meta = match yaml::parse(source) {
		Ok(value) => value,
		Err(first) => {
			let repaired = repair_ambiguous_scalars(source);
			if repaired == source {
				return Err(Error::new(format!("invalid agent frontmatter: {first}")));
			}
			yaml::parse(&repaired)
				.map_err(|error| Error::new(format!("invalid agent frontmatter: {error}")))?
		},
	};
	Ok((meta, &content[body_start..]))
}

/// Wrap plain scalars containing `: ` in quotes, matching the TypeScript
/// frontmatter repair path for descriptions such as `Use when: foo`.
fn repair_ambiguous_scalars(source: &str) -> String {
	let mut output = String::with_capacity(source.len());
	for line in source.lines() {
		let trimmed = line.trim_start();
		if let Some((key, value)) = trimmed.split_once(": ") {
			let plain = !value.starts_with(['"', '\'', '[', '{', '|', '>']);
			if plain && (value.contains(": ") || value.contains(" #")) {
				let indent = &line[..line.len() - trimmed.len()];
				output.push_str(indent);
				output.push_str(key);
				output.push_str(": \"");
				output.push_str(&value.replace('\\', "\\\\").replace('"', "\\\""));
				output.push_str("\"\n");
				continue;
			}
		}
		output.push_str(line);
		output.push('\n');
	}
	output
}

/// Normalize only the frontmatter's top-level keys (`thinking-level` ->
/// `thinkingLevel`). Nested values are payloads: output-schema property names
/// such as `line_start` must reach the validator and the child prompt verbatim.
fn camelize_keys(value: &yaml::Value) -> yaml::Value {
	match value {
		yaml::Value::Map(pairs) => yaml::Value::Map(
			pairs
				.iter()
				.map(|(key, value)| (camelize_key(key), value.clone()))
				.collect(),
		),
		other => other.clone(),
	}
}

fn camelize_key(key: &str) -> String {
	let mut chars = key.chars();
	let Some(first) = chars.next() else {
		return String::new();
	};
	let mut output = String::from(first);
	let mut upper = false;
	for ch in chars {
		if ch == '-' || ch == '_' {
			upper = true;
		} else if upper {
			output.extend(ch.to_uppercase());
			upper = false;
		} else {
			output.push(ch);
		}
	}
	output
}

fn required_text(meta: &yaml::Value, key: &str, file_path: &str) -> Result<String> {
	meta
		.get(key)
		.and_then(yaml::Value::to_text)
		.map(|text| text.trim().to_string())
		.filter(|text| !text.is_empty())
		.ok_or_else(|| {
			Error::new(format!(
				"{file_path}: agent frontmatter requires a non-empty '{key}'"
			))
		})
}

fn parse_list(value: Option<&yaml::Value>) -> Option<Vec<String>> {
	let value = value?;
	match value {
		yaml::Value::Null => None,
		yaml::Value::List(items) => Some(
			items
				.iter()
				.filter_map(yaml::Value::to_text)
				.map(|item| item.trim().to_string())
				.filter(|item| !item.is_empty())
				.collect(),
		),
		other => other.to_text().map(|text| {
			text
				.split(',')
				.map(str::trim)
				.filter(|item| !item.is_empty())
				.map(str::to_string)
				.collect()
		}),
	}
}

fn parse_bool(value: Option<&yaml::Value>) -> Option<bool> {
	match value? {
		yaml::Value::Bool(value) => Some(*value),
		other => other
			.to_text()
			.and_then(|text| match text.trim().to_ascii_lowercase().as_str() {
				"true" => Some(true),
				"false" => Some(false),
				_ => None,
			}),
	}
}

fn parse_handoff(value: Option<&yaml::Value>) -> Option<HandoffTarget> {
	match value? {
		yaml::Value::Bool(true) => Some(HandoffTarget::Default),
		yaml::Value::Bool(false) | yaml::Value::Null => None,
		yaml::Value::Str(text) if text.trim().is_empty() => None,
		other => other.to_text().map(|text| {
			if text.eq_ignore_ascii_case("true") {
				HandoffTarget::Default
			} else {
				HandoffTarget::Pattern(text.trim().to_string())
			}
		}),
	}
}

fn parse_spawns(value: Option<&yaml::Value>) -> Option<Spawns> {
	let value = value?;
	let list = parse_list(Some(value))?;
	if list.iter().any(|name| name == "*") {
		return Some(Spawns::All);
	}
	Some(Spawns::Only(list))
}

/// Keep names the runtime can recognize; alias the historical `search`/`find`
/// spellings to this build's `grep`/`glob`. Unknown prompt-supplied names are
/// preserved so the task tool can reject them explicitly instead of silently
/// granting or dropping a capability.
fn normalize_tool_name(name: &str) -> String {
	let trimmed = name.trim();
	let lower = trimmed.to_ascii_lowercase();
	match lower.as_str() {
		"search" => "grep".to_string(),
		"find" => "glob".to_string(),
		"read" | "grep" | "glob" | "bash" | "edit" | "write" | "task" | "hub" | "todo"
		| "ask" | "yield" | "lsp" | "web_search" | "ast_grep" | "ast_edit" | "think" => lower,
		_ => trimmed.to_string(),
	}
}

fn yaml_to_json(value: &yaml::Value) -> serde_json::Value {
	match value {
		yaml::Value::Null => serde_json::Value::Null,
		yaml::Value::Bool(value) => serde_json::Value::Bool(*value),
		yaml::Value::Int(value) => serde_json::Value::from(*value),
		yaml::Value::Float(value) => serde_json::Value::from(*value),
		yaml::Value::Str(value) => serde_json::Value::String(value.clone()),
		yaml::Value::List(items) => {
			serde_json::Value::Array(items.iter().map(yaml_to_json).collect())
		},
		yaml::Value::Map(pairs) => serde_json::Value::Object(
			pairs
				.iter()
				.map(|(key, value)| (key.clone(), yaml_to_json(value)))
				.collect(),
		),
	}
}

// ---------------------------------------------------------------------------
// `omp agents unpack`
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct UnpackOptions {
	/// Overwrite files that already exist. Without it existing files are skipped.
	pub force: bool,
	pub dir: Option<PathBuf>,
	pub user: bool,
	pub project: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnpackSummary {
	pub target_dir: String,
	pub total: usize,
	pub written: Vec<String>,
	pub skipped: Vec<String>,
}

pub fn unpack(project_dir: &Path, options: &UnpackOptions) -> Result<UnpackSummary> {
	let explicit_dir = options.dir.as_ref().map(|dir| dirs::absolute(project_dir, dir));
	if explicit_dir.is_none() && options.user && options.project {
		return Err(Error::new("Use either --user or --project, not both"));
	}
	let target = if let Some(dir) = explicit_dir {
		dir
	} else if options.user {
		dirs::agent_dir().join("agents")
	} else if options.project {
		dirs::project_config_dir(project_dir).join("agents")
	} else {
		dirs::agent_dir().join("agents")
	};
	fs::create_dir_all(&target)
		.map_err(|error| Error::with_source(format!("cannot create {}", target.display()), error))?;

	let mut agents = bundled_agents()?;
	agents.sort_by(|left, right| left.name.cmp(&right.name));
	let mut summary = UnpackSummary {
		target_dir: target.display().to_string(),
		total: agents.len(),
		written: Vec::new(),
		skipped: Vec::new(),
	};
	for agent in agents {
		if !is_safe_name(&agent.name) {
			return Err(Error::new(format!(
				"refusing to unpack agent with unsafe name '{}'",
				agent.name
			)));
		}
		let path = target.join(format!("{}.md", agent.name));
		if path.exists() && !options.force {
			summary.skipped.push(path.display().to_string());
			continue;
		}
		let content = serialize_agent(&agent);
		fs::write(&path, content)
			.map_err(|error| Error::with_source(format!("cannot write {}", path.display()), error))?;
		summary.written.push(path.display().to_string());
	}
	Ok(summary)
}

pub fn print_unpack(summary: &UnpackSummary, json: bool) -> Result<()> {
	if json {
		println!("{}", serde_json::to_string_pretty(summary)?);
		return Ok(());
	}
	println!("Bundled agents: {}", summary.total);
	println!("Target directory: {}", summary.target_dir);
	println!("✔ Written: {}", summary.written.len());
	if !summary.skipped.is_empty() {
		println!(
			"⚠ Skipped existing: {} (use --force to overwrite)",
			summary.skipped.len()
		);
	}
	for path in &summary.written {
		println!("  + {path}");
	}
	for path in &summary.skipped {
		println!("  = {path}");
	}
	Ok(())
}

/// Serialize one agent in the same frontmatter shape the TypeScript loader
/// writes. Fields this build does not round-trip (`readSummarize`, `prewalk`,
/// `advisor`, `autoloadSkills`) are intentionally omitted, matching
/// `toFrontmatter`.
pub fn serialize_agent(agent: &AgentDefinition) -> String {
	let mut frontmatter = String::new();
	yaml_scalar(&mut frontmatter, "name", &agent.name);
	yaml_scalar(&mut frontmatter, "description", &agent.description);
	if let Some(tools) = &agent.tools
		&& !tools.is_empty()
	{
		frontmatter.push_str("tools: \n");
		for tool in tools {
			frontmatter.push_str("  - ");
			frontmatter.push_str(&yaml_quote(tool));
			frontmatter.push('\n');
		}
	}
	if let Some(spawns) = &agent.spawns {
		match spawns {
			Spawns::All => yaml_scalar(&mut frontmatter, "spawns", "*"),
			Spawns::Only(names) if !names.is_empty() => {
				frontmatter.push_str("spawns: \n");
				for name in names {
					frontmatter.push_str("  - ");
					frontmatter.push_str(&yaml_quote(name));
					frontmatter.push('\n');
				}
			},
			Spawns::Only(_) => {},
		}
	}
	if !agent.model.is_empty() {
		frontmatter.push_str("model: \n");
		for model in &agent.model {
			frontmatter.push_str("  - ");
			frontmatter.push_str(&yaml_quote(model));
			frontmatter.push('\n');
		}
	}
	if let Some(level) = &agent.thinking_level {
		yaml_scalar(&mut frontmatter, "thinkingLevel", level);
	}
	if let Some(output) = &agent.output {
		frontmatter.push_str("output: \n");
		yaml_value(&mut frontmatter, output, 1);
	}
	if agent.blocking {
		frontmatter.push_str("blocking: true\n");
	}
	format!("---\n{frontmatter}---\n\n{}\n", agent.system_prompt.trim())
}

fn yaml_scalar(output: &mut String, key: &str, value: &str) {
	output.push_str(key);
	output.push_str(": ");
	output.push_str(&yaml_quote(value));
	output.push('\n');
}

fn yaml_value(output: &mut String, value: &serde_json::Value, depth: usize) {
	let indent = "  ".repeat(depth);
	match value {
		serde_json::Value::Object(map) => {
			for (key, value) in map {
				match value {
					serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
						output.push_str(&indent);
						output.push_str(key);
						output.push_str(":\n");
						yaml_value(output, value, depth + 1);
					},
					_ => {
						output.push_str(&indent);
						output.push_str(key);
						output.push_str(": ");
						output.push_str(&yaml_scalar_value(value));
						output.push('\n');
					},
				}
			}
		},
		serde_json::Value::Array(items) => {
			for item in items {
				output.push_str(&indent);
				match item {
					serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
						output.push_str("-\n");
						yaml_value(output, item, depth + 1);
					},
					_ => {
						output.push_str("- ");
						output.push_str(&yaml_scalar_value(item));
						output.push('\n');
					},
				}
			}
		},
		_ => {
			output.push_str(&indent);
			output.push_str(&yaml_scalar_value(value));
			output.push('\n');
		},
	}
}

fn yaml_scalar_value(value: &serde_json::Value) -> String {
	match value {
		serde_json::Value::Null => "null".to_string(),
		serde_json::Value::Bool(value) => value.to_string(),
		serde_json::Value::Number(value) => value.to_string(),
		serde_json::Value::String(text) => yaml_quote(text),
		other => other.to_string(),
	}
}

fn yaml_quote(value: &str) -> String {
	let needs_quotes = value.is_empty()
		|| value.starts_with(|ch: char| {
			matches!(
				ch,
				'@' | '*'
					| '&' | '!' | '#' | '?' | '|' | '>' | '{' | '}' | '[' | ']' | ',' | '\''
					| '"' | '%' | '-'
			)
		})
		|| value.contains(": ")
		|| value.contains(" #")
		|| matches!(
			value.to_ascii_lowercase().as_str(),
			"true" | "false" | "null" | "yes" | "no" | "on" | "off"
		)
		|| value.parse::<f64>().is_ok();
	if needs_quotes {
		format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
	} else {
		value.to_string()
	}
}

fn is_safe_name(name: &str) -> bool {
	!name.is_empty()
		&& name
			.chars()
			.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// `omp agents` with no action prints read-only help and writes nothing.
pub fn print_help() -> Result<()> {
	println!(
		"Usage: omp agents <action> [options]\n\n\
		 Actions:\n  unpack   Write the seven bundled agents to disk\n\n\
		 Options:\n  --dir <path>   Unpack into <project>/<path>\n  --user         Unpack into the user agent directory\n  --project      Unpack into <project>/.omp/agents\n  -f, --force    Overwrite existing files\n  --json         Print machine-readable JSON"
	);
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::{SystemTime, UNIX_EPOCH};

	struct TempDir(PathBuf);

	impl TempDir {
		fn new(label: &str) -> Self {
			let nanos = SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.expect("clock")
				.as_nanos();
			let path = std::env::temp_dir().join(format!(
				"omp-agents-{label}-{}-{nanos}",
				std::process::id()
			));
			fs::create_dir_all(&path).expect("create temp dir");
			Self(path)
		}
	}

	impl Drop for TempDir {
		fn drop(&mut self) {
			let _ = fs::remove_dir_all(&self.0);
		}
	}

	#[test]
	fn bundled_prompts_are_real_and_complete() {
		let agents = bundled_agents().expect("bundled");
		let names: Vec<_> = agents.iter().map(|agent| agent.name.as_str()).collect();
		assert_eq!(
			names,
			vec![
				"scout",
				"designer",
				"reviewer",
				"security-reviewer",
				"librarian",
				"task",
				"sonic"
			]
		);
		let scout = agents.iter().find(|a| a.name == "scout").expect("scout");
		assert!(scout.system_prompt.contains("Investigate the codebase rapidly"));
		assert_eq!(scout.tools.as_ref().expect("tools").last().map(String::as_str), Some("yield"));
		assert_eq!(scout.thinking_level.as_deref(), Some("medium"));
		assert!(scout.output.is_some());
		let librarian = agents.iter().find(|a| a.name == "librarian").expect("librarian");
		assert!(
			librarian
				.output
				.as_ref()
				.and_then(|schema| schema.pointer("/properties/sources/elements/properties/line_start"))
				.is_some(),
			"output schema property names must not be camelized"
		);
		let task = agents.iter().find(|a| a.name == "task").expect("task");
		assert_eq!(task.spawns, Some(Spawns::All));
		assert_eq!(task.model, vec!["@task".to_string()]);
		assert!(task.system_prompt.contains("Worker agent: delegated tasks."));
		let sonic = agents.iter().find(|a| a.name == "sonic").expect("sonic");
		assert_eq!(sonic.model, vec!["@smol".to_string()]);
		assert_eq!(sonic.thinking_level.as_deref(), Some("medium"));
	}

	#[test]
	fn project_beats_extension_beats_bundled() {
		let temp = TempDir::new("precedence");
		let project = temp.0.join("project");
		let extension = temp.0.join("extension");
		let project_agents = project.join(".omp/agents");
		fs::create_dir_all(&project_agents).expect("project agents");
		fs::create_dir_all(extension.join("agents")).expect("extension agents");
		fs::write(
			project_agents.join("scout.md"),
			"---\nname: scout\ndescription: project scout\n---\nproject body\n",
		)
		.expect("write project agent");
		fs::write(
			extension.join("agents").join("scout.md"),
			"---\nname: scout\ndescription: extension scout\n---\nextension body\n",
		)
		.expect("write extension agent");
		fs::write(
			extension.join("agents").join("extra.md"),
			"---\nname: extra\ndescription: extra\n---\nextra body\n",
		)
		.expect("write extra agent");

		let discovery = discover_agents(&project, &[extension]).expect("discover");
		assert_eq!(
			discovery.project_agents_dir.as_deref(),
			Some(project_agents.to_str().expect("utf8"))
		);
		let scout = discovery
			.agents
			.iter()
			.find(|agent| agent.name == "scout")
			.expect("scout");
		assert_eq!(scout.source, AgentSource::Project);
		assert_eq!(scout.description, "project scout");
		let extra = discovery
			.agents
			.iter()
			.find(|agent| agent.name == "extra")
			.expect("extra");
		assert_eq!(extra.source, AgentSource::Extension);
		assert!(discovery.agents.iter().any(|agent| agent.name == "reviewer"));
	}

	#[test]
	fn unpack_writes_seven_and_force_overwrites() {
		let temp = TempDir::new("unpack");
		let project = temp.0.join("project");
		fs::create_dir_all(&project).expect("project");
		let options = UnpackOptions { project: true, ..UnpackOptions::default() };
		let first = unpack(&project, &options).expect("unpack");
		assert_eq!(first.total, 7);
		assert_eq!(first.written.len(), 7);
		assert_eq!(first.skipped.len(), 0);
		for name in ["scout", "designer", "reviewer", "security-reviewer", "librarian", "task", "sonic"]
		{
			let path = project.join(".omp/agents").join(format!("{name}.md"));
			let content = fs::read_to_string(&path).expect("written agent");
			assert!(content.starts_with("---\n"));
			assert!(content.ends_with('\n'));
		}
		let second = unpack(&project, &options).expect("unpack again");
		assert_eq!(second.written.len(), 0);
		assert_eq!(second.skipped.len(), 7);
		fs::write(project.join(".omp/agents/scout.md"), "old").expect("mutate");
		let forced = unpack(
			&project,
			&UnpackOptions { project: true, force: true, ..UnpackOptions::default() },
		)
		.expect("force unpack");
		assert_eq!(forced.written.len(), 7);
		let scout = fs::read_to_string(project.join(".omp/agents/scout.md")).expect("scout");
		// The source wording is not the contract; the rewritten agent must parse
		// with real frontmatter and a real prompt.
		let parsed = parse_agent("scout.md", &scout, AgentSource::Project).expect("parsed force output");
		assert_eq!(parsed.name, "scout");
		assert!(!parsed.system_prompt.trim().is_empty(), "forced prompt body is empty");
		assert_ne!(scout, "old", "force did not overwrite the project agent");
	}

	#[test]
	fn serialized_agent_round_trips_schema_and_tools() {
		let agents = bundled_agents().expect("bundled");
		let scout = agents.iter().find(|agent| agent.name == "scout").expect("scout");
		let serialized = serialize_agent(scout);
		let parsed = parse_agent("roundtrip.md", &serialized, AgentSource::Project).expect("parse");
		assert_eq!(parsed.name, scout.name);
		assert_eq!(parsed.tools, scout.tools);
		assert_eq!(parsed.model, scout.model);
		assert_eq!(parsed.thinking_level, scout.thinking_level);
		assert_eq!(parsed.output, scout.output);
		assert_eq!(parsed.spawns, scout.spawns);
		assert_eq!(parsed.system_prompt, scout.system_prompt);
	}

	#[test]
	fn frontmatter_repairs_ambiguous_plain_scalars() {
		let agent = parse_agent(
			"ambiguous.md",
			"---\nname: custom\ndescription: Use when: the thing happens\n---\nbody\n",
			AgentSource::Project,
		)
		.expect("repaired parse");
		assert_eq!(agent.description, "Use when: the thing happens");
	}

	#[test]
	fn invalid_frontmatter_is_skipped_but_valid_siblings_load() {
		let temp = TempDir::new("invalid");
		let project = temp.0.join("project");
		let agents_dir = project.join(".omp/agents");
		fs::create_dir_all(&agents_dir).expect("agents dir");
		fs::write(agents_dir.join("broken.md"), "no frontmatter here").expect("broken");
		fs::write(
			agents_dir.join("good.md"),
			"---\nname: custom\ndescription: valid\ntools: read, fill\n---\nbody\n",
		)
		.expect("good");
		let discovery = discover_agents(&project, &[]).expect("discover");
		assert!(!discovery.agents.iter().any(|agent| agent.name == "broken"));
		let custom = discovery
			.agents
			.iter()
			.find(|agent| agent.name == "custom")
			.expect("custom");
		// Unknown tool names are preserved for explicit rejection at spawn.
		assert_eq!(
			custom.tools.as_deref(),
			Some(&["read".to_string(), "fill".to_string(), "yield".to_string()][..])
		);
	}

	#[test]
	fn explicit_empty_tools_stay_empty_and_spawns_infer() {
		let empty = parse_agent(
			"empty.md",
			"---\nname: bare\ndescription: none\ntools: []\n---\nbody\n",
			AgentSource::Project,
		)
		.expect("empty tools");
		assert_eq!(empty.tools, Some(Vec::new()));
		let inferred = parse_agent(
			"inferred.md",
			"---\nname: spawner\ndescription: spawns\ntools: read, task\n---\nbody\n",
			AgentSource::Project,
		)
		.expect("inferred spawns");
		assert_eq!(inferred.spawns, Some(Spawns::All));
		assert_eq!(
			inferred.tools.as_deref(),
			Some(&["read".to_string(), "task".to_string(), "yield".to_string()][..])
		);
	}

	#[test]
	fn unsafe_agent_names_are_never_written() {
		assert!(!is_safe_name("../escape"));
		assert!(!is_safe_name("bad name"));
		assert!(is_safe_name("security-reviewer"));
	}
}
