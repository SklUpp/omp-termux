//! `omp models`: list and search the file-backed model catalog.
//!
//! Mirrors `packages/coding-agent/src/cli/models-cli.ts` for the catalog-backed
//! paths: positional resolution (`ls`/`list`/`find`, any other first token is a
//! provider/substring filter for `ls`), exact-provider match then substring
//! filtering, the provider-grouped box table and the `--json` document.
//!
//! Model source is [`crate::catalog`] (`models.json`, else `models.yml`
//! providers). `config.yml` plus the `--config` overlays decide availability:
//! only providers with a configured key or explicit `auth: none` (keyless) are
//! listed, and `disabledProviders` hides them, like upstream `getAvailable()`.
//! There is no provider network registry or OAuth store in the headless build:
//! `refresh` and `-e` reject with their prerequisite, and OAuth-only providers
//! stay unlisted instead of silently degrading to a local re-read.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::catalog::{Catalog, ModelInfo};
use crate::config::Config;
use crate::{Error, Result};

/// Upstream `ModelsAction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
	Ls,
	Find,
	Refresh,
}

/// One `omp models` invocation (positional `action`/`pattern` plus flags).
#[derive(Debug, Default)]
pub struct Request {
	/// First positional: `ls`/`list`, `find`, `refresh`, or a provider filter.
	pub action:        Option<String>,
	/// Second positional: search substring or `ls` filter.
	pub pattern:       Option<String>,
	/// `--json`.
	pub json:          bool,
	/// `-e/--extension <path>` (repeatable).
	pub extensions:    Vec<PathBuf>,
	/// `--no-extensions`: restrict the listing to the file-backed catalog.
	pub no_extensions: bool,
	/// `--config <path>` overlays (repeatable); the root `--config` is merged in
	/// by the caller. They drive `disabledProviders` and provider credentials,
	/// exactly like the run path.
	pub config:        Vec<PathBuf>,
}

/// Upstream `resolveModelsArgs`: known keywords map to actions, anything else
/// is a provider/substring filter for the default `ls` view (the second
/// positional is ignored in that case, exactly as upstream).
#[must_use]
pub fn resolve(action: Option<&str>, pattern: Option<&str>) -> (Action, Option<String>) {
	match action {
		Some("ls" | "list") => (Action::Ls, pattern.map(str::to_string)),
		Some("find") => (Action::Find, pattern.map(str::to_string)),
		Some("refresh") => (Action::Refresh, pattern.map(str::to_string)),
		Some(provider) => (Action::Ls, Some(provider.to_string())),
		None => (Action::Ls, None),
	}
}

/// Run `omp models`, writing the listing to `out`.
pub fn run(request: &Request, cwd: &Path, out: &mut impl Write) -> Result<()> {
	let (action, pattern) = resolve(request.action.as_deref(), request.pattern.as_deref());
	if action == Action::Find && pattern.as_deref().map_or(true, |needle| needle.trim().is_empty()) {
		return Err(Error::new(
			"`omp models find` requires a search substring, e.g. `omp models find minimax`",
		));
	}
	if !request.extensions.is_empty() {
		let paths = request
			.extensions
			.iter()
			.map(|path| path.display().to_string())
			.collect::<Vec<_>>()
			.join(", ");
		return Err(Error::new(format!(
			"`omp models -e` needs the TypeScript extension runtime, which the headless build does not ship: {paths}"
		)));
	}
	if action == Action::Refresh {
		return Err(Error::new(
			"`omp models refresh` needs a live provider catalog fetch (network + provider auth), which the headless build does not implement; run `omp models` to list the local catalog",
		));
	}
	// `--no-extensions` asks for the file-backed catalog only: no ambient
	// discovery and no settings `extensions:` (upstream `disableExtensionDiscovery`).
	// Those are the only catalogs this build can produce, so the request is
	// satisfied by construction — the flag is consumed here instead of being
	// dropped as an unknown field. Upstream explicit `-e` paths survive the flag;
	// here they still fail above rather than being silently downgraded by it.
	if request.no_extensions {
		debug_assert!(
			request.extensions.is_empty(),
			"explicit extension paths must fail before --no-extensions is honored"
		);
	}
	// Settings drive availability, so they load even without `--config`:
	// `disabledProviders` and provider credentials never get validate-and-dropped.
	let config = Config::load_run(&request.config, Some(cwd))?;
	let (catalog, warning) = Catalog::load_listing(cwd)?;
	render(
		&catalog,
		&config,
		cwd,
		warning.as_deref(),
		action,
		pattern.as_deref(),
		request.json,
		out,
	)
}

