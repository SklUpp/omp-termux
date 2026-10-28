//! Task-agent catalog and delegation policy.
//!
//! The CLI owns filesystem discovery and frontmatter parsing; this module owns
//! the shared definition shape plus everything the `task` tool must enforce at
//! spawn time: spawn allowlists, disabled agents, model-role selection, effort
//! ceilings, and structured-output validation.
//!
//! No tool prompt can widen a permission: model selection goes through an
//! injected [`ChildModelFactory`] (the CLI's provider/config resolver), and tool
//! allowlists are intersected with the parent agent's effective allowlist by the
//! task tool.

use std::{
	collections::{BTreeMap, BTreeSet},
	sync::{Arc, RwLock},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::provider::ModelProvider;

/// Where an agent definition came from. Precedence is resolved by the CLI
/// before definitions reach this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentSource {
	Bundled,
	User,
	Project,
	Extension,
	Plugin,
}

impl AgentSource {
	#[must_use]
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Bundled => "bundled",
			Self::User => "user",
			Self::Project => "project",
			Self::Extension => "extension",
			Self::Plugin => "plugin",
		}
	}
}

/// Agent spawn allowlist from `spawns` frontmatter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spawns {
	/// `spawns: "*"` (also the implicit session default).
	All,
	/// `spawns: scout,reviewer`.
	Only(Vec<String>),
}

impl Spawns {
	#[must_use]
	pub fn default_agent(&self) -> &str {
		match self {
			Self::All => "task",
			Self::Only(names) => names.first().map_or("task", String::as_str),
		}
	}

	#[must_use]
	pub fn allows(&self, name: &str) -> bool {
		match self {
			Self::All => true,
			Self::Only(names) => names.iter().any(|allowed| allowed == name),
		}
	}

	#[must_use]
	pub fn error_text(&self) -> String {
		match self {
			Self::All => "*".to_string(),
			Self::Only(names) if names.is_empty() => {
				"none (spawns disabled for this agent)".to_string()
			},
			Self::Only(names) => names.join(","),
		}
	}
}

/// `prewalk` / `advisor` frontmatter value. Both describe hand-off/advice
/// behaviour this runtime does not implement, so the task tool rejects an agent
/// that requests one instead of silently ignoring it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffTarget {
	Default,
	Pattern(String),
}

/// One discovered agent definition.
#[derive(Debug, Clone)]
pub struct AgentDefinition {
	pub name: String,
	pub description: String,
	/// `None` means "inherit the session tool set"; `Some` is an explicit
	/// allowlist and always carries `yield`.
	pub tools: Option<Vec<String>>,
	pub spawns: Option<Spawns>,
	pub model: Vec<String>,
	pub output: Option<Value>,
	pub thinking_level: Option<String>,
	pub blocking: bool,
	pub read_summarize: Option<bool>,
	pub prewalk: Option<HandoffTarget>,
	pub advisor: Option<HandoffTarget>,
	pub autoload_skills: Vec<String>,
	pub system_prompt: String,
	pub source: AgentSource,
	pub file_path: String,
}

/// Effective output-schema enforcement policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SchemaMode {
	#[default]
	Permissive,
	Strict,
}

impl SchemaMode {
	#[must_use]
	pub fn parse(value: &str) -> Option<Self> {
		match value.trim() {
			"permissive" => Some(Self::Permissive),
			"strict" => Some(Self::Strict),
			_ => None,
		}
	}

	#[must_use]
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Permissive => "permissive",
			Self::Strict => "strict",
		}
	}
}

/// A provider resolved for one child agent.
pub struct ChildModel {
	pub provider: Arc<dyn ModelProvider>,
	/// `provider/model`, for result metadata only.
	pub model: String,
	/// Pre-expansion role alias (`smol` for `@smol`), when the pattern named one.
	pub role: Option<String>,
}

/// Model/provider resolver injected by the CLI. The catalog never constructs
/// providers itself, so `pi-agent` keeps no config or connection knowledge.
pub trait ChildModelFactory: Send + Sync {
	/// Whether `model` (a concrete pattern or role alias, without an effort
	/// suffix) can be turned into a provider.
	fn can_resolve(&self, model: &str) -> bool;

