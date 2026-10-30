//! Runs the upstream apply-patch corpus (21 cases) through
//! `parse_apply_patch` + `apply_inputs` and compares result trees byte for byte.
//!
//! Run with: `cargo test -p pi-agent --test apply_patch_corpus`

use std::{
	collections::{BTreeMap, BTreeSet},
	fs,
	path::{Path, PathBuf},
	sync::atomic::{AtomicUsize, Ordering},
};

use pi_agent::{
	CancellationToken,
	tools::edit_variants::{apply_patch::parse_apply_patch, patch::apply_inputs},
};

/// Upstream corpus, shared with `apply-patch.test.ts`.
const CORPUS: &str = "../../packages/coding-agent/test/fixtures/apply-patch/scenarios";

/// Corpus snapshot; a missing or extra case needs mapping review.
const EXPECTED_CASES: [&str; 21] = [
	"001_add_file",
	"003_multiple_chunks",
	"004_move_to_new_directory",
	"005_rejects_empty_patch",
	"006_rejects_missing_context",
	"007_rejects_missing_file_delete",
	"008_rejects_empty_update_hunk",
	"009_requires_existing_file_for_update",
	"010_move_rejects_existing_destination",
	"011_add_rejects_existing_file",
	"012_delete_directory_fails",
	"013_rejects_invalid_hunk_header",
	"014_update_file_appends_trailing_newline",
	"016_pure_addition_update_chunk",
	"017_whitespace_padded_hunk_header",
	"018_whitespace_padded_patch_markers",
	"019_unicode_simple",
	"020_delete_file_success",
	"020_whitespace_padded_patch_marker_lines",
	"021_update_file_deletion_only",
	"022_update_file_end_of_file_marker",
];

#[derive(Debug, PartialEq, Eq)]
enum Entry {
	Dir,
	File(Vec<u8>),
}

struct TempDir {
	path: PathBuf,
}

impl TempDir {
	fn new(label: &str) -> Self {
		static NEXT: AtomicUsize = AtomicUsize::new(0);
		loop {
			let path = std::env::temp_dir().join(format!(
				"pi-apply-patch-corpus-{label}-{}-{}",
				std::process::id(),
				NEXT.fetch_add(1, Ordering::SeqCst)
			));
			match fs::create_dir(&path) {
				Ok(()) => return Self { path },
				Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
				Err(err) => panic!("cannot create {}: {err}", path.display()),
			}
		}
	}

	fn path(&self) -> &Path {
		&self.path
	}
}

impl Drop for TempDir {
	fn drop(&mut self) {
		let _ = fs::remove_dir_all(&self.path);
	}
}

fn corpus_dir() -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS)
}

fn case_names(root: &Path) -> Vec<String> {
	let mut names = fs::read_dir(root)
		.unwrap_or_else(|err| panic!("corpus missing at {}: {err}", root.display()))
		.map(|entry| entry.expect("readable scenario entry"))
		.filter(|entry| entry.file_type().expect("scenario entry type").is_dir())
		.map(|entry| entry.file_name().to_string_lossy().into_owned())
		.collect::<Vec<_>>();
	names.sort();
	names
}

fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
	fs::create_dir_all(dst).map_err(|err| format!("create {}: {err}", dst.display()))?;
	for entry in fs::read_dir(src).map_err(|err| format!("read {}: {err}", src.display()))? {
		let entry = entry.map_err(|err| format!("read {}: {err}", src.display()))?;
		let source = entry.path();
		let target = dst.join(entry.file_name());
		let file_type = entry.file_type().map_err(|err| format!("stat {}: {err}", source.display()))?;
		if file_type.is_dir() {
			copy_tree(&source, &target)?;
		} else if file_type.is_file() {
			fs::copy(&source, &target)
				.map_err(|err| format!("copy {} -> {}: {err}", source.display(), target.display()))?;
		} else {
			return Err(format!("unsupported entry in corpus: {}", source.display()));
		}
	}
	Ok(())
}

/// Relative-path tree snapshot; directories included so empty dirs count.
fn snapshot(root: &Path) -> Result<BTreeMap<PathBuf, Entry>, String> {
	let mut entries = BTreeMap::new();
	if root.is_dir() {
		walk(root, Path::new(""), &mut entries)?;
	}
	Ok(entries)
}

