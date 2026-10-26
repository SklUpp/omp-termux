//! Text and NDJSON output.
//!
//! `text` prints the final assistant text on stdout and progress on stderr
//! (`print-mode.ts:228-231`); `json` emits one header line plus every event as
//! NDJSON (`print-mode.ts:112-156`).

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::ValueEnum;

use crate::{Result, agent::Event};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum OutputMode {
	/// Human-readable final text.
	#[default]
	Text,
	/// Newline-delimited session header and events.
	Json,
}

#[derive(Debug)]
pub struct Reporter<W: Write, E: Write> {
	mode: OutputMode,
	out: W,
	err: E,
	working_announced: bool,
	final_text: Option<String>,
}

impl<W: Write, E: Write> Reporter<W, E> {
	pub fn new(mode: OutputMode, out: W, err: E) -> Self {
		Self { mode, out, err, working_announced: false, final_text: None }
	}

	/// NDJSON session header. Emitted once, JSON mode only.
	pub fn session_header(&mut self, id: &str, cwd: &std::path::Path) -> Result<()> {
		if self.mode != OutputMode::Json {
			return Ok(());
		}
		let header = serde_json::json!({
			"type": "session",
			"version": 1,
			"id": id,
			"timestamp": iso8601(SystemTime::now()),
			"cwd": cwd.display().to_string(),
		});
		writeln!(self.out, "{header}")?;
		self.out.flush()?;
		Ok(())
	}

	/// Called before a turn starts: text mode announces work on stderr.
	pub fn begin_turn(&mut self) -> Result<()> {
		if self.mode == OutputMode::Text && !self.working_announced {
			writeln!(self.err, "Working...")?;
			self.working_announced = true;
		}
		Ok(())
	}

	/// Emit one event: NDJSON in JSON mode, stderr progress in text mode.
	pub fn event(&mut self, event: &Event) -> Result<()> {
		if self.mode == OutputMode::Json {
			let line = serde_json::to_string(event)?;
			writeln!(self.out, "{line}")?;
			self.out.flush()?;
		}
		match event {
			Event::MessageEnd { text } | Event::AgentEnd { text } => {
				self.final_text = Some(text.clone());
			},
			Event::ToolStart { tool, .. } => {
				if self.mode == OutputMode::Text {
					writeln!(self.err, "→ {tool}")?;
				}
			},
			Event::ToolEnd { tool, error: Some(error), .. } => {
				if self.mode == OutputMode::Text {
					writeln!(self.err, "✗ {tool}: {error}")?;
				}
			},
			_ => {},
		}
		Ok(())
	}

	/// Flush the final assistant text (text mode) and stdout.
	pub fn finish(&mut self) -> Result<()> {
		if self.mode == OutputMode::Text
			&& let Some(text) = self.final_text.take()
		{
			writeln!(self.out, "{text}")?;
		}
		self.out.flush()?;
		self.err.flush()?;
		Ok(())
	}
}

/// Session id: wall-clock nanos XOR pid, hex. Unique enough for a headless run.
#[must_use]
pub fn session_id() -> String {
	let nanos = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |duration| duration.as_nanos());
	format!("{:x}{:x}", std::process::id(), nanos)
}

/// RFC 3339 UTC timestamp without a chrono dependency.
#[must_use]
pub fn iso8601(time: SystemTime) -> String {
	let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
	let seconds = duration.as_secs();
	let days = i64::try_from(seconds / 86_400).unwrap_or(0);
	let seconds_of_day = seconds % 86_400;
	let (year, month, day) = civil_from_days(days);
	format!(
		"{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
		seconds_of_day / 3_600,
		(seconds_of_day % 3_600) / 60,
		seconds_of_day % 60,
		duration.subsec_millis(),
	)
}

/// Howard Hinnant's days-to-civil conversion.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
	let z = days + 719_468;
	let era = z.div_euclid(146_097);
	let day_of_era = z.rem_euclid(146_097);
	let year_of_era =
		(day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
	let year = year_of_era + era * 400;
	let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
	let month_prime = (5 * day_of_year + 2) / 153;
	let day = u32::try_from(day_of_year - (153 * month_prime + 2) / 5 + 1).unwrap_or(1);
	let month = u32::try_from(if month_prime < 10 {
		month_prime + 3
	} else {
		month_prime - 9
	})
	.unwrap_or(1);
	let year = if month <= 2 { year + 1 } else { year };
	(year, month, day)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn json_mode_emits_header_then_ndjson_events() {
		let mut out = Vec::new();
		let mut err = Vec::new();
		{
			let mut reporter = Reporter::new(OutputMode::Json, &mut out, &mut err);
			reporter
				.session_header("abc", std::path::Path::new("/work"))
				.expect("header");
			reporter
				.event(&Event::MessageEnd { text: "final".into() })
				.expect("event");
			reporter.finish().expect("finish");
		}
		let out = String::from_utf8(out).expect("utf8");
		let lines: Vec<&str> = out.lines().collect();
		assert_eq!(lines.len(), 2);
		assert!(lines[0].starts_with(r#"{"type":"session""#));
		assert_eq!(lines[1], r#"{"type":"message_end","text":"final"}"#);
		assert!(err.is_empty());
	}

	#[test]
	fn text_mode_keeps_progress_on_stderr() {
		let mut out = Vec::new();
		let mut err = Vec::new();
		{
			let mut reporter = Reporter::new(OutputMode::Text, &mut out, &mut err);
			reporter.begin_turn().expect("begin");
			reporter
				.event(&Event::ToolStart {
					id: "call-1".into(),
					tool: "read".into(),
					args: serde_json::json!({}),
				})
				.expect("tool");
			reporter
				.event(&Event::AgentEnd { text: "answer".into() })
				.expect("end");
			reporter.finish().expect("finish");
		}
		assert_eq!(String::from_utf8(out).expect("utf8"), "answer\n");
		let err = String::from_utf8(err).expect("utf8");
		assert!(err.contains("Working..."));
		assert!(err.contains("→ read"));
	}

	#[test]
	fn iso8601_encodes_epoch_and_leap_day() {
		assert_eq!(iso8601(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
		assert_eq!(
			iso8601(UNIX_EPOCH + std::time::Duration::from_secs(951_782_400)),
			"2000-02-29T00:00:00.000Z"
		);
	}
}
