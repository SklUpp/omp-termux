#!/usr/bin/env python3
"""Drive real `omp repl` sessions under a PTY and exercise `ask` input edges.

Usage:
    python3 scripts/ask-pty-smoke.py target/debug/omp

Each scenario starts a local OpenAI-compatible SSE fixture and a fresh REPL:

1. happy path — answer the recommended option and assert the model saw it;
2. partial line times out — an abandoned `999` (no Enter) must not become the
   next REPL message;
3. UTF-8 note near the deadline — a complete `n` note sent just before the
   main-prompt deadline keeps every codepoint and still records the selection;
4. cancellation — Ctrl-C with a partial line typed aborts the whole turn,
   recovers the REPL prompt, and does not steal the next message;
5. async delegation — root spawns a delayed child, receives the job id
   immediately, consumes exactly one final envelope via `hub jobs`, and prints
   its own completion before `/exit`.

Prints the captured transcript per scenario and exits non-zero on mismatch.
"""

import fcntl
import json
import os
import pty
import select
import termios
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

QUESTIONS = {
    "questions": [
        {
            "id": "color",
            "question": "Pick a color",
            "options": [
                {"label": "Red", "description": "warm"},
                {"label": "Blue", "description": "cool", "preview": "blue preview"},
            ],
            "recommended": 1,
        }
    ]
}


def tool_call_events(call_id: str, name: str, arguments: dict) -> str:
    return chunk(
        {
            "role": "assistant",
            "tool_calls": [
                {
                    "index": 0,
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": json.dumps(arguments)},
                }
            ],
        }
    )


def chunk(delta=None, finish=None):
    payload = {
        "id": "chatcmpl-smoke",
        "object": "chat.completion.chunk",
        "model": "mock-model",
        "choices": [{"index": 0, "delta": delta or {}, "finish_reason": finish}],
    }
    return f"data: {json.dumps(payload)}\n\n"


def ask_events():
    return "".join(
        [
            chunk(
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_ask",
                            "type": "function",
                            "function": {
                                "name": "ask",
                                "arguments": json.dumps(QUESTIONS),
                            },
                        }
                    ],
                }
            ),
        ]
    )


def final_events(text):
    return "".join([chunk({"content": text}), chunk(finish="stop")])


class Fixture(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    bodies: list[dict] = []
    responses: list = []

    def do_POST(self):  # noqa: N802 - http.server API
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        try:
            self.bodies.append(json.loads(body))
        except json.JSONDecodeError:
            self.bodies.append({"raw": body.decode(errors="replace")})
        index = min(len(self.bodies) - 1, len(self.responses) - 1)
        handler = self.responses[index]
        try:
            events = handler(self.bodies[-1])
        except TypeError:
            events = handler()
        payload = ("".join(events) + "data: [DONE]\n\n").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)
        self.wfile.flush()

    def log_message(self, *args):  # silence
        pass


def write_config(agent_dir: Path, port: int, ask_timeout_seconds: float | None) -> None:
    ask_block = ""
    if ask_timeout_seconds is not None:
        ask_block = f"ask:\n  timeout: {ask_timeout_seconds}\n"
    (agent_dir / "config.yml").write_text(
        "defaultModel: mock/mock-model\n"
        "approvalMode: yolo\n"
        f"{ask_block}"
        "providers:\n"
        "  mock:\n"
        f"    baseUrl: \"http://127.0.0.1:{port}/v1\"\n"
        "    api: openai-completions\n"
        "    apiKey: test-key\n",
        encoding="utf-8",
    )
    (agent_dir / "models.yml").write_text(
        "providers:\n"
        "  mock:\n"
        f"    baseUrl: \"http://127.0.0.1:{port}/v1\"\n"
        "    api: openai-completions\n"
        "    apiKey: test-key\n"
        "    models:\n"
        "      - id: mock-model\n"
        "        contextWindow: 128000\n",
        encoding="utf-8",
    )


ACTIVE: "Session | None" = None