	/// Build the provider for `model`. `effort` is the caller override and wins;
	/// `fallback_effort` covers the agent pattern suffix / `thinkingLevel` and
	/// only applies when the caller left effort unset.
	fn resolve(
		&self,
		model: &str,
		effort: Option<&str>,
		fallback_effort: Option<&str>,
	) -> Result<ChildModel, String>;
}

/// Discovered agents plus the delegation policy resolved from settings.
pub struct AgentCatalog {
	agents: Vec<AgentDefinition>,
	pub project_agents_dir: Option<String>,
	/// Replaced at the start of every root turn so role/default model
	/// resolution follows `/model`; definitions and policy stay stable.
	model_factory: RwLock<Option<Arc<dyn ChildModelFactory>>>,
	disabled: BTreeSet<String>,
	model_overrides: BTreeMap<String, Vec<String>>,
	pub max_recursion_depth: Option<i32>,
	pub max_effort: Option<String>,
}

impl AgentCatalog {
	#[must_use]
	pub fn new(agents: Vec<AgentDefinition>, project_agents_dir: Option<String>) -> Self {
		Self {
			agents,
			project_agents_dir,
			model_factory: RwLock::new(None),
			disabled: BTreeSet::new(),
			model_overrides: BTreeMap::new(),
			max_recursion_depth: None,
			max_effort: None,
		}
	}

	#[must_use]
	pub fn with_model_factory(self, factory: Arc<dyn ChildModelFactory>) -> Self {
		self.set_model_factory(factory);
		self
	}

	/// Swap the per-turn resolver snapshot without rediscovering agents.
	pub fn set_model_factory(&self, factory: Arc<dyn ChildModelFactory>) {
		*self
			.model_factory
			.write()
			.unwrap_or_else(|poison| poison.into_inner()) = Some(factory);
	}

	#[must_use]
	pub fn model_factory(&self) -> Option<Arc<dyn ChildModelFactory>> {
		self
			.model_factory
			.read()
			.unwrap_or_else(|poison| poison.into_inner())
			.clone()
	}

	#[must_use]
	pub fn with_policy(
		mut self,
		disabled: impl IntoIterator<Item = String>,
		model_overrides: BTreeMap<String, Vec<String>>,
		max_recursion_depth: Option<i32>,
		max_effort: Option<String>,
	) -> Self {
		self.disabled = disabled.into_iter().collect();
		self.model_overrides = model_overrides;
		self.max_recursion_depth = max_recursion_depth;
		self.max_effort = max_effort;
		self
	}

	#[must_use]
	pub fn agents(&self) -> &[AgentDefinition] {
		&self.agents
	}

	#[must_use]
	pub fn get(&self, name: &str) -> Option<&AgentDefinition> {
		self.agents.iter().find(|agent| agent.name == name)
	}

	#[must_use]
	pub fn names(&self) -> Vec<&str> {
		self.agents.iter().map(|agent| agent.name.as_str()).collect()
	}

	#[must_use]
	pub fn is_disabled(&self, name: &str) -> bool {
		self.disabled.contains(name)
	}

