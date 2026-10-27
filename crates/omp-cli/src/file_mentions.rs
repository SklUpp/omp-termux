//! Auto-read file mentions from user prompts (@file syntax).
//!
//! Mirrors `packages/coding-agent/src/utils/file-mentions.ts`:
//! - Extracts `@filepath`, `@"path with spaces"`, and `@'path with spaces'`.
//! - Respects boundaries: leading whitespace/delimiters, skips emails like `user@example.com`.
//! - Sanitizes punctuation surrounding unquoted mentions.
//! - Enforces 5 MiB text / 25 MiB image limits via bounded reads (even when files change/grow).
//! - Sniffs image MIME types by content (PNG, JPEG, GIF, WebP).
//! - Sniffs binary files (NUL bytes or invalid UTF-8) and skips injecting raw bytes.
//! - Renders directory listings up to 500 entries with case-insensitive sorting and relative ages.
//! - Emits structured [`FileMentionMessage`] and prompt block representations.

use std::collections::BinaryHeap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use serde::{Deserialize, Serialize};

use crate::dirs;

/// Maximum file size for auto-reading text files (5 MiB).
pub const MAX_AUTO_READ_TEXT_BYTES: u64 = 5 * 1024 * 1024;

/// Maximum file size for auto-reading image files (25 MiB).
pub const MAX_AUTO_READ_IMAGE_BYTES: u64 = 25 * 1024 * 1024;

/// Inline preview byte budget before head-truncation (50 KiB).
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

/// Inline preview line budget before head-truncation (3000 lines).
pub const DEFAULT_MAX_LINES: usize = 3000;

/// Directory listing entry limit (500 entries).
pub const DEFAULT_DIR_LIMIT: usize = 500;

/// Window sniffed for binary content classification (8 KiB, mirrors git).
pub const BINARY_SNIFF_BYTES: usize = 8192;

/// Standard base64 index table (RFC 4648).
const BASE64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Image payload compatible with multimodal model requests and session wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePayload {
	#[serde(default = "default_image_type", rename = "type")]
	pub r#type: String,
	#[serde(rename = "mimeType", alias = "mime_type")]
	pub mime_type: String,
	#[serde(rename = "data", alias = "data_base64")]
	pub data_base64: String,
}

fn default_image_type() -> String {
	"image".to_string()
}

impl ImagePayload {
	#[must_use]
	pub fn new(mime_type: impl Into<String>, data_base64: impl Into<String>) -> Self {
		Self {
			r#type: "image".to_string(),
			mime_type: mime_type.into(),
			data_base64: data_base64.into(),
		}
	}
}

/// Reason why a file's content was skipped from auto-reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SkippedReason {
	TooLarge,
	Binary,
	Unreadable,
}

/// A mentioned file or directory with its content, metadata, and optional image payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MentionedFile {
	pub path: String,
	pub content: String,
	#[serde(default, skip_serializing_if = "Option::is_none", alias = "line_count")]
	pub line_count: Option<usize>,
	#[serde(default, skip_serializing_if = "Option::is_none", alias = "byte_size")]
	pub byte_size: Option<u64>,
	#[serde(default, skip_serializing_if = "Option::is_none", alias = "skipped_reason")]
	pub skipped_reason: Option<SkippedReason>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub image: Option<ImagePayload>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMentionMessage {
	pub role: String,
	pub files: Vec<MentionedFile>,
	pub timestamp: u64,
}

impl FileMentionMessage {
	/// Returns references to files that are text or directory listings (no image payload).
	#[must_use]
	pub fn text_files(&self) -> Vec<&MentionedFile> {
		self.files.iter().filter(|f| f.image.is_none()).collect()
	}

	/// Returns references to files that have an image payload.
	#[must_use]
	pub fn image_files(&self) -> Vec<&MentionedFile> {
		self.files.iter().filter(|f| f.image.is_some()).collect()
	}

	/// Formats only the text files into prompt blocks.
	#[must_use]
	pub fn format_text_prompt_block(&self) -> String {
		let mut out = String::new();
		for file in self.text_files() {
			if !out.is_empty() {
				out.push('\n');
			}
			let inner = if file.content.is_empty() {
				"\n".to_string()
			} else {
				format!("\n{}\n", file.content)
			};
			out.push_str(&format!("<file path=\"{}\">{}</file>", file.path, inner));
		}
		out
	}

	/// Formats only the image files into prompt blocks.
	#[must_use]
	pub fn format_image_prompt_block(&self) -> String {
		let mut out = String::new();
		for file in self.image_files() {
			if !out.is_empty() {
				out.push('\n');
			}
			let inner = if file.content.is_empty() {
				"\n".to_string()
			} else {
				format!("\n{}\n", file.content)
			};
			out.push_str(&format!("<file path=\"{}\">{}</file>", file.path, inner));
		}
		out
	}
}

/// Result of formatting and truncating text output for inline prompt injection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextOutput {
	pub output: String,
	pub line_count: usize,
}

/// Checks if byte index `index` in `text` is a valid mention boundary.
///
/// In TS: index 0 or preceding char in `[\s([{<"'`]`.
#[must_use]
pub fn is_mention_boundary(text: &str, index: usize) -> bool {
	if index == 0 {
		return true;
	}
	if let Some(prev_char) = text[..index].chars().next_back() {
		prev_char.is_whitespace() || matches!(prev_char, '(' | '[' | '{' | '<' | '"' | '\'' | '`')
	} else {
		true
	}
}

