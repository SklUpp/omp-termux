//! Tool approval matrix (`--approval-mode`, `--allow`).
//!
//! `always-ask` prompts on stdin, `write` auto-approves read-only tools,
//! `yolo` approves everything; an explicit `--allow <tool>` always approves
//! that tool. Mirrors `packages/coding-agent` approval semantics.

use std::collections::BTreeSet;
use std::io::{BufRead, Write};

use async_trait::async_trait;
use clap::ValueEnum;

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum ApprovalMode {
	/// Ask before every tool call.
	#[default]
	#[value(name = "always-ask")]
	AlwaysAsk,
	/// Auto-approve read-only tools, ask for writes/execs.
	/// Note: In TS `approval.ts`, `write` mode approves up to `write` tier.
	/// The Android headless parity plan explicitly defines `write` mode as
	/// read-only auto-permitted, requiring explicit approval for writes and execs.
	Write,
	/// Auto-approve everything.
	Yolo,
}

impl ApprovalMode {
	#[must_use]
	pub fn parse(raw: &str) -> Option<Self> {
		match raw {
			"always-ask" => Some(Self::AlwaysAsk),
			"write" => Some(Self::Write),
			"yolo" => Some(Self::Yolo),
			_ => None,
		}
	}

	#[must_use]
	pub fn as_str(self) -> &'static str {
		match self {
			Self::AlwaysAsk => "always-ask",
			Self::Write => "write",
			Self::Yolo => "yolo",
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
	/// Run without asking.
	Auto,
	/// Ask the operator on stdin.
	Ask,
}

/// Capability tier of a tool invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ToolTier {
	Read,
	Write,
	Exec,
}

/// Check if arguments target an ssh:// URI.
pub fn has_ssh_target(args: &serde_json::Value) -> bool {
	let is_ssh = |s: &str| s.starts_with("ssh://");
	if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
		if is_ssh(p) {
			return true;
		}
	}
	if let Some(paths) = args.get("paths") {
		if let Some(s) = paths.as_str() {
			if is_ssh(s) {
				return true;
			}
		} else if let Some(arr) = paths.as_array() {
			for item in arr {
				if let Some(s) = item.as_str() {
					if is_ssh(s) {
						return true;
					}
				}
			}
		}
	}
	false
}

/// Classify tool invocation into a capability tier.
pub fn classify_tool(name: &str, args: &serde_json::Value) -> ToolTier {
	match name {
		"todo" | "glob" => ToolTier::Read,
		"read" => {
			if has_ssh_target(args) {
				ToolTier::Exec
			} else {
				ToolTier::Read
			}
		},
		"grep" => {
			if has_ssh_target(args) {
				ToolTier::Exec
			} else {
				ToolTier::Read
			}
		},
		"write" | "edit" => {
			if has_ssh_target(args) {
				ToolTier::Exec
			} else {
				ToolTier::Write
			}
		},
		"bash" => ToolTier::Exec,
		// MCP tools and unknown tools default to conservative Exec
		_ => ToolTier::Exec,
	}
}

/// Extract detail description for approval prompts.
pub fn extract_tool_detail(name: &str, args: &serde_json::Value) -> Option<String> {
	match name {
		"bash" => args.get("command").and_then(|v| v.as_str()).map(|cmd| {
			let trimmed = cmd.trim();
			if trimmed.chars().count() > 60 {
				let prefix: String = trimmed.chars().take(57).collect();
				format!("{prefix}...")
			} else {
				trimmed.to_string()
			}
		}),
		"write" | "edit" | "read" => {
			args.get("path").and_then(|v| v.as_str()).map(|p| p.trim().to_string())
		},
		_ => None,
	}
}

#[derive(Debug, Clone, Default)]
pub struct Approvals {
	pub mode: ApprovalMode,
	pub allow: BTreeSet<String>,
}

impl Approvals {
	#[must_use]
	pub fn new(mode: ApprovalMode, allow: impl IntoIterator<Item = String>) -> Self {
		Self { mode, allow: allow.into_iter().collect() }
	}

	/// Authorize a registered MCP tool name if the exact colon tool allow
	/// `mcp:<server_name>:<tool_name>` was permitted.
	pub fn allow_mcp_tool_origin(&mut self, server_name: &str, tool_name: &str, registered_name: &str) {
		let colon_allow = format!("mcp:{server_name}:{tool_name}");
		if self.allow.contains(&colon_allow) {
			self.allow.insert(registered_name.to_string());
		}
	}

