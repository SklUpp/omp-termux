import { describe, expect, it } from "bun:test";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";
import { buildGeneratedBlock, generateEnumExports } from "../../natives/scripts/gen-enums";

const FAKE_DTS = `
export declare class AudioCapture {}
export declare class AudioPlayback {}
export declare class LiveWebRtcPeer {}
export declare class Process {}
export declare function copyToClipboard(text: string): void;
export declare function readImageFromClipboard(): Promise<unknown>;
export declare function glob(): void;
`;

/**
 * Only the names the Android branch replaces. A block built from it has no
 * ungated fallback to excuse a call, so "the fallback ran" is observable.
 */
const GATED_DTS = `
export declare class AudioCapture {}
export declare class AudioPlayback {}
export declare class LiveWebRtcPeer {}
export declare function copyToClipboard(text: string): void;
export declare function readImageFromClipboard(): Promise<unknown>;
`;

const MARKER_START = "// --- generated native exports (do not edit) ---";
const MARKER_END = "// --- end generated native exports ---";

function extractBlock(source: string): string {
	const start = source.indexOf(MARKER_START);
	const end = source.indexOf(MARKER_END);
	if (start === -1 || end === -1) throw new Error("generated marker block missing");
	return source.slice(start + MARKER_START.length, end);
}

/** Evaluate the generated exports against a fake addon and platform. */
function instantiate(
	block: string,
	options: {
		platform: NodeJS.Platform;
		bindings: Record<string, unknown>;
		missingNativeExport: (name: string) => unknown;
		/**
		 * Class counterpart of `missingNativeExport`. The default is the current-addon
		 * shape (`undefined`), which keeps upstream's absent-binding passthrough.
		 */
		missingNativeClass?: (name: string) => unknown;
	},
): Record<string, any> {
	const names = [...block.matchAll(/^export const (\w+) =/gm)].map(match => match[1]!);
	const body = block.replace(/^export const /gm, "const ");
	const factory = new Function(
		"nativeBindings",
		"missingNativeExport",
		"missingNativeClass",
		"process",
		`${body}\nreturn { ${names.join(", ")} };`,
	);
	return factory(options.bindings, options.missingNativeExport, options.missingNativeClass ?? (() => undefined), {
		platform: options.platform,
	});
}

function missingExport(name: string): () => never {
	return () => {
		throw new Error(`missingNativeExport:${name}`);
	};
}

