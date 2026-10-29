//! CLI linter adapters: Biome and SwiftLint.
//!
//! Port of `packages/coding-agent/src/lsp/clients/biome-client.ts` and
//! `swiftlint-client.ts`. Both execute the already-resolved binary; neither
//! installs or downloads anything. An LSP server configured with `isLinter` is
//! handled by the normal LSP diagnostics path in [`super::diagnostics`].

use std::{
	path::{Path, PathBuf},
	time::Duration,
};

use serde_json::Value;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::{
	config::resolve_command,
	edits::write_text_atomic,
	types::{CustomLinter, Diagnostic, Position, Range, ServerConfig},
	utils::{absolutize, normalize_absolute},
};

const CLI_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinterKind {
	Biome,
	SwiftLint,
}

fn basename(command: &str) -> &str {
	command.rsplit('/').next().unwrap_or(command)
}

fn linter_kind(config: &ServerConfig) -> Option<LinterKind> {
	match config.custom_linter {
		Some(CustomLinter::Biome) => Some(LinterKind::Biome),
		Some(CustomLinter::SwiftLint) => Some(LinterKind::SwiftLint),
		None => match basename(&config.command) {
			"biome" => Some(LinterKind::Biome),
			"swiftlint" => Some(LinterKind::SwiftLint),
			_ => None,
		},
	}
}

fn resolved(config: &ServerConfig, cwd: &Path) -> Result<PathBuf, String> {
	config
		.resolved_command
		.clone()
		.or_else(|| resolve_command(&config.command, cwd))
		.ok_or_else(|| format!("LSP linter binary not found: {}", config.command))
}

struct CommandOutput {
	stdout: String,
	stderr: String,
	success: bool,
}

async fn run(
	program: &Path,
	args: &[String],
	cwd: &Path,
	cancel: &CancellationToken,
) -> Result<CommandOutput, String> {
	let mut command = Command::new(program);
	command
		.args(args)
		.current_dir(cwd)
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::piped())
		.kill_on_drop(true);
	#[cfg(unix)]
	command.process_group(0);
	let child = command.spawn().map_err(|error| error.to_string())?;
	let output = tokio::select! {
		_ = cancel.cancelled() => return Err("Operation aborted".to_string()),
		result = tokio::time::timeout(CLI_TIMEOUT, child.wait_with_output()) => match result {
			Ok(Ok(output)) => output,
			Ok(Err(error)) => return Err(error.to_string()),
			Err(_) => return Err("linter command timed out".to_string()),
		}
	};
	Ok(CommandOutput {
		stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
		stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
		success: output.status.success(),
	})
}

fn severity_from_name(severity: &str) -> u8 {
	match severity.to_ascii_lowercase().as_str() {
		"error" => 1,
		"warning" | "warn" => 2,
		"info" | "information" => 3,
		"hint" => 4,
		_ => 2,
	}
}

/// Run a custom linter for one file.
pub async fn lint(
	server_name: &str,
	config: &ServerConfig,
	cwd: &Path,
	file_path: &Path,
	cancel: &CancellationToken,
) -> Result<Vec<Diagnostic>, String> {
	let Some(kind) = linter_kind(config) else {
		return Err(format!("{server_name} is not a custom linter"));
	};
	let program = resolved(config, cwd)?;
	match kind {
		LinterKind::Biome => {
			let args = vec![
				"lint".to_string(),
				"--reporter=json".to_string(),
				file_path.to_string_lossy().into_owned(),
			];
			let output = run(&program, &args, cwd, cancel).await?;
			if !output.success && output.stdout.trim().is_empty() {
				return Err(format!("Biome lint failed: {}", output.stderr.trim()));
			}
			Ok(parse_biome(&output.stdout, file_path, cwd))
		}
		LinterKind::SwiftLint => {
			let args = vec![
				"lint".to_string(),
				"--quiet".to_string(),
				"--reporter".to_string(),
				"json".to_string(),
				file_path.to_string_lossy().into_owned(),
			];
			let output = run(&program, &args, cwd, cancel).await?;
			if output.stdout.is_empty() {
				return Ok(Vec::new());
			}
			Ok(parse_swiftlint(&output.stdout))
		}
	}
}

/// Run a custom linter's formatter. `None` means the linter has no formatter.
pub async fn format(
	server_name: &str,
	config: &ServerConfig,
	cwd: &Path,
	file_path: &Path,
	content: &str,
	cancel: &CancellationToken,
) -> Result<Option<String>, String> {
	let Some(kind) = linter_kind(config) else {
		return Err(format!("{server_name} is not a custom linter"));
	};
	let program = resolved(config, cwd)?;
	match kind {
		LinterKind::Biome => {
			write_text_atomic(file_path, content).await?;
			let args = vec![
				"format".to_string(),
				"--write".to_string(),
				file_path.to_string_lossy().into_owned(),
			];
			let output = run(&program, &args, cwd, cancel).await?;
			if !output.success {
				return Ok(Some(content.to_string()));
			}
			let formatted = tokio::fs::read_to_string(file_path)
				.await
				.map_err(|error| error.to_string())?;
			Ok(Some(formatted))
		}
		LinterKind::SwiftLint => Ok(Some(content.to_string())),
	}
}

