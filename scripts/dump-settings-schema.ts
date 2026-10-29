/**
 * Dump the full upstream settings schema to the JSON snapshot the Rust CLI
 * reads (`crates/omp-cli/settings-schema.snapshot.json`).
 *
 * The snapshot is generated, never hand-edited: every type, enum value and
 * default below is resolved by *evaluating* `SETTINGS_SCHEMA`, so imported
 * constants, spreads and arithmetic defaults come out exactly as the
 * TypeScript runtime computes them.
 *
 * Usage: `bun scripts/dump-settings-schema.ts` after the workspace
 * dependencies are installed (`bun install`), like any other TypeScript task.
 * `--check` renders without writing and exits 1 when the snapshot is stale.
 *
 * Reproducible on a clean checkout: `@oh-my-pi/*` specifiers are pinned to this
 * checkout's `packages/*` by workspace links this script creates and removes
 * itself, so a cached published copy can never be snapshotted and no manual
 * untracked shims are needed. Bun caches lookups at startup, so the evaluation
 * runs in a child process started once those links exist.
 *
 * `@oh-my-pi/pi-natives` is the exception: its addon is a per-platform build
 * that loads at import time (and does not exist on Android). The schema graph
 * reaches it through `tokenizer.ts` only — module-init property reads, no calls —
 * so the shim exports the real entrypoint's names as inert placeholders while
 * any actual call or construction throws. A pre-existing installation at that
 * path is never replaced or deleted.
 *
 * Provenance records the entry file hash *and* a hash of the transitive local
 * import graph, because defaults and enum values come from constants declared
 * in other files of this checkout; external packages are not hashed (they are
 * pinned by the workspace lockfile).
 */

import { createHash } from "node:crypto";
import * as fs from "node:fs";
import * as os from "node:os";
import { builtinModules } from "node:module";
import * as path from "node:path";

const ROOT = path.resolve(import.meta.dir, "..");
const SOURCE = "packages/coding-agent/src/config/settings-schema.ts";
const OUTPUT = "crates/omp-cli/settings-schema.snapshot.json";
const NATIVES_ENTRY = "packages/natives/native/index.js";
const NATIVES_PACKAGE = "@oh-my-pi/pi-natives";
const GENERATOR = "bun scripts/dump-settings-schema.ts";
const GENERATION_ENV = "OMP_SETTINGS_SCHEMA_DUMP";
const CHECK_FLAG = "--check";

interface PackageManifest {
	name?: string;
	version?: string;
	main?: string;
	exports?: string | Record<string, unknown>;
	dependencies?: Record<string, string>;
	devDependencies?: Record<string, string>;
}

interface RawSettingDef {
	type: string;
	values?: readonly string[];
	default?: unknown;
	ui?: {
		tab: string;
		group?: string;
		label: string;
		description: string;
	};
}

interface SnapshotEntry {
	path: string;
	type: string;
	values?: string[];
	default?: unknown;
	defaultAbsent?: true;
	credential: boolean;
	ui?: { tab: string; group?: string; label: string; description: string };
}

/** `@oh-my-pi/<name>` → absolute package directory. */
function workspacePackages(): Map<string, string> {
	const packages = new Map<string, string>();
	const root = path.join(ROOT, "packages");
	for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
		if (!entry.isDirectory()) continue;
		const dir = path.join(root, entry.name);
		const manifestPath = path.join(dir, "package.json");
		if (!fs.existsSync(manifestPath)) continue;
		const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8")) as PackageManifest;
		if (manifest.name) packages.set(manifest.name, dir);
	}
	return packages;
}

const WORKSPACE = workspacePackages();

function readManifest(dir: string): PackageManifest {
	return JSON.parse(fs.readFileSync(path.join(dir, "package.json"), "utf8")) as PackageManifest;
}

/** The runtime half of a package `exports` target (`import` first, then `default`). */
function exportTarget(target: unknown): string | undefined {
	if (typeof target === "string") return target;
	if (target && typeof target === "object") {
		const record = target as Record<string, unknown>;
		const chosen = record.import ?? record.default ?? record.bun ?? record.module;
		return typeof chosen === "string" ? chosen : undefined;
	}
	return undefined;
}

