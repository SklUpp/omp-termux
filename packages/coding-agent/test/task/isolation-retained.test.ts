import { afterEach, describe, expect, it, vi } from "bun:test";
import { type } from "@oh-my-pi/omptype";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";
import * as natives from "@oh-my-pi/pi-natives";
import { getWorktreesDir, setWorktreesDir } from "@oh-my-pi/pi-utils";
import { clearWorktrees, stopRetainedMount } from "@oh-my-pi/pi-coding-agent/cli/worktree-cli";
import { AgentRegistry } from "@oh-my-pi/pi-coding-agent/registry/agent-registry";
import * as executorModule from "@oh-my-pi/pi-coding-agent/task/executor";
import {
	claimIsolationOwner,
	ISOLATION_OWNER_FILE,
	readIsolationOwner,
	writeIsolationOwner,
} from "@oh-my-pi/pi-coding-agent/task/isolation-ownership";
import {
	prepareIsolationContext,
	releaseIsolationWorkspace,
	runIsolatedSubprocess,
} from "@oh-my-pi/pi-coding-agent/task/isolation-runner";
import * as vcsModule from "@oh-my-pi/pi-natives/vcs";
import { getTaskSchema } from "@oh-my-pi/pi-coding-agent/task/types";
import * as worktreeModule from "@oh-my-pi/pi-coding-agent/task/worktree";
import {
	assertIsolationWorkspaceReleased,
	ensureIsolation,
	getTaskIsolationSlot,
	ISOLATION_STATE_FILE,
	type IsolationState,
	type IsolationStateRead,
	type RepoBaseline,
	isolationStateExists,
	readIsolationState,
	resolveIsolationWorkspace,
	type WorktreeBaseline,
	writeIsolationState,
} from "@oh-my-pi/pi-coding-agent/task/worktree";
import type { SingleResult } from "@oh-my-pi/pi-tui/tools/task";

const tempRoots: string[] = [];

afterEach(async () => {
	vi.restoreAllMocks();
	AgentRegistry.resetGlobalForTests();
	await Promise.all(tempRoots.splice(0).map(dir => fs.rm(dir, { recursive: true, force: true })));
});

async function tempDir(prefix: string): Promise<string> {
	const dir = await fs.mkdtemp(path.join(os.tmpdir(), prefix));
	tempRoots.push(dir);
	return dir;
}

function repoBaselineFor(repoRoot: string): RepoBaseline {
	return {
		repoRoot,
		headCommit: "a".repeat(40),
		staged: "",
		unstaged: "diff --git a/file.txt b/file.txt",
		untracked: ["notes.txt"],
		untrackedPatch: "diff --git a/notes.txt b/notes.txt",
	};
}

function baselineFor(repoRoot: string): WorktreeBaseline {
	return { root: repoBaselineFor(repoRoot), nested: [] };
}

function result(id: string): SingleResult {
	return {
		id,
		agent: "task",
		task: "t",
		description: "d",
		index: 0,
		exitCode: 0,
		aborted: false,
		output: "done",
		durationMs: 1,
		agentSource: "bundled",
		stderr: "",
		truncated: false,
		tokens: 0,
		requests: 0,
	};
}

/** A pid that has been spawned and reaped, so `kill(pid, 0)` reports ESRCH. */
async function deadPid(): Promise<number> {
	const proc = Bun.spawn(["true"], { stdout: "ignore", stderr: "ignore" });
	await proc.exited;
	return proc.pid;
}

async function readLine(stream: ReadableStream<Uint8Array>): Promise<string> {
	const reader = stream.getReader();
	const decoder = new TextDecoder();
	let text = "";
	while (!text.includes("\n")) {
		const { value, done } = await reader.read();
		if (done) break;
		text += decoder.decode(value);
	}
	return text;
}

/** Child worker kept in-repo so its relative imports resolve in CI. */
const leaseChildScript = path.join(import.meta.dir, "..", "fixtures", "isolation-lease-child.ts");

/**
 * First line a child prints, or its exit code and stderr when it dies first:
 * a crashed child must never look like a silent hang.
 */
async function childVerdict(child: {
	stdout: ReadableStream<Uint8Array>;
	stderr: ReadableStream<Uint8Array>;
	exited: Promise<number>;
}): Promise<string> {
	return await Promise.race([
		readLine(child.stdout),
		child.exited.then(async code => `child exited (${code}): ${(await new Response(child.stderr).text()).trim()}`),
	]);
}

/** Lease worktrees root is per-process; the env override pins the parent's root. */
function spawnLeaseChild(args: string[]) {
	return Bun.spawn([process.execPath, leaseChildScript, ...args], {
		stdin: "pipe",
		stdout: "pipe",
		stderr: "pipe",
		env: { ...process.env, OMP_WORKTREE_DIR: getWorktreesDir() },
	});
}