class Session:
    def __init__(self, binary: str, responses: list, ask_timeout_seconds=None):
        global ACTIVE
        ACTIVE = self
        Fixture.bodies = []
        Fixture.responses = responses
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.tmp = tempfile.TemporaryDirectory(prefix="omp-ask-pty-")
        root = Path(self.tmp.name)
        agent_dir = root / "agent"
        agent_dir.mkdir()
        write_config(agent_dir, self.server.server_address[1], ask_timeout_seconds)
        command = [binary, "--cwd", str(root), "--no-session", "--no-skills", "repl"]
        self.master, slave = pty.openpty()

        def child_controlling_tty() -> None:
            # Without a session and controlling terminal the line discipline
            # cannot deliver SIGINT: writing b"\x03" would just be discarded by
            # ISIG (or queued as a literal byte), and the ask prompt would time
            # out instead of cancelling. Make the slave the child's controlling
            # terminal so ^C behaves like a real terminal.
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        self.proc = subprocess.Popen(
            command,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            close_fds=True,
            cwd=str(root),
            preexec_fn=child_controlling_tty,
            env={
                **os.environ,
                "NO_COLOR": "1",
                "TERM": "dumb",
                "PI_CODING_AGENT_DIR": str(agent_dir),
            },
        )
        os.close(slave)
        self.transcript = bytearray()

    def mark(self) -> int:
        return len(self.transcript)

    def read_until(self, marker: str, since: int = 0, timeout: float = 30.0) -> bool:
        deadline = time.time() + timeout
        needle = marker.encode()
        while time.time() < deadline:
            if self.transcript.find(needle, since) >= 0:
                return True
            ready, _, _ = select.select([self.master], [], [], 0.2)
            if not ready:
                continue
            try:
                data = os.read(self.master, 8192)
            except OSError:
                return self.transcript.find(needle, since) >= 0
            if not data:
                return self.transcript.find(needle, since) >= 0
            self.transcript.extend(data)
        return self.transcript.find(needle, since) >= 0

    def send(self, data: str | bytes) -> None:
        os.write(self.master, data.encode() if isinstance(data, str) else data)

    def text(self) -> str:
        return self.transcript.decode(errors="replace")

    def last_user_content(self, body: dict) -> str | None:
        for message in reversed(body.get("messages", [])):
            if message.get("role") == "user":
                return message.get("content")
        return None

    def close(self) -> int:
        if getattr(self, "closed", False):
            return getattr(self, "exit_code", 1)
        self.closed = True
        self.send("/exit\n")
        try:
            self.exit_code = self.proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.exit_code = self.proc.wait()
        os.close(self.master)
        self.server.shutdown()
        self.tmp.cleanup()
        return self.exit_code

    def debug_dump(self) -> str:
        transcript = self.text()[-4000:]
        requests = []
        for index, body in enumerate(Fixture.bodies):
            messages = body.get("messages", [])
            requests.append(
                {
                    "index": index,
                    "roles": [m.get("role") for m in messages],
                    "last_user": (self.last_user_content(body) or "")[:300],
                    "tool_names": [
                        call.get("function", {}).get("name")
                        for m in messages
                        if m.get("role") == "assistant"
                        for call in (m.get("tool_calls") or [])
                    ],
                    "tail": [
                        {
                            "role": m.get("role"),
                            "content": (m.get("content") or "")[:800],
                        }
                        for m in messages[-3:]
                    ],
                }
            )
        alive = self.proc.poll() if hasattr(self, "proc") else None
        return (
            f"requests={json.dumps(requests, ensure_ascii=False)}\n"
            f"exit={alive}\n--- last transcript ({len(transcript)} chars) ---\n{transcript}"
        )

    def abort(self) -> None:
        if getattr(self, "closed", False):
            return
        self.closed = True
        self.proc.kill()
        self.proc.wait()
        os.close(self.master)
        self.server.shutdown()
        self.tmp.cleanup()


def scenario_happy(binary: str) -> int:
    session = Session(binary, [ask_events, lambda: final_events("ASK_OK")])
    try:
        if not session.read_until("> "):
            return fail("REPL prompt did not appear")
        mark = session.mark()
        session.send("hi\n")
        if not session.read_until("Enter a number", mark):
            return fail("ask UI did not appear")
        ui = session.text()
        for needle in ("Blue (Recommended)", "cool", "blue preview"):
            if needle not in ui:
                return fail(f"missing {needle!r}")
        mark = session.mark()
        session.send("2\n")
        if not session.read_until("ASK_OK", mark):
            return fail("final answer did not arrive")
        code = session.close()
        if code != 0:
            return fail(f"omp exited {code}")
        if len(Fixture.bodies) != 2:
            return fail(f"expected 2 model requests, saw {len(Fixture.bodies)}")
        if "User selected: Blue" not in json.dumps(Fixture.bodies[1]):
            return fail("model did not receive the selected answer")
        print("PASS: happy path")
        return 0
    finally:
        session.abort()


