//! Eval cell watchdog with reference-counted pause/resume.
//!
//! A cell's `timeout` bounds only time while the kernel has control. Host-side
//! `agent()`, `parallel()`, `pipeline()`, and `completion()` bridges pause the
//! watchdog; returning control starts a fresh window.

use std::{
	sync::{
		Arc, Mutex,
		atomic::{AtomicBool, Ordering},
	},
	time::{Duration, Instant},
};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

struct State {
	depth: usize,
	deadline: Instant,
	timer: Option<JoinHandle<()>>,
	settled: bool,
}

struct Inner {
	idle: Duration,
	token: CancellationToken,
	timed_out: AtomicBool,
	state: Mutex<State>,
}

impl Inner {
	fn arm(self: &Arc<Self>) {
		let deadline = {
			let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
			if state.settled || state.depth > 0 {
				return;
			}
			if let Some(timer) = state.timer.take() {
				timer.abort();
			}
			state.deadline
		};
		let inner = Arc::clone(self);
		let timer = tokio::spawn(async move {
			loop {
				let now = Instant::now();
				if now >= deadline {
					break;
				}
				tokio::time::sleep(deadline.saturating_duration_since(now)).await;
				// Re-read the deadline: a resume may have moved it forward.
				let current = {
					let state = inner.state.lock().unwrap_or_else(|poison| poison.into_inner());
					state.deadline
				};
				if current <= now {
					break;
				}
			}
			let fired = {
				let mut state = inner.state.lock().unwrap_or_else(|poison| poison.into_inner());
				if state.settled || state.depth > 0 {
					false
				} else {
					state.settled = true;
					state.timer = None;
					true
				}
			};
			if fired {
				inner.timed_out.store(true, Ordering::SeqCst);
				inner.token.cancel();
			}
		});
		let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
		if !state.settled {
			state.timer = Some(timer);
		}
	}
}

/// Watchdog handle.
#[derive(Clone)]
pub struct IdleTimeout {
	inner: Arc<Inner>,
}

impl IdleTimeout {
	pub fn new(idle: Duration) -> Self {
		let idle = idle.max(Duration::from_millis(1));
		let inner = Arc::new(Inner {
			idle,
			token: CancellationToken::new(),
			timed_out: AtomicBool::new(false),
			state: Mutex::new(State {
				depth: 0,
				deadline: Instant::now() + idle,
				timer: None,
				settled: false,
			}),
		});
		inner.arm();
		Self { inner }
	}

	pub fn idle(&self) -> Duration {
		self.inner.idle
	}

	/// Cancelled with the `TimeoutError` analogue when the active window expires.
	pub fn token(&self) -> CancellationToken {
		self.inner.token.clone()
	}

	pub fn timed_out(&self) -> bool {
		self.inner.timed_out.load(Ordering::SeqCst)
	}

	/// Suspend accounting while host-side work runs.
	pub fn pause(&self) {
		let mut state = self.inner.state.lock().unwrap_or_else(|poison| poison.into_inner());
		if state.settled {
			return;
		}
		state.depth += 1;
		if state.depth == 1
			&& let Some(timer) = state.timer.take()
		{
			timer.abort();
		}
	}

	/// Resume accounting with a fresh window.
	pub fn resume(&self) {
		let arm = {
			let mut state = self.inner.state.lock().unwrap_or_else(|poison| poison.into_inner());
			if state.settled || state.depth == 0 {
				return;
			}
			state.depth -= 1;
			if state.depth > 0 {
				return;
			}
			state.deadline = Instant::now() + self.inner.idle;
			true
		};
		if arm {
			self.inner.arm();
		}
	}

	pub fn dispose(&self) {
		let mut state = self.inner.state.lock().unwrap_or_else(|poison| poison.into_inner());
		if state.settled {
			return;
		}
		state.settled = true;
		if let Some(timer) = state.timer.take() {
			timer.abort();
		}
	}
}

impl Drop for IdleTimeout {
	fn drop(&mut self) {
		// Each clone drops; the anyway-idempotent dispose runs once the last
		// handle disappears only when state is still unsettled.
		if Arc::strong_count(&self.inner) == 1 {
			self.dispose();
		}
	}
}

/// Combine the turn cancellation and the cell watchdog into one token.
pub fn combine_with(
	external: CancellationToken,
	idle: CancellationToken,
) -> (CancellationToken, JoinHandle<()>) {
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

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn pause_ignores_expiry_and_resume_restarts_window() {
		let idle = IdleTimeout::new(Duration::from_millis(100));
		idle.pause();
		tokio::time::sleep(Duration::from_millis(250)).await;
		assert!(!idle.token().is_cancelled());
		idle.resume();
		tokio::time::sleep(Duration::from_millis(60)).await;
		assert!(!idle.token().is_cancelled());
		tokio::time::sleep(Duration::from_millis(80)).await;
		assert!(idle.token().is_cancelled());
		assert!(idle.timed_out());
	}
}
