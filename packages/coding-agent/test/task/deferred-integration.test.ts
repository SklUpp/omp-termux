import { afterEach, describe, expect, it, vi } from "bun:test";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";
import * as natives from "@oh-my-pi/pi-natives";
import { getWorktreesDir, setWorktreesDir } from "@oh-my-pi/pi-utils";
import { Settings } from "@oh-my-pi/pi-coding-agent/config/settings";
import type { InternalUrl } from "@oh-my-pi/pi-coding-agent/internal-urls/types";
import { WorktreeProtocolHandler } from "@oh-my-pi/pi-coding-agent/internal-urls/worktree-protocol";
import { AgentLifecycleManager } from "@oh-my-pi/pi-coding-agent/registry/agent-lifecycle";
import { AgentRegistry, MAIN_AGENT_ID } from "@oh-my-pi/pi-coding-agent/registry/agent-registry";
import * as persistedAgents from "@oh-my-pi/pi-coding-agent/registry/persisted-agents";
import type { AgentSession } from "@oh-my-pi/pi-coding-agent/session/agent-session";
import {
	DEFERRED_INTEGRATION_SIGNAL,
	integrateDeferredAgent,
	isDeferredIntegrationSignal,
} from "@oh-my-pi/pi-coding-agent/task/deferred-integration";
import * as discoveryModule from "@oh-my-pi/pi-coding-agent/task/discovery";
import * as ownershipModule from "@oh-my-pi/pi-coding-agent/task/isolation-ownership";
import * as runnerModule from "@oh-my-pi/pi-coding-agent/task/isolation-runner";
import { cfgTaskIsolationDeferIntegration } from "@oh-my-pi/pi-coding-agent/task/settings";
import {
	resolveEffectiveSubagentPolicy,
	type StructuredSubagentRequest,
} from "@oh-my-pi/pi-coding-agent/task/structured-subagent";
import type { AgentDefinition } from "@oh-my-pi/pi-coding-agent/task/types";
import * as worktreeModule from "@oh-my-pi/pi-coding-agent/task/worktree";
import type { LoadedIsolationState } from "@oh-my-pi/pi-coding-agent/task/worktree";
import type { ToolSession } from "@oh-my-pi/pi-coding-agent/tools";

const AGENT: AgentDefinition = {
	name: "worker",
	description: "Test worker",
	systemPrompt: "Do the work.",
	source: "bundled",
	tools: ["read", "write"],
};
const JOURNAL_FILE = ".omp-integration-journal.json";
const tempRoots: string[] = [];
const originalWorktreesDir = getWorktreesDir();

afterEach(async () => {
	vi.restoreAllMocks();
	AgentRegistry.resetGlobalForTests();
	AgentLifecycleManager.resetGlobalForTests();
	setWorktreesDir(originalWorktreesDir);
	await Promise.all(tempRoots.splice(0).map(dir => fs.rm(dir, { recursive: true, force: true })));
});

async function tempDir(prefix: string): Promise<string> {
	const dir = await fs.mkdtemp(path.join(os.tmpdir(), prefix));
	tempRoots.push(dir);
	return dir;
}

function worktreeUrl(id: string): InternalUrl {
	return {
		rawHost: id,
		hostname: id.toLowerCase(),
		rawPathname: "",
		pathname: "",
		search: "",
		hash: "",
		href: `worktree://${id}`,
		rawHref: `worktree://${id}`,
	} as InternalUrl;
}

function session(settings: Settings, identity = true): ToolSession {
	return {
		cwd: "/tmp",
		hasUI: false,
		settings,
		getSessionFile: () => (identity ? "/tmp/parent.jsonl" : null),
		getSessionId: () => (identity ? "parent-1" : null),
		getSessionSpawns: () => "*",
		getSessionAgents: () => [],
		getPlanModeState: () => undefined,
	} as unknown as ToolSession;
}

function settings(overrides: Record<string, unknown> = {}): Settings {
	return Settings.isolated({
		"task.maxRecursionDepth": 2,
		"task.isolation.enabled": true,
		"task.isolation.apply": true,
		"isolation.backend": "rcopy",
		"task.enableLsp": true,
		...overrides,
	});
}

