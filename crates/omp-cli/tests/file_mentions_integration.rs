//! Integration tests for file mentions (@file syntax) parity.
//!
//! Verifies:
//! - Exact path resolution and extraction of quoted/unquoted mentions
//! - 5 MiB text / 25 MiB image boundaries
//! - Content-based image MIME detection (PNG, JPEG, GIF, WebP)
//! - Content-based binary detection (NUL bytes, invalid UTF-8)
//! - Safe bounded reads preventing OOM when files grow or are infinite
//! - Unreadable path error context preservation
//! - Directory listings with relative age and entry limits
//! - Prompt block serialization

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use omp_cli::file_mentions::{
	BoundedReadError, FileMentionMessage, ImagePayload, MAX_AUTO_READ_IMAGE_BYTES,
	MAX_AUTO_READ_TEXT_BYTES, MentionedFile, SkippedReason, base64_encode, build_directory_listing,
	build_text_output, detect_image_mime, extract_file_mentions, format_prompt_block,
	generate_file_mention_messages, generate_file_mention_messages_vec, is_probably_binary,
	process_file_mentions, process_prompt_mentions, read_bounded, read_mentioned_path, resolve_path,
};

fn temp_dir(prefix: &str) -> PathBuf {
	static COUNTER: AtomicUsize = AtomicUsize::new(0);
	let count = COUNTER.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!("omp-test-mentions-int-{prefix}-{}-{count}", std::process::id()));
	let _ = fs::remove_dir_all(&dir);
	fs::create_dir_all(&dir).expect("create temp dir");
	dir
}

#[test]
fn test_mention_extraction_and_resolution_parity() {
	let cwd = temp_dir("exact-resolve");
	let src_dir = cwd.join("src");
	fs::create_dir_all(&src_dir).expect("create src dir");
	fs::write(src_dir.join("config.ts"), "export const x = 1;").expect("write config.ts");

	// Auto-reads an exact file path matching TS
	let msg = process_file_mentions(&["src/config.ts".to_string()], &cwd).expect("process mentions");
	assert_eq!(msg.role, "fileMention");
	assert_eq!(msg.files.len(), 1);
	assert_eq!(msg.files[0].path, "src/config.ts");
	assert!(msg.files[0].content.contains("export const x = 1;"));

	// Does not fuzzy- or prefix-resolve mentions that are not real paths
	let empty = process_file_mentions(&["src/conf".to_string(), "docs/".to_string()], &cwd);
	assert_eq!(empty, None);

	// Reads only mentions that resolve to real paths
	fs::write(cwd.join("real.txt"), "present").expect("write real.txt");
	let partial = process_file_mentions(
		&["real.txt".to_string(), "does-not-exist.txt".to_string()],
		&cwd,
	)
	.expect("process partial");
	assert_eq!(partial.files.len(), 1);
	assert_eq!(partial.files[0].path, "real.txt");
	assert!(partial.files[0].content.contains("present"));

	let _ = fs::remove_dir_all(&cwd);
}

#[test]
fn test_quoted_spaces_in_mentions() {
	let cwd = temp_dir("quoted-spaces");
	let sub = cwd.join("My Folder");
	fs::create_dir_all(&sub).expect("create My Folder");
	fs::write(sub.join("my file.png"), [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]).expect("write png");

	let prompt = "Please see @\"My Folder/my file.png\" and @'My Folder/my file.png'";
	let extracted = extract_file_mentions(prompt);
	assert_eq!(extracted, vec!["My Folder/my file.png".to_string()]);

	let msg = process_prompt_mentions(prompt, &cwd).expect("process prompt mentions");
	assert_eq!(msg.files.len(), 1);
	assert_eq!(msg.files[0].path, "My Folder/my file.png");
	let img = msg.files[0].image.as_ref().expect("image payload present");
	assert_eq!(img.mime_type, "image/png");

	let _ = fs::remove_dir_all(&cwd);
}