/**
 * Hold the sandbox slot lease in a real child process until `release()` is
 * called. The child stays alive on stdin, so no wall-clock wait is involved.
 */
async function holdClaimInChild(slot: string): Promise<{ release: () => Promise<void> }> {
	const child = spawnLeaseChild(["hold", slot]);
	const verdict = await childVerdict(child);
	if (!verdict.includes("held")) {
		child.kill();
		throw new Error(`child did not hold the claim: ${verdict}`);
	}
	return {
		release: async () => {
			// End the pipe, not just write: the worker resumed stdin, so an open
			// pipe is a live handle and `exited` would never resolve.
			child.stdin.write("x\n");
			child.stdin.end();
			await child.exited;
		},
	};
}

interface IsolationFixture {
	root: string;
	repo: string;
	id: string;
	slot: string;
	base: string;
	workspace: string;
	artifactsDir: string;
	sessionFile: string;
	baseline: WorktreeBaseline;
}

/**
 * A descriptor that passes every binding check: sandbox under the worktrees
 * root in its derived slot, matching repo, and a real transcript file.
 */
async function isolationFixture(options?: { backend?: number; id?: string }): Promise<IsolationFixture> {
	const root = await tempDir("omp-iso-root-");
	setWorktreesDir(root);
	const repo = await tempDir("omp-iso-repo-");
	const id = options?.id ?? "agent-1";
	const slot = getTaskIsolationSlot(repo, id);
	const base = path.join(root, slot);
	const workspace = path.join(base, "m");
	await fs.mkdir(workspace, { recursive: true });
	const artifactsDir = await tempDir("omp-iso-artifacts-");
	const sessionFile = path.join(artifactsDir, `${id}.jsonl`);
	await Bun.write(sessionFile, "");
	await writeIsolationState(base, {
		version: 1,
		id,
		repoRoot: repo,
		backend: options?.backend ?? natives.IsoBackendKind.Rcopy,
		slot,
		mergedDir: "m",
		baseline: baselineFor(repo),
		sessionFile,
	});
	// A retained sandbox in production always carries the owner marker written at
	// spawn/claim; release requires this process to own it.
	await writeIsolationOwner(base, id);
	return { root, repo, id, slot, base, workspace, artifactsDir, sessionFile, baseline: baselineFor(repo) };
}

function loadedState(read: IsolationStateRead): IsolationState & { dir: string } {
	if (!("state" in read)) throw new Error(read.detail);
	return read.state;
}

describe("isolation state descriptor", () => {
	it("round-trips the baseline verbatim and validates on read", async () => {
		const base = await tempDir("omp-iso-state-");
		const transcript = path.join(base, "agent-1.jsonl");
		await Bun.write(transcript, "");
		const baseline = baselineFor(base);
		const state: IsolationState = {
			version: 1,
			id: "agent-1",
			repoRoot: base,
			backend: natives.IsoBackendKind.Rcopy,
			slot: "t1",
			mergedDir: "m",
			baseline,
			sessionFile: transcript,
			handedOff: "1234",
		};
		await writeIsolationState(base, state);
		expect(await isolationStateExists(base)).toBe(true);
		expect(await readIsolationState(base, "agent-1")).toEqual({ state: { ...state, dir: base } });

		// Another agent's id is a different workspace, not a free slot.
		const mismatch = await readIsolationState(base, "agent-2");
		expect("error" in mismatch && mismatch.error).toBe("mismatch");
	});

	it("refuses corrupt, missing, traversal-bearing, and unknown-backend descriptors", async () => {
		const base = await tempDir("omp-iso-state-bad-");
		const file = path.join(base, ISOLATION_STATE_FILE);
		const transcript = path.join(base, "agent-1.jsonl");
		await Bun.write(transcript, "");

		expect(await readIsolationState(base)).toMatchObject({ error: "missing" });

		await Bun.write(file, "{not json");
		expect(await readIsolationState(base)).toMatchObject({ error: "corrupt" });

		const valid = {
			version: 1,
			id: "agent-1",
			repoRoot: base,
			backend: natives.IsoBackendKind.Rcopy,
			slot: "t1",
			mergedDir: "m",
			baseline: baselineFor(base),
			sessionFile: transcript,
		};
		await Bun.write(file, JSON.stringify({ ...valid, mergedDir: "../escape" }));
		expect(await readIsolationState(base)).toMatchObject({ error: "corrupt" });

		await Bun.write(file, JSON.stringify({ ...valid, backend: 99 }));
		expect(await readIsolationState(base)).toMatchObject({ error: "corrupt" });

		const escapingNested = structuredClone(valid.baseline);
		escapingNested.nested.push({
			relativePath: "../outside",
			baseline: repoBaselineFor(path.join(base, "outside")),
		});
		await Bun.write(file, JSON.stringify({ ...valid, baseline: escapingNested }));
		expect(await readIsolationState(base)).toMatchObject({ error: "corrupt" });

		const mismatchedNested = structuredClone(valid.baseline);
		mismatchedNested.nested.push({
			relativePath: "sub",
			baseline: repoBaselineFor(path.join(base, "elsewhere")),
		});
		await Bun.write(file, JSON.stringify({ ...valid, baseline: mismatchedNested }));
		expect(await readIsolationState(base)).toMatchObject({ error: "corrupt" });
	});
});