function policyRequest(
	settingsValue: Settings,
	overrides: Record<string, unknown> = {},
	withIdentity = true,
): StructuredSubagentRequest {
	return {
		session: session(settingsValue, withIdentity),
		invocationKind: "task" as const,
		assignment: "Do the work.",
		agent: "worker",
		...overrides,
	} as StructuredSubagentRequest;
}

interface FixtureOptions {
	status?: "parked" | "running";
	mergeMode?: "patch" | "branch";
	childSessionId?: string;
	parentSessionId?: string;
	missingBinding?: boolean;
	ready?: boolean;
	hasRootChanges?: boolean;
	nestedPatches?: { relativePath: string; patch: string }[];
}

async function deferredFixture(options: FixtureOptions = {}) {
	const dir = await tempDir("omp-deferred-");
	const artifactsDir = path.join(dir, "artifacts");
	const workspace = path.join(dir, "wt", "t123456789", "m");
	const repoRoot = path.join(dir, "repo");
	await Promise.all([
		fs.mkdir(workspace, { recursive: true }),
		fs.mkdir(artifactsDir, { recursive: true }),
		fs.mkdir(repoRoot, { recursive: true }),
	]);
	const sessionFile = path.join(artifactsDir, "Worker.jsonl");
	const parentSessionFile = path.join(artifactsDir, "parent.jsonl");
	await Bun.write(
		sessionFile,
		`${JSON.stringify({ type: "session", id: "child-1", timestamp: new Date().toISOString(), cwd: workspace })}\n${JSON.stringify({ type: "session_init", id: "si", parentId: "child-1", timestamp: new Date().toISOString(), agent: "task", task: "do", isolated: true })}\n`,
	);
	await Bun.write(
		parentSessionFile,
		`${JSON.stringify({ type: "session", id: "parent-1", timestamp: new Date().toISOString(), cwd: dir })}\n`,
	);
	const binding = options.missingBinding
		? {}
		: {
				childSessionId: options.childSessionId ?? "child-1",
				parentSessionFile,
				parentSessionId: options.parentSessionId ?? "parent-1",
			};
	const state = {
		version: 1 as const,
		id: "Worker",
		repoRoot,
		backend: natives.IsoBackendKind.Rcopy,
		slot: "t123456789",
		mergedDir: "m",
		dir: path.dirname(workspace),
		baseline: {
			root: { repoRoot, headCommit: "base", staged: "", unstaged: "", untracked: [], untrackedPatch: "" },
			nested: [],
		},
		sessionFile,
		deferred: true,
		deferredReady: options.ready ?? true,
		mergeMode: options.mergeMode ?? ("patch" as const),
		...binding,
	};
	const registry = AgentRegistry.global();
	registry.register({
		id: "Worker",
		displayName: "Worker",
		kind: "sub",
		parentId: MAIN_AGENT_ID,
		status: options.status ?? "parked",
		session: null,
		sessionFile,
	});

	vi.spyOn(persistedAgents, "ensurePersistedRoster").mockResolvedValue(parentSessionFile);
	vi.spyOn(persistedAgents, "sessionFileBelongsToRoot").mockReturnValue(true);
	vi.spyOn(worktreeModule, "resolveIsolationWorkspace").mockResolvedValue({ state, workspaceDir: workspace });
	vi.spyOn(worktreeModule, "readIsolationState").mockResolvedValue({ state });
	vi.spyOn(worktreeModule, "bindIsolationState").mockResolvedValue({ workspaceDir: workspace });
	vi.spyOn(worktreeModule, "readSessionHeaderId").mockImplementation(async file =>
		file.includes("parent") ? "parent-1" : "child-1",
	);
	vi.spyOn(ownershipModule, "withIsolationLease").mockImplementation(async (_slot, fn) => fn());
	const readOwner = vi.spyOn(ownershipModule, "readIsolationOwner").mockResolvedValue(undefined);
	vi.spyOn(ownershipModule, "writeIsolationOwner").mockResolvedValue();
	const capture = {
		artifacts: {
			patchPath: path.join(artifactsDir, "Worker.patch"),
			hasRootChanges: options.hasRootChanges ?? false,
			nestedPatches: options.nestedPatches ?? [],
			nestedPatchPaths: [],
		},
		fingerprint: 1,
	};
	vi.spyOn(runnerModule, "writeIsolationPatch").mockResolvedValue(capture);
	const applyNested = vi.spyOn(worktreeModule, "applyNestedPatches").mockResolvedValue([]);
	const cleanup = vi.spyOn(worktreeModule, "cleanupIsolation").mockImplementation(async () => {
		await fs.rm(state.dir, { recursive: true, force: true });
	});
	const commit = vi.spyOn(worktreeModule, "commitToBranch").mockResolvedValue({
		branchName: "omp/task/Worker",
		baseSha: "base",
		rootPatch: "diff --git a/work.txt b/work.txt",
		nestedPatches: [],
	});
	const merge = vi.spyOn(runnerModule, "mergeIsolatedChanges").mockResolvedValue({
		summary: "\n\nApplied patches: yes",
		changesApplied: true,
		hadAnyChanges: true,
		mergedBranchForNestedPatches: false,
	});
	return {
		registry,
		state,
		parentSessionFile,
		parentSessionId: "parent-1",
		artifactsDir,
		merge,
		commit,
		applyNested,
		cleanup,
		readOwner,
		request: {
			registry,
			senderId: MAIN_AGENT_ID,
			target: "Worker",
			parentSessionFile,
			parentSessionId: "parent-1",
		},
	};
}

