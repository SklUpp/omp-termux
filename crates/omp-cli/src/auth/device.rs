//! RFC 8628 device-code polling.
//!
//! Anchors: `packages/ai/src/registry/oauth/device-code.ts:53`
//! (`pollOAuthDeviceCodeFlow`): 1s floor, `slow_down` adds 5s, provider expiry
//! deadline (infinite when absent), cancellation as `LoginCancelledError`, and
//! the timeout message that distinguishes clock-drift `slow_down` storms.

use std::future::Future;
use std::time::Duration;

use super::error::{AuthError, AuthErrorKind};
use super::http::Cancel;

const MINIMUM_INTERVAL_MS: u64 = 1000;
const DEFAULT_INTERVAL_SECONDS: f64 = 5.0;
const SLOW_DOWN_INCREMENT_MS: u64 = 5000;

#[derive(Debug)]
pub enum DevicePoll<T> {
	Complete(T),
	Pending,
	SlowDown,
	Failed(String),
}

/// Poll `poll` until it completes, fails, the provider deadline passes or the
/// host cancels. `poll` is an async closure so each attempt can do its own HTTP
/// round-trip with the caller's cancellation token.
pub async fn poll_device_code<T, F, Fut>(
	provider: &str,
	interval_seconds: Option<f64>,
	expires_in_seconds: Option<u64>,
	cancel: &Cancel,
	mut poll: F,
) -> Result<T, AuthError>
where
	F: FnMut() -> Fut,
	Fut: Future<Output = Result<DevicePoll<T>, AuthError>>,
{
	let deadline = expires_in_seconds.map(|seconds| tokio::time::Instant::now() + Duration::from_secs(seconds));
	let mut interval_ms =
		Duration::from_secs_f64(interval_seconds.unwrap_or(DEFAULT_INTERVAL_SECONDS)).as_millis().max(u128::from(MINIMUM_INTERVAL_MS)) as u64;
	let mut slow_down_responses = 0u32;

	loop {
		cancel.check(provider)?;
		if let Some(deadline) = deadline {
			if tokio::time::Instant::now() >= deadline {
				break;
			}
		}

		match poll().await? {
			DevicePoll::Complete(value) => return Ok(value),
			DevicePoll::Failed(message) => {
				return Err(AuthError::new(AuthErrorKind::Polling, message).for_provider(provider));
			},
			DevicePoll::SlowDown => {
				slow_down_responses += 1;
				interval_ms = (interval_ms + SLOW_DOWN_INCREMENT_MS).max(MINIMUM_INTERVAL_MS);
			},
			DevicePoll::Pending => {},
		}

		let wait = match deadline {
			Some(deadline) => {
				let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
				if remaining.is_zero() {
					break;
				}
				remaining.min(Duration::from_millis(interval_ms))
			},
			None => Duration::from_millis(interval_ms),
		};
		// Sleep in small slices so cancellation is observed promptly rather than
		// after the whole interval.
		let mut slept = Duration::ZERO;
		while slept < wait {
			cancel.check(provider)?;
			let slice = (wait - slept).min(Duration::from_millis(100));
			tokio::time::sleep(slice).await;
			slept += slice;
		}
	}

	if slow_down_responses > 0 {
		return Err(AuthError::new(
			AuthErrorKind::Timeout,
			"Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again.",
		)
		.for_provider(provider));
	}
	Err(AuthError::new(AuthErrorKind::Timeout, "Device flow timed out").for_provider(provider))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn completes_after_pending_and_slow_down() {
		let cancel = Cancel::new();
		let mut attempts = 0;
		let value = poll_device_code("test", Some(0.0), Some(30), &cancel, || {
			attempts += 1;
			let attempt = attempts;
			async move {
				Ok(match attempt {
					1 => DevicePoll::Pending,
					2 => DevicePoll::SlowDown,
					_ => DevicePoll::Complete(7),
				})
			}
		})
		.await
		.expect("completes");
		assert_eq!(value, 7);
	}

	#[tokio::test]
	async fn failure_is_polling_error() {
		let cancel = Cancel::new();
		let err = poll_device_code::<(), _, _>("test", Some(0.0), Some(30), &cancel, || async {
			Ok(DevicePoll::Failed("access_denied".to_string()))
		})
		.await
		.expect_err("fails");
		assert_eq!(err.kind, AuthErrorKind::Polling);
	}

	#[tokio::test]
	async fn cancellation_is_immediate() {
		let cancel = Cancel::new();
		cancel.cancel();
		let err = poll_device_code::<(), _, _>("test", Some(5.0), None, &cancel, || async {
			Ok(DevicePoll::Pending)
		})
		.await
		.expect_err("cancelled");
		assert_eq!(err.kind, AuthErrorKind::Cancelled);
	}

	#[tokio::test]
	async fn expiry_is_timeout() {
		let cancel = Cancel::new();
		let err = poll_device_code::<(), _, _>("test", Some(0.0), Some(0), &cancel, || async {
			Ok(DevicePoll::Pending)
		})
		.await
		.expect_err("timed out");
		assert_eq!(err.kind, AuthErrorKind::Timeout);
	}
}
