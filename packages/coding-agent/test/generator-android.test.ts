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
	options: { platform: NodeJS.Platform; bindings: Record<string, unknown>; missingNativeExport: (name: string) => unknown },
): Record<string, any> {
	const names = [...block.matchAll(/^export const (\w+) =/gm)].map(match => match[1]!);
	const body = block.replace(/^export const /gm, "const ");
	const factory = new Function(
		"nativeBindings",
		"missingNativeExport",
		"process",
		`${body}\nreturn { ${names.join(", ")} };`,
	);
	return factory(options.bindings, options.missingNativeExport, { platform: options.platform });
}

function missingExport(name: string): () => never {
	return () => {
		throw new Error(`missingNativeExport:${name}`);
	};
}

describe("gen-enums Android platform gating", () => {
	it("Android keeps the unsupported-on-Termux proxies", () => {
		const missingCalls: string[] = [];
		const exports = instantiate(buildGeneratedBlock(FAKE_DTS), {
			platform: "android",
			bindings: {},
			missingNativeExport: name => {
				missingCalls.push(name);
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
	});

	it("desktop keeps the stale-addon diagnostic instead of the Android message", () => {
		const exports = instantiate(buildGeneratedBlock(FAKE_DTS), {
			platform: "linux",
			bindings: {},
			missingNativeExport: missingExport,
		});

		expect(() => exports.copyToClipboard("x")).toThrow("missingNativeExport:copyToClipboard");
		expect(() => exports.readImageFromClipboard()).toThrow("missingNativeExport:readImageFromClipboard");
		// Upstream passthrough for classes: absent binding stays absent.
		expect(exports.AudioCapture).toBeUndefined();
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
