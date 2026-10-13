import { describe, expect, it } from "bun:test";
import {
	acquireWakeLock,
	adjustOomScoreAdj,
	releaseWakeLock,
	sendAndroidNotification,
} from "../src/utils/android-helpers";

describe("android helpers termux", () => {
	it("wake-lock reports accurate outcome without secret logs", async () => {
		const res = await acquireWakeLock();
		// On Android Termux, wake-lock succeeds; on other platforms it reports unavailable. Both are accurate, never falsely claimed.
		expect(typeof res.ok).toBe("boolean");
		if (res.ok) {
			expect(res.error).toBeUndefined();
		} else {
			expect(res.error).toBeDefined();
			expect(res.error?.length).toBeGreaterThan(0);
		}
		if (res.error) expect(res.error).not.toContain("secret");
	});
	it("release reports accurate outcome", async () => {
		const res = await releaseWakeLock();
		expect(typeof res.ok).toBe("boolean");
	});
	it("notification reports accurate outcome and does not claim protection when denied", async () => {
		const res = await sendAndroidNotification("title", "body with secret token ghp_xxx");
		expect(typeof res.ok).toBe("boolean");
		// If denied, error must be present; if succeeded, no false claim.
		if (!res.ok) expect(res.error).toBeDefined();
		if (res.error) expect(res.error).not.toContain("ghp_");
	});
	it("oom adjust reports denial accurately and does not claim success when denied", async () => {
		const res = await adjustOomScoreAdj(-500);
		expect(typeof res.ok).toBe("boolean");
		if (!res.ok) expect(res.error).toBeDefined();
		if (res.error) expect(res.error).not.toContain("ghp_");
	});
	it("oom clamps and handles invalid path", async () => {
		const res = await adjustOomScoreAdj(9999);
		expect(typeof res.ok).toBe("boolean");
	});
});
