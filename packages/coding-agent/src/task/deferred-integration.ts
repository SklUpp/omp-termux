/**
 * Explicit handoff for deferred isolated runs.
 *
 * `task.isolation.deferIntegration` keeps a successful isolated run's changes
 * in its retained workspace. The parent's consent is a manager signal, not a
 * message to the child:
 *
 *   write { path: "worktree://<id>", content: "/integrate" }
 *
 * This module is the management boundary. It never re-implements merging: it
 * proves identity, fences the agent against revival/release, captures the
 * latest delta, then calls the existing merge path with the merge mode recorded
 * at spawn. On any failure it preserves the workspace, descriptor, journal and
 * artifacts and reports the partial state honestly.
 *
 * Identity is a tuple, never just the reusable agent id:
 * - parent session file + session-header id (recorded at spawn);
 * - child transcript file + session-header id (recorded at run end);
 * - descriptor slot/repo/baseline binding (`bindIsolationState`).
 *
 * Durability:
 * - `applying` journal before the first parent mutation;
 * - `merged` journal + a tombstone beside the child transcript after success;
 * - a retry after a crashed/partial attempt fails closed instead of replaying;
 * - a retry after success is a no-op, even in a new process.
 */
import * as fs from "node:fs/promises";
import * as path from "node:path";
import * as vcs from "@oh-my-pi/pi-natives/vcs";
import type { NestedRepoPatch, SingleResult } from "@oh-my-pi/pi-tui/tools/task";
import { logger } from "@oh-my-pi/pi-utils";
import { AgentLifecycleManager } from "../registry/agent-lifecycle";
import { AgentRegistry, type AgentRef } from "../registry/agent-registry";
import { ensurePersistedRoster, sessionFileBelongsToRoot } from "../registry/persisted-agents";
import { SessionManager } from "../session/session-manager";
import {
	hasLiveIsolationOwner,
	readIsolationOwner,
	withIsolationLease,
	writeIsolationOwner,
} from "./isolation-ownership";
import { mergeIsolatedChanges, writeIsolationPatch } from "./isolation-runner";
import {
	applyNestedPatches,
	bindIsolationState,
	cleanupIsolation,
	commitToBranch,
	isolationHandle,
	readIsolationState,
	readSessionHeaderId,
	resolveIsolationWorkspace,
	type IsolationHandle,
	type LoadedIsolationState,
} from "./worktree";

/** Reserved coordination message: the parent's explicit integration consent. */
export const DEFERRED_INTEGRATION_SIGNAL = "/integrate";

/** Whether `message` is the reserved handoff signal rather than chat text. */
export function isDeferredIntegrationSignal(message: string): boolean {
	return message.trim() === DEFERRED_INTEGRATION_SIGNAL;
}

export interface DeferredIntegrationRequest {
	registry: AgentRegistry;
	/** Agent sending the signal; must be the target's parent session. */
	senderId: string;
	target: string;
	/** Sender's persisted transcript and session-header id (the authorization). */
	parentSessionFile?: string | null;
	parentSessionId?: string | null;
}

export interface DeferredIntegrationOutcome {
	text: string;
	isError: boolean;
}

interface IntegrationIdentity {
	childSessionFile: string;
	childSessionId: string;
	parentSessionFile: string;
	parentSessionId: string;
}

interface IntegrationJournal {
	version: 1;
	id: string;
	childSessionFile: string;
	childSessionId: string;
	parentSessionFile: string;
	parentSessionId: string;
	mergeMode: "patch" | "branch";
	phase: "applying" | "merged" | "partial" | "failed";
	startedAt: string;
	error?: string;
}

interface IntegrationTombstone {
	version: 1;
	id: string;
	childSessionFile: string;
	childSessionId: string;
	parentSessionFile: string;
	parentSessionId: string;
	mergedAt: string;
}

const JOURNAL_FILE = ".omp-integration-journal.json";

/**
 * Handle `/integrate` for one agent. Only called when the message matched the
 * reserved signal, so every outcome is an explicit answer.
 */
export async function integrateDeferredAgent(request: DeferredIntegrationRequest): Promise<DeferredIntegrationOutcome> {
	try {
		return await integrateDeferredAgentUnchecked(request);
	} catch (error) {
		const message = error instanceof Error ? error.message : String(error);
		return {
			text: `Cannot integrate "${request.target}": ${message}\nWorkspace, journal and artifacts were preserved; no cleanup was attempted.`,
			isError: true,
		};
	}
}

