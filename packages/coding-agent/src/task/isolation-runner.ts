/**
 * Reusable isolation lifecycle for subagent execution.
 *
 * Both `TaskTool` and the eval `agent()` bridge spawn subagents that can run
 * inside a copy-on-write worktree, capture their changes, and (optionally)
 * apply those changes back to the parent repo. The orchestration is identical
 * for both callers; this module hosts the shared lifecycle so eval `agent()`
 * does not need to round-trip through `TaskTool.#runSpawn`.
 *
 * Shape:
 *   1. {@link prepareIsolationContext} — resolve the git root.
 *   2. {@link runIsolatedSubprocess}    — start worktree, capture its
 *                                        baseline, run, capture changes, and
 *                                        transfer cleanup ownership.
 *   3. {@link mergeIsolatedChanges}     — apply captured changes back to the
 *                                        parent repo (skip when the caller
 *                                        opted out).
 *
 * Step 1 happens once per top-level call; steps 2 and 3 are per-spawn.
 */
import * as fs from "node:fs/promises";
import * as path from "node:path";
import type * as natives from "@oh-my-pi/pi-natives";
import * as vcs from "@oh-my-pi/pi-natives/vcs";
import { logger, prompt } from "@oh-my-pi/pi-utils";
import isolationErrorTemplate from "../prompts/tools/isolation-error.md" with { type: "text" };
import isolationSummaryTemplate from "../prompts/tools/isolation-summary.md" with { type: "text" };
import { AgentLifecycleManager } from "../registry/agent-lifecycle";
import { AgentRegistry } from "../registry/agent-registry";
import type { ToolSession } from "../tools";
import { generateCommitMessage } from "../utils/commit-message-generator";
import { trackLateCleanup } from "../utils/late-cleanup";
import type { ExecutorOptions } from "./executor";
import { runSubprocess } from "./executor";
import {
	needsNativeTeardown,
	readIsolationOwner,
	withIsolationLease,
	writeRetainedBackend,
} from "./isolation-ownership";
import type { NestedRepoPatch, SingleResult } from "@oh-my-pi/pi-tui/tools/task";
import {
	applyNestedPatches,
	captureDeltaPatch,
	captureIsolationBaseline,
	cleanupIsolation,
	cleanupTaskBranches,
	type CommitToBranchResult,
	bindIsolationState,
	commitToBranch,
	ensureIsolation,
	getRepoRoot,
	isolationHandle,
	type IsolationHandle,
	type LoadedIsolationState,
	mergeTaskBranches,
	patchIsolationState,
	writeIsolationState,
	type WorktreeBaseline,
} from "./worktree";

import { cfgTaskIsolationCommits } from "./settings";

type IsoBackendKind = natives.IsoBackendKind;

/** Which isolation outcome `isolation-summary.md` should describe. */
export type IsolationSummaryKind =
	| "captured"
	| "capture-error"
	| "nested-apply-failed"
	| "not-applied"
	| "branch-merge-failed"
	| "branch-capture-failed"
	| "merge-error";

/** Context for `isolation-summary.md`; unused fields are simply absent. */
export interface IsolationSummaryContext {
	kind: IsolationSummaryKind;
	branchName?: string;
	/** Root patch path, only when it holds changes. */
	rootPatchPath?: string;
	nestedCount?: number;
	nestedPatchPaths?: string[];
	error?: string;
	conflict?: string;
	/** Workspace kept because its changes could not be captured; a later release still owns it. */
	retainedDir?: string;
}

/**
 * Render one isolation outcome as the model-facing suffix appended to a task
 * result. Always starts with a blank line so it separates from the output it
 * follows.
 */
export function renderIsolationSummary(context: IsolationSummaryContext): string {
	return `\n\n${prompt.render(isolationSummaryTemplate, { ...context })}`;
}

/** Record artifact locations for `agent://` and mark the result as an isolated run. */
function rememberAgentArtifacts(result: SingleResult): SingleResult {
	AgentRegistry.global().setHistory(result.id, {
		outputPath: result.outputPath,
		patchPath: result.patchPath,
		branchName: result.branchName,
		nestedPatchPaths: result.nestedPatchPaths,
	});
	return { ...result, isolated: true };
}

/**
 * Decide the fate of a half-built task branch after apply-back threw.
 *
 * Returns the branch name when it carries at least one commit past `baseSha`
 * — the caller must keep it, because the isolation worktree that also held
 * those objects is about to be torn down. Returns `undefined` after deleting
 * a branch that is absent, empty, or still pinned at the baseline, preserving
 * the original stale-branch cleanup for the cases where nothing is at stake.
 *
 * `revList.range` throws when the branch does not exist, which is the common
 * "commitToBranch failed before it created anything" path; that is treated as
 * "nothing to rescue" only after confirming the ref is absent. Other probe
 * failures preserve the branch because deleting it could lose the only reachable
 * copy of the agent's commits.
 */
async function rescueTaskBranch(repoRoot: string, branchName: string, baseSha: string): Promise<string | undefined> {
	const repo = vcs.git(repoRoot);
	try {
		const carriedCommits = (await vcs.requireGit(repoRoot).revListRange(baseSha, branchName)).length;
		if (carriedCommits > 0) return branchName;
	} catch {
		try {
			if (await repo?.refExists(`refs/heads/${branchName}`)) return branchName;
		} catch {
			// An inconclusive recovery probe must never risk deleting the only ref.
			return branchName;
		}
	}
	try {
		await repo?.deleteBranch(branchName, true);
	} catch {
		// Best-effort cleanup matches the old façade's tryDelete semantics.
	}
	return undefined;
}

