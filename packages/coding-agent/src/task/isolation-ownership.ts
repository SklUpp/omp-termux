/**
 * Ownership marker for task-isolation sandboxes under `~/.omp/wt/`.
 *
 * Each isolation base dir (`ensureIsolation` in {@link ./worktree}) holds a
 * compact `m` mount plus this marker file naming the omp process that created
 * it. `omp worktree clear` consults the marker so it can distinguish a live
 * subagent's sandbox from a crashed run's leftover instead of deleting both.
 */
import * as fs from "node:fs/promises";
import * as path from "node:path";
import * as natives from "@oh-my-pi/pi-natives";
import { acquireFileLock, type FileLockHandle, getWorktreesDir } from "@oh-my-pi/pi-utils";
import { $ } from "bun";

const { IsoBackendKind } = natives;

/** Marker file written into a task-isolation base dir identifying its owner. */
export const ISOLATION_OWNER_FILE = ".omp-isolation-owner.json";

/** Recorded owner of a task-isolation sandbox. */
export interface IsolationOwner {
	/** PID of the omp process that created and owns the sandbox. */
	pid: number;
	/** Task id the sandbox was materialised for. */
	id: string;
	/**
	 * Process-instance start-time token for {@link pid}, when the OS can report
	 * it. Distinguishes the owning process from an unrelated process that later
	 * inherits a recycled pid, so a crashed sandbox is never pinned live.
	 */
	startToken?: string;
}

/**
 * Boot-stable start-time token for `pid`, or `null` when the process is gone or
 * the platform cannot report it. Read from the same source on write and
 * validate so an exact string compare rejects a recycled pid.
 *
 * Linux reads `/proc/<pid>/stat` field 22 (start time in clock ticks since
 * boot); other Unixes shell out to `ps -o lstart`. Platforms that report
 * neither (e.g. Windows) yield `null`, degrading to a pid-only liveness check.
 */
async function processStartToken(pid: number): Promise<string | null> {
	if (process.platform === "linux") {
		let stat: string;
		try {
			stat = await Bun.file(`/proc/${pid}/stat`).text();
		} catch {
			return null;
		}
		// The comm field (2) may embed spaces and parens, so parse the numeric
		// fields after the final ')'. `starttime` is field 22 overall, i.e. the
		// 20th token once `pid` and `(comm)` are dropped.
		const commEnd = stat.lastIndexOf(")");
		if (commEnd < 0) return null;
		const starttime = stat.slice(commEnd + 2).split(" ")[19];
		return starttime && starttime.length > 0 ? starttime : null;
	}
	const res = await $`ps -o lstart= -p ${pid}`.quiet().nothrow();
	if (res.exitCode !== 0) return null;
	const started = res.text().trim();
	return started.length > 0 ? started : null;
}

/**
 * Record the current process as owner of the sandbox rooted at `baseDir`.
 *
 * Written before the isolation backend materialises `m` so a concurrent
 * `omp worktree clear` never sees an owner-less sandbox mid-creation.
 */
export async function writeIsolationOwner(baseDir: string, id: string): Promise<void> {
	const startToken = await processStartToken(process.pid);
	const owner: IsolationOwner = { pid: process.pid, id, ...(startToken ? { startToken } : {}) };
	await Bun.write(path.join(baseDir, ISOLATION_OWNER_FILE), JSON.stringify(owner));
}

/**
 * Parsed owner marker of `baseDir`, or undefined when absent/malformed. Lets a
 * cold revive tell "no verifiable owner" apart from "another live process owns
 * this workspace" before it rewrites the marker.
 */
export async function readIsolationOwner(baseDir: string): Promise<IsolationOwner | undefined> {
	let decoded: unknown;
	try {
		decoded = await Bun.file(path.join(baseDir, ISOLATION_OWNER_FILE)).json();
	} catch {
		return undefined;
	}
	if (typeof decoded !== "object" || decoded === null || !("pid" in decoded)) return undefined;
	const pid = decoded.pid;
	if (typeof pid !== "number" || !Number.isInteger(pid) || pid <= 0) return undefined;
	const id = "id" in decoded && typeof decoded.id === "string" ? decoded.id : "";
	const startToken =
		"startToken" in decoded && typeof decoded.startToken === "string" && decoded.startToken.length > 0
			? decoded.startToken
			: undefined;
	return { pid, id, ...(startToken ? { startToken } : {}) };
}

/**
 * Whether a live omp process still owns the sandbox at `baseDir`.
 *
 * A missing or malformed marker means no verifiable owner — a crashed run or a
 * sandbox from before markers existed, both safe to reclaim. See
 * {@link isProcessAliveWithToken} for the recycled-pid rule.
 */
export async function hasLiveIsolationOwner(baseDir: string): Promise<boolean> {
	const owner = await readIsolationOwner(baseDir);
	return owner ? isProcessAliveWithToken(owner.pid, owner.startToken) : false;
}

/**
 * Liveness of a recorded pid, with recycled-pid rejection: `process.kill(pid, 0)`
 * can fail with `EPERM` for a live process, so only `ESRCH` counts as dead, and
 * a pinned start-time token that no longer matches proves the pid was recycled.
 */
async function isProcessAliveWithToken(pid: number, startToken?: string): Promise<boolean> {
	try {
		process.kill(pid, 0);
	} catch (err) {
		if ((err as NodeJS.ErrnoException).code === "ESRCH") return false;
	}
	if (startToken) {
		const current = await processStartToken(pid);
		if (current !== null && current !== startToken) return false;
	}
	return true;
}

/** Sidecar recording the native-teardown backend of a retained workspace. */
export const RETAINED_BACKEND_FILE = ".omp-retained-backend.json";

