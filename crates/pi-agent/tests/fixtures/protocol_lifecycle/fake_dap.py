#!/usr/bin/env python3
"""Minimal Content-Length DAP adapter used by protocol tests.

The first argument is a JSONL log. The adapter answers the launch handshake
only after `configurationDone`, emits a `stopped` event, and serves the
inspection requests the tool exposes. When the launch program is `reverse` it
also sends the two reverse requests the agent must handle. `die` exits with a
non-zero status while a request is pending.
"""

import base64
import json
import os
import sys
import threading
import time

LOG_PATH = sys.argv[1]
_log_lock = threading.Lock()
_write_lock = threading.Lock()
_seq_lock = threading.Lock()
_next_seq = 1000


def log(direction, message):
    with _log_lock:
        with open(LOG_PATH, "a", encoding="utf-8") as handle:
            handle.write(json.dumps({"dir": direction, "msg": message}, separators=(",", ":")) + "\n")


def next_seq():
    global _next_seq
    with _seq_lock:
        _next_seq += 1
        return _next_seq


def send(message):
    payload = json.dumps(message, separators=(",", ":")).encode("utf-8")
    frame = b"Content-Length: " + str(len(payload)).encode("ascii") + b"\r\n\r\n" + payload
    with _write_lock:
        sys.stdout.buffer.write(frame)
        sys.stdout.buffer.flush()
    log("out", message)


def respond(request, body=None, success=True, message=None):
    response = {
        "seq": next_seq(),
        "type": "response",
        "request_seq": request["seq"],
        "success": success,
        "command": request.get("command", ""),
    }
    if body is not None:
        response["body"] = body
    if message is not None:
        response["message"] = message
    send(response)


def event(name, body):
    send({"seq": next_seq(), "type": "event", "event": name, "body": body})


def reverse(command, arguments):
    send({"seq": next_seq(), "type": "request", "command": command, "arguments": arguments})


def read_message():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        name, _, value = line.decode("utf-8").partition(":")
        headers[name.strip().lower()] = value.strip()
    length = int(headers.get("content-length", "0"))
    body = b""
    while len(body) < length:
        chunk = sys.stdin.buffer.read(length - len(body))
        if not chunk:
            return None
        body += chunk
    return json.loads(body.decode("utf-8"))


def run_in_terminal_arguments():
    code = (
        "import os,sys;"
        "sys.stdout.write(os.environ.get('TERM','')+'|'+os.environ.get('CI',''));"
        "sys.stdout.flush()"
    )
    return {
        "kind": "integrated",
        "title": "fixture terminal",
        "cwd": os.getcwd(),
        "args": [sys.executable, "-c", code],
    }


def main():
    log("argv", {"argv": sys.argv})
    adapter_id = ""
    pending_launch = None
    while True:
        message = read_message()
        if message is None:
            return
        log("in", message)
        if message.get("type") == "response":
            continue
        if message.get("type") != "request":
            continue
        command = message.get("command", "")
        arguments = message.get("arguments") or {}
        if command == "initialize":
            adapter_id = str(arguments.get("adapterID", ""))
            respond(
                message,
                {
                    "supportsConfigurationDoneRequest": True,
                    "supportsTerminateRequest": True,
                    "supportsMemoryReferences": True,
                },
            )
        elif command in ("launch", "attach"):
            if adapter_id.endswith("skip"):
                continue
            event("initialized", {})
            if command == "launch" and arguments.get("program") == "reverse":
                reverse("runInTerminal", run_in_terminal_arguments())
                reverse(
                    "startDebugging",
                    {"request": "launch", "configuration": {"program": "child"}},
                )
            pending_launch = message
        elif command == "configurationDone":
            respond(message, {})
            if pending_launch is not None:
                respond(pending_launch, {})
                pending_launch = None
            event("stopped", {"reason": "entry", "threadId": 7, "allThreadsStopped": True})
        elif command == "threads":
            respond(message, {"threads": [{"id": 7, "name": "main"}]})
        elif command == "stackTrace":
            respond(
                message,
                {
                    "stackFrames": [
                        {
                            "id": 11,
                            "name": "main",
                            "source": {"name": "main.py", "path": "/tmp/main.py"},
                            "line": 3,
                            "column": 1,
                        }
                    ],
                    "totalFrames": 1,
                },
            )
        elif command == "scopes":
            respond(
                message,
                {
                    "scopes": [
                        {
                            "name": "Locals",
                            "presentationHint": "locals",
                            "variablesReference": 21,
                            "expensive": False,
                        }
                    ]
                },
            )
        elif command == "variables":
            respond(
                message,
                {
                    "variables": [
                        {"name": "answer", "value": "42", "type": "int", "variablesReference": 0}
                    ]
                },
            )
        elif command == "evaluate":
            respond(message, {"result": "43", "type": "int", "variablesReference": 0})
        elif command == "readMemory":
            respond(
                message,
                {
                    "address": "0x1000",
                    "data": base64.b64encode(b"\x01\x02\x03\x04").decode("ascii"),
                    "unreadableBytes": 0,
                },
            )
        elif command == "custom/echo":
            respond(message, {"marker": "custom", "echoed": arguments})
        elif command == "terminate":
            respond(message, {})
        elif command == "disconnect":
            respond(message, {})
            time.sleep(0.05)
            return
        elif command == "die":
            os._exit(7)
        else:
            respond(message, success=False, message="unsupported fixture command: %s" % command)


if __name__ == "__main__":
    main()
