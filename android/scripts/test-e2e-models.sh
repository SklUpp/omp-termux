#!/usr/bin/env bash
# Model-driven end-to-end check for an installed OMP build.
#
# Unlike the unit/overlay suites, every case here talks to a real model: it
# proves provider wiring, the agent loop, tool execution and file/image
# transport on the device. Requires configured credentials; never runs in CI.
#
# Cases:
#   1. plain reply      the exact token survives the provider round trip
#   2. strict JSON      exactly one JSON object equal to the asked object
#   3. bash tool        bash tool_execution_end aimed at the file + exact bytes
#   4. write/read tools successful write of the exact bytes, then a read of the
#                       same file, in that order, plus exact bytes on disk
#   5. image attachment two opaque fixtures with different colours; both final
#                       answers must name the right colour (opt-in: vision)
#
# Cases 3 and 4 run model-chosen commands with --auto-approve. A working
# directory is not a sandbox: an autonomous command can leave it. They are
# therefore opt-in via --allow-tools; read the warning it prints.
#
# Usage: bash android/scripts/test-e2e-models.sh [flags]
#   --omp PATH       omp binary (default: command -v omp)
#   --model NAME     pass through as --model
#   --timeout SEC    per model call, positive integer (default 180)
#   --allow-tools    run the tool cases with --auto-approve (warns first)
#   --image          run the image cases (needs a vision-capable model)
#   --keep           keep the temp workspace and raw event logs
#   --verbose        dump the complete raw event log of a failing case
set -uo pipefail

OMP_BIN=""
MODEL=""
CASE_TIMEOUT=180
ALLOW_TOOLS=0
WITH_IMAGE=0
KEEP=0
VERBOSE=0

need_value() { # $1 flag, $2 remaining argc, $3 value
	[ "$2" -ge 2 ] && [ -n "$3" ] || { echo "error: $1 needs a value" >&2; exit 2; }
	case "$3" in --*) echo "error: $1 needs a value, got option $3" >&2; exit 2 ;; esac
}

while [ $# -gt 0 ]; do
	case "$1" in
		--omp) need_value "$1" "$#" "${2:-}"; OMP_BIN="$2"; shift 2 ;;
		--model) need_value "$1" "$#" "${2:-}"; MODEL="$2"; shift 2 ;;
		--timeout) need_value "$1" "$#" "${2:-}"; CASE_TIMEOUT="$2"; shift 2 ;;
		--allow-tools) ALLOW_TOOLS=1; shift ;;
		--image) WITH_IMAGE=1; shift ;;
		--keep) KEEP=1; shift ;;
		--verbose) VERBOSE=1; shift ;;
		-h|--help) sed -n '2,28p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
		*) echo "error: unknown argument: $1" >&2; exit 2 ;;
	esac
done

case "$CASE_TIMEOUT" in
	''|*[!0-9]*) echo "error: --timeout must be a positive integer" >&2; exit 2 ;;
esac
[ "$CASE_TIMEOUT" -gt 0 ] || { echo "error: --timeout must be a positive integer" >&2; exit 2; }

[ -n "$OMP_BIN" ] || OMP_BIN=$(command -v omp || true)
[ -n "$OMP_BIN" ] || { echo "error: omp not found; pass --omp PATH" >&2; exit 2; }
for dep in jq timeout base64 cmp; do
	command -v "$dep" >/dev/null 2>&1 || { echo "error: missing dependency: $dep" >&2; exit 2; }
done

TMP_BASE="${TMPDIR:-${PREFIX:?set PREFIX or TMPDIR}/tmp}"
WORK=$(mktemp -d "$TMP_BASE/omp-e2e-models.XXXXXX") || exit 1
WORK=$(cd "$WORK" && pwd -P) || exit 1
cleanup() {
	if [ "$KEEP" -eq 1 ]; then
		echo "kept workspace: $WORK"
	else
		rm -rf "$WORK"
	fi
}
trap cleanup EXIT

pass=0; fail=0; skip=0

run_omp() { # $1 events file, rest: messages
	local out="$1"; shift
	local args=(-p --mode=json --no-session --no-title --cwd "$WORK")
	[ -n "$MODEL" ] && args+=(--model "$MODEL")
	[ "$ALLOW_TOOLS" -eq 1 ] && args+=(--auto-approve)
	: > "$out"
	# The omp launcher owns the LD_PRELOAD compat shim, so call the launcher.
	timeout "$CASE_TIMEOUT" "$OMP_BIN" "${args[@]}" "$@" >"$out" 2>"$out.err"
}

# Only the last turn is the answer; earlier turns are tool-loop chatter.
final_text() {
	jq -s -r '[.[]|select(.type=="turn_end")]|last|.message.content[]?|select(.type=="text")|.text' "$1" 2>/dev/null
}

trim_text() {
	sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//'
}

