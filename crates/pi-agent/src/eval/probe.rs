//! Bounded, detached runtime-availability probes.
//!
//! A probe never inherits host stdio, always has a wall-clock bound, and honours
//! the caller's cancellation. Candidates probed for one backend share a single
//! discovery deadline, so N hung interpreters cannot consume N times the bound.
//! Each probe leads its own session/process group; timeout and abort kill the
//! whole tree.

use std::{
	collections::HashMap,
	path::Path,
	time::{Duration, Instant},
};

use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

/// Wall-clock ceiling when the caller supplies no smaller bound.
pub const DEFAULT_PROBE_TIMEOUT_MS: u64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeResult {
	pub exit_code: Option<i32>,
	pub timed_out: bool,
	pub aborted: bool,
}

#[derive(Debug, Clone)]
pub struct ProbeCandidate {
	pub command: Vec<String>,
	pub env: HashMap<String, String>,
	pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateProbe {
	Ok(usize),
	Aborted,
	Failed(Vec<String>),
}

fn bound(timeout_ms: Option<u64>, ceiling_ms: Option<u64>) -> Duration {
	let ceiling = ceiling_ms.unwrap_or(0).max(DEFAULT_PROBE_TIMEOUT_MS);
	let requested = timeout_ms.filter(|value| *value > 0).unwrap_or(ceiling);
	Duration::from_millis(requested.min(ceiling))
}

/// Spawn `command` detached from host stdio and wait for it under `options`.
///
/// Returns `Err` only when the spawn itself fails (e.g. `ENOENT`); callers treat
/// that as an unavailable candidate.
pub async fn run_bounded_probe(
	command: &[String],
	cwd: &Path,
	env: &HashMap<String, String>,
	cancellation: &CancellationToken,
	timeout_ms: Option<u64>,
	ceiling_ms: Option<u64>,
) -> std::io::Result<ProbeResult> {
	let Some(program) = command.first() else {
		return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty probe command"));
	};
	if cancellation.is_cancelled() {
		return Ok(ProbeResult { exit_code: None, timed_out: false, aborted: true });
	}
	let mut process = Command::new(program);
	process
		.args(&command[1..])
		.current_dir(cwd)
		.env_clear()
		.envs(env)
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::null())
		.kill_on_drop(true);
	#[cfg(unix)]
	{
		// Lead a fresh session so the whole tree can be swept without touching
		// the host's terminal foreground group.
		unsafe {
			process.pre_exec(|| {
				libc::setsid();
				Ok(())
			});
		}
	}
	let mut child = process.spawn()?;
	let pid = child.id();
	let delay = bound(timeout_ms, ceiling_ms);
	let waited = tokio::select! {
		() = cancellation.cancelled() => None,
		status = tokio::time::timeout(delay, child.wait()) => Some(status),
	};
	match waited {
		None => {
			kill_tree(&mut child, pid).await;
			Ok(ProbeResult { exit_code: None, timed_out: false, aborted: true })
		},
		Some(Ok(Ok(status))) => Ok(ProbeResult {
			exit_code: status.code(),
			timed_out: false,
			aborted: false,
		}),
		Some(Ok(Err(_))) => {
			// The child was reaped by someone else or the wait failed; treat as
			// an unavailable candidate rather than a hard error.
			Ok(ProbeResult { exit_code: None, timed_out: false, aborted: false })
		},
		Some(Err(_)) => {
			kill_tree(&mut child, pid).await;
			Ok(ProbeResult { exit_code: None, timed_out: true, aborted: false })
		},
	}
}

async fn kill_tree(child: &mut Child, pid: Option<u32>) {
	#[cfg(unix)]
	if let Some(pid) = pid.filter(|pid| *pid > 1) {
		// SAFETY: signalling a process group we created; the negative pid is
		// guarded against the catastrophic 0/1 targets.
		unsafe {
			libc::kill(-(pid as i32), libc::SIGKILL);
		}
	}
	let _ = child.start_kill();
	let _ = child.wait().await;
}

/// Probe ordered candidates under one shared deadline; the first exit 0 wins.
pub async fn probe_candidates(
	candidates: &[ProbeCandidate],
	cwd: &Path,
	cancellation: &CancellationToken,
	timeout_ms: Option<u64>,
) -> CandidateProbe {
	let budget = bound(timeout_ms, None);
	let deadline = Instant::now() + budget;
	let mut failures = Vec::new();
	for (index, candidate) in candidates.iter().enumerate() {
		if cancellation.is_cancelled() {
			return CandidateProbe::Aborted;
		}
		let now = Instant::now();
		if now >= deadline {
			failures.push(format!("{} (probe budget exhausted)", candidate.label));
			break;
		}
		let remaining = deadline.saturating_duration_since(now).as_millis() as u64;
		match run_bounded_probe(
			&candidate.command,
			cwd,
			&candidate.env,
			cancellation,
			Some(remaining.max(1)),
			None,
		)
		.await
		{
			Ok(result) if result.exit_code == Some(0) => return CandidateProbe::Ok(index),
			Ok(result) if result.aborted => return CandidateProbe::Aborted,
			Ok(result) if result.timed_out => {
				failures.push(format!("{} (probe timed out)", candidate.label));
			},
			Ok(result) => {
				let code = result.exit_code.map_or_else(|| "signal".to_string(), |code| code.to_string());
				failures.push(format!("{} (exit code {code})", candidate.label));
			},
			Err(error) => failures.push(format!("{} ({error})", candidate.label)),
		}
	}
	CandidateProbe::Failed(failures)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn timed_out_probe_never_hangs() {
		let cancellation = CancellationToken::new();
		let env = HashMap::new();
		let result = run_bounded_probe(
			&["sh".to_string(), "-c".to_string(), "sleep 30".to_string()],
			Path::new("."),
			&env,
			&cancellation,
			Some(50),
			None,
		)
		.await
		.expect("spawn");
		assert!(result.timed_out);
		assert_eq!(result.exit_code, None);
	}

	#[tokio::test]
	async fn cancelled_probe_reports_abort() {
		let cancellation = CancellationToken::new();
		cancellation.cancel();
		let result = run_bounded_probe(
			&["sh".to_string(), "-c".to_string(), "sleep 30".to_string()],
			Path::new("."),
			&HashMap::new(),
			&cancellation,
			Some(50),
			None,
		)
		.await
		.expect("spawn");
		assert!(result.aborted);
	}
}