	/// Resolve and validate one spawn request against spawn policy, recursion
	/// prevention, and the disabled list.
	///
	/// Order mirrors the TypeScript preflight: self-spawn, parent `spawns`,
	/// disabled list, then definition lookup (so an unknown agent reports the
	/// available names).
	pub fn validate_selection<'a>(
		&'a self,
		name: &str,
		parent_agent: Option<&str>,
		parent_spawns: Option<&Spawns>,
	) -> Result<&'a AgentDefinition, String> {
		if parent_agent == Some(name) {
			return Err(format!(
				"Cannot spawn {name} agent from within itself (recursion prevention). Use a different agent type."
			));
		}
		if let Some(spawns) = parent_spawns
			&& !spawns.allows(name)
		{
			return Err(format!(
				"Cannot spawn '{name}'. Allowed: {}",
				spawns.error_text()
			));
		}
		if self.is_disabled(name) {
			let enabled = self
				.agents
				.iter()
				.filter(|agent| !self.disabled.contains(&agent.name))
				.map(|agent| agent.name.as_str())
				.collect::<Vec<_>>();
			let suffix = if enabled.is_empty() {
				String::new()
			} else {
				format!(" Available: {}", enabled.join(", "))
			};
			return Err(format!(
				"Agent \"{name}\" is disabled in settings. Enable it via /agents, or use a different agent type.{suffix}"
			));
		}
		self.get(name).ok_or_else(|| {
			let available = self
				.names()
				.into_iter()
				.filter(|candidate| !self.disabled.contains(*candidate))
				.collect::<Vec<_>>();
			format!(
				"Unknown agent \"{name}\". Available: {}",
				if available.is_empty() { "none".to_string() } else { available.join(", ") }
			)
		})
	}

	/// Resolve the child provider for an agent, honoring per-agent settings
	/// overrides and the per-spawn effort hint.
	///
	/// `None` means "inherit the parent provider" (agent declares no model).
	pub fn resolve_provider(
		&self,
		agent: &AgentDefinition,
		effort_hint: Option<&str>,
		factory: Option<&dyn ChildModelFactory>,
	) -> Result<Option<ChildModel>, String> {
		let patterns = self
			.model_overrides
			.get(&agent.name)
			.cloned()
			.unwrap_or_else(|| agent.model.clone());
		if patterns.is_empty() {
			return Ok(None);
		}
		let Some(factory) = factory else {
			return Err(format!(
				"Agent '{}' selects model {:?} but no model resolver is installed",
				agent.name, patterns
			));
		};
		let agent_effort = match &agent.thinking_level {
			Some(level) => thinking_to_effort(level)?,
			None => None,
		};
		// `auto` means inherit: it is a valid caller hint, normalized to no
		// override, never rejected.
		let requested_effort = effort_hint
			.filter(|effort| !effort.eq_ignore_ascii_case("auto"))
			.map(thinking_to_effort)
			.transpose()?
			.flatten();
		let mut last_error = None;
		for pattern in &patterns {
			let (base, suffix) = split_effort_selector(pattern);
			// Precedence matches the TS executor: caller `effort` wins over the
			// agent pattern's own `:level`, which wins over `thinkingLevel`.
			let caller_effort = cap_effort(requested_effort, self.max_effort.as_deref())?;
			let fallback_effort =
				cap_effort(suffix.or(agent_effort), self.max_effort.as_deref())?;
			if !factory.can_resolve(base) {
				last_error = Some(format!("model pattern '{pattern}' is unavailable"));
				continue;
			}
			match factory.resolve(base, caller_effort, fallback_effort) {
				Ok(mut child) => {
					child.role = role_of(pattern);
					return Ok(Some(child));
				},
				Err(error) => last_error = Some(error),
			}
		}
		Err(format!(
			"Agent '{}' has no available model among {:?}{}",
			agent.name,
			patterns,
			last_error.map(|error| format!(": {error}")).unwrap_or_default()
		))
	}
}

/// Effort selectors accepted after `:`, longest-first disambiguation by unique
/// prefix (matching the TypeScript `:hi`, `:med` abbreviations).
pub const EFFORT_SELECTORS: [&str; 7] =
	["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Split a trailing `:effort` selector from a model pattern.
#[must_use]
pub fn split_effort_selector(model: &str) -> (&str, Option<&'static str>) {
	let Some((base, suffix)) = model.rsplit_once(':') else {
		return (model, None);
	};
	if base.is_empty() || suffix.len() < 2 {
		return (model, None);
	}
	parse_effort_prefix(suffix).map_or((model, None), |effort| (base, Some(effort)))
}

/// Unique-prefix effort parser (minimum 2 characters, like TypeScript).
#[must_use]
pub fn parse_effort_prefix(value: &str) -> Option<&'static str> {
	let needle = value.trim().to_ascii_lowercase();
	if needle.len() < 2 {
		return None;
	}
	let mut matches = EFFORT_SELECTORS
		.iter()
		.filter(|selector| selector.starts_with(&needle))
		.copied();
	let first = matches.next()?;
	if matches.next().is_some() {
		return None;
	}
	Some(first)
}

/// Map `thinkingLevel` to a provider effort. `auto` means "model default".
pub fn thinking_to_effort(level: &str) -> Result<Option<&'static str>, String> {
	let normalized = level.trim().to_ascii_lowercase();
	if normalized == "auto" {
		return Ok(None);
	}
	parse_effort_prefix(&normalized)
		.map(Some)
		.ok_or_else(|| format!("invalid thinking level '{level}'"))
}

fn effort_rank(effort: &str) -> Option<usize> {
	EFFORT_SELECTORS.iter().position(|candidate| *candidate == effort)
}