/// Sanitizes an unquoted raw mention path by stripping surrounding punctuation.
///
/// In TS:
/// - Strips leading punctuation: `[`"'([{<]+`
/// - Strips trailing punctuation: `[)\]}>.,;:!?"'`]+$`
#[must_use]
pub fn sanitize_mention_path(raw_path: &str) -> Option<String> {
	let mut cleaned = raw_path.trim();
	cleaned = cleaned.trim_start_matches(|c: char| matches!(c, '`' | '"' | '\'' | '(' | '[' | '{' | '<'));
	cleaned = cleaned.trim_end_matches(|c: char| {
		matches!(c, ')' | ']' | '}' | '>' | '.' | ',' | ';' | ':' | '!' | '?' | '"' | '\'' | '`')
	});
	cleaned = cleaned.trim();
	if cleaned.is_empty() {
		None
	} else {
		Some(cleaned.to_string())
	}
}

/// Extracts all unique `@filepath` mentions from text, preserving first appearance order.
#[must_use]
pub fn extract_file_mentions(text: &str) -> Vec<String> {
	let mut mentions = Vec::new();
	let mut seen = std::collections::HashSet::new();

	let mut chars = text.char_indices().peekable();
	while let Some((idx, ch)) = chars.next() {
		if ch == '@' && is_mention_boundary(text, idx) {
			let rest = &text[idx + 1..];
			let mut matched_quoted = false;
			if let Some(stripped) = rest.strip_prefix('"') {
				if let Some(end) = stripped.find('"') {
					matched_quoted = true;
					let inner = stripped[..end].trim();
					if !inner.is_empty() && seen.insert(inner.to_string()) {
						mentions.push(inner.to_string());
					}
					let advance_to = idx + 1 + 1 + end + 1;
					while let Some(&(next_idx, _)) = chars.peek() {
						if next_idx < advance_to {
							chars.next();
						} else {
							break;
						}
					}
				}
			} else if let Some(stripped) = rest.strip_prefix('\'') {
				if let Some(end) = stripped.find('\'') {
					matched_quoted = true;
					let inner = stripped[..end].trim();
					if !inner.is_empty() && seen.insert(inner.to_string()) {
						mentions.push(inner.to_string());
					}
					let advance_to = idx + 1 + 1 + end + 1;
					while let Some(&(next_idx, _)) = chars.peek() {
						if next_idx < advance_to {
							chars.next();
						} else {
							break;
						}
					}
				}
			}

			if !matched_quoted {
				let token_end = rest
					.find(|c: char| c.is_whitespace() || c == '@')
					.unwrap_or(rest.len());
				let raw_token = &rest[..token_end];
				if let Some(cleaned) = sanitize_mention_path(raw_token) {
					if seen.insert(cleaned.clone()) {
						mentions.push(cleaned);
					}
				}
				let advance_to = idx + 1 + token_end;
				while let Some(&(next_idx, _)) = chars.peek() {
					if next_idx < advance_to {
						chars.next();
					} else {
						break;
					}
				}
			}
		}
	}

	mentions
}

/// Detects supported image MIME types by content magic bytes: PNG, JPEG, GIF, WebP.
#[must_use]
pub fn detect_image_mime(header: &[u8]) -> Option<&'static str> {
	// PNG: \x89PNG\r\n\x1a\n (8 bytes)
	const PNG_MAGIC: &[u8; 8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
	if header.starts_with(PNG_MAGIC) {
		return Some("image/png");
	}

	// JPEG: \xff\xd8\xff (3 bytes)
	if header.len() >= 3 && header[0] == 0xFF && header[1] == 0xD8 && header[2] == 0xFF {
		return Some("image/jpeg");
	}

	// GIF: GIF87a or GIF89a (6 bytes)
	if header.starts_with(b"GIF87a") || header.starts_with(b"GIF89a") {
		return Some("image/gif");
	}

	// WebP: RIFF????WEBP (12 bytes)
	if header.len() >= 12 && &header[0..4] == b"RIFF" && &header[8..12] == b"WEBP" {
		return Some("image/webp");
	}

	None
}

/// Sniffs up to [`BINARY_SNIFF_BYTES`] to classify content as binary (contains NUL or invalid UTF-8).
#[must_use]
pub fn is_probably_binary(bytes: &[u8]) -> bool {
	let sniff_len = bytes.len().min(BINARY_SNIFF_BYTES);
	let header = &bytes[..sniff_len];
	if header.contains(&0) {
		return true;
	}
	match std::str::from_utf8(header) {
		Ok(_) => false,
		Err(err) => err.error_len().is_some(),
	}
}

/// Encodes a byte slice into a standard RFC 4648 Base64 string.
#[must_use]
pub fn base64_encode(data: &[u8]) -> String {
	let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
	for chunk in data.chunks(3) {
		let b0 = chunk[0] as usize;
		let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
		let b2 = chunk.get(2).copied().unwrap_or(0) as usize;
		let n = (b0 << 16) | (b1 << 8) | b2;
		out.push(BASE64_ALPHABET[(n >> 18) & 63] as char);
		out.push(BASE64_ALPHABET[(n >> 12) & 63] as char);
		if chunk.len() > 1 {
			out.push(BASE64_ALPHABET[(n >> 6) & 63] as char);
		} else {
			out.push('=');
		}
		if chunk.len() > 2 {
			out.push(BASE64_ALPHABET[n & 63] as char);
		} else {
			out.push('=');
		}
	}
	out
}

/// Formats byte count matching TypeScript `formatBytes`.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
	if bytes < 1024 {
		format!("{bytes}B")
	} else if bytes < 1024 * 1024 {
		format!("{:.1}KB", bytes as f64 / 1024.0)
	} else if bytes < 1024 * 1024 * 1024 {
		format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
	} else {
		format!("{:.1}GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
	}
}

