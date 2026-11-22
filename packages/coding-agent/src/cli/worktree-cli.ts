/** CLI presentation and explicit-path Git worktree creation. */
import * as fs from "node:fs/promises";
import * as path from "node:path";
import * as vcs from "@oh-my-pi/pi-natives/vcs";
import { isEnoent } from "@oh-my-pi/pi-utils";
import chalk from "@oh-my-pi/pi-utils/chalk";
import { Settings } from "../config/settings";
import { claimIsolationOwner } from "../task/isolation-ownership";
import { releaseIsolationWorkspace } from "../task/isolation-runner";
import { cfgIsolationBackend, cfgWorktreeClone } from "../task/settings";
import {
	bindIsolationState,
	bindPersistedReleaseWorkspace,
	formatIsolationBackend,
	parseIsolationBackend,
	readIsolationState,
} from "../task/worktree";
import type { ManagedWorktree } from "../worktree/backend";
import { managedWorktrees } from "../worktree/worktrees";
export { stopRetainedMount } from "../worktree/git-managed";

export interface AddWorktreeOptions {
	cwd?: string;
	path: string;
	commit?: string;
	branch?: string;
	forceBranch?: string;
	detach: boolean;
	quiet: boolean;
}

export interface ListWorktreesOptions {
	json: boolean;
}

export interface ClearWorktreesOptions {
	/** Remove all managed entries, including live PR-checkout worktrees; retained task-isolation workspaces are always preserved. */
	all: boolean;
	/** Print what would be removed without touching the filesystem. */
	dryRun: boolean;
	json: boolean;
}

export interface ReleaseWorktreeOptions {
	/** Absolute or cwd-relative path of the task-isolation sandbox to release. */
	path: string;
	json: boolean;
}

export async function addWorktree(options: AddWorktreeOptions): Promise<void> {
	if (options.branch && options.forceBranch) {
		throw new Error("fatal: options '-b' and '-B' cannot be used together");
	}
	const cwd = path.resolve(options.cwd ?? process.cwd());
	const repository = vcs.requireGit(cwd);
	const worktreePath = path.resolve(cwd, options.path);
	try {
		const stat = await fs.stat(worktreePath);
		const nonEmpty = !stat.isDirectory() || (await fs.readdir(worktreePath)).length > 0;
		if (nonEmpty) throw new Error(`fatal: '${options.path}' already exists`);
	} catch (error) {
		if (!isEnoent(error)) throw error;
	}

	const settings = await Settings.init({ cwd });
	let ref: string;
	let detach = false;
	let createdBranch: string | undefined;
	if (options.branch || options.forceBranch) {
		const branch = options.branch ?? options.forceBranch;
		if (!branch) throw new Error("branch name is required");
		await repository.createBranch(branch, options.commit ?? "HEAD", Boolean(options.forceBranch));
		ref = branch;
		createdBranch = branch;
	} else if (options.detach) {
		ref = options.commit ?? "HEAD";
		detach = true;
	} else if (options.commit) {
		ref = options.commit;
		detach = !(await repository.refExists(`refs/heads/${options.commit}`));
	} else {
		const branch = path.basename(worktreePath);
		if (!(await repository.refExists(`refs/heads/${branch}`))) {
			await repository.createBranch(branch, "HEAD", false);
			createdBranch = branch;
		}
		ref = branch;
	}

	const commit = await repository.commitDetails(ref);
	const shortSha = commit.sha.slice(0, 7);
	const subject = commit.message.split("\n", 1)[0];
	if (!options.quiet) {
		const preparation = createdBranch
			? `new branch '${createdBranch}'`
			: detach
				? `detached HEAD ${shortSha}`
				: `checking out '${ref}'`;
		console.log(`Preparing worktree (${preparation})`);
	}
	const result = await repository.worktreeAdd(worktreePath, ref, {
		detach,
		clone: cfgWorktreeClone.get(settings),
		backend: parseIsolationBackend(cfgIsolationBackend.get(settings)),
	});
	if (!options.quiet) {
		console.log(`HEAD is now at ${shortSha} ${subject}`);
		if (result.clonedWith != null) {
			console.log(
				`Cloned from ${repository.info().repoRoot} via ${formatIsolationBackend(result.clonedWith)}, leaving out .git, other worktrees, and untracked .env files`,
			);
		}
	}
	if (result.cloneError) {
		console.error(chalk.dim(`warning: worktree clone fell back to plain checkout: ${result.cloneError}`));
	}
	const hook = await vcs.runPostCheckoutHook(worktreePath, commit.sha);
	process.stderr.write(hook.stdout);
	process.stderr.write(hook.stderr);
	if (hook.exitCode !== 0) process.exitCode = hook.exitCode;
}

