//! Protocol contract for the SearXNG-backed `web_search` tool.
//!
//! A loopback HTTP server stands in for the configured instance, so every
//! assertion is on consumer-visible tool output, the wire request, or the
//! failure a caller sees — never on source shape or a mock echo.

use std::time::Duration;

use pi_agent::{
	AgentContext, Tool, ToolError,
	tools::{SearxngConfig, WebSearchTool},
};
use serde_json::json;
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::TcpListener,
	sync::mpsc,
};
use tokio_util::sync::CancellationToken;

const CANNED: &str = r#"{
	"query": "cats & dogs",
	"results": [
		{
			"title": "Async Rust",
			"url": "https://example.org/async",
			"content": "  Cancellation is cooperative.  ",
			"publishedDate": "2020-01-01T00:00:00Z"
		},
		{
			"title": "",
			"url": "https://example.org/untitled",
			"snippet": "Fallback title and snippet"
		},
		{
			"title": "Broken row without a url",
			"content": "dropped: no url field"
		}
	],
	"suggestions": ["rust cancellation patterns", "tokio select"],
	"answers": ["Cooperative cancellation via tokens."]
}"#;

fn http_json(body: &str) -> String {
	format!(
		"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: \
		 close\r\n\r\n{body}",
		body.len()
	)
}

/// Spawn a loopback server answering `responses` in order, one connection each.
/// Every received request head is streamed to the caller as soon as it lands, so
/// an assertion never waits for a deliberately stalled connection. `None` keeps
/// that connection open without answering, so a cancelled request never
/// completes.
async fn loopback(
	responses: Vec<Option<String>>,
) -> (String, mpsc::UnboundedReceiver<String>, tokio::task::JoinHandle<()>) {
	let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
	let address = listener.local_addr().expect("loopback address");
	let (capture, captured) = mpsc::unbounded_channel();
	let handle = tokio::spawn(async move {
		for response in responses {
			let Ok((mut socket, _)) = listener.accept().await else {
				break;
			};
			let mut buffer = Vec::new();
			let mut chunk = [0u8; 1024];
			loop {
				match socket.read(&mut chunk).await {
					Ok(0) | Err(_) => break,
					Ok(read) => {
						buffer.extend_from_slice(&chunk[..read]);
						if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
							break;
						}
					},
				}
			}
			let head = String::from_utf8_lossy(&buffer).into_owned();
			let _ = capture.send(head);
			match response {
				Some(response) => {
					let _ = socket.write_all(response.as_bytes()).await;
					let _ = socket.flush().await;
				},
				None => tokio::time::sleep(Duration::from_secs(5)).await,
			}
		}
	});
	(format!("http://{address}"), captured, handle)
}

fn request_target(head: &str) -> String {
	head.lines()
		.next()
		.expect("request line")
		.split_whitespace()
		.nth(1)
		.expect("request target")
		.to_string()
}

fn query_pairs(head: &str) -> Vec<(String, String)> {
	let url = reqwest::Url::parse(&format!("http://localhost{}", request_target(head)))
		.expect("request target parses");
	url.query_pairs().map(|(key, value)| (key.into_owned(), value.into_owned())).collect()
}

fn query_value(pairs: &[(String, String)], key: &str) -> Option<String> {
	pairs
		.iter()
		.find(|(name, _)| name == key)
		.map(|(_, value)| value.clone())
}

fn header(head: &str, name: &str) -> Option<String> {
	head.lines().find_map(|line| {
		let (key, value) = line.split_once(':')?;
		key.trim().eq_ignore_ascii_case(name).then(|| value.trim().to_string())
	})
}

fn configured(endpoint: String) -> SearxngConfig {
	SearxngConfig {
		endpoint: Some(endpoint),
		token: Some("bearer-secret".to_string()),
		basic_username: Some("user".to_string()),
		basic_password: Some("pass-secret".to_string()),
		..SearxngConfig::default()
	}
}