/// Formats age in seconds matching TypeScript `formatAge`.
#[must_use]
pub fn format_age(age_seconds: u64) -> String {
	if age_seconds == 0 {
		return String::new();
	}
	let mins = age_seconds / 60;
	let hours = mins / 60;
	let days = hours / 24;
	let weeks = days / 7;
	let months = days / 30;

	if months > 0 {
		format!("{months}mo ago")
	} else if weeks > 0 {
		format!("{weeks}w ago")
	} else if days > 0 {
		format!("{days}d ago")
	} else if hours > 0 {
		format!("{hours}h ago")
	} else if mins > 0 {
		format!("{mins}m ago")
	} else {
		"just now".to_string()
	}
}

/// Truncates a UTF-8 string up to `max_bytes`, stopping at a character boundary.
#[must_use]
fn truncate_utf8_head(s: &str, max_bytes: usize) -> &str {
	if s.len() <= max_bytes {
		return s;
	}
	let mut end = max_bytes;
	while end > 0 && !s.is_char_boundary(end) {
		end -= 1;
	}
	&s[..end]
}

/// Builds text output applying head truncation rules and notices (mirrors TS `buildTextOutput`).
#[must_use]
pub fn build_text_output(text_content: &str) -> TextOutput {
	let lines: Vec<&str> = text_content.split('\n').collect();
	let total_file_lines = lines.len();

	let first_line = lines.first().copied().unwrap_or("");
	let first_line_bytes = first_line.len();

	if first_line_bytes > DEFAULT_MAX_BYTES {
		let snippet = truncate_utf8_head(first_line, DEFAULT_MAX_BYTES);
		let output = if !snippet.is_empty() {
			format!(
				"{snippet}\n\n[Line 1 is {}, exceeds {} limit. Showing first {} of the line.]",
				format_bytes(first_line_bytes as u64),
				format_bytes(DEFAULT_MAX_BYTES as u64),
				format_bytes(snippet.len() as u64)
			)
		} else {
			format!(
				"[Line 1 is {}, exceeds {} limit. Unable to display a valid UTF-8 snippet.]",
				format_bytes(first_line_bytes as u64),
				format_bytes(DEFAULT_MAX_BYTES as u64)
			)
		};
		return TextOutput {
			output,
			line_count: total_file_lines,
		};
	}

	let total_bytes = text_content.len();
	if total_file_lines <= DEFAULT_MAX_LINES && total_bytes <= DEFAULT_MAX_BYTES {
		return TextOutput {
			output: text_content.to_string(),
			line_count: total_file_lines,
		};
	}

	let mut included_lines = 0;
	let mut bytes_used = 0;
	let mut end_idx = 0;

	for (idx, line) in lines.iter().enumerate() {
		if included_lines >= DEFAULT_MAX_LINES {
			break;
		}
		let sep_bytes = if included_lines > 0 { 1 } else { 0 };
		if bytes_used + sep_bytes + line.len() > DEFAULT_MAX_BYTES {
			break;
		}
		bytes_used += sep_bytes + line.len();
		included_lines += 1;
		end_idx = if idx == 0 { line.len() } else { end_idx + 1 + line.len() };
	}

	let mut output = text_content[..end_idx].to_string();
	let next_offset = included_lines + 1;
	output.push_str(&format!(
		"\n\n[Showing lines 1-{included_lines} of {total_file_lines}. Use :{next_offset} to continue]"
	));

	TextOutput {
		output,
		line_count: total_file_lines,
	}
}

/// Builds directory listing formatted with relative age and `/` suffix (mirrors TS `buildDirectoryListing`).
#[derive(Debug)]
struct DirItem {
	sort_key: String,
	name: String,
	is_dir: bool,
	age_seconds: Option<u64>,
}

impl Ord for DirItem {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		self.sort_key.cmp(&other.sort_key)
	}
}

impl PartialOrd for DirItem {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
		Some(self.cmp(other))
	}
}

impl PartialEq for DirItem {
	fn eq(&self, other: &Self) -> bool {
		self.sort_key == other.sort_key
	}
}

impl Eq for DirItem {}

pub fn build_directory_listing(dir_path: &Path) -> std::io::Result<TextOutput> {
	let read_dir = match std::fs::read_dir(dir_path) {
		Ok(rd) => rd,
		Err(_) => {
			return Ok(TextOutput {
				output: "(empty directory)".to_string(),
				line_count: 1,
			});
		}
	};

	let mut heap: BinaryHeap<DirItem> = BinaryHeap::with_capacity(DEFAULT_DIR_LIMIT);
	let mut entry_limit_reached = false;
	let now = SystemTime::now();

	for entry in read_dir.filter_map(std::result::Result::ok) {
		let name = entry.file_name().to_string_lossy().to_string();
		let metadata = match entry.metadata() {
			Ok(m) => m,
			Err(_) => continue,
		};

		let is_dir = metadata.is_dir();
		let age_seconds = metadata
			.modified()
			.ok()
			.and_then(|m| now.duration_since(m).ok().map(|d| d.as_secs()));

		let item = DirItem {
			sort_key: name.to_lowercase(),
			name,
			is_dir,
			age_seconds,
		};

		if heap.len() < DEFAULT_DIR_LIMIT {
			heap.push(item);
		} else if let Some(mut top) = heap.peek_mut() {
			entry_limit_reached = true;
			if item.sort_key < top.sort_key {
				*top = item;
			}
		}
	}

	if heap.is_empty() {
		return Ok(TextOutput {
			output: "(empty directory)".to_string(),
			line_count: 1,
		});
	}

	let sorted_items = heap.into_sorted_vec();
	let mut results = Vec::with_capacity(sorted_items.len());

	for item in sorted_items {
		let suffix = if item.is_dir { "/" } else { "" };
		let age_str = match item.age_seconds {
			Some(secs) => {
				let age = format_age(secs);
				if !age.is_empty() {
					format!(" ({age})")
				} else {
					String::new()
				}
			}
			None => String::new(),
		};
		results.push(format!("{}{suffix}{age_str}", item.name));
	}
	if results.is_empty() {
		return Ok(TextOutput {
			output: "(empty directory)".to_string(),
			line_count: 1,
		});
	}

	let mut raw_output = String::new();
	let mut truncated = false;
	let mut used_bytes = 0;

	for (idx, line) in results.iter().enumerate() {
		let sep_len = if idx > 0 { 1 } else { 0 };
		if used_bytes + sep_len + line.len() > DEFAULT_MAX_BYTES {
			truncated = true;
			break;
		}
		if idx > 0 {
			raw_output.push('\n');
		}
		raw_output.push_str(line);
		used_bytes += sep_len + line.len();
	}

	let mut notices = Vec::new();
	if entry_limit_reached {
		notices.push(format!(
			"{DEFAULT_DIR_LIMIT} entries limit reached. Use limit={} for more",
			DEFAULT_DIR_LIMIT * 2
		));
	}
	if truncated {
		notices.push(format!("{} limit reached", format_bytes(DEFAULT_MAX_BYTES as u64)));
	}
	if !notices.is_empty() {
		raw_output.push_str(&format!("\n\n[{}]", notices.join(". ")));
	}

	let line_count = raw_output.split('\n').count();
	Ok(TextOutput {
		output: raw_output,
		line_count,
	})
}