/** Resolve `pkg` / `pkg/sub` inside a workspace package, wildcard exports included. */
function resolvePackageFile(dir: string, manifest: PackageManifest, subpath: string): string | undefined {
	const key = subpath ? `./${subpath}` : ".";
	const exportsField = manifest.exports;
	if (typeof exportsField === "string") {
		return key === "." ? path.resolve(dir, exportsField) : undefined;
	}
	if (exportsField) {
		const direct = exportTarget(exportsField[key]);
		if (direct) return path.resolve(dir, direct);
		let best: { prefixLength: number; target: string } | undefined;
		for (const [pattern, target] of Object.entries(exportsField)) {
			const star = pattern.indexOf("*");
			if (star < 0) continue;
			const prefix = pattern.slice(0, star);
			const suffix = pattern.slice(star + 1);
			if (key.length < prefix.length + suffix.length) continue;
			if (!key.startsWith(prefix) || !key.endsWith(suffix)) continue;
			const mapped = exportTarget(target);
			if (!mapped) continue;
			if (best && prefix.length <= best.prefixLength) continue;
			const middle = key.slice(prefix.length, key.length - suffix.length);
			best = { prefixLength: prefix.length, target: mapped.replace("*", middle) };
		}
		if (best) return path.resolve(dir, best.target);
	}
	if (key === ".") return path.resolve(dir, manifest.main ?? "src/index.ts");
	return undefined;
}

/** Names the real addon entrypoint exports, reused verbatim for the shim. */
function nativesShimSource(entry: string): string {
	const source = fs.readFileSync(entry, "utf8");
	const names = [...source.matchAll(/^export const ([A-Za-z0-9_]+)/gm)].map(match => match[1]);
	const unavailable = "native addon is unavailable during schema generation";
	// Module-init reads (e.g. `Encoding.ClaudeV3`) must not throw; any attempt to
	// call or construct the addon fails loudly instead of fabricating a value.
	return [
		"const inert = new Proxy(function inert() {}, {",
		"\tget: () => inert,",
		`\tapply: () => { throw new Error(${JSON.stringify(unavailable)}); },`,
		`\tconstruct: () => { throw new Error(${JSON.stringify(unavailable)}); },`,
		"});",
		...names.map(name => `export const ${name} = inert;`),
		"export default {};",
		"",
	].join("\n");
}

/**
 * The snapshot may only describe *this* checkout. `bun install` links the
 * workspace packages; without it Bun falls back to a published copy from its
 * cache, which would snapshot a different revision than the one being built.
 */
function assertWorkspaceResolution(): void {
	// `resolveSync` takes a file path, not a directory: anchor it on this script
	// so the lookup starts in `scripts/` and finds the links made above.
	const resolved = Bun.resolveSync("@oh-my-pi/pi-agent-core", import.meta.path);
	if (!resolved.startsWith(ROOT + path.sep)) {
		throw new Error(
			`@oh-my-pi/pi-agent-core resolves to ${resolved}, outside this checkout; ` +
				"the snapshot would describe a different revision.",
		);
	}
}

/**
 * Dev-time only: provide the workspace links `bun install` would make (and the
 * native addon shim) so generation works without installing anything. Files
 * created here are removed again afterwards; anything that already exists is
 * used as-is and never modified, and an existing shim path aborts the run.
 */
function withWorkspaceLinks(): () => void {
	const created: string[] = [];
	const remove = () => {
		for (const target of created.reverse()) {
			fs.rmSync(target, { force: true, recursive: true });
			for (let dir = path.dirname(target); dir.startsWith(ROOT); dir = path.dirname(dir)) {
				try {
					fs.rmdirSync(dir);
				} catch {
					break;
				}
			}
		}
	};
	try {
		for (const [name, dir] of WORKSPACE) {
			if (name === NATIVES_PACKAGE) continue;
			const target = path.join(ROOT, "node_modules", ...name.split("/"));
			if (fs.existsSync(target) || fs.lstatSync(target, { throwIfNoEntry: false })) continue;
			fs.mkdirSync(path.dirname(target), { recursive: true });
			fs.symlinkSync(dir, target, "dir");
			created.push(target);
		}
		linkExactDependencies(created);
		const nativesEntry = path.join(ROOT, NATIVES_ENTRY);
		for (const [name, dir] of WORKSPACE) {
			if (name === NATIVES_PACKAGE) continue;
			const manifest = readManifest(dir);
			if (!manifest.dependencies?.[NATIVES_PACKAGE] && !manifest.devDependencies?.[NATIVES_PACKAGE]) continue;
			const shimDir = path.join(dir, "node_modules", "@oh-my-pi", "pi-natives");
			if (fs.lstatSync(shimDir, { throwIfNoEntry: false })) {
				throw new Error(
					`refusing to replace existing ${path.relative(ROOT, shimDir)}; ` +
						"remove it (or generate without a non-hoisted native install).",
				);
			}
			fs.mkdirSync(shimDir, { recursive: true });
			fs.writeFileSync(
				path.join(shimDir, "package.json"),
				`${JSON.stringify(
					{ name: NATIVES_PACKAGE, type: "module", main: "./index.js", exports: { ".": "./index.js" } },
					null,
					2,
				)}\n`,
			);
			fs.writeFileSync(path.join(shimDir, "index.js"), nativesShimSource(nativesEntry));
			created.push(shimDir);
		}
	} catch (error) {
		remove();
		throw error;
	}
	return remove;
}

