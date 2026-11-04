/**
 * Termux notification wiring: the existing `completion.notify` / `error.notify` /
 * `ask.notify` gates and the global `PI_NOTIFICATIONS` suppression decide whether
 * an Android system notification (`termux-notification` via Termux:API) fires.
 *
 * Assertions stop at the real spawn boundary, so the tests exercise the same
 * helper and gating code the binary runs — not a mocked module binding.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { AssistantMessage } from "@oh-my-pi/pi-ai";
import { resetSettingsForTest, Settings, settings } from "@oh-my-pi/pi-coding-agent/config/settings";
import { EventController } from "@oh-my-pi/pi-coding-agent/modes/controllers/event-controller";
import { cfgAskNotify, cfgCompletionNotify, cfgErrorNotify } from "@oh-my-pi/pi-coding-agent/modes/settings";
import type { AgentSessionEvent } from "@oh-my-pi/pi-coding-agent/session/agent-session";
import type { ToolSession } from "@oh-my-pi/pi-coding-agent/tools";
import { AskTool } from "@oh-my-pi/pi-coding-agent/tools/ask";
import {
	androidNotificationId,
	clearAndroidNotification,
	sendAndroidNotification,
} from "@oh-my-pi/pi-coding-agent/utils/android-helpers";
import { TERMINAL } from "@oh-my-pi/pi-tui";
import { initTheme } from "@oh-my-pi/pi-tui/theme";

import { createInteractiveModeContext } from "./helpers/interactive-mode-context";

const originalWarpProtocol = process.env.WARP_CLI_AGENT_PROTOCOL_VERSION;
const originalPiNotifications = process.env.PI_NOTIFICATIONS;
const originalTermuxVersion = process.env.TERMUX_VERSION;
const statusId = androidNotificationId();

beforeAll(() => {
	initTheme();
});

beforeEach(async () => {
	resetSettingsForTest();
	delete process.env.WARP_CLI_AGENT_PROTOCOL_VERSION;
	delete process.env.PI_NOTIFICATIONS;
	delete process.env.TERMUX_VERSION;
	const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "omp-notify-test-"));
	await Settings.init({ inMemory: true, cwd: tempDir });
});

afterEach(() => {
	vi.restoreAllMocks();
	resetSettingsForTest();
	restoreEnv("WARP_CLI_AGENT_PROTOCOL_VERSION", originalWarpProtocol);
	restoreEnv("PI_NOTIFICATIONS", originalPiNotifications);
	restoreEnv("TERMUX_VERSION", originalTermuxVersion);
});

function restoreEnv(key: string, value: string | undefined): void {
	if (value === undefined) {
		delete process.env[key];
	} else {
		process.env[key] = value;
	}
}

/** Capture real `termux-notification` invocations; mark the runtime as Termux. */
function captureAndroidSpawns(): string[][] {
	process.env.TERMUX_VERSION = "0.118";
	const calls: string[][] = [];
	vi.spyOn(Bun, "spawn").mockImplementation(((cmd: string[]) => {
		calls.push(cmd);
		return {
			stderr: new ReadableStream({
				start(controller) {
					controller.close();
				},
			}),
			kill: () => {},
			exited: Promise.resolve(0),
		} as never;
	}) as unknown as typeof Bun.spawn);
	return calls;
}

function makeAssistantMessage(stopReason: "stop" | "aborted" | "error", text = "result text"): AssistantMessage {
	return {
		role: "assistant",
		content: [{ type: "text", text }],
		stopReason,
		usage: { inputTokens: 0, outputTokens: 0 },
		timestamp: Date.now(),
	} as unknown as AssistantMessage;
}

function makeContext(sessionName = "test-session") {
	return createInteractiveModeContext({
		sessionManager: { getSessionName: () => sessionName },
	});
}

function makeAgentEndEvent(
	messages: AssistantMessage[],
	isTerminal = true,
): Extract<AgentSessionEvent, { type: "agent_end" }> {
	return { type: "agent_end", messages, isTerminal } as Extract<AgentSessionEvent, { type: "agent_end" }>;
}

function makeAskSession(overrides: Partial<ToolSession> = {}): ToolSession {
	return {
		cwd: "/tmp",
		hasUI: true,
		canPromptUser: true,
		getSessionFile: () => null,
		getSessionSpawns: () => "*",
		getPlanModeState: () => null,
		settings,
		...overrides,
	} as unknown as ToolSession;
}

function answeredAskDialog() {
	return vi.fn().mockResolvedValue({
		kind: "submit",
		results: [{ id: "q1", question: "Proceed?", selectedOptions: ["Yes"] }],
	});
}

