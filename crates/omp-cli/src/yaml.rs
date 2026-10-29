//! Minimal YAML subset reader/writer for omp config files.
//!
//! Supported: block mappings, block sequences, `key:` with a nested block at
//! the same indent, flow sequences (`[a, b]`) and mappings (`{a: b}`),
//! single/double quoted scalars, comments, and `|`/`>` block scalars (with
//! `-`/`+` chomping). Anchors, tags, aliases, merge keys and multi-document
//! streams are deliberately unsupported (`ponytail:` a full YAML engine is a
//! dependency, not a feature we need — upgrade to `serde_yaml` only if omp
//! starts shipping YAML we do not author). [`to_string`] writes only this
//! subset back, so a rewritten file always re-parses.

use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
	Null,
	Bool(bool),
	Int(i64),
	Float(f64),
	Str(String),
	List(Vec<Value>),
	Map(Vec<(String, Value)>),
}

impl Value {
	#[must_use]
	pub fn get(&self, key: &str) -> Option<&Self> {
		match self {
			Self::Map(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
			_ => None,
		}
	}

	#[must_use]
	pub fn as_str(&self) -> Option<&str> {
		match self {
			Self::Str(s) => Some(s),
			_ => None,
		}
	}

	#[must_use]
	pub fn as_bool(&self) -> Option<bool> {
		match self {
			Self::Bool(b) => Some(*b),
			_ => None,
		}
	}

	#[must_use]
	pub fn as_list(&self) -> Option<&[Self]> {
		match self {
			Self::List(items) => Some(items),
			_ => None,
		}
	}

	#[must_use]
	pub fn as_map(&self) -> Option<&[(String, Self)]> {
		match self {
			Self::Map(pairs) => Some(pairs),
			_ => None,
		}
	}

	/// Scalar coerced to text: config files routinely carry unquoted numbers or
	/// booleans where the schema wants a string (`apiKey: 123`).
	#[must_use]
	pub fn to_text(&self) -> Option<String> {
		match self {
			Self::Str(s) => Some(s.clone()),
			Self::Int(i) => Some(i.to_string()),
			Self::Float(f) => Some(f.to_string()),
			Self::Bool(b) => Some(b.to_string()),
			Self::Null | Self::List(_) | Self::Map(_) => None,
		}
	}
}

pub fn parse(src: &str) -> Result<Value> {
	let mut parser = Parser { lines: src.lines().map(str::to_string).collect(), pos: 0 };
	parser.skip_ignorable();
	if parser.pos == parser.lines.len() {
		return Ok(Value::Null);
	}
	let indent = indent_of(&parser.lines[parser.pos], parser.pos + 1)?;
	let value = parser.parse_node(indent)?;
	parser.skip_ignorable();
	if parser.pos < parser.lines.len() {
		return Err(parser.unexpected("trailing content"));
	}
	Ok(value)
}

struct Parser {
	lines: Vec<String>,
	pos: usize,
}

impl Parser {
	fn parse_node(&mut self, indent: usize) -> Result<Value> {
		let content = strip_comment(&self.lines[self.pos]).trim().to_string();
		if content == "-" || content.starts_with("- ") {
			return self.parse_seq(indent);
		}
		if is_block_indicator(&content) {
			self.pos += 1;
			return self.parse_block_scalar(&content, indent);
		}
		if split_key(&content).is_none() {
			// Scalar node: reachable from a `- scalar` sequence item only.
			self.pos += 1;
			return parse_scalar(&content);
		}
		self.parse_map(indent)
	}

