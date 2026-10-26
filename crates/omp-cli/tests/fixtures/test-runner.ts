import { SessionManager } from "/data/data/com.termux/files/home/omp-parity-sessions/packages/coding-agent/src/session/session-manager.ts";
import { FileSessionStorage } from "/data/data/com.termux/files/home/omp-parity-sessions/packages/coding-agent/src/session/session-storage.ts";
import { mkdtempSync, rmSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const dir = mkdtempSync(join(tmpdir(), "ts-gen-test-"));
try {
	const storage = new FileSessionStorage();
	const sm = SessionManager.create("/path/to/test-project", dir, storage);
	await sm.setSessionName("Test Session Title", "auto");
	sm.appendMessage({
		role: "user",
		content: [{ type: "text", text: "hello from test user" }],
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
		timestamp: Date.now(),
	});
	sm.appendMessage({
		role: "toolResult",
		toolCallId: "call_abc123",
		toolName: "read",
		content: [{ type: "text", text: "file content" }],
		isError: false,
		timestamp: Date.now(),
	});
	await sm.flush();
	const f = sm.getSessionFile();
	console.log("Session file:", f);
	const content = readFileSync(f, "utf8");
	const lines = content.trim().split("\n");
	console.log("Lines count:", lines.length);
	for (let i = 0; i < lines.length; i++) {
		console.log(`L${i+1}: bytes=${Buffer.byteLength(lines[i])} text=${lines[i].slice(0, 90)}`);
	}
} finally {
	rmSync(dir, { recursive: true, force: true });
}
