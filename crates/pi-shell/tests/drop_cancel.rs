//! Regression: dropping the caller's shell future must still kill the spawned
//! shell task and its process tree. Previously the outer `select!` was the only
//! owner of cancellation, so dropping it detached the spawned task and left
//! `sleep; touch marker` running.
#![cfg(unix)]

use std::{
	fs,
	path::{Path, PathBuf},
	time::Duration,
};

use pi_shell::{ShellExecuteOptions, cancel::CancelToken, execute_shell};

fn temp_dir() -> PathBuf {
	let dir = std::env::temp_dir().join(format!(
		"pi-shell-drop-{}-{}",
		std::process::id(),
		std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.map(|d| d.as_nanos())
			.unwrap_or_default()
	));
	fs::create_dir_all(&dir).expect("temp dir");
	dir
}

fn pid_alive(pid: u32) -> bool {
	// A killed/reparented child may linger as a zombie; it holds no process
	// tree and can never run its tail, so treat it as gone.
	if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
		let state = stat.rsplit(')').next().and_then(|rest| rest.split_whitespace().next());
		return state != Some("Z");
	}
	std::process::Command::new("kill")
		.arg("-0")
		.arg(pid.to_string())
		.status()
		.is_ok_and(|status| status.success())
}

fn read_pid(path: &Path) -> Option<u32> {
	fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_run_future_kills_the_spawned_process_tree() {
	let dir = temp_dir();
	let pid_file = dir.join("pid.txt");
	let marker = dir.join("finished.txt");
	let command = format!(
		"sh -c 'echo $$ > {}; sleep 1; touch {}'",
		pid_file.display(),
		marker.display()
	);

	let options = ShellExecuteOptions {
		command,
		cwd: Some(dir.to_string_lossy().into_owned()),
		timeout_ms: Some(0),
		..Default::default()
	};
	let handle = tokio::spawn(async move {
		execute_shell(options, None, CancelToken::default()).await
	});

	// Wait until the external shell is actually running and recorded.
	let mut pid = None;
	for _ in 0..40 {
		pid = read_pid(&pid_file);
		if pid.is_some() {
			break;
		}
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
	let pid = pid.expect("external shell must start and record its pid");

	// This is the parent-future drop: no cancellation token is fired, no
	// `select!` branch runs again.
	handle.abort();
	let _ = handle.await;

	let mut gone = false;
	for _ in 0..40 {
		if !pid_alive(pid) {
			gone = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
	assert!(gone, "spawned shell process {pid} survived the caller drop");

	// Past the command's original sleep window: the detached shell must not
	// come back to create its marker.
	tokio::time::sleep(Duration::from_millis(1200)).await;
	assert!(!marker.exists(), "dropped run still executed its tail command");

	fs::remove_dir_all(dir).ok();
}
