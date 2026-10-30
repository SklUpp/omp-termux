#!/usr/bin/env python3
"""Deterministic ACP end-to-end smoke harness (stdlib only).

Not a Cargo test: run against a fresh binary after the CLI hook lands:

    python3 crates/omp-cli/tests/acp_accept.py ./target/debug/omp

It builds an isolated `PI_CONFIG_DIR`/`HOME`/XDG tree, starts a local
OpenAI-compatible SSE provider on 127.0.0.1, and drives `omp acp` over stdio.
The provider is fully deterministic: prompts containing `PERMISSION` ask for a
`bash` tool call, `CANCEL` streams one chunk then stalls briefly, everything
else returns fixed text. Assertions target wire order and protocol shape, never
model prose.

Covered: initialize + advertised capabilities, new/list/load/resume/fork/close,
set_mode plan/default, set_config_option thinking, prompt block fallbacks,
permission reverse request + `allow_always` persistence, cancel notification,
malformed-line recovery, unknown method, EOF cleanup.
"""

from __future__ import annotations

import argparse
import json
import os
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

REQUEST_TIMEOUT = 20.0
PROMPT_TIMEOUT = 30.0


class Provider:
    """Local OpenAI-compatible streaming endpoint."""

    def __init__(self, model: str = "mock-model") -> None:
        self.model = model
        self.requests = 0
        self.lock = threading.Lock()
        provider = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args):  # silence
                pass

            def do_POST(self):  # noqa: N802
                if not self.path.endswith("/chat/completions"):
                    self.send_error(404)
                    return
                length = int(self.headers.get("content-length", "0"))
                body = json.loads(self.rfile.read(length) or b"{}")
                with provider.lock:
                    provider.requests += 1
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.send_header("cache-control", "no-cache")
                self.send_header("connection", "close")
                self.end_headers()
                for frame in provider.frames(body):
                    self.wfile.write(frame.encode("utf-8") + b"\n\n")
                    self.wfile.flush()
                self.close_connection = True

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.server.server_address[1]}/v1"

    def start(self) -> None:
        self.thread.start()

    def stop(self) -> None:
        self.server.shutdown()
        self.server.server_close()

    def frames(self, body):
        messages = body.get("messages") or []
        last_user = ""
        tool_after_user = False
        for message in messages:
            role = message.get("role")
            if role == "tool":
                tool_after_user = True
            elif role == "user":
                last_user = text_of(message.get("content"))
                tool_after_user = False
        created = int(time.time())
        base = {"id": "chatcmpl-mock", "object": "chat.completion.chunk", "created": created, "model": self.model}

        def chunk(delta, finish=None):
            payload = dict(base)
            payload["choices"] = [{"index": 0, "delta": delta, "finish_reason": finish}]
            return "data: " + json.dumps(payload)

        if tool_after_user:
            yield chunk({"role": "assistant", "content": "tool done"})
            yield chunk({}, "stop")
            yield "data: [DONE]"
            return

        if "PERMISSION" in last_user:
            yield chunk(
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_permission",
                            "type": "function",
                            "function": {"name": "bash", "arguments": "{\"command\":\"echo permission\"}"},
                        }
                    ],
                }
            )
            yield chunk({}, "tool_calls")
            yield "data: [DONE]"
            return

        if "CANCEL" in last_user:
            yield chunk({"role": "assistant", "content": "cancel pending"})
            time.sleep(1.0)
            yield chunk({}, "stop")
            yield "data: [DONE]"
            return

        yield chunk({"role": "assistant", "content": "hello from mock"})
        yield chunk({}, "stop")
        yield "data: [DONE]"


def text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return " ".join(part.get("text", "") for part in content if isinstance(part, dict))
    return ""