function askContext(dialog: ReturnType<typeof answeredAskDialog>) {
	return {
		hasUI: true,
		ui: { askDialog: dialog },
		abort: vi.fn(),
	} as never;
}

/** Non-rich UI surface: `askDialog` is absent, like an RPC host without set_ask_dialog. */
function plainAskContext(select: () => Promise<string | undefined>) {
	return {
		hasUI: true,
		ui: {
			select,
			editor: async () => undefined,
			timeoutStartsOnPresentation: false,
		},
		abort: vi.fn(),
	} as never;
}

const ASK_PARAMS = { questions: [{ id: "q1", question: "Proceed?", options: [{ label: "Yes" }] }] };

describe("Termux notifications: completion", () => {
	it("posts one Android notification when completion.notify is on", () => {
		const calls = captureAndroidSpawns();
		vi.spyOn(TERMINAL, "sendNotification").mockImplementation(() => {});
		cfgCompletionNotify.override(settings, "on");

		new EventController(makeContext("my-session")).sendCompletionNotification(
			makeAgentEndEvent([makeAssistantMessage("stop")]),
		);

		expect(calls).toEqual([["termux-notification", "--id", statusId, "--title", "omp", "--content", "Complete"]]);
	});

	it("uses the fixed app title, never the conversation-derived session name", () => {
		const calls = captureAndroidSpawns();
		vi.spyOn(TERMINAL, "sendNotification").mockImplementation(() => {});
		cfgCompletionNotify.override(settings, "on");

		// Session names are generated from conversation text; a notification
		// listener must not be able to read them back.
		new EventController(makeContext("ghp_1234567890abcdef")).sendCompletionNotification(
			makeAgentEndEvent([makeAssistantMessage("stop")]),
		);

		expect(calls).toHaveLength(1);
		expect(calls[0]).toContain("omp");
		expect(calls[0]).not.toContain("ghp_");
	});

	it("never puts assistant message text into the notification", () => {
		const calls = captureAndroidSpawns();
		vi.spyOn(TERMINAL, "sendNotification").mockImplementation(() => {});
		cfgCompletionNotify.override(settings, "on");

		new EventController(makeContext("my-session")).sendCompletionNotification(
			makeAgentEndEvent([makeAssistantMessage("stop", "token ghp_1234567890abcdef")]),
		);

		expect(calls).toHaveLength(1);
		expect(JSON.stringify(calls)).not.toContain("ghp_");
	});

	it("honours completion.notify=off", () => {
		const calls = captureAndroidSpawns();
		vi.spyOn(TERMINAL, "sendNotification").mockImplementation(() => {});
		cfgCompletionNotify.override(settings, "off");

		new EventController(makeContext()).sendCompletionNotification(makeAgentEndEvent([makeAssistantMessage("stop")]));

		expect(calls).toHaveLength(0);
	});

	it("stays silent for aborted and errored turns", () => {
		const calls = captureAndroidSpawns();
		vi.spyOn(TERMINAL, "sendNotification").mockImplementation(() => {});
		cfgCompletionNotify.override(settings, "on");
		const controller = new EventController(makeContext());

		controller.sendCompletionNotification(makeAgentEndEvent([makeAssistantMessage("aborted")]));
		controller.sendCompletionNotification(makeAgentEndEvent([makeAssistantMessage("error")]));

		expect(calls).toHaveLength(0);
	});

	it("stays silent when the Warp CLI-agent protocol drives native UX", () => {
		const calls = captureAndroidSpawns();
		vi.spyOn(TERMINAL, "sendNotification").mockImplementation(() => {});
		cfgCompletionNotify.override(settings, "on");
		process.env.WARP_CLI_AGENT_PROTOCOL_VERSION = "1";

		new EventController(makeContext()).sendCompletionNotification(makeAgentEndEvent([makeAssistantMessage("stop")]));

		expect(calls).toHaveLength(0);
	});
});