#[tokio::test]
async fn returns_filtered_sources_with_basic_auth_and_search_params() {
	let (endpoint, mut captured, server) = loopback(vec![Some(http_json(CANNED))]).await;
	let tool = WebSearchTool::new(configured(endpoint));
	let context = AgentContext::new(std::env::temp_dir());
	let result = tool
		.execute(
			&json!({ "query": "cats & dogs !!g", "recency": "week", "limit": 3 }),
			&context,
		)
		.await
		.expect("search succeeds");
	let head = captured.recv().await.expect("request captured");
	server.abort();
	assert!(request_target(&head).starts_with("/search?"), "{head}");
	let params = query_pairs(&head);
	assert_eq!(
		query_value(&params, "q").as_deref(),
		Some("cats & dogs"),
		"external bang stripped, query decoded: {params:?}"
	);
	assert_eq!(query_value(&params, "format").as_deref(), Some("json"));
	assert_eq!(query_value(&params, "pageno").as_deref(), Some("1"));
	assert_eq!(
		query_value(&params, "time_range").as_deref(),
		Some("month"),
		"upstream maps week to month"
	);
	assert_eq!(
		header(&head, "authorization").as_deref(),
		Some("Basic dXNlcjpwYXNzLXNlY3JldA=="),
		"Basic credentials take precedence over the bearer token"
	);

	let content = &result.content;
	assert!(content.starts_with("Cooperative cancellation via tokens."), "{content}");
	assert!(content.contains("\n## Sources\n2 sources\n"), "{content}");
	assert!(content.contains("[1] Async Rust ("), "{content}");
	assert!(
		content.contains(" ago)\n    https://example.org/async"),
		"published date renders as a human age: {content}"
	);
	assert!(content.contains("    Cancellation is cooperative."), "{content}");
	assert!(
		content.contains("[2] https://example.org/untitled\n    https://example.org/untitled"),
		"a titleless row falls back to its url: {content}"
	);
	assert!(content.contains("    Fallback title and snippet"), "{content}");
	assert!(!content.contains("Broken row"), "rows without a url are dropped: {content}");
	assert!(content.contains("2 questions"), "{content}");
	assert!(
		!content.contains("bearer-secret") && !content.contains("pass-secret"),
		"credentials never reach the model: {content}"
	);
}

#[tokio::test]
async fn resolves_engine_shortcuts_through_the_config_endpoint() {
	let engines = r#"{"engines":[{"name":"Google","shortcut":"g"},{"name":"DuckDuckGo","shortcut":"ddg"}]}"#;
	let (endpoint, mut captured, server) =
		loopback(vec![Some(http_json(engines)), Some(http_json(CANNED))]).await;
	let tool = WebSearchTool::new(SearxngConfig {
		endpoint: Some(endpoint),
		engines: Some("g,ddg,unknown".to_string()),
		..SearxngConfig::default()
	});
	let context = AgentContext::new(std::env::temp_dir());
	tool.execute(&json!({ "query": "rust" }), &context)
		.await
		.expect("search succeeds after engine resolution");
	let config_head = captured.recv().await.expect("engine config requested");
	let search_head = captured.recv().await.expect("search requested");
	server.abort();
	assert!(
		request_target(&config_head).ends_with("/config"),
		"engine shortcuts resolve through /config: {config_head}"
	);
	assert!(request_target(&search_head).starts_with("/search?"), "{search_head}");
	let params = query_pairs(&search_head);
	assert_eq!(
		query_value(&params, "engines").as_deref(),
		Some("Google,DuckDuckGo,unknown"),
		"shortcuts resolve to canonical names, unknown entries pass through: {params:?}"
	);
}

#[tokio::test]
async fn cancellation_aborts_instead_of_returning_a_partial_answer() {
	let (endpoint, mut captured, server) = loopback(vec![None]).await;
	let tool = WebSearchTool::new(configured(endpoint));
	let token = CancellationToken::new();
	let context = AgentContext::with_cancellation(std::env::temp_dir(), token.clone());
	tokio::spawn(async move {
		tokio::time::sleep(Duration::from_millis(150)).await;
		token.cancel();
	});

	let error = tokio::time::timeout(
		Duration::from_secs(3),
		tool.execute(&json!({ "query": "slow" }), &context),
	)
	.await
	.expect("cancellation settles the request")
	.expect_err("a cancelled search fails");
	let arrived = captured.recv().await.is_some();
	server.abort();
	assert!(matches!(&error, ToolError::Aborted(_)), "{error}");
	assert!(arrived, "the request reached the configured endpoint");
}

#[tokio::test]
async fn missing_endpoint_reports_the_upstream_prerequisite() {
	let tool = WebSearchTool::new(SearxngConfig::default());
	let context = AgentContext::new(std::env::temp_dir());
	let error = tool
		.execute(&json!({ "query": "anything" }), &context)
		.await
		.expect_err("unconfigured search fails");
	assert!(
		format!("{error}").contains(
			"SearXNG endpoint not configured. Set searxng.endpoint in settings or SEARXNG_ENDPOINT \
			 in environment."
		),
		"{error}"
	);
}
