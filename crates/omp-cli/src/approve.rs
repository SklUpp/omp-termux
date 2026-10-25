//! Tool approval matrix (`--approval-mode`, `--allow`).
//!
//! `always-ask` prompts on stdin, `write` auto-approves read-only tools,
//! `yolo` approves everything; an explicit `--allow <tool>` always approves
//! that tool. Mirrors `packages/coding-agent` approval semantics.

use std::collections::BTreeSet;
use std::io::{BufRead, Write};

use clap::ValueEnum;

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum ApprovalMode {
	/// Ask before every tool call.
	#[default]
	#[value(name = "always-ask")]
	AlwaysAsk,
	/// Auto-approve read-only tools, ask for writes.
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

	/// Pure matrix: explicit allow, then `yolo`, then read-only under `write`.
	#[must_use]
	pub fn decide(&self, tool: &str, read_only: bool) -> Decision {
		if self.allow.contains(tool) || self.mode == ApprovalMode::Yolo {
			return Decision::Auto;
		}
		if read_only && self.mode == ApprovalMode::Write {
			return Decision::Auto;
		}
		Decision::Ask
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
}