describe("Termux notifications: error", () => {
	it("posts one Android notification when error.notify is on and stopReason is error", () => {
		const calls = captureAndroidSpawns();
		vi.spyOn(TERMINAL, "sendNotification").mockImplementation(() => {});
		cfgErrorNotify.override(settings, "on");

		new EventController(makeContext("err-session")).sendErrorNotification(
			makeAgentEndEvent([makeAssistantMessage("error")]),
		);

		expect(calls).toEqual([
			["termux-notification", "--id", statusId, "--title", "omp", "--content", "Stopped with error"],
		]);
	});

	it("honours error.notify=off (schema default)", () => {
		const calls = captureAndroidSpawns();
		vi.spyOn(TERMINAL, "sendNotification").mockImplementation(() => {});
		cfgErrorNotify.override(settings, "off");

		new EventController(makeContext()).sendErrorNotification(makeAgentEndEvent([makeAssistantMessage("error")]));

		expect(calls).toHaveLength(0);
	});

	it("stays silent for normal completions and non-terminal scheduling pauses", () => {
		const calls = captureAndroidSpawns();
		vi.spyOn(TERMINAL, "sendNotification").mockImplementation(() => {});
		cfgErrorNotify.override(settings, "on");
		const controller = new EventController(makeContext());

		controller.sendErrorNotification(makeAgentEndEvent([makeAssistantMessage("stop")]));
		controller.sendErrorNotification(makeAgentEndEvent([makeAssistantMessage("error")], false));

		expect(calls).toHaveLength(0);
	});
});

describe("Termux notifications: ask", () => {
	it("posts one per-prompt card and removes it after the dialog is answered", async () => {
		const calls = captureAndroidSpawns();
		vi.spyOn(TERMINAL, "sendNotification").mockImplementation(() => {});
		cfgAskNotify.override(settings, "on");

		await new AskTool(makeAskSession()).execute(
			"call-ask",
			ASK_PARAMS,
			undefined,
			undefined,
			askContext(answeredAskDialog()),
		);

		const post = calls.find(args => args[0] === "termux-notification");
		const remove = calls.find(args => args[0] === "termux-notification-remove");
		expect(post).toBeDefined();
		// Distinct from the completion/error status id, so the removal cannot
		// erase a card posted while the dialog was pending.
		expect(post?.[2]?.startsWith(`omp-ask-${process.pid}-`)).toBe(true);
		expect(post?.[2]).not.toBe(statusId);
		// Removal is serialized behind the post promise, so it comes second.
		expect(calls.map(args => args[0])).toEqual(["termux-notification", "termux-notification-remove"]);
		expect(remove?.[1]).toBe(post?.[2]);
	});

	it("clears the card on the plain (non-rich-dialog) path", async () => {
		const calls = captureAndroidSpawns();
		vi.spyOn(TERMINAL, "sendNotification").mockImplementation(() => {});
		cfgAskNotify.override(settings, "on");

		await new AskTool(makeAskSession()).execute(
			"call-ask",
			ASK_PARAMS,
			undefined,
			undefined,
			plainAskContext(async () => "Yes"),
		);

		const post = calls.find(args => args[0] === "termux-notification");
		expect(post?.[2]?.startsWith(`omp-ask-${process.pid}-`)).toBe(true);
		expect(post).toBeDefined();
		expect(calls.at(-1)).toEqual(["termux-notification-remove", post![2]]);
	});

	it("clears the card when the dialog throws", async () => {
		const calls = captureAndroidSpawns();
		vi.spyOn(TERMINAL, "sendNotification").mockImplementation(() => {});
		cfgAskNotify.override(settings, "on");

		await expect(
			new AskTool(makeAskSession()).execute(
				"call-ask",
				ASK_PARAMS,
				undefined,
				undefined,
				plainAskContext(async () => {
					throw new Error("ui failure");
				}),
			),
		).rejects.toBeDefined();

		const post = calls.find(args => args[0] === "termux-notification");
		expect(post).toBeDefined();
		expect(calls.at(-1)).toEqual(["termux-notification-remove", post![2]]);
	});

	it("clears the card when the ask is aborted before the dialog settles", async () => {
		const calls = captureAndroidSpawns();
		vi.spyOn(TERMINAL, "sendNotification").mockImplementation(() => {});
		cfgAskNotify.override(settings, "on");
		const controller = new AbortController();
		controller.abort();

		await expect(
			new AskTool(makeAskSession()).execute(
				"call-ask",
				ASK_PARAMS,
				controller.signal,
				undefined,
				askContext(vi.fn(() => new Promise(() => {}))),
			),
		).rejects.toBeDefined();

		const post = calls.find(args => args[0] === "termux-notification");
		expect(post).toBeDefined();
		expect(calls.at(-1)).toEqual(["termux-notification-remove", post![2]]);
	});

	it("posts and removes nothing when notifications are globally suppressed", async () => {
		const calls = captureAndroidSpawns();
		vi.spyOn(TERMINAL, "sendNotification").mockImplementation(() => {});
		cfgAskNotify.override(settings, "on");
		process.env.PI_NOTIFICATIONS = "off";

		await new AskTool(makeAskSession()).execute(
			"call-ask",
			ASK_PARAMS,
			undefined,
			undefined,
			askContext(answeredAskDialog()),
		);

		expect(calls).toHaveLength(0);
	});

	it("honours ask.notify=off", async () => {
		const calls = captureAndroidSpawns();
		vi.spyOn(TERMINAL, "sendNotification").mockImplementation(() => {});
		cfgAskNotify.override(settings, "off");

		await new AskTool(makeAskSession()).execute(
			"call-ask",
			ASK_PARAMS,
			undefined,
			undefined,
			askContext(answeredAskDialog()),
		);

		expect(calls).toHaveLength(0);
	});

	it("stays silent when the session has no UI", async () => {
		const calls = captureAndroidSpawns();
		vi.spyOn(TERMINAL, "sendNotification").mockImplementation(() => {});
		cfgAskNotify.override(settings, "on");

		await new AskTool(makeAskSession({ hasUI: false })).execute(
			"call-ask",
			ASK_PARAMS,
			undefined,
			undefined,
			askContext(answeredAskDialog()),
		);

		expect(calls).toHaveLength(0);
	});
});

