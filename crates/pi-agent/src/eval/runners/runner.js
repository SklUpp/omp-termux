// Persistent JavaScript kernel for omp eval (Bun subprocess, node:vm).
//
// Protocol: one JSON request per stdin line ({id,code,cwd,env,silent,storeHistory}
// or {type:"exit"}); JSON frames on stdout. Cell stdout/stderr go through a
// context facade, and the real `process.stdout`/`process.stderr`/`fs` fd 1-2
// write paths are patched after the protocol writer is captured, so output from
// `require("node:process")`/`require("node:fs")` is captured instead of dropping
// into (or forging) the frame stream.
//
// Contract port notes:
// - static imports are rewritten to `__omp_import__` resolved against the cell cwd;
// - top-level let/const are demoted to var so re-running a cell replaces bindings;
// - async cells (imports or top-level await) publish their declarations on
//   globalThis because function scope would otherwise lose them;
// - class declarations are demoted to `var C = class C`, but exotic destructured
//   declarations still follow normal script scoping.

const vm = require("node:vm");
const path = require("node:path");
const { pathToFileURL } = require("node:url");
const { createRequire } = require("node:module");

const emitRaw = process.stdout.write.bind(process.stdout);
const emitErrRaw = process.stderr.write.bind(process.stderr);
const fs = require("node:fs");
const webCrypto =
	typeof crypto !== "undefined" && crypto && typeof crypto.getRandomValues === "function"
		? crypto
		: require("node:crypto").webcrypto;
let CURRENT_RUN = null;
let executionCount = 0;
let displayCount = 0;
let silent = false;
let cwd = process.cwd();
let managedEnv = {};
const envSnapshot = { ...process.env };

function frame(value) {
	emitRaw(`${JSON.stringify(value)}\n`);
}

function chunk(text, channel = "stdout") {
	if (!CURRENT_RUN || typeof text !== "string" || text.length === 0) return;
	frame({ type: channel, id: CURRENT_RUN, data: text });
}

function textOf(data, encoding) {
	if (typeof data === "string") return data;
	if (data instanceof Uint8Array) {
		return Buffer.from(data.buffer, data.byteOffset, data.byteLength).toString(encoding || "utf8");
	}
	return String(data);
}

// `require("node:process")` and `require("node:fs")` expose the real objects, so
// a facade alone cannot keep user writes off the protocol pipe. Patch the real
// write entry points (the protocol's own `emitRaw` was bound first) and surface
// unparseable protocol lines as cell output in the kernel.
const realWriteSync = fs.writeSync.bind(fs);
const realWrite = fs.write.bind(fs);
const realWriteFileSync = fs.writeFileSync.bind(fs);

function fdText(fd, data, offset, length) {
	if (typeof data === "string") return { text: data, bytes: Buffer.byteLength(data) };
	if (data instanceof Uint8Array || data instanceof ArrayBuffer) {
		const view = data instanceof ArrayBuffer
			? Buffer.from(data)
			: Buffer.from(data.buffer, data.byteOffset, data.byteLength);
		const start = typeof offset === "number" ? offset : 0;
		const len = typeof length === "number" ? length : view.length - start;
		return { text: view.toString("utf8", start, start + len), bytes: len };
	}
	return null;
}

function patchStreamWrite(stream, channel) {
	const write = (data, encoding, callback) => {
		chunk(textOf(data, typeof encoding === "string" ? encoding : undefined), channel);
		if (typeof encoding === "function") encoding();
		if (typeof callback === "function") callback();
		return true;
	};
	try {
		Object.defineProperty(stream, "write", { value: write, configurable: true, writable: true });
	} catch {
		try {
			stream.write = write;
		} catch {
			// Native stream without a writable `write`: kernel raw-line fallback still
			// surfaces the bytes as cell output instead of a forged frame.
		}
	}
}