	/// Resolve approval decision given explicit tool tier.
	#[must_use]
	pub fn decide_tier(&self, tool: &str, tier: ToolTier) -> Decision {
		if self.allow.contains(tool) || self.mode == ApprovalMode::Yolo {
			return Decision::Auto;
		}
		if tier == ToolTier::Read && self.mode == ApprovalMode::Write {
			return Decision::Auto;
		}
		Decision::Ask
	}

	/// Pure matrix: explicit allow, then `yolo`, then read-only under `write`.
	#[must_use]
	pub fn decide(&self, tool: &str, read_only: bool) -> Decision {
		let tier = if read_only { ToolTier::Read } else { ToolTier::Write };
		self.decide_tier(tool, tier)
	}

	/// Decide, prompting on `input`/`prompt` when the matrix says ask.
	pub fn confirm(
		&self,
		tool: &str,
		read_only: bool,
		detail: Option<&str>,
		input: &mut impl BufRead,
		prompt: &mut impl Write,
	) -> Result<bool> {
		if self.decide(tool, read_only) == Decision::Auto {
			return Ok(true);
		}
		match detail {
			Some(detail) => write!(prompt, "Approve {tool} ({detail})? [y/N] ")?,
			None => write!(prompt, "Approve {tool}? [y/N] ")?,
		}
		prompt.flush()?;
		let mut line = String::new();
		if input.read_line(&mut line)? == 0 {
			return Err(Error::new("approval required but stdin closed"));
		}
		let answer = line.trim().to_ascii_lowercase();
		Ok(matches!(answer.as_str(), "y" | "yes"))
	}
}

#[async_trait]
impl pi_agent::ToolApprover for Approvals {
	async fn approve(&self, name: &str, args: &serde_json::Value) -> std::result::Result<(), String> {
		let tier = classify_tool(name, args);
		let decision = self.decide_tier(name, tier);
		match decision {
			Decision::Auto => Ok(()),
			Decision::Ask => {
				use std::io::IsTerminal;
				// In non-interactive environments (pipes, /dev/null, daemons, background),
				// closed stdin cannot prompt the operator; deny immediately before side effects.
				// MCP tool discovery already registered schemas via read-only tools/list;
				// this gate intercepts the execution before any side-effecting tools/call occurs.
				if !std::io::stdin().is_terminal() {
					return Err("approval required but stdin closed".to_string());
				}
				let detail = extract_tool_detail(name, args);
				let mut stdin = std::io::stdin().lock();
				let mut stderr = std::io::stderr().lock();
				let read_only = tier == ToolTier::Read;
				match self.confirm(name, read_only, detail.as_deref(), &mut stdin, &mut stderr) {
					Ok(true) => Ok(()),
					Ok(false) => Err(format!("approval denied for {name}")),
					Err(err) => Err(err.to_string()),
				}
			},
		}
	}
}
#[cfg(test)]
mod tests {
	use std::io::Cursor;

	use super::*;

	fn approvals(mode: ApprovalMode, allow: &[&str]) -> Approvals {
		Approvals::new(mode, allow.iter().map(ToString::to_string))
	}

	#[test]
	fn matrix_matches_typescript_semantics() {
		let ask = approvals(ApprovalMode::AlwaysAsk, &[]);
		assert_eq!(ask.decide("read", true), Decision::Ask);
		assert_eq!(ask.decide("bash", false), Decision::Ask);

		let auto = approvals(ApprovalMode::AlwaysAsk, &["bash"]);
		assert_eq!(auto.decide("bash", false), Decision::Auto);
		assert_eq!(auto.decide("write", false), Decision::Ask);

		let write = approvals(ApprovalMode::Write, &[]);
		assert_eq!(write.decide("read", true), Decision::Auto);
		assert_eq!(write.decide("write", false), Decision::Ask);

		let yolo = approvals(ApprovalMode::Yolo, &[]);
		assert_eq!(yolo.decide("bash", false), Decision::Auto);
	}

