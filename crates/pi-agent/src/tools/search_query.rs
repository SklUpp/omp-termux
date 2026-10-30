//! Structured web-search query parsing shared by search backends.
//!
//! Port of `packages/coding-agent/src/web/search/query.ts`. Agents embed
//! Google-style directives in queries regardless of what the backend parses,
//! so each provider canonicalizes the engine query
//! ([`format_scraper_query`]) and post-filters returned sources leniently
//! ([`filter_source_indices`]): a constraint dimension that would eliminate
//! every result is dropped and reported instead of returning nothing.
//!
//! The timestamp helpers are a localized port of the workspace's
//! `crates/omp-cli/src/session.rs::parse_iso8601_ms` day arithmetic (Howard
//! Hinnant's days-from-civil), extended with `±HH:MM` offsets and 1–9 digit
//! fractions because SearXNG publishes both. `libc::strptime` was rejected:
//! unix-only (no Windows target of this binary) and it yields no portable
//! epoch conversion.

use reqwest::Url;

/// One free-text token of the query (everything that is not a directive).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryTerm {
	/// Term text without quotes or operator prefixes.
	pub text:    String,
	/// Quoted exact phrase (`"like this"`) or verbatim-required (`+term`).
	pub phrase:  bool,
	/// Excluded via `-term` or `NOT term`.
	pub negated: bool,
	/// OR-group id; terms sharing an id are alternatives.
	pub group:   Option<u32>,
}

/// A raw query decomposed into free text plus every recognized constraint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StructuredQuery {
	/// Original query string, verbatim.
	pub raw:                String,
	/// Ordered free-text terms (phrases, exclusions, OR groups).
	pub terms:              Vec<QueryTerm>,
	/// `site:`/`domain:`/`host:` includes — any-of, lowercased, scheme
	/// stripped, may carry a path.
	pub sites:              Vec<String>,
	/// `-site:` exclusions.
	pub excluded_sites:     Vec<String>,
	/// `inurl:`/`url:`/`allinurl:` substrings — all must appear in the URL.
	pub in_url:             Vec<String>,
	/// `-inurl:` substrings — none may appear in the URL.
	pub excluded_in_url:    Vec<String>,
	/// `intitle:`/`title:`/`allintitle:` substrings.
	pub in_title:           Vec<String>,
	/// `-intitle:` substrings.
	pub excluded_in_title:  Vec<String>,
	/// `intext:`/`inbody:`/`inanchor:` body substrings (query-building only).
	pub in_text:            Vec<String>,
	/// `-intext:` body exclusions (query-building only).
	pub excluded_in_text:   Vec<String>,
	/// `filetype:`/`ext:` extensions — any-of, lowercased, no leading dot.
	pub filetypes:          Vec<String>,
	/// `-filetype:`/`-ext:` extensions.
	pub excluded_filetypes: Vec<String>,
	/// Inclusive lower publish-date bound from `after:`/`since:`, `YYYY-MM-DD`.
	pub after:              Option<String>,
	/// Exclusive upper publish-date bound from `before:`/`until:`.
	pub before:             Option<String>,
	/// Language code from `lang:`/`language:`, lowercased.
	pub lang:               Option<String>,
	/// True when any directive or boolean operator was recognized.
	pub has_directives:     bool,
	/// True when any post-filterable constraint is set.
	pub has_constraints:    bool,
}

struct RawToken {
	text:         String,
	quoted:       bool,
	quoted_value: bool,
}

impl RawToken {
	fn word(text: impl Into<String>) -> Self {
		Self { text: text.into(), quoted: false, quoted_value: false }
	}
}

fn is_quote(character: char) -> bool {
	matches!(character, '"' | '\u{201c}' | '\u{201d}')
}

/// Split a raw query into whitespace-delimited tokens, honoring quoted spans.
fn tokenize(raw: &str) -> Vec<RawToken> {
	let characters: Vec<char> = raw.chars().collect();
	let mut tokens = Vec::new();
	let mut index = 0;
	while index < characters.len() {
		let character = characters[index];
		if character.is_whitespace() {
			index += 1;
			continue;
		}
		if is_quote(character) {
			let mut cursor = index + 1;
			let mut buffer = String::new();
			while cursor < characters.len() && !is_quote(characters[cursor]) {
				buffer.push(characters[cursor]);
				cursor += 1;
			}
			let trimmed = buffer.trim();
			if !trimmed.is_empty() {
				tokens.push(RawToken { text: trimmed.to_string(), quoted: true, quoted_value: false });
			}
			index = cursor + 1;
			continue;
		}
		// Bare word; a quote directly after `name:` swallows the quoted span
		// into the same token (`intitle:"budget tips"`).
		let mut buffer = String::new();
		let mut quoted_value = false;
		while index < characters.len() && !characters[index].is_whitespace() {
			let character = characters[index];
			if is_quote(character) && buffer.ends_with(':') {
				let mut cursor = index + 1;
				while cursor < characters.len() && !is_quote(characters[cursor]) {
					buffer.push(characters[cursor]);
					cursor += 1;
				}
				quoted_value = true;
				index = cursor + 1;
				continue;
			}
			if is_quote(character) {
				break;
			}
			buffer.push(character);
			index += 1;
		}
		if !buffer.is_empty() {
			tokens.push(RawToken { text: buffer, quoted: false, quoted_value });
		}
	}
	split_parens(tokens)
}