fn cap_effort(
	effort: Option<&'static str>,
	cap: Option<&str>,
) -> Result<Option<&'static str>, String> {
	let Some(cap) = cap else {
		return Ok(effort);
	};
	let cap = parse_effort_prefix(cap)
		.ok_or_else(|| format!("invalid task.maxEffort '{cap}'"))?;
	match effort {
		None => Ok(None),
		Some(effort) => match (effort_rank(effort), effort_rank(cap)) {
			(Some(requested), Some(ceiling)) if requested > ceiling => Ok(Some(cap)),
			_ => Ok(Some(effort)),
		},
	}
}

fn role_of(pattern: &str) -> Option<String> {
	let trimmed = pattern.trim();
	let role = if let Some(role) = trimmed.strip_prefix('@') {
		role
	} else if let Some(role) = trimmed.strip_prefix("pi/") {
		role
	} else if trimmed == "*" || trimmed.starts_with("*:") {
		return Some("default".to_string());
	} else {
		return None;
	};
	Some(
		role.split(':')
			.next()
			.unwrap_or(role)
			.to_ascii_lowercase(),
	)
}

/// One structured-output validation failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaFailure {
	pub message: String,
	pub missing_required: Vec<String>,
}

/// Normalize an output-schema declaration: JSON strings are parsed, `null`
/// means "no schema", and boolean `false` is rejected (it rejects every value).
pub fn normalize_schema(schema: &Value) -> Result<Value, String> {
	let schema = match schema {
		Value::String(text) => serde_json::from_str::<Value>(text)
			.map_err(|error| format!("invalid JSON output schema: {error}"))?,
		Value::Null => return Ok(Value::Bool(true)),
		other => other.clone(),
	};
	match &schema {
		Value::Bool(true) => Ok(schema),
		Value::Bool(false) => Err("boolean false schema rejects all outputs".to_string()),
		Value::Object(_) => {
			validate_schema_declaration(&schema)?;
			Ok(schema)
		},
		_ => Err("output schema must be an object, JSON string, or true".to_string()),
	}
}

/// Reject schema keywords this validator cannot enforce instead of silently
/// accepting and ignoring them.
pub fn validate_schema_declaration(schema: &Value) -> Result<(), String> {
	match schema {
		Value::Bool(true) => Ok(()),
		Value::Bool(false) => Err("boolean false schema rejects all outputs".to_string()),
		Value::Object(map) => {
			for (key, value) in map {
				match key.as_str() {
					"type" => validate_type_declaration(value)?,
					"enum" => {
						if !value.is_array() {
							return Err("schema 'enum' must be an array".to_string());
						}
					},
					"required" => {
						let Some(items) = value.as_array() else {
							return Err("schema 'required' must be an array".to_string());
						};
						if items.iter().any(|item| !item.is_string()) {
							return Err("schema 'required' entries must be strings".to_string());
						}
					},
					"properties" | "optionalProperties" => {
						let Some(properties) = value.as_object() else {
							return Err(format!("schema '{key}' must be an object"));
						};
						for (name, property) in properties {
							validate_schema_declaration(property)
								.map_err(|error| format!("{key}.{name}: {error}"))?;
						}
					},
					"elements" | "items" | "values" => match value {
						Value::Bool(_) => {},
						Value::Object(_) => validate_schema_declaration(value)?,
						_ => return Err(format!("schema '{key}' must be a schema object")),
					},
					"additionalProperties" => match value {
						Value::Bool(_) => {},
						Value::Object(_) => validate_schema_declaration(value)?,
						_ => {
							return Err(
								"schema 'additionalProperties' must be a boolean or schema object"
									.to_string(),
							);
						},
					},
					"oneOf" | "anyOf" | "allOf" => {
						let Some(variants) = value.as_array() else {
							return Err(format!("schema '{key}' must be an array"));
						};
						for variant in variants {
							validate_schema_declaration(variant)?;
						}
					},
					"metadata" | "description" | "title" | "default" | "examples" =>
						{},
					other => {
						return Err(format!(
							"unsupported output-schema keyword '{other}': this build enforces only \
							 type/properties/optionalProperties/required/elements/values/items/enum/oneOf/anyOf/allOf/additionalProperties"
						));
					},
				}
			}
			Ok(())
		},
		_ => Err("schema node must be an object or boolean".to_string()),
	}
}

