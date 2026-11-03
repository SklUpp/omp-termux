import { afterEach, beforeEach, describe, expect, it, vi } from "bun:test";
import { $ } from "bun";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";
import * as natives from "@oh-my-pi/pi-natives";
import { getWorktreesDir, setWorktreesDir } from "@oh-my-pi/pi-utils";
import { releaseWorktree } from "@oh-my-pi/pi-coding-agent/cli/worktree-cli";
import {
	ISOLATION_OWNER_FILE,
	readIsolationOwner,
	writeIsolationOwner,
} from "@oh-my-pi/pi-coding-agent/task/isolation-ownership";
import * as worktreeModule from "@oh-my-pi/pi-coding-agent/task/worktree";
import {
	getTaskIsolationSlot,
	type RepoBaseline,
	type WorktreeBaseline,
	writeIsolationState,
} from "@oh-my-pi/pi-coding-agent/task/worktree";

const tempRoots: string[] = [];

beforeEach(() => {
	vi.spyOn(console, "log").mockImplementation(() => {});
});

afterEach(async () => {
	vi.restoreAllMocks();
	setWorktreesDir(undefined);
	await Promise.all(tempRoots.splice(0).map(dir => fs.rm(dir, { recursive: true, force: true })));
});

async function tempDir(prefix: string): Promise<string> {
	const dir = await fs.mkdtemp(path.join(os.tmpdir(), prefix));
	tempRoots.push(dir);
	return dir;
}

function repoBaseline(repoRoot: string): RepoBaseline {
	return {
		repoRoot,
		headCommit: "base",
		staged: "",
		unstaged: "",
		untracked: [],
		untrackedPatch: "",
	};
}

function baselineFor(repoRoot: string): WorktreeBaseline {
	return { root: repoBaseline(repoRoot), nested: [] };
}

async function retainedFixture(id = "agent-1") {
	const root = await tempDir("omp-release-root-");
	setWorktreesDir(root);
	const repo = await tempDir("omp-release-repo-");
	const slot = getTaskIsolationSlot(repo, id);
	const base = path.join(root, slot);
	await fs.mkdir(path.join(base, "m"), { recursive: true });
	await Bun.write(path.join(base, "m", "work.txt"), "post-handoff\n");
	const artifactsDir = await tempDir("omp-release-artifacts-");
	const sessionFile = path.join(artifactsDir, `${id}.jsonl`);
	const workspace = path.join(base, "m");
	await Bun.write(
		sessionFile,
		`${JSON.stringify({
			type: "session",
			id: `session-${id}`,
			timestamp: new Date().toISOString(),
			cwd: workspace,
		})}\n`,
	);
	// Spawn-time writer shape: `dir` is the sandbox base, `workspace` its mount.
	await Bun.write(path.join(artifactsDir, `${id}.isolation.json`), JSON.stringify({ dir: base, workspace }));
	await writeIsolationState(base, {
		version: 1,
		id,
		repoRoot: repo,
		backend: natives.IsoBackendKind.Rcopy,
		slot,
		mergedDir: "m",
		baseline: baselineFor(repo),
		sessionFile,
	});
	return { base, artifactsDir, sessionFile };
}

