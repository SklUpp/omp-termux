//! `inspect_image`: attach a local image file as vision-model image content.
//!
//! TS upstream (`packages/coding-agent/src/tools/inspect-image.ts`) loads the
//! file through `loadImageInput`, then sends it plus `question` to a *separate*
//! vision model (`completeSimple`) and returns only that model's text. Rust
//! `pi-agent` has no model registry, settings or API-key store, and
//! [`AgentContext`] carries no provider, so the completion half has no seam in
//! this crate. This tool performs the upstream half exactly — attachment
//! reference detection, path resolution, the 20 MiB gate, content sniffing and
//! the unsupported-format rejection — and returns the image as a
//! `details.images` content block. `loop_module::tool_image_parts_of` attaches
//! that block to the tool result message, so a vision-capable model receives
//! the same bytes TS would have uploaded.
//!
//! Build boundaries, reported instead of faked:
//! - `:img` on `.svg`/`.svgz` needs `resvg` rasterization; `resvg` lives only in
//!   `pi-natives` (a `cdylib`), so the call fails with the build-boundary text
//!   `read` uses.
//! - Resize / WebP re-encode needs a decoder; original bytes pass through.
//! - `Image #N` / `attachment://N` index per-turn attachments, which
//!   [`AgentContext`] does not carry, so they fail with the TS message for an
//!   empty attachment list.
//! - `details.model` / `details.usage` are `null`: no completion is issued.
//!
//! Sniffing mirrors `packages/utils/src/mime.ts` (`parseImageMetadata`).
//! `crates/omp-cli/src/file_mentions.rs::detect_image_mime` is the other Rust
//! sniffer but is unreachable from here: `omp-cli` depends on `pi-agent`.

use std::path::Path;
use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};

use super::{Tool, ToolError};
use crate::state::{AgentContext, ContentPart, ToolResult};

/// TS `MAX_IMAGE_INPUT_BYTES` (`utils/image-loading.ts`).
const MAX_IMAGE_INPUT_BYTES: usize = 20 * 1024 * 1024;
/// TS `DEFAULT_IMAGE_METADATA_HEADER_BYTES` (`packages/utils/src/mime.ts`).
const SNIFF_BYTES: usize = 256 * 1024;
const UNSUPPORTED_FORMAT: &str =
	"inspect_image only supports PNG, JPEG, GIF, and WEBP files detected by file content.";

/// TS `IMAGE_ATTACHMENT_REFERENCE_REGEX` (`tools/inspect-image.ts`).
static IMAGE_ATTACHMENT_REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
	Regex::new(r"(?i)^\s*(?:\[?Image #([1-9]\d*)(?:,[^\]\n]*)?\]?|(?:attachment|image)://([1-9]\d*))\s*$")
		.expect("attachment reference pattern is valid")
});

/// Header-derived image facts (TS `ImageMetadata`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImageMetadata {
	mime: &'static str,
	width: Option<u32>,
	height: Option<u32>,
	channels: Option<u8>,
	has_alpha: Option<bool>,
}

impl ImageMetadata {
	fn bare(mime: &'static str) -> Self {
		Self { mime, width: None, height: None, channels: None, has_alpha: None }
	}
}

/// Local `inspect_image` tool.
pub struct InspectImageTool;

impl InspectImageTool {
	#[must_use]
	pub fn new() -> Self {
		Self
	}
}

impl Default for InspectImageTool {
	fn default() -> Self {
		Self::new()
	}
}

#[async_trait]
impl Tool for InspectImageTool {
	fn name(&self) -> &str {
		"inspect_image"
	}

	fn description(&self) -> &str {
		"Attach a local PNG/JPEG/GIF/WEBP image file as image content for analysis; rejects unsupported formats and files over 20 MiB."
	}