# Drop a ``` / ```lang pair only when it wraps the whole payload.
strip_fence() {
	awk '{ line[++n] = $0 }
	     END {
	         first = line[1]
	         last = line[n]
	         if (n >= 2 && first ~ /^[[:space:]]*```[a-zA-Z0-9]*[[:space:]]*$/ && last ~ /^[[:space:]]*```[[:space:]]*$/) {
	             for (i = 2; i < n; i++) print line[i]
	         } else {
	             for (i = 1; i <= n; i++) print line[i]
	         }
	     }'
}

launcher_error() { # $1 events file
	grep -Eq '^Error:' "$1.err" 2>/dev/null && return 0
	jq -e 'select(.type|test("(^|_)error$"))' "$1" >/dev/null 2>&1
}

# Successful tool calls only; starts carry the args, ends carry the result.
tool_ok() { # $1 events, $2 tool name, $3 substring required in the start args
	# Success is the trace status; stdout/stderr text is command output.
	jq -s -e --arg name "$2" --arg needle "$3" '
		. as $ev
		| (reduce ($ev[] | select(.type=="tool_execution_start")) as $s ({}; .[$s.toolCallId] = $s)) as $starts
		| any($ev[] | select(.type=="tool_execution_end" and .toolName==$name);
			(($starts[.toolCallId].args | tojson) | contains($needle))
			and (.result.details.trace.status == 0)
		)' "$1" >/dev/null 2>&1
}

report() {
	case "$1" in
		PASS) pass=$((pass + 1)); printf 'PASS  %s\n' "$2" ;;
		FAIL) fail=$((fail + 1)); printf 'FAIL  %s: %s\n' "$2" "$3" ;;
		SKIP) skip=$((skip + 1)); printf 'SKIP  %s: %s\n' "$2" "$3" ;;
	esac
}

fail_case() { # $1 events file, $2 name, $3 reason
	report FAIL "$2" "$3"
	if [ "$VERBOSE" -eq 1 ]; then
		echo "---- stderr"
		cat "$1.err" 2>/dev/null
		echo "---- raw events"
		cat "$1" 2>/dev/null
	fi
}

case_reply() {
	local json="$WORK/1-reply.json" text rc
	run_omp "$json" 'Reply with exactly the token E2E-REPLY-OK and nothing else.'
	rc=$?
	launcher_error "$json" && { fail_case "$json" "plain reply" "launcher reported an error"; return; }
	[ "$rc" -eq 0 ] || { fail_case "$json" "plain reply" "omp exited $rc"; return; }
	text=$(final_text "$json" | trim_text)
	[ "$text" = "E2E-REPLY-OK" ] \
		&& report PASS "plain reply (exact model round trip)" \
		|| fail_case "$json" "plain reply" "answer is not the exact token"
}

case_json() {
	local json="$WORK/2-json.json" text rc
	run_omp "$json" 'Output only compact JSON with exactly the fields {"e2e":"ok","n":7}. No prose, no code fence.'
	rc=$?
	launcher_error "$json" && { fail_case "$json" "strict JSON" "launcher reported an error"; return; }
	[ "$rc" -eq 0 ] || { fail_case "$json" "strict JSON" "omp exited $rc"; return; }
	text=$(final_text "$json" | trim_text | strip_fence | trim_text)
	printf '%s' "$text" | jq -s -e 'length==1 and (.[0]|type=="object" and .=={"e2e":"ok","n":7})' >/dev/null 2>&1 \
		&& report PASS "strict JSON answer (single exact object)" \
		|| fail_case "$json" "strict JSON" "answer is not exactly one asked object"
}

case_bash_tool() {
	local json="$WORK/3-bash.json" marker="E2E-BASH-$RANDOM$RANDOM" rc
	run_omp "$json" "Use the bash tool to run exactly: printf '$marker' > e2e-bash.txt. Then reply DONE only."
	rc=$?
	launcher_error "$json" && { fail_case "$json" "bash tool" "launcher reported an error"; return; }
	[ "$rc" -eq 0 ] || { fail_case "$json" "bash tool" "omp exited $rc"; return; }
	tool_ok "$json" bash e2e-bash.txt || { fail_case "$json" "bash tool" "no successful bash call aimed at the file"; return; }
	printf '%s' "$marker" > "$WORK/e2e-bash.expected"
	cmp -s "$WORK/e2e-bash.expected" "$WORK/e2e-bash.txt" \
		&& report PASS "bash tool (call + exact bytes)" \
		|| fail_case "$json" "bash tool" "e2e-bash.txt missing or not the exact bytes"
}