/// One catalog entry prepared for listing (upstream `Model` subset).
struct Row<'a> {
	provider: &'a str,
	id:       &'a str,
	info:     &'a ModelInfo,
}

fn rows<'a>(catalog: &'a Catalog, available: &BTreeSet<&str>) -> Vec<Row<'a>> {
	let mut rows: Vec<Row<'a>> = catalog
		.providers
		.iter()
		.filter(|(provider, _)| available.contains(provider.as_str()))
		.flat_map(|(_, models)| {
			models
				.iter()
				.map(|(id, info)| Row { provider: info.provider.as_str(), id: id.as_str(), info })
		})
		.collect();
	rows.sort_by(|left, right| {
		left.provider.cmp(right.provider).then_with(|| left.id.cmp(right.id))
	});
	rows
}

/// Upstream `getAvailable()`: a provider is listed when settings do not disable
/// it and it is either keyless (`auth: none`) or carries a resolvable API key
/// from `config.yml` / `models.yml`. The headless build has no provider env-var
/// table and no OAuth store, so an unconfigured cloud provider is not claimed
/// as available (and OAuth-only providers stay hidden with the rest).
fn available_providers<'a>(
	catalog: &'a Catalog,
	config: &Config,
	cwd: &Path,
) -> BTreeSet<&'a str> {
	let disabled = config.disabled_providers(cwd);
	catalog
		.providers
		.keys()
		.filter(|provider| {
			!disabled.iter().any(|id| id == *provider)
				&& connection_is_usable(catalog, config, provider.as_str())
		})
		.map(String::as_str)
		.collect()
}

fn connection_is_usable(catalog: &Catalog, config: &Config, provider: &str) -> bool {
	[config.providers.get(provider), catalog.connections.get(provider)]
		.into_iter()
		.flatten()
		.any(|connection| {
			connection.auth.as_deref() == Some("none")
				|| connection.resolve_api_key().is_some_and(|key| !key.is_empty())
		})
}

/// Upstream filtering: exact provider match wins for `ls`; otherwise match the
/// needle against id, provider, `provider/id`, and name (case-insensitive).
fn filter<'rows, 'catalog>(
	rows: &'rows [Row<'catalog>],
	action: Action,
	pattern: Option<&str>,
) -> Vec<&'rows Row<'catalog>> {
	let Some(needle) = pattern.map(str::to_lowercase).filter(|needle| !needle.is_empty()) else {
		return rows.iter().collect();
	};
	if action != Action::Find {
		let exact: Vec<&'rows Row<'catalog>> =
			rows.iter().filter(|row| row.provider.to_lowercase() == needle).collect();
		if !exact.is_empty() {
			return exact;
		}
	}
	rows
		.iter()
		.filter(|row| {
			row.id.to_lowercase().contains(&needle)
				|| row.provider.to_lowercase().contains(&needle)
				|| format!("{}/{}", row.provider, row.id).to_lowercase().contains(&needle)
				|| row.info.name.as_deref().unwrap_or(row.id).to_lowercase().contains(&needle)
		})
		.collect()
}