	fn parse_map(&mut self, indent: usize) -> Result<Value> {
		let mut pairs: Vec<(String, Value)> = Vec::new();
		loop {
			self.skip_ignorable();
			if self.pos >= self.lines.len() {
				break;
			}
			let current = self.current_indent()?;
			if current < indent {
				break;
			}
			if current > indent {
				return Err(self.unexpected("unexpected indentation"));
			}
			let content = strip_comment(&self.lines[self.pos]).trim().to_string();
			let Some((raw_key, rest)) = split_key(&content) else {
				return Err(self.unexpected("expected `key: value`"));
			};
			check_key(raw_key, self.pos + 1)?;
			if raw_key.trim() == "<<" {
				return Err(Error::new(format!(
					"yaml: merge keys are not supported at line {}",
					self.pos + 1
				)));
			}
			let key = unquote(raw_key.trim()).map_err(|err| self.wrap(err))?;
			if pairs.iter().any(|(existing, _)| *existing == key) {
				return Err(Error::new(format!(
					"yaml: duplicate key `{key}` at line {}",
					self.pos + 1
				)));
			}
			let rest = rest.trim().to_string();
			self.pos += 1;
			let value = self.parse_value(&rest, indent)?;
			pairs.push((key, value));
		}
		Ok(Value::Map(pairs))
	}

	fn parse_seq(&mut self, indent: usize) -> Result<Value> {
		let mut items = Vec::new();
		loop {
			self.skip_ignorable();
			if self.pos >= self.lines.len() {
				break;
			}
			let current = self.current_indent()?;
			if current < indent {
				break;
			}
			if current > indent {
				return Err(self.unexpected("unexpected indentation"));
			}
			let content = strip_comment(&self.lines[self.pos]).trim().to_string();
			if content != "-" && !content.starts_with("- ") {
				break;
			}
			let rest = content[1..].trim_start().to_string();
			if rest.is_empty() {
				self.pos += 1;
				self.skip_ignorable();
				if self.pos < self.lines.len() && self.current_indent()? > indent {
					let child = self.current_indent()?;
					items.push(self.parse_node(child)?);
				} else {
					items.push(Value::Null);
				}
			} else if is_block_indicator(&rest) {
				self.pos += 1;
				items.push(self.parse_block_scalar(&rest, indent)?);
			} else {
				// `- key: value` / `- scalar`: re-indent the remainder to the
				// column it actually starts at (`-   key` puts it further right)
				// and parse it as a node so compact mappings gain their
				// continuation lines.
				let child_indent = indent + 1 + content[1..].len() - content[1..].trim_start().len();
				self.lines[self.pos] = format!("{}{rest}", " ".repeat(child_indent));
				items.push(self.parse_node(child_indent)?);
			}
		}
		Ok(Value::List(items))
	}

	fn parse_value(&mut self, rest: &str, indent: usize) -> Result<Value> {
		if rest.is_empty() {
			let saved = self.pos;
			self.skip_ignorable();
			if self.pos >= self.lines.len() {
				return Ok(Value::Null);
			}
			let next = self.current_indent()?;
			if next > indent {
				return self.parse_node(next);
			}
			let content = strip_comment(&self.lines[self.pos]).trim();
			if next == indent && (content == "-" || content.starts_with("- ")) {
				return self.parse_seq(indent);
			}
			self.pos = saved;
			return Ok(Value::Null);
		}
		if is_block_indicator(rest) {
			return self.parse_block_scalar(rest, indent);
		}
		parse_scalar(rest)
	}

	fn parse_block_scalar(&mut self, indicator: &str, parent_indent: usize) -> Result<Value> {
		let folded = indicator.starts_with('>');
		let strip = indicator.contains('-');
		let mut raw: Vec<(usize, String)> = Vec::new();
		while self.pos < self.lines.len() {
			let line = &self.lines[self.pos];
			if line.trim().is_empty() {
				raw.push((usize::MAX, String::new()));
				self.pos += 1;
				continue;
			}
			let indent = indent_of(line, self.pos + 1)?;
			if indent <= parent_indent {
				break;
			}
			raw.push((indent, line.trim_end().to_string()));
			self.pos += 1;
		}
		// `|+`/`>+` keep every trailing newline; clip (`|`) and strip (`|-`) drop them.
		if !indicator.contains('+') {
			while raw.last().is_some_and(|(indent, _)| *indent == usize::MAX) {
				raw.pop();
			}
		}
		let min = raw
			.iter()
			.filter(|(indent, _)| *indent != usize::MAX)
			.map(|(indent, _)| *indent)
			.min()
			.unwrap_or(0);
		let lines: Vec<&str> = raw
			.iter()
			.map(|(indent, text)| {
				if *indent == usize::MAX {
					""
				} else {
					&text[min..]
				}
			})
			.collect();
		if folded
			&& raw.iter().any(|(indent, text)| {
				*indent != usize::MAX && *indent > min && !text[min..].trim().is_empty()
			})
		{
			return Err(Error::new(
				"yaml: folded blocks with more-indented lines are not supported",
			));
		}
		let mut text = if folded {
			fold_lines(&lines)
		} else {
			lines.join("\n")
		};
		if !strip && !text.is_empty() {
			text.push('\n');
		}
		Ok(Value::Str(text))
	}