/// Resolves a mention path relative to `cwd` with tilde expansion.
#[must_use]
pub fn resolve_path(file_path: &str, cwd: &Path) -> PathBuf {
	if file_path.is_empty() || file_path.chars().all(|c| c == '/') {
		return cwd.to_path_buf();
	}
	if let Some(rest) = file_path.strip_prefix("~/") {
		if let Some(home) = dirs::home_dir() {
			return home.join(rest);
		}
	} else if file_path == "~" {
		if let Some(home) = dirs::home_dir() {
			return home;
		}
	}
	let p = Path::new(file_path);
	if p.is_absolute() {
		p.to_path_buf()
	} else {
		cwd.join(p)
	}
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "netbsd", target_os = "openbsd", target_os = "dragonfly"))))]
const O_NONBLOCK: i32 = 0o4000;

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "netbsd", target_os = "openbsd", target_os = "dragonfly"))]
const O_NONBLOCK: i32 = 0x0004;

#[cfg(unix)]
fn open_file_safe(path: &Path) -> std::io::Result<File> {
	let mut options = std::fs::OpenOptions::new();
	options.read(true);
	options.custom_flags(O_NONBLOCK);
	options.open(path)
}

#[cfg(not(unix))]
fn open_file_safe(path: &Path) -> std::io::Result<File> {
	File::open(path)
}

/// Error returned when bounded reading halts or fails.
#[derive(Debug, PartialEq, Eq)]
pub enum BoundedReadError {
	TooLarge { actual_read: u64 },
	Io(std::io::ErrorKind),
}

/// Safely reads up to `max_bytes` from `reader`, prepending any already-buffered `initial_buf`.
///
/// Halts reading as soon as total bytes exceed `max_bytes`, guaranteeing memory safety
/// even when reading unbounded streams (e.g., `/dev/zero`) or concurrently growing files.
/// Total buffered memory is strictly capped at `max_bytes + 1`.
pub fn read_bounded<R: Read>(
	mut reader: R,
	initial_buf: Vec<u8>,
	max_bytes: u64,
) -> Result<Vec<u8>, BoundedReadError> {
	let initial_len = initial_buf.len() as u64;
	if initial_len > max_bytes {
		return Err(BoundedReadError::TooLarge { actual_read: initial_len });
	}

	let remaining_limit = (max_bytes - initial_len) + 1;
	let mut full_buffer = initial_buf;
	let mut bounded_reader = (&mut reader).take(remaining_limit);

	let mut chunk = [0u8; 64 * 1024];
	loop {
		match bounded_reader.read(&mut chunk) {
			Ok(0) => break,
			Ok(n) => {
				full_buffer.extend_from_slice(&chunk[..n]);
				if full_buffer.len() as u64 > max_bytes {
					return Err(BoundedReadError::TooLarge {
						actual_read: full_buffer.len() as u64,
					});
				}
			}
			Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
			Err(err) => return Err(BoundedReadError::Io(err.kind())),
		}
	}

	Ok(full_buffer)
}

