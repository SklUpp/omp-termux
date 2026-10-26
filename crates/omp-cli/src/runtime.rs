//! `pi-agent` backed runner: OpenAI-compatible provider plus builtin tools.
//!
//! The CLI resolves data (config, catalog, prompts, skills) into
//! [`RunRequest`]; this module maps it onto `pi-agent`'s `AgentConfig` and
//! converts the loop's turn results back into the CLI [`Event`] stream.

use std::collections::BTreeMap;
use std::sync::Arc;

use pi_agent::{
	AgentConfig, AgentLoop, AgentState, BashTool, EditTool, GlobTool, GrepTool, ModelProvider,
	OpenAiCompatibleProvider, ReadTool, TodoTool, Tool, ToolRegistry, TurnResult, WriteTool,
};

use crate::agent::{AgentRunner, Event, EventStream, RunRequest};
use crate::approve::Decision;
use crate::{Error, Result};

/// Assistant-turn budget the CLI hands to the agent loop.
pub const DEFAULT_MAX_TURNS: usize = 32;
/// Context window assumed when the catalog declares none (matches `pi-agent`).
pub const DEFAULT_CONTEXT_LIMIT: usize = 32_768;

/// Builtin tools: name and whether the tool only reads.
///
/// `pi-agent` has no approval hook, so the registry *is* the approval gate:
/// a tool is offered to the model only when the approval matrix would
/// auto-approve it.
const BUILTIN_TOOLS: [(&str, bool); 7] = [
	("read", true),
	("glob", true),
	("grep", true),
	("write", false),
	("edit", false),
	("bash", false),
	("todo", false),
];

/// Synchronous `AgentRunner` that drives `pi-agent` on its own tokio runtime.
pub struct PiAgentRunner {
	runtime: tokio::runtime::Runtime,
}

impl PiAgentRunner {
	pub fn new() -> Result<Self> {
		let runtime = tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.build()
			.map_err(|err| Error::with_source("cannot start tokio runtime for pi-agent", err))?;
		Ok(Self { runtime })
	}
}

impl AgentRunner for PiAgentRunner {
	fn run(&self, request: &RunRequest) -> Result<EventStream> {
		let plan = RunPlan::resolve(request)?;
		let registry = build_registry(&plan.tools)?;
		let cwd = request.cwd.clone();
		let prompt = request.prompt.clone();
		let mut messages = history_messages(&request.history);
		if let Some(system) = plan.system_prompt {
			messages.insert(0, pi_agent::Message::system(system));
		}
		let dialog = async move {
			let provider = OpenAiCompatibleProvider::new(plan.base_url, plan.api_key, plan.model)
				.map_err(|err| Error::with_source("pi-agent provider setup failed", err))?
				.with_effort(plan.effort);
			let config = AgentConfig {
				max_turns: plan.max_turns,
				context_limit: plan.context_limit,
				system_prompt: None,
				cwd,
			};
			let provider: Arc<dyn ModelProvider> = Arc::new(provider);
			let agent = AgentLoop::new(config, provider, registry);
			let mut state = AgentState { messages, total_turns: 0 };
			let turns = agent
				.run_dialog(&prompt, &mut state, None)
				.await
				.map_err(|err| Error::with_source("pi-agent run failed", err))?;
			Ok::<_, Error>(events_from_turns(turns))
		};
		// Runtime-level bound: `pi-agent` has no cancellable deadline seam, so the
		// whole dialog is wrapped in a tokio timeout. Stream-level enforcement in
		// `deadline::drive` still preserves events that already arrived.
		let events = if let Some(budget) = request.max_time {
			match self.runtime.block_on(tokio::time::timeout(budget, dialog)) {
				Ok(result) => result?,
				Err(_) => return Err(Error::new(crate::deadline::TIMEOUT_MESSAGE)),
			}
		} else {
			self.runtime.block_on(dialog)?
		};
		Ok(Box::new(events.into_iter().map(Ok::<Event, Error>)))
	}
}

