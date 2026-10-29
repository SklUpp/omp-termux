//! Retained kernel registry keyed by language, owner, session, cwd, interpreter.
//!
//! The runner mutates process-global cwd/sys.path, so a live kernel is never
//! shared across directories. Multiple owners may register the same key; the
//! kernel lives until the last owner slot is disposed. `reset` replaces only the
//! requested language/session kernel and is destructive to concurrent sharers.

use std::{
	collections::HashMap,
	path::{Path, PathBuf},
	sync::{
		Arc, Mutex,
		atomic::{AtomicU64, Ordering},
	},
};

use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use super::{
	env::InterpreterRuntime,
	kernel::{Kernel, KernelSpawn},
	types::{EvalError, EvalLanguage},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KernelKey {
	pub language: EvalLanguage,
	pub owner: Option<String>,
	pub session_id: String,
	pub cwd: PathBuf,
	pub interpreter: Option<String>,
}

impl KernelKey {
	fn storage_key(&self) -> String {
		format!(
			"{}\0{}\0{}\0{}\0{}",
			self.language.token(),
			self.owner.as_deref().unwrap_or(""),
			self.session_id,
			self.cwd.display(),
			self.interpreter.as_deref().unwrap_or("")
		)
	}
}

struct Slot {
	key: KernelKey,
	generation: AtomicU64,
	kernel: AsyncMutex<Option<Arc<Kernel>>>,
}

/// Owned kernel registry.
#[derive(Default)]
pub struct SessionRegistry {
	slots: Mutex<HashMap<String, Arc<Slot>>>,
}

impl SessionRegistry {
	pub fn new() -> Self {
		Self::default()
	}

	fn slot(&self, key: &KernelKey) -> Arc<Slot> {
		let storage = key.storage_key();
		let mut slots = self.slots.lock().unwrap_or_else(|poison| poison.into_inner());
		Arc::clone(slots.entry(storage).or_insert_with(|| {
			Arc::new(Slot {
				key: key.clone(),
				generation: AtomicU64::new(0),
				kernel: AsyncMutex::new(None),
			})
		}))
	}

	/// Store an already-started kernel (readiness proven by its own handshake).
	///
	/// A live kernel for the slot wins: the newcomer is shut down so concurrent
	/// callers never keep two kernels for one key. `reset` callers retire the old
	/// kernel with [`Self::evict`] *before* booting the replacement.
	pub async fn adopt(&self, key: KernelKey, kernel: Arc<Kernel>) -> Arc<Kernel> {
		let slot = self.slot(&key);
		let mut guard = slot.kernel.lock().await;
		if let Some(existing) = guard.as_ref()
			&& existing.is_alive()
		{
			kernel.shutdown().await;
			return Arc::clone(existing);
		}
		if let Some(dead) = guard.take() {
			slot.generation.fetch_add(1, Ordering::SeqCst);
			dead.shutdown().await;
		}
		*guard = Some(Arc::clone(&kernel));
		kernel
	}

	/// Acquire (or replace) the kernel for `key`.
	///
	/// `reset` destroys the current kernel first. A dead retained kernel is
	/// replaced and the caller retries once at the tool layer.
	pub async fn acquire(
		&self,
		key: KernelKey,
		runtime: InterpreterRuntime,
		extra_env: HashMap<String, String>,
		reset: bool,
		cancellation: CancellationToken,
	) -> Result<Arc<Kernel>, EvalError> {
		let slot = self.slot(&key);
		let mut guard = slot.kernel.lock().await;
		if reset {
			slot.generation.fetch_add(1, Ordering::SeqCst);
			if let Some(kernel) = guard.take() {
				kernel.shutdown().await;
			}
		}
		if let Some(kernel) = guard.as_ref()
			&& kernel.is_alive()
		{
			return Ok(Arc::clone(kernel));
		}
		if let Some(dead) = guard.take() {
			slot.generation.fetch_add(1, Ordering::SeqCst);
			dead.shutdown().await;
		}
		let spawn = KernelSpawn {
			language: key.language,
			runtime,
			cwd: key.cwd.clone(),
			extra_env,
		};
		let kernel = Arc::new(Kernel::start(spawn, cancellation).await?);
		*guard = Some(Arc::clone(&kernel));
		Ok(kernel)
	}

	/// Live retained kernel for `key`, without booting a replacement.
	pub async fn live(&self, key: &KernelKey) -> Option<Arc<Kernel>> {
		let storage = key.storage_key();
		let slot = {
			let slots = self.slots.lock().unwrap_or_else(|poison| poison.into_inner());
			Arc::clone(slots.get(&storage)?)
		};
		let guard = slot.kernel.lock().await;
		guard.as_ref().filter(|kernel| kernel.is_alive()).cloned()
	}

	/// Retire every retained kernel for one language/owner/session/cwd.
	///
	/// Used by the readiness path, which boots the replacement before it can be
	/// adopted: the old process must be gone before the new one starts.
	pub async fn evict(&self, language: EvalLanguage, owner: Option<&str>, session_id: &str, cwd: &Path) {
		let removed: Vec<Arc<Slot>> = {
			let mut slots = self.slots.lock().unwrap_or_else(|poison| poison.into_inner());
			let keys: Vec<String> = slots
				.iter()
				.filter(|(_, slot)| {
					slot.key.language == language
						&& slot.key.owner.as_deref() == owner
						&& slot.key.session_id == session_id
						&& slot.key.cwd.as_path() == cwd
				})
				.map(|(key, _)| key.clone())
				.collect();
			keys.into_iter().filter_map(|key| slots.remove(&key)).collect()
		};
		for slot in removed {
			if let Some(kernel) = slot.kernel.lock().await.take() {
				kernel.shutdown().await;
			}
		}
	}

	/// Remove and shut down every kernel owned by `owner`.
	pub async fn dispose_owner(&self, owner: Option<&str>) {
		let removed: Vec<Arc<Slot>> = {
			let mut slots = self.slots.lock().unwrap_or_else(|poison| poison.into_inner());
			let keys: Vec<String> = slots
				.iter()
				.filter(|(_, slot)| slot.key.owner.as_deref() == owner)
				.map(|(key, _)| key.clone())
				.collect();
			keys.into_iter().filter_map(|key| slots.remove(&key)).collect()
		};
		for slot in removed {
			if let Some(kernel) = slot.kernel.lock().await.take() {
				kernel.shutdown().await;
			}
		}
	}

	pub async fn dispose_all(&self) {
		let removed: Vec<Arc<Slot>> = {
			let mut slots = self.slots.lock().unwrap_or_else(|poison| poison.into_inner());
			slots.drain().map(|(_, slot)| slot).collect()
		};
		for slot in removed {
			if let Some(kernel) = slot.kernel.lock().await.take() {
				kernel.shutdown().await;
			}
		}
	}

	/// Slots retained for one owner (diagnostics/tests).
	pub fn retained_count(&self, owner: Option<&str>) -> usize {
		self
			.slots
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.values()
			.filter(|slot| slot.key.owner.as_deref() == owner)
			.count()
	}

	/// Generation counter for one key (reset/dead replacement forks it).
	pub fn generation(&self, key: &KernelKey) -> Option<u64> {
		self
			.slots
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.get(&key.storage_key())
			.map(|slot| slot.generation.load(Ordering::SeqCst))
	}
}

/// Normalized cwd for keying: canonicalized when possible, never empty.
pub fn normalize_cwd(cwd: &Path) -> PathBuf {
	cwd.canonicalize().unwrap_or_else(|_| {
		if cwd.as_os_str().is_empty() {
			PathBuf::from(".")
		} else {
			cwd.to_path_buf()
		}
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn keys_separate_cwd_interpreter_and_owner() {
		let key = |cwd: &str, interpreter: Option<&str>, owner: Option<&str>| KernelKey {
			language: EvalLanguage::Py,
			owner: owner.map(str::to_string),
			session_id: "s".to_string(),
			cwd: PathBuf::from(cwd),
			interpreter: interpreter.map(str::to_string),
		};
		assert_ne!(key("/a", None, None).storage_key(), key("/b", None, None).storage_key());
		assert_ne!(key("/a", Some("py"), None).storage_key(), key("/a", Some("py3"), None).storage_key());
		assert_ne!(key("/a", None, Some("x")).storage_key(), key("/a", None, Some("y")).storage_key());
	}
}