/// Safely reads a single mentioned path enforcing limits, content classification, and bounded buffers.
///
/// Returns `None` if the path does not exist on disk (treating it as prose, not a file reference).
#[must_use]
pub fn read_mentioned_path(file_path: &str, cwd: &Path) -> Option<MentionedFile> {
	let abs_path = resolve_path(file_path, cwd);
	let metadata = match abs_path.metadata() {
		Ok(m) => m,
		Err(_) => return None,
	};

	if metadata.is_dir() {
		let listing = build_directory_listing(&abs_path).unwrap_or_else(|_| TextOutput {
			output: "(empty directory)".to_string(),
			line_count: 1,
		});
		return Some(MentionedFile {
			path: file_path.to_string(),
			content: listing.output,
			line_count: Some(listing.line_count),
			byte_size: None,
			skipped_reason: None,
			image: None,
		});
	}

	let file = match open_file_safe(&abs_path) {
		Ok(f) => f,
		Err(err) => {
			return Some(MentionedFile {
				path: file_path.to_string(),
				content: format!("(skipped auto-read: unreadable: {err})"),
				line_count: None,
				byte_size: None,
				skipped_reason: Some(SkippedReason::Unreadable),
				image: None,
			});
		}
	};

	let opened_meta = match file.metadata() {
		Ok(m) => m,
		Err(err) => {
			return Some(MentionedFile {
				path: file_path.to_string(),
				content: format!("(skipped auto-read: unreadable: {err})"),
				line_count: None,
				byte_size: None,
				skipped_reason: Some(SkippedReason::Unreadable),
				image: None,
			});
		}
	};

	if !opened_meta.is_file() {
		return Some(MentionedFile {
			path: file_path.to_string(),
			content: "(skipped auto-read: not a regular file)".to_string(),
			line_count: None,
			byte_size: None,
			skipped_reason: Some(SkippedReason::Unreadable),
			image: None,
		});
	}

	let mut file = file;

	// Read initial header window to classify format (image vs text) and binary status
	let mut initial_buf = vec![0u8; BINARY_SNIFF_BYTES];
	let mut initial_bytes_read = 0;
	while initial_bytes_read < initial_buf.len() {
		match file.read(&mut initial_buf[initial_bytes_read..]) {
			Ok(0) => break,
			Ok(n) => initial_bytes_read += n,
			Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
			Err(err) => {
				return Some(MentionedFile {
					path: file_path.to_string(),
					content: format!("(skipped auto-read: unreadable: {err})"),
					line_count: None,
					byte_size: None,
					skipped_reason: Some(SkippedReason::Unreadable),
					image: None,
				});
			}
		}
	}
	initial_buf.truncate(initial_bytes_read);

	if initial_bytes_read == 0 {
		return Some(MentionedFile {
			path: file_path.to_string(),
			content: String::new(),
			line_count: Some(1),
			byte_size: Some(0),
			skipped_reason: None,
			image: None,
		});
	}

	let detected_mime = detect_image_mime(&initial_buf);
	let (max_bytes, is_image) = match detected_mime {
		Some(mime) => (MAX_AUTO_READ_IMAGE_BYTES, Some(mime)),
		None => (MAX_AUTO_READ_TEXT_BYTES, None),
	};

	// Fast-reject if initial metadata already exceeds the limit
	if opened_meta.len() > max_bytes {
		return Some(MentionedFile {
			path: file_path.to_string(),
			content: format!("(skipped auto-read: too large, {})", format_bytes(opened_meta.len())),
			line_count: None,
			byte_size: Some(opened_meta.len()),
			skipped_reason: Some(SkippedReason::TooLarge),
			image: None,
		});
	}

	let full_buffer = match read_bounded(&mut file, initial_buf, max_bytes) {
		Ok(buf) => buf,
		Err(BoundedReadError::TooLarge { actual_read }) => {
			let disk_size = file.metadata().map(|m| m.len()).unwrap_or(0);
			let reported_size = if disk_size > max_bytes { disk_size } else { actual_read };
			return Some(MentionedFile {
				path: file_path.to_string(),
				content: format!("(skipped auto-read: too large, {})", format_bytes(reported_size)),
				line_count: None,
				byte_size: Some(reported_size),
				skipped_reason: Some(SkippedReason::TooLarge),
				image: None,
			});
		}
		Err(BoundedReadError::Io(kind)) => {
			return Some(MentionedFile {
				path: file_path.to_string(),
				content: format!("(skipped auto-read: unreadable: {kind:?})"),
				line_count: None,
				byte_size: None,
				skipped_reason: Some(SkippedReason::Unreadable),
				image: None,
			});
		}
	};

	let file_len = full_buffer.len() as u64;

	if let Some(mime) = is_image {
		let base64_data = base64_encode(&full_buffer);
		Some(MentionedFile {
			path: file_path.to_string(),
			content: String::new(),
			line_count: None,
			byte_size: Some(file_len),
			skipped_reason: None,
			image: Some(ImagePayload::new(mime, base64_data)),
		})
	} else {
		if is_probably_binary(&full_buffer) {
			return Some(MentionedFile {
				path: file_path.to_string(),
				content: format!("(skipped auto-read: binary file, {})", format_bytes(file_len)),
				line_count: None,
				byte_size: Some(file_len),
				skipped_reason: Some(SkippedReason::Binary),
				image: None,
			});
		}

		let text_content = match String::from_utf8(full_buffer) {
			Ok(s) => s,
			Err(_) => {
				return Some(MentionedFile {
					path: file_path.to_string(),
					content: format!("(skipped auto-read: binary file, {})", format_bytes(file_len)),
					line_count: None,
					byte_size: Some(file_len),
					skipped_reason: Some(SkippedReason::Binary),
					image: None,
				});
			}
		};

		let output = build_text_output(&text_content);
		Some(MentionedFile {
			path: file_path.to_string(),
			content: output.output,
			line_count: Some(output.line_count),
			byte_size: Some(file_len),
			skipped_reason: None,
			image: None,
		})
	}
}

/// Processes a list of file mention paths and returns a structured message.
///
/// Returns `None` if no files could be resolved or read.
#[must_use]
pub fn process_file_mentions(mentions: &[String], cwd: &Path) -> Option<FileMentionMessage> {
	if mentions.is_empty() {
		return None;
	}

	let mut files = Vec::new();
	for path in mentions {
		if let Some(mentioned) = read_mentioned_path(path, cwd) {
			files.push(mentioned);
		}
	}

	if files.is_empty() {
		return None;
	}

	let timestamp = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_millis() as u64)
		.unwrap_or(0);

	Some(FileMentionMessage {
		role: "fileMention".to_string(),
		files,
		timestamp,
	})
}

/// Extracts and processes mentions directly from raw prompt text.
#[must_use]
pub fn process_prompt_mentions(prompt: &str, cwd: &Path) -> Option<FileMentionMessage> {
	let mentions = extract_file_mentions(prompt);
	process_file_mentions(&mentions, cwd)
}