fn validate_type_declaration(value: &Value) -> Result<(), String> {
	match value {
		Value::String(name) => {
			if is_type_name(name) {
				Ok(())
			} else {
				Err(format!("unknown schema type '{name}'"))
			}
		},
		Value::Array(names) => {
			for name in names {
				let Some(name) = name.as_str() else {
					return Err("schema 'type' array entries must be strings".to_string());
				};
				if !is_type_name(name) {
					return Err(format!("unknown schema type '{name}'"));
				}
			}
			Ok(())
		},
		_ => Err("schema 'type' must be a string or array of strings".to_string()),
	}
}

fn is_type_name(name: &str) -> bool {
	matches!(
		name,
		"object"
			| "array"
			| "string"
			| "number"
			| "integer"
			| "boolean"
			| "null"
			| "int8"
			| "uint8"
			| "int16"
			| "uint16"
			| "int32"
			| "uint32"
			| "float32"
			| "float64"
			| "timestamp"
	)
}

/// Validate one value against an already-normalized schema.
pub fn validate_output(schema: &Value, value: &Value) -> Result<(), SchemaFailure> {
	let mut missing = Vec::new();
	match validate_node(schema, value, "", &mut missing) {
		Ok(()) if missing.is_empty() => Ok(()),
		Ok(()) => Err(SchemaFailure {
			message: format!("missing required field(s): {}", missing.join(", ")),
			missing_required: missing,
		}),
		Err(message) => Err(SchemaFailure {
			message,
			missing_required: missing,
		}),
	}
}

fn validate_node(
	schema: &Value,
	value: &Value,
	path: &str,
	missing: &mut Vec<String>,
) -> Result<(), String> {
	match schema {
		Value::Bool(true) => return Ok(()),
		Value::Bool(false) => return Err(at(path, "value is rejected by a false schema")),
		Value::Object(map) => {
			if let Some(enum_values) = map.get("enum").and_then(Value::as_array)
				&& !enum_values.contains(value)
			{
				return Err(at(path, &format!("value {value} is not in enum")));
			}
			if let Some(kind) = map.get("type") {
				check_type(kind, value, path)?;
			}
			for combinator in ["allOf", "anyOf", "oneOf"] {
				let Some(variants) = map.get(combinator).and_then(Value::as_array) else {
					continue;
				};
				let mut errors = Vec::new();
				let mut passed = 0_usize;
				let mut failed_missing = Vec::new();
				for variant in variants {
					// Keep each variant's accumulated `missing` private: a
					// failing sibling must not poison a passing anyOf/oneOf.
					let mut branch_missing = Vec::new();
					match validate_node(variant, value, path, &mut branch_missing) {
						Ok(()) if branch_missing.is_empty() => passed += 1,
						Ok(()) => {
							errors.push(at(path, "missing required field(s) in variant"));
							failed_missing.extend(branch_missing);
						},
						Err(error) => {
							errors.push(error);
							failed_missing.extend(branch_missing);
						},
					}
				}
				let valid = match combinator {
					"allOf" => passed == variants.len(),
					"anyOf" => passed > 0,
					_ => passed == 1,
				};
				if !valid {
					missing.extend(failed_missing);
					return Err(at(
						path,
						&format!(
							"{combinator} failed: {}",
							if errors.is_empty() { "no variant matched".to_string() } else { errors.join("; ") }
						),
					));
				}
			}
			let jtd = is_jtd_object(map);
			if jtd {
				if let Some(elements) = map.get("elements") {
					let Some(items) = value.as_array() else {
						return Err(at(path, "expected an array"));
					};
					for (index, item) in items.iter().enumerate() {
						validate_node(elements, item, &format!("{path}[{index}]"), missing)?;
					}
					return Ok(());
				}
				if let Some(values) = map.get("values") {
					let Some(entries) = value.as_object() else {
						return Err(at(path, "expected an object"));
					};
					for (key, entry) in entries {
						validate_node(values, entry, &join(path, key), missing)?;
					}
					return Ok(());
				}
				validate_object(map, value, path, missing, true)?;
				return Ok(());
			}
			if map.contains_key("properties")
				|| map.contains_key("required")
				|| map.contains_key("additionalProperties")
			{
				validate_object(map, value, path, missing, false)?;
			}
			if let Some(items) = map.get("items") {
				let Some(entries) = value.as_array() else {
					return Err(at(path, "expected an array"));
				};
				for (index, entry) in entries.iter().enumerate() {
					validate_node(items, entry, &format!("{path}[{index}]"), missing)?;
				}
			}
			Ok(())
		},
		_ => Err(at(path, "invalid schema node")),
	}
}

