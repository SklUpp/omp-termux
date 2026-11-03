// Worker body for clipboard-osc52.test.ts: runs copyToClipboard off the main
// thread, as the computer tool's `clipboard.write` does, and reports what
// reached this worker's stdout and the native backend. The worker has its own
// `process`, so the fakes below do not touch the test runner.
import { spyOn } from "bun:test";
import { workerData } from "node:worker_threads";
import { copyToClipboard } from "@oh-my-pi/pi-coding-agent/utils/clipboard";
import * as natives from "@oh-my-pi/pi-natives/clipboard";

declare const self: Worker;
const failNativeCopy = workerData === "native-fails";

Object.defineProperty(process.stdout, "isTTY", { value: true, configurable: true });
// Linux keeps macOS runs off the real pbcopy; the native write is stubbed.
Object.defineProperty(process, "platform", { value: "linux", configurable: true });
// A Termux host inherits TERMUX_VERSION; the fixture tests OSC 52/native
// routing, so keep the real termux-clipboard-set out of the worker.
delete process.env.TERMUX_VERSION;
const stdout: string[] = [];
spyOn(process.stdout, "write").mockImplementation(chunk => {
	stdout.push(typeof chunk === "string" ? chunk : chunk.toString());
	return true;
});
const nativeCopies: string[] = [];
spyOn(natives, "copyToClipboard").mockImplementation(async text => {
	nativeCopies.push(text);
	if (failNativeCopy) throw new Error("native clipboard unavailable");
});

try {
	await copyToClipboard("worker probe");
	self.postMessage({ stdout, nativeCopies });
} catch (error) {
	self.postMessage({
		stdout,
		nativeCopies,
		error: error instanceof Error ? error.message : String(error),
	});
}
