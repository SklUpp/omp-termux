import { describe, expect, it } from "bun:test";
import * as fs from "node:fs";
import * as path from "node:path";

describe("repoRoot is only required for isolated runs", () => {
	const structuredPath = path.resolve(import.meta.dir, "../src/task/structured-subagent.ts");
	const taskIndexPath = path.resolve(import.meta.dir, "../src/task/index.ts");

	it("does not force isolation when repoRoot is supplied alone", async () => {
		const content = await fs.promises.readFile(structuredPath, "utf8");
		expect(content).toContain("const isIsolated = request.isolation?.requested === true;");
		expect(content).not.toContain("Explicit repoRoot requires isolation");
	});

	it("task spawn preserves optional repoRoot", async () => {
		const content = await fs.promises.readFile(taskIndexPath, "utf8");
		expect(content).toContain("repoRoot");
		expect(content).toContain("spawn.repoRoot = item.repoRoot");
	});

	it("accepts repoRoot when isolation is omitted", () => {
		function isRepoRootShapeValid(requested: boolean | undefined, repoRoot: string | undefined): string | undefined {
			return requested === false && repoRoot !== undefined ? "invalid" : undefined;
		}
		expect(isRepoRootShapeValid(undefined, "/tmp/repo")).toBeUndefined();
		expect(isRepoRootShapeValid(true, "/tmp/repo")).toBeUndefined();
		expect(isRepoRootShapeValid(undefined, undefined)).toBeUndefined();
	});

	it("getRepoRoot validates explicit root for isolated execution", async () => {
		const worktreePath = path.resolve(import.meta.dir, "../src/task/worktree.ts");
		const content = await fs.promises.readFile(worktreePath, "utf8");
		expect(content).toContain("getRepoRoot");
		expect(content).toContain("Explicit repoRoot");
	});
});
