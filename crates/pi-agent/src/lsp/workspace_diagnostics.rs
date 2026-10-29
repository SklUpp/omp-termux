//! Workspace-wide project diagnostics.
//!
//! Port of `packages/coding-agent/src/lsp/workspace-diagnostics.ts`. Detection
//! collects every language marker at the workspace root (polyglot roots are not
//! short-circuited) and runs the matching toolchain checkers with a small
//! concurrency bound.

use std::{path::Path, time::Duration};

use futures::{StreamExt, stream};
use serde_json::Value;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectType {
	pub kind: String,
	pub command: Option<Vec<String>>,
	pub description: String,
}

#[derive(Debug, Clone)]
pub struct WorkspaceDiagnosticsResult {
	pub output: String,
	pub project_type: ProjectType,
	pub project_types: Vec<ProjectType>,
}

const MAX_CONCURRENT_CHECKERS: usize = 2;
const CHECKER_TIMEOUT: Duration = Duration::from_secs(10 * 60);

fn marker_exists(cwd: &Path, name: &str) -> bool {
	cwd.join(name).exists()
}

fn go_workspace_build_pattern(disk_path: &str) -> Option<String> {
	let trimmed = disk_path.trim();
	if trimmed.is_empty() {
		return None;
	}
	let windows_absolute = trimmed.len() > 2 && trimmed.as_bytes()[1] == b':';
	let is_absolute = trimmed.starts_with('/') || trimmed.starts_with('\\') || windows_absolute;
	let normalized = trimmed.replace('\\', "/");
	let dir = normalized.trim_end_matches('/');
	let dir = if dir.is_empty() { "." } else { dir };
	if dir == "." {
		return Some("./...".to_string());
	}
	if dir.ends_with("/...") {
		return Some(dir.to_string());
	}
	if is_absolute || dir.starts_with("./") || dir.starts_with("../") {
		return Some(format!("{dir}/..."));
	}
	Some(format!("./{dir}/..."))
}

fn parse_go_workspace_build_patterns(output: &str) -> Vec<String> {
	let Ok(parsed) = serde_json::from_str::<Value>(output) else {
		return Vec::new();
	};
	let Some(entries) = parsed.get("Use").and_then(Value::as_array) else {
		return Vec::new();
	};
	let mut patterns: Vec<String> = Vec::new();
	for entry in entries {
		let Some(disk_path) = entry.get("DiskPath").and_then(Value::as_str) else {
			continue;
		};
		if let Some(pattern) = go_workspace_build_pattern(disk_path) {
			if !patterns.contains(&pattern) {
				patterns.push(pattern);
			}
		}
	}
	patterns
}

async fn resolve_go_workspace_command(cwd: &Path, cancel: &CancellationToken) -> Vec<String> {
	let fallback = vec!["go".to_string(), "build".to_string(), "./...".to_string()];
	let output = async {
		let mut command = Command::new("go");
		command
			.args(["work", "edit", "-json"])
			.current_dir(cwd)
			.stdin(std::process::Stdio::null())
			.stdout(std::process::Stdio::piped())
			.stderr(std::process::Stdio::null())
			.kill_on_drop(true);
		#[cfg(unix)]
		command.process_group(0);
		let child = command.spawn().ok()?;
		tokio::select! {
			_ = cancel.cancelled() => None,
			result = child.wait_with_output() => {
				let result = result.ok()?;
				if !result.status.success() {
					return None;
				}
				Some(String::from_utf8_lossy(&result.stdout).into_owned())
			}
		}
	}
	.await;
	let Some(output) = output else {
		return fallback;
	};
	let patterns = parse_go_workspace_build_patterns(&output);
	if patterns.is_empty() {
		return fallback;
	}
	let mut command = vec!["go".to_string(), "build".to_string()];
	command.extend(patterns);
	command
}

pub async fn detect_project_types(cwd: &Path, cancel: &CancellationToken) -> Vec<ProjectType> {
	let mut detected = Vec::new();
	if marker_exists(cwd, "Cargo.toml") {
		detected.push(ProjectType {
			kind: "rust".to_string(),
			command: Some(vec![
				"cargo".to_string(),
				"check".to_string(),
				"--message-format=short".to_string(),
			]),
			description: "Rust (cargo check)".to_string(),
		});
	}
	if marker_exists(cwd, "tsconfig.json") {
		detected.push(ProjectType {
			kind: "typescript".to_string(),
			command: Some(vec![
				"npx".to_string(),
				"--no-install".to_string(),
				"tsc".to_string(),
				"--noEmit".to_string(),
			]),
			description: "TypeScript (npx --no-install tsc --noEmit)".to_string(),
		});
	}
	if marker_exists(cwd, "go.work") {
		detected.push(ProjectType {
			kind: "go".to_string(),
			command: Some(resolve_go_workspace_command(cwd, cancel).await),
			description: "Go workspace (go build)".to_string(),
		});
	} else if marker_exists(cwd, "go.mod") {
		detected.push(ProjectType {
			kind: "go".to_string(),
			command: Some(vec!["go".to_string(), "build".to_string(), "./...".to_string()]),
			description: "Go (go build)".to_string(),
		});
	}
	if marker_exists(cwd, "pyproject.toml") || marker_exists(cwd, "pyrightconfig.json") {
		detected.push(ProjectType {
			kind: "python".to_string(),
			command: Some(vec!["pyright".to_string()]),
			description: "Python (pyright)".to_string(),
		});
	}
	if detected.is_empty() {
		return vec![ProjectType {
			kind: "unknown".to_string(),
			command: None,
			description: "Unknown project type".to_string(),
		}];
	}
	detected
}