/// Split leading `(` and unbalanced trailing `)` into standalone tokens so
/// `(react OR vue)` parses while `site:wikipedia.org/Foo_(bar)` stays whole.
fn split_parens(tokens: Vec<RawToken>) -> Vec<RawToken> {
	let mut output = Vec::new();
	for token in tokens {
		if token.quoted || token.quoted_value {
			output.push(token);
			continue;
		}
		let mut text = token.text;
		while text.starts_with('(') {
			output.push(RawToken::word("("));
			text.remove(0);
		}
		let mut trailing = 0;
		while text.ends_with(')') {
			let body = &text[..text.len() - 1];
			let mut depth: i32 = 0;
			for character in body.chars() {
				match character {
					'(' => depth += 1,
					')' => depth -= 1,
					_ => {},
				}
			}
			if depth > 0 {
				break;
			}
			text.truncate(text.len() - 1);
			trailing += 1;
		}
		if !text.is_empty() {
			output.push(RawToken::word(text));
		}
		for _ in 0..trailing {
			output.push(RawToken::word(")"));
		}
	}
	output
}

struct Directive {
	negated: bool,
	name:    String,
	value:   String,
}

/// Parse `[+-]name:value`; names are ASCII letters/hyphens, unknown names
/// stay in the free text.
fn directive(token: &str) -> Option<Directive> {
	let colon = token.find(':')?;
	let mut head = &token[..colon];
	let value = &token[colon + 1..];
	let negated = head.starts_with('-');
	if negated || head.starts_with('+') {
		head = &head[1..];
	}
	if !head.chars().next().is_some_and(|character| character.is_ascii_alphabetic()) {
		return None;
	}
	if !head.chars().all(|character| character.is_ascii_alphabetic() || character == '-') {
		return None;
	}
	Some(Directive { negated, name: head.to_ascii_lowercase(), value: value.to_string() })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
	Site,
	InUrl,
	InTitle,
	InText,
	Filetype,
	Before,
	After,
	Lang,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AllMode {
	InTitle,
	InUrl,
	InText,
}

fn field(name: &str) -> Option<Field> {
	match name {
		"site" | "domain" | "host" => Some(Field::Site),
		"inurl" | "url" => Some(Field::InUrl),
		"intitle" | "title" => Some(Field::InTitle),
		"intext" | "inbody" | "inanchor" => Some(Field::InText),
		"filetype" | "ext" => Some(Field::Filetype),
		"before" | "until" => Some(Field::Before),
		"after" | "since" => Some(Field::After),
		"lang" | "language" => Some(Field::Lang),
		_ => None,
	}
}

fn all_mode(name: &str) -> Option<AllMode> {
	match name {
		"allintitle" => Some(AllMode::InTitle),
		"allinurl" => Some(AllMode::InUrl),
		"allintext" => Some(AllMode::InText),
		_ => None,
	}
}

/// True for operator/paren tokens and recognized directives — anything a bare
/// `name:` must not adopt as its value.
fn is_reserved_token(text: &str) -> bool {
	if matches!(text, "(" | ")" | "OR" | "AND" | "NOT" | "|" | "||" | "&&" | "!") {
		return true;
	}
	directive(text)
		.is_some_and(|parsed| field(&parsed.name).is_some() || all_mode(&parsed.name).is_some())
}

/// Convert date parts to a validated ISO date.
fn iso_date(year: u32, month: u32, day: u32) -> Option<String> {
	if !(1000..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
		return None;
	}
	Some(format!("{year:04}-{month:02}-{day:02}"))
}

/// Parse a `before:`/`after:` value into ISO `YYYY-MM-DD`.
fn parse_date_value(value: &str) -> Option<String> {
	let trimmed = value.trim();
	let fields: Vec<(&str, u32)> = trimmed
		.split(['-', '/', '.'])
		.map(|field| field.parse::<u32>().map(|number| (field, number)).ok())
		.collect::<Option<Vec<_>>>()?;
	if !fields.iter().all(|(field, _)| !field.is_empty() && field.bytes().all(|byte| byte.is_ascii_digit())) {
		return None;
	}
	match fields.as_slice() {
		[(year, year_value)] if year.len() == 4 => iso_date(*year_value, 1, 1),
		[(year, year_value), (month, month_value)] if year.len() == 4 => {
			iso_date(*year_value, *month_value, 1)
		},
		[(year, year_value), (month, month_value), (day, day_value)] if year.len() == 4 => {
			iso_date(*year_value, *month_value, *day_value)
		},
		[(month, month_value), (day, day_value), (year, year_value)] if year.len() == 4 => {
			let (month, day) = if *month_value > 12 && *day_value <= 12 {
				(*day_value, *month_value)
			} else {
				(*month_value, *day_value)
			};
			iso_date(*year_value, month, day)
		},
		_ => None,
	}
}

/// Lowercase a `site:` value and strip scheme, `*.` wildcard, trailing
/// slash/dot.
fn normalize_site(value: &str) -> String {
	let mut site = value.trim().to_ascii_lowercase();
	if let Some(index) = site.find("://") {
		let scheme = &site[..index];
		let valid = scheme.chars().next().is_some_and(|character| character.is_ascii_alphabetic())
			&& scheme
				.chars()
				.all(|character| character.is_ascii_alphanumeric() || matches!(character, '+' | '.' | '-'));
		if valid {
			site = site[index + 3..].to_string();
		}
	}
	if let Some(stripped) = site.strip_prefix("*.") {
		site = stripped.to_string();
	}
	site.trim_end_matches(['/', '.']).to_string()
}

struct Parser {
	query:        StructuredQuery,
	negate_next:  bool,
	or_pending:   bool,
	last_was_term: bool,
	group_seq:    u32,
	all_mode:     Option<AllMode>,
}

impl Parser {
	fn new(raw: &str) -> Self {
		Self {
			query: StructuredQuery { raw: raw.to_string(), ..StructuredQuery::default() },
			negate_next: false,
			or_pending: false,
			last_was_term: false,
			group_seq: 0,
			all_mode: None,
		}
	}

	fn push_constraint(&mut self, field: Field, value: &str, negated: bool) {
		self.query.has_directives = true;
		self.or_pending = false;
		self.last_was_term = false;
		let value = value.trim();
		if value.is_empty() {
			return;
		}
		match field {
			Field::Site => {
				let site = normalize_site(value);
				if !site.is_empty() {
					if negated { self.query.excluded_sites.push(site); } else { self.query.sites.push(site); }
				}
			},
			Field::InUrl => {
				if negated { self.query.excluded_in_url.push(value.to_string()); } else { self.query.in_url.push(value.to_string()); }
			},
			Field::InTitle => {
				if negated { self.query.excluded_in_title.push(value.to_string()); } else { self.query.in_title.push(value.to_string()); }
			},
			Field::InText => {
				if negated { self.query.excluded_in_text.push(value.to_string()); } else { self.query.in_text.push(value.to_string()); }
			},
			Field::Filetype => {
				let extension = value.trim_start_matches('.').to_ascii_lowercase();
				if !extension.is_empty() {
					if negated {
						self.query.excluded_filetypes.push(extension);
					} else {
						self.query.filetypes.push(extension);
					}
				}
			},
			Field::Before | Field::After | Field::Lang => {},
		}
	}

	fn push_constraint_all(&mut self, mode: AllMode, value: &str, negated: bool) {
		let field = match mode {
			AllMode::InTitle => Field::InTitle,
			AllMode::InUrl => Field::InUrl,
			AllMode::InText => Field::InText,
		};
		self.push_constraint(field, value, negated);
	}

	fn push_term(&mut self, text: String, phrase: bool) {
		let negated = std::mem::take(&mut self.negate_next);
		if let Some(mode) = self.all_mode {
			self.push_constraint_all(mode, &text, negated);
			return;
		}
		let mut term = QueryTerm { text, phrase, negated, group: None };
		if self.or_pending
			&& self.last_was_term
			&& let Some(previous) = self.query.terms.last_mut()
		{
			let group = match previous.group {
				Some(group) => group,
				None => {
					self.group_seq += 1;
					self.group_seq
				},
			};
			previous.group = Some(group);
			term.group = Some(group);
		}
		self.or_pending = false;
		self.last_was_term = true;
		self.query.terms.push(term);
	}
}

/// Parse a raw query into a [`StructuredQuery`].
///
/// Lenient by construction: unknown `name:value` tokens (URLs, `C:\paths`,
/// `TS2345:`) stay in the free text verbatim, and a directive with an
/// unparseable value degrades to a plain term instead of being dropped.
pub fn parse_search_query(raw: &str) -> StructuredQuery {
	let tokens = tokenize(raw);
	let mut parser = Parser::new(raw);
	let mut index = 0;
	while index < tokens.len() {
		let token = &tokens[index];
		if token.quoted {
			parser.push_term(token.text.clone(), true);
			index += 1;
			continue;
		}
		let text = token.text.as_str();
		if text == "(" || text == ")" {
			index += 1;
			continue;
		}
		if matches!(text, "OR" | "|" | "||") {
			parser.or_pending = true;
			parser.query.has_directives = true;
			index += 1;
			continue;
		}
		if matches!(text, "AND" | "&&") {
			parser.query.has_directives = true;
			index += 1;
			continue;
		}
		if matches!(text, "NOT" | "!") {
			parser.negate_next = true;
			parser.query.has_directives = true;
			index += 1;
			continue;
		}
		if text == "-" || text == "+" {
			// `-"exact phrase"` tokenizes as `-` + phrase; carry the negation over.
			if text == "-" && tokens.get(index + 1).is_some_and(|next| next.quoted) {
				parser.negate_next = true;
			}
			index += 1;
			continue;
		}

		if let Some(parsed) = directive(text) {
			if let Some(mode) = all_mode(&parsed.name) {
				parser.all_mode = Some(mode);
				parser.query.has_directives = true;
				let inline = parsed.value.trim();
				if !inline.is_empty() {
					parser.push_constraint_all(mode, inline, parsed.negated);
				}
				index += 1;
				continue;
			}
			if let Some(field) = field(&parsed.name) {
				let mut value = parsed.value.trim().to_string();
				let mut adopted_next = false;
				if value.is_empty()
					&& let Some(next) = tokens.get(index + 1)
					&& (next.quoted || !is_reserved_token(&next.text))
				{
					value = next.text.trim().to_string();
					adopted_next = true;
				}
				if value.is_empty() {
					parser.query.has_directives = true;
					index += 1;
					continue;
				}
				let negated = parsed.negated || std::mem::take(&mut parser.negate_next);
				match field {
					Field::Before | Field::After => {
						if let Some(iso) = parse_date_value(&value) {
							if field == Field::Before {
								parser.query.before = Some(iso);
							} else {
								parser.query.after = Some(iso);
							}
							parser.query.has_directives = true;
							parser.or_pending = false;
							parser.last_was_term = false;
						} else {
							parser.push_term(text.to_string(), false);
						}
					},
					Field::Lang => {
						parser.query.lang = Some(value.to_ascii_lowercase());
						parser.query.has_directives = true;
						parser.or_pending = false;
						parser.last_was_term = false;
					},
					constraint => parser.push_constraint(constraint, &value, negated),
				}
				index += if adopted_next { 2 } else { 1 };
				continue;
			}
		}

		// Plain term with optional +/- prefix.
		let mut body = text.to_string();
		if body.starts_with('-') && body.len() > 1 {
			parser.negate_next = true;
			parser.query.has_directives = true;
			body = body.trim_start_matches('-').to_string();
			if body.is_empty() {
				index += 1;
				continue;
			}
			// `- site:x` arrives pre-split; re-check the directive.
			if let Some(negated) = directive(&body)
				&& let Some(negated_field) = field(&negated.name)
				&& !matches!(negated_field, Field::Before | Field::After | Field::Lang)
			{
				parser.negate_next = false;
				parser.push_constraint(negated_field, negated.value.trim(), true);
				index += 1;
				continue;
			}
			parser.push_term(body, false);
			index += 1;
			continue;
		}
		if body.starts_with('+') && body.len() > 1 {
			// Legacy Google `+term`: verbatim/required — treat as an exact phrase.
			parser.push_term(body[1..].to_string(), true);
			parser.query.has_directives = true;
			index += 1;
			continue;
		}
		parser.push_term(body, false);
		index += 1;
	}

	let has_constraints = !parser.query.sites.is_empty()
		|| !parser.query.excluded_sites.is_empty()
		|| !parser.query.in_url.is_empty()
		|| !parser.query.excluded_in_url.is_empty()
		|| !parser.query.in_title.is_empty()
		|| !parser.query.excluded_in_title.is_empty()
		|| !parser.query.filetypes.is_empty()
		|| !parser.query.excluded_filetypes.is_empty()
		|| parser.query.after.is_some()
		|| parser.query.before.is_some();
	parser.query.has_constraints = has_constraints;
	parser.query
}

fn quote_value(value: &str) -> String {
	if value.chars().any(char::is_whitespace) { format!("\"{value}\"") } else { value.to_string() }
}

fn render_term(term: &QueryTerm) -> String {
	let body = if term.phrase { format!("\"{}\"", term.text) } else { term.text.clone() };
	if term.negated { format!("-{body}") } else { body }
}

fn render_terms(terms: &[QueryTerm]) -> String {
	let mut parts: Vec<String> = Vec::new();
	let mut index = 0;
	while index < terms.len() {
		if let Some(group) = terms[index].group {
			let mut members: Vec<String> = Vec::new();
			let mut cursor = index;
			while cursor < terms.len() && terms[cursor].group == Some(group) {
				members.push(render_term(&terms[cursor]));
				cursor += 1;
			}
			index = cursor;
			if members.len() > 1 {
				parts.push(format!("({})", members.join(" OR ")));
			} else if let Some(member) = members.into_iter().next() {
				parts.push(member);
			}
			continue;
		}
		parts.push(render_term(&terms[index]));
		index += 1;
	}
	parts.join(" ")
}

/// Rebuild a query string with full Google-style syntax (SearXNG parses the
/// classic operator set). Never returns an empty string for a non-empty
/// input: a directives-only query falls back to constraint values, then `raw`.
pub fn format_query(query: &StructuredQuery) -> String {
	let mut parts: Vec<String> = Vec::new();
	let text = render_terms(&query.terms);
	if !text.is_empty() {
		parts.push(text);
	}
	if query.sites.len() > 1 {
		parts.push(format!(
			"({})",
			query.sites.iter().map(|site| format!("site:{site}")).collect::<Vec<_>>().join(" OR ")
		));
	} else {
		parts.extend(query.sites.iter().map(|site| format!("site:{site}")));
	}
	parts.extend(query.excluded_sites.iter().map(|site| format!("-site:{site}")));
	parts.extend(query.in_url.iter().map(|value| format!("inurl:{}", quote_value(value))));
	parts.extend(query.excluded_in_url.iter().map(|value| format!("-inurl:{}", quote_value(value))));
	parts.extend(query.in_title.iter().map(|value| format!("intitle:{}", quote_value(value))));
	parts.extend(query.excluded_in_title.iter().map(|value| format!("-intitle:{}", quote_value(value))));
	parts.extend(query.in_text.iter().map(|value| format!("intext:{}", quote_value(value))));
	parts.extend(query.excluded_in_text.iter().map(|value| format!("-intext:{}", quote_value(value))));
	if query.filetypes.len() > 1 {
		parts.push(format!(
			"({})",
			query.filetypes.iter().map(|extension| format!("filetype:{extension}")).collect::<Vec<_>>().join(" OR ")
		));
	} else {
		parts.extend(query.filetypes.iter().map(|extension| format!("filetype:{extension}")));
	}
	parts.extend(query.excluded_filetypes.iter().map(|extension| format!("-filetype:{extension}")));
	if let Some(after) = &query.after {
		parts.push(format!("after:{after}"));
	}
	if let Some(before) = &query.before {
		parts.push(format!("before:{before}"));
	}
	let mut result = parts.join(" ").trim().to_string();
	if result.is_empty() {
		// Directives-only query: search the constraint values as plain keywords
		// so the engine still gets a meaningful query.
		result = query
			.sites
			.iter()
			.chain(&query.in_title)
			.chain(&query.in_url)
			.chain(&query.in_text)
			.chain(&query.filetypes)
			.map(String::as_str)
			.collect::<Vec<_>>()
			.join(" ")
			.trim()
			.to_string();
	}
	if result.is_empty() { query.raw.trim().to_string() } else { result }
}

/// Build the engine query for a credential-free HTML engine (SearXNG).
///
/// Canonicalizes directives, after demoting the operators that zero-match
/// across the scraper set: a path-carrying `site:` and every `inurl:` value
/// become plain keywords, because SearXNG matches `site:` against a bare
/// domain only. Negated forms pass through untouched. Directive-free queries
/// pass through byte-identical.
pub fn format_scraper_query(raw: &str) -> String {
	let parsed = parse_search_query(raw);
	if !parsed.has_directives {
		return raw.to_string();
	}
	let mut downgraded = parsed.clone();
	let demoted: Vec<String> = downgraded
		.sites
		.iter()
		.filter(|site| site.contains('/'))
		.cloned()
		.chain(downgraded.in_url.iter().cloned())
		.collect();
	downgraded.sites.retain(|site| !site.contains('/'));
	downgraded.in_url.clear();
	for text in demoted {
		downgraded.terms.push(QueryTerm { text, ..QueryTerm::default() });
	}
	format_query(&downgraded)
}

/// A search result view for the lenient constraint pass.
pub trait ConstraintSource {
	fn constraint_url(&self) -> &str;
	fn constraint_title(&self) -> &str;
	/// Publish time in epoch milliseconds; `None` when unknown (undated
	/// sources cannot violate a date bound).
	fn constraint_time_ms(&self) -> Option<i64>;
}

fn host_and_path(url: &str) -> Option<(String, String)> {
	let parsed = Url::parse(url).ok()?;
	let host = parsed.host_str()?.to_ascii_lowercase();
	Some((host, parsed.path().to_string()))
}

/// `site:` matcher: exact host or subdomain; a path-carrying `site:` also
/// requires the URL path to start with that path.
pub fn matches_site(url: &str, site: &str) -> bool {
	let Some((host, path)) = host_and_path(url) else {
		return false;
	};
	let (site_host, site_path) = match site.split_once('/') {
		Some((host, path)) => (host, format!("/{path}")),
		None => (site, String::new()),
	};
	if host != site_host && !host.ends_with(&format!(".{site_host}")) {
		return false;
	}
	if !site_path.is_empty() && !path.to_ascii_lowercase().starts_with(&site_path.to_ascii_lowercase()) {
		return false;
	}
	true
}

/// `filetype:` matcher: URL pathname ends with `.ext`.
fn matches_filetype(url: &str, extension: &str) -> bool {
	host_and_path(url).is_some_and(|(_, path)| path.to_ascii_lowercase().ends_with(&format!(".{extension}")))
}

/// Lenient post-filter: applies each constraint dimension in turn, skipping
/// (and reporting) any dimension that would eliminate every remaining source.
/// Guarantees a non-empty result for a non-empty input.
pub fn filter_source_indices<S: ConstraintSource>(
	sources: &[S],
	query: &StructuredQuery,
) -> (Vec<usize>, Vec<String>) {
	let mut current: Vec<usize> = (0..sources.len()).collect();
	let mut dropped: Vec<String> = Vec::new();
	if current.is_empty() {
		return (current, dropped);
	}
	for (label, predicate) in dimensions(query) {
		let kept: Vec<usize> = current
			.iter()
			.copied()
			.filter(|index| predicate(&sources[*index]))
			.collect();
		if kept.is_empty() {
			dropped.push(label);
		} else {
			current = kept;
		}
	}
	(current, dropped)
}

type Predicate<S> = Box<dyn Fn(&S) -> bool>;

fn dimensions<S: ConstraintSource>(query: &StructuredQuery) -> Vec<(String, Predicate<S>)> {
	let mut dimensions: Vec<(String, Predicate<S>)> = Vec::new();
	let lower = |value: &str| value.to_ascii_lowercase();

	if !query.sites.is_empty() {
		let sites = query.sites.clone();
		let label = sites.iter().map(|site| format!("site:{site}")).collect::<Vec<_>>().join(" OR ");
		dimensions.push((label, Box::new(move |source: &S| sites.iter().any(|site| matches_site(source.constraint_url(), site)))));
	}
	if !query.excluded_sites.is_empty() {
		let sites = query.excluded_sites.clone();
		let label = sites.iter().map(|site| format!("-site:{site}")).collect::<Vec<_>>().join(" ");
		dimensions.push((label, Box::new(move |source: &S| !sites.iter().any(|site| matches_site(source.constraint_url(), site)))));
	}
	if !query.in_url.is_empty() {
		let needles = query.in_url.iter().map(|value| value.to_ascii_lowercase()).collect::<Vec<_>>();
		let label = query.in_url.iter().map(|value| format!("inurl:{value}")).collect::<Vec<_>>().join(" ");
		dimensions.push((label, Box::new(move |source: &S| {
			let url = lower(source.constraint_url());
			needles.iter().all(|needle| url.contains(needle))
		})));
	}
	if !query.excluded_in_url.is_empty() {
		let needles = query.excluded_in_url.iter().map(|value| value.to_ascii_lowercase()).collect::<Vec<_>>();
		let label = query.excluded_in_url.iter().map(|value| format!("-inurl:{value}")).collect::<Vec<_>>().join(" ");
		dimensions.push((label, Box::new(move |source: &S| {
			let url = lower(source.constraint_url());
			!needles.iter().any(|needle| url.contains(needle))
		})));
	}
	if !query.in_title.is_empty() {
		let needles = query.in_title.iter().map(|value| value.to_ascii_lowercase()).collect::<Vec<_>>();
		let label = query.in_title.iter().map(|value| format!("intitle:{value}")).collect::<Vec<_>>().join(" ");
		dimensions.push((label, Box::new(move |source: &S| {
			let title = lower(source.constraint_title());
			needles.iter().all(|needle| title.contains(needle))
		})));
	}
	if !query.excluded_in_title.is_empty() {
		let needles = query.excluded_in_title.iter().map(|value| value.to_ascii_lowercase()).collect::<Vec<_>>();
		let label = query
			.excluded_in_title
			.iter()
			.map(|value| format!("-intitle:{value}"))
			.collect::<Vec<_>>()
			.join(" ");
		dimensions.push((label, Box::new(move |source: &S| {
			let title = lower(source.constraint_title());
			!needles.iter().any(|needle| title.contains(needle))
		})));
	}
	if !query.filetypes.is_empty() {
		let extensions = query.filetypes.clone();
		let label = extensions
			.iter()
			.map(|extension| format!("filetype:{extension}"))
			.collect::<Vec<_>>()
			.join(" OR ");
		dimensions.push((label, Box::new(move |source: &S| {
			extensions.iter().any(|extension| matches_filetype(source.constraint_url(), extension))
		})));
	}
	if !query.excluded_filetypes.is_empty() {
		let extensions = query.excluded_filetypes.clone();
		let label = extensions
			.iter()
			.map(|extension| format!("-filetype:{extension}"))
			.collect::<Vec<_>>()
			.join(" ");
		dimensions.push((label, Box::new(move |source: &S| {
			!extensions.iter().any(|extension| matches_filetype(source.constraint_url(), extension))
		})));
	}
	if query.after.is_some() || query.before.is_some() {
		let after_ms = query.after.as_deref().and_then(iso_date_ms);
		let before_ms = query.before.as_deref().and_then(iso_date_ms);
		let label = [
			query.after.as_ref().map(|after| format!("after:{after}")),
			query.before.as_ref().map(|before| format!("before:{before}")),
		]
		.into_iter()
		.flatten()
		.collect::<Vec<_>>()
		.join(" ");
		dimensions.push((label, Box::new(move |source: &S| {
			let Some(time) = source.constraint_time_ms() else {
				return true;
			};
			if after_ms.is_some_and(|after| time < after) {
				return false;
			}
			if before_ms.is_some_and(|before| time >= before) {
				return false;
			}
			true
		})));
	}
	dimensions
}

/// Parse an RFC 3339 timestamp into epoch milliseconds.
///
/// Localized port of the workspace helper's day arithmetic (Hinnant
/// days-from-civil) extended with `±HH:MM`/`±HHMM` offsets and 1–9 digit
/// fractions. A missing offset is read as UTC so repeated calls agree.
pub fn parse_timestamp_ms(value: &str) -> Option<i64> {
	let (date, rest) = match value.split_once('T').or_else(|| value.split_once(' ')) {
		Some((date, rest)) => (date, rest),
		// Date-only values parse as UTC midnight, like `Date.parse`.
		None => (value, "00:00:00"),
	};
	let parts: Vec<&str> = date.split('-').collect();
	let [year, month, day] = parts.as_slice() else {
		return None;
	};
	if year.len() != 4 || month.len() != 2 || day.len() != 2 {
		return None;
	}
	if !year.bytes().chain(month.bytes()).chain(day.bytes()).all(|byte| byte.is_ascii_digit()) {
		return None;
	}
	let year: i64 = year.parse().ok()?;
	let month: u32 = month.parse().ok()?;
	let day: u32 = day.parse().ok()?;
	if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
		return None;
	}

	let (clock, offset_minutes) = if let Some(clock) = rest.strip_suffix('Z') {
		(clock, 0)
	} else if let Some(index) = rest.rfind(['+', '-']) {
		(&rest[..index], parse_offset(&rest[index..])?)
	} else {
		(rest, 0)
	};
	let (hours_minutes_seconds, fraction) = match clock.split_once('.') {
		Some((clock, fraction)) => (clock, Some(fraction)),
		None => (clock, None),
	};
	let clock_parts: Vec<&str> = hours_minutes_seconds.split(':').collect();
	let [hour, minute, second] = clock_parts.as_slice() else {
		return None;
	};
	if hour.len() != 2 || minute.len() != 2 || second.len() != 2 {
		return None;
	}
	if !hour.bytes().chain(minute.bytes()).chain(second.bytes()).all(|byte| byte.is_ascii_digit()) {
		return None;
	}
	let hour: i64 = hour.parse().ok()?;
	let minute: i64 = minute.parse().ok()?;
	let second: i64 = second.parse().ok()?;
	if hour >= 24 || minute >= 60 || second >= 60 {
		return None;
	}
	let millis = match fraction {
		Some(fraction) => {
			if fraction.is_empty()
				|| fraction.len() > 9
				|| !fraction.bytes().all(|byte| byte.is_ascii_digit())
			{
				return None;
			}
			let mut digits = fraction.to_string();
			while digits.len() < 3 {
				digits.push('0');
			}
			digits[..3].parse::<i64>().ok()?
		},
		None => 0,
	};

	let days = days_from_civil(year, month, day);
	let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second;
	Some(seconds * 1_000 + millis - offset_minutes * 60_000)
}

