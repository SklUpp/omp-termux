//! Kernel display rendering, output truncation, and artifact metadata.
//!
//! Rich values arrive as Jupyter-style MIME bundles. Status, JSON, images,
//! markdown, and plain text are split into distinct channels; model-visible text
//! caps each JSON value at [`MAX_DISPLAY_TEXT_BYTES`], matching upstream.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::types::{EvalDisplayOutput, EvalStatusEvent, OutputMeta};

/// Default model-visible output window.
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// Live tail budget (consumed by hosts that stream updates).
pub const LIVE_TAIL_BYTES: usize = 100 * 1024;
/// Line cap for a rendered cell.
pub const MAX_OUTPUT_LINES: usize = 3000;
/// Per-`display()` JSON text cap.
pub const MAX_DISPLAY_TEXT_BYTES: usize = 8000;
/// Full capture retained for an artifact write.
pub const ARTIFACT_MAX_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct RenderedDisplay {
	pub text: String,
	pub outputs: Vec<EvalDisplayOutput>,
}

fn normalize_display_text(text: &str) -> String {
	if text.ends_with('\n') {
		text.to_string()
	} else {
		format!("{text}\n")
	}
}

fn html_to_markdown(html: &str) -> String {
	static SCRIPT: LazyLock<Regex> =
		LazyLock::new(|| Regex::new(r"(?is)<(?:script|style)[^>]*>.*?</(?:script|style)>").expect("script regex"));
	static LINK: LazyLock<Regex> =
		LazyLock::new(|| Regex::new(r#"(?is)<a\s[^>]*href=["']([^"']*)["'][^>]*>(.*?)</a>"#).expect("link regex"));
	static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]+>").expect("tag regex"));
	let without_scripts = SCRIPT.replace_all(html, "");
	let mut text = LINK.replace_all(&without_scripts, "[$2]($1)").into_owned();
	text = TAG.replace_all(&text, "").into_owned();
	text = text
		.replace("&nbsp;", " ")
		.replace("&lt;", "<")
		.replace("&gt;", ">")
		.replace("&quot;", "\"")
		.replace("&#39;", "'")
		.replace("&amp;", "&");
	text.lines()
		.map(str::trim_end)
		.collect::<Vec<_>>()
		.join("\n")
		.trim()
		.to_string()
}

/// Render one MIME bundle into text plus structured outputs.
pub fn render_display_bundle(bundle: &Map<String, Value>) -> RenderedDisplay {
	let data = match bundle.get("data").and_then(Value::as_object) {
		Some(data) => data,
		None => bundle,
	};
	let mut outputs = Vec::new();

	if let Some(status) = data.get("application/x-omp-status")
		&& let Some(event) = EvalStatusEvent::from_bundle(status)
	{
		outputs.push(EvalDisplayOutput::Status(event));
		return RenderedDisplay { text: String::new(), outputs };
	}

	if let Some(data) = data.get("image/png").and_then(Value::as_str) {
		outputs.push(EvalDisplayOutput::Image {
			data: data.to_string(),
			mime_type: "image/png".to_string(),
		});
	}
	if let Some(data) = data.get("image/jpeg").and_then(Value::as_str) {
		outputs.push(EvalDisplayOutput::Image {
			data: data.to_string(),
			mime_type: "image/jpeg".to_string(),
		});
	}
	if let Some(json) = data.get("application/json") {
		outputs.push(EvalDisplayOutput::Json(json.clone()));
	}

	if let Some(markdown) = data.get("text/markdown").and_then(Value::as_str) {
		outputs.push(EvalDisplayOutput::Markdown { text: Some(markdown.to_string()) });
		return RenderedDisplay { text: normalize_display_text(markdown), outputs };
	}
	if let Some(plain) = data.get("text/plain").and_then(Value::as_str) {
		return RenderedDisplay { text: normalize_display_text(plain), outputs };
	}
	if let Some(html) = data.get("text/html").and_then(Value::as_str) {
		let markdown = html_to_markdown(html);
		if !markdown.is_empty() {
			outputs.push(EvalDisplayOutput::Markdown { text: Some(markdown.clone()) });
		}
		return RenderedDisplay {
			text: if markdown.is_empty() { String::new() } else { normalize_display_text(&markdown) },
			outputs,
		};
	}
	RenderedDisplay { text: String::new(), outputs }
}

/// Format one JSON display value for model-visible text.
pub fn format_display_json_for_text(value: &Value) -> String {
	let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
	let chars = text.chars().count();
	if chars > MAX_DISPLAY_TEXT_BYTES {
		let cut: String = text.chars().take(MAX_DISPLAY_TEXT_BYTES).collect();
		format!("{cut}\n[…{}ch elided…]", chars - MAX_DISPLAY_TEXT_BYTES)
	} else {
		text
	}
}

/// `display[1]:\n{...}` chunks for every JSON output.
pub fn format_display_outputs_for_text(outputs: &[EvalDisplayOutput]) -> String {
	let mut chunks = Vec::new();
	let mut index = 0;
	for output in outputs {
		if let EvalDisplayOutput::Json(value) = output {
			index += 1;
			chunks.push(format!("display[{index}]:\n{}", format_display_json_for_text(value)));
		}
	}
	chunks.join("\n\n")
}

fn count_newlines(bytes: &[u8]) -> usize {
	bytes.iter().filter(|byte| **byte == b'\n').count()
}

/// Bounded capture for one cell plus an optional full artifact copy.
pub struct OutputSink {
	text: String,
	total_bytes: u64,
	total_lines: u64,
	output_bytes: usize,
	output_lines: usize,
	truncated: bool,
	spill_threshold: usize,
	artifact: Option<Vec<u8>>,
	artifact_truncated: bool,
}

impl OutputSink {
	pub fn new(spill_threshold: usize, keep_artifact: bool) -> Self {
		Self {
			text: String::new(),
			total_bytes: 0,
			total_lines: 0,
			output_bytes: 0,
			output_lines: 0,
			truncated: false,
			spill_threshold: spill_threshold.max(1),
			artifact: keep_artifact.then(Vec::new),
			artifact_truncated: false,
		}
	}

	pub fn push(&mut self, chunk: &str) {
		if chunk.is_empty() {
			return;
		}
		self.total_bytes += chunk.len() as u64;
		self.total_lines += count_newlines(chunk.as_bytes()) as u64;
		if let Some(artifact) = &mut self.artifact {
			if artifact.len() + chunk.len() <= ARTIFACT_MAX_BYTES {
				artifact.extend_from_slice(chunk.as_bytes());
			} else {
				let room = ARTIFACT_MAX_BYTES.saturating_sub(artifact.len());
				let mut end = room;
				while end > 0 && !chunk.is_char_boundary(end) {
					end -= 1;
				}
				artifact.extend_from_slice(&chunk.as_bytes()[..end]);
				self.artifact_truncated = true;
			}
		}
		if self.text.len() >= self.spill_threshold {
			self.truncated = true;
			return;
		}
		let room = self.spill_threshold - self.text.len();
		if chunk.len() <= room {
			self.text.push_str(chunk);
		} else {
			let mut end = room;
			while end > 0 && !chunk.is_char_boundary(end) {
				end -= 1;
			}
			self.text.push_str(&chunk[..end]);
			self.truncated = true;
		}
		self.output_bytes = self.text.len();
		self.output_lines = count_newlines(self.text.as_bytes());
		if self.output_lines > MAX_OUTPUT_LINES {
			self.truncate_lines();
		}
	}

	fn truncate_lines(&mut self) {
		let mut seen = 0;
		let mut cut = self.text.len();
		for (index, byte) in self.text.bytes().enumerate() {
			if byte == b'\n' {
				seen += 1;
				if seen >= MAX_OUTPUT_LINES {
					cut = index;
					break;
				}
			}
		}
		self.text.truncate(cut);
		self.output_bytes = self.text.len();
		self.output_lines = count_newlines(self.text.as_bytes());
		self.truncated = true;
	}

	pub fn text(&self) -> &str {
		&self.text
	}

	pub fn truncated(&self) -> bool {
		self.truncated
	}

	pub fn artifact(&self) -> Option<&[u8]> {
		self.artifact.as_deref()
	}

	pub fn artifact_truncated(&self) -> bool {
		self.artifact_truncated
	}

	/// Final model-visible text with an honest truncation marker.
	pub fn finish_text(&self) -> String {
		if self.truncated {
			format!(
				"{}\n[output truncated: kept {} of {} bytes, {} lines total]",
				self.text.trim_end(),
				self.output_bytes,
				self.total_bytes,
				self.total_lines
			)
		} else {
			self.text.clone()
		}
	}

	pub fn meta(&self, artifact: Option<(String, Option<String>)>) -> OutputMeta {
		let (artifact_id, artifact_path) = match artifact {
			Some((id, path)) => (Some(id), path),
			None => (None, None),
		};
		OutputMeta {
			artifact_id,
			artifact_path,
			total_bytes: self.total_bytes,
			total_lines: self.total_lines,
			output_bytes: self.output_bytes,
			output_lines: self.output_lines,
			truncated: self.truncated,
		}
	}
}

fn decode_base64(data: &str) -> Option<Vec<u8>> {
	super::util::base64_decode(data)
}

/// Pixel dimensions for PNG/JPEG payloads (no decode of the whole image).
pub fn image_dimensions(data_base64: &str) -> Option<(u32, u32)> {
	let bytes = decode_base64(data_base64)?;
	if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) && bytes.len() >= 24 {
		let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
		let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
		return Some((width, height));
	}
	if bytes.starts_with(&[0xff, 0xd8]) {
		let mut index = 2;
		while index + 9 < bytes.len() {
			if bytes[index] != 0xff {
				index += 1;
				continue;
			}
			let marker = bytes[index + 1];
			let length = usize::from(u16::from_be_bytes([bytes[index + 2], bytes[index + 3]]));
			let is_sof = matches!(
				marker,
				0xc0 | 0xc1 | 0xc2 | 0xc3 | 0xc5 | 0xc6 | 0xc7 | 0xc9 | 0xca | 0xcb | 0xcd | 0xce | 0xcf
			);
			if is_sof && index + 9 <= bytes.len() {
				let height = u32::from(u16::from_be_bytes([bytes[index + 5], bytes[index + 6]]));
				let width = u32::from(u16::from_be_bytes([bytes[index + 7], bytes[index + 8]]));
				return Some((width, height));
			}
			if length < 2 {
				return None;
			}
			index += 2 + length;
		}
	}
	None
}