class Client:
    def __init__(self, binary: str, root: Path, provider: Provider) -> None:
        self.config = root / "config"
        self.data = root / "data"
        self.state = root / "state"
        self.tmp = root / "tmp"
        self.agent_dir = self.config / "agent"
        for path in (self.config, self.agent_dir, self.data, self.state, self.tmp):
            path.mkdir(parents=True, exist_ok=True)
        (self.agent_dir / "config.yml").write_text(
            "defaultModel: mock/mock-model\n"
            "providers:\n"
            "  mock:\n"
            f"    baseUrl: \"{provider.base_url}\"\n"
            "    api: openai-completions\n"
            "    apiKey: test-key\n",
            encoding="utf-8",
        )
        (self.agent_dir / "models.json").write_text(
            '{"mock":{"mock-model":{"id":"mock-model","provider":"mock","contextWindow":128000}}}\n',
            encoding="utf-8",
        )
        (self.agent_dir / "models.yml").write_text(
            "providers:\n"
            "  mock:\n"
            f"    baseUrl: \"{provider.base_url}\"\n"
            "    api: openai-completions\n"
            "    apiKey: test-key\n"
            "    models:\n"
            "      - id: mock-model\n"
            "        contextWindow: 128000\n",
            encoding="utf-8",
        )
        env = dict(os.environ)
        env.update(
            PI_CONFIG_DIR=str(self.config),
            PI_CODING_AGENT_DIR=str(self.agent_dir),
            HOME=str(root),
            XDG_DATA_HOME=str(self.data),
            XDG_STATE_HOME=str(self.state),
            TMPDIR=str(self.tmp),
            NO_COLOR="1",
            TERM="dumb",
        )
        self.cwd = root / "workspace"
        self.cwd.mkdir()
        self.proc = subprocess.Popen(
            [binary, "acp"],
            cwd=str(self.cwd),
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        self.frames: "queue.Queue[dict]" = queue.Queue()
        self.raw: list[dict] = []
        self.stderr: list[str] = []
        self.next_id = 1
        self.pending: dict[int, dict] = {}
        self.lock = threading.Lock()
        threading.Thread(target=self._read_stdout, daemon=True).start()
        threading.Thread(target=self._read_stderr, daemon=True).start()

    def _read_stderr(self) -> None:
        assert self.proc.stderr is not None
        for line in self.proc.stderr:
            self.stderr.append(line.rstrip())

    def _read_stdout(self) -> None:
        assert self.proc.stdout is not None
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                frame = json.loads(line)
            except json.JSONDecodeError:
                self.frames.put({"__bad_line__": line})
                continue
            with self.lock:
                self.raw.append(frame)
            self.frames.put(frame)

    # ---- wire helpers -------------------------------------------------

    def send(self, frame: dict) -> None:
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(frame) + "\n")
        self.proc.stdin.flush()

    def send_raw(self, line: str) -> None:
        assert self.proc.stdin is not None
        self.proc.stdin.write(line + "\n")
        self.proc.stdin.flush()

    def wait_frame(self, predicate, timeout=REQUEST_TIMEOUT):
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError(
                    f"timeout waiting for frame; raw={self.raw[-8:]!r} stderr={self.stderr[-8:]!r}"
                )
            try:
                frame = self.frames.get(timeout=min(0.25, remaining))
            except queue.Empty:
                if self.proc.poll() is not None:
                    raise AssertionError(
                        f"ACP process exited {self.proc.returncode}; stderr={self.stderr[-20:]!r}"
                    )
                continue
            if predicate(frame):
                return frame

    def request(self, method, params=None, timeout=REQUEST_TIMEOUT):
        request_id = self.next_id
        self.next_id += 1
        frame = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            frame["params"] = params
        self.send(frame)
        response = self.wait_frame(
            lambda f: f.get("id") == request_id and ("result" in f or "error" in f), timeout
        )
        if "error" in response:
            raise AssertionError(f"{method} failed: {response['error']}")
        return response.get("result", {})

    def request_error(self, method, params=None):
        request_id = self.next_id
        self.next_id += 1
        frame = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            frame["params"] = params
        self.send(frame)
        response = self.wait_frame(
            lambda f: f.get("id") == request_id and ("result" in f or "error" in f)
        )
        return response.get("error")

    def notify(self, method, params=None):
        frame = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            frame["params"] = params
        self.send(frame)

    def prompt_answering_permission(self, session_id, text, option_id="allow_once", timeout=PROMPT_TIMEOUT):
        """Send a prompt and answer any `session/request_permission` frames
        that arrive before its response. Returns (response, requests)."""
        request_id = self.next_id
        self.next_id += 1
        frame = {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "session/prompt",
            "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": text}]},
        }
        self.send(frame)
        requests = []
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError(f"prompt {text!r} timed out; raw={self.raw[-8:]!r}")
            current = self.wait_frame(lambda _f: True, remaining)
            if current.get("method") == "session/request_permission":
                requests.append(current)
                self.send(
                    {
                        "jsonrpc": "2.0",
                        "id": current["id"],
                        "result": {"outcome": {"outcome": "selected", "optionId": option_id}},
                    }
                )
                continue
            if current.get("id") == request_id and ("result" in current or "error" in current):
                if "error" in current:
                    raise AssertionError(f"prompt failed: {current['error']}")
                return current.get("result", {}), requests

    def updates_since(self, mark, session_id):
        with self.lock:
            frames = list(self.raw[mark:])
        return [
            frame["params"]["update"]
            for frame in frames
            if frame.get("method") == "session/update"
            and frame.get("params", {}).get("sessionId") == session_id
        ]

    def mark(self):
        with self.lock:
            return len(self.raw)

    def wait_exit(self, timeout=10.0):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                return self.proc.returncode
            time.sleep(0.05)
        return None

    def close_stdin(self):
        assert self.proc.stdin is not None
        try:
            self.proc.stdin.close()
        except OSError:
            pass

    def kill(self):
        if self.proc.poll() is None:
            try:
                self.proc.kill()
            except OSError:
                pass
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass


def assert_eq(actual, expected, label):
    if actual != expected:
        raise AssertionError(f"{label}: expected {expected!r}, got {actual!r}")


def run(binary: str) -> None:
    root = Path(tempfile.mkdtemp(prefix="acp-accept."))
    provider = Provider()
    provider.start()
    client = Client(binary, root, provider)
    try:
        # 1. initialize + exact capability matrix
        caps = client.request("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
        assert_eq(caps["protocolVersion"], 1, "protocolVersion")
        assert_eq(caps["agentCapabilities"]["loadSession"], True, "loadSession")
        assert_eq(caps["agentCapabilities"]["mcpCapabilities"], {"http": True, "sse": True}, "mcp caps")
        assert_eq(
            caps["agentCapabilities"]["promptCapabilities"],
            {"embeddedContext": True, "image": True},
            "prompt caps",
        )
        assert_eq(
            set(caps["agentCapabilities"]["sessionCapabilities"].keys()),
            {"list", "fork", "resume", "close"},
            "session caps",
        )
        method_ids = [method["id"] for method in caps["authMethods"]]
        assert_eq(method_ids, ["agent"], "auth methods")
        assert client.request_error("authenticate", {"methodId": "nope"})["message"].startswith(
            "Unknown ACP auth method"
        ), "unknown auth method"

        initialized_terminal = client.request(
            "initialize",
            {"protocolVersion": 1, "clientCapabilities": {"auth": {"terminal": True}}},
        )
        terminal_ids = [method["id"] for method in initialized_terminal["authMethods"]]
        assert_eq(terminal_ids, ["agent", "terminal"], "terminal auth advertised")

        cwd = str(client.cwd)
        relative = client.request_error("session/new", {"cwd": "relative"})
        assert relative is not None, "relative cwd must fail"

        # 2. session/new + bootstrap
        mark = client.mark()
        created = client.request("session/new", {"cwd": cwd, "mcpServers": []})
        session_id = created["sessionId"]
        assert session_id, "sessionId"
        assert created["configOptions"], "configOptions"
        assert [mode["id"] for mode in created["modes"]["availableModes"]] == ["default", "plan"], "modes"
        bootstrap = client.wait_frame(
            lambda f: f.get("method") == "session/update"
            and f.get("params", {}).get("sessionId") == session_id
            and f["params"]["update"].get("sessionUpdate") == "session_info_update"
        )
        assert bootstrap, "bootstrap session info"
        updates = client.updates_since(mark, session_id)
        assert any(u.get("sessionUpdate") == "available_commands_update" for u in updates), "commands bootstrap"

        # 3. plain prompt: ordered chunks before the response
        mark = client.mark()
        response = client.request("session/prompt", {"sessionId": session_id, "prompt": [{"type": "text", "text": "hello"}]}, PROMPT_TIMEOUT)
        assert_eq(response["stopReason"], "end_turn", "plain stop reason")
        updates = client.updates_since(mark, session_id)
        chunks = [u for u in updates if u.get("sessionUpdate") == "agent_message_chunk"]
        assert chunk_text(chunks) == "hello from mock", f"chunks {chunks!r}"
        assert any(u.get("sessionUpdate") == "session_info_update" for u in updates), "turn session info"

        # 4. permission: ask once, persist allow_always, no second request
        first, permission_requests = client.prompt_answering_permission(
            session_id, "PERMISSION one", "allow_always"
        )
        assert_eq(first["stopReason"], "end_turn", "permission stop")
        assert_eq(len(permission_requests), 1, "permission request count")
        request = permission_requests[0]
        options = [option["optionId"] for option in request["params"]["options"]]
        assert_eq(options, ["allow_once", "allow_always", "reject_once", "reject_always"], "permission options")
        assert_eq(request["params"]["sessionId"], session_id, "permission session")
        second, repeated = client.prompt_answering_permission(session_id, "PERMISSION two")
        assert_eq(second["stopReason"], "end_turn", "second permission stop")
        assert_eq(len(repeated), 0, "allow_always persistence")

        # 5. cancel notification: cancelled response, session still usable
        mark = client.mark()
        request_id = client.next_id
        client.next_id += 1
        client.send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "session/prompt",
                "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "CANCEL please"}]},
            }
        )
        client.wait_frame(
            lambda f: f.get("method") == "session/update"
            and f.get("params", {}).get("sessionId") == session_id
            and f["params"]["update"].get("sessionUpdate") == "agent_message_chunk",
            PROMPT_TIMEOUT,
        )
        client.notify("session/cancel", {"sessionId": session_id})
        cancelled = client.wait_frame(
            lambda f: f.get("id") == request_id and ("result" in f or "error" in f), 10.0
        )
        assert_eq(cancelled.get("result", {}).get("stopReason"), "cancelled", "cancel stop reason")
        time.sleep(0.3)
        after = client.request(
            "session/prompt",
            {"sessionId": session_id, "prompt": [{"type": "text", "text": "after cancel"}]},
            PROMPT_TIMEOUT,
        )
        assert_eq(after["stopReason"], "end_turn", "prompt after cancel")

        # 6. list paging
        listing = client.request("session/list", {"cwd": cwd})
        ids = [session["sessionId"] for session in listing["sessions"]]
        assert session_id in ids, f"session list {ids!r}"
        assert "nextCursor" not in listing or int(listing["nextCursor"]) > 0, "cursor"

        # 7. mode + config option
        client.request("session/set_mode", {"sessionId": session_id, "modeId": "plan"})
        plan_updates = [
            u
            for u in client.updates_since(mark, session_id)
            if u.get("sessionUpdate") == "current_mode_update"
        ]
        assert plan_updates, "current_mode_update"
        client.request("session/set_mode", {"sessionId": session_id, "modeId": "default"})
        boolean_error = client.request_error(
            "session/set_config_option",
            {"sessionId": session_id, "configId": "mode", "value": True},
        )
        assert "Unsupported boolean ACP config option" in boolean_error["message"], "boolean config"
        configured = client.request(
            "session/set_config_option",
            {"sessionId": session_id, "configId": "thinking", "value": "off"},
        )
        assert configured["configOptions"], "config options response"

        # 8. resume after close
        client.request("session/close", {"sessionId": session_id})
        assert_eq(client.request("session/close", {"sessionId": session_id}), {}, "close idempotent")
        resumed = client.request("session/resume", {"sessionId": session_id, "cwd": cwd})
        assert resumed["configOptions"], "resume config"
        resumed_prompt = client.request(
            "session/prompt",
            {"sessionId": session_id, "prompt": [{"type": "text", "text": "resumed"}]},
            PROMPT_TIMEOUT,
        )
        assert_eq(resumed_prompt["stopReason"], "end_turn", "resume prompt")

        # 9. fork
        forked = client.request("session/fork", {"sessionId": session_id, "cwd": cwd})
        fork_id = forked["sessionId"]
        assert fork_id and fork_id != session_id, "fork identity"
        listing = client.request("session/list", {"cwd": cwd})
        assert {session_id, fork_id} <= {session["sessionId"] for session in listing["sessions"]}, "fork listed"

        # 10. load replays history before the response
        client.request("session/close", {"sessionId": session_id})
        client.request("session/close", {"sessionId": fork_id})
        mark = client.mark()
        loaded = client.request("session/load", {"sessionId": session_id, "cwd": cwd})
        assert loaded["configOptions"], "load config"
        replayed = client.updates_since(mark, session_id)
        assert any(u.get("sessionUpdate") == "user_message_chunk" for u in replayed), "load replay user"
        assert any(u.get("sessionUpdate") == "agent_message_chunk" for u in replayed), "load replay assistant"

        # 11. malformed line + unknown method keep the connection alive
        client.send_raw("{not json")
        parse_error = client.wait_frame(lambda f: f.get("error", {}).get("code") == -32700)
        assert parse_error, "parse error"
        unknown = client.request_error("totally/unknown", {})
        assert_eq(unknown["code"], -32601, "unknown method")
        again = client.request("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
        assert_eq(again["protocolVersion"], 1, "connection alive")

        # 12. EOF cleanup
        client.close_stdin()
        code = client.wait_exit(10.0)
        assert code is not None, f"process did not exit on EOF; stderr={client.stderr[-20:]!r}"
    finally:
        client.kill()
        provider.stop()
        print(f"acp_accept artifacts retained at: {root}")


def chunk_text(chunks):
    parts = []
    for update in chunks:
        content = update.get("content") or {}
        if content.get("type") == "text":
            parts.append(content.get("text", ""))
    return "".join(parts)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", help="path to the built omp binary")
    args = parser.parse_args(argv)
    binary = str(Path(args.binary).resolve())
    if not Path(binary).is_file():
        raise SystemExit(f"binary not found: {binary}")
    run(binary)
    print("acp_accept: OK")


if __name__ == "__main__":
    main()
