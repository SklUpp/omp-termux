//! `.editorconfig` formatting hints for LSP `textDocument/formatting`.
//!
//! Mirrors the editorconfig subset consumed by the TypeScript runtime: nearest
//! `.editorconfig` wins, `root = true` stops discovery, and sections apply in
//! file order from the chain root down to the file's directory. Discovery is
//! best-effort; filesystem errors mean "no hint".

use std::{
	collections::HashMap,
	path::{Path, PathBuf},
};

use pi_walker::CompiledWalkGlob;

/// Indent widths are clamped to the same range as the TypeScript runtime.
pub const MIN_TAB_WIDTH: u32 = 1;
pub const MAX_TAB_WIDTH: u32 = 16;
const NAME_MAX_BYTES: usize = 255;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EditorConfigFormatting {
	pub tab_size:       Option<u32>,
	pub insert_spaces:  Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndentStyle {
	Space,
	Tab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndentSize {
	Spaces(u32),
	Tab,
}

#[derive(Debug, Default)]
struct Match {
	style:      Option<IndentStyle>,
	indent:     Option<IndentSize>,
	tab_width:  Option<u32>,
}

#[derive(Debug)]
struct Section {
	pattern:    String,
	properties: HashMap<String, String>,
}

#[derive(Debug)]
struct ParsedConfig {
	root:     bool,
	sections: Vec<Section>,
}

#[derive(Debug)]
struct ChainEntry {
	dir:    PathBuf,
	parsed: ParsedConfig,
}

/// Resolve `.editorconfig` hints for `file` without defaults. Both fields are
/// `None` when the chain does not pin them.
pub fn get_editor_config_formatting(file: &Path) -> EditorConfigFormatting {
	if file.as_os_str().is_empty() {
		return EditorConfigFormatting::default();
	}
	if file.components().any(|component| component.as_os_str().len() > NAME_MAX_BYTES) {
		return EditorConfigFormatting::default();
	}
	let file_dir = file.parent().unwrap_or_else(|| Path::new("."));
	let chain = collect_chain(file_dir);
	if chain.is_empty() {
		return EditorConfigFormatting::default();
	}
	let mut resolved = Match::default();
	for entry in &chain {
		let relative = relative_path(&entry.dir, file);
		for section in &entry.parsed.sections {
			if !matches_pattern(&section.pattern, &relative) {
				continue;
			}
			if let Some(style) = section.properties.get("indent_style").map(String::as_str) {
				resolved.style = match style {
					"space" => Some(IndentStyle::Space),
					"tab" => Some(IndentStyle::Tab),
					_ => resolved.style,
				};
			}
			if let Some(raw) = section.properties.get("indent_size").map(String::as_str) {
				if raw == "tab" {
					resolved.indent = Some(IndentSize::Tab);
				} else if let Some(width) = parse_positive_integer(raw) {
					resolved.indent = Some(IndentSize::Spaces(width));
				}
			}
			if let Some(width) = section.properties.get("tab_width").and_then(|raw| parse_positive_integer(raw)) {
				resolved.tab_width = Some(width);
			}
		}
	}
	if resolved.style.is_none() && resolved.indent.is_none() && resolved.tab_width.is_none() {
		return EditorConfigFormatting::default();
	}
	let mut result = EditorConfigFormatting::default();
	match resolved.indent {
		Some(IndentSize::Spaces(width)) => result.tab_size = Some(width),
		_ => result.tab_size = resolved.tab_width,
	}
	result.insert_spaces = match resolved.style {
		Some(IndentStyle::Space) => Some(true),
		Some(IndentStyle::Tab) => Some(false),
		None => match resolved.indent {
			Some(IndentSize::Tab) => Some(false),
			Some(IndentSize::Spaces(_)) => Some(true),
			None => None,
		},
	};
	result
}

fn collect_chain(start_dir: &Path) -> Vec<ChainEntry> {
	let mut chain = Vec::new();
	let mut cursor = start_dir.to_path_buf();
	loop {
		let config_path = cursor.join(".editorconfig");
		if let Ok(content) = std::fs::read_to_string(&config_path) {
			let parsed = parse_config(&content);
			let root = parsed.root;
			chain.push(ChainEntry { dir: cursor.clone(), parsed });
			if root {
				break;
			}
		}
		let Some(parent) = cursor.parent() else {
			break;
		};
		if parent == cursor {
			break;
		}
		cursor = parent.to_path_buf();
	}
	chain.reverse();
	chain
}

fn parse_config(content: &str) -> ParsedConfig {
	let mut parsed = ParsedConfig { root: false, sections: Vec::new() };
	let mut current: Option<usize> = None;
	for raw_line in content.lines() {
		let line = raw_line.trim();
		if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
			continue;
		}
		if line.starts_with('[') && line.ends_with(']') && line.len() >= 2 {
			let pattern = line[1..line.len() - 1].trim();
			if pattern.is_empty() {
				current = None;
			} else {
				parsed.sections.push(Section { pattern: pattern.to_string(), properties: HashMap::new() });
				current = Some(parsed.sections.len() - 1);
			}
			continue;
		}
		let Some((key, value)) = line.split_once('=') else {
			continue;
		};
		let key = key.trim().to_ascii_lowercase();
		let value = value.trim().to_ascii_lowercase();
		if key.is_empty() {
			continue;
		}
		if let Some(index) = current {
			parsed.sections[index].properties.insert(key, value);
		} else if key == "root" {
			parsed.root = value == "true";
		}
	}
	parsed
}

fn parse_positive_integer(raw: &str) -> Option<u32> {
	if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
		return None;
	}
	let parsed = raw.parse::<u32>().ok()?;
	if parsed == 0 {
		return None;
	}
	Some(parsed.clamp(MIN_TAB_WIDTH, MAX_TAB_WIDTH))
}

