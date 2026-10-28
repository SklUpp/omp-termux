//! Headless `ask` tool.
//!
//! Mirrors `packages/coding-agent/src/tools/ask.ts`: schema validation (reserved
//! option labels, multi-select, recommended defaults), exclusive execution, and
//! LLM-facing answer formatting. The interactive surface is supplied by the CLI
//! through [`AskChannel`]; with no channel (headless runs, child agents) the tool
//! aborts exactly like the TypeScript `ToolAbortError` path and never reads
//! stdin.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{Tool, ToolError, tool_result};
use crate::state::{AgentContext, ToolResult};

pub const OTHER_OPTION: &str = "Other (type your own)";
pub const CHAT_ABOUT_THIS_OPTION: &str = "Chat about this";
pub const NEXT_OPTION: &str = "Next →";
pub const RECOMMENDED_SUFFIX: &str = " (Recommended)";

/// Labels the runtime adds itself; a model option may not collide with them.
pub const RESERVED_OPTION_LABELS: [&str; 3] =
	[OTHER_OPTION, CHAT_ABOUT_THIS_OPTION, NEXT_OPTION];

const DESCRIPTION: &str = "Ask user for clarification/input during task execution.

<conditions>
- Multiple approaches with significantly different tradeoffs user should weigh.
</conditions>

<instruction>
- `recommended: <index>` marks default (0-indexed); \" (Recommended)\" added automatically.
- Use `questions` for related questions, not one at a time.
- Set `multi: true` on a question to allow multiple selections.
- Short option labels; explanatory tradeoffs in `description`, not labels.
</instruction>

<caution>
- Provide 2-5 concise, distinct options.
</caution>

<critical>
- Default to action. Resolve ambiguity via repo conventions, existing patterns, reasonable defaults. Exhaust existing sources (code, configs, docs, history) before asking. Ask only when options have materially different tradeoffs the user must decide.
- If multiple choices acceptable: pick most conservative/standard option; proceed; state choice.
- Do NOT include \"Other\"; UI automatically adds \"Other (type your own)\" to every question.
</critical>";

/// One selectable answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskOption {
	pub label: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub description: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub preview: Option<String>,
}

/// A normalized question handed to the interactive surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskQuestion {
	pub id: String,
	pub question: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub header: Option<String>,
	pub options: Vec<AskOption>,
	#[serde(default)]
	pub multi: bool,
	/// 0-based index; out-of-range values are ignored by the UI.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub recommended: Option<i64>,
}

/// Answer for one question.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskAnswer {
	pub id: String,
	#[serde(default)]
	pub selected_options: Vec<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub custom_input: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub note: Option<String>,
	#[serde(default)]
	pub timed_out: bool,
}

/// Outcome of one interactive prompt sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskOutcome {
	Answered(Vec<AskAnswer>),
	/// The user chose "Chat about this" instead of answering.
	ChatRedirect,
	/// The user dismissed the prompt (Esc / EOF / `q`).
	Cancelled,
	/// The turn or tool was cancelled externally (Ctrl-C, parent abort).
	Aborted,
}

/// Interactive question surface. One implementation lives in the CLI; tests and
/// embedders can provide their own.
#[async_trait]
pub trait AskChannel: Send + Sync {
	async fn ask(&self, questions: &[AskQuestion], cancel: CancellationToken) -> AskOutcome;
}

/// Result of one question as persisted in `details.results`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct QuestionResultDetails {
	id: String,
	question: String,
	options: Vec<String>,
	multi: bool,
	selected_options: Vec<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	custom_input: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	note: Option<String>,
	#[serde(skip_serializing_if = "is_false")]
	timed_out: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SingleQuestionDetails {
	question: String,
	options: Vec<String>,
	multi: bool,
	selected_options: Vec<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	custom_input: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	note: Option<String>,
	#[serde(skip_serializing_if = "is_false")]
	timed_out: bool,
}

#[derive(Debug, Clone, Serialize)]
struct MultiQuestionDetails {
	results: Vec<QuestionResultDetails>,
}

fn is_false(value: &bool) -> bool {
	!*value
}