describe("cold release", () => {
	it("captures against the persisted baseline, commits changed work, and tears down the recorded backend", async () => {
		const fixture = await isolationFixture();
		const state = loadedState(await readIsolationState(fixture.base, fixture.id));
		const capture = vi
			.spyOn(worktreeModule, "captureDeltaPatch")
			.mockResolvedValue({ rootPatch: "diff --git a/file.txt b/file.txt", nestedPatches: [] });
		const commit = vi.spyOn(worktreeModule, "commitToBranch").mockResolvedValue(null);
		const cleanup = vi.spyOn(worktreeModule, "cleanupIsolation").mockResolvedValue();

		await releaseIsolationWorkspace(state, {
			artifactsDir: fixture.artifactsDir,
			expectedSessionFile: fixture.sessionFile,
		});

		expect(capture).toHaveBeenCalledWith(fixture.workspace, fixture.baseline);
		expect(commit).toHaveBeenCalledTimes(1);
		expect(cleanup).toHaveBeenCalledWith(
			expect.objectContaining({ mergedDir: fixture.workspace, backend: natives.IsoBackendKind.Rcopy }),
		);
	});

	it("skips the duplicate commit when nothing changed since the run-end handoff", async () => {
		const fixture = await isolationFixture();
		const handedOff = String(Bun.hash("same delta"));
		await writeIsolationState(fixture.base, {
			version: 1,
			id: fixture.id,
			repoRoot: fixture.repo,
			backend: natives.IsoBackendKind.Rcopy,
			slot: fixture.slot,
			mergedDir: "m",
			baseline: fixture.baseline,
			sessionFile: fixture.sessionFile,
			handedOff,
		});
		const state = loadedState(await readIsolationState(fixture.base, fixture.id));
		vi.spyOn(worktreeModule, "captureDeltaPatch").mockResolvedValue({ rootPatch: "same delta", nestedPatches: [] });
		const commit = vi.spyOn(worktreeModule, "commitToBranch").mockResolvedValue(null);
		const cleanup = vi.spyOn(worktreeModule, "cleanupIsolation").mockResolvedValue();

		await releaseIsolationWorkspace(state, {
			artifactsDir: fixture.artifactsDir,
			expectedSessionFile: fixture.sessionFile,
		});

		expect(commit).not.toHaveBeenCalled();
		expect(cleanup).toHaveBeenCalledTimes(1);
	});

	it("retains the workspace when the branch commit fails after a successful capture", async () => {
		const fixture = await isolationFixture();
		const state = loadedState(await readIsolationState(fixture.base, fixture.id));
		const gitInit = Bun.spawnSync(["git", "init", "-q", fixture.repo]);
		if (gitInit.exitCode !== 0) throw new Error(gitInit.stderr.toString());
		vi.spyOn(worktreeModule, "captureDeltaPatch").mockResolvedValue({
			rootPatch: "diff --git a/file.txt b/file.txt",
			nestedPatches: [],
		});
		vi.spyOn(worktreeModule, "commitToBranch").mockRejectedValue(new Error("git commit failed"));
		const cleanup = vi.spyOn(worktreeModule, "cleanupIsolation").mockResolvedValue();

		await expect(
			releaseIsolationWorkspace(state, {
				artifactsDir: fixture.artifactsDir,
				expectedSessionFile: fixture.sessionFile,
			}),
		).rejects.toThrow(/commit failed|retained/);

		expect(cleanup).not.toHaveBeenCalled();
		const retained = (await fs.readdir(fixture.root)).filter(name => name.startsWith(`${fixture.slot}.retained-`));
		expect(retained).toHaveLength(1);
	});

	it("retains the workspace and skips cleanup when capture fails", async () => {
		const fixture = await isolationFixture();
		const state = loadedState(await readIsolationState(fixture.base, fixture.id));
		vi.spyOn(worktreeModule, "captureDeltaPatch").mockRejectedValue(new Error("disk full"));
		const cleanup = vi.spyOn(worktreeModule, "cleanupIsolation").mockResolvedValue();

		await expect(
			releaseIsolationWorkspace(state, {
				artifactsDir: fixture.artifactsDir,
				expectedSessionFile: fixture.sessionFile,
			}),
		).rejects.toThrow(/retained|disk full/);

		expect(cleanup).not.toHaveBeenCalled();
		expect(await Bun.file(path.join(fixture.base, ISOLATION_STATE_FILE)).exists()).toBe(false);
		const retained = (await fs.readdir(fixture.root)).filter(name => name.startsWith(`${fixture.slot}.retained-`));
		expect(retained).toHaveLength(1);
		// The transcript pointer follows the rename so a later revive can release it.
		const pointer: unknown = await Bun.file(path.join(fixture.artifactsDir, "agent-1.isolation.json")).json();
		if (pointer === null || typeof pointer !== "object" || !("workspace" in pointer)) {
			throw new Error("expected a retained-workspace pointer");
		}
		expect(pointer.workspace).toBe(path.join(fixture.root, retained[0], "m"));
	});

	it("refuses to release a descriptor bound to another transcript or slot", async () => {
		const fixture = await isolationFixture();
		const otherTranscript = path.join(fixture.artifactsDir, "other.jsonl");
		await Bun.write(otherTranscript, "");
		const forged = loadedState(await readIsolationState(fixture.base, fixture.id));
		const capture = vi
			.spyOn(worktreeModule, "captureDeltaPatch")
			.mockResolvedValue({ rootPatch: "", nestedPatches: [] });
		const cleanup = vi.spyOn(worktreeModule, "cleanupIsolation").mockResolvedValue();

		await expect(
			releaseIsolationWorkspace(
				{ ...forged, sessionFile: otherTranscript },
				{ artifactsDir: fixture.artifactsDir, expectedSessionFile: fixture.sessionFile },
			),
		).rejects.toThrow(/belongs to transcript/);
		expect(capture).not.toHaveBeenCalled();
		expect(cleanup).not.toHaveBeenCalled();

		vi.restoreAllMocks();
		const wrongSlot = loadedState(await readIsolationState(fixture.base, fixture.id));
		await expect(
			releaseIsolationWorkspace(
				{ ...wrongSlot, slot: "t-unrelated" },
				{ artifactsDir: fixture.artifactsDir, expectedSessionFile: fixture.sessionFile },
			),
		).rejects.toThrow(/slot|not slot/);
	});

	it("refuses a destructive release when this process does not own the sandbox", async () => {
		const fixture = await isolationFixture();
		await fs.rm(path.join(fixture.base, ISOLATION_OWNER_FILE), { force: true });
		const state = loadedState(await readIsolationState(fixture.base, fixture.id));
		const capture = vi.spyOn(worktreeModule, "captureDeltaPatch").mockResolvedValue({
			rootPatch: "",
			nestedPatches: [],
		});
		const cleanup = vi.spyOn(worktreeModule, "cleanupIsolation").mockResolvedValue();

		await expect(
			releaseIsolationWorkspace(state, {
				artifactsDir: fixture.artifactsDir,
				expectedSessionFile: fixture.sessionFile,
			}),
		).rejects.toThrow(/not claimed by any process/);
		expect(capture).not.toHaveBeenCalled();
		expect(cleanup).not.toHaveBeenCalled();
	});

	it("recovers a renamed sandbox without a pointer and refuses two bound siblings", async () => {
		const fixture = await isolationFixture();
		const retained = `${fixture.base}.retained-1-abc`;
		await fs.rename(fixture.base, retained);
		// No pointer beside the transcript: the deterministic retained name must
		// still resolve after a failed/crashed pointer write.
		await expect(
			resolveIsolationWorkspace(fixture.sessionFile, fixture.id, fixture.workspace),
		).resolves.toMatchObject({ workspaceDir: path.join(retained, "m") });

		// A second valid copy under the same slot makes the choice ambiguous:
		// reviving into a guess could capture the wrong workspace.
		const twin = `${fixture.base}.retained-2-def`;
		await fs.mkdir(path.join(twin, "m"), { recursive: true });
		await writeIsolationState(twin, {
			version: 1,
			id: fixture.id,
			repoRoot: fixture.repo,
			backend: natives.IsoBackendKind.Rcopy,
			slot: fixture.slot,
			mergedDir: "m",
			baseline: fixture.baseline,
			sessionFile: fixture.sessionFile,
		});
		await expect(
			resolveIsolationWorkspace(fixture.sessionFile, fixture.id, fixture.workspace),
		).resolves.toMatchObject({ refusal: expect.stringMatching(/refusing to guess/) });
	});

	it("refuses a workspace whose mount dir is a symlink escaping the sandbox", async () => {
		const fixture = await isolationFixture();
		const outside = await tempDir("omp-iso-outside-");
		await fs.rm(fixture.workspace, { recursive: true, force: true });
		await fs.symlink(outside, fixture.workspace, "dir");

		await expect(
			resolveIsolationWorkspace(fixture.sessionFile, fixture.id, fixture.workspace),
		).resolves.toMatchObject({ refusal: expect.stringMatching(/escapes|gone/) });
	});
});