/**
 * Link exactly the third-party packages the schema graph imports, at the
 * versions pinned in `bun.lock`, from a tree that already has them installed.
 * Nothing is installed, nothing is guessed: an unresolvable or mismatched
 * version stops generation with the missing prerequisite spelled out.
 */
function linkExactDependencies(created: string[]): void {
	const locked = lockedVersions();
	const roots: string[] = [];
	if (process.env.OMP_SCHEMA_NODE_MODULES) roots.push(process.env.OMP_SCHEMA_NODE_MODULES);
	const prefix = process.env.PREFIX;
	if (prefix) roots.push(path.join(prefix, "lib/node_modules/@oh-my-pi/pi-coding-agent/node_modules"));
	const bunInstall = process.env.BUN_INSTALL ?? path.join(os.homedir(), ".bun");
	roots.push(path.join(bunInstall, "install/global/node_modules"));

	const missing: string[] = [];
	for (const name of externalSpecifiers(path.join(ROOT, SOURCE))) {
		const version = locked.get(name);
		if (!version) {
			missing.push(`${name} (not pinned in bun.lock)`);
			continue;
		}
		const target = path.join(ROOT, "node_modules", ...name.split("/"));
		if (fs.lstatSync(target, { throwIfNoEntry: false })) {
			const found = readPackageVersion(target);
			if (found !== version) missing.push(`${name}@${version} (found ${found ?? "no package.json"} at ${path.relative(ROOT, target)})`);
			continue;
		}
		const source = roots
			.map(root => path.join(root, ...name.split("/")))
			.find(dir => readPackageVersion(dir) === version);
		if (!source) {
			missing.push(`${name}@${version}`);
			continue;
		}
		fs.mkdirSync(path.dirname(target), { recursive: true });
		fs.symlinkSync(fs.realpathSync(source), target, "dir");
		created.push(target);
	}
	if (missing.length > 0) {
		throw new Error(
			`missing exact dependencies for the settings schema: ${missing.join(", ")}. ` +
				"Run `bun install`, or point OMP_SCHEMA_NODE_MODULES at a node_modules tree with those exact versions.",
		);
	}
}

function readPackageVersion(dir: string): string | undefined {
	try {
		const manifest = JSON.parse(fs.readFileSync(path.join(dir, "package.json"), "utf8")) as PackageManifest;
		return manifest.version;
	} catch {
		return undefined;
	}
}

/** Package versions pinned in `bun.lock` (`"name": ["name@version", ...]`). */
function lockedVersions(): Map<string, string> {
	const text = fs.readFileSync(path.join(ROOT, "bun.lock"), "utf8");
	const lock = JSON.parse(text.replace(/,(\s*[}\]])/g, "$1")) as {
		packages?: Record<string, unknown[]>;
	};
	const versions = new Map<string, string>();
	for (const [name, entry] of Object.entries(lock.packages ?? {})) {
		const resolved = entry[0];
		if (typeof resolved !== "string") continue;
		const at = resolved.lastIndexOf("@");
		if (at > 0) versions.set(name, resolved.slice(at + 1));
	}
	return versions;
}