export async function listWorktrees(options: ListWorktreesOptions): Promise<void> {
	const entries = await managedWorktrees();
	if (options.json) {
		console.log(JSON.stringify(entries, null, 2));
		return;
	}
	if (entries.length === 0) {
		console.log(chalk.dim("No agent-managed worktrees found."));
		return;
	}
	let live = 0;
	let orphaned = 0;
	let retained = 0;
	for (const entry of entries) {
		const tag = entry.retained
			? chalk.cyan("retained")
			: entry.orphanReason
				? chalk.yellow("orphaned")
				: chalk.green("live    ");
		const detail = formatEntryDetail(entry);
		console.log(`${tag}  ${entry.path}`);
		if (detail) console.log(`          ${chalk.dim(detail)}`);
		if (entry.retained) retained += 1;
		else if (entry.orphanReason) orphaned += 1;
		else live += 1;
	}
	console.log(chalk.dim(`\n${live} live · ${retained} retained · ${orphaned} orphaned · ${entries.length} total`));
}

export async function clearWorktrees(options: ClearWorktreesOptions): Promise<{ removed: number; failed: number }> {
	const entries = await managedWorktrees();
	// A live owner is never removed, under any flag: explicit release (or an
	// agent revive + release) is what ends a task-isolation workspace, and
	// `--all` is a GC for leftovers, not a force-kill. A descriptor means the
	// sandbox still holds uncaptured work, so `clear` — including `--all` —
	// always preserves it; only the explicit release path may capture and then
	// tear it down.
	const targets = options.all
		? entries.filter(entry => !(entry.kind === "task-isolation" && (entry.ownerAlive || entry.retained)))
		: entries.filter(entry => entry.orphanReason !== undefined && entry.retained !== true);
	const keptLive = entries.filter(entry =>
		options.all ? entry.kind === "task-isolation" && (entry.ownerAlive || entry.retained) : entry.retained === true,
	);

	if (targets.length === 0) {
		if (options.json) {
			console.log(JSON.stringify({ removed: 0, kept: entries.length, keptLive: keptLive.map(entry => entry.path) }));
		} else {
			console.log(chalk.dim(options.all ? "No worktrees to remove." : "No orphaned worktrees to remove."));
			for (const entry of keptLive) console.log(`${chalk.dim("kept  ")}  ${entry.path}`);
		}
		return { removed: 0, failed: 0 };
	}

	if (options.dryRun) {
		if (options.json) {
			console.log(
				JSON.stringify({ wouldRemove: targets.map(t => t.path), keptLive: keptLive.map(e => e.path) }, null, 2),
			);
		} else {
			for (const target of targets) {
				console.log(`${chalk.yellow("would remove")}  ${target.path}`);
			}
			for (const entry of keptLive) console.log(`${chalk.dim("kept  ")}  ${entry.path}`);
			console.log(chalk.dim(`\n${targets.length} dir${targets.length === 1 ? "" : "s"} would be removed.`));
		}
		return { removed: 0, failed: 0 };
	}

	const results: { path: string; ok: boolean; error?: string }[] = [];
	for (const target of targets) {
		try {
			// Re-check retained descriptor under the lease path: state may appear after classification.
			if (target.kind === "task-isolation") {
				const read = await readIsolationState(target.path);
				if (!("error" in read)) {
					results.push({
						path: target.path,
						ok: false,
						error: "retained task-isolation workspace; use `omp worktree release`",
					});
					continue;
				}
			}
			await target.remove();
			results.push({ path: target.path, ok: true });
		} catch (err) {
			results.push({ path: target.path, ok: false, error: err instanceof Error ? err.message : String(err) });
		}
	}

	const succeeded = results.filter(r => r.ok).length;
	const failed = results.length - succeeded;

	if (options.json) {
		console.log(
			JSON.stringify({ removed: succeeded, failed, keptLive: keptLive.map(e => e.path), results }, null, 2),
		);
		return { removed: succeeded, failed };
	}

	for (const result of results) {
		if (result.ok) {
			console.log(`${chalk.green("removed")}  ${result.path}`);
		} else {
			console.log(`${chalk.red("failed ")}  ${result.path}`);
			if (result.error) console.log(`          ${chalk.dim(result.error)}`);
		}
	}
	for (const entry of keptLive) console.log(`${chalk.dim("kept  ")}  ${entry.path}`);
	console.log(chalk.dim(`\n${succeeded} removed${failed > 0 ? ` · ${chalk.red(`${failed} failed`)}` : ""}`));
	return { removed: succeeded, failed };
}