fn relative_path(base: &Path, file: &Path) -> String {
	let Ok(relative) = file.strip_prefix(base) else {
		return ".".to_string();
	};
	let text = relative.to_string_lossy().replace('\\', "/");
	if text.is_empty() { ".".to_string() } else { text }
}

fn matches_pattern(pattern: &str, relative: &str) -> bool {
	let normalized = pattern.trim_start_matches('/').replace('\\', "/");
	if normalized.is_empty() {
		return false;
	}
	// A section without a slash applies at any depth; one with a slash is
	// anchored to the `.editorconfig` directory, matching editorconfig spec.
	let candidates: Vec<String> = if normalized.contains('/') {
		vec![normalized]
	} else {
		vec![normalized.clone(), format!("**/{normalized}")]
	};
	candidates.iter().any(|candidate| {
		CompiledWalkGlob::new([candidate.clone()]).is_ok_and(|glob| glob.is_match(relative))
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_indent_style_and_size_with_nearest_override() {
		let root = std::env::temp_dir().join(format!("pi-editorconfig-{}", std::process::id()));
		let sub = root.join("src");
		std::fs::create_dir_all(&sub).unwrap();
		std::fs::write(root.join(".editorconfig"), "root = true\n[*]\nindent_style = tab\nindent_size = 4\n").unwrap();
		std::fs::write(sub.join(".editorconfig"), "[*.rs]\nindent_style = space\nindent_size = 2\n").unwrap();
		let file = sub.join("main.rs");
		let formatting = get_editor_config_formatting(&file);
		assert_eq!(formatting.tab_size, Some(2));
		assert_eq!(formatting.insert_spaces, Some(true));
		let _ = std::fs::remove_dir_all(&root);
	}

	#[test]
	fn no_matching_section_yields_no_hints() {
		let root = std::env::temp_dir().join(format!("pi-editorconfig-none-{}", std::process::id()));
		std::fs::create_dir_all(&root).unwrap();
		std::fs::write(root.join(".editorconfig"), "root = true\n[*.go]\nindent_size = 8\n").unwrap();
		let formatting = get_editor_config_formatting(&root.join("main.rs"));
		assert_eq!(formatting, EditorConfigFormatting::default());
		let _ = std::fs::remove_dir_all(&root);
	}
}
