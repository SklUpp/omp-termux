// Validates v3_session.jsonl using the actual TS session-loader implementation.
import { loadSessionFile } from "@oh-my-pi/pi-coding-agent/session/session-loader";
import { FileSessionStorage } from "@oh-my-pi/pi-coding-agent/session/session-storage";
import { join } from "node:path";

import { existsSync } from "node:fs";
let fixture = join(import.meta.dir, "../v3_session.jsonl");
if (!existsSync(fixture)) {
	fixture = "/data/data/com.termux/files/home/omp-parity-sessions/crates/omp-cli/tests/fixtures/v3_session.jsonl";
}
const storage = new FileSessionStorage();
const result = await loadSessionFile(fixture, storage);
if (result.invalidHeader) {
	throw new Error("Invalid header in v3_session.jsonl");
}
if (result.malformedRecords > 0) {
	throw new Error(`Malformed records found: ${result.malformedRecords}`);
}
if (result.entries.length < 4) {
	throw new Error(`Expected at least 4 entries, got ${result.entries.length}`);
}
let hasCost = false;
for (const entry of result.entries) {
	if (entry.type === "message" && entry.message.role === "assistant") {
		const cost = entry.message.usage?.cost;
		if (cost && typeof cost.total === "number") {
			hasCost = true;
		}
	}
}
if (!hasCost) {
	throw new Error("Assistant message missing valid Usage.cost");
}
console.log("TS loader validation SUCCESS");
