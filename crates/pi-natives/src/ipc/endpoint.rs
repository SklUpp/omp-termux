//! Deterministic endpoint naming shared by every process that dials or binds.
//!
//! The helpers here are pure so the naming rules (stem sanitizing, `sun_path`
//! relocation, Windows pipe shape) are testable on any host OS.

#[cfg(windows)]
use std::path::{self, Path};
#[cfg(unix)]
use std::{
	env,
	path::{Component, Path, PathBuf},
};

use xxhash_rust::xxh64::xxh64;

/// `name` with every char outside `[A-Za-z0-9._-]` replaced by `_`.
pub fn sanitize_stem(name: &str) -> String {
	name
		.chars()
		.map(|ch| {
			if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
				ch
			} else {
				'_'
			}
		})
		.collect()
}

/// 16 lowercase hex digits identifying `bytes`.
pub fn hash16(bytes: &[u8]) -> String {
	format!("{:016x}", xxh64(bytes, 0))
}

/// Termux install root used when neither installed-root variable is set.
#[cfg(any(target_os = "android", all(test, unix)))]
const ANDROID_DEFAULT_PREFIX: &str = "/data/data/com.termux/files/usr";

/// Message returned when no `sun_path`-sized endpoint can be built.
pub const ENDPOINT_TOO_LONG: &str =
	"socket path does not fit sockaddr_un (set OMP_ANDROID_PREFIX to the installed Termux root)";

/// Private per-user directory that hosts relocated Unix sockets.
///
/// The value is a pure function of the effective uid and, on Android, of the
/// installed Termux root: `/tmp/omp-<euid>` elsewhere and
/// `<install root>/tmp/omp-<euid>` on Android. The launcher exports the
/// installed root as `OMP_ANDROID_PREFIX`, so every process started from the
/// same install computes the same directory even when a shell's `PREFIX`
/// differs or is unset. No filesystem probe takes part, so naming cannot
/// change with a transient permission or an existing-but-unusable directory.
/// `TMPDIR` is deliberately not consulted: shells, `termux-chroot`, and child
/// environments set it independently of the install.
#[cfg_attr(windows, allow(dead_code, reason = "pure helper unit-tested on every host"))]
pub fn relocation_dir(euid: u32) -> String {
	#[cfg(target_os = "android")]
	{
		android_relocation_dir(
			env::var_os("OMP_ANDROID_PREFIX").as_deref().map(Path::new),
			env::var_os("PREFIX").as_deref().map(Path::new),
			euid,
		)
	}
	#[cfg(not(target_os = "android"))]
	{
		format!("/tmp/omp-{euid}")
	}
}

/// `<root>/tmp/omp-<euid>`, where `root` is the installed root exported by the
/// launcher, else `PREFIX`, else the default Termux root. Relative candidates
/// are ignored so the endpoint is always absolute and shared.
#[cfg(any(target_os = "android", all(test, unix)))]
fn android_relocation_dir(installed: Option<&Path>, prefix: Option<&Path>, euid: u32) -> String {
	let root = installed
		.filter(|path| path.is_absolute())
		.or_else(|| prefix.filter(|path| path.is_absolute()))
		.unwrap_or_else(|| Path::new(ANDROID_DEFAULT_PREFIX));
	let tmp = root.join("tmp");
	let tmp = tmp.to_string_lossy();
	format!("{}/omp-{euid}", tmp.trim_end_matches('/'))
}

/// Unix socket path for `name` in the already-resolved `dir`.
///
/// `sun_path_max` is the size of `sockaddr_un::sun_path` including the NUL
/// terminator; a path that does not fit is relocated under
/// [`relocation_dir`] with a hashed basename. Fails with
/// [`ENDPOINT_TOO_LONG`] when even the relocated path cannot fit (an install
/// root long enough to exhaust `sun_path`).
#[cfg_attr(windows, allow(dead_code, reason = "pure helper unit-tested on every host"))]
pub fn unix_endpoint(
	dir: &str,
	name: &str,
	euid: u32,
	sun_path_max: usize,
) -> Result<String, &'static str> {
	unix_endpoint_with(dir, name, &relocation_dir(euid), sun_path_max)
}