describe("atomic owner claim", () => {
	it("claims again when a previous lease sidecar is still present", async () => {
		const fixture = await isolationFixture();
		await claimIsolationOwner(fixture.base, fixture.id, fixture.slot);
		expect((await readIsolationOwner(fixture.base))?.pid).toBe(process.pid);

		// The lease sidecar is never unlinked (that would split one lease across
		// two inodes); a second claim must reuse it and succeed once it is free.
		await claimIsolationOwner(fixture.base, fixture.id, fixture.slot);
		expect((await readIsolationOwner(fixture.base))?.pid).toBe(process.pid);
	});

	it("refuses while another process holds the slot lease, then succeeds after it exits", async () => {
		const fixture = await isolationFixture();
		const holder = await holdClaimInChild(fixture.slot);
		try {
			await expect(claimIsolationOwner(fixture.base, fixture.id, fixture.slot)).rejects.toThrow(
				/holds the isolation lease/,
			);
		} finally {
			await holder.release();
		}

		await claimIsolationOwner(fixture.base, fixture.id, fixture.slot);
		expect((await readIsolationOwner(fixture.base))?.pid).toBe(process.pid);
	});

	it("refuses a sandbox whose marker names another live owner", async () => {
		const fixture = await isolationFixture();
		const sleeper = Bun.spawn(["sleep", "30"], { stdout: "ignore", stderr: "ignore" });
		try {
			await Bun.write(
				path.join(fixture.base, ISOLATION_OWNER_FILE),
				JSON.stringify({ pid: sleeper.pid, id: fixture.id }),
			);
			await expect(claimIsolationOwner(fixture.base, fixture.id, fixture.slot)).rejects.toThrow(
				/owned by live omp process/,
			);
			expect((await readIsolationOwner(fixture.base))?.pid).toBe(sleeper.pid);
		} finally {
			sleeper.kill();
			await sleeper.exited;
		}
	});

	it("serialises two-process claims on the same slot", async () => {
		const fixture = await isolationFixture();
		// The fixture's marker names this (live) process, so either child would
		// refuse before ever racing the lease. Clear it so the winner can claim
		// and the loser observes the lease/owner contention instead.
		await fs.rm(path.join(fixture.base, ISOLATION_OWNER_FILE));
		const children = [0, 1].map(() => spawnLeaseChild(["claim", fixture.base, fixture.id, fixture.slot]));
		try {
			const outputs = await Promise.all(children.map(childVerdict));
			expect(outputs.filter(line => line.includes("won"))).toHaveLength(1);
			expect(outputs.join("")).toMatch(/lost:(.*lease.*|.*owned by live omp process.*)/);
			for (const child of children) {
				child.stdin.write("x\n");
				child.stdin.end();
			}
			await Promise.all(children.map(child => child.exited));
		} finally {
			for (const child of children) child.kill();
		}
	});
});

