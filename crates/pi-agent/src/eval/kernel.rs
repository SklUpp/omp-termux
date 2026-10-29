//! Persistent subprocess kernel: NDJSON framing, cancellation, shutdown.
//!
//! One kernel owns one interpreter process. Runners dup their original stdout
//! for the protocol channel and repoint fd 1/2 at capture pipes, so user output
//! cannot spoof a frame; unparseable lines on the protocol channel are surfaced
//! as cell output instead of being dropped.

use std::{
	collections::HashMap,
	path::{Path, PathBuf},
	process::Stdio,
	sync::{
		Arc, Mutex,
		atomic::{AtomicBool, Ordering},
	},
	time::Duration,
};

use tokio::{
	io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
	process::{Child, ChildStdin, Command},
	sync::{Mutex as AsyncMutex, mpsc},
	task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{
	env::InterpreterRuntime,
	languages,
	output::render_display_bundle,
	types::{
		EvalError, EvalLanguage, KernelError, KernelExecuteOptions, KernelExecuteResult,
		KernelFrame, KernelStatus,
	},
	util,
};

/// Hard cap for one protocol line before it is discarded.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Retained runner stderr for diagnostics.
const STDERR_TAIL_BYTES: usize = 4 * 1024;
#[cfg(unix)]
const TERM_SIGNAL: i32 = libc::SIGTERM;
#[cfg(unix)]
const KILL_SIGNAL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
const TERM_SIGNAL: i32 = 15;
#[cfg(not(unix))]
const KILL_SIGNAL: i32 = 9;

enum KernelEvent {
	Frame(KernelFrame),
	/// Non-protocol line written to the protocol fd by a child. Surfaced as
	/// cell output instead of being silently dropped.
	Raw(String),
	Closed,
}

/// Spawn configuration shared by all languages.
pub struct KernelSpawn {
	pub language: EvalLanguage,
	pub runtime: InterpreterRuntime,
	pub cwd: PathBuf,
	/// Bridge/session env merged over the filtered runtime env.
	pub extra_env: HashMap<String, String>,
}

pub struct Kernel {
	pub language: EvalLanguage,
	pub id: String,
	stdin: AsyncMutex<ChildStdin>,
	child: AsyncMutex<Child>,
	frames: AsyncMutex<mpsc::UnboundedReceiver<KernelEvent>>,
	alive: AtomicBool,
	shutdown_confirmed: AtomicBool,
	execution: AsyncMutex<()>,
	reader: Mutex<Option<JoinHandle<()>>>,
	stderr_drain: Mutex<Option<JoinHandle<()>>>,
	stderr_tail: Arc<Mutex<String>>,
	pid: Option<u32>,
	interrupt_grace: Duration,
}

impl Kernel {
	pub fn is_alive(&self) -> bool {
		self.alive.load(Ordering::SeqCst)
	}

	pub fn pid(&self) -> Option<u32> {
		self.pid
	}

	fn stderr_text(&self) -> String {
		self.stderr_tail.lock().unwrap_or_else(|poison| poison.into_inner()).clone()
	}

	/// Spawn the interpreter, run the language init cell, then its prelude.
	pub async fn start(spawn: KernelSpawn, cancellation: CancellationToken) -> Result<Self, EvalError> {
		let script = languages::stage_runner(spawn.language)?;
		let argv = languages::argv(spawn.language, &spawn.runtime.program, &script);
		let mut env = spawn.runtime.env.clone();
		env.extend(spawn.extra_env.clone());
		match spawn.language {
			EvalLanguage::Py => {
				env.insert("PYTHONUNBUFFERED".to_string(), "1".to_string());
				env.insert("PYTHONIOENCODING".to_string(), "utf-8".to_string());
			},
			EvalLanguage::Jl => {
				env.entry("GKSwstype".to_string()).or_insert_with(|| "100".to_string());
			},
			_ => {},
		}
		let kernel = Self::spawn_process(spawn.language, &argv, &spawn.cwd, env)?;

		let init = languages::init_script(spawn.language, &spawn.cwd);
		kernel.run_startup("init", &init, &cancellation).await?;
		let assets = languages::assets(spawn.language);
		if !assets.prelude.is_empty() {
			kernel
				.run_startup(&format!("{} prelude", spawn.language.display_name()), assets.prelude, &cancellation)
				.await?;
		}
		Ok(kernel)
	}

	/// Wire a child process into a protocol kernel without running init/prelude.
	fn spawn_process(
		language: EvalLanguage,
		argv: &[String],
		cwd: &Path,
		env: HashMap<String, String>,
	) -> Result<Self, EvalError> {
		let Some(program) = argv.first() else {
			return Err(EvalError::Kernel("empty kernel argv".to_string()));
		};
		let mut command = Command::new(program);
		command
			.args(&argv[1..])
			.current_dir(cwd)
			.env_clear()
			.envs(&env)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.kill_on_drop(true);
		#[cfg(unix)]
		{
			// SAFETY: `setsid` in the child before exec; it cannot affect the host.
			unsafe {
				command.pre_exec(|| {
					libc::setsid();
					Ok(())
				});
			}
		}
		let mut child = command.spawn()?;
		let pid = child.id();
		let stdin = child
			.stdin
			.take()
			.ok_or_else(|| EvalError::Kernel("kernel stdin was not captured".to_string()))?;
		let stdout = child
			.stdout
			.take()
			.ok_or_else(|| EvalError::Kernel("kernel stdout was not captured".to_string()))?;
		let stderr = child
			.stderr
			.take()
			.ok_or_else(|| EvalError::Kernel("kernel stderr was not captured".to_string()))?;

		let (frames_tx, frames_rx) = mpsc::unbounded_channel();
		let reader = tokio::spawn(read_frames(stdout, frames_tx));
		let stderr_tail = Arc::new(Mutex::new(String::new()));
		let stderr_drain = tokio::spawn(drain_stderr(stderr, Arc::clone(&stderr_tail)));

		Ok(Self {
			language,
			id: util::random_token(),
			stdin: AsyncMutex::new(stdin),
			child: AsyncMutex::new(child),
			frames: AsyncMutex::new(frames_rx),
			alive: AtomicBool::new(true),
			shutdown_confirmed: AtomicBool::new(false),
			execution: AsyncMutex::new(()),
			reader: Mutex::new(Some(reader)),
			stderr_drain: Mutex::new(Some(stderr_drain)),
			stderr_tail,
			pid,
			interrupt_grace: languages::INTERRUPT_ESCALATION,
		})
	}

	async fn run_startup(
		&self,
		label: &str,
		code: &str,
		cancellation: &CancellationToken,
	) -> Result<(), EvalError> {
		let token = CancellationToken::new();
		let (combined, combined_task) = combine(cancellation.clone(), token.clone());
		let timeout = languages::startup_timeout(self.language);
		let timer = tokio::spawn(async move {
			tokio::time::sleep(timeout).await;
			token.cancel();
		});
		let env = HashMap::new();
		let result = self
			.execute(
				code,
				KernelExecuteOptions {
					run_id: format!("{}-{label}", self.id),
					cwd: Path::new("."),
					env: &env,
					silent: true,
					store_history: false,
					cancellation: combined,
					on_chunk: None,
					on_display: None,
				},
			)
			.await;
		timer.abort();
		combined_task.abort();
		match result {
			Ok(result) if result.status == KernelStatus::Ok && !result.cancelled => Ok(()),
			Ok(result) => {
				let reason = result
					.error
					.map_or_else(|| "kernel startup was cancelled".to_string(), |error| error.value);
				let stderr = self.stderr_text();
				self.shutdown().await;
				Err(EvalError::Kernel(if stderr.trim().is_empty() {
					format!("{label} failed: {reason}")
				} else {
					format!("{label} failed: {reason}\n{stderr}")
				}))
			},
			Err(error) => {
				self.shutdown().await;
				Err(error)
			},
		}
	}

	/// Execute one cell. Concurrent callers serialize; one cell at a time.
	pub async fn execute(
		&self,
		code: &str,
		mut options: KernelExecuteOptions<'_>,
	) -> Result<KernelExecuteResult, EvalError> {
		let _guard = self.execution.lock().await;
		if !self.is_alive() {
			return Err(EvalError::Kernel(format!(
				"{} kernel is not running{}",
				self.language.display_name(),
				self.stderr_suffix()
			)));
		}
		if options.cancellation.is_cancelled() {
			return Ok(cancelled_result(true));
		}

		let payload = self.build_payload(code, &options);
		if payload.len() > MAX_FRAME_BYTES {
			return Err(EvalError::InvalidArguments(format!(
				"cell payload exceeds {MAX_FRAME_BYTES} bytes"
			)));
		}
		{
			let mut stdin = self.stdin.lock().await;
			stdin.write_all(payload.as_bytes()).await?;
			stdin.write_all(b"\n").await?;
			stdin.flush().await?;
		}

		let done = Arc::new(AtomicBool::new(false));
		let mut interrupt_sent = false;
		let mut status = KernelStatus::Ok;
		let mut error = None;
		let mut execution_count = None;
		let mut cancelled = false;

		let mut frames = self.frames.lock().await;
		loop {
			if options.cancellation.is_cancelled() && !interrupt_sent {
				interrupt_sent = true;
				cancelled = true;
				self.interrupt();
				spawn_escalation(self.pid, Arc::clone(&done), self.interrupt_escalation());
			}
			let event = tokio::select! {
				event = frames.recv() => event,
				() = options.cancellation.cancelled(), if !interrupt_sent => {
					interrupt_sent = true;
					cancelled = true;
					self.interrupt();
					spawn_escalation(self.pid, Arc::clone(&done), self.interrupt_escalation());
					continue;
				},
			};
			match event {
				None | Some(KernelEvent::Closed) => {
					done.store(true, Ordering::SeqCst);
					self.alive.store(false, Ordering::SeqCst);
					return Ok(KernelExecuteResult {
						status: KernelStatus::Error,
						execution_count,
						error: Some(KernelError {
							name: "KernelExited".to_string(),
							value: format!(
								"{} kernel exited unexpectedly{}",
								self.language.display_name(),
								self.stderr_suffix()
							),
							traceback: Vec::new(),
						}),
						cancelled: true,
						timed_out: false,
						stdin_requested: false,
						kernel_killed: true,
					});
				},
				Some(KernelEvent::Raw(text)) => {
					if let Some(on_chunk) = options.on_chunk.as_mut()
						&& !text.is_empty()
					{
						on_chunk(&text);
					}
				},
				Some(KernelEvent::Frame(frame)) => {
					if frame.id.as_deref() != Some(options.run_id.as_str()) {
						continue;
					}
					match frame.kind.as_str() {
						"started" => {},
						"stdout" | "stderr" => {
							if let (Some(chunk), Some(on_chunk)) =
								(frame.data.as_deref(), options.on_chunk.as_mut())
								&& !chunk.is_empty()
							{
								on_chunk(chunk);
							}
						},
						"display" | "result" => {
							let Some(bundle) = frame.bundle.as_ref() else { continue };
							let rendered = render_display_bundle(bundle);
							if !rendered.text.is_empty()
								&& let Some(on_chunk) = options.on_chunk.as_mut()
							{
								on_chunk(&rendered.text);
							}
							if let Some(on_display) = options.on_display.as_mut() {
								for output in rendered.outputs {
									on_display(output);
								}
							}
						},
						"error" => {
							status = KernelStatus::Error;
							let traceback = frame.traceback.unwrap_or_default();
							let name = frame.ename.unwrap_or_else(|| "Error".to_string());
							let value = frame.evalue.unwrap_or_default();
							let message = if traceback.is_empty() {
								format!("{name}: {value}\n")
							} else {
								format!("{}\n", traceback.join("\n"))
							};
							if let Some(on_chunk) = options.on_chunk.as_mut() {
								on_chunk(&message);
							}
							error = Some(KernelError { name, value, traceback });
						},
						"done" => {
							done.store(true, Ordering::SeqCst);
							if frame.status.as_deref() == Some("error") && status == KernelStatus::Ok {
								status = KernelStatus::Error;
							}
							if frame.cancelled.unwrap_or(false) {
								cancelled = true;
							}
							execution_count = frame.execution_count;
							return Ok(KernelExecuteResult {
								status,
								execution_count,
								error,
								cancelled,
								timed_out: false,
								stdin_requested: false,
								kernel_killed: false,
							});
						},
						_ => {},
					}
				},
			}
		}
	}

	fn interrupt_escalation(&self) -> Duration {
		self.interrupt_grace
	}

	fn stderr_suffix(&self) -> String {
		let stderr = self.stderr_text();
		let text = stderr.trim();
		if text.is_empty() {
			String::new()
		} else {
			format!(": {text}")
		}
	}

	fn build_payload(&self, code: &str, options: &KernelExecuteOptions<'_>) -> String {
		let cwd = options.cwd.display().to_string();
		match self.language {
			EvalLanguage::Jl => {
				let env_pairs = options
					.env
					.iter()
					.map(|(key, value)| {
						format!(
							"{}:{}",
							util::base64_encode(key.as_bytes()),
							util::base64_encode(value.as_bytes())
						)
					})
					.collect::<Vec<_>>()
					.join(" ");
				format!(
					"run\t{}\t{}\t{}\t{}\t{}\t{}",
					options.run_id,
					util::base64_encode(cwd.as_bytes()),
					if options.silent { "1" } else { "0" },
					if options.store_history && !options.silent { "1" } else { "0" },
					env_pairs,
					util::base64_encode(code.as_bytes())
				)
			},
			_ => serde_json::json!({
				"id": options.run_id,
				"code": code,
				"cwd": cwd,
				"env": options.env,
				"silent": options.silent,
				"storeHistory": options.store_history && !options.silent,
			})
			.to_string(),
		}
	}

	/// Deliver SIGINT to the runner so user code sees `KeyboardInterrupt`.
	pub fn interrupt(&self) {
		#[cfg(unix)]
		if let Some(pid) = self.pid.filter(|pid| *pid > 1) {
			// SAFETY: signal a child we spawned.
			unsafe {
				libc::kill(pid as i32, libc::SIGINT);
			}
		}
	}

	/// Graceful exit with SIGTERM/SIGKILL escalation and a group sweep.
	pub async fn shutdown(&self) {
		if self.shutdown_confirmed.swap(true, Ordering::SeqCst) {
			return;
		}
		self.alive.store(false, Ordering::SeqCst);
		{
			let mut stdin = self.stdin.lock().await;
			let _ = stdin.write_all(languages::exit_payload(self.language).as_bytes()).await;
			let _ = stdin.write_all(b"\n").await;
			let _ = stdin.flush().await;
			let _ = stdin.shutdown().await;
		}
		let grace = languages::SHUTDOWN_GRACE;
		let mut child = self.child.lock().await;
		if tokio::time::timeout(grace, child.wait()).await.is_ok() {
			return;
		}
		signal_group(self.pid, TERM_SIGNAL);
		if tokio::time::timeout(grace, child.wait()).await.is_ok() {
			signal_group(self.pid, KILL_SIGNAL);
			return;
		}
		let _ = child.start_kill();
		signal_group(self.pid, KILL_SIGNAL);
		let _ = tokio::time::timeout(grace, child.wait()).await;
	}

	/// Total owned time for diagnostics/tests.
	pub fn stderr_tail(&self) -> String {
		self.stderr_text()
	}
}

impl Drop for Kernel {
	fn drop(&mut self) {
		if let Some(reader) = self.reader.lock().unwrap_or_else(|poison| poison.into_inner()).take() {
			reader.abort();
		}
		if let Some(drain) = self.stderr_drain.lock().unwrap_or_else(|poison| poison.into_inner()).take() {
			drain.abort();
		}
		// Best-effort group sweep: Drop cannot await a graceful exit.
		signal_group(self.pid, KILL_SIGNAL);
	}
}

fn cancelled_result(timed_out: bool) -> KernelExecuteResult {
	KernelExecuteResult {
		status: KernelStatus::Error,
		execution_count: None,
		error: None,
		cancelled: true,
		timed_out,
		stdin_requested: false,
		kernel_killed: false,
	}
}

fn combine(external: CancellationToken, idle: CancellationToken) -> (CancellationToken, JoinHandle<()>) {
	let combined = CancellationToken::new();
	let target = combined.clone();
	let task = tokio::spawn(async move {
		tokio::select! {
			() = external.cancelled() => {},
			() = idle.cancelled() => {},
		}
		target.cancel();
	});
	(combined, task)
}

fn spawn_escalation(pid: Option<u32>, done: Arc<AtomicBool>, after: Duration) {
	tokio::spawn(async move {
		tokio::time::sleep(after).await;
		if done.load(Ordering::SeqCst) {
			return;
		}
		signal_group(pid, KILL_SIGNAL);
	});
}

fn signal_group(pid: Option<u32>, signal: i32) {
	#[cfg(unix)]
	if let Some(pid) = pid.filter(|pid| *pid > 1) {
		// SAFETY: negative pid targets the child session we created; 0/1 are
		// rejected because they would signal our own group or everything.
		unsafe {
			libc::kill(-(pid as i32), signal);
		}
	}
}

async fn read_frames(stdout: tokio::process::ChildStdout, sender: mpsc::UnboundedSender<KernelEvent>) {
	let mut reader = BufReader::new(stdout);
	let mut line = Vec::new();
	loop {
		line.clear();
		match reader.read_until(b'\n', &mut line).await {
			Ok(0) | Err(_) => break,
			Ok(_) => {},
		}
		if line.len() > MAX_FRAME_BYTES {
			continue;
		}
		let text = String::from_utf8_lossy(&line);
		if text.trim().is_empty() {
			continue;
		}
		match serde_json::from_str::<KernelFrame>(text.trim()) {
			Ok(frame) => {
				if sender.send(KernelEvent::Frame(frame)).is_err() {
					break;
				}
			},
			Err(_) => {
				if sender.send(KernelEvent::Raw(text.into_owned())).is_err() {
					break;
				}
			},
		}
	}
	let _ = sender.send(KernelEvent::Closed);
}

async fn drain_stderr(stderr: tokio::process::ChildStderr, tail: Arc<Mutex<String>>) {
	use tokio::io::AsyncReadExt as _;
	let mut reader = BufReader::new(stderr);
	let mut buffer = [0u8; 1024];
	loop {
		match reader.read(&mut buffer).await {
			Ok(0) | Err(_) => break,
			Ok(read) => {
				let text = String::from_utf8_lossy(&buffer[..read]);
				let mut tail = tail.lock().unwrap_or_else(|poison| poison.into_inner());
				tail.push_str(&text);
				if tail.len() > STDERR_TAIL_BYTES {
					let keep = tail.len() - STDERR_TAIL_BYTES;
					let cut = tail
						.char_indices()
						.find(|(index, _)| *index >= keep)
						.map_or(tail.len(), |(index, _)| index);
					tail.drain(..cut);
				}
			},
		}
	}
}

/// Staged runner path for diagnostics/tests.
pub fn staged_runner_path(language: EvalLanguage) -> std::io::Result<PathBuf> {
	languages::stage_runner(language)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::Instant;

	const FAKE_WIRE: &str = r#"trap '' INT
while IFS= read -r line; do
  case "$line" in
    *'"id":"'*)
      id=$(printf '%s' "$line" | sed -E 's/.*"id":"([^"]+)".*/\1/')
      printf '{"type":"stdout","id":"%s","data":"ready"}\n' "$id"
      ;;
  esac
done"#;

	const FAKE_WIRE_RAW: &str = r#"trap '' INT
while IFS= read -r line; do
  case "$line" in
    *'"id":"'*)
      id=$(printf '%s' "$line" | sed -E 's/.*"id":"([^"]+)".*/\1/')
      printf 'garbage-line\n'
      printf '{"type":"done","id":"%s","status":"ok"}\n' "$id"
      ;;
  esac
