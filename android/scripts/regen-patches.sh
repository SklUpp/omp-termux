#!/usr/bin/env bash
# Regenerate android/patches/*.patch against clean upstream (android/UPSTREAM_COMMIT).
#
# Each patch is generated independently against the recorded clean upstream base
# so that all patches are self-contained and reproducible.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PATCH_DIR="$REPO_ROOT/android/patches"
PIN_FILE="$REPO_ROOT/android/UPSTREAM_COMMIT"

cd "$REPO_ROOT"

if [[ ! -f "$PIN_FILE" ]]; then
	echo "error: missing recorded upstream commit: $PIN_FILE" >&2
	exit 1
fi
upstream_commit=$(tr -d '[:space:]' < "$PIN_FILE")

echo "Regenerating Android patches against clean upstream $upstream_commit..."

tmp_upstream=$(mktemp -d "${TMPDIR:-/data/data/com.termux/files/usr/tmp}/upstream.XXXXXX")
tmp_applied=$(mktemp -d "${TMPDIR:-/data/data/com.termux/files/usr/tmp}/applied.XXXXXX")
cleanup() {
	rm -rf "$tmp_upstream" "$tmp_applied"
}
trap cleanup EXIT

# 1. Prepare clean upstream
git archive "$upstream_commit" | tar -x -C "$tmp_upstream"
git -C "$tmp_upstream" init -q
git -C "$tmp_upstream" config user.name "CI"
git -C "$tmp_upstream" config user.email "ci@test"
git -C "$tmp_upstream" add .
git -C "$tmp_upstream" commit -m "upstream" -q

# 2. Build full applied tree in tmp_applied from working tree source files
# (excluding git metadata and patches)
git archive "$upstream_commit" | tar -x -C "$tmp_applied"

python3 - << PYEOF
import subprocess, os, shutil

repo_root = "$REPO_ROOT"
patch_dir = "$PATCH_DIR"
tmp_upstream = "$tmp_upstream"

# Termux patch definitions
termux_files = [
    "crates/pi-builtins/src/proc_snapshot.rs",
    "crates/pi-builtins/src/ps.rs",
    "crates/pi-natives/Cargo.toml",
    "crates/pi-natives/src/audio.rs",
    "crates/pi-natives/src/clipboard.rs",
    "crates/pi-natives/src/crash_handler.rs",
    "crates/pi-natives/src/lib.rs",
    "crates/pi-natives/src/live.rs",
    "crates/pi-shell/src/process.rs",
    "packages/coding-agent/src/cli.ts",
    "packages/coding-agent/src/cli/startup-cwd.ts",
    "packages/coding-agent/src/cli/update-cli.ts",
    "packages/coding-agent/src/launch/broker.ts",
    "packages/coding-agent/src/session/agent-session.ts",
    "packages/coding-agent/src/task/index.ts",
    "packages/coding-agent/src/task/isolation-runner.ts",
    "packages/coding-agent/src/task/structured-subagent.ts",
    "packages/coding-agent/src/task/types.ts",
    "packages/coding-agent/src/task/worktree.ts",
    "packages/coding-agent/src/tools/browser.ts",
    "packages/coding-agent/src/tools/browser/launch.ts",
    "packages/coding-agent/src/tools/browser/registry.ts",
    "packages/coding-agent/src/tools/browser/shared-daemon.ts",
    "packages/coding-agent/src/utils/clipboard.ts",
    "packages/coding-agent/test/tools/browser-android.test.ts",
    "packages/coding-agent/test/task/structured-subagent.test.ts",
    "packages/coding-agent/test/task/wire-schema.test.ts",
    "packages/natives/native/index.js",
    "packages/natives/native/loader-state.js",
    "packages/utils/src/android-system.ts",
    "packages/utils/src/index.ts",
    "packages/utils/src/procmgr.ts",
    "packages/utils/src/temp.ts"
]

exec_path_def = """\t"browser.executablePath": {
\t\ttype: "string",
\t\tdefault: undefined,
\t\tui: {
\t\t\ttab: "tools",
\t\t\tgroup: "Grep & Browser",
\t\t\tlabel: "Browser Executable Path",
\t\t\tdescription:
\t\t\t\t"Absolute path to the Chromium/Chrome executable for headless automation. Takes precedence over PUPPETEER_EXECUTABLE_PATH. Invalid explicit path fails closed.",
\t\t},
\t},
"""
android_settings_def = """\t"system.android.wakeLock": {
\t\ttype: "boolean",
\t\tdefault: false,
\t\tui: {
\t\t\ttab: "system",
\t\t\tgroup: "Android / Termux",
\t\t\tlabel: "Acquire WakeLock",
\t\t\tdescription: "Keep CPU active while agent is running to prevent Android background throttling (Termux only)",
\t\t},
\t},
\t"system.android.oomScoreAdj": {
\t\ttype: "number",
\t\tnullable: true,
\t\tdefault: null,
\t\tui: {
\t\t\ttab: "system",
\t\t\tgroup: "Android / Termux",
\t\t\tlabel: "OOM Score Adjustment",
\t\t\tdescription: "Adjust process OOM score (-1000 to 1000) to protect daemon from low-memory killer; null disables (best-effort, no guarantee)",
\t\t},
\t},
\t"system.android.notifications.enabled": {
\t\ttype: "boolean",
\t\tdefault: false,
\t\tui: {
\t\t\ttab: "system",
\t\t\tgroup: "Android / Termux",
\t\t\tlabel: "Android Notifications",
\t\t\tdescription: "Show Android system notifications via termux-notification when agent events occur (off by default)",
\t\t},
\t},
\t"system.android.notifications.events": {
\t\ttype: "array",
\t\tdefault: [],
\t\tui: {
\t\t\ttab: "system",
\t\t\tgroup: "Android / Termux",
\t\t\tlabel: "Notification Events",
\t\t\tdescription: "Which agent events trigger notifications (e.g., agent_end, turn_complete); empty means no events",
\t\t},
\t},
"""
isolation_repo_def = """\t"task.isolation.repoRoot": {
\t\ttype: "string",
\t\tnullable: true,
\t\tdefault: null,
\t\tui: {
\t\t\ttab: "tasks",
\t\t\tgroup: "Isolation",
\t\t\tlabel: "Isolation Repository Root",
\t\t\tdescription:
\t\t\t\t"Explicit path to the Git repository root to use for isolated task worktrees (defaults to current working directory)",
\t\t},
\t},
"""