describe("spawn setup lease", () => {
	it("holds the slot lease across setup and rejects a concurrent same-slot spawn", async () => {
		const root = await tempDir("omp-iso-spawn-root-");
		setWorktreesDir(root);
		const repo = await tempDir("omp-iso-spawn-repo-");
		Bun.spawnSync(["git", "init", "-q", repo]);
		const id = "agent-spawn-race";
		const slot = getTaskIsolationSlot(repo, id);
		let started!: () => void;
		const startedAt = new Promise<void>(resolve => (started = resolve));
		let open!: () => void;
		const gate = new Promise<void>(resolve => (open = resolve));
		vi.spyOn(natives, "isoResolve").mockResolvedValue({
			kind: natives.IsoBackendKind.Rcopy,
			candidates: [natives.IsoBackendKind.Rcopy],
			reason: null,
			fellBack: false,
		} as never);
		vi.spyOn(natives, "isoStart").mockImplementation(async (_candidate, _source, merged) => {
			await fs.mkdir(merged, { recursive: true });
			started();
			await gate;
		});
		vi.spyOn(vcsModule, "detachGitDir").mockResolvedValue(undefined as never);

		const first = ensureIsolation(repo, id, undefined, repo);
		await startedAt;
		// The setup is mid-flight: a second spawn on the same slot must refuse
		// instead of rebuilding the sandbox under the first one.
		await expect(ensureIsolation(repo, id, undefined, repo)).rejects.toThrow(
			/another omp process is preparing or using isolation workspace/,
		);
		open();
		const handle = await first;
		expect(handle.mergedDir).toBe(path.join(root, slot, "m"));
		expect(await readIsolationOwner(path.join(root, slot))).toMatchObject({ pid: process.pid });
	});

	it("keeps a spawn's workspace when clear --all runs during setup", async () => {
		const root = await tempDir("omp-iso-spawn-clear-");
		setWorktreesDir(root);
		const repo = await tempDir("omp-iso-spawn-clear-repo-");
		Bun.spawnSync(["git", "init", "-q", repo]);
		const id = "agent-spawn-clear";
		const slot = getTaskIsolationSlot(repo, id);
		let started!: () => void;
		const startedAt = new Promise<void>(resolve => (started = resolve));
		let open!: () => void;
		const gate = new Promise<void>(resolve => (open = resolve));
		vi.spyOn(natives, "isoResolve").mockResolvedValue({
			kind: natives.IsoBackendKind.Rcopy,
			candidates: [natives.IsoBackendKind.Rcopy],
			reason: null,
			fellBack: false,
		} as never);
		vi.spyOn(natives, "isoStart").mockImplementation(async (_candidate, _source, merged) => {
			await fs.mkdir(merged, { recursive: true });
			started();
			await gate;
		});
		vi.spyOn(vcsModule, "detachGitDir").mockResolvedValue(undefined as never);

		const spawn = ensureIsolation(repo, id, undefined, repo);
		await startedAt;
		// The live owner marker is published before the backend runs, so a clear
		// classified from the pre-spawn dead leftovers cannot delete this slot.
		await clearWorktrees({ all: true, dryRun: false, json: true });
		expect(await fs.stat(path.join(root, slot)).catch(() => null)).not.toBeNull();
		open();
		await spawn;
		await clearWorktrees({ all: true, dryRun: false, json: true });
		expect(await fs.stat(path.join(root, slot)).catch(() => null)).not.toBeNull();
	});

	it("refuses a live marker-only owner under the slot lease without deleting it", async () => {
		const root = await tempDir("omp-iso-spawn-live-");
		setWorktreesDir(root);
		const repo = await tempDir("omp-iso-spawn-live-repo-");
		Bun.spawnSync(["git", "init", "-q", repo]);
		const id = "agent-spawn-live";
		const slot = getTaskIsolationSlot(repo, id);
		const base = path.join(root, slot);
		await fs.mkdir(base, { recursive: true });
		await writeIsolationOwner(base, id);

		await expect(ensureIsolation(repo, id, undefined, repo)).rejects.toThrow(/owned by a live omp process/);
		expect(await readIsolationOwner(base)).toMatchObject({ pid: process.pid });
	});
});