#[allow(clippy::too_many_arguments, reason = "one internal render seam keeps the listing contract explicit")]
fn render(
	catalog: &Catalog,
	config: &Config,
	cwd: &Path,
	warning: Option<&str>,
	action: Action,
	pattern: Option<&str>,
	json_output: bool,
	out: &mut impl Write,
) -> Result<()> {
	let available = available_providers(catalog, config, cwd);
	let rows = rows(catalog, &available);
	let filtered = filter(&rows, action, pattern);

	if json_output {
		if let Some(warning) = warning {
			// JSON stays parseable on stdout; the catalog warning goes to stderr
			// like upstream `models-cli` does.
			eprintln!("Warning: models.yml validation failed — custom providers disabled");
			for line in warning.lines() {
				eprintln!("  {line}");
			}
		}
		let models: Vec<Value> = filtered.iter().map(|row| json_model(row)).collect();
		writeln!(out, "{}", serde_json::to_string(&json!({ "models": models }))?)?;
		return Ok(());
	}
	if let Some(warning) = warning {
		writeln!(out, "Warning: models.yml validation failed — custom providers disabled")?;
		for line in warning.lines() {
			writeln!(out, "  {line}")?;
		}
		writeln!(out)?;
	}
	if rows.is_empty() {
		writeln!(out, "No models available. Set API keys in environment variables.")?;
		return Ok(());
	}
	if filtered.is_empty() {
		writeln!(out, "No models matching \"{}\"", pattern.unwrap_or_default())?;
		return Ok(());
	}

	// One section per provider: heading plus a box table of that provider's models.
	let mut first = true;
	let mut index = 0;
	while index < filtered.len() {
		let provider = filtered[index].provider;
		let mut end = index + 1;
		while end < filtered.len() && filtered[end].provider == provider {
			end += 1;
		}
		let group = &filtered[index..end];
		if !first {
			writeln!(out)?;
		}
		first = false;
		// Upstream bolds the heading and dims the count; both are no-ops off a TTY.
		writeln!(out, "{provider} ({})", group.len())?;
		let table_rows: Vec<Vec<String>> = group
			.iter()
			.map(|row| {
				vec![
					row.id.to_string(),
					format_limit(row.info.context_window.map(|window| window as u64)),
					format_limit(row.info.max_tokens),
					thinking_cell(row.info),
					if input_includes_image(row.info) {
						"yes".to_string()
					} else {
						"no".to_string()
					},
				]
			})
			.collect();
		for line in box_table(
			&[
				("model", false),
				("context", true),
				("max-out", true),
				("thinking", false),
				("images", false),
			],
			&table_rows,
		) {
			writeln!(out, "{line}")?;
		}
		index = end;
	}
	Ok(())
}

