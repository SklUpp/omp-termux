//! Grep pattern compilation shared by `pi-natives` and `omp-cli`.
//!
//! Fallback order (all engines are tried in this order):
//! 1. sanitize non-quantifier braces, then try the Rust regex engine;
//! 2. try PCRE2 (lookaround/backreferences);
//! 3. retry with literal parentheses escaped after group-syntax errors;
//! 4. fall back to a fully escaped literal pattern.

use std::{borrow::Cow, fmt, sync::LazyLock};

use grep_matcher::{Matcher, NoCaptures, NoError};
use grep_pcre2::{
	Error as PcreError, RegexMatcher as PcreMatcher,
	RegexMatcherBuilder as PcreMatcherBuilder,
};
use grep_regex::{
	Error as RegexError, RegexMatcher, RegexMatcherBuilder,
};

/// PCRE2 JIT toggle: `OMP_PCRE2_JIT=1` forces JIT on, `0`/`false` forces it
/// off. Unset, JIT stays on everywhere except macOS, where PCRE2's SLJIT
/// executable allocator can fault while compiling patterns (issue #7399).
static PCRE2_JIT_ENABLED: LazyLock<bool> = LazyLock::new(|| match std::env::var("OMP_PCRE2_JIT") {
	Ok(value) if !value.is_empty() => value != "0" && !value.eq_ignore_ascii_case("false"),
	_ => !cfg!(target_os = "macos"),
});

/// Matcher selected by [`build_matcher`].
pub enum CompiledMatcher {
	/// Rust regex engine matcher.
	Rust(RegexMatcher),
	/// PCRE2 matcher for lookaround/backreference patterns.
	Pcre(PcreMatcher),
}

/// Error from either underlying matcher.
#[derive(Debug)]
pub enum CompiledMatcherError {
	/// Rust regex engine error.
	Rust(NoError),
	/// PCRE2 error.
	Pcre(PcreError),
}

impl fmt::Display for CompiledMatcherError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Rust(err) => err.fmt(formatter),
			Self::Pcre(err) => err.fmt(formatter),
		}
	}
}

impl Matcher for CompiledMatcher {
	type Captures = NoCaptures;
	type Error = CompiledMatcherError;

	fn find_at(
		&self,
		haystack: &[u8],
		at: usize,
	) -> std::result::Result<Option<grep_matcher::Match>, Self::Error> {
		match self {
			Self::Rust(matcher) => matcher
				.find_at(haystack, at)
				.map_err(CompiledMatcherError::Rust),
			Self::Pcre(matcher) => matcher
				.find_at(haystack, at)
				.map_err(CompiledMatcherError::Pcre),
		}
	}

	fn new_captures(&self) -> std::result::Result<Self::Captures, Self::Error> {
		Ok(NoCaptures::new())
	}
}

/// Check whether `bytes[start]` (which must be `b'{'`) begins a valid
/// repetition quantifier: `{N}`, `{N,}`, or `{N,M}` where N and M are decimal
/// digits. Returns the byte index of the closing `}` if valid.
const fn find_valid_repetition(bytes: &[u8], start: usize) -> Option<usize> {
	let len = bytes.len();
	let mut i = start + 1;
	// Must start with at least one digit.
	if i >= len || !bytes[i].is_ascii_digit() {
		return None;
	}
	while i < len && bytes[i].is_ascii_digit() {
		i += 1;
	}
	if i >= len {
		return None;
	}
	if bytes[i] == b'}' {
		return Some(i);
	}
	if bytes[i] != b',' {
		return None;
	}
	i += 1;
	if i >= len {
		return None;
	}
	// After comma: optional digits then `}`.
	while i < len && bytes[i].is_ascii_digit() {
		i += 1;
	}
	if i < len && bytes[i] == b'}' {
		return Some(i);
	}
	None
}

const fn find_braced_escape_end(bytes: &[u8], start: usize) -> Option<usize> {
	let mut i = start + 1;
	while i < bytes.len() {
		if bytes[i] == b'}' {
			return Some(i);
		}
		i += 1;
	}
	None
}