def scenario_partial_timeout(binary: str) -> int:
    session = Session(binary, [ask_events, lambda: final_events("ASK_TIMEOUT_OK"), lambda: final_events("SECOND_OK")], ask_timeout_seconds=1)
    try:
        if not session.read_until("> "):
            return fail("REPL prompt did not appear")
        mark = session.mark()
        session.send("hi\n")
        if not session.read_until("Enter a number", mark):
            return fail("ask UI did not appear")
        # Type without Enter: the canonical line is abandoned when the prompt
        # times out and must be flushed, not replayed into the next prompt.
        session.send("999")
        mark = session.mark()
        if not session.read_until("ASK_TIMEOUT_OK", mark, timeout=15):
            return fail("ask timeout result did not arrive")
        mark = session.mark()
        session.send("second\n")
        if not session.read_until("SECOND_OK", mark):
            return fail("next model turn did not run")
        code = session.close()
        if code != 0:
            return fail(f"omp exited {code}")
        third = Fixture.bodies[2]
        content = session.last_user_content(third)
        if content != "second":
            return fail(f"abandoned partial input stole the prompt: {content!r}")
        if "999" in json.dumps(third):
            return fail("abandoned partial line leaked into the model request")
        print("PASS: partial line timeout keeps the next prompt clean")
        return 0
    finally:
        session.abort()


def scenario_utf8_deadline(binary: str) -> int:
    session = Session(binary, [ask_events, lambda: final_events("UTF8_OK")], ask_timeout_seconds=1)
    try:
        if not session.read_until("> "):
            return fail("REPL prompt did not appear")
        mark = session.mark()
        session.send("hi\n")
        if not session.read_until("Enter a number", mark):
            return fail("ask UI did not appear")
        # A note is attached from the main prompt and shares its deadline, so
        # submitting `n <text>` just before the deadline exercises the real
        # timeout path (unlike custom input, which has no deadline).
        time.sleep(0.6)
        mark = session.mark()
        session.send("n Café ✓\n")
        # Answer the option immediately; the note must survive the re-render.
        session.send("1\n")
        if not session.read_until("UTF8_OK", mark):
            return fail("note answer did not reach the model")
        code = session.close()
        if code != 0:
            return fail(f"omp exited {code}")
        body = json.dumps(Fixture.bodies[1], ensure_ascii=False)
        if "User added note: Café ✓" not in body:
            return fail("UTF-8 note was split or dropped")
        if "Red" not in body:
            return fail("selection was lost when the note was submitted")
        print("PASS: UTF-8 note survives the main-prompt deadline")
        return 0
    finally:
        session.abort()


def scenario_cancel_recovery(binary: str) -> int:
    session = Session(binary, [ask_events, lambda: final_events("AFTER_CANCEL_OK")], ask_timeout_seconds=10)
    try:
        if not session.read_until("> "):
            return fail("REPL prompt did not appear")
        mark = session.mark()
        session.send("hi\n")
        if not session.read_until("Enter a number", mark):
            return fail("ask UI did not appear")
        session.send("77")
        time.sleep(0.1)
        mark = session.mark()
        session.send(b"\x03")  # Ctrl-C cancels the turn while a partial line is queued.
        if not session.read_until("> ", mark):
            return fail("REPL prompt did not return after cancellation")
        mark = session.mark()
        session.send("after\n")
        if not session.read_until("AFTER_CANCEL_OK", mark):
            return fail("post-cancellation turn did not run")
        code = session.close()
        if code != 0:
            return fail(f"omp exited {code}")
        if len(Fixture.bodies) != 2:
            return fail(f"expected 2 model requests, saw {len(Fixture.bodies)}")
        content = session.last_user_content(Fixture.bodies[1])
        if content != "after":
            return fail(f"cancelled partial input stole the next prompt: {content!r}")
        print("PASS: cancellation recovers without stealing the next prompt")
        return 0
    finally:
        session.abort()