fn json_model(row: &Row<'_>) -> Value {
	let cost = row.info.cost.clone().unwrap_or_else(|| {
		json!({ "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 })
	});
	json!({
		"provider": row.provider,
		"id": row.id,
		"selector": format!("{}/{}", row.provider, row.id),
		"name": row.info.name.as_deref().unwrap_or(row.id),
		"contextWindow": row.info.context_window,
		"maxTokens": row.info.max_tokens,
		"reasoning": row.info.reasoning,
		"thinking": row.info.efforts.clone(),
		"input": row.info.input.clone().unwrap_or_else(|| vec!["text".to_string()]),
		"cost": cost,
	})
}

fn thinking_cell(info: &ModelInfo) -> String {
	if let Some(efforts) = &info.efforts {
		return efforts.join(",");
	}
	if info.reasoning { "yes".to_string() } else { "-".to_string() }
}

fn input_includes_image(info: &ModelInfo) -> bool {
	info.input
		.as_ref()
		.is_some_and(|input| input.iter().any(|modality| modality == "image"))
}

fn format_limit(limit: Option<u64>) -> String {
	limit.map_or_else(|| "-".to_string(), format_number)
}

/// Upstream `formatNumber` from `@oh-my-pi/pi-utils`: compact K/M/B, one
/// decimal below 10 units of each scale.
fn format_number(value: u64) -> String {
	if value < 1_000 {
		return value.to_string();
	}
	let scaled = |divisor: f64| value as f64 / divisor;
	if value < 10_000 {
		return format!("{}K", trim1(scaled(1_000.0)));
	}
	if value < 1_000_000 {
		return format!("{}K", (value as f64 / 1_000.0).round() as u64);
	}
	if value < 10_000_000 {
		return format!("{}M", trim1(scaled(1_000_000.0)));
	}
	if value < 1_000_000_000 {
		return format!("{}M", (value as f64 / 1_000_000.0).round() as u64);
	}
	if value < 10_000_000_000 {
		return format!("{}B", trim1(scaled(1_000_000_000.0)));
	}
	format!("{}B", (value as f64 / 1_000_000_000.0).round() as u64)
}

/// `toFixed(1)` with a trailing `.0` dropped.
fn trim1(value: f64) -> String {
	let text = format!("{value:.1}");
	text.strip_suffix(".0").map_or_else(|| text.clone(), str::to_string)
}

/// Box-drawing table; columns carry `(header, right_align)`.
fn box_table(columns: &[(&str, bool)], rows: &[Vec<String>]) -> Vec<String> {
	let widths: Vec<usize> = columns
		.iter()
		.enumerate()
		.map(|(index, (header, _))| {
			rows.iter()
				.map(|row| display_width(&row[index]))
				.chain(std::iter::once(display_width(header)))
				.max()
				.unwrap_or(0)
		})
		.collect();
	let segments: Vec<String> = widths.iter().map(|width| "─".repeat(width + 2)).collect();
	let mut lines = vec![format!("┌{}┐", segments.join("┬"))];
	let headers: Vec<String> = columns.iter().map(|(header, _)| (*header).to_string()).collect();
	lines.push(render_row(&headers, &widths, columns));
	lines.push(format!("├{}┤", segments.join("┼")));
	for row in rows {
		lines.push(render_row(row, &widths, columns));
	}
	lines.push(format!("└{}┘", segments.join("┴")));
	lines
}

fn render_row(cells: &[String], widths: &[usize], columns: &[(&str, bool)]) -> String {
	let padded: Vec<String> = cells
		.iter()
		.enumerate()
		.map(|(index, cell)| {
			let right = columns.get(index).is_some_and(|(_, right)| *right);
			pad_cell(cell, widths[index], right)
		})
		.collect();
	format!("│ {} │", padded.join(" │ "))
}

fn pad_cell(text: &str, width: usize, right: bool) -> String {
	let padding = width.saturating_sub(display_width(text));
	if padding == 0 {
		return text.to_string();
	}
	let fill = " ".repeat(padding);
	if right { format!("{fill}{text}") } else { format!("{text}{fill}") }
}

/// ANSI-free catalog cells; counted per char.
/// ponytail: CJK/emoji double-width ids misalign the box; add `unicode-width`
/// if catalog ids ever need it.
fn display_width(text: &str) -> usize {
	text.chars().count()
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeMap;
	use std::fs;

	use super::*;
	use crate::config::{DisabledProviderEntry, ProviderConfig};

	fn model(
		provider: &str,
		name: Option<&str>,
		context_window: Option<usize>,
		max_tokens: Option<u64>,
		reasoning: bool,
		efforts: Option<&[&str]>,
		input: Option<&[&str]>,
		cost: Option<Value>,
	) -> ModelInfo {
		ModelInfo {
			provider: provider.to_string(),
			context_window,
			input: input.map(|items| items.iter().map(|item| (*item).to_string()).collect()),
			name: name.map(str::to_string),
			max_tokens,
			reasoning,
			efforts: efforts.map(|items| items.iter().map(|item| (*item).to_string()).collect()),
			cost,
			supports_usage_in_streaming: None,
		}
	}

	fn catalog() -> Catalog {
		let mut providers: BTreeMap<String, BTreeMap<String, ModelInfo>> = BTreeMap::new();
		providers.insert(
			"openai".to_string(),
			BTreeMap::from([(
				"gpt-5".to_string(),
				model(
					"openai",
					Some("GPT-5"),
					Some(1_048_576),
					Some(393_216),
					true,
					Some(&["low", "high"]),
					Some(&["text", "image"]),
					Some(json!({ "input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0 })),
				),
			)]),
		);
		providers.insert(
			"local".to_string(),
			BTreeMap::from([(
				"qwen".to_string(),
				model("local", None, Some(32_768), None, false, None, Some(&["text"]), None),
			)]),
		);
		Catalog {
			providers,
			connections: BTreeMap::new(),
			source: PathBuf::from("test-models.json"),
		}
	}

	fn config() -> Config {
		let mut config = Config::default();
		for provider in ["openai", "local"] {
			config.providers.insert(
				provider.to_string(),
				ProviderConfig { api_key: Some("sk-test".to_string()), ..ProviderConfig::default() },
			);
		}
		config
	}

	fn render_text(action: Action, pattern: Option<&str>) -> String {
		render_text_with(&catalog(), &config(), action, pattern)
	}

	fn render_text_with(
		catalog: &Catalog,
		config: &Config,
		action: Action,
		pattern: Option<&str>,
	) -> String {
		let mut out = Vec::new();
		render(catalog, config, Path::new("."), None, action, pattern, false, &mut out)
			.expect("render");
		String::from_utf8(out).expect("utf8")
	}

	fn write(path: &Path, text: &str) {
		fs::write(path, text).expect("write");
	}

	#[test]
	fn resolve_matches_upstream_keywords_and_provider_shortcut() {
		assert_eq!(resolve(None, None), (Action::Ls, None));
		assert_eq!(resolve(Some("list"), None), (Action::Ls, None));
		assert_eq!(
			resolve(Some("ls"), Some("openai")),
			(Action::Ls, Some("openai".to_string()))
		);
		assert_eq!(
			resolve(Some("find"), Some("mini")),
			(Action::Find, Some("mini".to_string()))
		);
		assert_eq!(resolve(Some("refresh"), None), (Action::Refresh, None));
		// Provider shortcut: second positional is ignored, like upstream.
		assert_eq!(
			resolve(Some("openai-codex"), Some("ignored")),
			(Action::Ls, Some("openai-codex".to_string()))
		);
	}

	#[test]
	fn lists_provider_tables_with_upstream_columns() {
		let text = render_text(Action::Ls, None);
		assert!(text.contains("openai (1)"), "{text}");
		assert!(text.contains("local (1)"), "{text}");
		assert!(text.contains("gpt-5"), "{text}");
		assert!(text.contains("1M"), "{text}");
		assert!(text.contains("393K"), "{text}");
		assert!(text.contains("low,high"), "{text}");
		assert!(text.contains("│ model │ context │ max-out │ thinking │ images │"), "{text}");
		assert!(text.contains("│ qwen "), "{text}");
	}

	#[test]
	fn filters_exact_provider_then_substring_and_reports_no_match() {
		let exact = render_text(Action::Ls, Some("local"));
		assert!(exact.contains("local (1)") && !exact.contains("openai ("));
		// Not a provider name: substring over id/name/selector.
		let by_name = render_text(Action::Ls, Some("gpt"));
		assert!(by_name.contains("openai (1)"));
		let find_case = render_text(Action::Find, Some("GPT"));
		assert!(find_case.contains("openai (1)"));
		assert_eq!(render_text(Action::Find, Some("missing")), "No models matching \"missing\"\n");
	}

	#[test]
	fn json_document_matches_upstream_model_shape() {
		let mut out = Vec::new();
		render(
			&catalog(),
			&config(),
			Path::new("."),
			None,
			Action::Ls,
			Some("openai"),
			true,
			&mut out,
		)
		.expect("render");
		let value: Value = serde_json::from_slice(&out).expect("json");
		let models = value["models"].as_array().expect("models");
		assert_eq!(models.len(), 1);
		let model = &models[0];
		assert_eq!(model["provider"], "openai");
		assert_eq!(model["id"], "gpt-5");
		assert_eq!(model["selector"], "openai/gpt-5");
		assert_eq!(model["name"], "GPT-5");
		assert_eq!(model["contextWindow"], 1_048_576);
		assert_eq!(model["maxTokens"], 393_216);
		assert_eq!(model["reasoning"], true);
		assert_eq!(model["thinking"], json!(["low", "high"]));
		assert_eq!(model["input"], json!(["text", "image"]));
		assert_eq!(model["cost"], json!({ "input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0 }));
	}

	#[test]
	fn find_without_pattern_and_refresh_are_errors() {
		let mut out = Vec::new();
		let error = run(
			&Request { action: Some("find".to_string()), ..Request::default() },
			Path::new("."),
			&mut out,
		)
		.expect_err("find needs a pattern");
		assert!(error.to_string().contains("`omp models find` requires a search substring"));

		let error = run(
			&Request { action: Some("refresh".to_string()), ..Request::default() },
			Path::new("."),
			&mut out,
		)
		.expect_err("refresh needs a network registry");
		assert!(error.to_string().contains("live provider catalog fetch"));
	}

	#[test]
	fn explicit_extension_paths_are_rejected() {
		let mut out = Vec::new();
		let error = run(
			&Request { extensions: vec![PathBuf::from("ext.ts")], ..Request::default() },
			Path::new("."),
			&mut out,
		)
		.expect_err("no extension runtime");
		assert!(error.to_string().contains("extension runtime"));
	}

	#[test]
	fn lists_only_authenticated_keyless_and_enabled_providers() {
		let mut catalog = catalog();
		catalog.providers.insert(
			"acme".to_string(),
			BTreeMap::from([(
				"m1".to_string(),
				model("acme", None, None, None, false, None, None, None),
			)]),
		);
		catalog.providers.insert(
			"oauthcorp".to_string(),
			BTreeMap::from([(
				"o1".to_string(),
				model("oauthcorp", None, None, None, false, None, None, None),
			)]),
		);
		// openai carries a configured key; local is keyless via `auth: none`.
		// acme has no connection and oauthcorp only an OAuth declaration, which
		// the headless build cannot satisfy.
		let mut config = Config::default();
		config.providers.insert(
			"openai".to_string(),
			ProviderConfig { api_key: Some("sk-openai".to_string()), ..ProviderConfig::default() },
		);
		config.providers.insert(
			"oauthcorp".to_string(),
			ProviderConfig { auth: Some("oauth".to_string()), ..ProviderConfig::default() },
		);
		catalog.connections.insert(
			"local".to_string(),
			ProviderConfig { auth: Some("none".to_string()), ..ProviderConfig::default() },
		);
		let text = render_text_with(&catalog, &config, Action::Ls, None);
		assert!(text.contains("openai (1)"), "{text}");
		assert!(text.contains("local (1)"), "{text}");
		assert!(!text.contains("acme"), "{text}");
		assert!(!text.contains("oauthcorp"), "{text}");

		// `disabledProviders` hides a provider even when it carries a key.
		config.disabled_providers = Some(vec![DisabledProviderEntry::All("openai".to_string())]);
		let text = render_text_with(&catalog, &config, Action::Ls, None);
		assert!(!text.contains("openai (1)"), "{text}");
		assert!(text.contains("local (1)"), "{text}");

		// Path-scoped entries only apply inside their prefix.
		config.disabled_providers = Some(vec![DisabledProviderEntry::Scoped {
			prefixes:  vec!["/elsewhere".to_string()],
			providers: vec!["local".to_string()],
		}]);
		assert!(render_text_with(&catalog, &config, Action::Ls, None).contains("local (1)"));
	}

	#[test]
	fn unconfigured_catalog_reports_no_models_available() {
		let text = render_text_with(&catalog(), &Config::default(), Action::Ls, None);
		assert_eq!(text, "No models available. Set API keys in environment variables.\n");

		let mut out = Vec::new();
		render(
			&catalog(),
			&Config::default(),
			Path::new("."),
			None,
			Action::Ls,
			None,
			true,
			&mut out,
		)
		.expect("render");
		let value: Value = serde_json::from_slice(&out).expect("json");
		assert_eq!(value, json!({ "models": [] }));
	}

	#[test]
	fn malformed_yml_warning_precedes_the_valid_catalog() {
		let mut out = Vec::new();
		render(
			&catalog(),
			&config(),
			Path::new("."),
			Some("models.yml: `providers` must be a mapping"),
			Action::Ls,
			None,
			false,
			&mut out,
		)
		.expect("render");
		let text = String::from_utf8(out).expect("utf8");
		assert!(
			text.starts_with(
				"Warning: models.yml validation failed — custom providers disabled\n  models.yml: \
				 `providers` must be a mapping\n\n"
			),
			"{text}"
		);
		assert!(text.contains("openai (1)"), "{text}");

		// JSON keeps stdout parseable; the warning is stderr-only there.
		let mut out = Vec::new();
		render(
			&catalog(),
			&config(),
			Path::new("."),
			Some("models.yml: `providers` must be a mapping"),
			Action::Ls,
			None,
			true,
			&mut out,
		)
		.expect("render");
		let value: Value = serde_json::from_slice(&out).expect("json");
		assert_eq!(value["models"].as_array().map(Vec::len), Some(2));
	}

	#[test]
	fn config_overlays_apply_credentials_and_disabled_providers() {
		let _env = crate::TEST_ENV_LOCK.lock().expect("env lock");
		let root = std::env::temp_dir()
			.join(format!("omp-cli-models-overlay-{}", std::process::id()));
		let cwd = root.join("work");
		fs::create_dir_all(&cwd).expect("cwd");
		write(&cwd.join("models.json"), r#"{"acme":{"m1":{"id":"m1","provider":"acme"}}}"#);

		let base = root.join("base");
		let previous_config = std::env::var_os("PI_CONFIG_DIR");
		let previous_agent = std::env::var_os("PI_CODING_AGENT_DIR");
		// SAFETY: guarded by TEST_ENV_LOCK, like the other env-mutating tests.
		unsafe {
			std::env::set_var("PI_CONFIG_DIR", &base);
			std::env::set_var("PI_CODING_AGENT_DIR", base.join("agent"));
		}
		let overlay = root.join("overlay.yml");
		write(
			&overlay,
			"providers:\n  acme:\n    baseUrl: http://127.0.0.1:1/v1\n    apiKey: sk-overlay\n",
		);
		let config =
			Config::load_with_cwd(&[overlay.clone()], Some(cwd.as_path()), &[]).expect("config");
		let catalog = Catalog::load(&cwd).expect("catalog");
		let listed = render_text_with(&catalog, &config, Action::Ls, None);

		write(
			&overlay,
			"disabledProviders: [acme]\nproviders:\n  acme:\n    baseUrl: http://127.0.0.1:1/v1\n    apiKey: sk-overlay\n",
		);
		let config = Config::load_with_cwd(&[overlay], Some(cwd.as_path()), &[]).expect("config");
		let hidden = render_text_with(&catalog, &config, Action::Ls, None);

		// SAFETY: same lock.
		unsafe {
			match previous_config {
				Some(value) => std::env::set_var("PI_CONFIG_DIR", value),
				None => std::env::remove_var("PI_CONFIG_DIR"),
			}
			match previous_agent {
				Some(value) => std::env::set_var("PI_CODING_AGENT_DIR", value),
				None => std::env::remove_var("PI_CODING_AGENT_DIR"),
			}
		}
		drop(_env);
		fs::remove_dir_all(root).ok();

		assert!(listed.contains("acme (1)"), "{listed}");
		assert!(hidden.contains("No models available"), "{hidden}");
		assert!(!hidden.contains("acme (1)"), "{hidden}");
	}
}