async function integrateDeferredAgentUnchecked(
	request: DeferredIntegrationRequest,
): Promise<DeferredIntegrationOutcome> {
	const { registry, senderId, target } = request;
	const parentSessionFile = request.parentSessionFile ?? undefined;
	const parentSessionId = request.parentSessionId ?? undefined;
	if (!parentSessionFile || !parentSessionId) {
		return {
			text: `Cannot integrate "${target}": the caller has no persisted parent session identity; deferred handoff requires both a session file and a session id.`,
			isError: true,
		};
	}
	// Resolve the caller's root roster even when an in-memory ref with the same
	// id exists: refs belonging to another root session must not authorize.
	const callerRoot = await ensurePersistedRoster(registry, parentSessionFile);
	if (!callerRoot) {
		return {
			text: `Cannot integrate "${target}": the caller transcript ${parentSessionFile} does not resolve to a session root.`,
			isError: true,
		};
	}
	let ref = registry.get(target);
	if (!ref?.sessionFile || !sessionFileBelongsToRoot(ref.sessionFile, callerRoot)) {
		return {
			text: `Cannot integrate "${target}": it does not belong to the caller's session root.`,
			isError: true,
		};
	}
	if (ref.kind === "advisor") {
		return {
			text: `Cannot integrate "${target}": advisor transcripts own no workspace.`,
			isError: true,
		};
	}
	if (ref.parentId !== senderId) {
		return {
			text: `Cannot integrate "${target}": only its parent agent may send ${DEFERRED_INTEGRATION_SIGNAL}.`,
			isError: true,
		};
	}
	if (!ref.sessionFile) {
		return {
			text: `Cannot integrate "${target}": it has no transcript to bind its workspace to.`,
			isError: true,
		};
	}

	const lifecycle = AgentLifecycleManager.global();
	const releaseReservation = await lifecycle.reserve(ref.id, ref);
	const reservedRef = ref;
	try {
		// Anything below may await; the reservation fences revival/release for
		// this exact ref, and every re-read starts from the current registry ref.
		ref = registry.get(target);
		if (ref !== reservedRef) throw new Error("the agent generation changed while its handoff hold was acquired");
		if (!ref || !sessionFileBelongsToRoot(ref.sessionFile ?? "", callerRoot)) {
			throw new Error("the agent was replaced while its handoff hold was acquired");
		}
		if (ref.history?.deferredIntegration === "merged") {
			return {
				text: `Agent "${target}" was already integrated; nothing is left to apply.`,
				isError: false,
			};
		}
		if (ref.status === "running") {
			return {
				text: `Cannot integrate "${target}" while it is running. Wait for it to finish (or park it) and signal again.`,
				isError: true,
			};
		}
		if (ref.status === "idle" && ref.session) {
			try {
				await lifecycle.park(ref.id);
			} catch (error) {
				throw new Error(
					`parking its live session failed: ${error instanceof Error ? error.message : String(error)}`,
				);
			}
			ref = registry.get(target);
			if (!ref || ref !== reservedRef) {
				throw new Error("the agent generation changed while its live session parked");
			}
			if (!ref || ref.session || ref.status === "running") {
				throw new Error("the live session did not park cleanly; retry once it is parked");
			}
		}
		if (ref.status !== "parked" && ref.status !== "aborted") {
			throw new Error(`the agent is ${ref.status}; only a parked or aborted agent can be integrated`);
		}
		if (!ref.sessionFile) throw new Error("the agent has no transcript to bind its workspace to");
		const resolved = await resolveHandoff(ref, parentSessionFile, parentSessionId);
		if ("outcome" in resolved) return resolved.outcome;
		if (registry.get(target) !== reservedRef) {
			throw new Error("the agent generation changed before the handoff could mutate the workspace");
		}

		return await withIsolationLease(resolved.state.slot, async () => {
			const owner = await readIsolationOwner(resolved.state.dir);
			if (owner && owner.pid !== process.pid && (await hasLiveIsolationOwner(resolved.state.dir))) {
				throw new Error(`isolation workspace is owned by live omp process ${owner.pid}; retry after it exits`);
			}
			await writeIsolationOwner(resolved.state.dir, ref.id);
			return runIntegration(ref, resolved.state, resolved.identity, resolved.artifactsDir);
		});
	} finally {
		releaseReservation();
	}
}

async function resolveHandoff(
	ref: AgentRef,
	parentSessionFile: string,
	parentSessionId: string,
): Promise<
	| { state: LoadedIsolationState; identity: IntegrationIdentity; artifactsDir: string }
	| { outcome: DeferredIntegrationOutcome }