/// [`unix_endpoint`] with an explicit relocation directory, for tests and for
/// callers that already resolved it.
fn unix_endpoint_with(
	dir: &str,
	name: &str,
	relocation: &str,
	sun_path_max: usize,
) -> Result<String, &'static str> {
	let stem = sanitize_stem(name);
	let direct = if dir.ends_with('/') {
		format!("{dir}{stem}.sock")
	} else {
		format!("{dir}/{stem}.sock")
	};
	if direct.len() < sun_path_max {
		return Ok(direct);
	}
	let mut key = Vec::with_capacity(dir.len() + 1 + name.len());
	key.extend_from_slice(dir.as_bytes());
	key.push(0);
	key.extend_from_slice(name.as_bytes());
	let relocated = format!("{relocation}/{}.sock", hash16(&key));
	if relocated.len() >= sun_path_max {
		return Err(ENDPOINT_TOO_LONG);
	}
	Ok(relocated)
}

/// Windows named-pipe path for `name` in the already-resolved `dir`.
#[cfg_attr(not(windows), allow(dead_code, reason = "pure helper unit-tested on every host"))]
pub fn windows_pipe_name(dir: &str, name: &str) -> String {
	format!(r"\\.\pipe\omp-{}-{}", sanitize_stem(name), hash16(dir.to_lowercase().as_bytes()))
}

/// Size of `sockaddr_un::sun_path` on this platform, including the NUL.
#[cfg(unix)]
pub const SUN_PATH_MAX: usize = if cfg!(any(target_os = "linux", target_os = "android")) {
	108
} else {
	104
};

/// Lexically absolute, normalized form of `dir` (like Node's `path.resolve`).
#[cfg(unix)]
pub fn resolve_dir(dir: &str) -> String {
	let path = Path::new(dir);
	let joined = if path.is_absolute() {
		path.to_path_buf()
	} else {
		env::current_dir()
			.unwrap_or_else(|_| PathBuf::from("/"))
			.join(path)
	};
	let mut resolved = PathBuf::from("/");
	for component in joined.components() {
		match component {
			Component::ParentDir => {
				resolved.pop();
			},
			Component::Normal(part) => resolved.push(part),
			Component::RootDir | Component::CurDir | Component::Prefix(_) => {},
		}
	}
	resolved
		.into_string()
		.unwrap_or_else(|raw| raw.to_string_lossy().into_owned())
}