/** Resolved repo shared by every isolated spawn in a single call. */
export interface IsolationContext {
	repoRoot: string;
}

/**
 * Resolve the git repo root isolated spawns are copied from and merged back
 * into. Throws when the cwd is not inside a git repository; callers surface
 * the error as a task-tool failure. Each spawn captures its own baseline from
 * its isolation copy (see {@link captureIsolationBaseline}).
 */
export async function prepareIsolationContext(cwd: string, explicitRepoRoot?: string): Promise<IsolationContext> {
	return { repoRoot: await getRepoRoot(cwd, explicitRepoRoot) };
}

/** Fingerprint of a captured delta, used to skip re-committing an unchanged workspace on release. */
function deltaFingerprint(rootPatch: string, nestedPatches: readonly NestedRepoPatch[]): bigint | number {
	let key = rootPatch;
	for (const { relativePath, patch } of nestedPatches) key += `\0${relativePath}\0${patch}`;
	return Bun.hash(key);
}

/** Build a commit-message callback for branch/nested commits; `undefined` ⇒ fall back to generic message. */
export type BuildCommitMessage = () => undefined | ((diff: string) => Promise<string | null>);

/**
 * Construct the commit-message factory used by isolation branch commits and
 * nested-repo patch commits. Returns a closure that, each time it's called,
 * either yields an AI-backed `(diff) => Promise<string|null>` callback (when
 * `task.isolation.commits === "ai"` and a model registry is available) or
 * `undefined` so the caller falls back to a generic commit message.
 *
 * Centralized so `TaskTool` and the eval `agent()` bridge share one wiring;
 * a drift here previously meant the two callers built subtly different
 * generators for the same setting.
 */
export function makeIsolationCommitMessage(session: ToolSession): BuildCommitMessage {
	return () => {
		const style = cfgTaskIsolationCommits.get(session.settings);
		if (style !== "ai" || !session.modelRegistry) return undefined;
		const registry = session.modelRegistry;
		const settings = session.settings;
		const sessionId = session.getSessionId?.() ?? undefined;
		return async (diff: string) => generateCommitMessage(diff, registry, settings, sessionId);
	};
}

export interface IsolatedRunOptions {
	/**
	 * Base run options handed to the subagent subprocess. This helper sets
	 * `worktree`, clears prepared/path extension preloads and custom-tool paths
	 * (isolated runs re-discover inside the worktree), and forwards everything
	 * else unchanged.
	 */
	baseOptions: ExecutorOptions;
	/** Context returned by {@link prepareIsolationContext}. */
	context: IsolationContext;
	/** PAL backend hint from `parseIsolationBackend(...)` (undefined ⇒ resolver picks). */
	preferredBackend: IsoBackendKind | undefined;
	/** Stable id used as the isolation worktree namespace and as the branch suffix. */
	agentId: string;
	/** Merge mode driving how changes are captured ("branch" commits, "patch" diffs). */
	mergeMode: "patch" | "branch";
	/** Output dir for `${agentId}.patch` artifacts (patch mode and branch-mode commit failures). */
	artifactsDir: string;
	/** Human description carried onto the branch commit (branch mode). */
	description?: string;
	/** Build a commit-message callback (`task.isolation.commits === "ai"`). */
	buildCommitMessage?: BuildCommitMessage;
	/**
	 * Construct a `SingleResult` when isolation setup throws — the caller has
	 * the full metadata (index, agent, assignment, modelOverride) needed to
	 * build a result shape consistent with their non-isolated path.
	 */
	buildFailureResult: (err: unknown) => SingleResult;
	/** Observe the real child result before post-run isolation work. */
	onSubprocessResult?: (result: SingleResult) => void;
}

/**
 * Write each nested-repo patch to `${artifactsDir}/${agentId}.nested-<n>-<path>.patch`
 * and return the paths. Throws on the first write failure: the caller must
 * then keep the isolation workspace alive, because it is the only other copy.
 * Every attempted destination is removed best-effort on failure — including
 * the in-progress file, which `Bun.write` may have created or truncated
 * before rejecting — so a half-written set cannot be mistaken for the
 * complete capture by the persisted-agent scanner.
 */
export async function persistNestedPatches(
	artifactsDir: string,
	agentId: string,
	nestedPatches: readonly NestedRepoPatch[],
): Promise<string[]> {
	const saved: string[] = [];
	try {
		for (const [index, nestedPatch] of nestedPatches.entries()) {
			const destination = path.join(
				artifactsDir,
				`${agentId}.nested-${index}-${nestedPatch.relativePath.replace(/[^a-zA-Z0-9._-]/g, "_") || "root"}.patch`,
			);
			// Track before writing: a mid-write failure (ENOSPC, quota) can
			// leave a truncated file behind, and `force: true` makes removing
			// a never-created path a no-op.
			saved.push(destination);
			await Bun.write(destination, nestedPatch.patch);
		}
	} catch (error) {
		await Promise.all(saved.map(file => fs.rm(file, { force: true }).catch(() => undefined)));
		throw error;
	}
	return saved;
}