/** Bare third-party specifiers reachable from the schema entry point. */
function externalSpecifiers(entry: string): string[] {
	const seen = new Set<string>([entry]);
	const queue = [entry];
	const bare = new Set<string>();
	while (queue.length > 0) {
		const file = queue.shift() as string;
		for (const specifier of staticSpecifiers(file)) {
			const resolved = resolveImport(file, specifier);
			if (resolved) {
				if (!seen.has(resolved)) {
					seen.add(resolved);
					queue.push(resolved);
				}
				continue;
			}
			if (specifier.startsWith(".") || specifier.startsWith("/") || isBuiltinSpecifier(specifier)) continue;
			if (specifier.startsWith("@oh-my-pi/")) continue;
			const parts = specifier.split("/");
			const name = specifier.startsWith("@") ? `${parts[0]}/${parts[1]}` : parts[0];
			// Type-only packages never need to resolve at runtime.
			if (name.startsWith("@types/")) continue;
			bare.add(name);
		}
	}
	return [...bare].sort();
}

/** Builtins never need a node_modules entry. */
const BUILTINS = new Set([...builtinModules, "bun:ffi", "bun:jsc", "bun:sqlite", "bun:test", "bun:wrap"]);

/**
 * Specifiers of the static imports/reexports Bun itself would resolve.
 * `Bun.Transpiler.scanImports` is the native parser, so strings that merely look
 * like imports (fixtures, embedded code, user snippets) are not reported.
 */
function staticSpecifiers(file: string): string[] {
	const extension = path.extname(file);
	if (![".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"].includes(extension)) return [];
	const loader = extension === ".tsx" ? "tsx" : extension === ".ts" ? "ts" : extension === ".jsx" ? "jsx" : "js";
	let transpiler = TRANSPILERS.get(loader);
	if (!transpiler) {
		transpiler = new Bun.Transpiler({ loader });
		TRANSPILERS.set(loader, transpiler);
	}
	const source = fs.readFileSync(file, "utf8");
	try {
		return transpiler.scanImports(source).map(record => record.path);
	} catch (error) {
		throw new Error(`cannot scan imports of ${path.relative(ROOT, file)}: ${(error as Error).message}`);
	}
}

const TRANSPILERS = new Map<string, Bun.Transpiler>();

/** True for builtins and non-package specifiers. */
function isBuiltinSpecifier(specifier: string): boolean {
	return (
		specifier.startsWith("node:") ||
		specifier.startsWith("bun:") ||
		specifier.startsWith("file:") ||
		BUILTINS.has(specifier) ||
		BUILTINS.has(specifier.split("/")[0])
	);
}

/** A specifier resolved to a file in this checkout, or undefined when external. */
function resolveImport(fromFile: string, specifier: string): string | undefined {
	if (specifier.startsWith(".") || specifier.startsWith("/")) {
		return probe(path.resolve(path.dirname(fromFile), specifier));
	}
	const parts = specifier.split("/");
	if (parts.length < 2 || parts[0] !== "@oh-my-pi") return undefined;
	const dir = WORKSPACE.get(`${parts[0]}/${parts[1]}`);
	if (!dir) return undefined;
	const file = resolvePackageFile(dir, readManifest(dir), parts.slice(2).join("/"));
	return file && fs.existsSync(file) ? file : undefined;
}

function probe(base: string): string | undefined {
	const candidates = [base, `${base}.ts`, `${base}.tsx`, `${base}.js`, `${base}.mjs`, `${base}.json`];
	candidates.push(path.join(base, "index.ts"), path.join(base, "index.tsx"), path.join(base, "index.js"));
	return candidates.find(candidate => fs.existsSync(candidate) && fs.statSync(candidate).isFile());
}

/** Content hash over the transitive local import graph, independent of order. */
function sourceGraph(entry: string): { files: number; sha256: string } {
	const hashes = new Map<string, string>();
	const queue = [entry];
	while (queue.length > 0) {
		const file = queue.shift() as string;
		if (hashes.has(file)) continue;
		const text = fs.readFileSync(file, "utf8");
		hashes.set(file, createHash("sha256").update(text).digest("hex"));
		for (const specifier of staticSpecifiers(file)) {
			const resolved = resolveImport(file, specifier);
			if (resolved) queue.push(resolved);
		}
	}
	const aggregate = [...hashes.entries()]
		.map(([file, hash]) => `${path.relative(ROOT, file)}\0${hash}`)
		.sort()
		.join("\n");
	return { files: hashes.size, sha256: createHash("sha256").update(aggregate).digest("hex") };
}