# 1. Generate 01-termux.patch
subprocess.check_call(["git", "-C", tmp_upstream, "reset", "--hard", "HEAD", "-q"])
subprocess.check_call(["git", "-C", tmp_upstream, "clean", "-fd", "-q"])

for f in termux_files:
    src = os.path.join(repo_root, f)
    dst = os.path.join(tmp_upstream, f)
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    shutil.copy2(src, dst)
    subprocess.check_call(["git", "-C", tmp_upstream, "add", "-N", f])

schema_path_t = os.path.join(tmp_upstream, "packages/coding-agent/src/config/settings-schema.ts")
content_t = open(schema_path_t).read()
content_t = content_t.replace('\t"browser.screenshotDir": {', exec_path_def + '\t"browser.screenshotDir": {')
content_t = content_t.replace('\t"advisor.enabled": {', android_settings_def + '\t"advisor.enabled": {')
content_t = content_t.replace('\t"task.isolation.merge": {', isolation_repo_def + '\n\t"task.isolation.merge": {')
with open(schema_path_t, "w") as f:
    f.write(content_t)
subprocess.check_call(["git", "-C", tmp_upstream, "add", "-N", "packages/coding-agent/src/config/settings-schema.ts"])

diff_t = subprocess.check_output(["git", "-C", tmp_upstream, "diff", "HEAD", "--binary"])
p1_path = os.path.join(patch_dir, "01-termux.patch")
with open(p1_path, "wb") as f:
    f.write(diff_t)
print(f"Generated 01-termux.patch ({len(diff_t)} bytes)")

# 2. Generate 02-autolearn.patch
subprocess.check_call(["git", "-C", tmp_upstream, "reset", "--hard", "HEAD", "-q"])
subprocess.check_call(["git", "-C", tmp_upstream, "clean", "-fd", "-q"])

autolearn_files_no_schema = [
    "docs/autolearn.md",
    "packages/coding-agent/src/autolearn/candidate-ledger.ts",
    "packages/coding-agent/src/autolearn/config.ts",
    "packages/coding-agent/src/autolearn/controller.ts",
    "packages/coding-agent/src/autolearn/index.ts",
    "packages/coding-agent/src/autolearn/learn-db.ts",
    "packages/coding-agent/src/autolearn/managed-skills.ts",
    "packages/coding-agent/src/autolearn/redact.ts",
    "packages/coding-agent/src/autolearn/skill-writer.ts",
    "packages/coding-agent/src/autolearn/verifier.ts",
    "packages/coding-agent/src/config/settings.ts",
    "packages/coding-agent/src/hindsight/bank.ts",
    "packages/coding-agent/src/memory-backend/runtime.ts",
    "packages/coding-agent/src/memory-backend/types.ts",
    "packages/coding-agent/src/mnemopi/backend.ts",
    "packages/coding-agent/src/mnemopi/config.ts",
    "packages/coding-agent/src/mnemopi/state.ts",
    "packages/coding-agent/src/modes/components/settings-defs.ts",
    "packages/coding-agent/src/sdk.ts",
    "packages/coding-agent/src/tools/index.ts",
    "packages/coding-agent/src/tools/learn.ts",
    "packages/coding-agent/src/tools/manage-skill.ts",
    "packages/coding-agent/test/autolearn-managed-skills.test.ts",
    "packages/coding-agent/test/custom-autolearn.test.ts"
]

for f in autolearn_files_no_schema:
    src = os.path.join(repo_root, f)
    dst = os.path.join(tmp_upstream, f)
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    shutil.copy2(src, dst)
    subprocess.check_call(["git", "-C", tmp_upstream, "add", "-N", f])

schema_path_a = os.path.join(tmp_upstream, "packages/coding-agent/src/config/settings-schema.ts")
curr_schema_content = open(os.path.join(repo_root, "packages/coding-agent/src/config/settings-schema.ts")).read()

curr_schema_content = curr_schema_content.replace(exec_path_def, "")
curr_schema_content = curr_schema_content.replace(android_settings_def, "")
curr_schema_content = curr_schema_content.replace(isolation_repo_def + "\n", "")
with open(schema_path_a, "w") as f:
    f.write(curr_schema_content)
subprocess.check_call(["git", "-C", tmp_upstream, "add", "-N", "packages/coding-agent/src/config/settings-schema.ts"])

diff_a = subprocess.check_output(["git", "-C", tmp_upstream, "diff", "HEAD", "--binary"])
p2_path = os.path.join(patch_dir, "02-autolearn.patch")
with open(p2_path, "wb") as f:
    f.write(diff_a)
print(f"Generated 02-autolearn.patch ({len(diff_a)} bytes)")

manifest_path = os.path.join(patch_dir, "MANIFEST")
with open(manifest_path, "w") as f:
    f.write("# Active patch order - each generated against clean upstream (android/UPSTREAM_COMMIT)\n# This manifest is validated by android/scripts/apply-patches.sh\n01-termux.patch\n02-autolearn.patch\n")

print("MANIFEST updated.")
PYEOF

echo "All patches regenerated successfully."