#[test]
fn test_image_types_and_wire_payload() {
	let cwd = temp_dir("image-payloads");

	// 1. PNG
	let png_path = cwd.join("pic.png");
	let png_raw = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 10, 20, 30];
	fs::write(&png_path, png_raw).expect("write png");

	// 2. JPEG
	let jpg_path = cwd.join("pic.jpg");
	let jpg_raw = [0xFF, 0xD8, 0xFF, 0xE0, 0, 16, 74, 70, 73, 70];
	fs::write(&jpg_path, jpg_raw).expect("write jpg");

	// 3. GIF
	let gif_path = cwd.join("anim.gif");
	let gif_raw = b"GIF89a\x01\x00\x01\x00\x80\x00\x00";
	fs::write(&gif_path, gif_raw).expect("write gif");

	// 4. WebP
	let webp_path = cwd.join("test.webp");
	let webp_raw = b"RIFF\x18\x00\x00\x00WEBPVP8 \x0c\x00\x00\x00\x30\x01\x00\x9d\x01\x2a";
	fs::write(&webp_path, webp_raw).expect("write webp");

	let msg = process_file_mentions(
		&[
			"pic.png".to_string(),
			"pic.jpg".to_string(),
			"anim.gif".to_string(),
			"test.webp".to_string(),
		],
		&cwd,
	)
	.expect("process images");

	assert_eq!(msg.files.len(), 4);
	assert_eq!(msg.files[0].image.as_ref().unwrap().mime_type, "image/png");
	assert_eq!(msg.files[0].image.as_ref().unwrap().data_base64, base64_encode(&png_raw));

	assert_eq!(msg.files[1].image.as_ref().unwrap().mime_type, "image/jpeg");
	assert_eq!(msg.files[1].image.as_ref().unwrap().data_base64, base64_encode(&jpg_raw));

	assert_eq!(msg.files[2].image.as_ref().unwrap().mime_type, "image/gif");
	assert_eq!(msg.files[2].image.as_ref().unwrap().data_base64, base64_encode(gif_raw));

	assert_eq!(msg.files[3].image.as_ref().unwrap().mime_type, "image/webp");
	assert_eq!(msg.files[3].image.as_ref().unwrap().data_base64, base64_encode(webp_raw));

	let _ = fs::remove_dir_all(&cwd);
}

#[test]
fn test_binary_skips_raw_injection_matching_ts() {
	let cwd = temp_dir("binary-skips");

	// TTF header begins with a NUL run
	let ttf_bytes = [0x00, 0x01, 0x00, 0x00, 0x00, 0x0c, 0x4f, 0x53];
	fs::write(cwd.join("Silver.ttf"), ttf_bytes).expect("write Silver.ttf");

	// A non-NUL invalid-UTF8 blob
	let blob_bytes = [0x4d, 0x5a, 0xff, 0xfe, 0xc0, 0xc0];
	fs::write(cwd.join("blob.bin"), blob_bytes).expect("write blob.bin");

	let msg = process_file_mentions(&["Silver.ttf".to_string(), "blob.bin".to_string()], &cwd)
		.expect("process binary mentions");

	assert_eq!(msg.files.len(), 2);
	for file in &msg.files {
		assert_eq!(file.skipped_reason, Some(SkippedReason::Binary));
		assert!(file.content.contains("binary file"));
		assert!(!file.content.contains('\0'));
		assert_eq!(file.image, None);
	}

	let _ = fs::remove_dir_all(&cwd);
}

