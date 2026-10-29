use std::{collections::BTreeMap, sync::Arc};

use serde_json::Value;

use super::{Tool, builtin_tools};

/// Registry storing available tools for model interaction.
///
/// Ordered (`BTreeMap`) so the schema list handed to the provider — part of the
/// prompt prefix — is byte-stable across runs and stays cacheable.
#[derive(Clone, Default)]
pub struct ToolRegistry {
	tools: BTreeMap<String, Arc<dyn Tool>>,
	server_errors: BTreeMap<String, String>,
	configured_servers: Vec<String>,
	/// Session-owned staged-preview policy. Registries are rebuilt per prompt,
	/// so the loop reads the policy from here and the owner keeps the state.
	policy: Option<Arc<dyn crate::state::ToolPolicy>>,
}

impl ToolRegistry {
	/// Create an empty registry.
	pub fn new() -> Self {
		Self::default()
	}

	/// Create a registry preloaded with the standard builtins.
	pub fn with_builtins() -> Self {
		let mut registry = Self::new();
		for tool in builtin_tools() {
			registry.register(tool);
		}
		registry
	}

	/// Register (or replace) a tool.
	pub fn register(&mut self, tool: Arc<dyn Tool>) {
		self.tools.insert(tool.name().to_string(), tool);
	}

	/// Unregister a tool by name, if present.
	pub fn remove(&mut self, name: &str) {
		self.tools.remove(name);
	}

	/// Mount the session's staged-preview policy on this registry.
	pub fn set_policy(&mut self, policy: Arc<dyn crate::state::ToolPolicy>) {
		self.policy = Some(policy);
	}

	/// Session policy consulted by the loop, when one is mounted.
	pub fn policy(&self) -> Option<Arc<dyn crate::state::ToolPolicy>> {
		self.policy.clone()
	}

	pub fn register_server_error(&mut self, server: impl Into<String>, error: impl Into<String>) {
		self.server_errors.insert(server.into(), error.into());
	}
	pub fn set_configured_servers(&mut self, servers: Vec<String>) {
		self.configured_servers = servers;
	}

	pub fn missing_tool_diagnostic(&self, name: &str) -> String {
		if !name.starts_with("mcp__") {
			return format!("Tool not found: {name}");
		}

		let mut all_servers = self.configured_servers.clone();
		for s in self.server_errors.keys() {
			if !all_servers.contains(s) {
				all_servers.push(s.clone());
			}
		}

		let matching: Vec<&str> = all_servers
			.iter()
			.filter(|s| crate::mcp::naming::mcp_tool_match_score(s, name) > 0)
			.map(String::as_str)
			.collect();

		if matching.len() == 1 {
			let server = matching[0];
			if let Some(err) = self.server_errors.get(server) {
				return format!("MCP tool unavailable: {err}; tool existence not confirmed");
			}
			format!("Tool not found: {name}")
		} else {
			// Ambiguous (>= 2) or no matches (0) remain unknown
			format!("Tool not found: {name}")
		}
	}

	/// Look up a tool by registry name or model-facing wire name.
	///
	/// A wire name that matches more than one tool is rejected (no arbitrary
	/// winner): ambiguous model-facing names must never route silently.
	pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
		if let Some(tool) = self.tools.get(name) {
			return Some(tool.clone());
		}
		let mut matches = self
			.tools
			.values()
			.filter(|tool| tool.wire_name() == Some(name));
		let tool = matches.next()?.clone();
		if matches.next().is_some() {
			return None;
		}
		Some(tool)
	}

	/// Registry keys (internal tool names) in stable order.
	pub fn names(&self) -> Vec<String> {
		self.tools.keys().cloned().collect()
	}

	/// Tool schemas in OpenAI `tools` format.
	pub fn schemas(&self) -> Vec<Value> {
		self
			.tools
			.values()
			.map(|tool| {
				serde_json::json!({
					"type": "function",
					"function": {
						"name": tool.wire_name().unwrap_or_else(|| tool.name()),
						"description": tool.description(),
						"parameters": tool.schema()
					}
				})
			})
			.collect()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_missing_tool_diagnostic_builtins() {
		let registry = ToolRegistry::new();
		assert_eq!(
			registry.missing_tool_diagnostic("nonexistent"),
			"Tool not found: nonexistent"
		);
		assert_eq!(
			registry.missing_tool_diagnostic("mcp__random_tool"),
			"Tool not found: mcp__random_tool"
		);
	}

	#[test]
	fn test_missing_tool_diagnostic_single_server_denied() {
		let mut registry = ToolRegistry::new();
		registry.set_configured_servers(vec!["mock".to_string()]);
		registry.register_server_error(
			"mock",
			"MCP server \"mock\" was not launched: stdio launch was not approved",
		);

		assert_eq!(
			registry.missing_tool_diagnostic("mcp__mock_danger_tool"),
			"MCP tool unavailable: MCP server \"mock\" was not launched: stdio launch was not approved; tool existence not confirmed"
		);
		assert_eq!(
			registry.missing_tool_diagnostic("mcp__unknown_tool"),
			"Tool not found: mcp__unknown_tool"
		);
	}

	#[test]
	fn test_missing_tool_diagnostic_ambiguous_servers() {
		let mut registry = ToolRegistry::new();
		registry.set_configured_servers(vec!["foo".to_string(), "foo_bar".to_string()]);
		registry.register_server_error("foo", "MCP server \"foo\" was not launched");

		// mcp__foo_bar_echo matches both foo and foo_bar -> ambiguous, remains unknown
		assert_eq!(
			registry.missing_tool_diagnostic("mcp__foo_bar_echo"),
			"Tool not found: mcp__foo_bar_echo"
		);
	}

	#[test]
	fn test_missing_tool_diagnostic_redacts_raw_error() {
		let mut registry = ToolRegistry::new();
		registry.set_configured_servers(vec!["remote".to_string()]);
		// Pre-sanitized error pushed by runtime:
		registry.register_server_error(
			"remote",
			"Failed to connect to MCP server \"remote\": connection failed",
		);

		let diag = registry.missing_tool_diagnostic("mcp__remote_fetch");
		assert!(
			!diag.contains("VERY_SECRET_KEY_123"),
			"raw error must not leak into diagnostic"
		);
		assert_eq!(
			diag,
			"MCP tool unavailable: Failed to connect to MCP server \"remote\": connection failed; tool existence not confirmed"
		);
	}
}