/// Escape `{` and `}` that don't form valid repetition quantifiers.
///
/// Patterns like `${platform}` or `a{b}` contain braces the regex engine
/// rejects as malformed repetitions. Since such braces can never be valid
/// regex syntax, turning them into `\{` / `\}` is semantics-preserving
/// and avoids confusing error messages for callers who pass literal text
/// fragments (e.g. JS template strings).
fn sanitize_braces(pattern: &str) -> Cow<'_, str> {
	let bytes = pattern.as_bytes();
	if !bytes.contains(&b'{') && !bytes.contains(&b'}') {
		return Cow::Borrowed(pattern);
	}

	let len = bytes.len();
	let mut result = String::with_capacity(len + 8);
	let mut modified = false;
	let mut i = 0;

	while i < len {
		// Pass escaped characters through unchanged.
		if bytes[i] == b'\\' && i + 1 < len {
			result.push('\\');
			i += 1;
			// The next character is the escaped literal; push it regardless.
			// Safety: index is in bounds (checked above).
			let ch = pattern[i..]
				.chars()
				.next()
				.expect("non-empty slice has a char");
			result.push(ch);
			i += ch.len_utf8();
			if matches!(ch, 'p' | 'P' | 'x' | 'u') && i < len && bytes[i] == b'{' {
				if let Some(end) = find_braced_escape_end(bytes, i) {
					result.push_str(&pattern[i..=end]);
					i = end + 1;
				} else {
					result.push_str(&pattern[i..]);
					i = len;
				}
			}
			continue;
		}

		if bytes[i] == b'{' {
			if let Some(end) = find_valid_repetition(bytes, i) {
				result.push_str(&pattern[i..=end]);
				i = end + 1;
				continue;
			}
			result.push_str("\\{");
			i += 1;
			modified = true;
			continue;
		}

		if bytes[i] == b'}' {
			result.push_str("\\}");
			i += 1;
			modified = true;
			continue;
		}

		let ch = pattern[i..]
			.chars()
			.next()
			.expect("non-empty slice has a char");
		result.push(ch);
		i += ch.len_utf8();
	}

	if modified {
		Cow::Owned(result)
	} else {
		Cow::Borrowed(pattern)
	}
}

/// Escape unescaped parentheses after a group-syntax regex error.
///
/// Search patterns like `fetchAnthropicProvider(` are common literal snippets,
/// but the regex engine parses the trailing `(` as the start of a capture
/// group. When the parser already reported invalid group syntax, escaping any
/// remaining literal parentheses preserves useful search behavior without
/// changing valid regexes.
fn escape_unescaped_parentheses(pattern: &str) -> Cow<'_, str> {
	let bytes = pattern.as_bytes();
	if !bytes.contains(&b'(') && !bytes.contains(&b')') {
		return Cow::Borrowed(pattern);
	}

	let mut result = String::with_capacity(pattern.len() + 4);
	let mut modified = false;
	let mut i = 0;

	while i < bytes.len() {
		if bytes[i] == b'\\' && i + 1 < bytes.len() {
			result.push('\\');
			i += 1;
			let ch = pattern[i..]
				.chars()
				.next()
				.expect("non-empty slice has a char");
			result.push(ch);
			i += ch.len_utf8();
			continue;
		}

		let ch = pattern[i..]
			.chars()
			.next()
			.expect("non-empty slice has a char");
		if matches!(ch, '(' | ')') {
			result.push('\\');
			modified = true;
		}
		result.push(ch);
		i += ch.len_utf8();
	}

	if modified {
		Cow::Owned(result)
	} else {
		Cow::Borrowed(pattern)
	}
}

fn build_regex_matcher(
	pattern: &str,
	ignore_case: bool,
	multiline: bool,
) -> std::result::Result<RegexMatcher, RegexError> {
	let build = |line_terminated| {
		let mut builder = RegexMatcherBuilder::new();
		builder.case_insensitive(ignore_case).multi_line(multiline);
		if line_terminated {
			builder.line_terminator(Some(b'\n'));
		}
		builder.build(pattern)
	};

	if !multiline && let Ok(matcher) = build(true) {
		return Ok(matcher);
	}
	build(false)
}

fn build_pcre_matcher(
	pattern: &str,
	ignore_case: bool,
	multiline: bool,
) -> std::result::Result<PcreMatcher, PcreError> {
	let mut builder = PcreMatcherBuilder::new();
	builder
		.caseless(ignore_case)
		.multi_line(multiline)
		.utf(true)
		.ucp(true)
		.jit_if_available(*PCRE2_JIT_ENABLED);
	builder.build(pattern)
}