describe("run end retention", () => {
	it("keeps the workspace and descriptor for a one-shot isolated run", async () => {
		const root = await tempDir("omp-iso-root-");
		setWorktreesDir(root);
		const repo = await tempDir("omp-iso-repo-");
		const id = "agent-1";
		const base = path.join(root, getTaskIsolationSlot(repo, id));
		const workspace = path.join(base, "m");
		const artifactsDir = path.join(root, "artifacts");
		await fs.mkdir(workspace, { recursive: true });
		await fs.mkdir(artifactsDir, { recursive: true });
		const baseline = baselineFor(repo);
		vi.spyOn(worktreeModule, "ensureIsolation").mockResolvedValue({
			mergedDir: workspace,
			backend: natives.IsoBackendKind.Rcopy,
			fellBack: false,
			fallbackReason: null,
		});
		vi.spyOn(worktreeModule, "captureIsolationBaseline").mockResolvedValue(baseline);
		vi.spyOn(worktreeModule, "captureDeltaPatch").mockResolvedValue({ rootPatch: "one shot", nestedPatches: [] });
		const cleanup = vi.spyOn(worktreeModule, "cleanupIsolation").mockResolvedValue();
		let onRelease: ((opts?: { retain?: boolean }) => Promise<void>) | undefined;
		vi.spyOn(executorModule, "runSubprocess").mockImplementation(async options => {
			onRelease = options.onRelease;
			// `finalizeSubagentLifecycle` releases owned resources with retain for
			// one-shot runs, then the run-end capture below still happens.
			await options.onRelease?.({ retain: true });
			return result(id);
		});

		await runIsolatedSubprocess({
			baseOptions: { keepAlive: false } as never,
			context: { repoRoot: repo },
			preferredBackend: undefined,
			agentId: id,
			mergeMode: "patch",
			artifactsDir,
			buildFailureResult: () => result(id),
		});

		expect(onRelease).toBeDefined();
		expect(cleanup).not.toHaveBeenCalled();
		expect(await isolationStateExists(base)).toBe(true);
		expect(await Bun.file(path.join(artifactsDir, `${id}.patch`)).text()).toBe("one shot");
		const state = loadedState(await readIsolationState(base, id));
		expect(state.slot).toBe(getTaskIsolationSlot(repo, id));
		expect(state.sessionFile).toBe(path.join(artifactsDir, `${id}.jsonl`));
	});

	it("refuses a same-id spawn while the workspace is retained", async () => {
		const fixture = await isolationFixture();
		// Marker-only leftovers keep today's reclaim semantics.
		const markerOnly = path.join(fixture.root, "marker-only");
		await fs.mkdir(path.join(markerOnly, "m"), { recursive: true });
		await Bun.write(path.join(markerOnly, ISOLATION_OWNER_FILE), JSON.stringify({ pid: 1, id: fixture.id }));
		await expect(assertIsolationWorkspaceReleased(markerOnly, fixture.id)).resolves.toBeUndefined();

		await expect(assertIsolationWorkspaceReleased(fixture.base, fixture.id)).rejects.toThrow(/retained/);
	});
});