	fn current_indent(&self) -> Result<usize> {
		indent_of(&self.lines[self.pos], self.pos + 1)
	}

	fn skip_ignorable(&mut self) {
		while self.pos < self.lines.len() {
			let trimmed = self.lines[self.pos].trim();
			if trimmed.is_empty() || trimmed.starts_with('#') {
				self.pos += 1;
			} else {
				break;
			}
		}
	}

	fn unexpected(&self, what: &str) -> Error {
		Error::new(format!("yaml: {what} at line {}", self.pos + 1))
	}

	fn wrap(&self, err: Error) -> Error {
		Error::new(format!("{err} (line {})", self.pos + 1))
	}
}

fn indent_of(line: &str, line_no: usize) -> Result<usize> {
	let mut count = 0;
	for byte in line.bytes() {
		match byte {
			b' ' => count += 1,
			b'\t' => {
				return Err(Error::new(format!(
					"yaml: tabs are not allowed for indentation (line {line_no})"
				)));
			},
			_ => break,
		}
	}
	Ok(count)
}

/// Anchors, aliases and tags are not modelled; a decorated key would otherwise
/// be read as a literal string and rewritten with quotes around the decoration.
fn check_key(raw_key: &str, line_no: usize) -> Result<()> {
	let key = raw_key.trim();
	if key.starts_with(['&', '*', '!']) {
		return Err(Error::new(format!(
			"yaml: anchors, aliases and tags are not supported (line {line_no})"
		)));
	}
	Ok(())
}

fn is_block_indicator(rest: &str) -> bool {
	matches!(rest, "|" | "|-" | "|+" | ">" | ">-" | ">+")
}

fn fold_lines(lines: &[&str]) -> String {
	let mut out = String::new();
	let mut previous_blank = true;
	for line in lines {
		if line.is_empty() {
			out.push('\n');
			previous_blank = true;
			continue;
		}
		if !out.is_empty() && !previous_blank {
			out.push(' ');
		}
		out.push_str(line);
		previous_blank = false;
	}
	out
}

fn strip_comment(line: &str) -> &str {
	let mut in_double = false;
	let mut in_single = false;
	let mut escaped = false;
	let mut previous: Option<char> = None;
	for (idx, ch) in line.char_indices() {
		if in_double {
			// `\"` is a literal quote, not the end of the scalar: YAML
			// double-quoted scalars use backslash escapes.
			if escaped {
				escaped = false;
			} else if ch == '\\' {
				escaped = true;
			} else if ch == '"' {
				in_double = false;
			}
			previous = Some(ch);
			continue;
		}
		match ch {
			'"' if !in_single => in_double = true,
			'\'' => in_single = !in_single,
			'#' if !in_single => {
				if idx == 0 || previous.is_some_and(|p| p == ' ' || p == '\t') {
					return &line[..idx];
				}
			},
			_ => {},
		}
		previous = Some(ch);
	}
	line
}

fn split_key(content: &str) -> Option<(&str, &str)> {
	let mut in_double = false;
	let mut in_single = false;
	for (idx, ch) in content.char_indices() {
		match ch {
			'"' if !in_single => in_double = !in_double,
			'\'' if !in_double => in_single = !in_single,
			':' if !in_double && !in_single => {
				let rest = &content[idx + 1..];
				if rest.is_empty() || rest.starts_with(' ') {
					return Some((&content[..idx], rest));
				}
			},
			_ => {},
		}
	}
	None
}

fn unquote(raw: &str) -> Result<String> {
	if let Some(inner) = raw.strip_prefix('\'') {
		let Some(inner) = inner.strip_suffix('\'') else {
			return Err(Error::new("yaml: unterminated single-quoted scalar"));
		};
		return Ok(inner.replace("''", "'"));
	}
	if let Some(inner) = raw.strip_prefix('"') {
		let Some(inner) = inner.strip_suffix('"') else {
			return Err(Error::new("yaml: unterminated double-quoted scalar"));
		};
		let mut out = String::with_capacity(inner.len());
		let mut chars = inner.chars();
		while let Some(ch) = chars.next() {
			if ch != '\\' {
				out.push(ch);
				continue;
			}
			let Some(escape) = chars.next() else {
				return Err(Error::new("yaml: dangling escape in double-quoted scalar"));
			};
			// YAML's double-quoted escapes; an unknown one is refused instead of
			// silently dropping the backslash and changing the value.
			let decoded = match escape {
				'0' => '\0',
				'a' => '\u{7}',
				'b' => '\u{8}',
				't' | '\t' => '\t',
				'n' => '\n',
				'v' => '\u{b}',
				'f' => '\u{c}',
				'r' => '\r',
				'e' => '\u{1b}',
				' ' => ' ',
				'"' => '"',
				'/' => '/',
				'\\' => '\\',
				'N' => '\u{85}',
				'_' => '\u{a0}',
				'L' => '\u{2028}',
				'P' => '\u{2029}',
				'x' => read_hex_escape(&mut chars, 2)?,
				'u' => read_hex_escape(&mut chars, 4)?,
				'U' => read_hex_escape(&mut chars, 8)?,
				other => {
					return Err(Error::new(format!(
						"yaml: unsupported escape `\\{other}` in double-quoted scalar"
					)));
				},
			};
			out.push(decoded);
		}
		return Ok(out);
	}
	Ok(raw.to_string())
}

/// One `\xNN`, `\uNNNN` or `\UNNNNNNNN` escape body.
fn read_hex_escape(chars: &mut std::str::Chars<'_>, digits: usize) -> Result<char> {
	let mut code = 0u32;
	for _ in 0..digits {
		let Some(ch) = chars.next() else {
			return Err(Error::new("yaml: truncated hex escape in double-quoted scalar"));
		};
		let Some(value) = ch.to_digit(16) else {
			return Err(Error::new(format!(
				"yaml: invalid hex digit `{ch}` in double-quoted scalar escape"
			)));
		};
		code = code * 16 + value;
	}
	char::from_u32(code).ok_or_else(|| Error::new("yaml: invalid code point in double-quoted scalar"))
}

/// YAML 1.2 core integer (`[-+]?[0-9]+`, `0o…`, `0x…`, `0b…`). Rust's `i64`
/// parser would also take `1_000`-style text the core schema reads as a string.
fn yaml_int(text: &str) -> Option<Value> {
	let (negative, body) = match text.strip_prefix('-') {
		Some(rest) => (true, rest),
		None => (false, text.strip_prefix('+').unwrap_or(text)),
	};
	for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
		if let Some(digits) = body.strip_prefix(prefix) {
			if digits.is_empty() || !digits.chars().all(|ch| ch.is_digit(radix)) {
				return None;
			}
			let magnitude = i128::from(u64::from_str_radix(digits, radix).ok()?);
			return Some(if negative { i128_to_value(-magnitude) } else { i128_to_value(magnitude) });
		}
	}
	if body.is_empty() || !body.bytes().all(|byte| byte.is_ascii_digit()) {
		return None;
	}
	let text = if negative { format!("-{body}") } else { body.to_string() };
	text.parse::<i64>().ok().map(Value::Int)
}