/// Compile `pattern` with the native fallback chain.
///
/// On total engine failure the pattern is matched as a literal; an error is
/// returned only if even the escaped literal cannot compile.
pub fn build_matcher(
	pattern: &str,
	ignore_case: bool,
	multiline: bool,
) -> std::result::Result<CompiledMatcher, String> {
	let sanitized = sanitize_braces(pattern);
	let err = match build_regex_matcher(sanitized.as_ref(), ignore_case, multiline) {
		Ok(matcher) => return Ok(CompiledMatcher::Rust(matcher)),
		Err(err) => err,
	};

	// PCRE2 supports features the Rust regex engine deliberately omits, such
	// as lookaround and backreferences.
	if let Ok(matcher) = build_pcre_matcher(sanitized.as_ref(), ignore_case, multiline) {
		return Ok(CompiledMatcher::Pcre(matcher));
	}

	// Targeted retry: a stray `(`/`)` in an otherwise valid regex (e.g.
	// `fetchProvider(`) — escape the parentheses but keep the rest of the regex
	// working.
	let message = err.to_string();
	if message.contains("unclosed group") || message.contains("unopened group") {
		let escaped = escape_unescaped_parentheses(sanitized.as_ref());
		if escaped.as_ref() != sanitized.as_ref() {
			if let Ok(matcher) = build_regex_matcher(escaped.as_ref(), ignore_case, multiline) {
				return Ok(CompiledMatcher::Rust(matcher));
			}
			if let Ok(matcher) = build_pcre_matcher(escaped.as_ref(), ignore_case, multiline) {
				return Ok(CompiledMatcher::Pcre(matcher));
			}
		}
	}

	// Final fallback: both engines rejected the pattern, so match it literally
	// instead of failing the whole search.
	build_regex_matcher(&regex::escape(pattern), ignore_case, multiline)
		.map(CompiledMatcher::Rust)
		.map_err(|_| format!("Regex error: {message}"))
}

#[cfg(test)]
mod tests {
	use grep_matcher::Matcher;

	use super::{escape_unescaped_parentheses, sanitize_braces};

	#[test]
	fn preserves_unicode_property_escapes() {
		assert_eq!(sanitize_braces(r"\p{Greek}").as_ref(), r"\p{Greek}");
	}

	#[test]
	fn preserves_hex_brace_escapes() {
		assert_eq!(sanitize_braces(r"\x{41}").as_ref(), r"\x{41}");
	}

	#[test]
	fn preserves_malformed_braced_escapes() {
		assert_eq!(sanitize_braces(r"\p{Greek").as_ref(), r"\p{Greek");
	}

	#[test]
	fn escapes_non_quantifier_braces() {
		assert_eq!(sanitize_braces("${platform}").as_ref(), "$\\{platform\\}");
	}

	#[test]
	fn preserves_valid_quantifiers() {
		assert_eq!(sanitize_braces("a{2,4}").as_ref(), "a{2,4}");
	}

	#[test]
	fn preserves_escaped_parentheses() {
		assert_eq!(escape_unescaped_parentheses(r"foo\(bar\)").as_ref(), r"foo\(bar\)");
	}

	#[test]
	fn escapes_literal_parentheses() {
		assert_eq!(
			escape_unescaped_parentheses("fetchAnthropicProvider(").as_ref(),
			r"fetchAnthropicProvider\("
		);
		assert_eq!(
			escape_unescaped_parentheses("fetchAnthropicProvider()").as_ref(),
			r"fetchAnthropicProvider\(\)"
		);
	}

	#[test]
	fn invalid_regex_falls_back_to_literal() {
		// Patterns that are not valid regex syntax (unclosed class, dangling
		// quantifier, stray `)`) must degrade to a literal search rather than
		// erroring.
		for (pattern, hay, miss) in [
			("foo[bar", &b"x foo[bar y"[..], &b"foobar"[..]),
			("+++", &b"a+++b"[..], &b"ab"[..]),
			("fail)", &b"(1 fail)"[..], &b"failure"[..]),
		] {
			let matcher = super::build_matcher(pattern, false, false)
				.unwrap_or_else(|e| panic!("`{pattern}` should fall back to literal, got: {e}"));
			assert!(matcher.is_match(hay).unwrap(), "`{pattern}` should match {hay:?}");
			assert!(!matcher.is_match(miss).unwrap(), "`{pattern}` should not match {miss:?}");
		}
	}

	#[test]
	fn stray_parenthesis_preserves_surrounding_regex() {
		// The targeted retry escapes the stray `(` but keeps `.*` as a regex.
		let matcher =
			super::build_matcher("foo.*(bar", false, false).expect("retry with escaped paren");
		assert!(matcher.is_match(b"fooXYZ(bar").unwrap());
		assert!(!matcher.is_match(b"foobar").unwrap());
	}

	#[test]
	fn valid_regex_is_not_escaped() {
		// A parseable pattern stays a regex: `fo+` matches repeats, which the
		// literal `fo+` never would.
		let matcher = super::build_matcher("fo+", false, false).expect("valid regex");
		assert!(matcher.is_match(b"foooo").unwrap());
		assert!(!matcher.is_match(b"bar").unwrap());
	}
}
