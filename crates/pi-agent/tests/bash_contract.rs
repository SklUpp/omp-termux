//! Consumer-visible contract for `BashTool`: per-call `env`/`cwd`/`timeout`,
//! rejection of run modes the Rust tool does not implement, the output cap, and
//! timeout/cancellation error results.

use std::{
	path::PathBuf,
	sync::atomic::{AtomicUsize, Ordering},
	time::Duration,
};

use pi_agent::{AgentContext, BashTool, CancellationToken, Tool, ToolError};
use serde_json::json;

struct TempDir {
	path: PathBuf,
}

impl TempDir {
	fn new(label: &str) -> Self {
		static NEXT: AtomicUsize = AtomicUsize::new(0);
		let unique =
			format!("pi-agent-{label}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst));
		let path = std::env::temp_dir().join(unique);
		std::fs::create_dir_all(&path).expect("create temp dir");
		Self { path }
	}

	fn join(&self, child: &str) -> PathBuf {
		self.path.join(child)
	}
}

impl Drop for TempDir {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.path);
	}
}

/// First line of a bash result, i.e. the captured output without the status
/// trailer the tool appends.
fn first_line(result: &pi_agent::ToolResult) -> &str {
	result.content.lines().next().unwrap_or("")
}

#[tokio::test]
async fn env_overlay_applies_and_values_stay_literal() {
	let dir = TempDir::new("bash-env");
	let mut ctx = AgentContext::new(dir.path.clone());
	ctx.env.insert("BASH_CONTRACT".to_string(), "base".to_string());

	let result = BashTool
		.execute(
			&json!({
				"command": "echo \"$BASH_CONTRACT\"; echo \"$BASH_LITERAL\"",
				"env": { "BASH_CONTRACT": "overlay", "BASH_LITERAL": "$(id)" }
			}),
			&ctx,
		)
		.await
		.expect("bash");

	assert!(!result.is_error, "got: {}", result.content);
	assert!(result.content.contains("overlay"), "overlay missing: {}", result.content);
	assert!(
		result.content.contains("$(id)"),
		"env value was interpreted as shell code: {}",
		result.content
	);
	assert_eq!(
		ctx.env.get("BASH_CONTRACT").map(String::as_str),
		Some("base"),
		"per-call env must not mutate the agent environment"
	);
}

#[tokio::test]
async fn invalid_env_names_and_non_string_values_are_rejected() {
	let dir = TempDir::new("bash-env-invalid");
	let ctx = AgentContext::new(dir.path.clone());

	let bad_name = BashTool
		.execute(&json!({"command": "true", "env": {"BAD-NAME": "x"}}), &ctx)
		.await
		.expect_err("invalid name must be rejected");
	assert!(matches!(&bad_name, ToolError::InvalidArguments(_)));
	assert!(bad_name.to_string().contains("BAD-NAME"));

	let leading_digit = BashTool
		.execute(&json!({"command": "true", "env": {"1LEAD": "x"}}), &ctx)
		.await
		.expect_err("leading digit must be rejected");
	assert!(leading_digit.to_string().contains("1LEAD"));

	let bad_value = BashTool
		.execute(&json!({"command": "true", "env": {"OK": 1}}), &ctx)
		.await
		.expect_err("non-string value must be rejected");
	assert!(matches!(&bad_value, ToolError::InvalidArguments(_)));

	let bad_shape = BashTool
		.execute(&json!({"command": "true", "env": "FOO=bar"}), &ctx)
		.await
		.expect_err("string env must be rejected");
	assert!(matches!(&bad_shape, ToolError::InvalidArguments(_)));
}