/**
 * Backends whose workspaces `omp worktree clear` must not remove with plain
 * recursive `rm`, but route through native `isoStop` teardown instead:
 * mounts (overlayfs, projfs), where `rm` destroys the preserved layer and
 * fails on the mountpoint, and Btrfs subvolumes, whose root is only removable
 * via subvolume delete (its `stop` falls back to plain `rm` for ordinary
 * dirs, so routing it is always safe). Plain-copy and reflink backends need
 * no teardown — and their `stop` routines delete data themselves, so they
 * must never be routed through it.
 *
 * Notably absent: ZFS clones also need dataset-aware teardown, but `isoStop`
 * locates the dataset by its recorded mountpoint property, which no longer
 * matches after the retain rename — that needs pi-iso mount-table support
 * before a sidecar here could help.
 */
export function needsNativeTeardown(backend: unknown): backend is number {
	return backend === IsoBackendKind.Overlayfs || backend === IsoBackendKind.Projfs || backend === IsoBackendKind.Btrfs;
}

/**
 * Record which backend built a retained workspace, so cleanup can route it
 * through native teardown before removal. Best-effort: retention stays valid
 * without it (the workspace merely falls back to plain recursive removal).
 */
export async function writeRetainedBackend(baseDir: string, backend: number): Promise<void> {
	await Bun.write(
		path.join(baseDir, RETAINED_BACKEND_FILE),
		JSON.stringify({ backend, retainedAt: new Date().toISOString() }),
	);
}

/**
 * Backend recorded for a retained workspace when it needs native teardown
 * before remove. `undefined` only when no sidecar exists. Any other read
 * problem (permissions, I/O, malformed JSON) throws instead of reading as
 * absent: the workspace may be a live mount whose guard must not be silently
 * bypassed — the caller then leaves it in place rather than removing through
 * the mount. A well-formed record for a backend needing no teardown keeps
 * today's removal behavior.
 */
export async function readRetainedMountBackend(dir: string): Promise<number | undefined> {
	const sidecar = path.join(dir, RETAINED_BACKEND_FILE);
	if (!(await Bun.file(sidecar).exists())) return undefined;
	const decoded: unknown = await Bun.file(sidecar).json();
	if (typeof decoded !== "object" || decoded === null || !("backend" in decoded)) {
		throw new Error(`retained-mount metadata at ${sidecar} is malformed`);
	}
	const backend = decoded.backend;
	if (typeof backend !== "number" || !Number.isInteger(backend)) {
		throw new Error(`retained-mount metadata at ${sidecar} is malformed`);
	}
	if (!needsNativeTeardown(backend)) return undefined;
	return backend;
}

/** Lease sidecars live OUTSIDE the sandbox: retention renames must never move a held lock. */
function isolationLeaseDir(): string {
	return path.join(path.dirname(getWorktreesDir()), ".isolation-leases");
}

/**
 * Immutable lease key: the derived slot, shared by the original and every
 * `.retained-*` sibling. Mirrors `getTaskIsolationSlot` in `./worktree`, which
 * imports this module, so the shape is restated instead of imported.
 */
const ISOLATION_SLOT_RE = /^t[0-9a-f]{9}$/;

function isolationLeaseTarget(slot: string): string {
	if (!ISOLATION_SLOT_RE.test(slot)) throw new Error(`invalid isolation slot "${slot}"`);
	return path.join(isolationLeaseDir(), slot);
}

/** Raised when another process holds the slot lease; callers must preserve the sandbox. */
export class IsolationLeaseBusyError extends Error {
	constructor(slot: string) {
		super(`another omp process holds the isolation lease for ${slot}`);
		this.name = "IsolationLeaseBusyError";
	}
}

/**
 * Run `fn` while holding the slot's OS-backed lease.
 *
 * Reuses the repository's file-lock primitive, keyed by the immutable slot in a
 * directory outside the sandbox, so a retention rename never breaks the inode
 * a holder locked and a crash releases the lease with its process. The sidecar
 * is never unlinked (that can split one lease across two inodes). Probes are
 * deliberately absent: `isFileLockHeld` reports unsupported on some platforms,
 * so callers attempt the real acquisition and treat contention as "preserve".
 */
export async function withIsolationLease<T>(slot: string, fn: () => Promise<T>): Promise<T> {
	const target = isolationLeaseTarget(slot);
	await fs.mkdir(path.dirname(target), { recursive: true, mode: 0o700 });
	let handle: FileLockHandle;
	try {
		// One attempt: contention means another process owns the handover now.
		handle = await acquireFileLock(target, { retries: 1 });
	} catch {
		throw new IsolationLeaseBusyError(slot);
	}
	try {
		return await fn();
	} finally {
		handle.release();
	}
}

/**
 * Claim the retained sandbox for this process, atomically across processes.
 *
 * The owner marker is what `omp worktree clear` consults, so a cold/warm revive
 * must refresh it: otherwise the revived run looks owner-less and `clear --all`
 * could delete a workspace a live session is writing. Read-check-write happens
 * under the slot lease, so two simultaneous revives cannot both believe they
 * own the sandbox; the loser refuses, and a marker naming another live process
 * is never stolen. A dead owner (crash, previous boot) is reclaimed.
 */
export async function claimIsolationOwner(baseDir: string, id: string, slot: string): Promise<void> {
	await withIsolationLease(slot, async () => {
		const owner = await readIsolationOwner(baseDir);
		if (owner && owner.pid !== process.pid && (await isProcessAliveWithToken(owner.pid, owner.startToken))) {
			throw new Error(
				`isolation workspace ${baseDir} is owned by live omp process ${owner.pid}; finish or release it there before reviving this agent`,
			);
		}
		await writeIsolationOwner(baseDir, id);
	});
}