> {
	const sessionFile = ref.sessionFile;
	if (!sessionFile) return { outcome: { text: `Cannot integrate "${ref.id}": no transcript.`, isError: true } };
	const peek = await SessionManager.peekSessionInit(sessionFile);
	if (!peek?.init || peek.init.isolated !== true) {
		return {
			outcome: { text: `Cannot integrate "${ref.id}": it did not run isolated.`, isError: true },
		};
	}
	const childSessionId = await readSessionHeaderId(sessionFile);
	if (!childSessionId) {
		return {
			outcome: {
				text: `Cannot integrate "${ref.id}": its transcript has no session-header id.`,
				isError: true,
			},
		};
	}
	const artifactsDir = path.dirname(sessionFile);

	// Durable no-op after success: the tombstone outlives the workspace, so a
	// repeat signal in a new process does not need the descriptor.
	const tombstone = await readTombstone(artifactsDir, ref.id);
	if (
		tombstone &&
		(await tombstoneMatches(tombstone, {
			id: ref.id,
			childSessionFile: sessionFile,
			childSessionId,
		})) &&
		tombstone.parentSessionFile === (await canonicalFile(parentSessionFile)) &&
		tombstone.parentSessionId === parentSessionId
	) {
		// Completed handoff: answer no-op, never teardown. The workspace may have
		// been revived and edited after completion; deleting it here would destroy
		// that work. Teardown is a separate, explicitly owned decision.
		return {
			outcome: {
				text: `Agent "${ref.id}" was already integrated; nothing is left to apply. A completed handoff is final: later workspace edits are not merged and the workspace is not removed.`,
				isError: false,
			},
		};
	}

	const resolved = await resolveIsolationWorkspace(sessionFile, ref.id, peek.cwd);
	if ("refusal" in resolved) {
		return {
			outcome: {
				text: `Cannot integrate "${ref.id}": ${resolved.refusal}. If it was already integrated, nothing is left to apply.`,
				isError: true,
			},
		};
	}
	const state = resolved.state;
	const identity = {
		childSessionFile: await canonicalFile(sessionFile),
		childSessionId,
		parentSessionFile: await canonicalFile(parentSessionFile),
		parentSessionId,
	};
	const missing = await descriptorIdentityProblem(state, identity);
	if (missing) {
		return {
			outcome: {
				text: `Cannot integrate "${ref.id}": ${missing}. The workspace, descriptor and artifacts were preserved; use \`omp worktree release\` only after manual review.`,
				isError: true,
			},
		};
	}

	const journal = await readJournal(state.dir, state);
	if (journal && (journal.phase === "applying" || journal.phase === "partial" || journal.phase === "failed")) {
		return {
			outcome: {
				text: `Cannot integrate "${ref.id}": a previous handoff is marked "${journal.phase}"${
					journal.error ? ` (${journal.error})` : ""
				}. The parent may be partially changed. Workspace, journal and artifacts were preserved; inspect them before retrying.`,
				isError: true,
			},
		};
	}
	return { state, identity, artifactsDir };
}