/// `display image N: WxH pixels` note, when dimensions are recoverable.
pub fn dimension_note(index: usize, mime_type: &str, data: &str) -> Option<String> {
	let (width, height) = image_dimensions(data)?;
	Some(format!("display image {index}: {width}x{height}px {mime_type}"))
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn status_bundle_bypasses_text() {
		let mut bundle = Map::new();
		bundle.insert(
			"application/x-omp-status".to_string(),
			json!({"op": "read", "path": "a.txt"}),
		);
		let rendered = render_display_bundle(&bundle);
		assert_eq!(rendered.text, "");
		assert_eq!(rendered.outputs.len(), 1);
		let EvalDisplayOutput::Status(event) = &rendered.outputs[0] else { panic!("status") };
		assert_eq!(event.op, "read");
		assert_eq!(event.fields.get("path").and_then(Value::as_str), Some("a.txt"));
	}

	#[test]
	fn markdown_wins_over_plain_and_json_is_separate() {
		let mut bundle = Map::new();
		bundle.insert("text/plain".to_string(), json!("plain"));
		bundle.insert("text/markdown".to_string(), json!("# title"));
		bundle.insert("application/json".to_string(), json!({"a": 1}));
		let rendered = render_display_bundle(&bundle);
		assert_eq!(rendered.text, "# title\n");
		assert!(rendered.outputs.iter().any(|output| matches!(output, EvalDisplayOutput::Json(_))));
		assert!(rendered.outputs.iter().any(|output| matches!(output, EvalDisplayOutput::Markdown { .. })));
	}

	#[test]
	fn json_text_cap_is_measured_in_chars() {
		let text = format_display_json_for_text(&json!("é".repeat(MAX_DISPLAY_TEXT_BYTES + 10)));
		assert!(text.contains("ch elided"));
		assert!(text.chars().count() <= MAX_DISPLAY_TEXT_BYTES + 30);
	}

	#[test]
	fn output_sink_marks_truncation_and_caps_lines() {
		let mut sink = OutputSink::new(8, false);
		sink.push("abcdefghij");
		assert!(sink.truncated());
		assert_eq!(sink.text(), "abcdefgh");
		assert!(sink.finish_text().contains("truncated"));
	}

	#[test]
	fn png_dimensions_are_read_from_ihdr() {
		let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
		png.extend_from_slice(&[0, 0, 0, 13]);
		png.extend_from_slice(b"IHDR");
		png.extend_from_slice(&7u32.to_be_bytes());
		png.extend_from_slice(&9u32.to_be_bytes());
		let encoded = super::super::util::base64_encode(&png);
		assert_eq!(image_dimensions(&encoded), Some((7, 9)));
	}
}