#[tokio::test]
async fn cwd_resolves_relative_absolute_and_tilde() {
	let dir = TempDir::new("bash-cwd");
	let sub = dir.join("sub");
	std::fs::create_dir_all(&sub).expect("mkdir sub");
	let home = dir.join("home");
	let nested = home.join("nested");
	std::fs::create_dir_all(&nested).expect("mkdir nested");

	let mut ctx = AgentContext::new(dir.path.clone());
	ctx.env.insert("HOME".to_string(), home.display().to_string());

	let relative = BashTool
		.execute(&json!({"command": "pwd", "cwd": "sub"}), &ctx)
		.await
		.expect("bash");
	assert!(!relative.is_error, "got: {}", relative.content);
	assert_same_dir(first_line(&relative), &sub);

	let absolute = BashTool
		.execute(&json!({"command": "pwd", "cwd": sub.display().to_string()}), &ctx)
		.await
		.expect("bash");
	assert_same_dir(first_line(&absolute), &sub);

	let tilde = BashTool
		.execute(&json!({"command": "pwd", "cwd": "~/nested"}), &ctx)
		.await
		.expect("bash");
	assert_same_dir(first_line(&tilde), &nested);

	let bare_tilde = BashTool
		.execute(&json!({"command": "pwd", "cwd": "~"}), &ctx)
		.await
		.expect("bash");
	assert_same_dir(first_line(&bare_tilde), &home);
}

fn assert_same_dir(reported: &str, expected: &std::path::Path) {
	let reported = std::path::Path::new(reported.trim());
	let resolved = std::fs::canonicalize(reported).unwrap_or_else(|_| reported.to_path_buf());
	let expected = std::fs::canonicalize(expected).expect("canonical expected dir");
	assert_eq!(resolved, expected, "reported cwd {reported:?}");
}

#[tokio::test]
async fn invalid_cwd_is_an_error_result_and_runs_nothing() {
	let dir = TempDir::new("bash-cwd-invalid");
	let marker = dir.join("marker.txt");
	let ctx = AgentContext::new(dir.path.clone());

	let missing = BashTool
		.execute(
			&json!({
				"command": format!("touch {}", marker.display()),
				"cwd": "/nonexistent/bash-contract-directory"
			}),
			&ctx,
		)
		.await
		.expect("bash");
	assert!(missing.is_error);
	assert!(
		missing.content.contains("Working directory does not exist: /nonexistent/bash-contract-directory"),
		"got: {}",
		missing.content
	);

	let file = dir.join("plain-file");
	std::fs::write(&file, "x").expect("write file");
	let not_a_directory = BashTool
		.execute(
			&json!({
				"command": format!("touch {}", marker.display()),
				"cwd": file.display().to_string()
			}),
			&ctx,
		)
		.await
		.expect("bash");
	assert!(not_a_directory.is_error);
	assert!(
		not_a_directory.content.contains("Working directory is not a directory"),
		"got: {}",
		not_a_directory.content
	);

	let numeric = BashTool
		.execute(&json!({"command": "true", "cwd": 7}), &ctx)
		.await
		.expect_err("non-string cwd must be rejected");
	assert!(matches!(&numeric, ToolError::InvalidArguments(_)));

	assert!(!marker.exists(), "command ran despite an invalid cwd");
}

#[tokio::test]
async fn timeout_zero_disables_the_deadline() {
	let dir = TempDir::new("bash-timeout-zero");
	let ctx = AgentContext::new(dir.path.clone());

	let result = BashTool
		.execute(&json!({"command": "sleep 1.5; echo done", "timeout": 0}), &ctx)
		.await
		.expect("bash");

	assert!(!result.is_error, "timeout 0 must not impose a deadline: {}", result.content);
	assert!(result.content.contains("done"), "got: {}", result.content);
}

#[tokio::test]
async fn small_timeouts_clamp_up_to_one_second() {
	let dir = TempDir::new("bash-timeout-clamp");
	let ctx = AgentContext::new(dir.path.clone());

	// A 1 ms deadline would kill this; the documented floor is one second.
	let result = BashTool
		.execute(&json!({"command": "sleep 0.3; echo clamped", "timeout": 0.001}), &ctx)
		.await
		.expect("bash");

	assert!(!result.is_error, "timeout was not clamped to the 1s floor: {}", result.content);
	assert!(result.content.contains("clamped"), "got: {}", result.content);

	let non_finite = BashTool
		.execute(&json!({"command": "true", "timeout": "soon"}), &ctx)
		.await
		.expect_err("non-numeric timeout must be rejected");
	assert!(matches!(&non_finite, ToolError::InvalidArguments(_)));
}