/// Provider/model/budget resolution from a [`RunRequest`].
struct RunPlan {
	base_url: String,
	api_key: String,
	model: String,
	effort: Option<String>,
	max_turns: usize,
	context_limit: usize,
	system_prompt: Option<String>,
	tools: Vec<&'static str>,
}

impl RunPlan {
	fn resolve(request: &RunRequest) -> Result<Self> {
		let requested = request
			.model
			.clone()
			.ok_or_else(|| Error::new("no model: pass --model or set defaultModel in config"))?;
		// `provider/id` (first segment is the provider, as in TS model roles),
		// `provider/id:effort`, or a bare id resolved through the catalog.
		let mut effort: Option<&str> = None;
		let (provider, model) = match request.provider.as_deref() {
			Some(provider) => {
				let prefix = format!("{provider}/");
				let raw = requested
					.strip_prefix(prefix.as_str())
					.unwrap_or(&requested);
				let (id, parsed) = split_effort(raw);
				effort = parsed;
				(provider.to_string(), id.to_string())
			},
			None => match requested.split_once('/') {
				Some((provider, id)) if !provider.is_empty() && !id.is_empty() => {
					let (id, parsed) = split_effort(id);
					effort = parsed;
					(provider.to_string(), id.to_string())
				},
				_ => {
					let (id, parsed) = split_effort(&requested);
					effort = parsed;
					let provider = request
						.catalog
						.provider_of(id)
						.map(str::to_string)
						.ok_or_else(|| {
							Error::new(format!(
								"no provider for model `{id}`: pass --provider or use `provider/{id}`"
							))
						})?;
					(provider, id.to_string())
				},
			},
		};
		// Connection block: config.yml first, then models.yml providers.<id>.
		let connection = request
			.config
			.providers
			.get(&provider)
			.cloned()
			.or_else(|| request.catalog.connections.get(&provider).cloned());
		let base_url = connection
			.as_ref()
			.and_then(|connection| connection.base_url.clone())
			.ok_or_else(|| {
				Error::new(format!("provider `{provider}` has no baseUrl in config.yml or models.yml"))
			})?;
		// A keyless provider (local gateway, `auth: none`) is legal: the OpenAI
		// client sends an empty bearer token.
		let api_key = request
			.api_key
			.clone()
			.or_else(|| {
				connection
					.as_ref()
					.and_then(|connection| connection.resolve_api_key())
			})
			.unwrap_or_default();
		let context_limit = request
			.catalog
			.context_window(&provider, &model)
			.unwrap_or(DEFAULT_CONTEXT_LIMIT);
		Ok(Self {
			base_url,
			api_key,
			model,
			effort: effort.map(str::to_string),
			max_turns: request.max_turns.max(1),
			context_limit,
			system_prompt: compose_system_prompt(request),
			tools: allowed_tools(request)?,
		})
	}
}

/// Trailing `:effort` selector (`:high`, `:xhi`, …) is not part of the model id.
const EFFORT_SELECTORS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

fn split_effort(model: &str) -> (&str, Option<&str>) {
	let Some((id, suffix)) = model.rsplit_once(':') else {
		return (model, None);
	};
	if id.is_empty() || suffix.len() < 2 {
		return (model, None);
	}
	let mut matching = EFFORT_SELECTORS
		.iter()
		.filter(|selector| selector.starts_with(suffix))
		.copied();
	let Some(selector) = matching.next() else {
		return (model, None);
	};
	if matching.next().is_some() {
		return (model, None);
	}
	(id, Some(selector))
}

/// Tools the model may call, derived from the approval matrix.
fn allowed_tools(request: &RunRequest) -> Result<Vec<&'static str>> {
	if !request.tools_enabled {
		return Ok(Vec::new());
	}
	let mut tools: Vec<&'static str> = BUILTIN_TOOLS
		.iter()
		.filter(|(name, read_only)| request.approvals.decide(name, *read_only) == Decision::Auto)
		.map(|(name, _)| *name)
		.collect();
	if tools.is_empty() {
		return Err(Error::new(
			"approval gating requires --yolo in this build: pi-agent has no approval hook, so \
			 --approval-mode always-ask needs --allow <tool> to offer any tool",
		));
	}
	tools.sort_unstable();
	Ok(tools)
}