fn parse_biome(stdout: &str, target_file: &Path, cwd: &Path) -> Vec<Diagnostic> {
	let Ok(parsed) = serde_json::from_str::<Value>(stdout) else {
		return Vec::new();
	};
	let Some(diagnostics) = parsed.get("diagnostics").and_then(Value::as_array) else {
		return Vec::new();
	};
	let target = normalize_absolute(&absolutize(target_file));
	let mut out = Vec::new();
	for diagnostic in diagnostics {
		let Some(location) = diagnostic.get("location") else {
			continue;
		};
		let Some(file) = location.get("path").and_then(Value::as_str) else {
			continue;
		};
		let file_path = if Path::new(file).is_absolute() {
			PathBuf::from(file)
		} else {
			cwd.join(file)
		};
		if normalize_absolute(&absolutize(&file_path)) != target {
			continue;
		}
		let start = location.get("start");
		let null_value = Value::Null;
		let end = location.get("end").unwrap_or_else(|| start.unwrap_or(&null_value));
		let number = |value: Option<&Value>, key: &str, fallback: u64| -> u32 {
			value
				.and_then(|value| value.get(key))
				.and_then(Value::as_u64)
				.unwrap_or(fallback)
				.saturating_sub(1) as u32
		};
		let message = diagnostic
			.get("message")
			.and_then(Value::as_str)
			.unwrap_or("Biome diagnostic");
		let severity = diagnostic.get("severity").and_then(Value::as_str).unwrap_or("warning");
		let code = diagnostic.get("category").and_then(Value::as_str).map(str::to_string);
		out.push(Diagnostic {
			range: Range {
				start: Position {
					line: number(start, "line", 1),
					character: number(start, "column", 1),
				},
				end: Position {
					line: number(Some(end), "line", 1),
					character: number(Some(end), "column", 1),
				},
			},
			severity: Some(severity_from_name(severity)),
			code: code.map(Value::String),
			source: Some("biome".to_string()),
			message: message.to_string(),
			tags: None,
			related_information: None,
			data: None,
		});
	}
	out
}

fn parse_swiftlint(stdout: &str) -> Vec<Diagnostic> {
	let Ok(parsed) = serde_json::from_str::<Value>(stdout) else {
		return Vec::new();
	};
	let Some(violations) = parsed.as_array() else {
		return Vec::new();
	};
	let mut out = Vec::new();
	for violation in violations {
		let line = violation.get("line").and_then(Value::as_u64).unwrap_or(1).max(1) - 1;
		let character = violation
			.get("character")
			.and_then(Value::as_u64)
			.unwrap_or(1)
			.max(1)
			- 1;
		let severity = violation
			.get("severity")
			.and_then(Value::as_str)
			.unwrap_or("warning");
		let message = violation
			.get("reason")
			.and_then(Value::as_str)
			.unwrap_or("SwiftLint violation");
		let rule = violation.get("rule_id").and_then(Value::as_str);
		out.push(Diagnostic {
			range: Range {
				start: Position { line: line as u32, character: character as u32 },
				end: Position { line: line as u32, character: character as u32 },
			},
			severity: Some(severity_from_name(severity)),
			code: rule.map(|rule| Value::String(rule.to_string())),
			source: Some("swiftlint".to_string()),
			message: message.to_string(),
			tags: None,
			related_information: None,
			data: None,
		});
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn biome_filters_to_target_file_and_converts_positions() {
		let stdout = r#"{"diagnostics":[
			{"category":"lint/style","severity":"error","message":"bad","location":{"path":"src/a.ts","start":{"line":2,"column":3},"end":{"line":2,"column":5}}},
			{"category":"lint","severity":"warning","message":"other","location":{"path":"src/b.ts","start":{"line":1,"column":1}}}
		]}"#;
		let diagnostics = parse_biome(stdout, Path::new("/project/src/a.ts"), Path::new("/project"));
		assert_eq!(diagnostics.len(), 1);
		assert_eq!(diagnostics[0].range.start.line, 1);
		assert_eq!(diagnostics[0].range.start.character, 2);
		assert_eq!(diagnostics[0].severity, Some(1));
	}

	#[test]
	fn swiftlint_converts_one_based_positions() {
		let stdout = r#"[{"line":4,"character":7,"severity":"warning","reason":"why","rule_id":"rule"}]"#;
		let diagnostics = parse_swiftlint(stdout);
		assert_eq!(diagnostics[0].range.start.line, 3);
		assert_eq!(diagnostics[0].range.start.character, 6);
		assert_eq!(diagnostics[0].source.as_deref(), Some("swiftlint"));
	}
}
