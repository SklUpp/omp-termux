#!/usr/bin/env bun
/**
 * Build Termux arm64 JS package without loading host (linux-x64) pi-natives.
 * Upstream gen:bundle pulls @oh-my-pi/pi-utils main export → natives → fails on CI.
 * Full arm64 package still ships: docs-embedded CLI + natives loader stubs.
 * Android .node is attached later by package-release.
 */
import * as path from "node:path";
import { gzipSync } from "node:zlib";
import { Glob } from "bun";

const repoRoot = path.resolve(import.meta.dir, "../..");
const packageDir = path.join(repoRoot, "packages/coding-agent");
const docsDir = path.join(repoRoot, "docs");
const outDir = path.join(repoRoot, "termux-build");
const bundleDir = path.join(repoRoot, "termux-bundle");

async function buildDocsPayload(): Promise<string> {
	const glob = new Glob("**/*.md");
	const files: string[] = [];
	for await (const relativePath of glob.scan(docsDir)) {
		files.push(relativePath.split(path.sep).join("/"));
	}
	files.sort();
	const bodies = await Promise.all(files.map(file => Bun.file(path.join(docsDir, file)).text()));
	const bodiesB64 = Buffer.from(gzipSync(Buffer.from(JSON.stringify(bodies)), { level: 9 })).toString(
		"base64",
	);
	return `${JSON.stringify(files)}\n${bodiesB64}`;
}

async function main(): Promise<void> {
	process.chdir(repoRoot);
	const statsDir = path.join(repoRoot, "packages/stats");
	try {
		// Static imports cannot run here: stats build scripts use cwd-relative paths
		// and must execute before CLI bundling, then reset their generated source.
		// Stats scripts use project-relative paths; execute them after entering package root.
		process.chdir(statsDir);
		await import(path.join(statsDir, "build.ts"));
		const { buildArchiveBase64 } = await import(path.join(statsDir, "scripts/generate-client-bundle.ts"));
		await Bun.write("src/embedded-client.generated.txt", await buildArchiveBase64("dist/client"));
		process.chdir(repoRoot);

		// The prebuilt CLI has no stats source tree at runtime. Generate the
		// deterministic dashboard archive before Bun.build embeds it.
		const docsPayload = await buildDocsPayload();

		await Bun.$`rm -rf ${outDir} ${bundleDir}`.quiet();
		await Bun.$`mkdir -p ${outDir} ${bundleDir}/node_modules/@oh-my-pi/pi-natives/native`.quiet();

		const output = await Bun.build({
			entrypoints: [path.join(packageDir, "src/cli.ts")],
			outdir: outDir,
			target: "bun",
			splitting: true,
			external: [
				"@oh-my-pi/pi-natives",
				"@huggingface/transformers",
				"fastembed",
				"onnxruntime-node",
				"omp-legacy-pi-modules",
			],
			define: {
				"process.env.PI_BUNDLED": JSON.stringify("true"),
				"process.env.PI_DOCS_EMBED": JSON.stringify(docsPayload),
			},
			minify: {
				whitespace: true,
				syntax: true,
				identifiers: true,
				keepNames: true,
			},
			throw: false,
		});
		if (!output.success) {
			throw new Error(`Termux CLI bundle failed:\n${output.logs.map(log => log.message).join("\n")}`);
		}

		await Bun.$`cp -R ${outDir}/. ${bundleDir}/`.quiet();
		const cliDst = path.join(bundleDir, "cli.js");
		const cli = await Bun.file(cliDst).text();
		const shebang = cli.startsWith("#!") ? cli : `#!/usr/bin/env bun\n${cli}`;
		await Bun.write(cliDst, shebang);

		const nativesPkg = path.join(repoRoot, "packages/natives/package.json");
		const nativesNative = path.join(repoRoot, "packages/natives/native");
		await Bun.$`cp ${nativesPkg} ${bundleDir}/node_modules/@oh-my-pi/pi-natives/`.quiet();
		const nativeFilesGlob = new Glob("*.js");
		for await (const name of nativeFilesGlob.scan(nativesNative)) {
			await Bun.$`cp ${path.join(nativesNative, name)} ${bundleDir}/node_modules/@oh-my-pi/pi-natives/native/`.quiet();
		}

		// Provide omp-legacy-pi-modules fallback stub so extensions never fail on import
		const legacyDir = path.join(bundleDir, "node_modules/omp-legacy-pi-modules");
		await Bun.$`mkdir -p ${legacyDir}`.quiet();
		await Bun.write(
			path.join(legacyDir, "package.json"),
			JSON.stringify({ name: "omp-legacy-pi-modules", version: "1.0.0", type: "module", main: "index.js" }, null, 2),
		);
		await Bun.write(path.join(legacyDir, "index.js"), "export const BUNDLED_PI_MODULE_LOADERS = {};\n");

		const tarPath = path.join(repoRoot, "termux-js.tar.gz");
		await Bun.$`tar -czf ${tarPath} -C ${bundleDir} .`.quiet();
		const size = (await Bun.file(tarPath).size) / 1024 / 1024;
		console.log(`Termux JS bundle: ${tarPath} (${size.toFixed(1)} MiB), docs files=${docsPayload.split("\n")[0].length > 2 ? "yes" : "no"}`);
	} finally {
		await Bun.write(path.join(statsDir, "src/embedded-client.generated.txt"), "");
	}
}

await main();