#[derive(Debug, Deserialize)]
struct RawOption {
	label: String,
	#[serde(default)]
	description: Option<String>,
	#[serde(default)]
	preview: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawQuestion {
	id: String,
	question: String,
	#[serde(default)]
	header: Option<String>,
	options: Vec<RawOption>,
	#[serde(default)]
	multi: Option<bool>,
	#[serde(default)]
	recommended: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RawInput {
	questions: Vec<RawQuestion>,
}

fn normalize_questions(raw: Vec<RawQuestion>) -> Result<Vec<AskQuestion>, ToolError> {
	let mut questions = Vec::with_capacity(raw.len());
	for question in raw {
		let mut options = Vec::with_capacity(question.options.len());
		for option in question.options {
			if RESERVED_OPTION_LABELS.contains(&option.label.as_str()) {
				return Err(ToolError::InvalidArguments(format!(
					"question \"{}\": option label \"{}\" is reserved by the runtime",
					question.id, option.label
				)));
			}
			let description = option
				.description
				.map(|text| text.trim().to_string())
				.filter(|text| !text.is_empty());
			let preview = option
				.preview
				.filter(|text| !text.trim().is_empty());
			options.push(AskOption {
				label: option.label,
				description,
				preview,
			});
		}
		let header = question
			.header
			.filter(|text| !text.trim().is_empty());
		let recommended = question.recommended.and_then(|value| {
			(value.is_finite() && value.fract() == 0.0).then_some(value as i64)
		});
		questions.push(AskQuestion {
			id: question.id,
			question: question.question,
			header,
			options,
			multi: question.multi.unwrap_or(false),
			recommended,
		});
	}
	Ok(questions)
}

pub fn parse_questions(args: &Value) -> Result<Vec<AskQuestion>, ToolError> {
	let raw: RawInput = serde_json::from_value(args.clone())
		.map_err(|err| ToolError::InvalidArguments(err.to_string()))?;
	normalize_questions(raw.questions)
}

/// Add the recommended suffix unless the label already carries it. Out-of-range
/// or negative indices are ignored, matching the TypeScript UI.
#[must_use]
pub fn display_label(option: &AskOption, index: usize, recommended: Option<i64>) -> String {
	if recommended == Some(index as i64) && !option.label.ends_with(RECOMMENDED_SUFFIX) {
		format!("{}{RECOMMENDED_SUFFIX}", option.label)
	} else {
		option.label.clone()
	}
}

#[must_use]
pub fn strip_recommended_suffix(label: &str) -> &str {
	label.strip_suffix(RECOMMENDED_SUFFIX).unwrap_or(label)
}

/// Timeout/auto default: the recommended option when in range, else the first.
#[must_use]
pub fn auto_selection(options: &[AskOption], recommended: Option<i64>) -> Vec<String> {
	if options.is_empty() {
		return Vec::new();
	}
	let index = recommended
		.filter(|value| *value >= 0 && (*value as usize) < options.len())
		.map_or(0, |value| value as usize);
	vec![options[index].label.clone()]
}

fn format_single_question_response(answer: &AskAnswer, multi: bool) -> String {
	let mut parts = Vec::new();
	if !answer.selected_options.is_empty() {
		let selected = if multi {
			format!("User selected: {}", answer.selected_options.join(", "))
		} else {
			format!("User selected: {}", answer.selected_options[0])
		};
		parts.push(if answer.timed_out {
			format!("{selected} (auto-selected after timeout)")
		} else {
			selected
		});
	}
	if let Some(custom) = answer.custom_input.as_deref().filter(|custom| !custom.is_empty()) {
		if custom.contains('\n') {
			let indented = custom
				.lines()
				.map(|line| format!("  {line}"))
				.collect::<Vec<_>>()
				.join("\n");
			parts.push(format!("User provided custom input:\n{indented}"));
		} else {
			parts.push(format!("User provided custom input: {custom}"));
		}
	}
	if let Some(note) = answer.note.as_deref().filter(|note| !note.is_empty()) {
		if note.contains('\n') {
			let indented = note
				.lines()
				.map(|line| format!("  {line}"))
				.collect::<Vec<_>>()
				.join("\n");
			parts.push(format!("User added note:\n{indented}"));
		} else {
			parts.push(format!("User added note: {note}"));
		}
	}
	if parts.is_empty() {
		if multi {
			"User did not select any options".to_string()
		} else {
			"User cancelled the selection".to_string()
		}
	} else {
		parts.join("\n")
	}
}

fn format_question_result(result: &QuestionResultDetails) -> String {
	let note = result
		.note
		.as_deref()
		.filter(|note| !note.is_empty())
		.map_or_else(String::new, |note| format!(" (note: {note})"));
	if let Some(custom) = result.custom_input.as_deref().filter(|custom| !custom.is_empty()) {
		return format!("{}: \"{custom}\"{note}", result.id);
	}
	if result.selected_options.is_empty() {
		return if result.multi {
			format!("{}: []{note}", result.id)
		} else {
			format!("{}: (cancelled){note}", result.id)
		};
	}
	let suffix = format!(
		"{}{note}",
		if result.timed_out { " (auto-selected after timeout)" } else { "" }
	);
	if result.multi {
		format!("{}: [{}]{suffix}", result.id, result.selected_options.join(", "))
	} else {
		format!("{}: {}{suffix}", result.id, result.selected_options[0])
	}
}

fn question_result_details(
	question: &AskQuestion,
	answer: &AskAnswer,
) -> QuestionResultDetails {
	QuestionResultDetails {
		id: question.id.clone(),
		question: question.question.clone(),
		options: question.options.iter().map(|option| option.label.clone()).collect(),
		multi: question.multi,
		selected_options: answer.selected_options.clone(),
		custom_input: answer.custom_input.clone(),
		note: answer.note.clone(),
		timed_out: answer.timed_out,
	}
}

/// Interactive user prompting during execution.
#[derive(Debug, Default)]
pub struct AskTool;

impl AskTool {
	#[must_use]
	pub fn new() -> Self {
		Self
	}
}

#[async_trait]
impl Tool for AskTool {
	fn name(&self) -> &'static str {
		"ask"
	}

	fn description(&self) -> &'static str {
		DESCRIPTION
	}