describe("deferred integration setting", () => {
	it("is off by default and only shown while apply is on", () => {
		expect(cfgTaskIsolationDeferIntegration.default).toBe(false);
		expect(cfgTaskIsolationDeferIntegration.ui?.condition).toBe("isolationApplyEnabled");
	});

	it("defers only an effective apply=true isolated task or eval run", async () => {
		vi.spyOn(discoveryModule, "discoverAgents").mockResolvedValue({ agents: [AGENT], projectAgentsDir: null });
		const deferOn = settings({ "task.isolation.deferIntegration": true });
		const task = await resolveEffectiveSubagentPolicy(policyRequest(deferOn, { isolation: { requested: true } }));
		expect(task.applyChanges).toBe(true);
		expect(task.deferIntegration).toBe(true);
		expect(task.parentSessionId).toBe("parent-1");

		const evalRun = await resolveEffectiveSubagentPolicy(
			policyRequest(deferOn, { invocationKind: "eval", isolation: { requested: true } }),
		);
		expect(evalRun.applyChanges).toBe(true);
		expect(evalRun.deferIntegration).toBe(true);

		const evalNoApplySetting = await resolveEffectiveSubagentPolicy(
			policyRequest(settings({ "task.isolation.apply": false, "task.isolation.deferIntegration": true }), {
				invocationKind: "eval",
				isolation: { requested: true },
			}),
		);
		expect(evalNoApplySetting.applyChanges).toBe(true);
		expect(evalNoApplySetting.deferIntegration).toBe(false);

		const taskPerCallApply = await resolveEffectiveSubagentPolicy(
			policyRequest(settings({ "task.isolation.apply": false, "task.isolation.deferIntegration": true }), {
				isolation: { requested: true, apply: true },
			}),
		);
		expect(taskPerCallApply.applyChanges).toBe(true);
		expect(taskPerCallApply.deferIntegration).toBe(false);

		const noApply = await resolveEffectiveSubagentPolicy(
			policyRequest(deferOn, { isolation: { requested: true, apply: false } }),
		);
		expect(noApply.deferIntegration).toBe(false);

		const deferOff = await resolveEffectiveSubagentPolicy(
			policyRequest(settings(), { isolation: { requested: true } }),
		);
		expect(deferOff.deferIntegration).toBe(false);
	});

	it("fails preflight when the parent session has no durable identity", async () => {
		vi.spyOn(discoveryModule, "discoverAgents").mockResolvedValue({ agents: [AGENT], projectAgentsDir: null });
		const deferOn = settings({ "task.isolation.deferIntegration": true });
		await expect(
			resolveEffectiveSubagentPolicy(policyRequest(deferOn, { isolation: { requested: true } }, false)),
		).rejects.toThrow("persisted parent session");
	});
});