interface IsolationPatchArtifacts {
	patchPath: string;
	hasRootChanges: boolean;
	nestedPatches: NestedRepoPatch[];
	nestedPatchPaths: string[];
}

interface IsolationPatchCapture {
	artifacts: IsolationPatchArtifacts;
	/** {@link deltaFingerprint} of the written delta. */
	fingerprint: bigint | number;
}

/**
 * Capture the isolation delta and write every part of it to disk — the root
 * patch and one file per nested repo — before the caller tears the workspace
 * down. Throws when any write fails so nothing captured is ever the only copy.
 */
async function writeIsolationPatch(
	isolationDir: string,
	baseline: WorktreeBaseline,
	artifactsDir: string,
	agentId: string,
): Promise<IsolationPatchCapture> {
	const delta = await captureDeltaPatch(isolationDir, baseline);
	const patchPath = path.join(artifactsDir, `${agentId}.patch`);
	await Bun.write(patchPath, delta.rootPatch);
	const nestedPatchPaths = await persistNestedPatches(artifactsDir, agentId, delta.nestedPatches);
	return {
		artifacts: {
			patchPath,
			hasRootChanges: delta.rootPatch.trim().length > 0,
			nestedPatches: delta.nestedPatches,
			nestedPatchPaths,
		},
		fingerprint: deltaFingerprint(delta.rootPatch, delta.nestedPatches),
	};
}

/**
 * Move a retained isolation workspace out of its deterministic
 * (`repoRoot` + agent id) slot into a globally unique sibling, so a later
 * isolated run with the same id cannot wipe it: `ensureIsolation`
 * unconditionally removes the deterministic base dir before writing its
 * owner marker. The owner marker, `m` mount, and backend sidecar move along,
 * so `omp worktree clear` still classifies and reclaims the workspace with
 * native teardown. Backends needing it (mounts, Btrfs subvolumes) record the
 * sidecar BEFORE the move so it travels atomically — a crash between rename
 * and a later write would leave a mounted workspace with a dead owner and
 * no metadata, and cleanup would traverse the live mount.
 */
export interface RetainedWorkspace {
	/** Workspace path to report (unique sibling on success, original dir when the move fails). */
	dir: string;
	/**
	 * False when cleanup metadata is missing that `clear` would need: the
	 * sidecar could not be written (plausible under the same disk pressure
	 * that forced retention). The error must then say the mount needs a
	 * manual unmount instead of advertising plain `worktree clear`.
	 */
	sidecarOk: boolean;
}

export async function retainIsolationWorkspace(
	isolationDir: string,
	backend?: natives.IsoBackendKind,
): Promise<RetainedWorkspace> {
	const baseDir = path.dirname(isolationDir);
	const retainedBase = `${baseDir}.retained-${Date.now().toString(36)}-${Math.floor(Math.random() * 2 ** 32).toString(16)}`;
	const needsSidecar = backend !== undefined && needsNativeTeardown(backend);
	let sidecarOk = !needsSidecar;
	if (needsSidecar && backend !== undefined) {
		try {
			await writeRetainedBackend(baseDir, backend);
			sidecarOk = true;
		} catch {
			sidecarOk = false;
		}
	}
	// A valid move can still fail transiently (Windows AV/indexer locks);
	// retry briefly before conceding the deterministic slot.
	for (let attempt = 0; attempt < 3; attempt++) {
		try {
			await fs.rename(baseDir, retainedBase);
			return { dir: path.join(retainedBase, path.basename(isolationDir)), sidecarOk };
		} catch {
			if (attempt === 2) return { dir: isolationDir, sidecarOk };
			await Bun.sleep(25);
		}
	}
	return { dir: isolationDir, sidecarOk };
}
/** Context for `isolation-error.md`: the `result.error` text for a run whose changes could not be captured or landed. */
interface IsolationErrorContext {
	kind: "merge-failed" | "patch-capture-failed" | "nested-capture-failed";
	message: string;
	captureError?: string;
	rescueBranch?: string;
	/** Set when the workspace was kept because its changes could not be written out. */
	retainedDir?: string;
	/**
	 * Set when the retained mount's unmount metadata is missing: cleanup
	 * cannot unmount before removal, so the message must direct a manual
	 * unmount instead of advertising plain `worktree clear`.
	 */
	sidecarMissing?: boolean;
}

function renderIsolationError(context: IsolationErrorContext): string {
	return prompt.render(isolationErrorTemplate, { ...context });
}

/** Pointer beside the transcript naming where an agent's isolation sandbox lives, across retention renames. */
function retainedPointerPath(artifactsDir: string, id: string): string {
	return path.join(artifactsDir, `${id}.isolation.json`);
}

/**
 * Record the sandbox `baseDir` that holds `workspace` beside the transcript.
 * Best-effort: the header cwd still resolves an unrenamed workspace, and a
 * rename-time failure keeps the (already reported) error path working.
 */
async function writeIsolationPointer(
	artifactsDir: string,
	id: string,
	baseDir: string,
	workspace: string,
): Promise<void> {
	try {
		await Bun.write(retainedPointerPath(artifactsDir, id), JSON.stringify({ dir: baseDir, workspace }));
	} catch (error) {
		logger.debug("isolation pointer write failed", {
			id,
			workspace,
			error: error instanceof Error ? error.message : String(error),
		});
	}
}