/// Epoch milliseconds of an ISO `YYYY-MM-DD` date at UTC midnight.
pub fn iso_date_ms(date: &str) -> Option<i64> {
	let parts: Vec<&str> = date.split('-').collect();
	let [year, month, day] = parts.as_slice() else {
		return None;
	};
	if year.len() != 4 || month.len() != 2 || day.len() != 2 {
		return None;
	}
	let year: i64 = year.parse().ok()?;
	let month: u32 = month.parse().ok()?;
	let day: u32 = day.parse().ok()?;
	if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
		return None;
	}
	Some(days_from_civil(year, month, day) * 86_400_000)
}

fn parse_offset(value: &str) -> Option<i64> {
	let (sign, body) = if let Some(body) = value.strip_prefix('+') {
		(1, body)
	} else if let Some(body) = value.strip_prefix('-') {
		(-1, body)
	} else {
		return None;
	};
	// Validate before slicing: `body.len() == 4` counts bytes, so a stray
	// multi-byte character (`+1€`, `+😀`) would put a fixed byte offset inside
	// a UTF-8 sequence and panic.
	if !body.is_ascii() {
		return None;
	}
	let (hours, minutes) = match body.split_once(':') {
		Some((hours, minutes)) => (hours, minutes),
		None if body.len() == 4 => (&body[..2], &body[2..]),
		None => return None,
	};
	if hours.len() != 2 || minutes.len() != 2 {
		return None;
	}
	if !hours.bytes().chain(minutes.bytes()).all(|byte| byte.is_ascii_digit()) {
		return None;
	}
	let hours: i64 = hours.parse().ok()?;
	let minutes: i64 = minutes.parse().ok()?;
	if hours > 23 || minutes > 59 {
		return None;
	}
	Some(sign * (hours * 60 + minutes))
}