fn i128_to_value(number: i128) -> Value {
	i64::try_from(number).map_or_else(|_| Value::Float(number as f64), Value::Int)
}

/// YAML 1.2 core float (`[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?`,
/// plus `.inf`/`.nan`). Rust's parser also accepts `inf`, `NaN` and underscores,
/// which YAML keeps as strings.
fn yaml_float(text: &str) -> Option<f64> {
	let (sign, body) = match text.strip_prefix('-') {
		Some(rest) => (-1.0, rest),
		None => (1.0, text.strip_prefix('+').unwrap_or(text)),
	};
	match body {
		".inf" | ".Inf" | ".INF" => return Some(sign * f64::INFINITY),
		".nan" | ".NaN" | ".NAN" => return Some(f64::NAN),
		_ => {},
	}
	let (mantissa, exponent) = match body.split_once(['e', 'E']) {
		Some((mantissa, exponent)) => (mantissa, Some(exponent)),
		None => (body, None),
	};
	let (whole, fraction) = match mantissa.split_once('.') {
		Some((whole, fraction)) => (whole, Some(fraction)),
		None => (mantissa, None),
	};
	if whole.is_empty() && fraction.is_none_or(str::is_empty) {
		return None;
	}
	if !whole.bytes().all(|byte| byte.is_ascii_digit())
		|| fraction.is_some_and(|digits| !digits.bytes().all(|byte| byte.is_ascii_digit()))
	{
		return None;
	}
	if let Some(exponent) = exponent {
		let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
		if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
			return None;
		}
	}
	let value: f64 = text.parse().ok()?;
	value.is_finite().then_some(value)
}

