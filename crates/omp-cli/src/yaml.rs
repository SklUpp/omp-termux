//! Minimal YAML subset reader for omp config files.
//!
//! Supported: block mappings, block sequences, `key:` with a nested block at
//! the same indent, flow sequences (`[a, b]`), single/double quoted scalars,
//! comments, and `|`/`>` block scalars (with `-`/`+` chomping). Anchors, tags,
//! aliases, merge keys and multi-document streams are deliberately unsupported
//! (`ponytail:` a full YAML engine is a dependency, not a feature we need —
//! upgrade to `serde_yaml` only if omp starts shipping YAML we do not author).

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
		while raw.last().is_some_and(|(indent, _)| *indent == usize::MAX) {
			raw.pop();
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
			match chars.next() {
				Some('n') => out.push('\n'),
				Some('t') => out.push('\t'),
				Some('r') => out.push('\r'),
				Some(other) => out.push(other),
				None => return Err(Error::new("yaml: dangling escape in double-quoted scalar")),
			}
		}
		return Ok(out);
	}
	Ok(raw.to_string())
}

fn parse_scalar(raw: &str) -> Result<Value> {
	let text = raw.trim();
	if text.is_empty() {
		return Ok(Value::Null);
	}
	if text.starts_with('"') || text.starts_with('\'') {
		return Ok(Value::Str(unquote(text)?));
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
	match text {
		"null" | "Null" | "NULL" | "~" => Ok(Value::Null),
		"true" | "True" | "TRUE" => Ok(Value::Bool(true)),
		"false" | "False" | "FALSE" => Ok(Value::Bool(false)),
		_ => {
			if let Ok(int) = text.parse::<i64>() {
				return Ok(Value::Int(int));
			}
			if let Ok(float) = text.parse::<f64>() {
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
	fn rejects_duplicate_and_tabs() {
		assert!(parse("a: 1\na: 2\n").is_err());
		assert!(parse("a:\n\tb: 1\n").is_err());
	}
}
