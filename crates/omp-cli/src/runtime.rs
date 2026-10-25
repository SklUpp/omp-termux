//! `pi-agent` backed runner: OpenAI-compatible provider plus builtin tools.
//!
//! The CLI resolves data (config, catalog, prompts, skills) into
//! [`RunRequest`]; this module maps it onto `pi-agent`'s `AgentConfig` and
//! converts the loop's turn results back into the CLI [`Event`] stream.

use std::collections::BTreeMap;
use std::sync::Arc;

use pi_agent::{
	AgentConfig, AgentLoop, AgentState, BashTool, GlobTool, GrepTool, ModelProvider,
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
const BUILTIN_TOOLS: [(&str, bool); 6] = [
	("read", true),
	("glob", true),
	("grep", true),
	("write", false),
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
		let registry = build_registry(&plan.tools);
		let cwd = request.cwd.clone();
		let prompt = request.prompt.clone();
		let events = self.runtime.block_on(async move {
			let provider = OpenAiCompatibleProvider::new(plan.base_url, plan.api_key, plan.model)
				.map_err(|err| Error::with_source("pi-agent provider setup failed", err))?;
			let config = AgentConfig {
				max_turns: plan.max_turns,
				context_limit: plan.context_limit,
				system_prompt: plan.system_prompt,
				cwd,
			};
			let provider: Arc<dyn ModelProvider> = Arc::new(provider);
			let agent = AgentLoop::new(config, provider, registry);
			let mut state = AgentState::default();
			let turns = agent
				.run_dialog(&prompt, &mut state, None)
				.await
				.map_err(|err| Error::with_source("pi-agent run failed", err))?;
			Ok::<_, Error>(events_from_turns(turns))
		})?;
		Ok(Box::new(events.into_iter().map(Ok::<Event, Error>)))
	}
}

/// Provider/model/budget resolution from a [`RunRequest`].
struct RunPlan {
	base_url: String,
	api_key: String,
	model: String,
	max_turns: usize,
	context_limit: usize,
	system_prompt: Option<String>,
	tools: Vec<&'static str>,
}

impl RunPlan {
	fn resolve(request: &RunRequest) -> Result<Self> {
		let model = request
			.model
			.clone()
			.ok_or_else(|| Error::new("no model: pass --model or set defaultModel in config"))?;
		let provider = request
			.provider
			.clone()
			.or_else(|| request.catalog.provider_of(&model).map(str::to_string))
			.ok_or_else(|| {
				Error::new(format!(
					"no provider for model `{model}`: pass --provider or add the model to models.json"
				))
			})?;
		let base_url = request
			.config
			.providers
			.get(&provider)
			.and_then(|config| config.base_url.clone())
			.ok_or_else(|| Error::new(format!("provider `{provider}` has no baseUrl in config")))?;
		let api_key = request
			.api_key
			.clone()
			.or_else(|| request.config.resolve_api_key(&provider))
			.ok_or_else(|| {
				Error::new(format!(
					"provider `{provider}` has no apiKey in config and --api-key was not passed"
				))
			})?;
		let model = model
			.split_once('/')
			.filter(|(prefix, _)| *prefix == provider.as_str())
			.map_or_else(|| model.clone(), |(_, id)| id.to_string());
		let context_limit = request
			.catalog
			.context_window(&provider, &model)
			.unwrap_or(DEFAULT_CONTEXT_LIMIT);
		Ok(Self {
			base_url,
			api_key,
			model,
			max_turns: request.max_turns.max(1),
			context_limit,
			system_prompt: compose_system_prompt(request),
			tools: allowed_tools(request)?,
		})
	}
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

fn build_registry(tools: &[&'static str]) -> ToolRegistry {
	let mut registry = ToolRegistry::new();
	for name in tools {
		let tool: Arc<dyn Tool> = match *name {
			"read" => Arc::new(ReadTool),
			"glob" => Arc::new(GlobTool),
			"grep" => Arc::new(GrepTool),
			"write" => Arc::new(WriteTool),
			"bash" => Arc::new(BashTool),
			"todo" => Arc::new(TodoTool::new()),
			_ => continue,
		};
		registry.register(tool);
	}
	registry
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
			events.push(Event::ToolStart { tool: call.name.clone(), args: call.arguments.clone() });
		}
		for result in &turn.tool_results {
			let tool = tool_names
				.get(&result.tool_call_id)
				.cloned()
				.unwrap_or_else(|| "tool".to_string());
			let error = result.is_error.then(|| result.content.clone());
			events.push(Event::ToolEnd { tool, error });
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
		Catalog { providers, source: PathBuf::from("test-models.json") }
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
		assert_eq!(plan.tools, ["bash", "glob", "grep", "read", "todo", "write"]);
		let system = plan.system_prompt.expect("system prompt");
		for part in ["base prompt", "appended", "repo rules", "skill body"] {
			assert!(system.contains(part), "system prompt is missing {part}");
		}
	}

	#[test]
	fn plan_requires_base_url_and_api_key() {
		let mut missing_url = request(yolo());
		missing_url.config.providers.clear();
		let err = RunPlan::resolve(&missing_url).err().expect("no baseUrl");
		assert!(err.to_string().contains("has no baseUrl"));

		let mut missing_key = request(yolo());
		if let Some(provider) = missing_key.config.providers.get_mut("local") {
			provider.api_key = None;
		}
		let err = RunPlan::resolve(&missing_key).err().expect("no apiKey");
		assert!(err.to_string().contains("has no apiKey"));

		let mut no_model = request(yolo());
		no_model.model = None;
		assert!(RunPlan::resolve(&no_model).is_err());
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
			Event::ToolEnd { tool, error } => Some((tool.as_str(), error.clone())),
			_ => None,
		});
		assert_eq!(tool_error, Some(("read", Some("boom".to_string()))));
		assert!(matches!(
			events.last(),
			Some(Event::AgentEnd { text }) if text == "done"
		));
	}
}