/// Alias for [`process_file_mentions`] for parity with TS `generateFileMentionMessages`.
#[must_use]
pub fn generate_file_mention_messages(
	file_paths: &[String],
	cwd: &Path,
) -> Option<FileMentionMessage> {
	process_file_mentions(file_paths, cwd)
}

/// Returns a vector containing the generated [`FileMentionMessage`], or an empty vector
/// if no files could be read (matching TS return type `Promise<AgentMessage[]>`).
#[must_use]
pub fn generate_file_mention_messages_vec(
	file_paths: &[String],
	cwd: &Path,
) -> Vec<FileMentionMessage> {
	process_file_mentions(file_paths, cwd).into_iter().collect()
}

/// Formats a [`FileMentionMessage`] into standard `<file path="...">` blocks for prompt context.
#[must_use]
pub fn format_prompt_block(message: &FileMentionMessage) -> String {
	let mut out = String::new();
	for file in &message.files {
		if !out.is_empty() {
			out.push('\n');
		}
		let inner = if file.content.is_empty() {
			"\n".to_string()
		} else {
			format!("\n{}\n", file.content)
		};
		out.push_str(&format!("<file path=\"{}\">{}</file>", file.path, inner));
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::fs;
	use std::sync::atomic::{AtomicUsize, Ordering};

	fn temp_dir(prefix: &str) -> PathBuf {
		static COUNTER: AtomicUsize = AtomicUsize::new(0);
		let count = COUNTER.fetch_add(1, Ordering::Relaxed);
		let dir = std::env::temp_dir().join(format!("omp-test-mentions-{prefix}-{}-{count}", std::process::id()));
		let _ = fs::remove_dir_all(&dir);
		fs::create_dir_all(&dir).expect("create temp dir");
		dir
	}

	#[test]
	fn test_extract_file_mentions_basic_and_punctuation() {
		let text = "Check @src/main.rs and (@docs/readme.md)! Also @config.json; see @foo.rs.";
		let mentions = extract_file_mentions(text);
		assert_eq!(
			mentions,
			vec![
				"src/main.rs".to_string(),
				"docs/readme.md".to_string(),
				"config.json".to_string(),
				"foo.rs".to_string()
			]
		);
	}

	#[test]
	fn test_extract_file_mentions_quoted_spaces() {
		let text = "Please see @\"My Folder/my file.png\" and @'Other Folder/notes.txt'";
		let mentions = extract_file_mentions(text);
		assert_eq!(
			mentions,
			vec!["My Folder/my file.png".to_string(), "Other Folder/notes.txt".to_string()]
		);
	}

	#[test]
	fn test_extract_file_mentions_boundary_and_emails() {
		let text = "Send to user@example.com, not @valid.txt. Double @@invalid, but [@bracket.rs] works.";
		let mentions = extract_file_mentions(text);
		assert_eq!(mentions, vec!["valid.txt".to_string(), "bracket.rs".to_string()]);
	}

	#[test]
	fn test_extract_file_mentions_deduplication() {
		let text = "Look at @same.rs, then another @same.rs and once more @\"same.rs\"";
		let mentions = extract_file_mentions(text);
		assert_eq!(mentions, vec!["same.rs".to_string()]);
	}

	#[test]
	fn test_detect_image_mime_types() {
		let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
		assert_eq!(detect_image_mime(&png), Some("image/png"));

		let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10];
		assert_eq!(detect_image_mime(&jpeg), Some("image/jpeg"));

		let gif87 = b"GIF87a\x01\x00\x01\x00";
		assert_eq!(detect_image_mime(gif87), Some("image/gif"));

		let gif89 = b"GIF89a\x01\x00\x01\x00";
		assert_eq!(detect_image_mime(gif89), Some("image/gif"));

		let webp = b"RIFF\x14\x00\x00\x00WEBPVP8 ";
		assert_eq!(detect_image_mime(webp), Some("image/webp"));

		let plain_text = b"Hello world this is not an image";
		assert_eq!(detect_image_mime(plain_text), None);

		let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>";
		assert_eq!(detect_image_mime(svg), None);
	}

	#[test]
	fn test_is_probably_binary_classification() {
		// NUL byte
		let nul_bytes = [b'a', b'b', 0x00, b'c'];
		assert!(is_probably_binary(&nul_bytes));

		// Invalid UTF-8 sequence
		let invalid_utf8 = [0x4d, 0x5a, 0xff, 0xfe, 0xc0, 0xc0];
		assert!(is_probably_binary(&invalid_utf8));

		// Valid ASCII
		let ascii = b"fn main() { println!(\"hello\"); }";
		assert!(!is_probably_binary(ascii));

		// Valid UTF-8 Cyrillic
		let cyrillic = "Привет, мир!".as_bytes();
		assert!(!is_probably_binary(cyrillic));

		// Incomplete UTF-8 sequence at end of buffer (boundary tolerance)
		// Russian 'П' is 2 bytes: 0xD0, 0x9F. If buffer ends at 0xD0:
		let truncated = [0xD0];
		assert!(!is_probably_binary(&truncated));
	}

	#[test]
	fn test_base64_encode_known_vectors() {
		assert_eq!(base64_encode(b""), "");
		assert_eq!(base64_encode(b"f"), "Zg==");
		assert_eq!(base64_encode(b"fo"), "Zm8=");
		assert_eq!(base64_encode(b"foo"), "Zm9v");
		assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
		assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
		assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
	}

	#[test]
	fn test_format_bytes_formatting() {
		assert_eq!(format_bytes(0), "0B");
		assert_eq!(format_bytes(512), "512B");
		assert_eq!(format_bytes(1024), "1.0KB");
		assert_eq!(format_bytes(1536), "1.5KB");
		assert_eq!(format_bytes(51200), "50.0KB");
		assert_eq!(format_bytes(5 * 1024 * 1024), "5.0MB");
		assert_eq!(format_bytes(25 * 1024 * 1024), "25.0MB");
		assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0GB");
	}

	#[test]
	fn test_format_age_formatting() {
		assert_eq!(format_age(0), "");
		assert_eq!(format_age(10), "just now");
		assert_eq!(format_age(120), "2m ago");
		assert_eq!(format_age(7200), "2h ago");
		assert_eq!(format_age(86400 * 3), "3d ago");
		assert_eq!(format_age(86400 * 14), "2w ago");
		assert_eq!(format_age(86400 * 60), "2mo ago");
	}

	#[test]
	fn test_build_text_output_head_truncation() {
		// Short text
		let out = build_text_output("line 1\nline 2\nline 3");
		assert_eq!(out.output, "line 1\nline 2\nline 3");
		assert_eq!(out.line_count, 3);

		// Exceeding byte limit with multiple lines
		let long_line = "a".repeat(1000);
		let mut many_lines = Vec::new();
		for _ in 0..100 {
			many_lines.push(long_line.as_str());
		}
		let content = many_lines.join("\n");
		let truncated = build_text_output(&content);
		assert!(truncated.output.contains("[Showing lines 1-"));
		assert!(truncated.output.contains("Use :"));

		// Line 1 exceeding byte limit alone
		let single_huge_line = "x".repeat(60 * 1024);
		let single_trunc = build_text_output(&single_huge_line);
		assert!(single_trunc.output.contains("[Line 1 is 60.0KB, exceeds 50.0KB limit. Showing first 50.0KB of the line.]"));
	}

	#[test]
	fn test_directory_listing_and_empty() {
		let dir = temp_dir("listing");
		let empty_res = build_directory_listing(&dir).expect("empty dir");
		assert_eq!(empty_res.output, "(empty directory)");
		assert_eq!(empty_res.line_count, 1);

		fs::write(dir.join("b.txt"), "hello").expect("write b");
		fs::write(dir.join("a.txt"), "world").expect("write a");
		fs::create_dir(dir.join("sub")).expect("create sub");

		let filled_res = build_directory_listing(&dir).expect("filled dir");
		let lines: Vec<&str> = filled_res.output.lines().collect();
		assert_eq!(lines.len(), 3);
		assert!(lines[0].starts_with("a.txt"));
		assert!(lines[1].starts_with("b.txt"));
		assert!(lines[2].starts_with("sub/"));

		let _ = fs::remove_dir_all(&dir);
	}

	#[test]
	fn test_read_mentioned_path_text_and_image_and_binary() {
		let dir = temp_dir("path-reads");

		// 1. Text file
		let text_file = dir.join("hello.rs");
		fs::write(&text_file, "pub fn hello() -> &'static str { \"world\" }").expect("write text");
		let text_mention = read_mentioned_path("hello.rs", &dir).expect("read text");
		assert_eq!(text_mention.path, "hello.rs");
		assert!(text_mention.content.contains("pub fn hello()"));
		assert_eq!(text_mention.skipped_reason, None);
		assert_eq!(text_mention.image, None);

		// 2. Supported Image (PNG)
		let png_file = dir.join("test.png");
		let png_bytes = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];
		fs::write(&png_file, png_bytes).expect("write png");
		let img_mention = read_mentioned_path("test.png", &dir).expect("read png");
		assert_eq!(img_mention.path, "test.png");
		assert_eq!(img_mention.skipped_reason, None);
		let img = img_mention.image.expect("image payload");
		assert_eq!(img.mime_type, "image/png");
		assert_eq!(img.data_base64, base64_encode(&png_bytes));

		// 3. Binary file
		let bin_file = dir.join("test.bin");
		let bin_bytes = [0x00, 0x01, 0x02, 0x03, 0xFF, 0xFE];
		fs::write(&bin_file, bin_bytes).expect("write bin");
		let bin_mention = read_mentioned_path("test.bin", &dir).expect("read bin");
		assert_eq!(bin_mention.path, "test.bin");
		assert_eq!(bin_mention.skipped_reason, Some(SkippedReason::Binary));
		assert!(bin_mention.content.contains("binary file"));
		assert_eq!(bin_mention.image, None);

		// 4. Nonexistent file
		assert_eq!(read_mentioned_path("does-not-exist.txt", &dir), None);

		let _ = fs::remove_dir_all(&dir);
	}

	#[test]
	fn test_read_mentioned_path_oversized_text() {
		let dir = temp_dir("oversize");
		let big_text = dir.join("big.txt");

		// Simulate large file by setting sparse or large file
		let f = File::create(&big_text).expect("create file");
		f.set_len(MAX_AUTO_READ_TEXT_BYTES + 1024).expect("set len");
		drop(f);

		let mention = read_mentioned_path("big.txt", &dir).expect("read oversize text");
		assert_eq!(mention.skipped_reason, Some(SkippedReason::TooLarge));
		assert!(mention.content.contains("too large"));

		let _ = fs::remove_dir_all(&dir);
	}

	#[test]
	fn test_format_prompt_block() {
		let msg = FileMentionMessage {
			role: "fileMention".to_string(),
			files: vec![
				MentionedFile {
					path: "src/main.rs".to_string(),
					content: "fn main() {}".to_string(),
					line_count: Some(1),
					byte_size: Some(12),
					skipped_reason: None,
					image: None,
				},
				MentionedFile {
					path: "pic.png".to_string(),
					content: String::new(),
					line_count: None,
					byte_size: Some(100),
					skipped_reason: None,
					image: Some(ImagePayload::new("image/png", "aGVsbG8=")),
				},
			],
			timestamp: 123456789,
		};

		let formatted = format_prompt_block(&msg);
		assert!(formatted.contains("<file path=\"src/main.rs\">\nfn main() {}\n</file>"));
		assert!(formatted.contains("<file path=\"pic.png\">\n</file>"));
	}

	#[test]
	fn test_extract_file_mentions_unclosed_quotes() {
		let text = "Check @\"unclosed_quote.txt for details and @'other_unclosed.rs here";
		let mentions = extract_file_mentions(text);
		assert_eq!(
			mentions,
			vec!["unclosed_quote.txt".to_string(), "other_unclosed.rs".to_string()]
		);
	}

	#[test]
	fn test_serde_camel_case_wire_parity() {
		let msg = FileMentionMessage {
			role: "fileMention".to_string(),
			files: vec![MentionedFile {
				path: "photo.jpg".to_string(),
				content: String::new(),
				line_count: None,
				byte_size: Some(2048),
				skipped_reason: None,
				image: Some(ImagePayload::new("image/jpeg", "aW1hZ2U=")),
			}],
			timestamp: 55555,
		};

		let json = serde_json::to_string(&msg).expect("serialize");
		assert!(json.contains("\"byteSize\":2048"));
		assert!(json.contains("\"mimeType\":\"image/jpeg\""));
		assert!(json.contains("\"data\":\"aW1hZ2U=\""));
		assert!(json.contains("\"type\":\"image\""));

		// Deserialization roundtrip
		let deserialized: FileMentionMessage = serde_json::from_str(&json).expect("deserialize");
		assert_eq!(deserialized, msg);

		// Deserialization from snake_case legacy format
		let snake_json = r#"{
			"role": "fileMention",
			"files": [{
				"path": "test.txt",
				"content": "abc",
				"line_count": 1,
				"byte_size": 3,
				"skipped_reason": "tooLarge",
				"image": {
					"mime_type": "image/png",
					"data_base64": "xyz"
				}
			}],
			"timestamp": 123
		}"#;
		let parsed: FileMentionMessage = serde_json::from_str(snake_json).expect("parse snake");
		assert_eq!(parsed.files[0].line_count, Some(1));
		assert_eq!(parsed.files[0].byte_size, Some(3));
		assert_eq!(parsed.files[0].skipped_reason, Some(SkippedReason::TooLarge));
		let img = parsed.files[0].image.as_ref().unwrap();
		assert_eq!(img.mime_type, "image/png");
		assert_eq!(img.data_base64, "xyz");
	}

	#[test]
	fn test_read_bounded_infinite_stream_prevents_oom() {
		// Infinite stream of bytes (simulating growing file or /dev/zero)
		let infinite = std::io::repeat(b'a');
		let res = read_bounded(infinite, Vec::new(), 1024);
		match res {
			Err(BoundedReadError::TooLarge { actual_read }) => {
				assert_eq!(actual_read, 1025);
			}
			other => panic!("expected TooLarge, got {other:?}"),
		}

		// Normal finite stream under limit
		let finite = std::io::Cursor::new(b"hello world".to_vec());
		let buf = read_bounded(finite, Vec::new(), 1024).expect("read finite");
		assert_eq!(buf, b"hello world");

		// Initial buffer already near limit
		let finite2 = std::io::Cursor::new(b"world".to_vec());
		let buf2 = read_bounded(finite2, b"hello ".to_vec(), 1024).expect("read with initial");
		assert_eq!(buf2, b"hello world");
	}

	#[test]
	fn test_resolve_path_bare_slash_to_cwd() {
		let cwd = Path::new("/custom/project/dir");
		assert_eq!(resolve_path("/", cwd), PathBuf::from("/custom/project/dir"));
		assert_eq!(resolve_path("///", cwd), PathBuf::from("/custom/project/dir"));
		assert_eq!(resolve_path("", cwd), PathBuf::from("/custom/project/dir"));
		assert_eq!(resolve_path("foo/bar.txt", cwd), PathBuf::from("/custom/project/dir/foo/bar.txt"));
	}

	#[test]
	fn test_build_directory_listing_exceeds_limit_bounded_memory() {
		let tmp = temp_dir("dir-500-limit");
		// Create 510 files
		for i in 0..510 {
			let name = format!("item_{i:04}.txt");
			std::fs::write(tmp.join(name), b"test").expect("write file");
		}

		let output = build_directory_listing(&tmp).expect("listing");
		assert!(output.output.contains("500 entries limit reached"));
		assert!(output.output.contains("item_0000.txt"));
		assert!(output.output.contains("item_0499.txt"));
		// item_0500 should be excluded by the 500 cap
		assert!(!output.output.contains("item_0500.txt"));

		let _ = std::fs::remove_dir_all(&tmp);
	}

	#[cfg(unix)]
	#[test]
	fn test_fifo_opened_non_blocking_rejected() {
		unsafe extern "C" {
			fn mkfifo(path: *const std::os::raw::c_char, mode: std::os::raw::c_uint) -> std::os::raw::c_int;
		}

		let tmp = temp_dir("fifo-test");
		let fifo_path = tmp.join("test_fifo.pipe");
		let c_str = std::ffi::CString::new(fifo_path.to_str().unwrap()).unwrap();
		let res = unsafe { mkfifo(c_str.as_ptr(), 0o600) };
		assert_eq!(res, 0, "mkfifo failed");

		let mention = read_mentioned_path(&fifo_path.to_string_lossy(), &tmp);
		assert!(mention.is_some(), "mention must be resolved");
		let file = mention.unwrap();
		assert_eq!(file.skipped_reason, Some(SkippedReason::Unreadable));
		assert!(file.content.contains("not a regular file"));

		let _ = std::fs::remove_file(&fifo_path);
		let _ = std::fs::remove_dir_all(&tmp);
	}
}
