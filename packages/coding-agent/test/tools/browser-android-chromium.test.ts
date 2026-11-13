import { afterAll, describe, expect, it } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

const repoRoot = path.resolve(import.meta.dir, "../..");
const probeFixture = path.join(import.meta.dir, "..", "fixtures", "browser-executable-probe.ts");
// Android has no /bin; Termux and the emulator both ship /system/bin/sh.
const shell = fs.existsSync("/system/bin/sh") ? "/system/bin/sh" : "/bin/sh";
const tempDirs: string[] = [];

function makeTempDir(): string {
	const dir = fs.mkdtempSync(path.join(os.tmpdir(), "omp-android-chromium-"));
	tempDirs.push(dir);
	return dir;
}

function writeChrome(file: string, versionOutput: string): void {
	fs.mkdirSync(path.dirname(file), { recursive: true });
	fs.writeFileSync(file, `#!${shell}\necho "${versionOutput}"\n`);
	fs.chmodSync(file, 0o755);
}

/** The parent's environment without variables that pick or relocate a browser. */
function probeEnv(extra: Record<string, string>): Record<string, string> {
	const env: Record<string, string> = {};
	for (const [key, value] of Object.entries(process.env)) {
		if (value === undefined) continue;
		if (key.startsWith("PUPPETEER_") || key.startsWith("XDG_")) continue;
		if (key === "PI_CODING_AGENT_DIR" || key === "PI_CONFIG_DIR" || key === "OMP_PROFILE" || key === "PI_PROFILE") {
			continue;
		}
		env[key] = value;
	}
	env.HOME = env.HOME ?? os.homedir();
	env.OMP_BROWSER_PROBE_PLATFORM = "android";
	env.PUPPETEER_EXECUTABLE_PATH = "";
	return { ...env, ...extra };
}

interface ProbeResult {
	stdout: string;
	stderr: string;
	exitCode: number;
}

/** Run launch.ts in a child whose `process.platform` is forced to `android`. */
function probeAndroid(extra: Record<string, string>): ProbeResult {
	const result = Bun.spawnSync([process.execPath, probeFixture], {
		cwd: repoRoot,
		env: probeEnv(extra),
		stdout: "pipe",
		stderr: "pipe",
	});
	return {
		stdout: new TextDecoder().decode(result.stdout).trim(),
		stderr: new TextDecoder().decode(result.stderr),
		exitCode: result.exitCode ?? -1,
	};
}

afterAll(() => {
	for (const dir of tempDirs) fs.rmSync(dir, { recursive: true, force: true });
});

// Chrome validation execs a shell script, so the fixture cannot run on Windows.
describe.skipIf(process.platform === "win32")("Android Chromium discovery", () => {
	it("finds the canonical Termux:X11 binary under $PREFIX/lib/chromium", () => {
		const root = makeTempDir();
		const prefix = path.join(root, "prefix");
		const emptyPath = path.join(root, "empty-path");
		fs.mkdirSync(emptyPath, { recursive: true });
		// A PATH wrapper that also answers `--version`; the canonical package
		// binary must still win.
		writeChrome(path.join(prefix, "bin", "chromium"), "Chromium 100.0.0.0");
		const chrome = path.join(prefix, "lib", "chromium", "chrome");
		writeChrome(chrome, "Chromium 149.0.7827.155");

		const result = probeAndroid({ PREFIX: prefix, PATH: emptyPath });

		expect(result.exitCode, result.stderr).toBe(0);
		expect(result.stdout).toBe(chrome);
	});

	it("falls back to the $PREFIX/bin wrapper when the canonical binary is absent", () => {
		const root = makeTempDir();
		const prefix = path.join(root, "prefix");
		const emptyPath = path.join(root, "empty-path");
		fs.mkdirSync(emptyPath, { recursive: true });
		const wrapper = path.join(prefix, "bin", "chromium");
		writeChrome(wrapper, "Chromium 149.0.7827.155");

		const result = probeAndroid({ PREFIX: prefix, PATH: emptyPath });

		expect(result.exitCode, result.stderr).toBe(0);
		expect(result.stdout).toBe(wrapper);
	});

	it("throws install guidance when the only binary is broken", () => {
		const root = makeTempDir();
		const prefix = path.join(root, "prefix");
		const emptyPath = path.join(root, "empty-path");
		fs.mkdirSync(emptyPath, { recursive: true });
		writeChrome(path.join(prefix, "lib", "chromium", "chrome"), "not-a-browser");

		const result = probeAndroid({ PREFIX: prefix, PATH: emptyPath });

		expect(result.exitCode).not.toBe(0);
		expect(result.stderr).toContain("No working Chromium executable found on Android");
		expect(result.stderr).toContain("pkg install -y x11-repo");
		// The managed Linux download must never be reached on Android.
		expect(result.stderr).not.toContain("must not download a browser");
	});

	it("accepts a working explicit PUPPETEER_EXECUTABLE_PATH", () => {
		const root = makeTempDir();
		const chrome = path.join(root, "custom", "chrome");
		writeChrome(chrome, "Chromium 149.0.7827.155");

		const result = probeAndroid({
			PREFIX: path.join(root, "prefix"),
			PATH: path.join(root, "empty-path"),
			PUPPETEER_EXECUTABLE_PATH: chrome,
		});

		expect(result.exitCode, result.stderr).toBe(0);
		expect(result.stdout).toBe(chrome);
	});

	it("fails an explicit PUPPETEER_EXECUTABLE_PATH that does not exist", () => {
		const root = makeTempDir();
		const missing = path.join(root, "missing", "chrome");

		const result = probeAndroid({ PREFIX: path.join(root, "prefix"), PUPPETEER_EXECUTABLE_PATH: missing });

		expect(result.exitCode).not.toBe(0);
		expect(result.stderr).toContain("not a working Chromium executable");
		expect(result.stderr).toContain(missing);
	});

	it("fails an explicit PUPPETEER_EXECUTABLE_PATH that is not Chromium", () => {
		const root = makeTempDir();
		const broken = path.join(root, "custom", "chrome");
		writeChrome(broken, "not-a-browser");

		const result = probeAndroid({ PREFIX: path.join(root, "prefix"), PUPPETEER_EXECUTABLE_PATH: broken });

		expect(result.exitCode).not.toBe(0);
		expect(result.stderr).toContain("not a working Chromium executable");
	});
});