describe("Termux notifications: global suppression and failure modes", () => {
	it("suppresses Android notifications when PI_NOTIFICATIONS is off", async () => {
		const calls = captureAndroidSpawns();
		process.env.PI_NOTIFICATIONS = "off";

		const outcome = await sendAndroidNotification("Title", "Body");

		expect(calls).toHaveLength(0);
		expect(outcome).toEqual({ ok: false, error: "notifications suppressed" });
	});

	it("suppresses Android notifications when PI_NOTIFICATIONS is 0 or false", async () => {
		const calls = captureAndroidSpawns();

		process.env.PI_NOTIFICATIONS = "0";
		expect((await sendAndroidNotification("T", "B")).ok).toBe(false);
		process.env.PI_NOTIFICATIONS = "false";
		expect((await sendAndroidNotification("T", "B")).ok).toBe(false);

		expect(calls).toHaveLength(0);
	});

	it("reports unavailability instead of firing outside Termux", async () => {
		const calls = captureAndroidSpawns();
		delete process.env.TERMUX_VERSION;
		if (process.platform === "android") return; // Termux itself: platform check cannot be faked.

		const outcome = await sendAndroidNotification("T", "B");

		expect(calls).toHaveLength(0);
		expect(outcome).toEqual({ ok: false, error: "notification unavailable on this platform" });
	});

	it("does not throw when Termux:API is missing", async () => {
		process.env.TERMUX_VERSION = "0.118";
		vi.spyOn(Bun, "spawn").mockImplementation(() => {
			throw new Error("ENOENT: termux-notification not found");
		});

		const outcome = await sendAndroidNotification("Title", "Body");

		expect(outcome.ok).toBe(false);
		expect(outcome.error).toBeDefined();
	});

	it("returns the command error without leaking arguments or secrets", async () => {
		process.env.TERMUX_VERSION = "0.118";
		vi.spyOn(Bun, "spawn").mockImplementation((() => {
			return {
				stderr: new ReadableStream({
					start(controller) {
						controller.enqueue(new TextEncoder().encode("generic failure\n"));
						controller.close();
					},
				}),
				kill: () => {},
				exited: Promise.resolve(1),
			} as never;
		}) as unknown as typeof Bun.spawn);

		const outcome = await sendAndroidNotification("Safe Title", "Contains secret: ghp_1234567890abcdef");

		expect(outcome).toEqual({ ok: false, error: "generic failure" });
		expect(outcome.error).not.toContain("ghp_");
	});

	it("truncates title to 200 and content to 500 characters", async () => {
		const calls = captureAndroidSpawns();

		await sendAndroidNotification("t".repeat(400), "b".repeat(900));

		expect(calls[0]).toContain("t".repeat(200));
		expect(calls[0]).toContain("b".repeat(500));
	});

	it("reuses one per-process id so events overwrite instead of stacking", async () => {
		const calls = captureAndroidSpawns();

		await sendAndroidNotification("First", "Complete");
		await sendAndroidNotification("Second", "Waiting for input");
		await sendAndroidNotification("Custom", "Body", { id: "omp-custom" });

		expect(calls.map(args => args[2])).toEqual([statusId, statusId, "omp-custom"]);
	});

	it("clearAndroidNotification removes only the id it is given", async () => {
		const calls = captureAndroidSpawns();
		process.env.PI_NOTIFICATIONS = "off";

		await clearAndroidNotification(statusId);

		expect(calls).toEqual([["termux-notification-remove", statusId]]);
	});
});