patchStreamWrite(process.stdout, "stdout");
patchStreamWrite(process.stderr, "stderr");
fs.writeSync = (fd, data, offset, length, position) => {
	if (fd === 1 || fd === 2) {
		const parsed = fdText(fd, data, offset, length);
		if (parsed) {
			chunk(parsed.text, fd === 2 ? "stderr" : "stdout");
			return parsed.bytes;
		}
	}
	return realWriteSync(fd, data, offset, length, position);
};
fs.write = (...args) => {
	const fd = args[0];
	if (fd === 1 || fd === 2) {
		const callback = typeof args.at(-1) === "function" ? args.pop() : undefined;
		const parsed = fdText(fd, args[1], typeof args[2] === "number" ? args[2] : undefined, typeof args[3] === "number" ? args[3] : undefined);
		if (parsed) {
			chunk(parsed.text, fd === 2 ? "stderr" : "stdout");
			if (callback) process.nextTick(callback, null, parsed.bytes, args[1]);
			return;
		}
	}
	return realWrite(...args);
};
fs.writeFileSync = (target, data, options) => {
	if (target === 1 || target === 2) {
		const parsed = fdText(target, data);
		if (parsed) {
			chunk(parsed.text, target === 2 ? "stderr" : "stdout");
			return;
		}
	}
	return realWriteFileSync(target, data, options);
};

function status(op, extra = {}) {
	if (!CURRENT_RUN) return;
	frame({ type: "display", id: CURRENT_RUN, bundle: { "application/x-omp-status": { op, ...extra } } });
}

function toJsonSafe(value) {
	if (typeof value === "bigint") return `${value}n`;
	if (typeof value === "function") return `[Function ${value.name || "anonymous"}]`;
	if (typeof value === "undefined") return null;
	try {
		return JSON.parse(
			JSON.stringify(value, (_key, inner) => {
				if (typeof inner === "bigint") return `${inner}n`;
				if (typeof inner === "function") return `[Function ${inner.name || "anonymous"}]`;
				return inner;
			}),
		);
	} catch {
		return String(value);
	}
}

function normalizeBundle(value) {
	if (value && typeof value === "object" && !Array.isArray(value)) {
		if (value.data && typeof value.data === "object") return value.data;
		const keys = Object.keys(value);
		if (keys.some(key => key.includes("/") || key === "text" || key === "html" || key === "json")) {
			return value;
		}
	}
	return { "application/json": toJsonSafe(value) };
}

function display(value) {
	if (!CURRENT_RUN) return;
	displayCount += 1;
	frame({ type: "display", id: CURRENT_RUN, bundle: { data: normalizeBundle(value) } });
}

function textFrom(value) {
	if (value && typeof value === "object" && typeof value.text === "string") return value.text;
	return typeof value === "string" ? value : JSON.stringify(value);
}

async function bridgeCall(name, args) {
	if (!CURRENT_RUN) throw new Error("eval bridge called outside an execution");
	const url = managedEnv.PI_TOOL_BRIDGE_URL || envSnapshot.PI_TOOL_BRIDGE_URL;
	const token = managedEnv.PI_TOOL_BRIDGE_TOKEN || envSnapshot.PI_TOOL_BRIDGE_TOKEN;
	const session = managedEnv.PI_TOOL_BRIDGE_SESSION || envSnapshot.PI_TOOL_BRIDGE_SESSION;
	if (!url || !token || !session) throw new Error("eval tool bridge is not configured");
	const response = await fetch(`${url}/v1/tool`, {
		method: "POST",
		headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
		body: JSON.stringify({ session, run: CURRENT_RUN, name, args: args ?? {} }),
	});
	const payload = await response.json();
	if (!payload.ok) throw new Error(payload.error || "eval bridge call failed");
	return payload.value;
}

async function importModule(specifier) {
	const resolved =
		specifier.startsWith(".") || specifier.startsWith("/")
			? pathToFileURL(path.resolve(cwd, specifier)).href
			: specifier;
	return await import(resolved);
}

