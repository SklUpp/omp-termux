import { SessionManager } from "/data/data/com.termux/files/home/omp-parity-sessions/packages/coding-agent/src/session/session-manager.ts";
import { FileSessionStorage } from "/data/data/com.termux/files/home/omp-parity-sessions/packages/coding-agent/src/session/session-storage.ts";
import { mkdtempSync, rmSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const targetPath = "/data/data/com.termux/files/home/omp-parity-sessions/crates/omp-cli/tests/fixtures/v3_session.jsonl";
const dir = mkdtempSync(join(tmpdir(), "ts-v3-gen-"));
try {
	const storage = new FileSessionStorage();
	const sm = SessionManager.create("/path/to/test-project", dir, storage);
	await sm.setSessionName("Test Session Title", "auto");
	sm.appendMessage({
		role: "user",
		content: [
			{ type: "text", text: "hello from test user" },
			{ type: "image", data: "aGVsbG8=", mimeType: "image/png" }
		],
		timestamp: Date.now(),
	});
	sm.appendMessage({
		role: "assistant",
		content: [
			{ type: "text", text: "calling tool" },
			{ type: "toolCall", id: "call_abc123", name: "read", arguments: { path: "foo.txt" } }
		],
		api: "openai-completions",
		provider: "openai",
		model: "gpt-4o",
		usage: {
			input: 10,
			output: 20,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: 30,
			cost: { input: 0.0001, output: 0.0002, cacheRead: 0, cacheWrite: 0, total: 0.0003 }
		},
		stopReason: "toolUse",
		timestamp: Date.now(),
	});
	sm.appendMessage({
		role: "toolResult",
		toolCallId: "call_abc123",
		toolName: "read",
		content: [
			{ type: "text", text: "file content" },
			{ type: "image", data: "aW1hZ2VkYXRh", mimeType: "image/png" }
		],
		isError: false,
		timestamp: Date.now(),
	});
	await sm.flush();
	const generatedFile = sm.getSessionFile();
	if (!generatedFile) throw new Error("No session file produced");
	const content = readFileSync(generatedFile, "utf8");
	writeFileSync(targetPath, content, "utf8");
	console.log("Committed TS-generated fixture with images to", targetPath);
} finally {
	rmSync(dir, { recursive: true, force: true });
}