async function runIntegration(
	ref: AgentRef,
	state: LoadedIsolationState,
	identity: IntegrationIdentity,
	artifactsDir: string,
): Promise<DeferredIntegrationOutcome> {
	const id = ref.id;
	const handle = isolationHandle(state);
	const description = ref.displayName || id;
	const mergeMode = state.mergeMode ?? "patch";
	// Re-read and re-bind under the lease: the descriptor path cannot be swapped
	// between authorization and mutation, and a retained rename is followed.
	const currentRead = await readIsolationState(state.dir, state.id);
	if ("error" in currentRead) throw new Error(currentRead.detail);
	const bound = await bindIsolationState(currentRead.state, identity.childSessionFile);
	if ("refusal" in bound) throw new Error(`the workspace no longer binds to the child transcript: ${bound.refusal}`);
	const current = currentRead.state;
	const changed = await descriptorIdentityProblem(current, identity);
	if (changed) throw new Error(`session identity changed under the lease: ${changed}`);
	const journal = await readJournal(current.dir, current);
	if (journal?.phase === "merged") {
		await writeTombstone(artifactsDir, id, identity);
		return completeHandoff(current, handle, identity, artifactsDir, true, false, current.mergeMode ?? "patch");
	}
	if (journal) {
		throw new Error(
			`a previous handoff is marked "${journal.phase}"${journal.error ? ` (${journal.error})` : ""}; refusing to replay it`,
		);
	}

	// Capture the latest delta before any parent mutation. These files are the
	// recovery record if the stock merge fails or is partial.
	const capture = await writeIsolationPatch(handle.mergedDir, current.baseline, artifactsDir, id);
	const nested = await selectApplicableNestedPatches(current.repoRoot, capture.artifacts.nestedPatches);
	await assertRootPatchIsUnambiguous(current.repoRoot, capture.artifacts.patchPath, capture.artifacts.hasRootChanges);

	await writeJournal(current.dir, {
		version: 1,
		id,
		childSessionFile: identity.childSessionFile,
		childSessionId: identity.childSessionId,
		parentSessionFile: identity.parentSessionFile,
		parentSessionId: identity.parentSessionId,
		mergeMode,
		phase: "applying",
		startedAt: new Date().toISOString(),
	});

	const branchFields =
		mergeMode === "branch"
			? await commitToBranch(handle.mergedDir, current.baseline, id, description).then(commit => ({
					...(commit?.branchName === undefined ? {} : { branchName: commit.branchName }),
					...(commit?.baseSha === undefined ? {} : { branchBaseSha: commit.baseSha }),
				}))
			: {};
	const result = integrationResult({
		id,
		description,
		patchPath: capture.artifacts.patchPath,
		hasRootChanges: capture.artifacts.hasRootChanges,
		nestedPatches: nested.applicable,
		nestedPatchPaths: capture.artifacts.nestedPatchPaths,
		...branchFields,
	});

	const outcome = await mergeIsolatedChanges({ result, repoRoot: current.repoRoot, mergeMode });
	if (outcome.changesApplied !== true) {
		await writeJournal(current.dir, {
			...journalBase(identity, id, mergeMode),
			phase: "failed",
			startedAt: new Date().toISOString(),
			error: stripNotification(outcome.summary),
		});
		return {
			text: `Cannot integrate "${id}": ${stripNotification(outcome.summary)}\nWorkspace, artifacts and journal kept at \`${current.dir}\`. The stock merge reported no applied changes; inspect the parent before retrying.`,
			isError: true,
		};
	}

	if (nested.applicable.length > 0) {
		let warnings: string;
		try {
			warnings = (await applyNestedPatches(current.repoRoot, nested.applicable)).join("\n");
		} catch (error) {
			const message = error instanceof Error ? error.message : String(error);
			await writeJournal(current.dir, {
				...journalBase(identity, id, mergeMode),
				phase: "partial",
				startedAt: new Date().toISOString(),
				error: message,
			});
			return {
				text: `Incomplete integrate "${id}": nested repository apply failed: ${message}\nThe root merge may already have landed. Workspace, artifacts and journal kept at \`${current.dir}\`; inspect every nested repo before retrying.`,
				isError: true,
			};
		}
		if (warnings.trim().length > 0) {
			await writeJournal(current.dir, {
				...journalBase(identity, id, mergeMode),
				phase: "partial",
				startedAt: new Date().toISOString(),
				error: stripNotification(warnings),
			});
			return {
				text: `Incomplete integrate "${id}": ${stripNotification(warnings)}\nThe root merge landed, but nested repositories reported warnings or failures. Workspace, artifacts and journal kept at \`${current.dir}\`; inspect every nested repo before retrying.`,
				isError: true,
			};
		}
	}

	await writeTombstone(artifactsDir, id, identity);
	return completeHandoff(
		current,
		handle,
		identity,
		artifactsDir,
		outcome.hadAnyChanges,
		nested.alreadyApplied.length > 0,
		mergeMode,
	);
}

