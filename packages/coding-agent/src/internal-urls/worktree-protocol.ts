/**
 * Protocol handler for `worktree://`: the isolation manager's explicit
 * integration endpoint.
 *
 * A deferred isolated run (`task.isolation.deferIntegration`) keeps its
 * changes in a retained workspace. The parent's consent is a write to this
 * manager — not a message to the child, which never runs to perform the
 * handoff:
 *
 *   write { path: "worktree://<id>", content: "/integrate" }
 *
 * The handler identifies the workspace by the child transcript's session id,
 * the spawning parent session, and the descriptor binding — never by the
 * reusable agent id alone — then claims, captures, and calls the stock merge.
 * Reading the URL never acts; it only explains the write form.
 */
import worktreePromptDoc from "../prompts/internal-urls/worktree.md" with { type: "text" };
import { integrateDeferredAgent, isDeferredIntegrationSignal } from "../task/deferred-integration";
import type {
	InternalResource,
	InternalUrl,
	InternalWriteResult,
	ProtocolHandler,
	SchemeSpec,
	WriteContext,
} from "./types";

export class WorktreeProtocolHandler implements ProtocolHandler {
	readonly scheme = "worktree";
	readonly spec: SchemeSpec = {
		backing: "virtual",
		selectors: "none",
		immutable: false,
		write: { via: "handler", payload: "verbatim", scope: "workspace", tier: () => "read" },
	};

	promptDoc(): string {
		return worktreePromptDoc.trim();
	}

	async locate(): Promise<string | null> {
		return null;
	}

	async write(url: InternalUrl, content: string, context?: WriteContext): Promise<InternalWriteResult> {
		const session = context?.session;
		if (!session) throw new Error("worktree:// requires a tool session");
		const registry = session.agentRegistry;
		const senderId = session.getAgentId?.();
		if (!registry || !senderId) throw new Error("The isolation manager is unavailable in this session.");
		const id = agentId(url);
		if (!isDeferredIntegrationSignal(content)) {
			throw new Error(
				`worktree://${id} accepts only \`/integrate\`; write that signal to merge its retained changes.`,
			);
		}
		// Restore a parked roster through the integration call: an unknown id
		// then reports through the shared outcome instead of a raw bus error.
		const outcome = await integrateDeferredAgent({
			registry,
			senderId,
			target: id,
			parentSessionFile: session.getSessionFile?.() ?? undefined,
			parentSessionId: session.getSessionId?.() ?? undefined,
		});
		return {
			content: [{ type: "text", text: outcome.text }],
			isError: outcome.isError,
		};
	}

	async resolve(url: InternalUrl): Promise<InternalResource> {
		const id = agentId(url);
		return {
			url: url.rawHref ?? url.href,
			contentType: "text/markdown",
			content: [
				`\`worktree://${id}\` manages the retained isolation workspace for agent \`${id}\`.`,
				"",
				`- integrate: \`write\` with path \`worktree://${id}\` and content \`/integrate\`.`,
				`- status/transcript: \`history://${id}\`; run logs: \`agent://${id}\`.`,
			].join("\n"),
			notes: ["Writes perform the handoff; reads only describe it."],
		};
	}
}

/** `worktree://<id>`: one agent id, no path/query/hash selectors. */
function agentId(url: InternalUrl): string {
	const id = url.rawHost || url.hostname;
	const path = url.rawPathname ?? url.pathname;
	if (!id) throw new Error("worktree:// requires an agent id: worktree://<id>");
	if (id === "all") throw new Error("worktree://all is not a valid target; name one agent id.");
	if (path !== "" && path !== "/") {
		throw new Error(`Invalid worktree:// URL: ${url.rawHref ?? url.href}. Use worktree://<id> only.`);
	}
	if (url.search || url.hash) {
		throw new Error(`Invalid worktree:// URL: ${url.rawHref ?? url.href}. Use worktree://<id> only.`);
	}
	return id;
}
