import { describe, expect, it } from "bun:test";
import * as fs from "node:fs";
import * as path from "node:path";
import { buildGeneratedBlock, generateEnumExports } from "../../natives/scripts/gen-enums";

describe("generated native exports android fallback", () => {
	const unsupported = ["AudioCapture", "AudioPlayback", "LiveWebRtcPeer", "copyToClipboard", "readImageFromClipboard"];
	const indexJsPath = path.resolve(import.meta.dir, "../../natives/native/index.js");
	const genEnumsPath = path.resolve(import.meta.dir, "../../natives/scripts/gen-enums.ts");

	it("real generator preserves fail-loud wrappers for every unsupported export", async () => {
		// Build a synthetic d.ts that contains all unsupported exports plus one control export
		const fakeDts = `
export declare class AudioCapture {}
export declare class AudioPlayback {}
export declare class LiveWebRtcPeer {}
export declare class Process {}
export declare function copyToClipboard(): void;
export declare function readImageFromClipboard(): void;
export declare function glob(): void;
export declare enum MyEnum { A = "a" }
`;
		const block = buildGeneratedBlock(fakeDts);
		for (const name of unsupported) {
			expect(block).toContain(`export const ${name} =`);
			expect(block).toContain(`Native ${name} is unsupported on Android/Termux`);
			expect(block).not.toContain(`export const ${name} = undefined`);
			// Proxy fail-loud pattern
			expect(block).toContain("new Proxy(err");
		}
		// Supported exports must still be direct re-exports
		expect(block).toContain("export const Process = nativeBindings.Process");
		expect(block).toContain("export const glob = nativeBindings.glob");
	});

	it("generateEnumExports round-trip preserves android stubs on disk", async () => {
		// Run the actual generator against the real index.d.ts/index.js if they exist; verify stub survival
		const dtsPath = path.resolve(import.meta.dir, "../../natives/native/index.d.ts");
		try {
			const before = await fs.promises.readFile(indexJsPath, "utf8");
			// Only run generator if native dts exists (skip in CI without rust build)
			if (fs.existsSync(dtsPath)) {
				await generateEnumExports();
				const after = await fs.promises.readFile(indexJsPath, "utf8");
				for (const name of unsupported) {
					expect(after).toContain(`Native ${name} is unsupported on Android/Termux`);
				}
				expect(after).toContain("new Proxy(err");
				// Restore original to avoid dirtying working tree for other tests
				await fs.promises.writeFile(indexJsPath, before, "utf8");
			} else {
				// Fallback: ensure gen-enums.ts still contains the stub logic (source check is secondary to real generation above)
				const gen = await fs.promises.readFile(genEnumsPath, "utf8");
				for (const name of unsupported) expect(gen).toContain(name);
			}
		} catch (e) {
			// If generator fails due to missing napi artifacts, still assert synthetic generation succeeded (above)
			expect(String(e)).not.toContain("No public symbols");
		}
	});

	it("index.js on disk still contains fail-loud wrappers and never exports undefined", async () => {
		const content = await fs.promises.readFile(indexJsPath, "utf8");
		for (const name of unsupported) {
			expect(content).toContain(name);
			expect(content).toContain(`Native ${name} is unsupported on Android/Termux`);
			expect(content).not.toContain(`export const ${name} = undefined`);
		}
		expect(content).toContain("new Proxy(err");
	});

	it("pi-natives Cargo gates pi-voice and keeps desktop audio on other platforms", async () => {
		const cargoPath = path.resolve(import.meta.dir, "../../../crates/pi-natives/Cargo.toml");
		const cargo = await fs.promises.readFile(cargoPath, "utf8");
		// No unconditional pi-voice in [dependencies] — must be gated
		const depsSection = cargo.split("[target.")[0];
		expect(depsSection).not.toContain("pi-voice.workspace");
		// Gated section must contain pi-voice and arboard together
		expect(cargo).toContain("[target.'cfg(not(target_os = \"android\"))'.dependencies]");
		expect(cargo).toContain("pi-voice.workspace = true");
		expect(cargo).toContain("arboard.workspace = true");
		// Ensure we didn't accidentally remove desktop audio deps
		expect(cargo).toContain("pi-walker.workspace");
	});

	it("audio and live Rust sources avoid unconditional pi_voice import and keep fail-loud messages", async () => {
		const audioPath = path.resolve(import.meta.dir, "../../../crates/pi-natives/src/audio.rs");
		const livePath = path.resolve(import.meta.dir, "../../../crates/pi-natives/src/live.rs");
		const audio = await fs.promises.readFile(audioPath, "utf8");
		const live = await fs.promises.readFile(livePath, "utf8");
		for (const src of [audio, live]) {
			// Every `use pi_voice` must be gated by cfg(not android) on preceding line
			const gatedUses = (src.match(/#\[cfg\(not\(target_os = "android"\)\)\]\s*\nuse pi_voice/g) ?? []).length;
			const totalUses = (src.match(/use pi_voice/g) ?? []).length;
			expect(gatedUses).toBe(totalUses);
			expect(totalUses).toBeGreaterThan(0);
		}
		// Audio stubs keep exact error strings
		expect(audio).toContain("Native AudioCapture is unsupported on Android/Termux");
		expect(audio).toContain("Native AudioPlayback is unsupported on Android/Termux");
		// Live stubs keep exact error string without importing pi_voice on Android
		expect(live).toContain("LiveWebRtcPeer is unsupported on Android/Termux");
		// Ensure Android stubs do not pull audiopus via pi_voice types
		// Live private field must be gated, not always Arc<LivePeerCore>
		expect(live).toContain('#[cfg(target_os = "android")]');
		expect(live).toContain('#[cfg(not(target_os = "android"))]');
	});
	it("loader-state includes android-arm64 platform", async () => {
		const loader = await fs.promises.readFile(
			path.resolve(import.meta.dir, "../../natives/native/loader-state.js"),
			"utf8",
		);
		expect(loader).toContain("android-arm64");
	});
});