/** Write the durable completion record, then tear the workspace down. */
async function completeHandoff(
	state: LoadedIsolationState,
	handle: IsolationHandle,
	identity: IntegrationIdentity,
	artifactsDir: string,
	hadChanges: boolean,
	skippedNested: boolean,
	mergeMode: "patch" | "branch" = "patch",
): Promise<DeferredIntegrationOutcome> {
	const id = state.id;
	await writeJournal(state.dir, {
		...journalBase(identity, id, mergeMode),
		phase: "merged",
		startedAt: new Date().toISOString(),
	});
	const warnings = skippedNested ? "\nAlready-applied nested patches were skipped." : "";
	const outcomeLabel = hadChanges
		? mergeMode === "branch"
			? "branch merged"
			: "patch applied"
		: "no changes to apply";
	try {
		await cleanupIsolation(handle);
	} catch (error) {
		logger.warn("deferred integration cleanup failed", {
			id,
			error: error instanceof Error ? error.message : String(error),
		});
		return {
			text: `Integrated "${id}" into \`${state.repoRoot}\` (${outcomeLabel}). Workspace cleanup failed; it stays at \`${state.dir}\`.${warnings}`,
			isError: false,
		};
	}
	const workspaceGone = !(await fs.stat(state.dir).catch(() => undefined));
	if (!workspaceGone) {
		return {
			text: `Integrated "${id}" into \`${state.repoRoot}\` (${outcomeLabel}). The workspace could not be removed and stays at \`${state.dir}\`.${warnings}`,
			isError: false,
		};
	}
	AgentRegistry.global().setHistory(id, { deferredIntegration: "merged" });
	const artifactsNote = artifactsDir ? ` Artifacts remain in \`${artifactsDir}\`.` : "";
	return {
		text: `Integrated "${id}" into \`${state.repoRoot}\` (${outcomeLabel}). Workspace released.${artifactsNote}${warnings}`,
		isError: false,
	};
}

function journalBase(identity: IntegrationIdentity, id: string, mergeMode: "patch" | "branch") {
	return { version: 1 as const, id, ...identity, mergeMode };
}

async function descriptorIdentityProblem(
	state: LoadedIsolationState,
	identity: IntegrationIdentity,
): Promise<string | undefined> {
	if (!state.deferred) return "it was not spawned with deferred integration";
	if (!state.deferredReady) return "its run did not finish successfully with deferred integration";
	const { childSessionId, parentSessionFile, parentSessionId } = state;
	if (!childSessionId || !parentSessionFile || !parentSessionId) {
		return "its descriptor predates strict session binding";
	}
	if (childSessionId !== identity.childSessionId) return "the child session id no longer matches the descriptor";
	if ((await canonicalFile(parentSessionFile)) !== identity.parentSessionFile) {
		return "the parent session file no longer matches the descriptor";
	}
	if (parentSessionId !== identity.parentSessionId) return "the parent session id no longer matches the descriptor";
	return undefined;
}

function integrationResult(input: {
	id: string;
	description: string;
	patchPath: string;
	hasRootChanges: boolean;
	nestedPatches: NestedRepoPatch[];
	nestedPatchPaths: string[];
	branchName?: string;
	branchBaseSha?: string;
}): SingleResult {
	return {
		index: 0,
		id: input.id,
		agent: input.description,
		agentSource: "user",
		task: "",
		description: input.description,
		exitCode: 0,
		output: "",
		stderr: "",
		truncated: false,
		durationMs: 0,
		tokens: 0,
		requests: 0,
		patchPath: input.patchPath,
		hasRootChanges: input.hasRootChanges,
		nestedPatches: input.nestedPatches,
		nestedPatchPaths: input.nestedPatchPaths,
		...(input.branchName === undefined ? {} : { branchName: input.branchName }),
		...(input.branchBaseSha === undefined ? {} : { branchBaseSha: input.branchBaseSha }),
	};
}

/**
 * Refuse before mutation unless the root patch has exactly one defensible
 * application: clean forward, or already applied. Both/neither means the
 * stock merge could duplicate or conflict, so the handoff fails closed.
 */
async function assertRootPatchIsUnambiguous(
	repoRoot: string,
	patchPath: string,
	hasRootChanges: boolean,
): Promise<void> {
	if (!hasRootChanges) return;
	const patch = await Bun.file(patchPath).text();
	if (!patch.trim()) return;
	const repo = vcs.requireGit(repoRoot);
	const [forward, reverse] = await Promise.all([
		repo.canApplyPatch(patch, {}).catch(() => false),
		repo.canApplyPatch(patch, { reverse: true }).catch(() => false),
	]);
	if (!forward && !reverse) throw new Error("the root patch applies neither forward nor in reverse");
	if (forward && reverse)
		throw new Error("the root patch is ambiguous (applies both ways); parent may already contain it");
}

/**
 * Split nested patches into forward-applicable and already-applied sets.
 * Anything else is a conflict; the handoff is refused before the stock merge.
 */