function rewriteStaticImports(source) {
	let index = 0;
	let found = false;
	const sourceWithoutImports = source
		.replace(
			/^[ \t]*import\s+(?:([\w$]+)\s*,?\s*)?(?:\*\s+as\s+([\w$]+)|\{([^}]*)\})?\s*from\s*(['"])([^'"]+)\4;?[ \t]*$/gm,
			(_match, defaultName, namespace, named, _quote, specifier) => {
				found = true;
				index += 1;
				const binding = `__omp_m${index}`;
				const parts = [`const ${binding} = await __omp_import__(${JSON.stringify(specifier)});`];
				if (defaultName) parts.push(`const ${defaultName} = ${binding}.default;`);
				if (namespace) parts.push(`const ${namespace} = ${binding};`);
				if (named && named.trim()) {
					parts.push(`const {${named.replace(/\s+as\s+/g, ": ")}} = ${binding};`);
				}
				return parts.join(" ");
			},
		)
		.replace(/^[ \t]*import\s*(['"])([^'"]+)\1;?[ \t]*$/gm, (_match, _quote, specifier) => {
			found = true;
			return `await __omp_import__(${JSON.stringify(specifier)});`;
		});
	return { source: sourceWithoutImports, found };
}

function demoteTopLevelDeclarations(source) {
	return source
		.replace(/^(const|let)\s+/gm, "var ")
		.replace(/^class\s+([A-Za-z_$][\w$]*)/gm, "var $1 = class $1");
}

function topLevelBindings(source) {
	const names = new Set();
	for (const match of source.matchAll(
		/^(?:var|let|const|class|async\s+function|function)\s+([A-Za-z_$][\w$]*)/gm,
	)) {
		names.add(match[1]);
	}
	for (const match of source.matchAll(/^import\s+(?:([\w$]+)\s*,?\s*)?(?:\*\s+as\s+([\w$]+)|\{([^}]*)\})?/gm)) {
		if (match[1]) names.add(match[1]);
		if (match[2]) names.add(match[2]);
		if (match[3]) {
			for (const part of match[3].split(",")) {
				const name = part.trim().split(/\s+as\s+/).pop();
				if (name && /^[A-Za-z_$][\w$]*$/.test(name)) names.add(name);
			}
		}
	}
	return [...names];
}

function splitFinalExpression(source) {
	const lines = source.replace(/\s+$/u, "").split("\n");
	for (let index = lines.length - 1; index >= 0; index -= 1) {
		const line = lines[index].trim();
		if (!line || line.startsWith("//") || line.startsWith("/*") || line.startsWith("*")) continue;
		if (
			/^(?:var|let|const|class|function|async|if|for|while|switch|try|catch|finally|return|throw|import|export|await)\b/.test(
				line,
			)
		) {
			return { body: source, expression: null };
		}
		if (line.startsWith("}") || line.endsWith("{")) return { body: source, expression: null };
		const expression = line.replace(/;\s*$/, "");
		return { body: lines.slice(0, index).join("\n"), expression };
	}
	return { body: source, expression: null };
}

function safeInspect(value) {
	try {
		return typeof value === "string" ? value : JSON.stringify(value);
	} catch {
		return String(value);
	}
}

function makeConsole() {
	const write = channel => (...args) => {
		chunk(`${args.map(value => safeInspect(value)).join(" ")}\n`, channel);
	};
	const timers = new Map();
	const counts = new Map();
	return {
		log: write("stdout"),
		info: write("stdout"),
		debug: write("stdout"),
		warn: write("stderr"),
		error: write("stderr"),
		dir: value => write("stdout")(value),
		trace: (...args) => write("stdout")(`Trace: ${args.map(safeInspect).join(" ")}`),
		assert: (condition, ...args) => {
			if (!condition) write("stderr")("Assertion failed:", ...args);
		},
		group: (...args) => write("stdout")(...args),
		groupCollapsed: (...args) => write("stdout")(...args),
		groupEnd: () => {},
		table: value => write("stdout")(value),
		time: label => timers.set(String(label ?? "default"), Date.now()),
		timeLog: label => {
			const key = String(label ?? "default");
			const start = timers.get(key);
			if (start !== undefined) write("stdout")(`${key}: ${Date.now() - start}ms`);
		},
		timeEnd: label => {
			const key = String(label ?? "default");
			const start = timers.get(key);
			if (start !== undefined) {
				timers.delete(key);
				write("stdout")(`${key}: ${Date.now() - start}ms`);
			}
		},
		count: label => {
			const key = String(label ?? "default");
			const next = (counts.get(key) ?? 0) + 1;
			counts.set(key, next);
			write("stdout")(`${key}: ${next}`);
		},
		countReset: label => counts.delete(String(label ?? "default")),
	};
}

const consoleFacade = makeConsole();
const processFacade = {
	env: envSnapshot,
	cwd: () => cwd,
	chdir: target => {
		cwd = path.resolve(cwd, target);
	},
	stdout: { write: text => chunk(String(text)) },
	stderr: { write: text => chunk(String(text), "stderr") },
	platform: process.platform,
	arch: process.arch,
	version: process.version,
	versions: process.versions,
	exitCode: 0,
};

const context = vm.createContext({});

async function concurrencyLimit() {
	try {
		const value = await bridgeCall("__concurrency__", {});
		const limit = Math.trunc(Number(value && typeof value === "object" ? value.limit : value));
		return Number.isFinite(limit) && limit > 0 ? limit : 0;
	} catch {
		return 0;
	}
}

async function pool(items, fn) {
	const list = Array.from(items ?? []);
	if (list.length === 0) return [];
	const limit = await concurrencyLimit();
	const width = limit > 0 ? Math.min(limit, list.length) : list.length;
	const results = new Array(list.length);
	const errors = new Map();
	let next = 0;
	const worker = async () => {
		while (true) {
			const index = next++;
			if (index >= list.length) return;
			try {
				results[index] = await fn(list[index], index);
			} catch (error) {
				errors.set(index, error);
			}
		}
	};
	await Promise.all(Array.from({ length: width }, () => worker()));
	if (errors.size > 0) throw errors.get(Math.min(...errors.keys()));
	return results;
}

const helpers = {
	display,
	log: message => status("log", { message: String(message) }),
	phase: title => status("phase", { title: String(title) }),
	status,
	async read(file, options = {}) {
		let target = String(file);
		const offset = typeof options.offset === "number" ? options.offset : 1;
		const limit = typeof options.limit === "number" ? options.limit : undefined;
		if (offset > 1 || limit !== undefined) {
			target = limit === undefined ? `${target}:${offset}-` : `${target}:${offset}-${offset + limit - 1}`;
		}
		return textFrom(await bridgeCall("read", { path: target }));
	},
	async write(file, data) {
		return await bridgeCall("write", { path: String(file), content: String(data) });
	},
	env: (key, fallback) => {
		const name = String(key);
		return managedEnv[name] ?? envSnapshot[name] ?? fallback;
	},
	budget: {
		total: async () => (await bridgeCall("__budget__", {})).total ?? null,
		spent: async () => Number((await bridgeCall("__budget__", {})).spent ?? 0),
		remaining: async () => {
			const snapshot = await bridgeCall("__budget__", {});
			return snapshot.total == null ? Infinity : Math.max(0, Number(snapshot.total) - Number(snapshot.spent ?? 0));
		},
		hard: async () => Boolean((await bridgeCall("__budget__", {})).hard),
	},
	async completion(prompt, options = {}) {
		const value = await bridgeCall("__completion__", { prompt, ...options });
		const text = textFrom(value);
		return Object.prototype.hasOwnProperty.call(options, "schema") ? JSON.parse(text) : text;
	},
	async agent(prompt, options = {}) {
		const value = await bridgeCall("__agent__", { prompt, ...options, handle: Boolean(options.handle) });
		const text = textFrom(value);
		const data = value && typeof value === "object" && Object.prototype.hasOwnProperty.call(value, "data") ? value.data : undefined;
		const parsed = data !== undefined ? data : Object.prototype.hasOwnProperty.call(options, "schema") ? JSON.parse(text) : text;
		if (!options.handle) return parsed;
		const details = value && typeof value === "object" ? value.details : undefined;
		if (!details || details.id == null) return { text, output: text, handle: null, id: null, agent: null };
		const node = { text, output: text, handle: `agent://${details.id}`, id: details.id, agent: details.agent ?? null };
		if (parsed !== text) node.data = parsed;
		return node;
	},
	async output(...ids) {
		let options = {};
		let targets = ids;
		if (ids.length > 0) {
			const last = ids.at(-1);
			if (last && typeof last === "object" && !Array.isArray(last)) {
				options = last;
				targets = ids.slice(0, -1);
			}
		}
		const values = await Promise.all(targets.map(id => helpers.read(`agent://${id}`, options)));
		return values.length === 1 ? values[0] : values;
	},
	parallel: thunks =>
		pool(thunks, (thunk, index) => {
			if (typeof thunk !== "function") throw new TypeError("parallel() expects an iterable of functions");
			return thunk(index);
		}),
	pipeline: async (items, ...stages) => {
		let current = Array.from(items ?? []);
		for (const stage of stages) {
			if (typeof stage !== "function") throw new TypeError("pipeline() stages must be functions");
			current = await pool(current, stage);
		}
		return current;
	},
};

const toolProxy = new Proxy(
	{},
	{
		get(_target, property) {
			if (typeof property !== "string") return undefined;
			return async args => bridgeCall(property, args ?? {});
		},
	},
);

Object.assign(context, {
	console: consoleFacade,
	process: processFacade,
	setTimeout,
	clearTimeout,
	setInterval,
	clearInterval,
	setImmediate,
	clearImmediate,
	queueMicrotask,
	fetch,
	URL,
	URLSearchParams,
	TextEncoder,
	TextDecoder,
	AbortController,
	AbortSignal,
	structuredClone,
	atob: value => Buffer.from(value, "base64").toString("binary"),
	btoa: value => Buffer.from(value, "binary").toString("base64"),
	Buffer,
	Bun: typeof Bun !== "undefined" ? Bun : undefined,
	fs,
	crypto: webCrypto,
	webcrypto: webCrypto,
	createRequire,
	require: createRequire(path.join(cwd, "cell.js")),
	__omp_import__: importModule,
	...helpers,
	tool: toolProxy,
	print: consoleFacade.log,
});

async function execute(request) {
	CURRENT_RUN = request.id;
	silent = Boolean(request.silent);
	displayCount = 0;
	managedEnv = request.env ?? {};
	cwd = path.resolve(request.cwd || cwd);
	process.chdir(cwd);
	context.require = createRequire(path.join(cwd, "cell.js"));
	executionCount += 1;

	let source = String(request.code ?? "");
	if (
		typeof Bun !== "undefined" &&
		typeof Bun.Transpiler === "function" &&
		/(?:\bimport\s+type\b|\binterface\s+\w|\btype\s+\w+\s*=|\b(?:as|satisfies)\s+(?:const\b|[A-Z]\w*)|:\s*(?:string|number|boolean|any|unknown|void|never)\b)/.test(
			source,
		)
	) {
		try {
			source = new Bun.Transpiler({ loader: "ts" }).transformSync(source);
		} catch {
			// Leave the source untouched; the runtime reports the real syntax error.
		}
	}
	const names = new Set(topLevelBindings(source));
	const rewritten = rewriteStaticImports(source);
	const demoted = demoteTopLevelDeclarations(rewritten.source);
	for (const name of topLevelBindings(demoted)) names.add(name);
	const { body, expression } = splitFinalExpression(demoted);
	const needsAsync = rewritten.found || /\bawait\b/.test(body) || expression !== null;

	let scriptSource;
	if (needsAsync) {
		const publish = [...names].map(name => `globalThis[${JSON.stringify(name)}] = ${name};`).join(" ");
		const valueLine = expression === null ? "" : `const __omp_last__ = (${expression});`;
		scriptSource = `(async () => {\n${body}\n;${valueLine}\n${publish}\nreturn ${expression === null ? "undefined" : "__omp_last__"};\n})()`;
	} else {
		scriptSource = expression === null ? body : `${body}\n;(${expression})`;
	}

	let value;
	try {
		const script = new vm.Script(scriptSource, {
			filename: `eval-${request.id}.js`,
			importModuleDynamically: importModule,
		});
		value = script.runInContext(context);
		if (value && typeof value.then === "function") value = await value;
	} catch (error) {
		frame({
			type: "error",
			id: request.id,
			ename: error && error.name ? String(error.name) : "Error",
			evalue: error && error.message ? String(error.message) : String(error),
			traceback: error && error.stack ? String(error.stack).split("\n") : [],
		});
		frame({ type: "done", id: request.id, status: "error", executionCount });
		CURRENT_RUN = null;
		return;
	}

	if (!silent && displayCount === 0 && value !== undefined && value !== null) {
		try {
			display(value);
		} catch {
			// Auto-display is best-effort; execution succeeded regardless.
		}
	}
	frame({ type: "done", id: request.id, status: "ok", executionCount });
	CURRENT_RUN = null;
}

let buffer = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", data => {
	buffer += data;
	while (true) {
		const newline = buffer.indexOf("\n");
		if (newline < 0) break;
		const line = buffer.slice(0, newline).trim();
		buffer = buffer.slice(newline + 1);
		if (!line) continue;
		let request;
		try {
			request = JSON.parse(line);
		} catch {
			continue;
		}
		if (request.type === "exit") process.exit(0);
		execute(request).catch(error => {
			frame({
				type: "error",
				id: request.id,
				ename: "KernelError",
				evalue: error && error.message ? String(error.message) : String(error),
				traceback: [],
			});
			frame({ type: "done", id: request.id, status: "error", executionCount });
			CURRENT_RUN = null;
		});
	}
});
process.stdin.on("end", () => process.exit(0));