describe("deferred integration signal", () => {
	it("matches only the reserved control message", () => {
		expect(DEFERRED_INTEGRATION_SIGNAL).toBe("/integrate");
		expect(isDeferredIntegrationSignal(" /integrate \n")).toBe(true);
		expect(isDeferredIntegrationSignal("/integrate now")).toBe(false);
		expect(isDeferredIntegrationSignal("integrate")).toBe(false);
	});

	it("exposes the handoff on the isolation-manager URL, not the agent URL", async () => {
		const handler = new WorktreeProtocolHandler();
		expect(handler.scheme).toBe("worktree");
		expect(handler.spec.write?.scope).toBe("workspace");
		const resource = await handler.resolve(worktreeUrl("Worker"));
		expect(resource.content).toContain("worktree://Worker");
		const session = {
			agentRegistry: AgentRegistry.global(),
			getAgentId: () => MAIN_AGENT_ID,
			getSessionFile: () => null,
		} as unknown as ToolSession;
		await expect(handler.write(worktreeUrl("Worker"), "hello", { session })).rejects.toThrow("accepts only");
	});

	it("captures and applies, writes a durable tombstone, then releases", async () => {
		const { request, merge, artifactsDir } = await deferredFixture();
		const outcome = await integrateDeferredAgent(request);
		expect(outcome.isError).toBe(false);
		expect(outcome.text).toContain('Integrated "Worker"');
		expect(outcome.text).toContain("Workspace released");
		expect(merge).toHaveBeenCalledTimes(1);
		expect(await Bun.file(path.join(artifactsDir, "Worker.integration.json")).exists()).toBe(true);
	});

	it("uses the merge strategy recorded at spawn", async () => {
		const { request, merge, commit } = await deferredFixture({ mergeMode: "branch" });
		const outcome = await integrateDeferredAgent(request);
		expect(outcome.isError).toBe(false);
		expect(commit).toHaveBeenCalledTimes(1);
		expect(merge.mock.calls[0]?.[0].mergeMode).toBe("branch");
	});

	it("refuses a running agent without touching its workspace", async () => {
		const { request, merge } = await deferredFixture({ status: "running" });
		const outcome = await integrateDeferredAgent(request);
		expect(outcome.isError).toBe(true);
		expect(merge).not.toHaveBeenCalled();
		expect(worktreeModule.cleanupIsolation).not.toHaveBeenCalled();
	});

	it("refuses a signal from anyone but the parent", async () => {
		const { request, merge } = await deferredFixture();
		const outcome = await integrateDeferredAgent({ ...request, senderId: "Someone" });
		expect(outcome.isError).toBe(true);
		expect(outcome.text).toContain("only its parent agent");
		expect(merge).not.toHaveBeenCalled();
	});

	it("keeps the workspace and fails closed when the stock merge fails", async () => {
		const { request, merge } = await deferredFixture();
		merge.mockResolvedValue({
			summary: "<system-notification>Patches were not applied and must be handled manually.</system-notification>",
			changesApplied: false,
			hadAnyChanges: false,
			mergedBranchForNestedPatches: false,
		});
		const first = await integrateDeferredAgent(request);
		expect(first.isError).toBe(true);
		expect(first.text).toContain("Workspace");
		expect(worktreeModule.cleanupIsolation).not.toHaveBeenCalled();

		const second = await integrateDeferredAgent(request);
		expect(second.isError).toBe(true);
		expect(second.text).toContain("previous handoff");
		expect(merge).toHaveBeenCalledTimes(1);
	});

	it("is a durable no-op after success, without touching a revived workspace", async () => {
		const { request, merge, cleanup } = await deferredFixture();
		const first = await integrateDeferredAgent(request);
		expect(first.isError).toBe(false);
		cleanup.mockClear();
		const second = await integrateDeferredAgent(request);
		expect(second.isError).toBe(false);
		expect(second.text).toContain("already integrated");
		expect(merge).toHaveBeenCalledTimes(1);
		expect(cleanup).not.toHaveBeenCalled();
	});

	it("fails closed when a previous attempt is marked applying", async () => {
		const { request, state, merge } = await deferredFixture();
		await Bun.write(
			path.join(state.dir, JOURNAL_FILE),
			JSON.stringify({
				version: 1,
				id: "Worker",
				childSessionFile: state.sessionFile,
				childSessionId: "child-1",
				parentSessionFile: state.parentSessionFile,
				parentSessionId: "parent-1",
				mergeMode: "patch",
				phase: "applying",
				startedAt: new Date().toISOString(),
			}),
		);
		const outcome = await integrateDeferredAgent(request);
		expect(outcome.isError).toBe(true);
		expect(outcome.text).toContain("previous handoff");
		expect(merge).not.toHaveBeenCalled();
		expect(worktreeModule.cleanupIsolation).not.toHaveBeenCalled();
	});

	it("refuses a legacy descriptor without strict session binding", async () => {
		const { request, merge } = await deferredFixture({ missingBinding: true });
		const outcome = await integrateDeferredAgent(request);
		expect(outcome.isError).toBe(true);
		expect(outcome.text).toContain("predates strict session binding");
		expect(merge).not.toHaveBeenCalled();
		expect(worktreeModule.cleanupIsolation).not.toHaveBeenCalled();
	});

	it("refuses a failed deferred run that never became ready", async () => {
		const { request, merge } = await deferredFixture({ ready: false });
		const outcome = await integrateDeferredAgent(request);
		expect(outcome.isError).toBe(true);
		expect(outcome.text).toContain("did not finish successfully");
		expect(merge).not.toHaveBeenCalled();
		expect(worktreeModule.cleanupIsolation).not.toHaveBeenCalled();
	});

	it("rejects a reused child session id", async () => {
		const { request, merge } = await deferredFixture({ childSessionId: "child-old" });
		const outcome = await integrateDeferredAgent(request);
		expect(outcome.isError).toBe(true);
		expect(outcome.text).toContain("child session id no longer matches");
		expect(merge).not.toHaveBeenCalled();
	});

	it("rejects a different parent session file", async () => {
		const { request, parentSessionFile, state, merge } = await deferredFixture();
		const other = path.join(path.dirname(parentSessionFile), "other-parent.jsonl");
		await Bun.write(other, `${JSON.stringify({ type: "session", id: "parent-1", cwd: "/tmp" })}\n`);
		state.parentSessionFile = other;
		const outcome = await integrateDeferredAgent(request);
		expect(outcome.isError).toBe(true);
		expect(outcome.text).toContain("parent session file no longer matches");
		expect(merge).not.toHaveBeenCalled();
	});

	it("rejects a different parent session id", async () => {
		const { request, merge } = await deferredFixture({ parentSessionId: "parent-other" });
		const outcome = await integrateDeferredAgent(request);
		expect(outcome.isError).toBe(true);
		expect(outcome.text).toContain("parent session id no longer matches");
		expect(merge).not.toHaveBeenCalled();
	});

	it("keeps recovery state when nested apply reports warnings", async () => {
		const { request, state, merge, applyNested } = await deferredFixture({
			nestedPatches: [
				{
					relativePath: "nested",
					patch: "diff --git a/n.txt b/n.txt\nnew file mode 100644\n--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1 @@\n+hi\n",
				},
			],
		});
		const nestedDir = path.join(state.repoRoot, "nested");
		await fs.mkdir(nestedDir, { recursive: true });
		await Bun.$`git -C ${nestedDir} init -q && git -C ${nestedDir} config user.email a@b && git -C ${nestedDir} config user.name a && git -C ${nestedDir} commit -q --allow-empty -m base`;
		applyNested.mockResolvedValue([
			"<system-notification>nested changes restored from stash (warnings).</system-notification>",
		]);
		const outcome = await integrateDeferredAgent(request);
		expect(outcome.isError).toBe(true);
		expect(outcome.text).toContain("Incomplete integrate");
		expect(merge).toHaveBeenCalledTimes(1);
		expect(worktreeModule.cleanupIsolation).not.toHaveBeenCalled();
		expect(await Bun.file(path.join(state.dir, JOURNAL_FILE)).exists()).toBe(true);
	});

	it("reclaims a dead owner marker under the lease", async () => {
		const { request, merge, readOwner } = await deferredFixture();
		readOwner.mockResolvedValue({ pid: 999_999, id: "Worker" });
		vi.spyOn(ownershipModule, "hasLiveIsolationOwner").mockResolvedValue(false);
		const outcome = await integrateDeferredAgent(request);
		expect(outcome.isError).toBe(false);
		expect(merge).toHaveBeenCalledTimes(1);
	});

	it("refuses a live foreign owner", async () => {
		const { request, merge, readOwner } = await deferredFixture();
		readOwner.mockResolvedValue({ pid: 999_999, id: "Worker" });
		vi.spyOn(ownershipModule, "hasLiveIsolationOwner").mockResolvedValue(true);
		const outcome = await integrateDeferredAgent(request);
		expect(outcome.isError).toBe(true);
		expect(outcome.text).toContain("live omp process");
		expect(merge).not.toHaveBeenCalled();
	});

	it("refuses a hold while a release is in flight", async () => {
		const { registry } = await deferredFixture();
		const ref = registry.get("Worker")!;
		const gate = Promise.withResolvers<void>();
		// Minimal session double: only `dispose` is observed by release.
		const liveSession = { dispose: () => gate.promise } as unknown as AgentSession;
		expect(registry.attachSession("Worker", liveSession, undefined, ref)).toBe(true);
		const lifecycle = AgentLifecycleManager.global();
		const releasing = lifecycle.release("Worker", ref);
		await Promise.resolve();
		await expect(lifecycle.reserve("Worker", ref)).rejects.toThrow("revived or released");
		gate.resolve();
		await releasing;
	});

	it("shared bind rejects a reused session generation", async () => {
		const root = await tempDir("omp-wt-");
		setWorktreesDir(root);
		const repoRoot = await tempDir("omp-repo-");
		const slot = worktreeModule.getTaskIsolationSlot(repoRoot, "Worker");
		const dir = path.join(root, slot);
		await fs.mkdir(path.join(dir, "m"), { recursive: true });
		const sessionFile = path.join(await tempDir("omp-sess-"), "Worker.jsonl");
		await Bun.write(
			sessionFile,
			`${JSON.stringify({ type: "session", id: "child-new", cwd: path.join(dir, "m") })}\n`,
		);
		const baseline = {
			root: { repoRoot, headCommit: "x", staged: "", unstaged: "", untracked: [], untrackedPatch: "" },
			nested: [],
		};
		// Structural test fixture: bind only reads these descriptor fields.
		const state = {
			version: 1,
			id: "Worker",
			repoRoot,
			backend: natives.IsoBackendKind.Rcopy,
			slot,
			mergedDir: "m",
			dir,
			baseline,
			sessionFile,
			deferred: true,
			deferredReady: true,
			childSessionId: "child-old",
			parentSessionFile: sessionFile,
			parentSessionId: "parent-1",
		} as LoadedIsolationState;
		const reused = await worktreeModule.bindIsolationState(state, sessionFile);
		expect("refusal" in reused && reused.refusal).toContain("belongs to session child-old");

		const legacy = { ...state, childSessionId: undefined } as LoadedIsolationState;
		const missing = await worktreeModule.bindIsolationState(legacy, sessionFile);
		expect("refusal" in missing && missing.refusal).toContain("no child session generation");
	});

	it("shares its hold with revival and release", async () => {
		const { registry } = await deferredFixture();
		const ref = registry.get("Worker")!;
		const lifecycle = AgentLifecycleManager.global();
		const release = await lifecycle.reserve("Worker", ref);
		await expect(lifecycle.ensureLive("Worker")).rejects.toThrow("reserved");
		expect(await lifecycle.release("Worker", ref)).toBe(false);
		release();
	});
});