async function selectApplicableNestedPatches(
	repoRoot: string,
	patches: readonly NestedRepoPatch[],
): Promise<{ applicable: NestedRepoPatch[]; alreadyApplied: string[] }> {
	const applicable: NestedRepoPatch[] = [];
	const alreadyApplied: string[] = [];
	const rootPrefix = `${path.resolve(repoRoot)}${path.sep}`;
	for (const patch of patches) {
		if (!patch.patch.trim()) continue;
		const nestedDir = path.resolve(repoRoot, patch.relativePath);
		if (nestedDir !== path.resolve(repoRoot) && !nestedDir.startsWith(rootPrefix)) {
			throw new Error(`nested repository path escapes the parent repo: ${patch.relativePath}`);
		}
		try {
			await fs.access(path.join(nestedDir, ".git"));
		} catch {
			throw new Error(`nested repository \`${patch.relativePath}\` is missing`);
		}
		const repo = vcs.requireGit(nestedDir);
		const [forward, reverse] = await Promise.all([
			repo.canApplyPatch(patch.patch, {}).catch(() => false),
			repo.canApplyPatch(patch.patch, { reverse: true }).catch(() => false),
		]);
		if (forward && reverse) {
			throw new Error(`nested repository patch is ambiguous in \`${patch.relativePath}\``);
		}
		if (forward) applicable.push(patch);
		else if (reverse) alreadyApplied.push(patch.relativePath);
		else throw new Error(`nested repository patch conflicts in \`${patch.relativePath}\``);
	}
	return { applicable, alreadyApplied };
}

function journalPath(dir: string): string {
	return path.join(dir, JOURNAL_FILE);
}

async function readJournal(dir: string, state: LoadedIsolationState): Promise<IntegrationJournal | undefined> {
	const file = journalPath(dir);
	if (!(await Bun.file(file).exists())) return undefined;
	try {
		const parsed = (await Bun.file(file).json()) as IntegrationJournal;
		if (
			parsed?.version !== 1 ||
			parsed.id !== state.id ||
			parsed.childSessionId !== state.childSessionId ||
			parsed.parentSessionId !== state.parentSessionId ||
			(await canonicalFile(parsed.childSessionFile)) !== (await canonicalFile(state.sessionFile ?? "")) ||
			(await canonicalFile(parsed.parentSessionFile)) !== (await canonicalFile(state.parentSessionFile ?? ""))
		) {
			throw new Error("integration journal identity does not match the descriptor");
		}
		return parsed;
	} catch (error) {
		if (error instanceof Error && error.message.includes("identity does not match")) throw error;
		if ((error as NodeJS.ErrnoException)?.code === "ENOENT") return undefined;
		throw new Error(`integration journal at ${file} is unreadable; refusing to guess the previous outcome`);
	}
}

async function writeJournal(dir: string, journal: IntegrationJournal): Promise<void> {
	await writeJsonAtomic(journalPath(dir), journal);
}

async function writeTombstone(artifactsDir: string, id: string, identity: IntegrationIdentity): Promise<void> {
	const tombstone: IntegrationTombstone = {
		version: 1,
		id,
		childSessionFile: identity.childSessionFile,
		childSessionId: identity.childSessionId,
		parentSessionFile: identity.parentSessionFile,
		parentSessionId: identity.parentSessionId,
		mergedAt: new Date().toISOString(),
	};
	await writeJsonAtomic(path.join(artifactsDir, `${id}.integration.json`), tombstone);
}

async function readTombstone(artifactsDir: string, id: string): Promise<IntegrationTombstone | undefined> {
	try {
		const parsed = (await Bun.file(path.join(artifactsDir, `${id}.integration.json`)).json()) as IntegrationTombstone;
		return parsed?.version === 1 && parsed.id === id ? parsed : undefined;
	} catch {
		return undefined;
	}
}

async function tombstoneMatches(
	tombstone: IntegrationTombstone,
	expected: { id: string; childSessionFile: string; childSessionId: string },
): Promise<boolean> {
	return (
		tombstone.id === expected.id &&
		tombstone.childSessionId === expected.childSessionId &&
		tombstone.childSessionFile === (await canonicalFile(expected.childSessionFile))
	);
}

async function writeJsonAtomic(file: string, value: unknown): Promise<void> {
	const temp = `${file}.${process.pid}.${Math.random().toString(36).slice(2)}.tmp`;
	await Bun.write(temp, JSON.stringify(value));
	await fs.rename(temp, file);
}

async function canonicalFile(file: string): Promise<string> {
	try {
		return await fs.realpath(file);
	} catch {
		return path.resolve(file);
	}
}

/** Drop the model-facing wrapper so a failure can be embedded in one line. */
function stripNotification(summary: string): string {
	return summary
		.replace(/<\/?system-notification>/g, "")
		.replace(/\s+/g, " ")
		.trim();
}