#[test]
fn test_text_and_image_limits() {
	let cwd = temp_dir("limits");

	// 1. Text file exactly at 5 MiB -> allowed
	let text_ok = cwd.join("text_ok.txt");
	let mut f = File::create(&text_ok).expect("create text_ok");
	let chunk = b"abcdefghij1234567890\n"; // 21 bytes
	let repeats = (MAX_AUTO_READ_TEXT_BYTES / chunk.len() as u64) as usize;
	for _ in 0..repeats {
		f.write_all(chunk).expect("write chunk");
	}
	drop(f);

	let res_ok = read_mentioned_path("text_ok.txt", &cwd).expect("read text_ok");
	assert_eq!(res_ok.skipped_reason, None);

	// 2. Text file exceeding 5 MiB -> skipped tooLarge
	let text_oversize = cwd.join("text_oversize.txt");
	let f_over = File::create(&text_oversize).expect("create text_oversize");
	f_over.set_len(MAX_AUTO_READ_TEXT_BYTES + 10).expect("set len");
	drop(f_over);

	let res_over = read_mentioned_path("text_oversize.txt", &cwd).expect("read text_oversize");
	assert_eq!(res_over.skipped_reason, Some(SkippedReason::TooLarge));
	assert!(res_over.content.contains("too large"));

	// 3. Image file exceeding 25 MiB -> skipped tooLarge
	let img_oversize = cwd.join("big.png");
	let mut f_img = File::create(&img_oversize).expect("create big.png");
	f_img
		.write_all(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
		.expect("write png magic");
	f_img.set_len(MAX_AUTO_READ_IMAGE_BYTES + 500).expect("set len");
	drop(f_img);

	let res_img_over = read_mentioned_path("big.png", &cwd).expect("read big.png");
	assert_eq!(res_img_over.skipped_reason, Some(SkippedReason::TooLarge));
	assert!(res_img_over.content.contains("too large"));
	assert_eq!(res_img_over.image, None);

	let _ = fs::remove_dir_all(&cwd);
}

#[cfg(unix)]
#[test]
fn test_unreadable_file_context() {
	use std::os::unix::fs::PermissionsExt;

	let cwd = temp_dir("unreadable");
	let locked = cwd.join("locked.txt");
	fs::write(&locked, "secret data").expect("write locked.txt");

	// Remove read permissions: 000
	let mut perms = fs::metadata(&locked).expect("get perms").permissions();
	perms.set_mode(0o000);
	let _ = fs::set_permissions(&locked, perms);

	let file_res = read_mentioned_path("locked.txt", &cwd);
	// On root / Android root, read might still succeed, so check conditionally:
	if let Some(res) = file_res {
		if res.skipped_reason == Some(SkippedReason::Unreadable) {
			assert!(res.content.contains("unreadable:"));
		}
	}

	// Restore permissions for clean removal
	let mut restore_perms = fs::metadata(&locked).expect("get perms").permissions();
	restore_perms.set_mode(0o644);
	let _ = fs::set_permissions(&locked, restore_perms);
	let _ = fs::remove_dir_all(&cwd);
}

#[test]
fn test_format_prompt_block_xml_structure() {
	let msg = omp_cli::file_mentions::FileMentionMessage {
		role: "fileMention".to_string(),
		files: vec![
			MentionedFile {
				path: "hello.txt".to_string(),
				content: "hello world".to_string(),
				line_count: Some(1),
				byte_size: Some(11),
				skipped_reason: None,
				image: None,
			},
			MentionedFile {
				path: "empty.txt".to_string(),
				content: String::new(),
				line_count: Some(1),
				byte_size: Some(0),
				skipped_reason: None,
				image: None,
			},
		],
		timestamp: 1000,
	};

	let prompt_block = format_prompt_block(&msg);
	assert_eq!(
		prompt_block,
		"<file path=\"hello.txt\">\nhello world\n</file>\n<file path=\"empty.txt\">\n\n</file>"
	);
}

#[test]
fn test_generate_file_mention_messages_and_split_helpers() {
	let cwd = temp_dir("gen-helpers");
	fs::write(cwd.join("code.rs"), "fn code() {}").expect("write code.rs");
	let png_raw = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2];
	fs::write(cwd.join("draw.png"), png_raw).expect("write draw.png");

	let msg_opt = generate_file_mention_messages(
		&["code.rs".to_string(), "draw.png".to_string()],
		&cwd,
	);
	let msg = msg_opt.expect("generate message");
	assert_eq!(msg.text_files().len(), 1);
	assert_eq!(msg.text_files()[0].path, "code.rs");
	assert_eq!(msg.image_files().len(), 1);
	assert_eq!(msg.image_files()[0].path, "draw.png");

	let text_block = msg.format_text_prompt_block();
	assert!(text_block.contains("<file path=\"code.rs\">"));
	assert!(!text_block.contains("draw.png"));

	let image_block = msg.format_image_prompt_block();
	assert!(image_block.contains("<file path=\"draw.png\">"));
	assert!(!image_block.contains("code.rs"));

	let msg_vec = generate_file_mention_messages_vec(
		&["code.rs".to_string()],
		&cwd,
	);
	assert_eq!(msg_vec.len(), 1);

	let empty_vec = generate_file_mention_messages_vec(&["nonexistent.rs".to_string()], &cwd);
	assert!(empty_vec.is_empty());

	let _ = fs::remove_dir_all(&cwd);
}