describe("gen-enums Android platform gating", () => {
	it("Android keeps the unsupported-on-Termux proxies", () => {
		const missingCalls: string[] = [];
		const missingClassCalls: string[] = [];
		const exports = instantiate(buildGeneratedBlock(GATED_DTS), {
			platform: "android",
			bindings: {},
			missingNativeExport: name => {
				missingCalls.push(name);
				return undefined;
			},
			missingNativeClass: name => {
				missingClassCalls.push(name);
				return undefined;
			},
		});

		expect(() => exports.copyToClipboard("x")).toThrow("unsupported on Android/Termux");
		expect(() => exports.readImageFromClipboard()).toThrow("unsupported on Android/Termux");
		expect(() => new exports.AudioCapture()).toThrow("unsupported on Android/Termux");
		expect(() => new exports.AudioPlayback()).toThrow("unsupported on Android/Termux");
		expect(() => new exports.LiveWebRtcPeer()).toThrow("unsupported on Android/Termux");
		// The unsupported names must take the Android branch, not the desktop
		// missing-export fallback.
		expect(missingCalls).not.toContain("copyToClipboard");
		expect(missingCalls).not.toContain("readImageFromClipboard");
		// `GATED_DTS` holds nothing but gated names, so any fallback call — for a
		// function or for a class — proves the Android branch was bypassed.
		expect(missingCalls).toEqual([]);
		expect(missingClassCalls).toEqual([]);
	});

	it("desktop keeps the stale-addon diagnostic instead of the Android message", () => {
		const exports = instantiate(buildGeneratedBlock(FAKE_DTS), {
			platform: "linux",
			bindings: {},
			missingNativeExport: missingExport,
		});

		expect(() => exports.copyToClipboard("x")).toThrow("missingNativeExport:copyToClipboard");
		expect(() => exports.readImageFromClipboard()).toThrow("missingNativeExport:readImageFromClipboard");
		// Upstream passthrough for classes: absent binding stays absent on a
		// current addon (the harness default `missingNativeClass`).
		expect(exports.AudioCapture).toBeUndefined();
	});

	it("desktop throws the stale class stub where Android still says unsupported", () => {
		// `missingNativeClass` returns `undefined` for a current addon and a
		// throwing stub for a stale one (loader-state.js); the Android branch must
		// never surface that desktop-only diagnostic, which is the whole point of
		// the platform split.
		const staleClass = (name: string) =>
			class StaleNativeClass {
				constructor() {
					throw new Error(`missingNativeClass:${name}`);
				}
			};
		const block = buildGeneratedBlock(GATED_DTS);

		const desktop = instantiate(block, {
			platform: "linux",
			bindings: {},
			missingNativeExport: missingExport,
			missingNativeClass: staleClass,
		});
		expect(() => new desktop.AudioCapture()).toThrow("missingNativeClass:AudioCapture");
		expect(() => new desktop.AudioPlayback()).toThrow("missingNativeClass:AudioPlayback");
		expect(() => new desktop.LiveWebRtcPeer()).toThrow("missingNativeClass:LiveWebRtcPeer");

		const android = instantiate(block, {
			platform: "android",
			bindings: {},
			missingNativeExport: missingExport,
			missingNativeClass: staleClass,
		});
		expect(() => new android.AudioCapture()).toThrow("unsupported on Android/Termux");
		expect(() => new android.AudioPlayback()).toThrow("unsupported on Android/Termux");
		expect(() => new android.LiveWebRtcPeer()).toThrow("unsupported on Android/Termux");
	});

	it("both platforms use the real binding when the addon exports it", () => {
		const copy = () => "copied";
		const glob = () => "globbed";
		for (const platform of ["android", "linux"] as const) {
			const exports = instantiate(buildGeneratedBlock(FAKE_DTS), {
				platform,
				bindings: { copyToClipboard: copy, glob },
				missingNativeExport: missingExport,
			});
			expect(exports.copyToClipboard).toBe(copy);
			expect(exports.glob).toBe(glob);
		}
	});
});

describe("generateEnumExports consumer", () => {
	it("writes a platform-gated block that keeps desktop diagnostics", async () => {
		const dir = await fs.mkdtemp(path.join(os.tmpdir(), "omp-gen-enums-"));
		try {
			await fs.writeFile(path.join(dir, "index.d.ts"), FAKE_DTS);
			await fs.writeFile(
				path.join(dir, "index.js"),
				`import { loadNative, missingNativeExport } from "./loader-state.js";\nconst nativeBindings = loadNative();\n${MARKER_START}\n${MARKER_END}\n`,
			);

			await generateEnumExports({ nativeDir: dir });

			const written = await fs.readFile(path.join(dir, "index.js"), "utf8");
			const block = extractBlock(written);

			const androidMissing: string[] = [];
			const android = instantiate(block, {
				platform: "android",
				bindings: {},
				missingNativeExport: name => {
					androidMissing.push(name);
					return undefined;
				},
			});
			expect(androidMissing).not.toContain("copyToClipboard");
			expect(() => android.copyToClipboard("x")).toThrow("unsupported on Android/Termux");

			const glob = () => "globbed";
			const desktop = instantiate(block, {
				platform: "linux",
				bindings: { glob },
				missingNativeExport: missingExport,
			});
			expect(() => desktop.copyToClipboard("x")).toThrow("missingNativeExport:copyToClipboard");
			expect(() => desktop.readImageFromClipboard()).toThrow("missingNativeExport:readImageFromClipboard");
			// Supported exports stay direct passthroughs on both platforms.
			expect(desktop.glob).toBe(glob);
		} finally {
			await fs.rm(dir, { recursive: true, force: true });
		}
	});
});