fn parse_scalar(raw: &str) -> Result<Value> {
	let text = raw.trim();
	if text.is_empty() {
		return Ok(Value::Null);
	}
	if text.starts_with('"') || text.starts_with('\'') {
		return Ok(Value::Str(unquote(text)?));
	}
	if text.starts_with(['&', '*', '!']) {
		return Err(Error::new(format!(
			"yaml: anchors, aliases and tags are not supported (`{text}`)"
		)));
	}
	if text.starts_with('[') {
		let Some(inner) = text.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
			return Err(Error::new(format!("yaml: unterminated flow sequence `{text}`")));
		};
		let mut items = Vec::new();
		for part in split_flow(inner) {
			if !part.trim().is_empty() {
				items.push(parse_scalar(part)?);
			}
		}
		return Ok(Value::List(items));
	}
	if text.starts_with('{') {
		let Some(inner) = text.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
			return Err(Error::new(format!("yaml: unterminated flow mapping `{text}`")));
		};
		let mut pairs: Vec<(String, Value)> = Vec::new();
		for part in split_flow(inner) {
			if part.trim().is_empty() {
				continue;
			}
			let Some((raw_key, rest)) = split_key(part) else {
				return Err(Error::new(format!("yaml: expected `key: value` in `{text}`")));
			};
			check_key(raw_key, 0)?;
			if raw_key.trim() == "<<" {
				return Err(Error::new(format!("yaml: merge keys are not supported in `{text}`")));
			}
			let key = unquote(raw_key.trim())?;
			if pairs.iter().any(|(existing, _)| *existing == key) {
				return Err(Error::new(format!("yaml: duplicate key `{key}` in `{text}`")));
			}
			pairs.push((key, parse_scalar(rest)?));
		}
		return Ok(Value::Map(pairs));
	}
	match text {
		"null" | "Null" | "NULL" | "~" => Ok(Value::Null),
		"true" | "True" | "TRUE" => Ok(Value::Bool(true)),
		"false" | "False" | "FALSE" => Ok(Value::Bool(false)),
		_ => {
			if let Some(number) = yaml_int(text) {
				return Ok(number);
			}
			if let Some(float) = yaml_float(text) {
				return Ok(Value::Float(float));
			}
			Ok(Value::Str(text.to_string()))
		},
	}
}