fn walk(
	root: &Path,
	relative: &Path,
	entries: &mut BTreeMap<PathBuf, Entry>,
) -> Result<(), String> {
	let dir = root.join(relative);
	for entry in fs::read_dir(&dir).map_err(|err| format!("read {}: {err}", dir.display()))? {
		let entry = entry.map_err(|err| format!("read {}: {err}", dir.display()))?;
		let path = relative.join(entry.file_name());
		let absolute = root.join(&path);
		let file_type = entry
			.file_type()
			.map_err(|err| format!("stat {}: {err}", absolute.display()))?;
		if file_type.is_dir() {
			entries.insert(path.clone(), Entry::Dir);
			walk(root, &path, entries)?;
		} else if file_type.is_file() {
			let bytes = fs::read(&absolute)
				.map_err(|err| format!("read {}: {err}", absolute.display()))?;
			entries.insert(path, Entry::File(bytes));
		} else {
			return Err(format!("unsupported entry in tree: {}", absolute.display()));
		}
	}
	Ok(())
}

fn describe(entry: Option<&Entry>) -> String {
	match entry {
		None => "nothing".to_string(),
		Some(Entry::Dir) => "directory".to_string(),
		Some(Entry::File(bytes)) => format!("file of {} bytes", bytes.len()),
	}
}

fn first_difference(
	expected: &BTreeMap<PathBuf, Entry>,
	actual: &BTreeMap<PathBuf, Entry>,
) -> Option<String> {
	let paths: BTreeSet<&PathBuf> = expected.keys().chain(actual.keys()).collect();
	for path in paths {
		let (want, got) = (expected.get(path), actual.get(path));
		let detail = match (want, got) {
			(Some(Entry::Dir), Some(Entry::Dir)) => continue,
			(Some(Entry::File(want)), Some(Entry::File(got))) => {
				if want == got {
					continue;
				}
				let offset = want
					.iter()
					.zip(got)
					.position(|(a, b)| a != b)
					.unwrap_or_else(|| want.len().min(got.len()));
				format!(
					"expected {} bytes, actual {} bytes; first difference at byte {offset} (expected {:?}, actual {:?})",
					want.len(),
					got.len(),
					want.get(offset).copied(),
					got.get(offset).copied()
				)
			},
			_ => format!("expected {}, actual {}", describe(want), describe(got)),
		};
		return Some(format!("{}: {detail}", path.display()));
	}
	None
}

#[tokio::test]
async fn upstream_corpus_trees_match_after_apply() {
	let root = corpus_dir();
	assert!(root.is_dir(), "corpus missing at {}", root.display());

	let cases = case_names(&root);
	let discovered: Vec<&str> = cases.iter().map(String::as_str).collect();
	assert_eq!(
		discovered,
		EXPECTED_CASES,
		"corpus case set changed at {}",
		root.display()
	);

	let token = CancellationToken::new();
	for case in &cases {
		let case_dir = root.join(case);
		let patch_path = case_dir.join("patch.txt");
		let expected_dir = case_dir.join("expected");
		assert!(patch_path.is_file(), "case {case}: missing {}", patch_path.display());
		assert!(expected_dir.is_dir(), "case {case}: missing {}", expected_dir.display());

		let temp = TempDir::new(case);
		let input_dir = case_dir.join("input");
		if input_dir.is_dir() {
			copy_tree(&input_dir, temp.path()).unwrap_or_else(|err| panic!("case {case}: {err}"));
		}
		let initial = snapshot(temp.path()).unwrap_or_else(|err| panic!("case {case}: {err}"));
		let expected = snapshot(&expected_dir).unwrap_or_else(|err| panic!("case {case}: {err}"));
		let requires_change = initial != expected;

		let patch = fs::read_to_string(&patch_path)
			.unwrap_or_else(|err| panic!("case {case}: read {}: {err}", patch_path.display()));
		// Upstream swallows refusal errors; mutating cases must not error.
		let apply_error = match parse_apply_patch(&patch) {
			Ok(hunks) => apply_inputs(&hunks, temp.path(), &token, false)
				.await
				.err()
				.map(|err| err.to_string()),
			Err(err) => Some(err.to_string()),
		};
		let actual = snapshot(temp.path()).unwrap_or_else(|err| panic!("case {case}: {err}"));
		if requires_change
			&& let Some(error) = &apply_error
		{
			panic!("case {case} is a mutating case but apply failed: {error}");
		}
		if let Some(difference) = first_difference(&expected, &actual) {
			match &apply_error {
				Some(error) => panic!("case {case} diverged at {difference} (apply error: {error})"),
				None => panic!("case {case} diverged at {difference}"),
			}
		}
	}
}
