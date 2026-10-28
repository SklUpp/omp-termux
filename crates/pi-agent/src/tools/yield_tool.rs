use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolError};
use crate::state::{AgentContext, ToolResult};

pub struct YieldTool;

impl YieldTool {
	pub fn new() -> Self {
		Self
	}
}

impl Default for YieldTool {
	fn default() -> Self {
		Self::new()
	}
}

#[async_trait]
impl Tool for YieldTool {
	fn name(&self) -> &str {
		"yield"
	}

	fn description(&self) -> &str {
		"Submit final or incremental results back to the parent agent."
	}

	fn schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"type": {
					"description": "Yield type: string or omitted for terminal completion; array of strings for incremental findings.",
					"oneOf": [
						{ "type": "string" },
						{ "type": "array", "items": { "type": "string" } }
					]
				},
				"result": {
					"type": "object",
					"description": "Result payload: data for success or error for failure.",
					"properties": {
						"data": {},
						"error": { "type": "string" }
					}
				}
			},
			"required": ["result"]
		})
	}

	fn is_exclusive(&self) -> bool {
		false
	}

	async fn execute(&self, args: &Value, _ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let result_val = args.get("result").ok_or_else(|| {
			ToolError::InvalidArguments("Missing required parameter 'result'".to_string())
		})?;

		let result_obj = result_val.as_object().ok_or_else(|| {
			ToolError::InvalidArguments("Parameter 'result' must be an object".to_string())
		})?;

		let data_opt = result_obj.get("data");
		let error_opt = result_obj.get("error");

		if data_opt.is_none() && error_opt.is_none() {
			return Err(ToolError::InvalidArguments(
				"result must contain either 'data' or 'error'".to_string(),
			));
		}

		if data_opt.is_some() && error_opt.is_some() {
			return Err(ToolError::InvalidArguments(
				"result cannot contain both 'data' and 'error'".to_string(),
			));
		}

		let terminate = match args.get("type") {
			None | Some(Value::Null) => true,
			Some(Value::String(_)) => true,
			Some(Value::Array(arr)) => {
				if arr.iter().all(|item| item.is_string()) {
					false
				} else {
					return Err(ToolError::InvalidArguments(
						"items in 'type' array must be strings".to_string(),
					));
				}
			},
			Some(_) => {
				return Err(ToolError::InvalidArguments(
					"'type' must be a string or array of strings".to_string(),
				));
			},
		};

		let status = if error_opt.is_some() { "aborted" } else { "success" };

		let mut payload = serde_json::Map::new();
		if let Some(data) = data_opt {
			payload.insert("data".to_string(), data.clone());
		}
		if let Some(err) = error_opt {
			payload.insert("error".to_string(), err.clone());
		}
		payload.insert("status".to_string(), Value::String(status.to_string()));
		if let Some(t) = args.get("type") {
			if !t.is_null() {
				payload.insert("type".to_string(), t.clone());
			}
		}

		let content = serde_json::to_string(&Value::Object(payload))
			.unwrap_or_else(|_| "{}".to_string());

		Ok(ToolResult {
			tool_call_id: String::new(),
			content,
			is_error: false,
			terminate,
		})
	}
}

#[cfg(test)]
mod tests {
	use std::path::PathBuf;
	use super::*;

	#[tokio::test]
	async fn test_yield_terminal_no_type() {
		let tool = YieldTool::new();
		let ctx = AgentContext::new(PathBuf::from("/tmp"));
		let res = tool
			.execute(&json!({ "result": { "data": "test_data" } }), &ctx)
			.await
			.unwrap();
		assert!(res.terminate);
		assert!(!res.is_error);
		let parsed: Value = serde_json::from_str(&res.content).unwrap();
		assert_eq!(parsed["status"], "success");
		assert_eq!(parsed["data"], "test_data");
	}

	#[tokio::test]
	async fn test_yield_terminal_string_type() {
		let tool = YieldTool::new();
		let ctx = AgentContext::new(PathBuf::from("/tmp"));
		let res = tool
			.execute(
				&json!({ "type": "final", "result": { "data": 42 } }),
				&ctx,
			)
			.await
			.unwrap();
		assert!(res.terminate);
		assert!(!res.is_error);
		let parsed: Value = serde_json::from_str(&res.content).unwrap();
		assert_eq!(parsed["type"], "final");
		assert_eq!(parsed["status"], "success");
		assert_eq!(parsed["data"], 42);
	}

	#[tokio::test]
	async fn test_yield_incremental_array_type() {
		let tool = YieldTool::new();
		let ctx = AgentContext::new(PathBuf::from("/tmp"));
		let res = tool
			.execute(
				&json!({ "type": ["finding_1", "finding_2"], "result": { "data": "part" } }),
				&ctx,
			)
			.await
			.unwrap();
		assert!(!res.terminate);
		assert!(!res.is_error);
	}

	#[tokio::test]
	async fn test_yield_empty_result_fails() {
		let tool = YieldTool::new();
		let ctx = AgentContext::new(PathBuf::from("/tmp"));
		let err = tool.execute(&json!({ "result": {} }), &ctx).await.unwrap_err();
		assert!(matches!(err, ToolError::InvalidArguments(_)));
	}
}