/**
 * Record a workspace that was moved out of its deterministic slot: the pointer
 * lets a later process find the descriptor, and the Hub row can show where the
 * retained work lives.
 */
async function rememberRetainedWorkspace(artifactsDir: string, id: string, retained: RetainedWorkspace): Promise<void> {
	AgentRegistry.global().setHistory(id, { isolationDir: retained.dir });
	await writeIsolationPointer(artifactsDir, id, path.dirname(retained.dir), retained.dir);
}

/** Options for {@link releaseIsolationWorkspace}. */
export interface IsolationReleaseOptions {
	/** Dir holding `${id}.patch` artifacts. */
	artifactsDir: string;
	/** Human description carried onto a branch commit. */
	description?: string;
	/** Release the owning session's resources before the workspace teardown. */
	releaseParent?: () => Promise<void>;
	/**
	 * Transcript the CALLER knows this agent owns, independent of the descriptor.
	 * Binding against it stops a forged/copied descriptor from naming its own
	 * transcript to authorize capture and deletion of another session's sandbox.
	 */
	expectedSessionFile: string;
}

/**
 * Capture a workspace's changes, commit them when they differ from the handoff
 * already delivered, and tear the workspace down.
 *
 * Shared by the live release (same process) and the cold release of a revived
 * run, so both diff against the persisted baseline, skip a duplicate commit,
 * and stop the backend the workspace recorded. Capture failure retains the
 * workspace under a unique sibling, points the transcript at it, and throws
 * WITHOUT cleanup — the workspace is then the only copy of the run's changes.
 */
export async function releaseIsolationWorkspace(
	state: LoadedIsolationState,
	opts: IsolationReleaseOptions,
): Promise<void> {
	// Prove the descriptor is bound to the caller's transcript and slot BEFORE
	// any capture or teardown: a copied pointer/descriptor must not hand release
	// another session's workspace. Refusal throws with no mutation.
	const binding = await bindIsolationState(state, opts.expectedSessionFile);
	if ("refusal" in binding) throw new Error(`Cannot release isolation workspace: ${binding.refusal}`);
	const handle = isolationHandle(state);
	// Destructive release also runs under the claim lock and requires that THIS
	// process currently owns the sandbox (marker written by spawn/revive claim):
	// a foreign-live or unclaimed workspace is refused untouched, and `clear`
	// cannot tear it down mid-release.
	await withIsolationLease(state.slot, async () => {
		const owner = await readIsolationOwner(state.dir);
		if (!owner || owner.pid !== process.pid) {
			throw new Error(
				`Cannot release isolation workspace: it is ${owner ? `owned by omp process ${owner.pid}` : "not claimed by any process"}; revive and claim it first`,
			);
		}
		let retain = false;
		try {
			let capture: IsolationPatchCapture;
			try {
				capture = await writeIsolationPatch(handle.mergedDir, state.baseline, opts.artifactsDir, state.id);
			} catch (captureErr) {
				retain = true;
				const retained = await retainIsolationWorkspace(handle.mergedDir, handle.backend);
				await rememberRetainedWorkspace(opts.artifactsDir, state.id, retained);
				throw new Error(
					renderIsolationError({
						kind: "patch-capture-failed",
						message: captureErr instanceof Error ? captureErr.message : String(captureErr),
						retainedDir: retained.dir,
						sidecarMissing: !retained.sidecarOk,
					}),
				);
			}
			const patchResult = capture.artifacts;
			AgentRegistry.global().setHistory(state.id, {
				patchPath: patchResult.patchPath,
				nestedPatchPaths: patchResult.nestedPatchPaths,
			});
			// Nothing changed since the run-end capture the caller already merged
			// or applied: a branch now would only duplicate that work.
			if (state.handedOff !== undefined && String(capture.fingerprint) === state.handedOff) return;
			let commitResult: CommitToBranchResult | null;
			try {
				commitResult = await commitToBranch(
					handle.mergedDir,
					state.baseline,
					state.id,
					opts.description,
					undefined,
				);
			} catch (commitErr) {
				// The written patch holds the diff, but commits made inside the
				// workspace and the branch ref live only there. Keep the workspace
				// and its descriptor so the release can be retried instead of
				// losing them.
				retain = true;
				const rescueBranch = await rescueTaskBranch(
					state.baseline.root.repoRoot,
					`omp/task/${state.id}`,
					state.baseline.root.headCommit,
				);
				const retained = await retainIsolationWorkspace(handle.mergedDir, handle.backend);
				await rememberRetainedWorkspace(opts.artifactsDir, state.id, retained);
				throw new Error(
					renderIsolationError({
						kind: "merge-failed",
						message: commitErr instanceof Error ? commitErr.message : String(commitErr),
						rescueBranch,
						retainedDir: retained.dir,
						sidecarMissing: !retained.sidecarOk,
					}),
				);
			}
			AgentRegistry.global().setHistory(state.id, {
				patchPath: patchResult.patchPath,
				branchName: commitResult?.branchName,
				nestedPatchPaths: patchResult.nestedPatchPaths,
			});
		} finally {
			await opts.releaseParent?.();
			if (!retain) await cleanupIsolation(handle);
		}
	});
}