	fn schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"path": {
					"type": "string",
					"description": "image file path, local .svg/.svgz path with :img, Image #N label, or attachment://N URI"
				},
				"question": { "type": "string", "description": "question about image" }
			},
			"required": ["path", "question"],
			"additionalProperties": false
		})
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let obj = args
			.as_object()
			.ok_or_else(|| ToolError::InvalidArguments("Arguments must be an object".to_string()))?;
		let path = obj
			.get("path")
			.and_then(Value::as_str)
			.ok_or_else(|| ToolError::InvalidArguments("Missing required parameter 'path'".to_string()))?;
		obj.get("question").and_then(Value::as_str).ok_or_else(|| {
			ToolError::InvalidArguments("Missing required parameter 'question'".to_string())
		})?;
		for key in obj.keys() {
			if key != "path" && key != "question" {
				return Err(ToolError::InvalidArguments(format!(
					"Unknown parameter '{key}': inspect_image accepts only 'path' and 'question'"
				)));
			}
		}

		if IMAGE_ATTACHMENT_REFERENCE.is_match(path) {
			return Err(ToolError::Rejected(format!(
				"No image attachments are available in this turn. path=\"{path}\" must be a readable file path or attachment URI."
			)));
		}

		// `splitPathAndSelPreferringLiteral`: a path that exists verbatim wins over
		// splitting its trailing `:img`.
		let (mut raw_path, mut img_selector) = strip_img_selector(path);
		if img_selector {
			let literal = super::read::resolve_path(ctx, &expand_tilde(path, ctx));
			if tokio::fs::metadata(&literal).await.is_ok() {
				raw_path = path;
				img_selector = false;
			}
		}
		if img_selector {
			let lowered = raw_path.to_ascii_lowercase();
			return Err(ToolError::Rejected(if lowered.ends_with(".svg") || lowered.ends_with(".svgz") {
				format!("Image/SVG rendering is not supported in this build: '{raw_path}'.")
			} else {
				"inspect_image ':img' only supports .svg and .svgz files.".to_string()
			}));
		}

		let target = super::read::resolve_path(ctx, &expand_tilde(raw_path, ctx));
		let stat = tokio::fs::metadata(&target)
			.await
			.map_err(|err| read_failure(&target, &err))?;
		if !stat.is_file() {
			return Err(ToolError::Rejected(format!(
				"Failed to read {}: not a regular file",
				target.display()
			)));
		}
		if stat.len() > MAX_IMAGE_INPUT_BYTES as u64 {
			return Err(too_large(stat.len()));
		}

		let (bytes, size) = super::read_capped(&target, MAX_IMAGE_INPUT_BYTES)
			.await
			.map_err(|err| read_failure(&target, &err))?;
		if size > MAX_IMAGE_INPUT_BYTES as u64 {
			return Err(too_large(size));
		}

		let metadata = sniff_image_metadata(&bytes[..bytes.len().min(SNIFF_BYTES)])
			.ok_or_else(|| ToolError::Rejected(UNSUPPORTED_FORMAT.to_string()))?;
		let mime = metadata.mime;
		let image = ContentPart::image(crate::eval::util::base64_encode(&bytes), mime)
			.map_err(|err| ToolError::ExecutionFailed(format!("image payload rejected: {err}")))?;
		let images = serde_json::to_value(vec![image])
			.map_err(|err| ToolError::ExecutionFailed(err.to_string()))?;

		let mut details = json!({
			"model": null,
			"imagePath": target.display().to_string(),
			"mimeType": mime,
			"usage": null,
			"bytes": size,
			"images": images,
		});
		if let (Some(width), Some(height)) = (metadata.width, metadata.height) {
			details["width"] = json!(width);
			details["height"] = json!(height);
		}
		if let Some(channels) = metadata.channels {
			details["channels"] = json!(channels);
		}
		if let Some(has_alpha) = metadata.has_alpha {
			details["hasAlpha"] = json!(has_alpha);
		}

		Ok(ToolResult {
			tool_call_id: String::new(),
			content: format!("Read image file [{mime}]"),
			is_error: false,
			terminate: false,
			details: Some(details),
		})
	}
}

/// Literal `:img` suffix split (TS `splitPathAndSelPreferringLiteral` narrowed
/// to the only selector this tool honours).
fn strip_img_selector(raw: &str) -> (&str, bool) {
	let bytes = raw.as_bytes();
	if bytes.len() > 4 && bytes[bytes.len() - 4..].eq_ignore_ascii_case(b":img") {
		(&raw[..raw.len() - 4], true)
	} else {
		(raw, false)
	}
}

/// `expandTilde` (`path-utils.ts`), byte-identical to `read`'s private helper.
fn expand_tilde(raw: &str, ctx: &AgentContext) -> String {
	let Some(home) = ctx.env.get("HOME").filter(|home| !home.is_empty()) else {
		return raw.to_string();
	};
	if raw == "~" {
		return home.clone();
	}
	if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
		return format!("{home}/{rest}");
	}
	if let Some(rest) = raw.strip_prefix('~') {
		return Path::new(home).join(rest).to_string_lossy().into_owned();
	}
	raw.to_string()
}

fn read_failure(target: &Path, err: &std::io::Error) -> ToolError {
	ToolError::Rejected(format!("Failed to read {}: {err}", target.display()))
}

/// TS `ImageInputTooLargeError` message.
fn too_large(bytes: u64) -> ToolError {
	ToolError::Rejected(format!(
		"Image file too large: {} exceeds {} limit.",
		super::read::format_bytes(bytes),
		super::read::format_bytes(MAX_IMAGE_INPUT_BYTES as u64)
	))
}

/// TS `parseImageMetadata`: first matching format wins.
fn sniff_image_metadata(header: &[u8]) -> Option<ImageMetadata> {
	parse_png(header)
		.or_else(|| parse_jpeg(header))
		.or_else(|| parse_gif(header))
		.or_else(|| parse_webp(header))
}