case_write_read_tools() {
	local json="$WORK/4-write-read.json" marker="E2E-TOK-$RANDOM$RANDOM" text rc
	run_omp "$json" "Use the write tool (not bash) to write exactly the bytes $marker, with no trailing newline, to e2e-token.txt. Then use the read tool to read e2e-token.txt and reply with the file content and nothing else."
	rc=$?
	launcher_error "$json" && { fail_case "$json" "write/read tools" "launcher reported an error"; return; }
	[ "$rc" -eq 0 ] || { fail_case "$json" "write/read tools" "omp exited $rc"; return; }
	jq -s -e --arg f e2e-token.txt --arg m "$marker" --arg w "$WORK" '
		def target: (sub(":(raw|[0-9].*)$"; ""));
		def same_file: target == $w + "/" + $f or target == "./" + $f or target == $f;
		. as $ev
		| (reduce ($ev[] | select(.type=="tool_execution_start")) as $s ({}; .[$s.toolCallId] = $s)) as $starts
		| ([$ev | to_entries[] | select(.value.type=="tool_execution_end" and .value.toolName=="write")]
			| map(select(
				($starts[.value.toolCallId].args.content // null) == $m
				and (($starts[.value.toolCallId].args.path // "") | same_file)
				and ((((.value.result.content[]?.text) // "") | test("Successfully wrote")))
			)) | first | .key) as $wi
		| ($wi != null)
		  and any($ev | to_entries[] | select(.value.type=="tool_execution_end" and .value.toolName=="read" and .key > $wi);
			(($starts[.value.toolCallId].args.path // "") | same_file)
			and (((((.value.result.content[]?.text) // "") | sub("\\n$"; "")) == $m))
		  )' "$json" >/dev/null 2>&1 \
		|| { fail_case "$json" "write/read tools" "no successful write-then-read of the exact bytes"; return; }
	printf '%s' "$marker" > "$WORK/e2e-token.expected"
	cmp -s "$WORK/e2e-token.expected" "$WORK/e2e-token.txt" \
		|| { fail_case "$json" "write/read tools" "file bytes differ from the marker"; return; }
	text=$(final_text "$json" | trim_text)
	[ "$text" = "$marker" ] \
		&& report PASS "write/read tools (ordered calls, exact bytes, exact read-back)" \
		|| fail_case "$json" "write/read tools" "read-back answer is not the exact token"
}

# Images run without --auto-approve in an empty directory: nothing local to
# inspect, so the answer comes from the attachment. The cwd is not a sandbox,
# and a failure is final (no retry) so a wrong or errored call cannot pass.
run_omp_image() { # $1 events file, rest: messages
	local out="$1"; shift
	local args=(-p --mode=json --no-session --no-title --cwd "$WORK/image-answer")
	[ -n "$MODEL" ] && args+=(--model "$MODEL")
	mkdir -p "$WORK/image-answer"
	: > "$out"
	timeout "$CASE_TIMEOUT" "$OMP_BIN" "${args[@]}" "$@" >"$out" 2>"$out.err"
}

# The complete answer must be the colour word: no prose is tolerated.
image_answer_ok() {
	local answer
	answer=$(printf '%s' "$1" | trim_text | tr '[:upper:]' '[:lower:]' \
		| sed -e 's/^[`"'"'"'[:space:]]*//' -e 's/[`"'"'"'[:space:].;:,!?]*$//')
	[ "$answer" = "$2" ]
}

run_image_case() { # $1 base64, $2 expected colour word, $3 label
	local png="$WORK/sample-$3.png" json text rc
	printf '%s' "$1" | base64 -d > "$png"
	json="$WORK/5-image-$3.json"
	run_omp_image "$json" "@$png" "Reply with only the dominant colour of this image as one lowercase word, without punctuation or explanation."
	rc=$?
	launcher_error "$json" && { fail_case "$json" "image $3" "launcher reported an error"; return 1; }
	[ "$rc" -eq 0 ] || { fail_case "$json" "image $3" "omp exited $rc"; return 1; }
	text=$(final_text "$json")
	image_answer_ok "$text" "$2" || { fail_case "$json" "image $3" "answer is not exactly the colour word"; return 1; }
	return 0
}

case_images() {
	# Two colours: a blind model would have to guess both answers right. That
	# raises the bar but cannot by itself prove the bytes reached the provider.
	local red='iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAYAAABytg0kAAAAEUlEQVR42mP4z8DwH4QZYAwAR8oH+Rq28akAAAAASUVORK5CYII='
	local blue='iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAYAAABytg0kAAAAEElEQVR42mNgYPj/H4KhDAA/0gf5XBPgQgAAAABJRU5ErkJggg=='
	run_image_case "$red" red a || return
	run_image_case "$blue" blue b || return
	report PASS "image attachment (two-colour vision round trip)"
}

echo "omp: $OMP_BIN ($(timeout 15 "$OMP_BIN" --version 2>/dev/null | head -1))"
echo "model: ${MODEL:-<profile default>}"
echo "workspace: $WORK"
if [ "$ALLOW_TOOLS" -eq 1 ]; then
	echo "warning: --allow-tools lets the model run commands with --auto-approve;"
	echo "         the working directory is not a filesystem sandbox."
fi
echo

case_reply
case_json
if [ "$ALLOW_TOOLS" -eq 1 ]; then
	case_bash_tool
	case_write_read_tools
else
	report SKIP "bash tool" "pass --allow-tools to run model-chosen commands"
	report SKIP "write/read tools" "pass --allow-tools to run model-chosen commands"
fi
if [ "$WITH_IMAGE" -eq 1 ]; then
	case_images
else
	report SKIP "image attachment" "pass --image with a vision-capable model"
fi

echo
printf 'summary: %d passed, %d failed, %d skipped\n' "$pass" "$fail" "$skip"
[ "$fail" -eq 0 ] || exit 1