def async_dispatch(body: dict) -> str:
    messages = body.get("messages", [])
    last = messages[-1] if messages else {}
    content = last.get("content") or ""
    tool_contents = [m.get("content") or "" for m in messages if m.get("role") == "tool"]
    if any('"op":"jobs"' in text or '"op": "jobs"' in text for text in tool_contents):
        return final_events("ASYNC_ROOT_DONE")
    if last.get("role") == "tool" and "Spawned agent" in content:
        # Hold the root request while the child settles so `hub jobs` sees a
        # completed job and consumes its one final envelope.
        time.sleep(1.5)
        return tool_call_events("call_hub", "hub", {"op": "jobs"})
    if last.get("role") == "user" and "produce child result" in content:
        time.sleep(1.2)
        return tool_call_events("call_yield", "yield", {"result": {"data": "child-output"}})
    if last.get("role") == "user" and "start async" in content:
        return tool_call_events(
            "call_task",
            "task",
            {"name": "slow-child", "task": "produce child result"},
        )
    return final_events("UNEXPECTED")


def scenario_async_delegation(binary: str) -> int:
    session = Session(binary, [async_dispatch])
    try:
        if not session.read_until("> "):
            return fail("REPL prompt did not appear")
        mark = session.mark()
        session.send("start async\n")
        if not session.read_until("ASYNC_ROOT_DONE", mark, timeout=45):
            return fail("async root completion did not arrive")
        code = session.close()
        if code != 0:
            return fail(f"omp exited {code}")
        bodies = Fixture.bodies
        if len(bodies) != 4:
            return fail(f"expected 4 model requests, saw {len(bodies)}")
        receipt = next(
            (
                b
                for b in bodies
                if any(
                    m.get("role") == "tool" and "Spawned agent" in (m.get("content") or "")
                    for m in b["messages"]
                )
            ),
            None,
        )
        if receipt is None:
            return fail("root never received the immediate spawn receipt")
        receipt_blob = json.dumps(receipt, ensure_ascii=False)
        if "job `task-" not in receipt_blob:
            return fail("spawn receipt did not expose the immediate job id")
        child_requests = [
            b
            for b in bodies
            if b["messages"]
            and b["messages"][-1].get("role") == "user"
            and b["messages"][-1].get("content") == "produce child result"
        ]
        if len(child_requests) != 1:
            return fail(f"expected one child request, saw {len(child_requests)}")
        jobs_result = next(
            (
                b
                for b in bodies
                if any(
                    m.get("role") == "tool" and '"op":"jobs"' in (m.get("content") or "")
                    for m in b["messages"]
                )
            ),
            None,
        )
        if jobs_result is None:
            return fail("hub jobs never consumed the settled result")
        jobs_blob = json.dumps(jobs_result, ensure_ascii=False)
        if "child-output" not in jobs_blob:
            return fail("jobs snapshot did not carry the child envelope")
        final_blob = json.dumps(bodies[-1], ensure_ascii=False)
        if final_blob.count("child-output") != 1:
            return fail("final envelope was delivered more or less than once")
        if final_blob.count("<task-result id=") != 1:
            return fail("duplicate background envelope reached the root")
        print("PASS: async delegation delivers one envelope and immediate ids")
        return 0
    finally:
        session.abort()


def fail(message: str) -> int:
    print(f"FAIL: {message}")
    if ACTIVE is not None:
        print(ACTIVE.debug_dump())
    return 1


def main() -> int:
    if len(sys.argv) not in (2, 3):
        print(__doc__)
        return 2
    binary = str(Path(sys.argv[1]).resolve())
    only = sys.argv[2] if len(sys.argv) > 2 else None
    for scenario in (
        scenario_happy,
        scenario_partial_timeout,
        scenario_utf8_deadline,
        scenario_cancel_recovery,
        scenario_async_delegation,
    ):
        if only is not None and only not in scenario.__name__:
            continue
        code = scenario(binary)
        if code != 0:
            return code
    print("PASS: all ask PTY scenarios")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