fn parse_png(header: &[u8]) -> Option<ImageMetadata> {
	const MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
	if !header.starts_with(&MAGIC) {
		return None;
	}
	if header.len() < 16 || &header[12..16] != b"IHDR" || header.len() < 26 {
		return Some(ImageMetadata::bare("image/png"));
	}
	let width = u32::from_be_bytes([header[16], header[17], header[18], header[19]]);
	let height = u32::from_be_bytes([header[20], header[21], header[22], header[23]]);
	let (channels, has_alpha) = match header[25] {
		0 => (Some(1), Some(false)),
		2 => (Some(3), Some(false)),
		3 => (Some(3), None),
		4 => (Some(2), Some(true)),
		6 => (Some(4), Some(true)),
		_ => (None, None),
	};
	Some(ImageMetadata {
		mime: "image/png",
		width: Some(width),
		height: Some(height),
		channels,
		has_alpha,
	})
}

fn parse_jpeg(header: &[u8]) -> Option<ImageMetadata> {
	const MAGIC: [u8; 3] = [0xff, 0xd8, 0xff];
	if !header.starts_with(&MAGIC) {
		return None;
	}
	if header.len() < 4 {
		return Some(ImageMetadata::bare("image/jpeg"));
	}
	let mut offset = 2usize;
	while offset + 9 < header.len() {
		if header[offset] != 0xff {
			offset += 1;
			continue;
		}
		let mut marker_offset = offset + 1;
		while marker_offset < header.len() && header[marker_offset] == 0xff {
			marker_offset += 1;
		}
		if marker_offset >= header.len() {
			break;
		}
		let marker = header[marker_offset];
		let segment_offset = marker_offset + 1;
		if marker == 0xd8 || marker == 0xd9 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
			offset = segment_offset;
			continue;
		}
		if segment_offset + 1 >= header.len() {
			break;
		}
		let segment_length =
			u16::from_be_bytes([header[segment_offset], header[segment_offset + 1]]) as usize;
		if segment_length < 2 {
			break;
		}
		let is_start_of_frame =
			(0xc0..=0xcf).contains(&marker) && marker != 0xc4 && marker != 0xc8 && marker != 0xcc;
		if is_start_of_frame {
			if segment_offset + 7 >= header.len() {
				break;
			}
			let height =
				u16::from_be_bytes([header[segment_offset + 3], header[segment_offset + 4]]) as u32;
			let width =
				u16::from_be_bytes([header[segment_offset + 5], header[segment_offset + 6]]) as u32;
			return Some(ImageMetadata {
				mime: "image/jpeg",
				width: Some(width),
				height: Some(height),
				channels: Some(header[segment_offset + 7]),
				has_alpha: Some(false),
			});
		}
		offset = segment_offset + segment_length;
	}
	Some(ImageMetadata::bare("image/jpeg"))
}

fn parse_gif(header: &[u8]) -> Option<ImageMetadata> {
	if !header.starts_with(b"GIF87a") && !header.starts_with(b"GIF89a") {
		return None;
	}
	if header.len() < 10 {
		return Some(ImageMetadata::bare("image/gif"));
	}
	Some(ImageMetadata {
		mime: "image/gif",
		width: Some(u32::from(u16::from_le_bytes([header[6], header[7]]))),
		height: Some(u32::from(u16::from_le_bytes([header[8], header[9]]))),
		channels: Some(3),
		has_alpha: None,
	})
}

fn parse_webp(header: &[u8]) -> Option<ImageMetadata> {
	if header.len() < 12 || &header[0..4] != b"RIFF" || &header[8..12] != b"WEBP" {
		return None;
	}
	if header.len() < 30 {
		return Some(ImageMetadata::bare("image/webp"));
	}
	if &header[12..16] == b"VP8X" {
		let has_alpha = (header[20] & 0x10) != 0;
		let width =
			(u32::from(header[24]) | (u32::from(header[25]) << 8) | (u32::from(header[26]) << 16))
				+ 1;
		let height =
			(u32::from(header[27]) | (u32::from(header[28]) << 8) | (u32::from(header[29]) << 16))
				+ 1;
		return Some(ImageMetadata {
			mime: "image/webp",
			width: Some(width),
			height: Some(height),
			channels: Some(if has_alpha { 4 } else { 3 }),
			has_alpha: Some(has_alpha),
		});
	}
	if &header[12..16] == b"VP8L" {
		if header.len() < 25 {
			return Some(ImageMetadata::bare("image/webp"));
		}
		let bits = u32::from_le_bytes([header[21], header[22], header[23], header[24]]);
		let has_alpha = (bits >> 28) & 1 == 1;
		return Some(ImageMetadata {
			mime: "image/webp",
			width: Some((bits & 0x3fff) + 1),
			height: Some(((bits >> 14) & 0x3fff) + 1),
			channels: Some(if has_alpha { 4 } else { 3 }),
			has_alpha: Some(has_alpha),
		});
	}
	if &header[12..16] == b"VP8 " {
		return Some(ImageMetadata {
			mime: "image/webp",
			width: Some(u32::from(u16::from_le_bytes([header[26], header[27]]) & 0x3fff)),
			height: Some(u32::from(u16::from_le_bytes([header[28], header[29]]) & 0x3fff)),
			channels: Some(3),
			has_alpha: Some(false),
		});
	}
	Some(ImageMetadata::bare("image/webp"))
}