fn interpret_empty_result(exit_code: Option<i32>, signal: Option<String>, command: &[String]) -> String {
	if exit_code == Some(0) {
		return "No issues found".to_string();
	}
	let detail = signal.map_or_else(
		|| format!("exited with code {}", exit_code.unwrap_or(-1)),
		|signal| format!("was killed by {signal}"),
	);
	format!(
		"Failed to run {}: the checker {detail} without reporting anything, so the workspace was not verified",
		command.join(" ")
	)
}

async fn run_project_diagnostics(
	cwd: &Path,
	project_type: &ProjectType,
	cancel: &CancellationToken,
) -> String {
	let Some(command) = &project_type.command else {
		return "Cannot detect project type. Supported: Rust (Cargo.toml), TypeScript (tsconfig.json), Go (go.work/go.mod), Python (pyproject.toml)".to_string();
	};
	if cancel.is_cancelled() {
		return "Operation aborted".to_string();
	}
	let mut process = Command::new(&command[0]);
	process
		.args(&command[1..])
		.current_dir(cwd)
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::piped())
		.kill_on_drop(true);
	#[cfg(unix)]
	process.process_group(0);
	let child = match process.spawn() {
		Ok(child) => child,
		Err(error) => return format!("Failed to run {}: {error}", command.join(" ")),
	};
	let output = tokio::select! {
		_ = cancel.cancelled() => return "Operation aborted".to_string(),
		result = tokio::time::timeout(CHECKER_TIMEOUT, child.wait_with_output()) => match result {
			Ok(Ok(output)) => output,
			Ok(Err(error)) => return format!("Failed to run {}: {error}", command.join(" ")),
			Err(_) => return format!("Failed to run {}: timed out", command.join(" ")),
		}
	};
	let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
	combined.push_str(&String::from_utf8_lossy(&output.stderr));
	let combined = combined.trim().to_string();
	if combined.is_empty() {
		return interpret_empty_result(output.status.code(), exit_signal(&output.status), command);
	}
	let lines: Vec<&str> = combined.split('\n').collect();
	if lines.len() > 50 {
		return format!("{}\n[…{}ln elided…]", lines[..50].join("\n"), lines.len() - 50);
	}
	combined
}

fn exit_signal(status: &std::process::ExitStatus) -> Option<String> {
	#[cfg(unix)]
	{
		use std::os::unix::process::ExitStatusExt;
		status.signal().map(|signal| format!("signal {signal}"))
	}
	#[cfg(not(unix))]
	{
		let _ = status;
		None
	}
}

fn combine_descriptions(types: &[ProjectType]) -> String {
	types.iter().map(|project| project.description.as_str()).collect::<Vec<_>>().join(" + ")
}

fn combine_outputs(sections: &[(String, String)]) -> String {
	if sections.len() == 1 {
		return sections[0].1.clone();
	}
	sections
		.iter()
		.map(|(description, output)| format!("=== {description} ===\n{output}"))
		.collect::<Vec<_>>()
		.join("\n\n")
}

pub async fn run_workspace_diagnostics(
	cwd: &Path,
	cancel: &CancellationToken,
) -> WorkspaceDiagnosticsResult {
	let project_types = detect_project_types(cwd, cancel).await;
	let primary = project_types
		.first()
		.cloned()
		.unwrap_or(ProjectType {
			kind: "unknown".to_string(),
			command: None,
			description: "Unknown project type".to_string(),
		});
	let project_type = if project_types.len() > 1 {
		ProjectType { description: combine_descriptions(&project_types), ..primary }
	} else {
		primary
	};

	let futures = project_types.iter().cloned().map(|project| {
		let cwd = cwd.to_path_buf();
		let cancel = cancel.clone();
		async move {
			let description = project.description.clone();
			let output = run_project_diagnostics(&cwd, &project, &cancel).await;
			(description, output)
		}
	});
	let sections: Vec<(String, String)> = stream::iter(futures)
		.buffered(MAX_CONCURRENT_CHECKERS)
		.collect()
		.await;

	WorkspaceDiagnosticsResult {
		output: combine_outputs(&sections),
		project_type,
		project_types,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn go_workspace_patterns_normalize_paths() {
		assert_eq!(go_workspace_build_pattern("."), Some("./...".to_string()));
		assert_eq!(go_workspace_build_pattern("mod"), Some("./mod/...".to_string()));
		assert_eq!(go_workspace_build_pattern("./mod/"), Some("./mod/...".to_string()));
		assert_eq!(go_workspace_build_pattern("/abs/mod"), Some("/abs/mod/...".to_string()));
		assert_eq!(go_workspace_build_pattern("mod/..."), Some("mod/...".to_string()));
	}

	#[test]
	fn parses_go_work_use_entries() {
		let output = r#"{"Use":[{"DiskPath":"."},{"DiskPath":"mod"},{"DiskPath":""}]}"#;
		assert_eq!(
			parse_go_workspace_build_patterns(output),
			vec!["./...".to_string(), "./mod/...".to_string()]
		);
	}

	#[test]
	fn empty_checker_crash_is_not_clean() {
		let command = vec!["cargo".to_string(), "check".to_string()];
		let message = interpret_empty_result(Some(101), None, &command);
		assert!(message.contains("not verified"));
		assert_eq!(interpret_empty_result(Some(0), None, &command), "No issues found");
	}
}