fn build_registry(tools: &[&'static str]) -> Result<ToolRegistry> {
	let mut registry = ToolRegistry::new();
	for name in tools {
		let tool: Arc<dyn Tool> = match *name {
			"read" => Arc::new(ReadTool),
			"glob" => Arc::new(GlobTool),
			"grep" => Arc::new(GrepTool),
			"write" => Arc::new(WriteTool),
			"edit" => Arc::new(EditTool),
			"bash" => Arc::new(BashTool),
			"todo" => Arc::new(TodoTool::new()),
			other => {
				return Err(Error::new(format!(
					"unknown builtin tool `{other}`: BUILTIN_TOOLS and build_registry drifted"
				)));
			},
		};
		registry.register(tool);
	}
	Ok(registry)
}

/// File-resolved prompt pieces joined into one system message.
fn compose_system_prompt(request: &RunRequest) -> Option<String> {
	let mut parts: Vec<String> = Vec::new();
	for text in [request.system_prompt.as_deref(), request.append_system_prompt.as_deref()]
		.into_iter()
		.flatten()
	{
		if !text.trim().is_empty() {
			parts.push(text.trim_end().to_string());
		}
	}
	for file in &request.context_files {
		parts.push(format!("# {}\n\n{}", file.path.display(), file.content.trim_end()));
	}
	for skill in &request.skills {
		parts.push(format!("# Skill: {}\n\n{}", skill.name, skill.body.trim()));
	}
	(!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// Previous session turns as `pi-agent` history messages.
fn history_messages(history: &[crate::session::TurnRecord]) -> Vec<pi_agent::Message> {
	let mut messages = Vec::new();
	for turn in history {
		messages.push(pi_agent::Message::user(turn.prompt.clone()));
		if turn.tool_calls.is_empty() {
			if !turn.assistant.is_empty() {
				messages.push(pi_agent::Message::assistant(turn.assistant.clone()));
			}
			continue;
		}
		let calls: Vec<pi_agent::ToolCall> = turn
			.tool_calls
			.iter()
			.map(|call| pi_agent::ToolCall {
				id: call.id.clone(),
				name: call.name.clone(),
				arguments: call.arguments.clone(),
			})
			.collect();
		messages.push(pi_agent::Message::assistant_with_tools(turn.assistant.clone(), calls));
		for call in &turn.tool_calls {
			messages.push(pi_agent::Message::tool(call.id.clone(), call.result.clone()));
		}
	}
	messages
}

/// Map `pi-agent` turn results onto the CLI event stream.
fn events_from_turns(turns: Vec<TurnResult>) -> Vec<Event> {
	let mut events = Vec::new();
	let mut final_text = None;
	for turn in turns {
		let tool_names: BTreeMap<String, String> = turn
			.message
			.tool_calls
			.iter()
			.map(|call| (call.id.clone(), call.name.clone()))
			.collect();
		events.push(Event::MessageStart { role: "assistant".to_string() });
		if !turn.message.content.is_empty() {
			events.push(Event::MessageUpdate { delta: turn.message.content.clone() });
		}
		events.push(Event::MessageEnd { text: turn.message.content.clone() });
		for call in &turn.message.tool_calls {
			events.push(Event::ToolStart {
				id: call.id.clone(),
				tool: call.name.clone(),
				args: call.arguments.clone(),
			});
		}
		for result in &turn.tool_results {
			let tool = tool_names
				.get(&result.tool_call_id)
				.cloned()
				.unwrap_or_else(|| "tool".to_string());
			let error = result.is_error.then(|| result.content.clone());
			events.push(Event::ToolEnd {
				id: result.tool_call_id.clone(),
				tool,
				result: result.content.clone(),
				error,
			});
		}
		let stop_reason = if turn.is_completed {
			"end_turn"
		} else {
			"tool_use"
		};
		events.push(Event::TurnEnd { stop_reason: stop_reason.to_string() });
		if turn.is_completed {
			final_text = Some(turn.message.content);
		}
	}
	if let Some(text) = final_text {
		events.push(Event::AgentEnd { text });
	}
	events
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeMap;
	use std::path::PathBuf;
	use std::sync::Arc;

	use pi_agent::{Message, ToolCall, ToolResult};

	use super::*;
	use crate::approve::{ApprovalMode, Approvals};
	use crate::catalog::{Catalog, ModelInfo};
	use crate::config::{Config, ProviderConfig};
	use crate::output::OutputMode;
	use crate::prompts::ContextFile;
	use crate::skills::Skill;

	fn catalog() -> Catalog {
		let mut providers = BTreeMap::new();
		let mut models = BTreeMap::new();
		models.insert(
			"llama3".to_string(),
			ModelInfo { provider: "local".to_string(), context_window: Some(8_192) },
		);
		providers.insert("local".to_string(), models);
		Catalog { providers, connections: BTreeMap::new(), source: PathBuf::from("test-models.json") }
	}

	fn catalog_with_connection(provider: &str, base_url: &str, api_key: Option<&str>) -> Catalog {
		let mut catalog = catalog();
		catalog.connections.insert(
			provider.to_string(),
			ProviderConfig {
				base_url: Some(base_url.to_string()),
				api_key: api_key.map(str::to_string),
				..ProviderConfig::default()
			},
		);
		catalog
	}

	fn request(approvals: Approvals) -> RunRequest {
		let mut config = Config::default();
		config.providers.insert(
			"local".to_string(),
			ProviderConfig {
				base_url: Some("http://127.0.0.1:8080/v1".to_string()),
				api_key: Some("sk-test".to_string()),
				..ProviderConfig::default()
			},
		);
		RunRequest {
			prompt: "fix the build".to_string(),
			cwd: PathBuf::from("/work"),
			output_mode: OutputMode::Text,
			model: Some("local/llama3".to_string()),
			provider: None,
			api_key: None,
			system_prompt: Some("base prompt".to_string()),
			append_system_prompt: Some("appended".to_string()),
			context_files: vec![ContextFile {
				path: PathBuf::from("/work/.omp/AGENTS.md"),
				content: "repo rules".to_string(),
			}],
			skills: vec![Skill {
				name: "review".to_string(),
				description: "review code".to_string(),
				path: PathBuf::from("/work/.omp/skills/review/SKILL.md"),
				body: "skill body".to_string(),
			}],
			config,
			catalog: Arc::new(catalog()),
			approvals,
			max_time: None,
			tools_enabled: true,
			max_turns: 5,
			history: Vec::new(),
		}
	}

	fn yolo() -> Approvals {
		Approvals::new(ApprovalMode::Yolo, Vec::new())
	}

	#[test]
	fn plan_maps_request_onto_provider_config() {
		let plan = RunPlan::resolve(&request(yolo())).expect("plan");
		assert_eq!(plan.base_url, "http://127.0.0.1:8080/v1");
		assert_eq!(plan.api_key, "sk-test");
		assert_eq!(plan.model, "llama3");
		assert_eq!(plan.max_turns, 5);
		assert_eq!(plan.context_limit, 8_192);
		assert_eq!(plan.tools, ["bash", "edit", "glob", "grep", "read", "todo", "write"]);
		assert_eq!(plan.effort, None);
		let system = plan.system_prompt.expect("system prompt");
		for part in ["base prompt", "appended", "repo rules", "skill body"] {
			assert!(system.contains(part), "system prompt is missing {part}");
		}
	}

	#[test]
	fn plan_requires_a_connection_and_allows_keyless_providers() {
		let mut missing_url = request(yolo());
		missing_url.config.providers.clear();
		let err = RunPlan::resolve(&missing_url).err().expect("no baseUrl");
		assert!(err.to_string().contains("has no baseUrl"));

		// Keyless local gateways are legal: the client sends an empty bearer.
		let mut missing_key = request(yolo());
		if let Some(provider) = missing_key.config.providers.get_mut("local") {
			provider.api_key = None;
		}
		let plan = RunPlan::resolve(&missing_key).expect("keyless provider");
		assert_eq!(plan.api_key, "");

		let mut no_model = request(yolo());
		no_model.model = None;
		assert!(RunPlan::resolve(&no_model).is_err());
	}

	#[test]
	fn effort_suffix_is_split_from_the_model_id() {
		assert_eq!(split_effort("llama3"), ("llama3", None));
		assert_eq!(split_effort("llama3:high"), ("llama3", Some("high")));
		assert_eq!(split_effort("llama3:hi"), ("llama3", Some("high")));
		assert_eq!(split_effort("llama3:xhi"), ("llama3", Some("xhigh")));
		assert_eq!(split_effort("llama3:max"), ("llama3", Some("max")));
		// Not an effort selector: the suffix stays part of the id.
		assert_eq!(split_effort("qwen:7b"), ("qwen:7b", None));
		assert_eq!(split_effort("llama3:m"), ("llama3:m", None));
		assert_eq!(split_effort(":high"), (":high", None));
	}

	#[test]
	fn explicit_provider_still_parses_the_effort_suffix() {
		let mut request = request(yolo());
		request.model = Some("llama3:high".to_string());
		request.provider = Some("local".to_string());
		let plan = RunPlan::resolve(&request).expect("plan");
		assert_eq!(plan.model, "llama3");
		assert_eq!(plan.effort.as_deref(), Some("high"));

		request.model = Some("llama3".to_string());
		let plan = RunPlan::resolve(&request).expect("plan");
		assert_eq!(plan.effort, None);
	}

	#[test]
	fn registry_implements_every_builtin_tool_name() {
		let names: Vec<&'static str> = BUILTIN_TOOLS.iter().map(|(name, _)| *name).collect();
		build_registry(&names).expect("every BUILTIN_TOOLS entry is implemented");
		let err = build_registry(&["nope"]).err().expect("unknown tool");
		assert!(err.to_string().contains("unknown builtin tool `nope`"));
	}

	#[test]
	fn provider_prefixed_model_resolves_through_models_yml_connection() {
		let mut nested = request(yolo());
		nested.model = Some("9router/ocg/mimo-v2.6-pro:high".to_string());
		nested.provider = None;
		nested.config.providers.clear();
		nested.catalog =
			Arc::new(catalog_with_connection("9router", "http://127.0.0.1:20128/v1", None));
		let plan = RunPlan::resolve(&nested).expect("plan");
		assert_eq!(plan.model, "ocg/mimo-v2.6-pro");
		assert_eq!(plan.base_url, "http://127.0.0.1:20128/v1");
		assert_eq!(plan.api_key, "");
		// `:high` is a reasoning-effort selector, not part of the model id; it is
		// normalized to the full selector and carried to the provider builder.
		assert_eq!(plan.effort.as_deref(), Some("high"));

		let mut explicit = request(yolo());
		explicit.model = Some("ocg/mimo-v2.6-pro".to_string());
		explicit.provider = Some("9router".to_string());
		explicit.config.providers.clear();
		explicit.catalog = Arc::new(catalog_with_connection(
			"9router",
			"http://127.0.0.1:20128/v1",
			Some("sk-models-yml"),
		));
		let plan = RunPlan::resolve(&explicit).expect("plan");
		assert_eq!(plan.model, "ocg/mimo-v2.6-pro");
		assert_eq!(plan.api_key, "sk-models-yml");

		// Explicit provider plus the fully prefixed form strips the prefix once.
		let mut prefixed = request(yolo());
		prefixed.model = Some("9router/ocg/mimo-v2.6-pro".to_string());
		prefixed.provider = Some("9router".to_string());
		prefixed.config.providers.clear();
		prefixed.catalog =
			Arc::new(catalog_with_connection("9router", "http://127.0.0.1:20128/v1", None));
		let plan = RunPlan::resolve(&prefixed).expect("plan");
		assert_eq!(plan.model, "ocg/mimo-v2.6-pro");
	}

	#[test]
	fn config_connection_wins_over_models_yml() {
		let mut request = request(yolo());
		request.catalog = Arc::new(catalog_with_connection(
			"local",
			"http://models-yml.example/v1",
			Some("sk-models-yml"),
		));
		let plan = RunPlan::resolve(&request).expect("plan");
		assert_eq!(plan.base_url, "http://127.0.0.1:8080/v1");
		assert_eq!(plan.api_key, "sk-test");
	}

	#[test]
	fn write_mode_offers_read_only_tools_only() {
		let plan =
			RunPlan::resolve(&request(Approvals::new(ApprovalMode::Write, Vec::new()))).expect("plan");
		assert_eq!(plan.tools, ["glob", "grep", "read"]);
	}

	#[test]
	fn always_ask_needs_an_explicit_allow() {
		let err = RunPlan::resolve(&request(Approvals::new(ApprovalMode::AlwaysAsk, Vec::new())))
			.err()
			.expect("gating impossible");
		assert!(err.to_string().contains("approval gating requires --yolo"));

		let allowed = Approvals::new(ApprovalMode::AlwaysAsk, ["bash".to_string()]);
		let plan = RunPlan::resolve(&request(allowed)).expect("plan");
		assert_eq!(plan.tools, ["bash"]);
	}

	#[test]
	fn no_tools_disables_the_registry() {
		let mut request = request(yolo());
		request.tools_enabled = false;
		let plan = RunPlan::resolve(&request).expect("plan");
		assert!(plan.tools.is_empty());
	}

	#[test]
	fn history_turns_become_pi_agent_messages() {
		let history = vec![crate::session::TurnRecord {
			prompt: "fix".to_string(),
			assistant: "done".to_string(),
			stop_reason: "end_turn".to_string(),
			tool_calls: vec![crate::session::ToolCallRecord {
				id: "call-1".to_string(),
				name: "read".to_string(),
				arguments: serde_json::json!({"path": "a.txt"}),
				result: "contents".to_string(),
				is_error: false,
			}],
		}];
		let messages = history_messages(&history);
		assert_eq!(messages.len(), 3);
		assert_eq!(messages[0].role, pi_agent::Role::User);
		assert_eq!(messages[0].content, "fix");
		assert_eq!(messages[1].role, pi_agent::Role::Assistant);
		assert_eq!(messages[1].tool_calls.len(), 1);
		assert_eq!(messages[1].tool_calls[0].id, "call-1");
		assert_eq!(messages[2].role, pi_agent::Role::Tool);
		assert_eq!(messages[2].tool_call_id.as_deref(), Some("call-1"));
		assert_eq!(messages[2].content, "contents");
	}

	#[test]
	fn turn_results_map_to_cli_events() {
		let turn = TurnResult {
			message: Message::assistant_with_tools(
				"done",
				vec![ToolCall {
					id: "call-1".to_string(),
					name: "read".to_string(),
					arguments: serde_json::json!({"path": "a.txt"}),
				}],
			),
			tool_results: vec![ToolResult {
				tool_call_id: "call-1".to_string(),
				content: "boom".to_string(),
				is_error: true,
			}],
			is_completed: true,
		};
		let events = events_from_turns(vec![turn]);
		assert!(matches!(events.first(), Some(Event::MessageStart { .. })));
		let tool_start = events
			.iter()
			.find_map(|event| match event {
				Event::ToolStart { tool, .. } => Some(tool.as_str()),
				_ => None,
			})
			.expect("tool start");
		assert_eq!(tool_start, "read");
		let tool_error = events.iter().find_map(|event| match event {
			Event::ToolEnd { tool, error, .. } => Some((tool.as_str(), error.clone())),
			_ => None,
		});
		assert_eq!(tool_error, Some(("read", Some("boom".to_string()))));
		assert!(matches!(
			events.last(),
			Some(Event::AgentEnd { text }) if text == "done"
		));
	}
}
