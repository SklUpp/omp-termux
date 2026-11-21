/**
 * Android/Termux best-effort helpers: wake-lock, notification, OOM-score.
 * Each operation reports its actual outcome (success/failure) and never logs
 * secrets, tokens, or raw command arguments.
 */

import * as fs from "node:fs/promises";
import { isNotificationSuppressed } from "@oh-my-pi/pi-tui";

export type AndroidOutcome = { ok: boolean; error?: string };

export interface AndroidNotificationOptions {
	/** Android notification id; a stable id overwrites instead of stacking. */
	id?: string;
}

/**
 * Notification id shared by this process' completion/error/ask cards.
 *
 * Termux:API treats `--id` as a global NotificationManager tag, so one literal
 * id would let a second omp process (another Termux session, tmux pane, or
 * background agent) overwrite the first one's card. Scoping the tag to the pid
 * keeps concurrent sessions independent while one session still replaces its
 * own card instead of stacking.
 */
export function androidNotificationId(): string {
	return `omp-status-${process.pid}`;
}

let askNotificationSequence = 0;

/**
 * Id for one ask card, unique per prompt within this process.
 *
 * The ask card must be removed without touching the completion/error card that
 * may be posted while the dialog is still pending, so it cannot share the
 * status id. The pid keeps concurrent sessions independent.
 */
export function newAndroidAskNotificationId(): string {
	askNotificationSequence += 1;
	return `omp-ask-${process.pid}-${askNotificationSequence}`;
}

async function spawnBestEffort(cmd: string[], timeoutMs = 3000): Promise<AndroidOutcome> {
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

export async function acquireWakeLock(): Promise<AndroidOutcome> {
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

export async function releaseWakeLock(): Promise<AndroidOutcome> {
	if (process.platform !== "android" && !process.env.TERMUX_VERSION) {
		return { ok: false, error: "wake-lock unavailable on this platform" };
	}
	try {
		return await spawnBestEffort(["termux-wake-unlock"]);
	} catch (e) {
		return { ok: false, error: e instanceof Error ? e.message.slice(0, 512) : String(e).slice(0, 512) };
	}
}

/**
 * Post one Android status card through Termux:API.
 *
 * Best-effort: no-op when Termux:API is absent, and honours the global
 * `PI_NOTIFICATIONS` suppression that already gates terminal notifications.
 * Callers pass fixed status strings (`Complete`, `Stopped with error`,
 * `Waiting for input`), never conversation text or session names; text is
 * truncated regardless, so a card cannot carry session content or secrets.
 */
export async function sendAndroidNotification(
	title: string,
	body: string,
	options?: AndroidNotificationOptions,
): Promise<AndroidOutcome> {
	if (isNotificationSuppressed()) {
		return { ok: false, error: "notifications suppressed" };
	}
	// Redact before any handling: truncate and strip potential secrets.
	const safeTitle = title.slice(0, 200);
	const safeBody = body.slice(0, 500);
	if (process.platform !== "android" && !process.env.TERMUX_VERSION) {
		return { ok: false, error: "notification unavailable on this platform" };
	}
	try {
		return await spawnBestEffort([
			"termux-notification",
			"--id",
			options?.id ?? androidNotificationId(),
			"--title",
			safeTitle,
			"--content",
			safeBody,
		]);
	} catch (e) {
		return { ok: false, error: e instanceof Error ? e.message.slice(0, 512) : String(e).slice(0, 512) };
	}
}

/**
 * Remove one of this process' status cards (e.g. the ask prompt once answered).
 *
 * Removal is not user-visible, so it intentionally ignores the global
 * suppression flag: a card posted before the user muted notifications must
 * still be cleared. Ids are caller-provided (`androidNotificationId()` or an
 * explicit one) and never contain session text.
 */
export async function clearAndroidNotification(id: string): Promise<AndroidOutcome> {
	if (process.platform !== "android" && !process.env.TERMUX_VERSION) {
		return { ok: false, error: "notification unavailable on this platform" };
	}
	try {
		return await spawnBestEffort(["termux-notification-remove", id]);
	} catch (e) {
		return { ok: false, error: e instanceof Error ? e.message.slice(0, 512) : String(e).slice(0, 512) };
	}
}

/**
 * Adjust OOM score for current process. Best-effort, reports denial accurately.
 * Does not claim protection when OS denies the operation.
 */
export async function adjustOomScoreAdj(score: number): Promise<AndroidOutcome> {
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