describe("explicit repoRoot targeting", () => {
	async function gitInit(dir: string): Promise<void> {
		const child = Bun.spawnSync(["git", "init", "-q", dir]);
		if (child.exitCode !== 0) throw new Error(child.stderr.toString());
	}

	it("isolates the current repo by default and another repo only when asked", async () => {
		const parent = await tempDir("omp-iso-parent-");
		const other = await tempDir("omp-iso-other-");
		await gitInit(parent);
		await gitInit(other);
		const cwdBefore = process.cwd();

		const byDefault = await prepareIsolationContext(parent);
		expect(path.resolve(byDefault.repoRoot)).toBe(await fs.realpath(parent));

		const explicit = await prepareIsolationContext(parent, other);
		expect(path.resolve(explicit.repoRoot)).toBe(await fs.realpath(other));
		expect(process.cwd()).toBe(cwdBefore);
	});
});

describe("task schema keeps an explicit repoRoot", () => {
	it("preserves repoRoot with taskFile without implying isolated", () => {
		const schema = getTaskSchema({ isolationEnabled: true, batchEnabled: false, file: true, structured: true });
		const withIsolation = schema({
			taskFile: "local://assignment.md",
			solutionSpace: "one change",
			isolated: true,
			repoRoot: "/repo/other",
		});
		if (withIsolation instanceof type.errors) throw new Error(withIsolation.summary);
		expect(withIsolation).toMatchObject({ isolated: true, repoRoot: "/repo/other" });

		const defaultRepo = schema({ taskFile: "local://assignment.md", solutionSpace: "one change", isolated: true });
		if (defaultRepo instanceof type.errors) throw new Error(defaultRepo.summary);
		expect(defaultRepo).toMatchObject({ isolated: true });

		const rootAlone = schema({
			taskFile: "local://assignment.md",
			solutionSpace: "one change",
			repoRoot: "/repo/other",
		});
		if (rootAlone instanceof type.errors) throw new Error(rootAlone.summary);
		expect(rootAlone).toMatchObject({ repoRoot: "/repo/other" });
		expect(rootAlone).not.toHaveProperty("isolated");
	});
});

