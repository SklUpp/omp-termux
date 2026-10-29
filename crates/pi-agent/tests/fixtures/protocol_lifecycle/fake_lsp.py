#!/usr/bin/env python3
"""Minimal Content-Length JSON-RPC language server used by protocol tests.

Every inbound and outbound message is appended to the JSONL log passed as the
first argument, so tests assert the real wire conversation rather than an
internal mock. `test/delayed` answers out of order to exercise request
correlation, `test/hang` never answers, and `test/crash` exits mid-request.
"""

import json
import os
import sys
import threading
import time

LOG_PATH = sys.argv[1]
_log_lock = threading.Lock()
_write_lock = threading.Lock()


def log(direction, message):
    with _log_lock:
        with open(LOG_PATH, "a", encoding="utf-8") as handle:
            handle.write(json.dumps({"dir": direction, "msg": message}, separators=(",", ":")) + "\n")


def send(message):
    payload = json.dumps(message, separators=(",", ":")).encode("utf-8")
    frame = b"Content-Length: " + str(len(payload)).encode("ascii") + b"\r\n\r\n" + payload
    with _write_lock:
        sys.stdout.buffer.write(frame)
        sys.stdout.buffer.flush()
    log("out", message)


def respond(request, result):
    send({"jsonrpc": "2.0", "id": request.get("id"), "result": result})


def notify(method, params):
    send({"jsonrpc": "2.0", "method": method, "params": params})


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


def publish(uri, version, messages):
    notify(
        "textDocument/publishDiagnostics",
        {
            "uri": uri,
            "version": version,
            "diagnostics": [
                {
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1},
                    },
                    "severity": 1,
                    "message": message,
                }
                for message in messages
            ],
        },
    )


def delayed(request):
    params = request.get("params") or {}
    time.sleep(float(params.get("delayMs", 0)) / 1000.0)
    respond(request, {"echo": params.get("n")})


def main():
    log("argv", {"argv": sys.argv})
    while True:
        message = read_message()
        if message is None:
            return
        log("in", message)
        method = message.get("method")
        params = message.get("params") or {}
        if method == "initialize":
            respond(message, {"capabilities": {"hoverProvider": True, "definitionProvider": True}})
        elif method == "test/delayed":
            threading.Thread(target=delayed, args=(message,), daemon=True).start()
        elif method == "test/hang":
            continue
        elif method == "test/crash":
            os._exit(3)
        elif method == "textDocument/hover":
            position = params.get("position") or {}
            respond(
                message,
                {
                    "contents": {
                        "kind": "markdown",
                        "value": "fixture hover at line %s" % position.get("line", 0),
                    }
                },
            )
        elif method == "textDocument/rename":
            document = params.get("textDocument") or {}
            respond(
                message,
                {
                    "changes": {
                        document.get("uri", ""): [
                            {
                                "range": {
                                    "start": {"line": 0, "character": 0},
                                    "end": {"line": 0, "character": 5},
                                },
                                "newText": params.get("newName", ""),
                            }
                        ]
                    }
                },
            )
        elif method in ("textDocument/didOpen", "textDocument/didChange"):
            document = params.get("textDocument") or {}
            version = document.get("version", 1)
            publish(document.get("uri", ""), version, ["seen-v%s" % version])
        elif method == "textDocument/didClose":
            document = params.get("textDocument") or {}
            publish(document.get("uri", ""), None, [])
        elif method == "shutdown":
            respond(message, None)
        elif method == "exit":
            return
        elif message.get("id") is not None:
            send(
                {
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "error": {"code": -32601, "message": "method not found"},
                }
            )


if __name__ == "__main__":
    main()