describe("worktree release retained task-isolation", () => {
	it("captures post-handoff work and tears the sandbox down on explicit release", async () => {
		const { base, artifactsDir } = await retainedFixture();
		const capture = vi
			.spyOn(worktreeModule, "captureDeltaPatch")
			.mockResolvedValue({ rootPatch: "diff --git a/work b/work", nestedPatches: [] });
		const commit = vi
			.spyOn(worktreeModule, "commitToBranch")
			.mockResolvedValue({ branchName: "omp/task/agent-1", baseSha: "base", rootPatch: "", nestedPatches: [] });
		const cleanup = vi.spyOn(worktreeModule, "cleanupIsolation").mockResolvedValue();

		await releaseWorktree({ path: base, json: true });

		expect(capture).toHaveBeenCalledTimes(1);
		expect(commit).toHaveBeenCalledTimes(1);
		expect(commit.mock.calls[0]?.[0]).toBe(path.join(base, "m"));
		expect(cleanup).toHaveBeenCalledTimes(1);
		expect(cleanup.mock.calls[0]?.[0]?.mergedDir).toBe(path.join(base, "m"));
		expect(await Bun.file(path.join(artifactsDir, "agent-1.patch")).text()).toBe("diff --git a/work b/work");
		// The explicit release claims the slot for this process before capture.
		expect((await readIsolationOwner(base))?.pid).toBe(process.pid);
	});

	it("refuses a sandbox with a live foreign owner and leaves it untouched", async () => {
		const { base } = await retainedFixture();
		const sleeper = Bun.spawn(["sleep", "60"], { stdout: "ignore", stderr: "ignore" });
		try {
			await Bun.write(path.join(base, ISOLATION_OWNER_FILE), JSON.stringify({ pid: sleeper.pid, id: "agent-1" }));
			const capture = vi.spyOn(worktreeModule, "captureDeltaPatch");
			const cleanup = vi.spyOn(worktreeModule, "cleanupIsolation");

			await expect(releaseWorktree({ path: base, json: true })).rejects.toThrow("owned by a live omp process");

			expect(capture).not.toHaveBeenCalled();
			expect(cleanup).not.toHaveBeenCalled();
			expect(await Bun.file(path.join(base, "m", "work.txt")).text()).toBe("post-handoff\n");
		} finally {
			sleeper.kill();
			await sleeper.exited;
		}
	});

	it("releases a retention-renamed sandbox using the retained pointer shape", async () => {
		const { base, artifactsDir } = await retainedFixture();
		const retainedBase = `${base}.retained-test-1`;
		await fs.rename(base, retainedBase);
		// Retention rewrites the pointer to the renamed sandbox's mount; `dir` is
		// that renamed sandbox base and the header still records the original mount.
		await Bun.write(
			path.join(artifactsDir, "agent-1.isolation.json"),
			JSON.stringify({ dir: retainedBase, workspace: path.join(retainedBase, "m") }),
		);
		const capture = vi
			.spyOn(worktreeModule, "captureDeltaPatch")
			.mockResolvedValue({ rootPatch: "diff --git a/work b/work", nestedPatches: [] });
		const commit = vi
			.spyOn(worktreeModule, "commitToBranch")
			.mockResolvedValue({ branchName: "omp/task/agent-1", baseSha: "base", rootPatch: "", nestedPatches: [] });
		const cleanup = vi.spyOn(worktreeModule, "cleanupIsolation").mockResolvedValue();

		await releaseWorktree({ path: retainedBase, json: true });

		expect(capture).toHaveBeenCalledTimes(1);
		expect(commit.mock.calls[0]?.[0]).toBe(path.join(retainedBase, "m"));
		expect(cleanup.mock.calls[0]?.[0]?.mergedDir).toBe(path.join(retainedBase, "m"));
	});

	it("rejects a transcript whose pointer names another workspace", async () => {
		const { base, artifactsDir } = await retainedFixture();
		const otherRoot = await tempDir("omp-release-other-");
		const otherMount = path.join(otherRoot, "m");
		await fs.mkdir(otherMount, { recursive: true });
		await Bun.write(
			path.join(artifactsDir, "agent-1.isolation.json"),
			JSON.stringify({ dir: otherRoot, workspace: otherMount }),
		);
		const capture = vi.spyOn(worktreeModule, "captureDeltaPatch");

		await expect(releaseWorktree({ path: base, json: true })).rejects.toThrow("points at");

		expect(capture).not.toHaveBeenCalled();
		expect(await Bun.file(path.join(base, "m", "work.txt")).text()).toBe("post-handoff\n");
	});

	it("rejects a transcript that was not created in the selected workspace", async () => {
		const { base } = await retainedFixture();
		const victimRoot = await tempDir("omp-release-victim-");
		const victimArtifacts = await tempDir("omp-release-victim-artifacts-");
		const victimWorkspace = path.join(victimRoot, "m");
		await fs.mkdir(victimWorkspace, { recursive: true });
		const victimTranscript = path.join(victimArtifacts, "agent-1.jsonl");
		await Bun.write(
			victimTranscript,
			`${JSON.stringify({
				type: "session",
				id: "victim",
				timestamp: new Date().toISOString(),
				cwd: victimWorkspace,
			})}\n`,
		);
		await Bun.write(
			path.join(victimArtifacts, "agent-1.isolation.json"),
			JSON.stringify({ dir: victimRoot, workspace: victimWorkspace }),
		);
		const descriptor = path.join(base, ".omp-isolation-state.json");
		const state = JSON.parse(await Bun.file(descriptor).text());
		await Bun.write(descriptor, JSON.stringify({ ...state, sessionFile: victimTranscript }));
		const capture = vi.spyOn(worktreeModule, "captureDeltaPatch");
		const cleanup = vi.spyOn(worktreeModule, "cleanupIsolation");

		await expect(releaseWorktree({ path: base, json: true })).rejects.toThrow("was not created in");

		expect(capture).not.toHaveBeenCalled();
		expect(cleanup).not.toHaveBeenCalled();
		expect(await Bun.file(path.join(base, "m", "work.txt")).text()).toBe("post-handoff\n");
		expect(await Bun.file(path.join(victimArtifacts, "agent-1.patch")).exists()).toBe(false);
	});

	it("rejects a directory without a descriptor and preserves it", async () => {
		const root = await tempDir("omp-release-root-");
		setWorktreesDir(root);
		const dir = path.join(root, "not-isolated");
		await fs.mkdir(dir, { recursive: true });
		await expect(releaseWorktree({ path: dir, json: true })).rejects.toThrow("Cannot release");
		expect(await fs.stat(dir)).not.toBeNull();
		expect(await fs.stat(path.join(dir, ISOLATION_OWNER_FILE)).catch(() => null)).toBeNull();
	});

	it.skipIf(process.platform === "win32")("accepts release as a positional action through the real CLI", async () => {
		const root = await tempDir("omp-release-cli-root-");
		setWorktreesDir(root);
		const home = await tempDir("omp-release-cli-home-");
		const dir = path.join(root, "not-isolated");
		await fs.mkdir(dir, { recursive: true });
		const cli = path.resolve(import.meta.dir, "../../src/cli.ts");
		const result = await $`bun ${cli} worktree release ${dir}`
			.env({ ...process.env, OMP_WORKTREE_DIR: root, HOME: home })
			.quiet()
			.nothrow();
		expect(result.exitCode).toBe(1);
		expect(result.stderr.toString()).toContain("Cannot release");
	});
});