fn split_flow(inner: &str) -> Vec<&str> {
	let mut parts = Vec::new();
	let mut start = 0;
	let mut depth = 0usize;
	let mut in_double = false;
	let mut in_single = false;
	for (idx, ch) in inner.char_indices() {
		match ch {
			'"' if !in_single => in_double = !in_double,
			'\'' if !in_double => in_single = !in_single,
			'[' | '{' if !in_double && !in_single => depth += 1,
			']' | '}' if !in_double && !in_single => depth = depth.saturating_sub(1),
			',' if !in_double && !in_single && depth == 0 => {
				parts.push(&inner[start..idx]);
				start = idx + 1;
			},
			_ => {},
		}
	}
	parts.push(&inner[start..]);
	parts
}

/// Serialize back to the subset [`parse`] reads, preserving mapping order.
///
/// Types are re-emitted from [`Value`], so a rewritten document keeps every key
/// (including ones this binary does not understand) but not its comments or
/// formatting — the same contract as the TypeScript writer, which also
/// re-serializes the whole file.
#[must_use]
pub fn to_string(value: &Value) -> String {
	let mut out = String::new();
	match value {
		Value::Map(pairs) if !pairs.is_empty() => write_map(&mut out, pairs, 0),
		Value::List(items) if !items.is_empty() => write_seq(&mut out, items, 0),
		other => {
			out.push_str(&scalar_or_flow(other));
			out.push('\n');
		},
	}
	out
}

fn write_map(out: &mut String, pairs: &[(String, Value)], indent: usize) {
	for (key, value) in pairs {
		out.push_str(&" ".repeat(indent));
		out.push_str(&string_scalar(key));
		out.push(':');
		match value {
			Value::Map(inner) if !inner.is_empty() => {
				out.push('\n');
				write_map(out, inner, indent + 2);
			},
			Value::List(items) if !items.is_empty() => {
				out.push('\n');
				write_seq(out, items, indent + 2);
			},
			other => {
				out.push(' ');
				out.push_str(&scalar_or_flow(other));
				out.push('\n');
			},
		}
	}
}

fn write_seq(out: &mut String, items: &[Value], indent: usize) {
	for item in items {
		out.push_str(&" ".repeat(indent));
		match item {
			// A bare `-` line keeps the nested block unambiguous for the reader;
			// `- key: value` first lines are also accepted, but not emitted.
			Value::Map(inner) if !inner.is_empty() => {
				out.push_str("-\n");
				write_map(out, inner, indent + 2);
			},
			Value::List(inner) if !inner.is_empty() => {
				out.push_str("-\n");
				write_seq(out, inner, indent + 2);
			},
			other => {
				out.push_str("- ");
				out.push_str(&scalar_or_flow(other));
				out.push('\n');
			},
		}
	}
}

fn scalar_or_flow(value: &Value) -> String {
	match value {
		Value::Null => "null".to_string(),
		Value::Bool(flag) => flag.to_string(),
		Value::Int(number) => number.to_string(),
		Value::Float(number) => float_text(*number),
		Value::Str(text) => string_scalar(text),
		Value::List(items) if items.is_empty() => "[]".to_string(),
		Value::Map(pairs) if pairs.is_empty() => "{}".to_string(),
		_ => unreachable!("non-empty collections are emitted as blocks"),
	}
}

fn float_text(number: f64) -> String {
	if number.is_nan() {
		return ".nan".to_string();
	}
	if number.is_infinite() {
		return if number.is_sign_negative() { "-.inf".to_string() } else { ".inf".to_string() };
	}
	let text = format!("{number}");
	// `2` would re-read as an integer; keep the value a float.
	if text.contains('.') || text.contains('e') || text.contains('E') {
		text
	} else {
		format!("{text}.0")
	}
}

fn string_scalar(text: &str) -> String {
	if needs_quotes(text) {
		let mut out = String::with_capacity(text.len() + 2);
		out.push('"');
		for ch in text.chars() {
			match ch {
				'\\' => out.push_str("\\\\"),
				'"' => out.push_str("\\\""),
				'\n' => out.push_str("\\n"),
				'\r' => out.push_str("\\r"),
				'\t' => out.push_str("\\t"),
				'\u{85}' => out.push_str("\\N"),
				'\u{2028}' => out.push_str("\\L"),
				'\u{2029}' => out.push_str("\\P"),
				other if (other as u32) < 0x20 || other as u32 == 0x7f => {
					out.push_str(&format!("\\x{:02x}", other as u32));
				},
				other => out.push(other),
			}
		}
		out.push('"');
		return out;
	}
	text.to_string()
}