/**
 * Run a subagent inside an isolation worktree and capture its changes.
 *
 * Branch mode: on success, commits the diff onto `omp/task/${agentId}` and
 * returns `branchName` + `nestedPatches` (+ `nestedPatchPaths`). On commit
 * failure the still-live isolation diff is written to
 * `${artifactsDir}/${agentId}.patch`, the task branch is kept when it already
 * carries commits (deleted otherwise), and `result.error` carries the
 * merge-failure message plus recovery hint.
 *
 * Patch mode: on success, writes `${artifactsDir}/${agentId}.patch` plus one
 * `${agentId}.nested-<n>-<path>.patch` per nested repo and returns
 * `patchPath` + `nestedPatches` + `nestedPatchPaths`.
 *
 * Failure paths preserve the underlying `SingleResult` whenever possible so
 * the caller can still surface the subagent's output; only isolation setup
 * itself routes through {@link IsolatedRunOptions.buildFailureResult}.
 *
 * Kept-alive runs retain the isolation handle through idle/parked lifecycle
 * transitions, then capture final changes and clean up on release. One-shot
 * and failed startup paths clean up in `finally`. If captured changes cannot
 * be written to disk, the workspace is retained under a unique `.retained-*`
 * sibling and its path is named in the resulting error.
 */
export async function runIsolatedSubprocess(opts: IsolatedRunOptions): Promise<SingleResult> {
	let taskBaseline: WorktreeBaseline | undefined;
	/** Fingerprint of the delta already handed to the caller at run end; release only re-captures on change. */
	let handedOff: bigint | number | undefined;
	let handle: IsolationHandle | undefined;
	/** Sandbox dir currently holding this run's descriptor (follows retention renames). */
	let stateDir: string | undefined;
	/** Immutable slot basename: retention renames the base dir, the slot never changes. */
	let isolationSlot: string | undefined;
	/** True once the descriptor exists: such a workspace is retained until an explicit release. */
	let stateWritten = false;
	let deferredCleanup: Promise<void> | undefined;
	let retainWorkspace = false;
	let baseReleasePromise: Promise<void> | undefined;
	let cleanupPromise: Promise<void> | undefined;
	let releasePromise: Promise<void> | undefined;
	/** The run's IRC wake hold, released only once the run-end capture is done. */
	let releaseIrcWakes: (() => void) | undefined;
	const releaseBase = (): Promise<void> => {
		baseReleasePromise ??= opts.baseOptions.onRelease?.() ?? Promise.resolve();
		return baseReleasePromise;
	};
	const cleanupHandle = (): Promise<void> => {
		cleanupPromise ??= (async () => {
			await releaseBase();
			if (handle && !retainWorkspace) await cleanupIsolation(handle);
		})();
		return cleanupPromise;
	};
	/**
	 * Terminal handler wired into the agent lifecycle for EVERY isolated run.
	 * `retain` keeps the workspace (one-shot completion, kill tombstone,
	 * shutdown) for the explicit release that a revived run performs later; a
	 * plain release captures and lands the delta through the shared handler.
	 */
	const releaseIsolation = (release?: { retain?: boolean }): Promise<void> => {
		releasePromise ??= (async () => {
			if (!handle || !taskBaseline) {
				await cleanupHandle();
				return;
			}
			if (release?.retain) {
				retainWorkspace = true;
				await releaseBase();
				return;
			}
			const sessionFile =
				AgentRegistry.global().get(opts.agentId)?.sessionFile ??
				path.join(opts.artifactsDir, `${opts.agentId}.jsonl`);
			await releaseIsolationWorkspace(
				{
					version: 1,
					id: opts.agentId,
					repoRoot: opts.context.repoRoot,
					backend: handle.backend,
					slot: isolationSlot ?? path.basename(path.dirname(handle.mergedDir)),
					mergedDir: path.basename(handle.mergedDir),
					baseline: taskBaseline,
					// Follow a retention rename performed at run end: the original
					// handle path no longer exists after `retainIsolationWorkspace`.
					dir: stateDir ?? path.dirname(handle.mergedDir),
					sessionFile,
					...(handedOff === undefined ? {} : { handedOff: String(handedOff) }),
				},
				{
					artifactsDir: opts.artifactsDir,
					description: opts.description,
					releaseParent: releaseBase,
					expectedSessionFile: sessionFile,
				},
			);
		})();
		return releasePromise;
	};
	try {
		handle = await ensureIsolation(opts.context.repoRoot, opts.agentId, opts.preferredBackend);
		const isolationDir = handle.mergedDir;
		const isolationBaseDir = path.dirname(isolationDir);
		isolationSlot = path.basename(isolationBaseDir);
		const isolationBackend = handle.backend;
		const baseline = await captureIsolationBaseline(isolationDir, opts.context.repoRoot);
		taskBaseline = baseline;
		// Persist the recovery contract before the agent can change the workspace:
		// a later process must capture this run's delta against this exact
		// baseline and stop the backend that materialised it. A write failure is
		// a setup failure — nothing is claimed yet, so the catch below discards
		// the copy and the agent never runs.
		await writeIsolationState(isolationBaseDir, {
			version: 1,
			id: opts.agentId,
			repoRoot: opts.context.repoRoot,
			backend: isolationBackend,
			slot: isolationSlot,
			mergedDir: path.basename(isolationDir),
			baseline,
			// Convention path; the run-end patch replaces it with the exact
			// transcript the registry recorded.
			sessionFile: path.join(opts.artifactsDir, `${opts.agentId}.jsonl`),
		});
		stateDir = isolationBaseDir;
		stateWritten = true;
		await writeIsolationPointer(opts.artifactsDir, opts.agentId, isolationBaseDir, isolationDir);
		const result = await runSubprocess({
			...opts.baseOptions,
			worktree: isolationDir,
			preloadedExtensionPaths: undefined,
			preloadedPreparedExtensions: undefined,
			preloadedCustomToolPaths: undefined,
			onCleanupDeferred: completion => {
				deferredCleanup = completion;
				opts.baseOptions.onCleanupDeferred?.(completion);
			},
			// Every isolated run keeps the workspace until an explicit release;
			// one-shot runs pass `retain` from `finalizeSubagentLifecycle`.
			onRelease: releaseIsolation,
			deferIrcWakeRelease: release => {
				releaseIrcWakes = release;
			},
		});
		opts.onSubprocessResult?.(result);
		AgentRegistry.global().setHistory(opts.agentId, { isolationDir });
		// A successful result cannot be captured while deferred owner jobs or
		// shutdown hooks may still write the worktree. Failed runs skip capture,
		// so their cleanup remains asynchronous.
		if (deferredCleanup && result.exitCode === 0) {
			await deferredCleanup;
		}
		if (opts.mergeMode === "branch" && result.exitCode === 0) {
			let commitResult: CommitToBranchResult | null;
			try {
				commitResult = await commitToBranch(
					isolationDir,
					baseline,
					opts.agentId,
					opts.description,
					opts.buildCommitMessage?.(),
				);
			} catch (mergeErr) {
				// Agent succeeded but the branch commit failed. `commitToBranch`
				// is not atomic: the clean-baseline path fetches the agent's
				// commits into the parent ODB and creates `omp/task/<id>` before
				// it commits the leftover working-tree delta, so a throw from
				// that trailing step leaves behind a branch that already holds
				// every commit the agent made. The isolation worktree — the only
				// other copy of those objects — is destroyed by the `finally`
				// below, so deleting the branch unconditionally turned a
				// recoverable merge conflict into permanent loss of committed
				// work (#8868). Delete only when nothing is at stake.
				const baseSha = baseline.root.headCommit;
				const branchName = `omp/task/${opts.agentId}`;
				const rescueBranch = await rescueTaskBranch(opts.context.repoRoot, branchName, baseSha);
				const msg = mergeErr instanceof Error ? mergeErr.message : String(mergeErr);
				try {
					const capture = await writeIsolationPatch(isolationDir, baseline, opts.artifactsDir, opts.agentId);
					return rememberAgentArtifacts({
						...result,
						...capture.artifacts,
						error: renderIsolationError({ kind: "merge-failed", message: msg, rescueBranch }),
					});
				} catch (patchErr) {
					retainWorkspace = true;
					const retained = await retainIsolationWorkspace(isolationDir, isolationBackend);
					stateDir = path.dirname(retained.dir);
					await rememberRetainedWorkspace(opts.artifactsDir, opts.agentId, retained);
					return rememberAgentArtifacts({
						...result,
						error: renderIsolationError({
							kind: "merge-failed",
							message: msg,
							captureError: patchErr instanceof Error ? patchErr.message : String(patchErr),
							rescueBranch,
							retainedDir: retained.dir,
							sidecarMissing: !retained.sidecarOk,
						}),
					});
				}
			}
			handedOff = deltaFingerprint(commitResult?.rootPatch ?? "", commitResult?.nestedPatches ?? []);
			// The branch holds the root-repo work, but nested-repo patches exist
			// only in memory until written; the workspace goes away in `finally`.
			try {
				const nestedPatchPaths = await persistNestedPatches(
					opts.artifactsDir,
					opts.agentId,
					commitResult?.nestedPatches ?? [],
				);
				return rememberAgentArtifacts({
					...result,
					branchName: commitResult?.branchName,
					branchBaseSha: commitResult?.baseSha,
					nestedPatches: commitResult?.nestedPatches,
					nestedPatchPaths,
				});
			} catch (persistErr) {
				retainWorkspace = true;
				const retained = await retainIsolationWorkspace(isolationDir, isolationBackend);
				stateDir = path.dirname(retained.dir);
				await rememberRetainedWorkspace(opts.artifactsDir, opts.agentId, retained);
				return rememberAgentArtifacts({
					...result,
					branchName: commitResult?.branchName,
					branchBaseSha: commitResult?.baseSha,
					nestedPatches: commitResult?.nestedPatches,
					error: renderIsolationError({
						kind: "nested-capture-failed",
						message: persistErr instanceof Error ? persistErr.message : String(persistErr),
						retainedDir: retained.dir,
						sidecarMissing: !retained.sidecarOk,
					}),
				});
			}
		}
		if (result.exitCode === 0) {
			try {
				const capture = await writeIsolationPatch(isolationDir, baseline, opts.artifactsDir, opts.agentId);
				handedOff = capture.fingerprint;
				return rememberAgentArtifacts({ ...result, ...capture.artifacts });
			} catch (patchErr) {
				retainWorkspace = true;
				const retained = await retainIsolationWorkspace(isolationDir, isolationBackend);
				stateDir = path.dirname(retained.dir);
				await rememberRetainedWorkspace(opts.artifactsDir, opts.agentId, retained);
				return rememberAgentArtifacts({
					...result,
					error: renderIsolationError({
						kind: "patch-capture-failed",
						message: patchErr instanceof Error ? patchErr.message : String(patchErr),
						retainedDir: retained.dir,
						sidecarMissing: !retained.sidecarOk,
					}),
				});
			}
		}
		return rememberAgentArtifacts(result);
	} catch (err) {
		return rememberAgentArtifacts(opts.buildFailureResult(err));
	} finally {
		// Record what the caller already received so a later release commits only
		// genuinely new work. Best-effort: a missing patch degrades to an
		// unconditional re-capture, never to a lost workspace.
		if (stateWritten && stateDir) {
			const sessionFile = AgentRegistry.global().get(opts.agentId)?.sessionFile ?? undefined;
			try {
				await patchIsolationState(stateDir, {
					...(sessionFile === undefined ? {} : { sessionFile }),
					...(handedOff === undefined ? {} : { handedOff: String(handedOff) }),
				});
			} catch (error) {
				logger.debug("isolation state patch failed; a later release re-captures unconditionally", {
					agentId: opts.agentId,
					error: error instanceof Error ? error.message : String(error),
				});
			}
		}
		// A workspace with a descriptor is retained until an explicit release:
		// run end (one-shot, shutdown, kill) never destroys it.
		if (
			handle &&
			!stateWritten &&
			!retainWorkspace &&
			!releasePromise &&
			!(opts.baseOptions.keepAlive !== false && AgentLifecycleManager.global().has(opts.agentId))
		) {
			if (deferredCleanup) {
				trackLateCleanup(deferredCleanup.then(cleanupHandle), {
					agentId: opts.agentId,
					resource: "isolation",
				});
			} else {
				await cleanupHandle();
			}
		}
		// The worktree is captured: a held peer message may now wake a turn in it.
		if (releaseIrcWakes && opts.baseOptions.deferIrcWakeRelease)
			opts.baseOptions.deferIrcWakeRelease(releaseIrcWakes);
		else releaseIrcWakes?.();
	}
}