/// Lexically absolute, normalized form of `dir` (like Node's `path.resolve`).
#[cfg(windows)]
pub fn resolve_dir(dir: &str) -> String {
	let target = if dir.is_empty() {
		Path::new(".")
	} else {
		Path::new(dir)
	};
	let resolved = path::absolute(target).unwrap_or_else(|_| target.to_path_buf());
	let mut text = resolved
		.into_string()
		.unwrap_or_else(|raw| raw.to_string_lossy().into_owned());
	// `GetFullPathNameW` keeps a trailing separator; `path.resolve` drops it
	// except on a drive root such as `C:\`.
	while text.len() > 3 && text.ends_with(['\\', '/']) {
		text.pop();
	}
	text
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn sanitizes_stem_per_char() {
		assert_eq!(sanitize_stem("a b"), "a_b");
		assert_eq!(sanitize_stem("Ok.name_1-x"), "Ok.name_1-x");
		assert_eq!(sanitize_stem("a/b\\c:é"), "a_b_c__");
	}

	#[test]
	fn hash16_is_sixteen_hex_digits_and_stable() {
		let hash = hash16(b"abc");
		assert_eq!(hash.len(), 16);
		assert!(
			hash
				.bytes()
				.all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
		);
		assert_eq!(hash, hash16(b"abc"));
		assert_ne!(hash, hash16(b"abd"));
	}

	#[test]
	fn unix_endpoint_keeps_short_paths_in_dir() {
		assert_eq!(unix_endpoint("/tmp/x", "a b", 501, 104).as_deref(), Ok("/tmp/x/a_b.sock"));
		assert_eq!(unix_endpoint("/", "s", 501, 104).as_deref(), Ok("/s.sock"));
	}

	#[test]
	fn unix_endpoint_relocates_at_sun_path_threshold() {
		// `<dir>/<stem>.sock` plus NUL must fit in `sun_path_max`.
		let name = "n";
		let suffix_len = "/n.sock".len();
		let fits = format!("/{}", "d".repeat(104 - 1 - suffix_len - 1));
		let direct = format!("{fits}/n.sock");
		assert_eq!(direct.len(), 103);
		assert_eq!(unix_endpoint(&fits, name, 501, 104).as_deref(), Ok(direct.as_str()));

		let too_long = format!("{fits}d");
		let relocated = unix_endpoint(&too_long, name, 501, 104).unwrap();
		let mut key = too_long.clone().into_bytes();
		key.push(0);
		key.extend_from_slice(name.as_bytes());
		assert_eq!(relocated, format!("{}/{}.sock", relocation_dir(501), hash16(&key)));
		// Linux's larger sun_path keeps the same directory in place.
		assert_eq!(unix_endpoint(&too_long, name, 501, 108).unwrap(), format!("{too_long}/n.sock"));
	}

	#[cfg(unix)]
	#[test]
	fn unix_endpoint_rejects_relocated_paths_that_cannot_fit() {
		let too_long = format!("/{}", "d".repeat(200));
		let prefix_of = |len: usize| format!("/{}", "p".repeat(len - 1));

		// `<root>/omp-<uid>/<hash16>.sock`: 34 bytes with uid 501 and one path
		// separator, so an 74-byte install root lands exactly on 108.
		let at_limit = android_relocation_dir(Some(Path::new(&prefix_of(74))), None, 501);
		assert_eq!(at_limit.len(), 74 + "/tmp/omp-501".len());
		assert_eq!(unix_endpoint_with(&too_long, "n", &at_limit, 108), Err(ENDPOINT_TOO_LONG));

		let one_short = android_relocation_dir(Some(Path::new(&prefix_of(73))), None, 501);
		assert_eq!(
			unix_endpoint_with(&too_long, "n", &one_short, 108)
				.unwrap()
				.len(),
			107
		);

		// The limit counts bytes, not chars: a multibyte root of the same byte
		// length is rejected identically.
		let multibyte = format!("/{}{}", "é".repeat(36), "p");
		assert_eq!(multibyte.len(), 74);
		let multibyte = android_relocation_dir(Some(Path::new(&multibyte)), None, 501);
		assert_eq!(unix_endpoint_with(&too_long, "n", &multibyte, 108), Err(ENDPOINT_TOO_LONG));
	}

	#[cfg(unix)]
	#[test]
	fn android_relocation_dir_is_fixed_by_install_root() {
		let default = "/data/data/com.termux/files/usr/tmp/omp-501";
		assert_eq!(android_relocation_dir(None, None, 501), default);
		assert_eq!(
			android_relocation_dir(Some(Path::new(ANDROID_DEFAULT_PREFIX)), None, 501),
			default
		);
		assert_eq!(android_relocation_dir(None, Some(Path::new("/x")), 501), "/x/tmp/omp-501");
		assert_eq!(
			android_relocation_dir(Some(Path::new("/x/")), Some(Path::new("/y")), 501),
			"/x/tmp/omp-501"
		);
		// A relative root cannot name a shared absolute socket.
		assert_eq!(
			android_relocation_dir(Some(Path::new("relative")), Some(Path::new("relative")), 501),
			default
		);
		// The launcher's installed root wins over a shell's differing PREFIX, so
		// the same install names one socket regardless of the caller environment.
		assert_eq!(
			android_relocation_dir(Some(Path::new("/installed")), Some(Path::new("/other")), 501),
			android_relocation_dir(Some(Path::new("/installed")), None, 501)
		);
	}

	#[cfg(target_os = "android")]
	#[test]
	fn android_relocated_endpoint_uses_install_root_and_stays_bounded() {
		let installed = std::env::var_os("OMP_ANDROID_PREFIX").map(std::path::PathBuf::from);
		let prefix = std::env::var_os("PREFIX").map(std::path::PathBuf::from);
		let root = android_relocation_dir(installed.as_deref(), prefix.as_deref(), 501);
		let relocated =
			unix_endpoint(&format!("/{}", "d".repeat(200)), "n", 501, SUN_PATH_MAX).unwrap();
		assert!(relocated.starts_with(&format!("{root}/")), "{relocated}");
		assert!(relocated.ends_with(".sock"));
		assert!(relocated.len() < SUN_PATH_MAX);
	}

	#[test]
	fn relocation_hash_separates_dir_and_name() {
		let dir = format!("/{}", "x".repeat(200));
		assert_ne!(
			unix_endpoint(&dir, "ab", 0, 104).unwrap(),
			unix_endpoint(&format!("{dir}a"), "b", 0, 104).unwrap()
		);
	}

	#[test]
	fn windows_pipe_name_shape() {
		let name = windows_pipe_name(r"C:\Users\Me\Proj", "my sock");
		assert_eq!(name, format!(r"\\.\pipe\omp-my_sock-{}", hash16(br"c:\users\me\proj")));
		assert_eq!(name, windows_pipe_name(r"c:\users\me\PROJ", "my sock"));
	}

	#[cfg(unix)]
	#[test]
	fn resolve_dir_normalizes_lexically() {
		assert_eq!(resolve_dir("/a/b/../c/./d/"), "/a/c/d");
		assert_eq!(resolve_dir("/.."), "/");
		let cwd = std::env::current_dir().unwrap();
		assert_eq!(resolve_dir("x"), cwd.join("x").to_str().unwrap());
	}
}
