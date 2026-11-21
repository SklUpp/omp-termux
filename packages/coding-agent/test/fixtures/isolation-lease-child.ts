import { claimIsolationOwner, withIsolationLease } from "../../src/task/isolation-ownership";

const [mode, ...args] = process.argv.slice(2);
if (!mode) throw new Error("Expected a mode");

async function waitForRelease(): Promise<void> {
	process.stdin.resume();
	await new Promise(resolve => process.stdin.once("data", resolve));
	// Drop the stdin handle: the parent closes it after one byte, and a resumed
	// pipe would otherwise keep this process alive past the release.
	process.stdin.pause();
}

if (mode === "hold") {
	const [slot] = args;
	if (!slot) throw new Error("Expected a slot");
	await withIsolationLease(slot, async () => {
		process.stdout.write("held\n");
		await waitForRelease();
	});
} else if (mode === "claim") {
	const [base, id, slot] = args;
	if (!base || !id || !slot) throw new Error("Expected base, id, and slot");
	try {
		await claimIsolationOwner(base, id, slot);
		process.stdout.write("won\n");
	} catch (error) {
		process.stdout.write(`lost:${error instanceof Error ? error.message : String(error)}\n`);
	}
	await waitForRelease();
} else {
	throw new Error(`Unknown mode ${mode}`);
}