export interface IsolationMergeOptions {
	result: SingleResult;
	repoRoot: string;
	mergeMode: "patch" | "branch";
}

export interface IsolationMergeOutcome {
	/** Trailing summary appended to the subagent's result text. May be empty. */
	summary: string;
	/**
	 * Tri-state apply outcome:
	 * - `true`  — merge ran (or had nothing to apply) and left the repo clean.
	 * - `false` — merge attempted and failed; artifacts are preserved.
	 * - `null`  — caller skipped the merge phase entirely (e.g. `apply=false`).
	 */
	changesApplied: boolean | null;
	hadAnyChanges: boolean;
	/** True iff the root branch actually merged — gates nested-repo patch application. */
	mergedBranchForNestedPatches: boolean;
}

/**
 * Apply changes captured by {@link runIsolatedSubprocess} back to the parent
 * repo: patch apply (patch mode) or cherry-pick + cleanup (branch mode).
 *
 * The caller decides whether to run this at all — eval `agent()` with
 * `apply=False` skips this step and surfaces the patch artifact / branch name
 * instead.
 */
export async function mergeIsolatedChanges(opts: IsolationMergeOptions): Promise<IsolationMergeOutcome> {
	const { result, repoRoot, mergeMode } = opts;
	const repo = vcs.requireGit(repoRoot);
	// A capture failure retains the workspace; naming it tells the parent the
	// work is still recoverable instead of lost.
	const retainedDir = AgentRegistry.global().get(result.id)?.history?.isolationDir;
	try {
		if (mergeMode === "branch") {
			if (!result.branchName && result.exitCode === 0 && !result.aborted && result.error) {
				return {
					summary: renderIsolationSummary({
						kind: "branch-capture-failed",
						error: result.error,
						rootPatchPath: result.patchPath,
						nestedPatchPaths: result.nestedPatchPaths,
						...(retainedDir ? { retainedDir } : {}),
					}),
					changesApplied: false,
					hadAnyChanges: false,
					mergedBranchForNestedPatches: false,
				};
			}
			const canApplyNestedOnly =
				!result.branchName && result.exitCode === 0 && !result.aborted && (result.nestedPatches?.length ?? 0) > 0;
			if (!result.branchName || result.exitCode !== 0 || result.aborted) {
				return {
					summary: canApplyNestedOnly
						? "\n\nNo root changes to apply; nested repository patches captured."
						: "\n\nNo changes to apply.",
					changesApplied: true,
					hadAnyChanges: canApplyNestedOnly,
					mergedBranchForNestedPatches: canApplyNestedOnly,
				};
			}
			const mergeResult = await mergeTaskBranches(repoRoot, [
				{
					branchName: result.branchName,
					taskId: result.id,
					description: result.description,
					baseSha: result.branchBaseSha,
				},
			]);
			const mergedBranchForNestedPatches = mergeResult.merged.includes(result.branchName);
			const changesApplied = mergeResult.failed.length === 0;
			const hadAnyChanges = changesApplied && mergeResult.merged.length > 0;

			let summary: string;
			if (changesApplied) {
				summary = hadAnyChanges ? `\n\nMerged branch: ${result.branchName}` : "\n\nNo changes to apply.";
			} else {
				// The nested patches are skipped when the branch did not merge; name
				// their files so the parent can recover them alongside the branch.
				summary = renderIsolationSummary({
					kind: "branch-merge-failed",
					branchName: result.branchName,
					conflict: mergeResult.conflict,
					nestedPatchPaths: result.nestedPatchPaths,
				});
			}

			// Clean up the merged branch (keep failed ones for manual resolution)
			if (changesApplied) {
				await cleanupTaskBranches(repoRoot, [result.branchName]);
			}
			return { summary, changesApplied, hadAnyChanges, mergedBranchForNestedPatches };
		}

		// Patch mode: apply the patch from a successful run. A failed or
		// aborted run has nothing to apply and must not block the result.
		let changesApplied: boolean;
		let hadAnyChanges: boolean;
		const succeeded = result.exitCode === 0 && !result.error && !result.aborted;
		if (!succeeded) {
			changesApplied = true;
			hadAnyChanges = false;
		} else if (!result.patchPath) {
			changesApplied = false;
			hadAnyChanges = false;
		} else {
			const patchText = await Bun.file(result.patchPath).text();
			if (!patchText.trim()) {
				changesApplied = true;
				hadAnyChanges = false;
			} else {
				const normalized = patchText.endsWith("\n") ? patchText : `${patchText}\n`;
				// Idempotence: declare a no-op only when the reverse patch applies AND
				// the forward patch does not. `--reverse --check` alone can theoretically
				// succeed if the file happens to carry the postimage at another location
				// via git-apply's fuzz factor; requiring the forward check to fail
				// removes that ambiguity while still catching true already-applied
				// runs. Reads only — neither call touches the worktree, unlike
				// `--3way --check`, which exits 0 even when the real apply would
				// leave conflict markers and unmerged index entries.
				const [alreadyApplied, forwardApplies] = await Promise.all([
					repo.canApplyPatch(normalized, { reverse: true }).catch(() => false),
					repo.canApplyPatch(normalized, {}).catch(() => false),
				]);
				hadAnyChanges = false;
				if (alreadyApplied && !forwardApplies) {
					changesApplied = true;
				} else if (forwardApplies) {
					changesApplied = true;
					try {
						await repo.applyPatch(normalized, {});
						hadAnyChanges = true;
					} catch {
						changesApplied = false;
					}
				} else {
					changesApplied = false;
				}
			}
		}

		let summary: string;
		if (changesApplied) {
			summary = hadAnyChanges ? "\n\nApplied patches: yes" : "\n\nNo changes to apply.";
		} else {
			// Nested apply is skipped when the root patch did not apply; the
			// persisted nested patches are the parent's only pointer to that work.
			summary = renderIsolationSummary({
				kind: "not-applied",
				rootPatchPath: result.patchPath,
				nestedPatchPaths: result.nestedPatchPaths,
				...(retainedDir ? { retainedDir } : {}),
			});
		}
		return { summary, changesApplied, hadAnyChanges, mergedBranchForNestedPatches: false };
	} catch (mergeErr) {
		return {
			summary: renderIsolationSummary({
				kind: "merge-error",
				error: mergeErr instanceof Error ? mergeErr.message : String(mergeErr),
				branchName: result.branchName,
				rootPatchPath: result.patchPath,
				nestedPatchPaths: result.nestedPatchPaths,
			}),
			changesApplied: false,
			hadAnyChanges: false,
			mergedBranchForNestedPatches: false,
		};
	}
}