/**
 * Capture a retained task-isolation sandbox and tear it down. Unlike
 * `clear`, this is the only path that may delete a descriptor-bearing workspace.
 */
export async function releaseWorktree(options: ReleaseWorktreeOptions): Promise<void> {
	const dir = path.resolve(options.path);
	const read = await readIsolationState(dir);
	if ("error" in read) {
		throw new Error(`Cannot release ${dir}: ${read.detail ?? read.error}`);
	}
	// The CLI names the workspace, not a transcript: the descriptor's own
	// `sessionFile` is not independent evidence, so prove the session header and
	// retention pointer place this transcript in the selected workspace before
	// claim/capture, and take the artifact dir from that verified transcript.
	const identity = await bindPersistedReleaseWorkspace(read.state, {
		requirePointer: !read.state.childSessionId,
	});
	if ("refusal" in identity) throw new Error(`Cannot release ${dir}: ${identity.refusal}`);
	const binding = await bindIsolationState(read.state, identity.transcriptFile, {
		allowLegacyGeneration: true,
	});
	if ("refusal" in binding) throw new Error(`Cannot release ${dir}: ${binding.refusal}`);
	const artifactsDir = identity.artifactsDir;
	// Claim under the slot lease: a live foreign owner blocks; a dead/own marker
	// is reclaimed so release's own-process check passes.
	await claimIsolationOwner(read.state.dir, read.state.id, read.state.slot);
	await releaseIsolationWorkspace(read.state, {
		artifactsDir,
		description: "omp worktree release",
		expectedSessionFile: identity.transcriptFile,
		allowLegacyGeneration: true,
		allowHeaderGeneration: true,
	});
	if (options.json) {
		console.log(JSON.stringify({ released: dir, artifactsDir }));
	} else {
		console.log(`${chalk.green("released")}  ${dir}`);
		console.log(`          ${chalk.dim(`artifacts in ${artifactsDir}`)}`);
	}
}

function formatEntryDetail(entry: ManagedWorktree): string {
	const parts: string[] = [];
	if (entry.kind === "pr-checkout") {
		const repo = entry.parentRepo ? path.basename(entry.parentRepo) : "unknown repo";
		const branch = entry.branch ?? "unknown branch";
		parts.push(`${repo} · ${branch}`);
	} else if (entry.kind === "ovnfs") {
		parts.push(`ovnfs · ${entry.parentRepo ?? "unknown source"}${entry.branch ? ` · ${entry.branch}` : ""}`);
	} else if (entry.kind === "task-isolation") {
		parts.push(
			entry.retained
				? "task-isolation sandbox (retained; use `omp worktree release`)"
				: entry.ownerAlive
					? "task-isolation sandbox (live owner)"
					: "task-isolation sandbox",
		);
	} else if (entry.kind === "empty") {
		parts.push("legacy project shell");
	} else {
		parts.push("unrecognized contents");
	}
	if (entry.orphanReason) parts.push(entry.orphanReason);
	return parts.join(" — ");
}
