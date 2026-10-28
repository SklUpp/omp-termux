//! Content-derived snapshot tags and text-shape normalization.
//!
//! Mirrors `packages/hashline/src/format.ts` and `normalize.ts`: tags are
//! `xxh32(normalizeTrailingWhitespace(text), 0) & 0xffff` formatted as four
//! uppercase hex digits.

/// Number of hex characters in a content-derived tag.
pub const HASH_LENGTH: usize = 4;

/// Trim trailing `[ \t\r]` from every line so CRLF files and display-trimmed
/// lines hash identically to their LF/no-trailing-space form.
pub fn normalize_hash_text(text: &str) -> String {
	let mut out = String::with_capacity(text.len());
	for (index, line) in text.split('\n').enumerate() {
		if index > 0 {
			out.push('\n');
		}
		out.push_str(line.trim_end_matches([' ', '\t', '\r']));
	}
	out
}

/// 4-hex content tag carried by a hashline section header.
pub fn compute_file_hash(text: &str) -> String {
	let normalized = normalize_hash_text(text);
	let value = xxhash_rust::xxh32::xxh32(normalized.as_bytes(), 0) & 0xffff;
	format!("{value:04X}")
}

/// First line-ending style in `content`; LF when neither style is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
	Lf,
	Crlf,
}

pub fn detect_line_ending(content: &str) -> LineEnding {
	let crlf = content.find("\r\n");
	let lf = content.find('\n');
	match (crlf, lf) {
		(Some(crlf), Some(lf)) if crlf < lf => LineEnding::Crlf,
		_ => LineEnding::Lf,
	}
}

/// Normalize every line ending to LF.
pub fn normalize_to_lf(text: &str) -> String {
	if !text.contains('\r') {
		return text.to_string();
	}
	let mut out = String::with_capacity(text.len());
	let mut chars = text.chars().peekable();
	while let Some(ch) = chars.next() {
		if ch == '\r' {
			if chars.peek() == Some(&'\n') {
				chars.next();
			}
			out.push('\n');
		} else {
			out.push(ch);
		}
	}
	out
}

/// Re-encode LF text with the requested line ending.
pub fn restore_line_endings(text: &str, ending: LineEnding) -> String {
	match ending {
		LineEnding::Lf => text.to_string(),
		LineEnding::Crlf => text.replace('\n', "\r\n"),
	}
}

/// Split a leading UTF-8 BOM from `content`.
pub fn strip_bom(content: &str) -> (&str, &str) {
	match content.strip_prefix('\u{feff}') {
		Some(rest) => ("\u{feff}", rest),
		None => ("", content),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn hash_is_stable_across_line_endings_and_trailing_space() {
		assert_eq!(compute_file_hash("a \nb\t\n"), "9A46");
		assert_eq!(compute_file_hash("a\r\nb\r\n"), "9A46");
		assert_eq!(compute_file_hash("a\nb\n"), "9A46");
		assert_eq!(compute_file_hash(""), "5D05");
	}

	#[test]
	fn line_endings_round_trip() {
		assert_eq!(detect_line_ending("a\r\nb\n"), LineEnding::Crlf);
		assert_eq!(detect_line_ending("a\nb\r\n"), LineEnding::Lf);
		assert_eq!(normalize_to_lf("a\r\nb\rc\n"), "a\nb\nc\n");
		assert_eq!(restore_line_endings("a\nb\n", LineEnding::Crlf), "a\r\nb\r\n");
	}
}