export interface NestedPatchApplyOptions {
	/** Subagent result carrying `nestedPatches`/`exitCode`/`aborted`. */
	result: SingleResult;
	repoRoot: string;
	mergeMode: "patch" | "branch";
	/** Parent merge outcome — patch mode skips nested apply when this is `false`. */
	changesApplied: boolean | null;
	/** Branch mode gates nested apply on whether the root branch merged. */
	mergedBranchForNestedPatches: boolean;
	/** Optional AI commit-message callback for nested commits; falls back to a generic message. */
	commitMessage?: (diff: string) => Promise<string | null>;
}

/**
 * Apply nested-repo patches after the parent merge phase. Centralizes the
 * three-way gate (exitCode/aborted, patch-mode failed parent, branch-mode
 * branch-merged) and the non-fatal failure handling so `TaskTool` and the
 * eval `agent()` bridge use one implementation.
 *
 * Returns a system-notification suffix to append to the parent merge summary,
 * or an empty string when nothing was applied or the nested apply succeeded.
 */
export async function applyEligibleNestedPatches(opts: NestedPatchApplyOptions): Promise<string> {
	const { result, repoRoot, mergeMode, changesApplied, mergedBranchForNestedPatches, commitMessage } = opts;
	if (mergeMode === "patch" && changesApplied === false) return "";
	const nestedPatches = result.nestedPatches ?? [];
	const eligible =
		nestedPatches.length > 0 &&
		result.exitCode === 0 &&
		!result.aborted &&
		(mergeMode !== "branch" || mergedBranchForNestedPatches);
	if (!eligible) return "";
	try {
		const warnings = await applyNestedPatches(repoRoot, nestedPatches, commitMessage);
		if (warnings.length === 0) return "";
		return `\n\n<system-notification>${warnings.join("\n")}</system-notification>`;
	} catch (applyErr) {
		// Nested patch failures are non-fatal to the parent merge, but the patch
		// files are the only surviving copy of that work — name them.
		return renderIsolationSummary({
			kind: "nested-apply-failed",
			error: applyErr instanceof Error ? applyErr.message : String(applyErr),
			nestedPatchPaths: result.nestedPatchPaths,
		});
	}
}