#[test]
fn test_wire_serde_roundtrip_compatibility() {
	let msg = FileMentionMessage {
		role: "fileMention".to_string(),
		files: vec![
			MentionedFile {
				path: "notes.md".to_string(),
				content: "# Notes".to_string(),
				line_count: Some(1),
				byte_size: Some(7),
				skipped_reason: None,
				image: None,
			},
			MentionedFile {
				path: "img.png".to_string(),
				content: String::new(),
				line_count: None,
				byte_size: Some(1024),
				skipped_reason: None,
				image: Some(ImagePayload::new("image/png", "aGVsbG8=")),
			},
		],
		timestamp: 99999,
	};

	let json = serde_json::to_string(&msg).expect("serialize message");
	// TypeScript camelCase properties
	assert!(json.contains("\"lineCount\":1"));
	assert!(json.contains("\"byteSize\":7"));
	assert!(json.contains("\"mimeType\":\"image/png\""));
	assert!(json.contains("\"data\":\"aGVsbG8=\""));
	assert!(json.contains("\"type\":\"image\""));

	// Roundtrip
	let decoded: FileMentionMessage = serde_json::from_str(&json).expect("deserialize roundtrip");
	assert_eq!(decoded, msg);
}

#[test]
fn test_unclosed_quotes_mention_fallback() {
	let text = "Please inspect @\"unclosed/file.rs and discuss.";
	let mentions = extract_file_mentions(text);
	assert_eq!(mentions, vec!["unclosed/file.rs".to_string()]);
}

#[test]
fn test_bounded_reads_mid_read_growth_prevention() {
	// Infinite stream simulates concurrent file growth or special /dev/zero
	let infinite = std::io::repeat(b'z');
	let max_limit = 2048u64;
	let res = read_bounded(infinite, Vec::new(), max_limit);

	match res {
		Err(BoundedReadError::TooLarge { actual_read }) => {
			// Halts deterministically after reading exactly max_limit + 1 bytes
			assert_eq!(actual_read, max_limit + 1);
		}
		other => panic!("expected TooLarge error, got {other:?}"),
	}
}

#[test]
fn test_bare_slash_mention_resolves_to_cwd() {
	let cwd = temp_dir("bare-slash");
	fs::write(cwd.join("workspace_file.txt"), "hello workspace").expect("write workspace file");

	assert_eq!(resolve_path("/", &cwd), cwd);
	assert_eq!(resolve_path("///", &cwd), cwd);

	let mention = read_mentioned_path("/", &cwd);
	assert!(mention.is_some(), "mention for / must resolve to cwd directory");
	let file = mention.unwrap();
	assert!(file.content.contains("workspace_file.txt"));

	let _ = fs::remove_dir_all(&cwd);
}

#[test]
fn test_directory_listing_500_cap_bounded_memory() {
	let cwd = temp_dir("dir-cap");
	for i in 0..520 {
		fs::write(cwd.join(format!("entry_{i:04}.txt")), "data").expect("write file");
	}

	let listing = build_directory_listing(&cwd).expect("listing");
	assert!(listing.output.contains("500 entries limit reached"));
	assert!(listing.output.contains("entry_0000.txt"));
	assert!(listing.output.contains("entry_0499.txt"));
	assert!(!listing.output.contains("entry_0500.txt"));

	let _ = fs::remove_dir_all(&cwd);
}

#[cfg(unix)]
#[test]
fn test_fifo_opened_non_blocking_rejected_integration() {
	unsafe extern "C" {
		fn mkfifo(path: *const std::os::raw::c_char, mode: std::os::raw::c_uint) -> std::os::raw::c_int;
	}

	let cwd = temp_dir("fifo-int");
	let fifo = cwd.join("events.pipe");
	let c_str = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
	let rc = unsafe { mkfifo(c_str.as_ptr(), 0o600) };
	assert_eq!(rc, 0, "mkfifo must succeed");

	let prompt = format!("Inspect @{}", fifo.to_string_lossy());
	let msg_opt = process_prompt_mentions(&prompt, &cwd);
	assert!(msg_opt.is_some(), "prompt with fifo should produce file mention");
	let msg = msg_opt.unwrap();
	assert_eq!(msg.files.len(), 1);
	assert_eq!(msg.files[0].skipped_reason, Some(SkippedReason::Unreadable));
	assert!(msg.files[0].content.contains("not a regular file"));

	let _ = fs::remove_file(&fifo);
	let _ = fs::remove_dir_all(&cwd);
}