#[tokio::test]
async fn timeout_kills_a_long_command_and_reports_a_timeout() {
	let dir = TempDir::new("bash-timeout-kill");
	let marker = dir.join("finished.txt");
	let ctx = AgentContext::new(dir.path.clone());

	let result = BashTool
		.execute(
			&json!({
				"command": format!("sleep 5; touch {}", marker.display()),
				"timeout": 1
			}),
			&ctx,
		)
		.await
		.expect("bash");

	assert!(result.is_error);
	assert!(result.content.contains("timed out: true"), "got: {}", result.content);
	assert!(!marker.exists(), "command ran past its timeout");
}

#[tokio::test]
async fn cancellation_aborts_the_run_before_its_deadline() {
	let dir = TempDir::new("bash-cancel");
	let marker = dir.join("finished.txt");
	let token = CancellationToken::new();
	let ctx = AgentContext::with_cancellation(dir.path.clone(), token.clone());

	let args = json!({"command": format!("sleep 5; touch {}", marker.display())});
	let handle = tokio::spawn(async move { BashTool.execute(&args, &ctx).await });
	tokio::time::sleep(Duration::from_millis(200)).await;
	token.cancel();

	let result = tokio::time::timeout(Duration::from_secs(10), handle)
		.await
		.expect("cancelled run must settle")
		.expect("join")
		.expect("bash");

	assert!(result.is_error);
	assert!(result.content.contains("cancelled: true"), "got: {}", result.content);
	assert!(!result.content.contains("timed out: true"), "got: {}", result.content);
	assert!(!marker.exists(), "cancelled command kept running");
}

#[tokio::test]
async fn unsupported_run_modes_are_rejected_not_silently_ignored() {
	let dir = TempDir::new("bash-modes");
	let ctx = AgentContext::new(dir.path.clone());

	let pty = BashTool
		.execute(&json!({"command": "true", "pty": true}), &ctx)
		.await
		.expect_err("pty must be rejected");
	assert!(matches!(&pty, ToolError::InvalidArguments(_)));
	assert!(pty.to_string().contains("pty"));

	let background = BashTool
		.execute(&json!({"command": "true", "async": true}), &ctx)
		.await
		.expect_err("async must be rejected");
	assert!(background.to_string().contains("async"));

	let legacy = BashTool
		.execute(&json!({"command": "true", "timeout_ms": 1000}), &ctx)
		.await
		.expect_err("timeout_ms was removed");
	assert!(matches!(&legacy, ToolError::InvalidArguments(_)));
	assert!(legacy.to_string().contains("timeout"), "got: {legacy}");

	let explicit_off = BashTool
		.execute(&json!({"command": "echo off", "pty": false, "async": false}), &ctx)
		.await
		.expect("explicit false stays accepted");
	assert!(!explicit_off.is_error);
	assert!(explicit_off.content.contains("off"));
}

#[tokio::test]
async fn output_stays_capped_and_the_status_trailer_survives() {
	let dir = TempDir::new("bash-cap");
	let ctx = AgentContext::new(dir.path.clone());

	let result = BashTool
		.execute(
			&json!({"command": "head -c 200000 /dev/zero | tr '\\0' 'a'"}),
			&ctx,
		)
		.await
		.expect("bash");

	assert!(!result.is_error, "got: {}", result.content.len());
	assert!(result.content.contains("output truncated:"), "cap missing: {}", result.content.len());
	assert!(result.content.len() < 100 * 1024, "output escaped the cap: {}", result.content.len());
	assert!(result.content.contains("exit code: 0"), "trailer lost: {}", result.content);
}