#[cfg(test)]
mod tests {
	use std::path::PathBuf;
	use std::time::{SystemTime, UNIX_EPOCH};

	use super::*;

	fn temp_dir(label: &str) -> PathBuf {
		let nanos = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
		let dir = std::env::temp_dir().join(format!(
			"omp-inspect-image-{label}-{nanos}-{}",
			std::process::id()
		));
		std::fs::create_dir_all(&dir).expect("temp dir");
		dir
	}

	/// PNG magic + IHDR (1x1, RGBA) — enough for content sniffing, no decoder.
	fn png_header_with_ihdr() -> Vec<u8> {
		let mut png = Vec::new();
		png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
		png.extend_from_slice(&[0, 0, 0, 13]);
		png.extend_from_slice(b"IHDR");
		png.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
		png
	}

	#[tokio::test]
	async fn returns_image_content_block_with_ts_metadata() {
		let dir = temp_dir("ok");
		let path = dir.join("pixel.png");
		let png = png_header_with_ihdr();
		std::fs::write(&path, &png).expect("write png");
		let ctx = AgentContext::new(dir.clone());

		let result = InspectImageTool::new()
			.execute(
				&json!({ "path": path.display().to_string(), "question": "What is visible?" }),
				&ctx,
			)
			.await
			.expect("inspect_image");
		assert_eq!(result.content, "Read image file [image/png]");
		assert!(!result.is_error);

		let details = result.details.clone().expect("details");
		let expected_path = path.display().to_string();
		assert_eq!(details["imagePath"].as_str(), Some(expected_path.as_str()));
		assert_eq!(details["mimeType"].as_str(), Some("image/png"));
		assert_eq!(details["width"].as_u64(), Some(1));
		assert_eq!(details["height"].as_u64(), Some(1));
		assert_eq!(details["channels"].as_u64(), Some(4));
		assert_eq!(details["hasAlpha"].as_bool(), Some(true));
		assert_eq!(details["bytes"].as_u64(), Some(png.len() as u64));
		let expected_data = crate::eval::util::base64_encode(&png);
		assert_eq!(details["images"][0]["data"].as_str(), Some(expected_data.as_str()));
		assert_eq!(details["images"][0]["mimeType"].as_str(), Some("image/png"));

		let parts = crate::loop_module::tool_image_parts_of(&result).expect("consumer accepts block");
		assert_eq!(parts.len(), 1);
		assert!(parts[0].is_image());
		std::fs::remove_dir_all(&dir).expect("cleanup");
	}

	#[tokio::test]
	async fn rejects_bad_args_attachments_and_non_images() {
		let dir = temp_dir("reject");
		let ctx = AgentContext::new(dir.clone());
		let tool = InspectImageTool::new();

		let extra = tool
			.execute(&json!({ "path": "a.png", "question": "q", "extra": 1 }), &ctx)
			.await;
		assert!(matches!(extra, Err(ToolError::InvalidArguments(_))));

		let attachment = tool.execute(&json!({ "path": "Image #1", "question": "q" }), &ctx).await;
		assert!(matches!(
			attachment,
			Err(ToolError::Rejected(message))
				if message.contains("No image attachments are available in this turn")
		));

		let text = dir.join("notes.png");
		std::fs::write(&text, b"not an image").expect("write text");
		let unsupported = tool
			.execute(&json!({ "path": text.display().to_string(), "question": "q" }), &ctx)
			.await;
		assert!(matches!(
			unsupported,
			Err(ToolError::Rejected(message))
				if message.contains("only supports PNG, JPEG, GIF, and WEBP")
		));

		let img_selector = tool
			.execute(&json!({ "path": "photo.png:img", "question": "q" }), &ctx)
			.await;
		assert!(matches!(
			img_selector,
			Err(ToolError::Rejected(message))
				if message == "inspect_image ':img' only supports .svg and .svgz files."
		));

		let missing = tool
			.execute(&json!({ "path": "missing.png", "question": "q" }), &ctx)
			.await;
		assert!(matches!(missing, Err(ToolError::Rejected(_))));
		std::fs::remove_dir_all(&dir).expect("cleanup");
	}
}
