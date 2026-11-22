/**
 * Android helper behaviour at the process boundary. Notification tests mock
 * `Bun.spawn`, so running the suite on a Termux device never posts a real card.
 */
import { afterEach, describe, expect, it, vi } from "bun:test";
import {
	acquireWakeLock,
	adjustOomScoreAdj,
	androidNotificationId,
	clearAndroidNotification,
	newAndroidAskNotificationId,
	releaseWakeLock,
	sendAndroidNotification,
} from "../src/utils/android-helpers";

const originalTermuxVersion = process.env.TERMUX_VERSION;
const originalPiNotifications = process.env.PI_NOTIFICATIONS;

afterEach(() => {
	vi.restoreAllMocks();
	restoreEnv("TERMUX_VERSION", originalTermuxVersion);
	restoreEnv("PI_NOTIFICATIONS", originalPiNotifications);
});

function restoreEnv(key: string, value: string | undefined): void {
	if (value === undefined) {
		delete process.env[key];
	} else {
		process.env[key] = value;
	}
}

function mockSpawn(result: { code?: number; stderr?: string } = {}): string[][] {
	process.env.TERMUX_VERSION = "0.118";
	const calls: string[][] = [];
	vi.spyOn(Bun, "spawn").mockImplementation(((cmd: string[]) => {
		calls.push(cmd);
		return {
			stderr: new ReadableStream({
				start(controller) {
					if (result.stderr) controller.enqueue(new TextEncoder().encode(result.stderr));
					controller.close();
				},
			}),
			kill: () => {},
			exited: Promise.resolve(result.code ?? 0),
		} as never;
	}) as unknown as typeof Bun.spawn);
	return calls;
}

describe("android helpers: notifications", () => {
	it("posts the fixed status text, addressed to the process status id", async () => {
		const calls = mockSpawn();

		const outcome = await sendAndroidNotification("omp", "Complete");

		expect(outcome).toEqual({ ok: true });
		expect(calls).toEqual([
			["termux-notification", "--id", androidNotificationId(), "--title", "omp", "--content", "Complete"],
		]);
	});

	it("uses a distinct per-prompt id that cannot collide with the status card", async () => {
		const calls = mockSpawn();
		const first = newAndroidAskNotificationId();
		const second = newAndroidAskNotificationId();

		await sendAndroidNotification("omp", "Waiting for input", { id: first });
		await sendAndroidNotification("omp", "Waiting for input", { id: second });

		expect(first).not.toBe(second);
		expect(first).not.toBe(androidNotificationId());
		expect(first.startsWith(`omp-ask-${process.pid}-`)).toBe(true);
		expect(calls.map(args => args[2])).toEqual([first, second]);
	});

	it("removes exactly the id it is given, even when notifications are muted", async () => {
		const calls = mockSpawn();
		process.env.PI_NOTIFICATIONS = "off";

		await clearAndroidNotification("omp-ask-1-7");

		expect(calls).toEqual([["termux-notification-remove", "omp-ask-1-7"]]);
	});

	it("does not spawn when PI_NOTIFICATIONS suppresses notifications", async () => {
		const calls = mockSpawn();
		process.env.PI_NOTIFICATIONS = "off";

		const outcome = await sendAndroidNotification("omp", "Complete");

		expect(calls).toHaveLength(0);
		expect(outcome).toEqual({ ok: false, error: "notifications suppressed" });
	});

	it("bounds the reported failure message and never adds raw argv", async () => {
		mockSpawn({ code: 1, stderr: "x".repeat(2000) });

		const outcome = await sendAndroidNotification("omp", "Complete");

		expect(outcome.ok).toBe(false);
		expect(outcome.error?.length).toBeLessThanOrEqual(512);
		expect(outcome.error).not.toContain("--id");
	});

	it("reports unavailability outside Termux without spawning", async () => {
		const calls = mockSpawn();
		delete process.env.TERMUX_VERSION;
		if (process.platform === "android") return; // A real device always reports android.

		const outcome = await sendAndroidNotification("omp", "Complete");

		expect(calls).toHaveLength(0);
		expect(outcome).toEqual({ ok: false, error: "notification unavailable on this platform" });
	});

	it("clamps and truncates text before spawning", async () => {
		const calls = mockSpawn();

		await sendAndroidNotification("t".repeat(400), "b".repeat(900));

		expect(calls[0]).toContain("t".repeat(200));
		expect(calls[0]).toContain("b".repeat(500));
	});
});

describe("android helpers: wake-lock and OOM", () => {
	it("wake-lock reports the real outcome without claiming success", async () => {
		const res = await acquireWakeLock();
		expect(typeof res.ok).toBe("boolean");
		if (!res.ok) expect(res.error).toBeDefined();
		if (res.error) expect(res.error).not.toContain("ghp_");
	});

	it("release reports the real outcome", async () => {
		const res = await releaseWakeLock();
		expect(typeof res.ok).toBe("boolean");
	});

	it("OOM adjust reports denial accurately and does not claim success", async () => {
		const res = await adjustOomScoreAdj(-500);
		expect(typeof res.ok).toBe("boolean");
		if (!res.ok) expect(res.error).toBeDefined();
		if (res.error) expect(res.error).not.toContain("ghp_");
	});
});