fn is_jtd_object(map: &serde_json::Map<String, Value>) -> bool {
	map.contains_key("optionalProperties")
		|| map.contains_key("elements")
		|| map.contains_key("values")
		|| map.contains_key("discriminator")
		|| map.contains_key("ref")
		|| (map.contains_key("properties") && !map.contains_key("type"))
}

fn validate_object(
	map: &serde_json::Map<String, Value>,
	value: &Value,
	path: &str,
	missing: &mut Vec<String>,
	jtd: bool,
) -> Result<(), String> {
	let Some(object) = value.as_object() else {
		return Err(at(path, "expected an object"));
	};
	let properties = map.get("properties").and_then(Value::as_object);
	let optional = map.get("optionalProperties").and_then(Value::as_object);
	if let Some(properties) = properties {
		for (name, schema) in properties {
			match object.get(name) {
				Some(entry) => validate_node(schema, entry, &join(path, name), missing)?,
				None if jtd => missing.push(join(path, name)),
				None => {
					let required = map
						.get("required")
						.and_then(Value::as_array)
						.is_some_and(|items| items.iter().any(|item| item.as_str() == Some(name)));
					if required {
						missing.push(join(path, name));
					}
				},
			}
		}
	}
	if let Some(optional) = optional {
		for (name, schema) in optional {
			if let Some(entry) = object.get(name) {
				validate_node(schema, entry, &join(path, name), missing)?;
			}
		}
	}
	if let Some(required) = map.get("required").and_then(Value::as_array) {
		for name in required.iter().filter_map(Value::as_str) {
			if !object.contains_key(name) && !missing.iter().any(|item| item == &join(path, name)) {
				missing.push(join(path, name));
			}
		}
	}
	let additional = map.get("additionalProperties");
	let declared: BTreeSet<&str> = properties
		.into_iter()
		.flat_map(serde_json::Map::keys)
		.chain(optional.into_iter().flat_map(serde_json::Map::keys))
		.map(String::as_str)
		.collect();
	for (key, entry) in object {
		if declared.contains(key.as_str()) {
			continue;
		}
		match additional {
			Some(Value::Bool(false)) => {
				return Err(at(path, &format!("unexpected property '{key}'")));
			},
			Some(schema @ Value::Object(_)) => {
				validate_node(schema, entry, &join(path, key), missing)?;
			},
			_ if jtd => {
				return Err(at(path, &format!("unexpected property '{key}'")));
			},
			_ => {},
		}
	}
	Ok(())
}

fn check_type(kind: &Value, value: &Value, path: &str) -> Result<(), String> {
	let names: Vec<&str> = match kind {
		Value::String(name) => vec![name.as_str()],
		Value::Array(names) => names.iter().filter_map(Value::as_str).collect(),
		_ => return Err(at(path, "invalid schema type")),
	};
	if names.iter().any(|name| type_matches(name, value)) {
		return Ok(());
	}
	Err(at(
		path,
		&format!("expected type {}, got {}", names.join("|"), json_type(value)),
	))
}

fn type_matches(name: &str, value: &Value) -> bool {
	match name {
		"object" => value.is_object(),
		"array" => value.is_array(),
		"string" | "timestamp" => value.is_string(),
		"boolean" => value.is_boolean(),
		"null" => value.is_null(),
		"number" | "float32" | "float64" => value.is_number(),
		"integer" | "int8" | "uint8" | "int16" | "uint16" | "int32" | "uint32" => {
			value.as_i64().is_some() || value.as_u64().is_some()
		},
		_ => false,
	}
}

fn json_type(value: &Value) -> &'static str {
	match value {
		Value::Null => "null",
		Value::Bool(_) => "boolean",
		Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
		Value::Number(_) => "number",
		Value::String(_) => "string",
		Value::Array(_) => "array",
		Value::Object(_) => "object",
	}
}

/// True when a schema node declares an array value. Used to accumulate a single
/// incremental `type:["findings"]` yield as a one-element list.
#[must_use]
pub fn schema_is_array(value: &Value) -> bool {
	match value {
		Value::Object(map) => {
			if map.get("elements").is_some() {
				return true;
			}
			if let Some(kind) = map.get("type") {
				let array = match kind {
					Value::String(name) => name == "array",
					Value::Array(names) => names.iter().any(|name| name.as_str() == Some("array")),
					_ => false,
				};
				if array {
					return true;
				}
			}
			["anyOf", "oneOf", "allOf"].iter().any(|key| {
				map.get(*key)
					.and_then(Value::as_array)
					.is_some_and(|variants| variants.iter().any(schema_is_array))
			})
		},
		_ => false,
	}
}

