// Helper to bundle and execute TS scripts on Termux Android arm64
// where pi-natives binary addon is not compiled for Android.
import { build } from "bun";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { tmpdir } from "node:os";

const targetScript = process.argv[2];
if (!targetScript) {
	console.error("Usage: bun run-ts.ts <script.ts>");
	process.exit(1);
}

const outdir = join(tmpdir(), "omp-ts-runner-build");
const mockPath = join(import.meta.dir, "native-mock.js");

const result = await build({
	entrypoints: [targetScript],
	outdir,
	target: "bun",
	external: ["omp-legacy-pi-modules"],
	plugins: [
		{
			name: "natives-mock",
			setup(build) {
				build.onResolve({ filter: /^@oh-my-pi\/pi-natives/ }, () => ({ path: mockPath }));
			}
		}
	]
});

if (!result.success) {
	console.error("Build failed:", result.logs);
	process.exit(1);
}

const scriptName = targetScript.split("/").pop()!.replace(/\.ts$/, ".js");
const bundled = join(outdir, scriptName);
const proc = spawnSync("bun", [bundled], { stdio: "inherit" });
process.exit(proc.status ?? 0);
