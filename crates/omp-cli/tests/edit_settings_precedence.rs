//! Public `edit` settings contract: `modelVariants` keeps raw keys for a
//! case-sensitive merge but matches case-insensitively in insertion order, and
//! `edit.mode` / `readLineNumbers` layer through `Config::merge`.

use std::path::Path;

use omp_cli::{
	config::{Config, EditMode, ModelVariants},
	yaml,
};

fn parse(source: &str) -> Config {
	let value = yaml::parse(source).expect("yaml parses");
	Config::from_value(&value, Path::new("settings.yml")).expect("config parses")
}

fn entries(config: &Config) -> &[(String, Option<String>)] {
	let ModelVariants::Entries(entries) = &config.edit.model_variants else {
		panic!("modelVariants entries: {:?}", config.edit.model_variants);
	};
	entries
}

#[test]
fn model_variants_match_the_first_inserted_pattern_case_insensitively() {
	let flash_first = parse("edit:\n  modelVariants:\n    flash: replace\n    gemini: sloppy\n");
	// "google/gemini-flash" contains both patterns; the first one authored wins.
	assert_eq!(
		flash_first.edit.variant_for_model("google/gemini-flash"),
		Some(EditMode::Replace)
	);

	let gemini_first = parse("edit:\n  modelVariants:\n    gemini: sloppy\n    flash: replace\n");
	assert_eq!(
		gemini_first.edit.variant_for_model("google/gemini-flash"),
		Some(EditMode::Sloppy),
		"insertion order is the precedence, not a sorted key order"
	);
	assert_eq!(gemini_first.edit.variant_for_model("local/other"), None);
}

#[test]
fn model_variants_merge_keeps_raw_keys_distinct_and_replaces_in_place() {
	let mut config =
		parse("edit:\n  modelVariants:\n    GPT: replace\n    claude: patch\n");
	config.merge(parse(
		"edit:\n  modelVariants:\n    gpt: sloppy\n    Claude: apply_patch\n    gemini: hashline\n",
	));

	// Case differs, so both layers keep their own entry: 2 base + 3 overlay.
	assert_eq!(entries(&config).len(), 5);
	assert_eq!(entries(&config)[0], ("GPT".to_string(), Some("replace".to_string())));
	assert_eq!(entries(&config)[1], ("claude".to_string(), Some("patch".to_string())));
	// Matching folds case, so the lowest-case-insensitive entry still wins.
	assert_eq!(
		config.edit.variant_for_model("openai/gpt-4"),
		Some(EditMode::Replace)
	);
	assert_eq!(
		config.edit.variant_for_model("anthropic/CLAUDE-3"),
		Some(EditMode::Patch)
	);
	assert_eq!(
		config.edit.variant_for_model("xai/gemini-pro"),
		Some(EditMode::Hashline)
	);

	// An exact raw key replaces the value where the lower layer put it.
	config.merge(parse("edit:\n  modelVariants:\n    GPT: sloppy\n"));
	assert_eq!(entries(&config)[0], ("GPT".to_string(), Some("sloppy".to_string())));
	assert_eq!(
		config.edit.variant_for_model("openai/gpt-4"),
		Some(EditMode::Sloppy)
	);
}

#[test]
fn model_variants_keep_positions_until_filtered_and_non_objects_clear() {
	// Invalid in the lower layer, repaired by the overlay: the raw key keeps its
	// position, so `m` still wins the first match for "m-gpt-4".
	let mut repaired = parse("edit:\n  modelVariants:\n    m: bogus\n    gpt: patch\n");
	repaired.merge(parse("edit:\n  modelVariants:\n    m: replace\n"));
	assert_eq!(
		repaired.edit.variant_for_model("m-gpt-4"),
		Some(EditMode::Replace)
	);
	assert_eq!(
		repaired.edit.variant_for_model("gpt-4"),
		Some(EditMode::Patch)
	);

	// A non-object overlay replaces the inherited group instead of preserving it.
	let mut cleared = parse("edit:\n  modelVariants:\n    gpt: patch\n");
	cleared.merge(parse("edit:\n  modelVariants: false\n"));
	assert_eq!(cleared.edit.model_variants, ModelVariants::NotAMap);
	assert_eq!(cleared.edit.variant_for_model("gpt-4"), None);
}

#[test]
fn edit_mode_and_read_line_numbers_layer_independently() {
	let mut base = parse("edit:\n  mode: sloppy\nreadLineNumbers: true\n");
	assert_eq!(base.edit.mode(), EditMode::Sloppy);
	assert!(base.read_line_numbers());

	// An overlay that only carries one leaf leaves the other untouched.
	base.merge(parse("readLineNumbers: false\n"));
	assert_eq!(base.edit.mode(), EditMode::Sloppy);
	assert!(!base.read_line_numbers());

	base.merge(parse("edit:\n  mode: apply_patch\n"));
	assert_eq!(base.edit.mode(), EditMode::ApplyPatch);
	assert!(!base.read_line_numbers());
}

#[test]
fn legacy_atom_and_vim_modes_migrate_and_unknown_modes_fail_the_load() {
	assert_eq!(parse("edit:\n  mode: atom\n").edit.mode(), EditMode::Hashline);
	assert_eq!(parse("edit:\n  mode: vim\n").edit.mode(), EditMode::Hashline);
	assert_eq!(parse("\"edit.mode\": atom\n").edit.mode(), EditMode::Hashline);
	assert_eq!(
		parse("edit:\n  modelVariants:\n    atom-model: atom\n")
			.edit
			.variant_for_model("atom-model-1"),
		Some(EditMode::Hashline)
	);

	let value = yaml::parse("edit:\n  mode: bogus\n").expect("yaml parses");
	assert!(
		Config::from_value(&value, Path::new("settings.yml")).is_err(),
		"an unknown edit.mode is a load error, not a silent default"
	);
}