	fn schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"questions": {
					"type": "array",
					"minItems": 1,
					"description": "questions to ask",
					"items": {
						"type": "object",
						"properties": {
							"id": { "type": "string", "description": "question id" },
							"question": { "type": "string", "description": "question text" },
							"header": { "type": "string", "description": "optional short display chip for rich ask dialogs" },
							"options": {
								"type": "array",
								"description": "available options",
								"items": {
									"type": "object",
									"properties": {
										"label": { "type": "string", "description": "display label" },
										"description": { "type": "string", "description": "optional explanatory text displayed below the label" },
										"preview": { "type": "string", "description": "optional rich preview content for interactive ask dialogs" }
									},
									"required": ["label"]
								}
							},
							"multi": { "type": "boolean", "description": "allow multiple selections" },
							"recommended": { "type": "number", "description": "recommended option index" }
						},
						"required": ["id", "question", "options"]
					}
				}
			},
			"required": ["questions"],
			"additionalProperties": false
		})
	}

	fn is_exclusive(&self) -> bool {
		true
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let questions = parse_questions(args)?;
		if questions.is_empty() {
			return Ok(tool_result("Error: questions must not be empty", false));
		}

		let Some(channel) = ctx.ask.as_ref() else {
			ctx.cancellation_token.cancel();
			return Err(ToolError::Aborted(
				"Ask tool requires interactive mode".to_string(),
			));
		};
		if ctx.cancellation_token.is_cancelled() {
			return Err(ToolError::Aborted("Ask input was cancelled".to_string()));
		}

		let outcome = channel.ask(&questions, ctx.cancellation_token.clone()).await;
		match outcome {
			AskOutcome::Cancelled => {
				ctx.cancellation_token.cancel();
				Err(ToolError::Aborted(
					"Ask tool was cancelled by the user".to_string(),
				))
			},
			AskOutcome::Aborted => {
				ctx.cancellation_token.cancel();
				Err(ToolError::Aborted("Ask input was cancelled".to_string()))
			},
			AskOutcome::ChatRedirect => {
				let question_text = questions
					.iter()
					.map(|question| question.question.as_str())
					.collect::<Vec<_>>()
					.join("\n");
				let mut result = tool_result(
					format!(
						"User chose to chat about this instead of answering.\n\nQuestions asked:\n{question_text}"
					),
					false,
				);
				result.details = Some(json!({
					"chatRedirect": true,
					"questions": questions.iter().map(|q| q.question.clone()).collect::<Vec<_>>(),
				}));
				Ok(result)
			},
			AskOutcome::Answered(answers) => {
				if answers.len() != questions.len() {
					return Err(ToolError::ExecutionFailed(
						"Ask dialog returned a result count that does not match the requested questions"
							.to_string(),
					));
				}
				for (question, answer) in questions.iter().zip(&answers) {
					if answer.id != question.id {
						return Err(ToolError::ExecutionFailed(
							"Ask dialog returned results that do not match the requested question order"
								.to_string(),
						));
					}
				}

				if questions.len() == 1 {
					let question = &questions[0];
					let answer = &answers[0];
					if !answer.timed_out
						&& answer.selected_options.is_empty()
						&& answer.custom_input.is_none()
						&& !question.multi
					{
						ctx.cancellation_token.cancel();
						return Err(ToolError::Aborted(
							"Ask tool was cancelled by the user".to_string(),
						));
					}
					let details = SingleQuestionDetails {
						question: question.question.clone(),
						options: question.options.iter().map(|option| option.label.clone()).collect(),
						multi: question.multi,
						selected_options: answer.selected_options.clone(),
						custom_input: answer.custom_input.clone(),
						note: answer.note.clone(),
						timed_out: answer.timed_out,
					};
					let mut result = tool_result(
						format_single_question_response(answer, question.multi),
						false,
					);
					result.details = serde_json::to_value(details).ok();
					return Ok(result);
				}

				let results: Vec<QuestionResultDetails> = questions
					.iter()
					.zip(&answers)
					.map(|(question, answer)| question_result_details(question, answer))
					.collect();
				let response = format!(
					"User answers:\n{}",
					results
						.iter()
						.map(format_question_result)
						.collect::<Vec<_>>()
						.join("\n")
				);
				let mut result = tool_result(response, false);
				result.details = serde_json::to_value(MultiQuestionDetails { results }).ok();
				Ok(result)
			},
		}
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use super::*;

	struct ScriptedAsk {
		outcome: AskOutcome,
		seen: std::sync::Mutex<Option<Vec<AskQuestion>>>,
	}

	#[async_trait]
	impl AskChannel for ScriptedAsk {
		async fn ask(&self, questions: &[AskQuestion], _cancel: CancellationToken) -> AskOutcome {
			*self.seen.lock().unwrap() = Some(questions.to_vec());
			self.outcome.clone()
		}
	}

	fn ctx_with(outcome: AskOutcome) -> (AgentContext, Arc<ScriptedAsk>) {
		let channel = Arc::new(ScriptedAsk {
			outcome,
			seen: std::sync::Mutex::new(None),
		});
		let mut ctx = AgentContext::new(std::env::temp_dir());
		ctx.ask = Some(channel.clone() as Arc<dyn AskChannel>);
		(ctx, channel)
	}

	fn args() -> Value {
		json!({
			"questions": [{
				"id": "q1",
				"question": "Pick one",
				"options": [
					{"label": "A", "description": " first "},
					{"label": "B", "preview": "preview text"}
				],
				"recommended": 99
			}]
		})
	}

	#[tokio::test]
	async fn parses_schema_and_normalizes_options() {
		let questions = parse_questions(&args()).expect("schema");
		assert_eq!(questions.len(), 1);
		assert_eq!(questions[0].options[0].description.as_deref(), Some("first"));
		assert_eq!(questions[0].options[1].preview.as_deref(), Some("preview text"));
		assert_eq!(questions[0].recommended, Some(99));

		let multi = parse_questions(&json!({
			"questions": [{"id": "q", "question": "Q", "options": [], "multi": true}]
		}))
		.expect("multi");
		assert!(multi[0].multi);
	}

	#[tokio::test]
	async fn rejects_reserved_labels_and_bad_shapes() {
		for label in RESERVED_OPTION_LABELS {
			let err = parse_questions(&json!({
				"questions": [{"id": "q", "question": "Q", "options": [{"label": label}]}]
			}))
			.expect_err("reserved label");
			assert!(matches!(err, ToolError::InvalidArguments(_)), "{err}");
		}
		assert!(matches!(
			parse_questions(&json!({"questions": "nope"})),
			Err(ToolError::InvalidArguments(_))
		));
		assert!(matches!(
			parse_questions(&json!({"questions": [{"id": "q"}]})),
			Err(ToolError::InvalidArguments(_))
		));
	}

	#[test]
	fn recommended_suffix_and_timeout_selection_match_upstream() {
		let options = vec![
			AskOption { label: "A".into(), ..AskOption::default() },
			AskOption { label: "B".into(), ..AskOption::default() },
		];
		assert_eq!(display_label(&options[0], 0, Some(0)), "A (Recommended)");
		assert_eq!(display_label(&options[1], 1, Some(0)), "B");
		assert_eq!(display_label(&options[0], 0, Some(7)), "A");
		assert_eq!(display_label(&options[0], 0, Some(-1)), "A");
		assert_eq!(auto_selection(&options, Some(1)), vec!["B".to_string()]);
		assert_eq!(auto_selection(&options, Some(7)), vec!["A".to_string()]);
		assert_eq!(auto_selection(&options, Some(-1)), vec!["A".to_string()]);
		assert!(auto_selection(&[], Some(0)).is_empty());
		let already = AskOption {
			label: "A (Recommended)".into(),
			..AskOption::default()
		};
		assert_eq!(display_label(&already, 0, Some(0)), "A (Recommended)");
		assert_eq!(strip_recommended_suffix("A (Recommended)"), "A");
	}

	#[tokio::test]
	async fn headless_aborts_without_prompting() {
		let ctx = AgentContext::new(std::env::temp_dir());
		let err = AskTool.execute(&args(), &ctx).await.expect_err("headless");
		assert!(matches!(err, ToolError::Aborted(message) if message == "Ask tool requires interactive mode"));
		assert!(ctx.cancellation_token.is_cancelled());
	}

	#[tokio::test]
	async fn single_answer_formats_selection_custom_and_timeout() {
		let (ctx, channel) = ctx_with(AskOutcome::Answered(vec![AskAnswer {
			id: "q1".into(),
			selected_options: vec!["B".into()],
			..AskAnswer::default()
		}]));
		let result = AskTool.execute(&args(), &ctx).await.expect("answer");
		assert_eq!(result.content, "User selected: B");
		assert!(!result.is_error);
		let details = result.details.expect("details");
		assert_eq!(details["question"], "Pick one");
		assert_eq!(details["options"], json!(["A", "B"]));
		assert_eq!(details["multi"], false);
		assert_eq!(details["selectedOptions"], json!(["B"]));
		assert!(details.get("timedOut").is_none());
		assert_eq!(channel.seen.lock().unwrap().as_ref().unwrap()[0].id, "q1");

		let (ctx, _) = ctx_with(AskOutcome::Answered(vec![AskAnswer {
			id: "q1".into(),
			custom_input: Some("free\ntext".into()),
			note: Some("why".into()),
			timed_out: true,
			..AskAnswer::default()
		}]));
		let result = AskTool.execute(&args(), &ctx).await.expect("custom");
		assert_eq!(
			result.content,
			"User provided custom input:\n  free\n  text\nUser added note: why"
		);
		let details = result.details.expect("details");
		assert_eq!(details["customInput"], "free\ntext");
		assert_eq!(details["note"], "why");
		assert_eq!(details["timedOut"], true);

		let (ctx, _) = ctx_with(AskOutcome::Answered(vec![AskAnswer {
			id: "q1".into(),
			selected_options: vec!["A".into()],
			timed_out: true,
			..AskAnswer::default()
		}]));
		let result = AskTool.execute(&args(), &ctx).await.expect("timeout");
		assert_eq!(result.content, "User selected: A (auto-selected after timeout)");

		let (ctx, _) = ctx_with(AskOutcome::Answered(vec![AskAnswer {
			id: "q1".into(),
			custom_input: Some(String::new()),
			..AskAnswer::default()
		}]));
		let result = AskTool.execute(&args(), &ctx).await.expect("empty custom");
		assert_eq!(result.content, "User cancelled the selection");
		assert_eq!(result.details.expect("details")["customInput"], "");
	}

	#[tokio::test]
	async fn multi_answers_and_empty_multi_selection_are_valid() {
		let multi_args = json!({
			"questions": [
				{"id": "a", "question": "A?", "options": [{"label": "x"}, {"label": "y"}], "multi": true},
				{"id": "b", "question": "B?", "options": [{"label": "z"}]}
			]
		});
		let (ctx, _) = ctx_with(AskOutcome::Answered(vec![
			AskAnswer {
				id: "a".into(),
				selected_options: vec!["x".into(), "y".into()],
				..AskAnswer::default()
			},
			AskAnswer {
				id: "b".into(),
				selected_options: vec![],
				note: Some("skipped".into()),
				..AskAnswer::default()
			},
		]));
		let result = AskTool.execute(&multi_args, &ctx).await.expect("multi");
		assert_eq!(
			result.content,
			"User answers:\na: [x, y]\nb: (cancelled) (note: skipped)"
		);
		let details = result.details.expect("details");
		assert_eq!(details["results"][0]["selectedOptions"], json!(["x", "y"]));
		assert_eq!(details["results"][1]["selectedOptions"], json!([]));

		let (ctx, _) = ctx_with(AskOutcome::Answered(vec![
			AskAnswer {
				id: "a".into(),
				selected_options: vec![],
				timed_out: true,
				..AskAnswer::default()
			},
			AskAnswer {
				id: "b".into(),
				selected_options: vec!["z".into()],
				..AskAnswer::default()
			},
		]));
		let result = AskTool.execute(&multi_args, &ctx).await.expect("multi timeout");
		assert_eq!(result.content, "User answers:\na: []\nb: z");
	}

	#[tokio::test]
	async fn cancellation_chat_redirect_and_mismatch_are_handled() {
		let (ctx, _) = ctx_with(AskOutcome::Cancelled);
		let err = AskTool.execute(&args(), &ctx).await.expect_err("cancel");
		assert!(matches!(err, ToolError::Aborted(message) if message == "Ask tool was cancelled by the user"));
		assert!(ctx.cancellation_token.is_cancelled());

		let (ctx, _) = ctx_with(AskOutcome::Aborted);
		let err = AskTool.execute(&args(), &ctx).await.expect_err("abort");
		assert!(matches!(err, ToolError::Aborted(message) if message == "Ask input was cancelled"));

		let (ctx, _) = ctx_with(AskOutcome::ChatRedirect);
		let result = AskTool.execute(&args(), &ctx).await.expect("chat");
		assert_eq!(
			result.content,
			"User chose to chat about this instead of answering.\n\nQuestions asked:\nPick one"
		);
		let details = result.details.expect("details");
		assert_eq!(details["chatRedirect"], true);

		let (ctx, _) = ctx_with(AskOutcome::Answered(vec![]));
		assert!(AskTool.execute(&args(), &ctx).await.is_err());

		let (ctx, _) = ctx_with(AskOutcome::Answered(vec![AskAnswer {
			id: "wrong".into(),
			..AskAnswer::default()
		}]));
		assert!(AskTool.execute(&args(), &ctx).await.is_err());
	}

	#[test]
	fn tool_contract_is_exclusive_and_matches_schema() {
		let tool = AskTool::new();
		assert!(tool.is_exclusive());
		assert_eq!(tool.name(), "ask");
		assert!(tool.description().contains("Default to action"));
		let schema = tool.schema();
		assert_eq!(schema["required"], json!(["questions"]));
		assert_eq!(schema["properties"]["questions"]["minItems"], 1);
		assert_eq!(
			schema["properties"]["questions"]["items"]["required"],
			json!(["id", "question", "options"])
		);
	}

	#[test]
	fn empty_questions_return_the_upstream_error_text() {
		let runtime = tokio::runtime::Runtime::new().unwrap();
		let ctx = AgentContext::new(std::env::temp_dir());
		let result = runtime
			.block_on(AskTool.execute(&json!({"questions": []}), &ctx))
			.expect("empty");
		assert_eq!(result.content, "Error: questions must not be empty");
	}
}