/// Top-level schema property names that hold arrays.
#[must_use]
pub fn array_valued_labels(schema: &Value) -> BTreeSet<String> {
	let mut labels = BTreeSet::new();
	for section in ["properties", "optionalProperties"] {
		let Some(properties) = schema.get(section).and_then(Value::as_object) else {
			continue;
		};
		for (name, property) in properties {
			if schema_is_array(property) {
				labels.insert(name.clone());
			}
		}
	}
	labels
}

fn at(path: &str, message: &str) -> String {
	if path.is_empty() {
		format!("$: {message}")
	} else {
		format!("{path}: {message}")
	}
}

fn join(path: &str, key: &str) -> String {
	if path.is_empty() {
		key.to_string()
	} else {
		format!("{path}.{key}")
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn effort_prefixes_are_unique_and_minimum_two_chars() {
		assert_eq!(parse_effort_prefix("hi"), Some("high"));
		assert_eq!(parse_effort_prefix("med"), Some("medium"));
		assert_eq!(parse_effort_prefix("lo"), Some("low"));
		assert_eq!(parse_effort_prefix("m"), None);
		assert_eq!(parse_effort_prefix("x"), None);
		assert_eq!(thinking_to_effort("auto"), Ok(None));
		assert_eq!(thinking_to_effort("medium"), Ok(Some("medium")));
		assert!(thinking_to_effort("bogus").is_err());
	}

	#[test]
	fn effort_suffix_splits_only_for_known_selectors() {
		assert_eq!(split_effort_selector("provider/model:high"), ("provider/model", Some("high")));
		assert_eq!(split_effort_selector("provider/model"), ("provider/model", None));
		assert_eq!(split_effort_selector("a:b"), ("a:b", None));
	}

	#[derive(Default)]
	struct RecordingFactory {
		seen: std::sync::Mutex<Vec<(Option<String>, Option<String>)>>,
	}

	impl ChildModelFactory for RecordingFactory {
		fn can_resolve(&self, _model: &str) -> bool {
			true
		}

		fn resolve(
			&self,
			_model: &str,
			effort: Option<&str>,
			fallback_effort: Option<&str>,
		) -> Result<ChildModel, String> {
			self.seen.lock().expect("lock").push((
				effort.map(str::to_string),
				fallback_effort.map(str::to_string),
			));
			Err("recorded".to_string())
		}
	}

	fn effort_agent(model: &str, thinking: &str) -> AgentDefinition {
		AgentDefinition {
			name: "probe".to_string(),
			description: "test".to_string(),
			tools: None,
			spawns: None,
			model: vec![model.to_string()],
			output: None,
			thinking_level: Some(thinking.to_string()),
			blocking: false,
			read_summarize: None,
			prewalk: None,
			advisor: None,
			autoload_skills: Vec::new(),
			system_prompt: String::new(),
			source: AgentSource::Project,
			file_path: "embedded:probe.md".to_string(),
		}
	}

	#[test]
	fn caller_effort_beats_pattern_suffix_and_thinking_level() {
		let factory = Arc::new(RecordingFactory::default());
		let catalog = AgentCatalog::new(Vec::new(), None)
			.with_model_factory(Arc::clone(&factory) as Arc<dyn ChildModelFactory>);
		let agent = effort_agent("local/model:high", "low");
		let factory_ref: &dyn ChildModelFactory = factory.as_ref();

		assert!(catalog.resolve_provider(&agent, Some("medium"), Some(factory_ref)).is_err());
		assert!(catalog.resolve_provider(&agent, None, Some(factory_ref)).is_err());

		let agent_no_suffix = effort_agent("local/model", "low");
		assert!(catalog.resolve_provider(&agent_no_suffix, None, Some(factory_ref)).is_err());
		// `auto` is a valid inherit hint on both the caller and the agent.
		assert!(catalog
			.resolve_provider(&agent_no_suffix, Some("auto"), Some(factory_ref))
			.is_err());

		let seen = factory.seen.lock().expect("lock");
		assert_eq!(seen[0], (Some("medium".to_string()), Some("high".to_string())));
		assert_eq!(seen[1], (None, Some("high".to_string())));
		assert_eq!(seen[2], (None, Some("low".to_string())));
		assert_eq!(seen[3], (None, Some("low".to_string())));
	}

	#[test]
	fn spawns_report_default_and_allowlist() {
		assert_eq!(Spawns::All.default_agent(), "task");
		assert!(Spawns::All.allows("scout"));
		let only = Spawns::Only(vec!["scout".into(), "reviewer".into()]);
		assert_eq!(only.default_agent(), "scout");
		assert!(only.allows("reviewer"));
		assert!(!only.allows("task"));
	}

	#[test]
	fn jtd_properties_are_required_and_closed() {
		let schema = json!({
			"properties": {
				"summary": { "type": "string" },
				"files": { "elements": { "properties": { "path": { "type": "string" } } } }
			}
		});
		let valid = json!({ "summary": "ok", "files": [{ "path": "a.rs" }] });
		assert!(validate_output(&schema, &valid).is_ok());
		let missing = validate_output(&schema, &json!({ "summary": "ok" })).expect_err("missing files");
		assert_eq!(missing.missing_required, vec!["files".to_string()]);
		let extra = validate_output(&schema, &json!({ "summary": "ok", "files": [], "x": 1 }))
			.expect_err("closed");
		assert!(extra.message.contains("unexpected property 'x'"));
	}

	#[test]
	fn json_schema_required_and_types_are_enforced() {
		let schema = json!({
			"type": "object",
			"properties": { "count": { "type": "integer" }, "name": { "type": "string" } },
			"required": ["count"]
		});
		assert!(validate_output(&schema, &json!({ "count": 2 })).is_ok());
		let failure = validate_output(&schema, &json!({ "name": "x" })).expect_err("missing count");
		assert_eq!(failure.missing_required, vec!["count".to_string()]);
		let wrong = validate_output(&schema, &json!({ "count": "two" })).expect_err("wrong type");
		assert!(wrong.message.contains("expected type integer"));
	}

	#[test]
	fn output_schema_string_and_null_normalize_like_caller() {
		assert!(normalize_schema(&json!("{\"type\":\"object\"}")).is_ok());
		assert_eq!(normalize_schema(&Value::Null).expect("null"), Value::Bool(true));
		assert!(normalize_schema(&json!("not json")).is_err());
	}

	#[test]
	fn combinator_branch_missing_does_not_poison_passing_variant() {
		let schema = json!({
			"anyOf": [
				{ "type": "object", "required": ["a"] },
				{ "type": "object", "required": ["b"] }
			]
		});
		assert!(
			validate_output(&schema, &json!({ "a": 1 })).is_ok(),
			"failed sibling's missing must be discarded"
		);
		let failure = validate_output(&schema, &json!({ "c": 1 })).expect_err("neither");
		assert!(failure.missing_required.contains(&"a".to_string()));
		assert!(failure.missing_required.contains(&"b".to_string()));

		let exclusive = json!({
			"oneOf": [
				{ "type": "object", "properties": { "a": { "type": "integer" } }, "required": ["a"] },
				{ "type": "object", "properties": { "b": { "type": "integer" } }, "required": ["b"] }
			]
		});
		assert!(validate_output(&exclusive, &json!({ "a": 1 })).is_ok());
		assert!(validate_output(&exclusive, &json!({ "a": 1, "b": 2 })).is_err());
	}

	#[test]
	fn unsupported_keywords_are_rejected_not_ignored() {
		let error = normalize_schema(&json!({ "type": "string", "pattern": "^a" })).expect_err("pattern");
		assert!(error.contains("unsupported output-schema keyword 'pattern'"));
		assert!(normalize_schema(&json!("{\"type\":\"string\"}")).is_ok());
		assert!(normalize_schema(&json!(false)).is_err());
	}

	#[test]
	fn array_labels_cover_jtd_and_json_shapes() {
		let schema = json!({
			"properties": {
				"files": { "elements": { "type": "string" } },
				"summary": { "type": "string" },
				"findings": { "type": "array", "items": { "type": "string" } }
			}
		});
		let labels = array_valued_labels(&schema);
		assert!(labels.contains("files"));
		assert!(labels.contains("findings"));
		assert!(!labels.contains("summary"));
	}
}
