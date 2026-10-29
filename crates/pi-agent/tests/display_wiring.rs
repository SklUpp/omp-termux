//! Consumer-level regressions for the display/`edit.mode` wiring: advertised
//! wire name, real dispatch through the registry, ambiguity rejection, and
//! per-child model + no-edit suppression.

use std::sync::{
	Arc, Mutex,
	atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use pi_agent::{
	AgentContext, DelegationLimits, EditTool, Supervisor, Tool, ToolError, ToolRegistry,
	tools::{
		edit_variants::{
			EditMode, ResolvedSessionTools, SessionDisplayResolver, default_session_tools,
			normalize_edit_mode,
		},
		hashline::{SnapshotStore, edit::EditToolOptions},
		read::FileDisplayInputs,
	},
};
use serde_json::{Value, json};

fn temp_dir(label: &str) -> std::path::PathBuf {
	static COUNTER: AtomicU64 = AtomicU64::new(0);
	let dir = std::env::temp_dir().join(format!(
		"pi-agent-display-wiring-{label}-{}-{}",
		std::process::id(),
		COUNTER.fetch_add(1, Ordering::SeqCst)
	));
	std::fs::create_dir_all(&dir).expect("mkdir");
	dir
}

struct AliasTool {
	name: &'static str,
	wire: &'static str,
}

#[async_trait]
impl Tool for AliasTool {
	fn name(&self) -> &str {
		self.name
	}

	fn wire_name(&self) -> Option<&str> {
		Some(self.wire)
	}

	fn description(&self) -> &str {
		"test alias"
	}

	fn schema(&self) -> Value {
		json!({ "type": "object" })
	}

	async fn execute(
		&self,
		_args: &Value,
		_ctx: &AgentContext,
	) -> Result<pi_agent::ToolResult, ToolError> {
		Err(ToolError::ExecutionFailed("not executed".to_string()))
	}
}

#[tokio::test]
async fn apply_patch_is_advertised_and_routed_through_the_registry() {
	let dir = temp_dir("wire");
	let mut registry = ToolRegistry::new();
	let edit = Arc::new(EditTool::with_mode(
		Arc::new(SnapshotStore::new()),
		EditToolOptions::default(),
		EditMode::ApplyPatch,
	));
	registry.register(edit);

	let schemas = registry.schemas();
	assert_eq!(schemas[0]["function"]["name"], "apply_patch");
	assert!(registry.get("edit").is_some(), "internal key stays `edit`");
	let routed = registry.get("apply_patch").expect("wire alias resolves");
	assert_eq!(routed.name(), "edit");

	let ctx = AgentContext::new(dir.clone());
	let payload = "*** Begin Patch\n*** Add File: added.txt\n+hello\n*** End Patch";
	let tool = registry.get("apply_patch").expect("alias");
	let result = tool
		.execute(&json!({ "input": payload }), &ctx)
		.await
		.expect("dispatch through alias executes apply_patch");
	assert!(!result.is_error, "{}", result.content);
	assert_eq!(
		std::fs::read_to_string(dir.join("added.txt")).expect("created file"),
		"hello\n"
	);
}

#[test]
fn ambiguous_wire_names_are_rejected_not_arbitrated() {
	let mut registry = ToolRegistry::new();
	registry.register(Arc::new(AliasTool { name: "alpha", wire: "shared" }));
	registry.register(Arc::new(AliasTool { name: "beta", wire: "shared" }));
	assert!(registry.get("alpha").is_some());
	assert!(
		registry.get("shared").is_none(),
		"two tools sharing a wire name must not resolve arbitrarily"
	);
}

#[derive(Default)]
struct RecordingResolver {
	seen: Mutex<Vec<(Option<String>, bool)>>,
}

impl SessionDisplayResolver for RecordingResolver {
	fn resolve(&self, active_model: Option<&str>, has_edit_tool: bool) -> ResolvedSessionTools {
		self.seen.lock().expect("seen").push((
			active_model.map(str::to_string),
			has_edit_tool,
		));
		ResolvedSessionTools {
			display:   FileDisplayInputs::new(EditMode::Hashline, has_edit_tool, false),
			edit_tool: normalize_edit_mode("hashline").expect("hashline"),
		}
	}
}

#[tokio::test]
async fn children_resolve_their_own_model_and_drop_anchors_without_edit() {
	let supervisor = Supervisor::with_selection(
		ToolRegistry::with_builtins(),
		Arc::new(
			pi_agent::OpenAiCompatibleProvider::new("http://127.0.0.1:9", "test-key", "test-model")
				.expect("provider"),
		),
		None,
		DelegationLimits::default(),
		true,
		false,
	);
	let resolver = Arc::new(RecordingResolver::default());
	supervisor.set_display_resolver(resolver.clone());
	supervisor.set_active_model("local/root-model");

	// Direct child: no explicit model means the root's actual `provider/id`.
	let mut child = supervisor.child_registry(0, None);
	assert_eq!(
		resolver.seen.lock().expect("seen").last().cloned(),
		Some((Some("local/root-model".to_string()), true)),
		"inherited parent model must not be dropped"
	);
	assert!(child.get("read").is_some());

	// Agent allowlist removes `edit`; rebinding with the child model suppresses
	// hashline display and leaves the read in plain mode (no anchors).
	child.remove("edit");
	supervisor.rebind_session_tools(&mut child, Some("local/child-model"));
	assert_eq!(
		resolver.seen.lock().expect("seen").last().cloned(),
		Some((Some("local/child-model".to_string()), false))
	);

	let dir = temp_dir("no-edit");
	std::fs::write(dir.join("a.txt"), "alpha\nbeta\n").expect("fixture");
	let ctx = AgentContext::new(dir.clone());
	let read = child.get("read").expect("read present");
	let result = read
		.execute(&json!({ "path": "a.txt" }), &ctx)
		.await
		.expect("read executes");
	assert!(!result.is_error, "{}", result.content);
	assert!(
		!result.content.contains('#'),
		"no hashline tag without edit: {}",
		result.content
	);
	assert!(
		!result.content.contains("1|") && !result.content.contains("1:"),
		"no plain line numbers by default: {}",
		result.content
	);
}

#[test]
fn default_builtin_context_keeps_hashline_and_edit() {
	let resolved = default_session_tools(true);
	assert_eq!(resolved.edit_tool, EditMode::Hashline);
	assert!(resolved.display.has_edit_tool);
	assert!(!resolved.display.read_line_numbers);
	let registry = ToolRegistry::with_builtins();
	assert!(registry.get("read").is_some());
	assert!(registry.get("edit").is_some());
}