/// Quote anything that would not read back as the same string: YAML keywords,
/// numbers, indicators, comment introducers and surrounding whitespace.
fn needs_quotes(text: &str) -> bool {
	if text.is_empty() || text.trim() != text {
		return true;
	}
	if text
		.chars()
		.any(|ch| matches!(ch, '\n' | '\r' | '\t') || ch.is_control())
	{
		return true;
	}
	if text.contains(" #") || text.contains(": ") || text.ends_with(':') {
		return true;
	}
	// Quotes and backslashes would otherwise confuse the line scanners.
	if text.contains('"') || text.contains('\\') {
		return true;
	}
	if text.starts_with(['-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`']) {
		return true;
	}
	if matches!(
		text.to_ascii_lowercase().as_str(),
		"null" | "~" | "true" | "false" | "yes" | "no" | "on" | "off"
	) {
		return true;
	}
	// Anything the YAML reader would turn into a number must stay a string.
	yaml_int(text).is_some() || yaml_float(text).is_some()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_nested_config() {
		let doc = r#"
# comment
defaultModel: openai/gpt-5
providers:
  openai:
    baseUrl: "https://api.openai.com/v1"
    apiKey: OPENAI_API_KEY
    headers:
      X-Title: omp
  local:
    api: openai-completions
approvalMode: write
allow:
  - read
  - bash
cwd: /tmp/work
"#;
		let value = parse(doc).expect("parse");
		assert_eq!(value.get("defaultModel").and_then(Value::as_str), Some("openai/gpt-5"));
		let openai = value
			.get("providers")
			.and_then(|p| p.get("openai"))
			.expect("provider");
		assert_eq!(openai.get("baseUrl").and_then(Value::as_str), Some("https://api.openai.com/v1"));
		assert_eq!(
			openai
				.get("headers")
				.and_then(|h| h.get("X-Title"))
				.and_then(Value::as_str),
			Some("omp")
		);
		let allow = value.get("allow").and_then(Value::as_list).expect("allow");
		assert_eq!(allow.len(), 2);
	}

	#[test]
	fn parses_block_scalar_and_flow_list() {
		let doc = "systemPrompt: |\n  line one\n  line two\nskills: [review, \"test\"]\nempty: >-\n  folded\n  text\n";
		let value = parse(doc).expect("parse");
		assert_eq!(value.get("systemPrompt").and_then(Value::as_str), Some("line one\nline two\n"));
		let skills = value
			.get("skills")
			.and_then(Value::as_list)
			.expect("skills");
		assert_eq!(skills[1].as_str(), Some("test"));
		assert_eq!(value.get("empty").and_then(Value::as_str), Some("folded text"));
	}

	#[test]
	fn sequence_at_key_indent_belongs_to_key() {
		let value = parse("allow:\n- read\n- write\nnext: true\n").expect("parse");
		assert_eq!(value.get("allow").and_then(Value::as_list).map(<[_]>::len), Some(2));
		assert_eq!(value.get("next").and_then(Value::as_bool), Some(true));
	}

	#[test]
	fn escaped_quote_does_not_start_a_comment() {
		let value = parse("key: \"a\\\" #x\"\nnext: true\n").expect("parse");
		assert_eq!(value.get("key").and_then(Value::as_str), Some("a\" #x"));
		assert_eq!(value.get("next").and_then(Value::as_bool), Some(true));
		// A real comment after a closed double-quoted scalar is still stripped.
		let value = parse("key: \"a\" # comment\n").expect("parse");
		assert_eq!(value.get("key").and_then(Value::as_str), Some("a"));
	}

	#[test]
	fn compact_sequence_items_keep_continuation_lines() {
		let doc = "items:\n  -   name: one\n      kind: a\n  -   name: two\n      kind: b\n";
		let value = parse(doc).expect("parse");
		let items = value.get("items").and_then(Value::as_list).expect("items");
		assert_eq!(items.len(), 2);
		assert_eq!(items[0].get("name").and_then(Value::as_str), Some("one"));
		assert_eq!(items[0].get("kind").and_then(Value::as_str), Some("a"));
		assert_eq!(items[1].get("name").and_then(Value::as_str), Some("two"));
		assert_eq!(items[1].get("kind").and_then(Value::as_str), Some("b"));
	}

	#[test]
	fn quoted_numeric_strings_round_trip_as_strings() {
		for text in ["0x10", "0o17", "0b101", "1e3", "5.", ".5", ".inf", "-.nan", "1_000"] {
			let value = Value::Str(text.to_string());
			let document = Value::Map(vec![("custom".to_string(), value.clone())]);
			let yaml = to_string(&document);
			assert!(yaml.contains('"'), "{text} must be quoted: {yaml}");
			assert_eq!(parse(&yaml).expect("parse"), document, "{text}");
		}
	}

	#[test]
	fn numbers_follow_the_yaml_core_schema() {
		let scalar = |text: &str| parse(&format!("v: {text}\n")).expect("parse").get("v").cloned();
		assert_eq!(scalar("0x10"), Some(Value::Int(16)));
		assert_eq!(scalar("0o17"), Some(Value::Int(15)));
		assert_eq!(scalar("0b101"), Some(Value::Int(5)));
		assert_eq!(scalar("-0x10"), Some(Value::Int(-16)));
		assert_eq!(scalar(".5"), Some(Value::Float(0.5)));
		assert_eq!(scalar("5."), Some(Value::Float(5.0)));
		assert_eq!(scalar("1e3"), Some(Value::Float(1000.0)));
		// Not core numbers: Rust spellings and `1_000` stay strings.
		assert_eq!(scalar("inf"), Some(Value::Str("inf".to_string())));
		assert_eq!(scalar("NaN"), Some(Value::Str("NaN".to_string())));
		assert_eq!(scalar("1_000"), Some(Value::Str("1_000".to_string())));
		// `.inf`/`.nan` are core numbers but JSON cannot carry them.
		assert_eq!(scalar(".inf"), Some(Value::Float(f64::INFINITY)));
		assert!(matches!(scalar(".nan"), Some(Value::Float(number)) if number.is_nan()));
	}

	#[test]
	fn anchors_aliases_tags_and_merge_keys_are_refused() {
		for document in [
			"defaultModel: &m anthropic/claude\n",
			"defaultModel: *m\n",
			"defaultModel: !!str x\n",
			"<<: *base\n",
			"base: {<<: *b}\n",
		] {
			assert!(parse(document).is_err(), "{document} must be refused");
		}
	}

	#[test]
	fn double_quoted_escapes_decode_or_fail_closed() {
		let scalar = |text: &str| parse(&format!("v: \"{text}\"\n")).expect("parse").get("v").cloned();
		assert_eq!(scalar("a\\nb"), Some(Value::Str("a\nb".to_string())));
		assert_eq!(scalar("\\u00e9"), Some(Value::Str("é".to_string())));
		assert_eq!(scalar("\\U0001F600"), Some(Value::Str("😀".to_string())));
		assert!(parse("v: \"\\q\"\n").is_err(), "unknown escape must fail");
		assert!(parse("v: \"\\u12\"\n").is_err(), "truncated escape must fail");
	}

	#[test]
	fn block_scalar_keep_chomping_preserves_trailing_blank_lines() {
		let parsed = parse("custom: |+\n  a\n\nnext: 1\n").expect("parse");
		assert_eq!(parsed.get("custom"), Some(&Value::Str("a\n\n".to_string())));
		let clipped = parse("custom: |\n  a\n\nnext: 1\n").expect("parse");
		assert_eq!(clipped.get("custom"), Some(&Value::Str("a\n".to_string())));
	}

	#[test]
	fn rejects_duplicate_and_tabs() {
		assert!(parse("a: 1\na: 2\n").is_err());
		assert!(parse("a:\n\tb: 1\n").is_err());
	}
}