describe("worktree clear retention", () => {
	it("tears down the descriptor-recorded backend even without a sidecar", async () => {
		const fixture = await isolationFixture({ backend: natives.IsoBackendKind.Overlayfs });
		const stop = vi.spyOn(natives, "isoStop").mockResolvedValue();
		const preserved = path.join(fixture.root, "corrupt-descriptor");
		await fs.mkdir(path.join(preserved, "m"), { recursive: true });
		await Bun.write(path.join(preserved, ISOLATION_STATE_FILE), "{oops");

		await expect(stopRetainedMount(fixture.base)).resolves.toBe(true);
		expect(stop).toHaveBeenCalledWith(natives.IsoBackendKind.Overlayfs, fixture.workspace);

		// Corrupt state must preserve the workspace: clear reports and leaves it.
		await expect(stopRetainedMount(preserved)).rejects.toThrow(/malformed|corrupt|unreadable/);
		expect(await fs.stat(preserved)).toBeDefined();
	});

	it("preserves descriptor-bearing retained sandboxes on every clear, removing only marker-only leftovers", async () => {
		const worktrees = await tempDir("omp-iso-clear-");
		setWorktreesDir(worktrees);
		const repo = await tempDir("omp-iso-clear-repo-");
		const transcript = path.join(worktrees, "session.jsonl");
		await Bun.write(transcript, "");

		const retainedDead = path.join(worktrees, "retained-dead");
		const liveOwned = path.join(worktrees, "live-owned");
		const markerOnly = path.join(worktrees, "marker-only");
		for (const dir of [retainedDead, liveOwned]) {
			await fs.mkdir(path.join(dir, "m"), { recursive: true });
			await writeIsolationState(dir, {
				version: 1,
				id: path.basename(dir),
				repoRoot: repo,
				backend: natives.IsoBackendKind.Rcopy,
				slot: path.basename(dir),
				mergedDir: "m",
				baseline: baselineFor(repo),
				sessionFile: transcript,
			});
		}
		await fs.mkdir(path.join(markerOnly, "m"), { recursive: true });
		await Bun.write(
			path.join(retainedDead, ISOLATION_OWNER_FILE),
			JSON.stringify({ pid: await deadPid(), id: "retained-dead" }),
		);
		await Bun.write(
			path.join(markerOnly, ISOLATION_OWNER_FILE),
			JSON.stringify({ pid: await deadPid(), id: "marker-only" }),
		);
		await writeIsolationOwner(liveOwned, "live-owned");

		// Dry-run must classify descriptor-bearing sandboxes as kept, matching the
		// real run, instead of presenting retained work as removable.
		const logs: string[] = [];
		const log = vi.spyOn(console, "log").mockImplementation((...args: unknown[]) => {
			logs.push(args.map(String).join(" "));
		});
		try {
			await clearWorktrees({ all: true, dryRun: true, json: true });
		} finally {
			log.mockRestore();
		}
		const plan = JSON.parse(logs.join("\n")) as { wouldRemove: string[]; keptLive: string[] };
		expect(plan.wouldRemove).toEqual([markerOnly]);
		expect(plan.keptLive).toEqual(expect.arrayContaining([retainedDead, liveOwned]));

		// Default clear already reclaims orphaned marker-only leftovers; both
		// descriptor-bearing sandboxes are kept.
		await clearWorktrees({ all: false, dryRun: false, json: true });
		expect(await fs.stat(retainedDead).catch(() => null)).not.toBeNull();
		expect(await fs.stat(liveOwned).catch(() => null)).not.toBeNull();
		expect(await fs.stat(markerOnly).catch(() => null)).toBeNull();

		// `--all` is still a GC, not a release: a descriptor means the sandbox
		// holds uncaptured work (or in-workspace commits), so only the explicit
		// release path may remove it.
		await clearWorktrees({ all: true, dryRun: false, json: true });
		expect(await fs.stat(retainedDead).catch(() => null)).not.toBeNull();
		expect(await fs.stat(liveOwned).catch(() => null)).not.toBeNull();
		expect(await isolationStateExists(retainedDead)).toBe(true);
	});

	it("keys the lease to the immutable slot across a retention rename", async () => {
		const worktrees = await tempDir("omp-iso-rename-");
		setWorktreesDir(worktrees);
		const repo = await tempDir("omp-iso-rename-repo-");
		const id = "agent-rename";
		const slot = getTaskIsolationSlot(repo, id);
		const base = path.join(worktrees, slot);
		await fs.mkdir(path.join(base, "m"), { recursive: true });
		await Bun.write(path.join(base, ISOLATION_OWNER_FILE), JSON.stringify({ pid: await deadPid(), id }));
		// Marker-only so the sandbox stays reclaimable after the lease drops; the
		// point is the lease key, not descriptor retention (covered above).
		// Acquire the lease on the ORIGINAL sandbox, then let retention rename it.
		// The lease lives outside the sandbox and is keyed by the immutable slot,
		// so it must still exclude `clear` of the renamed sibling.
		const holder = await holdClaimInChild(slot);
		const retained = `${base}.retained-1-abc`;
		try {
			await fs.rename(base, retained);
			await clearWorktrees({ all: true, dryRun: false, json: true });
			expect(await fs.stat(retained).catch(() => null)).not.toBeNull();
		} finally {
			await holder.release();
		}
		// Once the holder exits the OS lease dies with it; the same slot key then
		// lets clear reclaim the renamed sandbox.
		await clearWorktrees({ all: true, dryRun: false, json: true });
		expect(await fs.stat(retained).catch(() => null)).toBeNull();
	});
});