	#[test]
	fn confirm_reads_yes_from_stdin() {
		let approvals = approvals(ApprovalMode::AlwaysAsk, &[]);
		let mut input = Cursor::new(b"y\n");
		let mut prompt = Vec::new();
		assert!(
			approvals
				.confirm("bash", false, Some("rm -rf build"), &mut input, &mut prompt)
				.expect("confirm")
		);
		assert!(
			String::from_utf8(prompt)
				.expect("utf8")
				.starts_with("Approve bash")
		);

		let mut input = Cursor::new(b"\n");
		let mut prompt = Vec::new();
		assert!(
			!approvals
				.confirm("bash", false, None, &mut input, &mut prompt)
				.expect("confirm")
		);
	}

	#[test]
	fn allowed_tool_skips_the_prompt() {
		let approvals = approvals(ApprovalMode::AlwaysAsk, &["read"]);
		let mut input = Cursor::new(Vec::new());
		let mut prompt = Vec::new();
		assert!(
			approvals
				.confirm("read", true, None, &mut input, &mut prompt)
				.expect("confirm")
		);
	}

	#[test]
	fn arg_sensitive_classification_and_decide_tier() {
		use serde_json::json;

		// Local vs SSH paths
		assert_eq!(classify_tool("todo", &json!({})), ToolTier::Read);
		assert_eq!(classify_tool("glob", &json!({})), ToolTier::Read);
		assert_eq!(
			classify_tool("read", &json!({"path": "/local/file.txt"})),
			ToolTier::Read
		);
		assert_eq!(
			classify_tool("read", &json!({"path": "ssh://host/file.txt"})),
			ToolTier::Exec
		);
		assert_eq!(
			classify_tool("grep", &json!({"paths": ["local", "ssh://remote"]})),
			ToolTier::Exec
		);
		assert_eq!(
			classify_tool("write", &json!({"path": "file.txt"})),
			ToolTier::Write
		);
		assert_eq!(
			classify_tool("write", &json!({"path": "ssh://box/f.txt"})),
			ToolTier::Exec
		);
		assert_eq!(classify_tool("bash", &json!({"command": "ls"})), ToolTier::Exec);
		assert_eq!(classify_tool("mcp__server_tool", &json!({})), ToolTier::Exec);

		// Tier decisions
		let write_mode = approvals(ApprovalMode::Write, &[]);
		assert_eq!(write_mode.decide_tier("read", ToolTier::Read), Decision::Auto);
		assert_eq!(write_mode.decide_tier("write", ToolTier::Write), Decision::Ask);
		assert_eq!(write_mode.decide_tier("bash", ToolTier::Exec), Decision::Ask);
		assert_eq!(write_mode.decide_tier("read", ToolTier::Exec), Decision::Ask);

		let allow_bash = approvals(ApprovalMode::AlwaysAsk, &["bash"]);
		assert_eq!(allow_bash.decide_tier("bash", ToolTier::Exec), Decision::Auto);
		assert_eq!(allow_bash.decide_tier("write", ToolTier::Write), Decision::Ask);
	}

	#[test]
	fn test_allow_mcp_tool_origin_matches_exact_colon_syntax() {
		let mut app = approvals(ApprovalMode::AlwaysAsk, &["mcp:my_server:my_tool"]);
		// Server-level allow does NOT auto-authorize all tools
		let mut server_level = approvals(ApprovalMode::AlwaysAsk, &["mcp:my_server"]);
		server_level.allow_mcp_tool_origin("my_server", "my_tool", "mcp__my_server_my_tool");
		assert_eq!(server_level.decide("mcp__my_server_my_tool", false), Decision::Ask);

		// Other tool on same server is NOT authorized
		app.allow_mcp_tool_origin("my_server", "other_tool", "mcp__my_server_other_tool");
		assert_eq!(app.decide("mcp__my_server_other_tool", false), Decision::Ask);

		// Same tool on other server is NOT authorized
		app.allow_mcp_tool_origin("other_server", "my_tool", "mcp__other_server_my_tool");
		assert_eq!(app.decide("mcp__other_server_my_tool", false), Decision::Ask);

		// Exact origin match authorizes execution
		app.allow_mcp_tool_origin("my_server", "my_tool", "mcp__my_server_my_tool");
		assert_eq!(app.decide("mcp__my_server_my_tool", false), Decision::Auto);
	}
}