done"#;

	#[cfg(unix)]
	#[tokio::test]
	async fn non_protocol_lines_surface_as_cell_output() {
		let env = std::env::vars().collect::<HashMap<_, _>>();
		let mut kernel = Kernel::spawn_process(
			EvalLanguage::Py,
			&["sh".to_string(), "-c".to_string(), FAKE_WIRE_RAW.to_string()],
			Path::new("."),
			env,
		)
		.expect("fake kernel spawn");
		let mut sink = String::new();
		let env = HashMap::new();
		let result = {
			let on_chunk = |chunk: &str| sink.push_str(chunk);
			kernel
				.execute(
					"ignored",
					KernelExecuteOptions {
						run_id: "run-raw".to_string(),
						cwd: Path::new("."),
						env: &env,
						silent: false,
						store_history: false,
						cancellation: CancellationToken::new(),
						on_chunk: Some(Box::new(on_chunk)),
						on_display: None,
					},
				)
				.await
				.expect("execute")
		};
		assert_eq!(result.status, KernelStatus::Ok, "done frame was recognized: {result:?}");
		assert!(sink.contains("garbage-line"), "raw line was dropped: {sink:?}");
		kernel.shutdown().await;
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn fake_wire_cancel_escalates_and_reports_kernel_killed() {
		let env = std::env::vars().collect::<HashMap<_, _>>();
		let mut kernel = Kernel::spawn_process(
			EvalLanguage::Py,
			&["sh".to_string(), "-c".to_string(), FAKE_WIRE.to_string()],
			Path::new("."),
			env,
		)
		.expect("fake kernel spawn");
		kernel.interrupt_grace = Duration::from_millis(200);

		let cancellation = CancellationToken::new();
		let trigger = cancellation.clone();
		tokio::spawn(async move {
			tokio::time::sleep(Duration::from_millis(150)).await;
			trigger.cancel();
		});
		let started = Instant::now();
		let mut sink = String::new();
		let env = HashMap::new();
		let result = {
			let on_chunk = |chunk: &str| sink.push_str(chunk);
			kernel
				.execute(
					"ignored",
					KernelExecuteOptions {
						run_id: "run-1".to_string(),
						cwd: Path::new("."),
						env: &env,
						silent: false,
						store_history: false,
						cancellation: cancellation.clone(),
						on_chunk: Some(Box::new(on_chunk)),
						on_display: None,
					},
				)
				.await
				.expect("execute")
		};
		assert!(sink.contains("ready"), "wire output was captured: {sink:?}");
		assert!(result.cancelled || result.kernel_killed);
		assert!(started.elapsed() < Duration::from_secs(2), "escalation was bounded");
		assert!(!kernel.is_alive() || result.kernel_killed);
	}
}
