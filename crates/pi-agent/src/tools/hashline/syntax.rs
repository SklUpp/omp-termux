//! Syntax probe for candidate edit results, mirroring upstream `syntax.ts`.
//!
//! The probe never proves *about* the edit: `false` conflates "does not parse",
//! "unrecognized language" and "cannot tell". Callers only use it to withhold
//! structural permission or to warn that a previously-parsing file stopped.

use std::path::Path;

/// `true` when `text` parses without a syntax error under the language inferred
/// from `path`. Unrecognized languages and parse failures are both `false`.
pub fn parses_cleanly(path: &str, text: &str) -> bool {
	let Some(lang) = pi_ast::SupportLang::from_path(Path::new(path)) else { return false };
	match pi_ast::parse_cache::parse_cached(text, lang) {
		Ok(Some(tree)) => !tree.root_node().has_error(),
		_ => false,
	}
}