function gitRevision(): string {
	try {
		return Bun.spawnSync(["git", "rev-parse", "HEAD"], { cwd: ROOT }).stdout.toString().trim();
	} catch {
		return "unknown";
	}
}

async function render(): Promise<string> {
	// Dynamic on purpose: the workspace links must exist before the schema graph
	// resolves its imports.
	const schema = (await import(path.join(ROOT, SOURCE))) as {
		SETTINGS_SCHEMA: Record<string, RawSettingDef>;
		getUi: (settingPath: string) => RawSettingDef["ui"];
		isCredential: (settingPath: string) => boolean;
	};

	const settings: SnapshotEntry[] = Object.entries(schema.SETTINGS_SCHEMA).map(([settingPath, def]) => {
		const entry: SnapshotEntry = { path: settingPath, type: def.type };
		if (def.values) entry.values = [...def.values];
		// `undefined` and `null` are different answers: a setting whose default
		// is undefined is *unset* (absent), while an explicit null would be a value.
		if (def.default === undefined) entry.defaultAbsent = true;
		else entry.default = structuredClone(def.default);
		entry.credential = schema.isCredential(settingPath);
		const ui = schema.getUi(settingPath);
		if (ui) {
			entry.ui = { tab: ui.tab, label: ui.label, description: ui.description };
			if (ui.group !== undefined) entry.ui.group = ui.group;
		}
		return entry;
	});

	const sourceBytes = fs.readFileSync(path.join(ROOT, SOURCE));
	const snapshot = {
		provenance: {
			source: SOURCE,
			sourceSha256: createHash("sha256").update(sourceBytes).digest("hex"),
			importGraph: {
				...sourceGraph(path.join(ROOT, SOURCE)),
				bareDependencies: externalSpecifiers(path.join(ROOT, SOURCE)),
			},
			generator: GENERATOR,
			gitRevision: gitRevision(),
			notes: [
				"Generated by evaluating SETTINGS_SCHEMA; do not edit by hand.",
				"importGraph hashes the transitive local imports; external packages follow the lockfile.",
			],
		},
		settings,
	};
	return `${JSON.stringify(snapshot, null, "\t")}\n`;
}

/**
 * Compare a snapshot while ignoring `provenance.gitRevision`: it records the
 * commit the file was generated from, so committing the snapshot would
 * otherwise make every later `--check` report drift.
 */
function drifted(current: string, next: string): boolean {
	const normalize = (text: string): string => {
		try {
			const parsed = JSON.parse(text) as { provenance?: Record<string, unknown> };
			if (parsed.provenance) delete parsed.provenance.gitRevision;
			return JSON.stringify(parsed);
		} catch {
			return text;
		}
	};
	return normalize(current) !== normalize(next);
}

async function main(): Promise<void> {
	// Bun caches module lookups at startup, so a link created during this
	// process is invisible to it: the evaluation runs in a child started once
	// the links exist.
	if (process.env[GENERATION_ENV] === "1") {
		assertWorkspaceResolution();
		const target = path.join(ROOT, OUTPUT);
		const text = await render();
		const stats = `${JSON.parse(text).settings.length} settings, ${Buffer.byteLength(text)} bytes`;
		if (process.argv.includes(CHECK_FLAG)) {
			const current = fs.existsSync(target) ? fs.readFileSync(target, "utf8") : "";
			if (drifted(current, text)) {
				console.error(`${OUTPUT} is stale; run \`${GENERATOR}\` to regenerate (${stats}).`);
				process.exitCode = 1;
			}
			return;
		}
		fs.mkdirSync(path.dirname(target), { recursive: true });
		fs.writeFileSync(target, text);
		fs.chmodSync(target, 0o644);
		console.error(`wrote ${OUTPUT}: ${stats}`);
		return;
	}

	const removeLinks = withWorkspaceLinks();
	try {
		const child = Bun.spawnSync([process.execPath, import.meta.path, ...process.argv.slice(2)], {
			env: { ...process.env, [GENERATION_ENV]: "1" },
			stdio: ["inherit", "inherit", "inherit"],
		});
		if (child.exitCode !== 0) process.exitCode = child.exitCode ?? 1;
	} finally {
		removeLinks();
	}
}

await main();
