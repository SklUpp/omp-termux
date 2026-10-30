use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fmt;

pub const MCP_PROTOCOL_VERSION: &str = "2025-11-25";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestIdFormat {
	Number,
	String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct MCPStdioServerConfig {
	pub command: String,
	#[serde(default)]
	pub args: Vec<String>,
	#[serde(default)]
	pub env: HashMap<String, String>,
	#[serde(default)]
	pub cwd: Option<String>,
	#[serde(default)]
	pub timeout: Option<u64>,
	#[serde(default, rename = "requestIdFormat")]
	pub request_id_format: Option<RequestIdFormat>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct MCPHttpServerConfig {
	pub url: String,
	#[serde(default)]
	pub headers: HashMap<String, String>,
	#[serde(default)]
	pub timeout: Option<u64>,
	#[serde(default, rename = "requestIdFormat")]
	pub request_id_format: Option<RequestIdFormat>,
	#[serde(default, rename = "headerPolicy")]
	pub header_policy: Option<String>,
	#[serde(default)]
	pub auth: Option<Value>,
	#[serde(default)]
	pub oauth: Option<Value>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct MCPSseServerConfig {
	pub url: String,
	#[serde(default)]
	pub headers: HashMap<String, String>,
	#[serde(default)]
	pub timeout: Option<u64>,
	#[serde(default, rename = "requestIdFormat")]
	pub request_id_format: Option<RequestIdFormat>,
	#[serde(default, rename = "headerPolicy")]
	pub header_policy: Option<String>,
	#[serde(default)]
	pub auth: Option<Value>,
	#[serde(default)]
	pub oauth: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MCPServerConfig {
	#[serde(rename = "stdio")]
	Stdio(MCPStdioServerConfig),
	#[serde(rename = "http")]
	Http(MCPHttpServerConfig),
	#[serde(rename = "sse")]
	Sse(MCPSseServerConfig),
}
fn redact_url(raw: &str) -> String {
	let mut result = raw.to_string();
	if let Some((scheme_prefix, rest)) = result.split_once("://") {
		let scheme_prefix = scheme_prefix.to_string();
		let path_start = rest
			.find('/')
			.or_else(|| rest.find('?'))
			.unwrap_or(rest.len());
		let authority = &rest[..path_start];
		let path_and_beyond = &rest[path_start..];
		if let Some((userinfo, host)) = authority.split_once('@') {
			let redacted_userinfo = if let Some((user, _pass)) = userinfo.split_once(':') {
				format!("{user}:[REDACTED]")
			} else {
				"[REDACTED]".to_string()
			};
			result = format!("{scheme_prefix}://{redacted_userinfo}@{host}{path_and_beyond}");
		}
	}
	if let Some((base, fragment)) = result.split_once('#') {
		let fragment = fragment.to_string();
		if let Some((base_no_query, _query)) = base.split_once('?') {
			format!("{base_no_query}?[REDACTED]#{fragment}")
		} else {
			result
		}
	} else if let Some((base_no_query, _query)) = result.split_once('?') {
		format!("{base_no_query}?[REDACTED]")
	} else {
		result
	}
}

fn redact_map<'a>(map: &'a HashMap<String, String>) -> BTreeMap<&'a str, &'static str> {
	map.keys().map(|k| (k.as_str(), "[REDACTED]")).collect()
}

impl fmt::Debug for MCPStdioServerConfig {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let redacted_env = redact_map(&self.env);
		f.debug_struct("MCPStdioServerConfig")
			.field("command", &self.command)
			.field("args", &self.args)
			.field("env", &redacted_env)
			.field("cwd", &self.cwd)
			.field("timeout", &self.timeout)
			.field("request_id_format", &self.request_id_format)
			.finish()
	}
}

impl fmt::Debug for MCPHttpServerConfig {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let redacted_url = redact_url(&self.url);
		let redacted_headers = redact_map(&self.headers);
		let redacted_auth = self.auth.as_ref().map(|_| "[REDACTED]");
		let redacted_oauth = self.oauth.as_ref().map(|_| "[REDACTED]");
		f.debug_struct("MCPHttpServerConfig")
			.field("url", &redacted_url)
			.field("headers", &redacted_headers)
			.field("timeout", &self.timeout)
			.field("request_id_format", &self.request_id_format)
			.field("header_policy", &self.header_policy)
			.field("auth", &redacted_auth)
			.field("oauth", &redacted_oauth)
			.finish()
	}
}

impl fmt::Debug for MCPSseServerConfig {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let redacted_url = redact_url(&self.url);
		let redacted_headers = redact_map(&self.headers);
		let redacted_auth = self.auth.as_ref().map(|_| "[REDACTED]");
		let redacted_oauth = self.oauth.as_ref().map(|_| "[REDACTED]");
		f.debug_struct("MCPSseServerConfig")
			.field("url", &redacted_url)
			.field("headers", &redacted_headers)
			.field("timeout", &self.timeout)
			.field("request_id_format", &self.request_id_format)
			.field("header_policy", &self.header_policy)
			.field("auth", &redacted_auth)
			.field("oauth", &redacted_oauth)
			.finish()
	}
}

impl MCPServerConfig {
	pub fn timeout(&self) -> Option<u64> {
		match self {
			Self::Stdio(s) => s.timeout,
			Self::Http(h) => h.timeout,
			Self::Sse(s) => s.timeout,
		}
	}

