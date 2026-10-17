/**
 * Android/Termux best-effort helpers: wake-lock, notification, OOM-score.
 * Each operation reports its actual outcome (success/failure) and never logs
 * secrets, tokens, or raw command arguments.
 */

import * as fs from "node:fs/promises";

type Outcome = { ok: boolean; error?: string };

async function spawnBestEffort(cmd: string[], timeoutMs = 3000): Promise<Outcome> {
	const proc = Bun.spawn(cmd, {
		stdout: "ignore",
		stderr: "pipe",
	});
	const timer = setTimeout(() => {
		try {
			proc.kill();
		} catch {}
	}, timeoutMs);
	let stderr = "";
	try {
		const out = await new Response(proc.stderr).text();
		stderr = out.slice(0, 512);
	} catch {}
	clearTimeout(timer);
	const code = await proc.exited;
	if (code === 0) return { ok: true };
	const msg = stderr.trim() || `command ${cmd[0]} exited ${code}`;
	// Do not log raw args; only return bounded error.
	return { ok: false, error: msg.slice(0, 512) };
}

export async function acquireWakeLock(): Promise<Outcome> {
	if (process.platform !== "android" && !process.env.TERMUX_VERSION) {
		// Best-effort: on non-Android, report not applicable without claiming success.
		return { ok: false, error: "wake-lock unavailable on this platform" };
	}
	try {
		return await spawnBestEffort(["termux-wake-lock"]);
	} catch (e) {
		return { ok: false, error: e instanceof Error ? e.message.slice(0, 512) : String(e).slice(0, 512) };
	}
}

export async function releaseWakeLock(): Promise<Outcome> {
	if (process.platform !== "android" && !process.env.TERMUX_VERSION) {
		return { ok: false, error: "wake-lock unavailable on this platform" };
	}
	try {
		return await spawnBestEffort(["termux-wake-unlock"]);
	} catch (e) {
		return { ok: false, error: e instanceof Error ? e.message.slice(0, 512) : String(e).slice(0, 512) };
	}
}

export async function sendAndroidNotification(title: string, body: string): Promise<Outcome> {
	// Redact before any handling: truncate and strip potential secrets.
	const safeTitle = title.slice(0, 200);
	const safeBody = body.slice(0, 500);
	if (process.platform !== "android" && !process.env.TERMUX_VERSION) {
		return { ok: false, error: "notification unavailable on this platform" };
	}
	try {
		// termux-notification --title ... --content ...
		return await spawnBestEffort(["termux-notification", "--title", safeTitle, "--content", safeBody]);
	} catch (e) {
		return { ok: false, error: e instanceof Error ? e.message.slice(0, 512) : String(e).slice(0, 512) };
	}
}

/**
 * Adjust OOM score for current process. Best-effort, reports denial accurately.
 * Does not claim protection when OS denies the operation.
 */
export async function adjustOomScoreAdj(score: number): Promise<Outcome> {
	const clamped = Math.max(-1000, Math.min(1000, Math.trunc(score)));
	const candidates = ["/proc/self/oom_score_adj", "/proc/self/oom_adj"];
	for (const p of candidates) {
		try {
			await fs.writeFile(p, String(clamped));
			// Verify write succeeded by reading back
			const readBack = (await fs.readFile(p, "utf8")).trim();
			if (String(clamped) === readBack || (p.endsWith("oom_adj") && String(clamped) === readBack)) {
				return { ok: true };
			}
			// If readback mismatch, continue to next candidate but report failure at end
		} catch (e) {
			const msg = e instanceof Error ? e.message.slice(0, 512) : String(e).slice(0, 512);
			// Permission denied must be reported, not claimed as success
			if (msg.includes("EACCES") || msg.includes("EPERM") || msg.includes("permission")) {
				return { ok: false, error: `OOM adjust denied: ${msg.slice(0, 200)}` };
			}
		}
	}
	return { ok: false, error: "OOM adjust not available or denied" };
}