/// Howard Hinnant's days-from-civil (proleptic Gregorian, 1970-01-01 = 0).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
	let year = if month <= 2 { year - 1 } else { year };
	let era = if year >= 0 { year } else { year - 399 } / 400;
	let year_of_era = (year - era * 400) as i64;
	let month_prime = ((month as i64) + 9) % 12;
	let day_of_year = (153 * month_prime + 2) / 5 + day as i64 - 1;
	let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
	era * 146_097 + day_of_era - 719_468
}

fn is_leap_year(year: i64) -> bool {
	(year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
	match month {
		1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
		4 | 6 | 9 | 11 => 30,
		2 if is_leap_year(year) => 29,
		2 => 28,
		_ => 0,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	struct Source {
		url:    &'static str,
		title:  &'static str,
		time:   Option<i64>,
	}

	impl ConstraintSource for Source {
		fn constraint_url(&self) -> &str {
			self.url
		}

		fn constraint_title(&self) -> &str {
			self.title
		}

		fn constraint_time_ms(&self) -> Option<i64> {
			self.time
		}
	}

	#[test]
	fn parses_directives_and_boolean_operators() {
		let query = parse_search_query("rust \"async runtime\" OR tokio -nightly site:docs.rs inurl:std");
		assert_eq!(query.sites, ["docs.rs"]);
		assert_eq!(query.in_url, ["std"]);
		assert!(query.terms.iter().any(|term| term.phrase && term.text == "async runtime"));
		assert!(query.terms.iter().any(|term| term.negated && term.text == "nightly"));
		assert!(query.terms.iter().any(|term| term.group.is_some()));
		assert!(query.has_directives && query.has_constraints);
	}

	#[test]
	fn adopts_next_token_only_for_non_reserved_directives() {
		let query = parse_search_query("site: example.com rust");
		assert_eq!(query.sites, ["example.com"]);
		let query = parse_search_query("site: OR rust");
		assert!(query.sites.is_empty());
	}

	#[test]
	fn maps_language_and_date_bounds() {
		let query = parse_search_query("lang:de after:2024 before:2025-06 rust");
		assert_eq!(query.lang.as_deref(), Some("de"));
		assert_eq!(query.after.as_deref(), Some("2024-01-01"));
		assert_eq!(query.before.as_deref(), Some("2025-06-01"));
	}

	#[test]
	fn formats_scraper_query_with_structural_demotion() {
		assert_eq!(format_scraper_query("plain rust async"), "plain rust async");
		assert_eq!(
			format_scraper_query("site:github.com/anthropics rust"),
			"rust github.com/anthropics"
		);
		assert_eq!(
			format_scraper_query("site:arxiv.org inurl:pdf transformers"),
			"transformers pdf site:arxiv.org"
		);
		assert_eq!(format_scraper_query("rust -site:spam.example"), "rust -site:spam.example");
	}

	#[test]
	fn relaxes_constraints_instead_of_returning_nothing() {
		let sources = vec![
			Source { url: "https://other.example/a", title: "Alpha", time: None },
			Source { url: "https://other.example/b", title: "Beta", time: None },
		];
		let query = parse_search_query("site:example.com");
		let (kept, dropped) = filter_source_indices(&sources, &query);
		assert_eq!(kept, [0, 1], "site dimension wiped everything and was relaxed");
		assert_eq!(dropped, ["site:example.com"]);

		let query = parse_search_query("intitle:Alpha");
		let (kept, dropped) = filter_source_indices(&sources, &query);
		assert_eq!(kept, [0], "a satisfiable dimension is enforced");
		assert!(dropped.is_empty());

		let query = parse_search_query("rust");
		let (kept, dropped) = filter_source_indices(&sources, &query);
		assert_eq!(kept, [0, 1]);
		assert!(dropped.is_empty());
	}

	#[test]
	fn date_bounds_pass_undated_sources_and_filter_dated_ones() {
		let sources = vec![
			Source { url: "https://a.example", title: "", time: Some(iso_date_ms("2020-01-01").unwrap()) },
			Source { url: "https://b.example", title: "", time: None },
		];
		let query = parse_search_query("rust after:2024-01-01");
		let (kept, dropped) = filter_source_indices(&sources, &query);
		assert_eq!(kept, [1], "undated source passes, old source filtered");
		assert!(dropped.is_empty());
	}

	#[test]
	fn parses_rfc3339_with_offsets_and_fractions() {
		assert_eq!(parse_timestamp_ms("1970-01-01T00:00:00Z"), Some(0));
		assert_eq!(parse_timestamp_ms("1970-01-01T02:00:00+02:00"), Some(0));
		assert_eq!(parse_timestamp_ms("1969-12-31T23:00:00-01:00"), Some(0));
		assert_eq!(parse_timestamp_ms("1970-01-01T00:00:00.123Z"), Some(123));
		assert_eq!(parse_timestamp_ms("1970-01-01T00:00:00.123456Z"), Some(123));
		assert_eq!(parse_timestamp_ms("2026-02-31T00:00:00Z"), None);
		assert_eq!(parse_timestamp_ms("2026-13-01T00:00:00Z"), None);
		assert_eq!(parse_timestamp_ms("not a date"), None);
	}

	#[test]
	fn malformed_offsets_and_truncations_return_none_instead_of_panicking() {
		// Byte offsets are only sliced after an ASCII check: a multi-byte
		// character inside the offset must not land mid-sequence.
		assert_eq!(parse_timestamp_ms("2026-01-01T00:00:00+1€"), None);
		assert_eq!(parse_timestamp_ms("2026-01-01T00:00:00+01€0"), None);
		assert_eq!(parse_timestamp_ms("2026-01-01T00:00:00+😀"), None);
		assert_eq!(parse_timestamp_ms("2026-01-01T00:00:00.123€Z"), None);
		assert_eq!(parse_timestamp_ms("2026-01-01T00:00:00+1é:00"), None);
		// Truncated clock fields and offsets.
		assert_eq!(parse_timestamp_ms("2026-01-01T00:00"), None);
		assert_eq!(parse_timestamp_ms("2026-01-01T"), None);
		assert_eq!(parse_timestamp_ms("2026-01-01T00:00:00+01"), None);
		assert_eq!(
			parse_timestamp_ms("2026-01-01T00:00:00+0100"),
			Some(iso_date_ms("2026-01-01").unwrap() - 3_600_000)
		);
	}

	#[test]
	fn matches_sites_and_filetypes_like_upstream() {
		assert_eq!(normalize_site("https://*.Docs.rs/"), "docs.rs");
		assert!(matches_site("https://docs.rs/std", "docs.rs"));
		assert!(matches_site("https://sub.docs.rs/x", "docs.rs"));
		assert!(matches_site("https://github.com/anthropics/x", "github.com/anthropics"));
		assert!(!matches_site("https://github.com/other/x", "github.com/anthropics"));
		assert!(!matches_site("https://evil-docs.rs/x", "docs.rs"));
		assert!(matches_filetype("https://example.org/a/b.pdf", "pdf"));
		assert!(!matches_filetype("https://example.org/a/b.html", "pdf"));
	}
}
