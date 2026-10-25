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

	/// Look up a tool by name.
	pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
		self.tools.get(name).cloned()
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
						"name": tool.name(),
						"description": tool.description(),
						"parameters": tool.schema()
					}
				})
			})
			.collect()
	}
}