	pub fn request_id_format(&self) -> RequestIdFormat {
		let fmt = match self {
			Self::Stdio(s) => s.request_id_format,
			Self::Http(h) => h.request_id_format,
			Self::Sse(s) => s.request_id_format,
		};
		fmt.unwrap_or(RequestIdFormat::Number)
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
	pub jsonrpc: String,
	pub id: Value,
	pub method: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
	pub jsonrpc: String,
	pub id: Value,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub result: Option<Value>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
	pub jsonrpc: String,
	pub method: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
	pub code: i64,
	pub message: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPClientInfo {
	pub name: String,
	pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPRootsCapability {
	#[serde(rename = "listChanged", default)]
	pub list_changed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MCPClientCapabilities {
	#[serde(default)]
	pub roots: Option<MCPRootsCapability>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPInitializeParams {
	#[serde(rename = "protocolVersion")]
	pub protocol_version: String,
	pub capabilities: MCPClientCapabilities,
	#[serde(rename = "clientInfo")]
	pub client_info: MCPClientInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPServerInfo {
	pub name: String,
	#[serde(default)]
	pub version: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MCPServerCapabilities {
	#[serde(default)]
	pub tools: Option<Value>,
	#[serde(default)]
	pub resources: Option<Value>,
	#[serde(default)]
	pub logging: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPInitializeResult {
	#[serde(rename = "protocolVersion")]
	pub protocol_version: String,
	pub capabilities: MCPServerCapabilities,
	#[serde(rename = "serverInfo")]
	pub server_info: MCPServerInfo,
	#[serde(default)]
	pub instructions: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPToolDefinition {
	pub name: String,
	#[serde(default)]
	pub description: Option<String>,
	#[serde(rename = "inputSchema", default)]
	pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPToolsListResult {
	#[serde(default)]
	pub tools: Vec<MCPToolDefinition>,
	#[serde(rename = "nextCursor", default)]
	pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MCPContent {
	#[serde(rename = "text")]
	Text { text: String },
	#[serde(rename = "image")]
	Image {
		data: String,
		#[serde(rename = "mimeType")]
		mime_type: String,
	},
	#[serde(rename = "resource")]
	Resource {
		uri: String,
		#[serde(default)]
		text: Option<String>,
		#[serde(default)]
		blob: Option<String>,
	},
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPToolCallResult {
	#[serde(default)]
	pub content: Vec<MCPContent>,
	#[serde(rename = "isError", default)]
	pub is_error: Option<bool>,
	#[serde(rename = "_meta", default)]
	pub _meta: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MCPAnnotationAudience {
	User,
	Assistant,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MCPAnnotations {
	#[serde(default)]
	pub audience: Option<Vec<MCPAnnotationAudience>>,
	#[serde(default)]
	pub priority: Option<f64>,
	#[serde(rename = "lastModified", default)]
	pub last_modified: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPResource {
	pub uri: String,
	pub name: String,
	#[serde(default)]
	pub title: Option<String>,
	#[serde(default)]
	pub description: Option<String>,
	#[serde(rename = "mimeType", default)]
	pub mime_type: Option<String>,
	#[serde(default)]
	pub size: Option<u64>,
	#[serde(default)]
	pub annotations: Option<MCPAnnotations>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPResourceTemplate {
	#[serde(rename = "uriTemplate")]
	pub uri_template: String,
	pub name: String,
	#[serde(default)]
	pub title: Option<String>,
	#[serde(default)]
	pub description: Option<String>,
	#[serde(rename = "mimeType", default)]
	pub mime_type: Option<String>,
	#[serde(default)]
	pub annotations: Option<MCPAnnotations>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MCPResourcesListResult {
	#[serde(default)]
	pub resources: Vec<MCPResource>,
	#[serde(rename = "nextCursor", default)]
	pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MCPResourceTemplatesListResult {
	#[serde(rename = "resourceTemplates", default)]
	pub resource_templates: Vec<MCPResourceTemplate>,
	#[serde(rename = "nextCursor", default)]
	pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPResourceContentItem {
	pub uri: String,
	#[serde(rename = "mimeType", default)]
	pub mime_type: Option<String>,
	#[serde(default)]
	pub text: Option<String>,
	#[serde(default)]
	pub blob: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MCPResourceReadResult {
	#[serde(default)]
	pub contents: Vec<MCPResourceContentItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPResourceReadParams {
	pub uri: String,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_redact_url() {
		assert_eq!(
			redact_url("https://user:password@example.com/mcp?token=secret123#frag"),
			"https://user:[REDACTED]@example.com/mcp?[REDACTED]#frag"
		);
		assert_eq!(
			redact_url("https://example.com/sse?key=12345"),
			"https://example.com/sse?[REDACTED]"
		);
		assert_eq!(redact_url("http://localhost:8080/mcp"), "http://localhost:8080/mcp");
	}

	#[test]
	fn test_config_debug_redaction() {
		let mut env = HashMap::new();
		env.insert("TOKEN".into(), "secret_val".into());
		let stdio = MCPStdioServerConfig {
			command: "cmd".into(),
			args: vec![],
			env,
			cwd: None,
			timeout: None,
			request_id_format: None,
		};
		let s = format!("{stdio:?}");
		assert!(!s.contains("secret_val"));
		assert!(s.contains("[REDACTED]"));

		let mut headers = HashMap::new();
		headers.insert("Authorization".into(), "Bearer secret_bearer".into());
		let http = MCPHttpServerConfig {
			url: "https://user:pass@example.com/mcp?secret_q=1".into(),
			headers,
			timeout: None,
			request_id_format: None,
			header_policy: None,
			auth: Some(serde_json::json!({"auth_key": "secret_auth"})),
			oauth: Some(serde_json::json!({"oauth_key": "secret_oauth"})),
		};
		let h = format!("{http:?}");
		assert!(!h.contains("pass"));
		assert!(!h.contains("secret_q"));
		assert!(!h.contains("secret_bearer"));
		assert!(!h.contains("secret_auth"));
		assert!(!h.contains("secret_oauth"));
		assert!(h.contains("[REDACTED]"));
	}
}
