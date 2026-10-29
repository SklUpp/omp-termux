//! LSP protocol payloads and server configuration types shared by the native
//! client, operation layer, and tool adapter.
//!
//! Port of `packages/coding-agent/src/lsp/types.ts`. Fields keep the protocol's
//! camelCase wire spelling via serde renames; unknown server fields are ignored
//! so a newer LSP server cannot break the client.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// =============================================================================
// Core protocol types
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Position {
	pub line: u32,
	pub character: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Range {
	pub start: Position,
	pub end: Position,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
	pub uri: String,
	pub range: Range,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocationLink {
	pub origin_selection_range: Option<Range>,
	pub target_uri: String,
	pub target_range: Range,
	pub target_selection_range: Range,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkupContent {
	pub kind: String,
	pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hover {
	pub contents: Value,
	pub range: Option<Range>,
}

// =============================================================================
// Diagnostics
// =============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
	pub range: Range,
	pub severity: Option<u8>,
	pub code: Option<Value>,
	pub source: Option<String>,
	pub message: String,
	pub tags: Option<Vec<u8>>,
	pub related_information: Option<Value>,
	pub data: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct PublishedDiagnostics {
	pub diagnostics: Vec<Diagnostic>,
	pub version: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishDiagnosticsParams {
	pub uri: String,
	pub diagnostics: Vec<Diagnostic>,
	pub version: Option<i64>,
}

// =============================================================================
// Text edits and workspace edits
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextEdit {
	pub range: Range,
	pub new_text: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub insert_text_format: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentIdentifier {
	pub uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptionalVersionedTextDocumentIdentifier {
	pub uri: String,
	pub version: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentEdit {
	pub text_document: OptionalVersionedTextDocumentIdentifier,
	pub edits: Vec<TextEdit>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CreateFileOptions {
	#[serde(default)]
	pub overwrite: Option<bool>,
	#[serde(default)]
	pub ignore_if_exists: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RenameFileOptions {
	#[serde(default)]
	pub overwrite: Option<bool>,
	#[serde(default)]
	pub ignore_if_exists: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DeleteFileOptions {
	#[serde(default)]
	pub recursive: Option<bool>,
	#[serde(default)]
	pub ignore_if_not_exists: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ResourceOperation {
	Create {
		uri: String,
		#[serde(default)]
		options: CreateFileOptions,
	},
	Rename {
		#[serde(rename = "oldUri")]
		old_uri: String,
		#[serde(rename = "newUri")]
		new_uri: String,
		#[serde(default)]
		options: RenameFileOptions,
	},
	Delete {
		uri: String,
		#[serde(default)]
		options: DeleteFileOptions,
	},
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum WorkspaceDocumentChange {
	Text(TextDocumentEdit),
	Resource(ResourceOperation),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceEdit {
	#[serde(default)]
	pub changes: Option<BTreeMap<String, Vec<TextEdit>>>,
	#[serde(default)]
	pub document_changes: Option<Vec<WorkspaceDocumentChange>>,
}

impl WorkspaceEdit {
	#[must_use]
	pub fn is_empty(&self) -> bool {
		self.changes.as_ref().is_none_or(BTreeMap::is_empty)
			&& self.document_changes.as_ref().is_none_or(Vec::is_empty)
	}
}

// =============================================================================
// Code actions
// =============================================================================

#[derive(Debug, Clone, Deserialize)]
pub struct Command {
	pub title: String,
	pub command: String,
	#[serde(default)]
	pub arguments: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeActionDisabled {
	pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeAction {
	pub title: String,
	#[serde(default)]
	pub kind: Option<String>,
	#[serde(default)]
	pub diagnostics: Option<Vec<Diagnostic>>,
	#[serde(default)]
	pub is_preferred: Option<bool>,
	#[serde(default)]
	pub disabled: Option<CodeActionDisabled>,
	#[serde(default)]
	pub edit: Option<WorkspaceEdit>,
	#[serde(default)]
	pub command: Option<Command>,
	#[serde(default)]
	pub data: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum CodeActionOrCommand {
	Command(Command),
	Action(CodeAction),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeActionContext {
	pub diagnostics: Vec<Diagnostic>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub only: Option<Vec<String>>,
	pub trigger_kind: u8,
}

// =============================================================================
// Symbols
// =============================================================================

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSymbol {
	pub name: String,
	#[serde(default)]
	pub detail: Option<String>,
	pub kind: u8,
	pub range: Range,
	pub selection_range: Range,
	#[serde(default)]
	pub children: Option<Vec<DocumentSymbol>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolInformation {
	pub name: String,
	pub kind: u8,
	pub location: Location,
	#[serde(default)]
	pub container_name: Option<String>,
}

// =============================================================================
// JSON-RPC envelopes
// =============================================================================

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcRequest {
	pub jsonrpc: &'static str,
	pub id: i64,
	pub method: String,
	pub params: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcNotification {
	pub jsonrpc: &'static str,
	pub method: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
	pub jsonrpc: &'static str,
	pub id: Value,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub result: Option<Value>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
	pub code: i64,
	pub message: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub data: Option<Value>,
}

// =============================================================================
// Server configuration
// =============================================================================

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
	#[serde(default)]
	pub flycheck: Option<bool>,
	#[serde(default)]
	pub ssr: Option<bool>,
	#[serde(default)]
	pub expand_macro: Option<bool>,
	#[serde(default)]
	pub runnables: Option<bool>,
	#[serde(default)]
	pub related_tests: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReadyTimings {
	#[serde(default)]
	pub timeout_ms: Option<u64>,
	#[serde(default)]
	pub poll_ms: Option<u64>,
	#[serde(default)]
	pub settle_ms: Option<u64>,
	#[serde(default)]
	pub status_request_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomLinter {
	Biome,
	SwiftLint,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
	pub command: String,
	#[serde(default)]
	pub args: Vec<String>,
	pub file_types: Vec<String>,
	#[serde(default)]
	pub language_id: Option<String>,
	pub root_markers: Vec<String>,
	#[serde(default)]
	pub init_options: Option<Value>,
	#[serde(default)]
	pub settings: Option<Value>,
	#[serde(default)]
	pub disabled: Option<bool>,
	#[serde(default)]
	pub warmup_timeout_ms: Option<u64>,
	#[serde(default)]
	pub workspace_ready_timings: Option<WorkspaceReadyTimings>,
	#[serde(default)]
	pub capabilities: Option<ServerCapabilities>,
	#[serde(default)]
	pub is_linter: Option<bool>,
	/// Runtime-only: absolute path resolved during config loading. Never read
	/// from user configuration.
	#[serde(skip)]
	pub resolved_command: Option<PathBuf>,
	/// Runtime-only: CLI linter adapter selected by the configuration entry
	/// name (`biome` or `swiftlint`), matching upstream runtime defaults.
	#[serde(skip)]
	pub custom_linter: Option<CustomLinter>,
}

impl ServerConfig {
	#[must_use]
	pub fn is_custom_linter(&self) -> bool {
		self.custom_linter.is_some()
	}

	#[must_use]
	pub fn is_project_aware(&self) -> bool {
		!self.is_custom_linter() && !self.is_linter.unwrap_or(false)
	}

	#[must_use]
	pub fn capabilities_flag(&self, pick: impl Fn(&ServerCapabilities) -> Option<bool>) -> bool {
		self.capabilities.as_ref().and_then(pick).unwrap_or(false)
	}
}

#[derive(Debug, Clone, Default)]
pub struct LspConfig {
	servers: Vec<(String, ServerConfig)>,
	pub idle_timeout_ms: Option<u64>,
}

impl LspConfig {
	#[must_use]
	pub fn new(servers: Vec<(String, ServerConfig)>, idle_timeout_ms: Option<u64>) -> Self {
		Self { servers, idle_timeout_ms }
	}

	#[must_use]
	pub fn servers(&self) -> &[(String, ServerConfig)] {
		&self.servers
	}

	#[must_use]
	pub fn get(&self, name: &str) -> Option<&ServerConfig> {
		self.servers.iter().find(|(n, _)| n == name).map(|(_, c)| c)
	}

	#[must_use]
	pub fn names(&self) -> Vec<&str> {
		self.servers.iter().map(|(n, _)| n.as_str()).collect()
	}

	#[must_use]
	pub fn is_empty(&self) -> bool {
		self.servers.is_empty()
	}
}

// =============================================================================
// Tool parameters
// =============================================================================

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LspParams {
	pub action: String,
	#[serde(default)]
	pub file: Option<String>,
	#[serde(default)]
	pub line: Option<u32>,
	#[serde(default)]
	pub symbol: Option<String>,
	#[serde(default)]
	pub query: Option<String>,
	#[serde(default)]
	pub new_name: Option<String>,
	#[serde(default)]
	pub apply: Option<bool>,
	#[serde(default)]
	pub timeout: Option<f64>,
	#[serde(default)]
	pub payload: Option<String>,
}

/// Actions that never mutate the workspace or language-server state.
pub const LSP_READONLY_ACTIONS: &[&str] = &[
	"diagnostics",
	"definition",
	"type_definition",
	"implementation",
	"references",
	"hover",
	"symbols",
	"status",
	"capabilities",
];

pub const LSP_ACTIONS: &[&str] = &[
	"diagnostics",
	"definition",
	"references",
	"hover",
	"symbols",
	"rename",
	"rename_file",
	"code_actions",
	"format",
	"type_definition",
	"implementation",
	"status",
	"reload",
	"capabilities",
	"request",
];

#[must_use]
pub fn is_readonly_action(action: &str) -> bool {
	LSP_READONLY_ACTIONS.contains(&action)
}
