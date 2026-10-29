#!/usr/bin/env python3
"""Post-failure, read-only Julia bootstrap diagnostic for the Rust agent CI.

Runs only after a failing `cargo test`, never installs anything, and never
changes the suite's verdict. Its purpose is the one measurement the paired
`julia -e exit(0)` probes cannot make: how far the *native* staged Julia runner
bootstraps on a fresh cwd and the filtered eval env before the 15s kernel
startup timer fires.

What it reproduces from the Rust host (`crates/pi-agent/src/eval/`):
  * argv of `languages::argv(Jl, ..)` and the runner staged by
    `languages::stage_runner` (the Rust staging dir is preferred; a labelled
    fallback copy is used when the suite left none behind);
  * `env::filter_env(Jl, ..)` plus the `GKSwstype=100` default that
    `Kernel::start` inserts;
  * the exact TSV `run` payload of `Kernel::build_payload` carrying
    `languages::init_script(Jl, ..)` (base64 cwd + LOAD_PATH edit), then the
    `prelude.jl` payload only once the init `done` frame arrives with
    `status="ok"` and `cancelled=false` — the same condition `Kernel::start`
    enforces;
  * a non-blocking stdin queue drained on `EVENT_WRITE` in the same selector
    that drains both output pipes (no threads), so a payload larger than the
    pipe capacity stays inside the 25s whole-experiment cap; graceful `exit`
    once startup finished or failed, SIGKILL of the whole owned process group
    otherwise, and a reap on every path.

It prints frame TYPES and IDs only (never runner output), the first 1200 bytes
of stderr plus its total size, spawn/first-frame/init-done/prelude-done
latencies, the retained init/prelude `done` status, wall/cpu/exit/signal — and
one verdict:

  no_frames                       nothing printed before the cap/exit
  started_without_done            init `started` seen, no `done` (init hang)
  init_failed                     init `done` errored, was cancelled, or had no status
  prelude_started_without_done    init finished, prelude did not
  prelude_missing                 init finished, prelude never started
  prelude_failed                  prelude `done` errored, was cancelled, or had no status
  full_ready                      init + prelude both `done` with `status="ok"`

Anything but an explicit `status="ok"` fails closed: the prelude is never sent
and `full_ready` is never reported for an errored, cancelled or status-less
`done`.

The minimal `julia -e exit(0)` probes run *after* the measurement so they
cannot warm the cache it is measuring; they are post-failure diagnostics, not a
cold-boot proof.

Usage:
  python3 scripts/julia-bootstrap-diagnostic.py
  python3 scripts/julia-bootstrap-diagnostic.py --julia /usr/bin/julia
  # exercise this protocol without Julia (fake TSV wrapper, e.g. a shell script)
  python3 scripts/julia-bootstrap-diagnostic.py --fake-cmd "python3 /tmp/fake_runner.py"
"""

import argparse
import os
import shlex
import shutil
import signal
import selectors
import subprocess
import sys
import tempfile
import time
import traceback
from pathlib import Path

TOTAL_TIMEOUT = 25.0  # whole handshake experiment
REAP_GRACE = 2.0  # after SIGKILL
SHUTDOWN_GRACE = 1.0  # mirrors languages::SHUTDOWN_GRACE
CMD_TIMEOUT = 20.0  # provenance/version/minimal probes
OUT_LIMIT = 1200  # stderr head, matches the step's previous bound
FRAME_LOG_LIMIT = 200  # recorded frames before only counting

# Deliberate diagnostic copy of crates/pi-agent/src/eval/env.rs
# (SHARED_DENY, COMMON_ALLOW + JL_ALLOW, allow_for(Jl) prefixes and
# SECRET_PATTERN). Product code stays the single source of truth; nothing
# derived from the environment is printed, only key counts.
ENV_DENY = frozenset(
    """
    OPENAI_API_KEY ANTHROPIC_API_KEY GOOGLE_API_KEY GEMINI_API_KEY OPENROUTER_API_KEY
    PERPLEXITY_API_KEY PERPLEXITY_COOKIES EXA_API_KEY AZURE_OPENAI_API_KEY MISTRAL_API_KEY
    PI_API_KEY PI_TOKEN PI_PASSWORD PI_SESSION PI_TOOL_BRIDGE_TOKEN
    """.split()
)
ENV_ALLOW = frozenset(
    """
    PATH HOME USER USERNAME LOGNAME SHELL LANG LC_ALL LC_CTYPE LC_MESSAGES TERM
    TERM_PROGRAM TERM_PROGRAM_VERSION TMPDIR TEMP TMP XDG_CACHE_HOME XDG_CONFIG_HOME
    XDG_DATA_HOME XDG_RUNTIME_DIR SSH_AUTH_SOCK SSH_AGENT_PID SSH_CONNECTION SSH_CLIENT
    SSH_TTY LD_LIBRARY_PATH DYLD_LIBRARY_PATH TZ SYSTEMROOT WINDIR COMSPEC PATHEXT
    DISPLAY XAUTHORITY
    """.split()
)
ENV_PREFIXES = ("LC_", "XDG_", "PI_", "JULIA_", "OPENBLAS_", "MKL_")
ENV_SECRET = __import__("re").compile(
    r"(?i)API[_-]?KEY|APIKEY|SECRET|TOKEN|PASSWORD|PASSWD|CREDENTIAL"
    r"|ACCESS[_-]?KEY|PRIVATE[_-]?KEY"
)

RUNNER_REL = Path("crates/pi-agent/src/eval/runners/runner.jl")
PRELUDE_REL = Path("crates/pi-agent/src/eval/runners/prelude.jl")


def jl_env(env):
    return {
        key: value
        for key, value in env.items()
        if key not in ENV_DENY
        and (key in ENV_ALLOW or (not ENV_SECRET.search(key) and key.startswith(ENV_PREFIXES)))
    }


def child_cpu():
    t = os.times()
    return t.children_user + t.children_system


def tmp_root():
    return Path(os.environ.get("TMPDIR") or tempfile.gettempdir())


def b64(text):
    import base64

    return base64.b64encode(text.encode() if isinstance(text, str) else text).decode()


def run_payload(run_id, cwd, code, env_pairs=""):
    """One TSV line exactly as kernel::build_payload formats Jl payloads."""
    return "run\t{}\t{}\t1\t0\t{}\t{}".format(run_id, b64(cwd), env_pairs, b64(code))


def init_code(cwd):
    """Diagnostic copy of languages::init_script(Jl, cwd)."""
    return (
        '__omp_init_cwd = String(Base64.base64decode("%s"))\n'
        "try cd(__omp_init_cwd) catch; end\n"
        "if !(__omp_init_cwd in LOAD_PATH); pushfirst!(LOAD_PATH, __omp_init_cwd); end"
    ) % b64(str(cwd))


def store(state, stream, chunk):
    state["total"][stream] += len(chunk)
    head = state["head"][stream]
    room = OUT_LIMIT - len(head)
    if room > 0:
        head += chunk[:room]


def residual(state, stream):
    """One non-blocking read: a survivor after SIGKILL costs at most one buffer."""
    try:
        os.set_blocking(stream.fileno(), False)
        chunk = os.read(stream.fileno(), 65536)
    except (OSError, ValueError):
        chunk = b""
    if chunk:
        store(state, stream, chunk)
    try:
        stream.close()
    except OSError:
        pass


def find_runner(repo):
    """Prefer the runner the failing suite actually staged under $TMPDIR.

    Returns (path, matches, origin, stage_dir): stage_dir owns the fallback copy
    and must be cleaned up by the caller; a staged runner found in $TMPDIR is
    never touched or removed.
    """
    src = repo / RUNNER_REL
    staged_dir = tmp_root() / "omp-jl-runner"
    try:
        wanted = src.read_bytes()
    except OSError:
        wanted = None
    candidates = []
    for path in staged_dir.glob("runner-*.jl"):
        try:
            candidates.append((path.stat().st_mtime, path))
        except OSError:
            continue
    for _, path in sorted(candidates, reverse=True):
        try:
            data = path.read_bytes()
        except OSError:
            continue
        return path, wanted is not None and data == wanted, "rust-staged", None
    if wanted is None:
        return None, False, "missing", None
    stage_dir = tempfile.TemporaryDirectory(prefix="omp-jl-stage-")
    path = Path(stage_dir.name) / "runner.jl"
    path.write_bytes(wanted)
    return path, True, "fallback-copy", stage_dir


def handshake(cmd, env, cwd, prelude, kernel_id):
    """Spawn the runner, drive init then prelude, bounded and always reaped."""
    proc = subprocess.Popen(
        cmd,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        cwd=str(cwd),
        start_new_session=True,
    )
    t0 = time.monotonic()
    cpu0 = child_cpu()
    streams = (proc.stdout, proc.stderr)
    state = {
        "streams": {"stdout": proc.stdout, "stderr": proc.stderr},
        "head": {stream: bytearray() for stream in streams},
        "total": {stream: 0 for stream in streams},
        "frames": [],
        "frame_total": 0,
        "types": set(),
        "mark": {},
        "stage": "init",
        "init_status": None,
        "init_cancelled": False,
        "prelude_status": None,
        "prelude_cancelled": False,
        "timed_out": False,
        "kill_reason": None,
        "stdin_error": None,
        "shutdown_sent": False,
    }
    init_id = "{}-init".format(kernel_id)
    prelude_id = "{}-Julia prelude".format(kernel_id)
    out_buf = bytearray()
    stage = "init"
    stdin_fd = proc.stdin.fileno()
    pending = bytearray()

    def flush_stdin():
        # Non-blocking: bytes the pipe refuses stay queued for EVENT_WRITE, so a
        # payload larger than the pipe can never stall the deadline loop.
        while pending:
            try:
                written = os.write(stdin_fd, pending[:65536])
            except BlockingIOError:
                return
            except OSError as exc:
                state["stdin_error"] = type(exc).__name__
                pending.clear()
                break
            if not written:
                state["stdin_error"] = "zero-write"
                pending.clear()
                break
            del pending[:written]
        if proc.stdin in sel.get_map():
            sel.unregister(proc.stdin)

    def send(text):
        pending.extend(text.encode() + b"\n")
        if proc.stdin not in sel.get_map():
            sel.register(proc.stdin, selectors.EVENT_WRITE)
        flush_stdin()

    def shutdown():
        nonlocal stage
        stage = "shutdown"
        send("exit")
        state["shutdown_sent"] = True
        mark("shutdown_sent")

    def mark(name):
        state["mark"].setdefault(name, time.monotonic() - t0)

    def frame(line):
        text = line.decode("utf-8", "replace")
        ftype = _field(text, "type")
        fid = _field(text, "id")
        status = _field(text, "status")
        state["frame_total"] += 1
        if ftype:
            state["types"].add(ftype)
        if len(state["frames"]) < FRAME_LOG_LIMIT:
            state["frames"].append((round(time.monotonic() - t0, 3), ftype, fid, status))
        if ftype == "started":
            mark("init_started" if fid == init_id else "prelude_started" if fid == prelude_id else "other_started")
        return ftype, fid, status

    sel = selectors.DefaultSelector()
    try:
        for stream in streams:
            os.set_blocking(stream.fileno(), False)
            sel.register(stream, selectors.EVENT_READ)
        os.set_blocking(stdin_fd, False)
        send(run_payload(init_id, ".", init_code(cwd)))
        mark("init_sent")
        deadline = t0 + TOTAL_TIMEOUT
        while True:
            if stage == "shutdown" and proc.poll() is not None:
                break
            now = time.monotonic()
            if now >= deadline:
                state["timed_out"] = True
                state["kill_reason"] = "total-{}s".format(int(TOTAL_TIMEOUT))
                break
            if stage == "shutdown" and now >= state["mark"]["shutdown_sent"] + t0 + SHUTDOWN_GRACE:
                state["kill_reason"] = "shutdown-grace"
                break
            if not any(stream in sel.get_map() for stream in streams):
                # Both pipes closed: nothing left to observe, only reaping.
                try:
                    proc.wait(timeout=min(deadline - now, 0.2))
                except subprocess.TimeoutExpired:
                    state["kill_reason"] = state["kill_reason"] or "stdio-closed"
                break
            for key, _ in sel.select(min(deadline - now, 0.1)):
                stream = key.fileobj
                if stream is proc.stdin:
                    flush_stdin()
                    continue
                try:
                    chunk = os.read(stream.fileno(), 65536)
                except BlockingIOError:
                    continue
                except OSError:
                    chunk = b""
                if not chunk:
                    sel.unregister(stream)
                    continue
                store(state, stream, chunk)
                if stream is not proc.stdout:
                    continue
                out_buf += chunk
                while b"\n" in out_buf:
                    raw, _, rest = bytes(out_buf).partition(b"\n")
                    out_buf = bytearray(rest)
                    ftype, fid, status = frame(raw)
                    cancelled = '"cancelled":true' in raw.decode("utf-8", "replace")
                    if ftype == "done" and stage == "init" and fid == init_id:
                        mark("init_done")
                        state["init_status"] = status
                        state["init_cancelled"] = cancelled
                        # Native Kernel::start needs status Ok and not cancelled;
                        # anything else aborts startup, so never send the prelude.
                        if status == "ok" and not cancelled:
                            stage = "prelude"
                            send(run_payload(prelude_id, ".", prelude))
                            mark("prelude_sent")
                        else:
                            shutdown()
                    elif ftype == "done" and stage == "prelude" and fid == prelude_id:
                        mark("prelude_done")
                        state["prelude_status"] = status
                        state["prelude_cancelled"] = cancelled
                        shutdown()
    finally:
        sel.close()
        if proc.poll() is None:
            state["kill_reason"] = state["kill_reason"] or "still-running"
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                proc.wait(timeout=REAP_GRACE)
            except subprocess.TimeoutExpired:
                print("reap-timeout: still alive {} after SIGKILL".format(REAP_GRACE), flush=True)
        for stream in streams:
            residual(state, stream)
        try:
            proc.stdin.close()
        except (OSError, ValueError):
            pass
    state["wall"] = time.monotonic() - t0
    state["cpu"] = child_cpu() - cpu0
    state["returncode"] = proc.returncode
    state["stage"] = stage
    return state


def _field(line, name):
    """Cheap key scrape: frames are large, only type/id/status are reported."""
    key = '"{}"'.format(name)
    at = line.find(key)
    if at < 0:
        return None
    at = line.find('"', line.find(":", at) + 1)
    if at < 0:
        return None
    end = line.find('"', at + 1)
    return line[at + 1 : end] if end > at else None


def verdict(state):
    mark = state["mark"]
    if not state["frame_total"]:
        return "no_frames"
    if "init_done" not in mark:
        return "started_without_done" if "init_started" in mark else "no_frames"
    # Missing status is not Ok either: fail closed like Kernel::start does.
    if state.get("init_status") != "ok" or state.get("init_cancelled"):
        return "init_failed"
    if "prelude_done" not in mark:
        return "prelude_started_without_done" if "prelude_started" in mark else "prelude_missing"
    if state.get("prelude_status") != "ok" or state.get("prelude_cancelled"):
        return "prelude_failed"
    return "full_ready"


def report(state, cmd):
    mark = state["mark"]
    print("=== bootstrap-handshake (native staged runner, filtered env, fresh cwd)", flush=True)
    print("argv={}".format(shlex.join(cmd)), flush=True)
    print("t+{:<16}{:7.3f}s".format("spawn", 0.0), flush=True)
    for name in ("init_sent", "init_started", "init_done", "prelude_sent", "prelude_started", "prelude_done", "shutdown_sent"):
        if name in mark:
            print("t+{:<16}{:7.3f}s".format(name, mark.get(name)), flush=True)
    print(
        "frames={} types={}".format(state["frame_total"], ",".join(sorted(state["types"])) or "none"),
        flush=True,
    )
    for rel, ftype, fid, status in state["frames"]:
        print("frame +{:.3f}s type={} id={}{}".format(rel, ftype, fid, " status={}".format(status) if status else ""), flush=True)
    if state["frame_total"] > len(state["frames"]):
        print("frames-log-truncated={}".format(state["frame_total"] - len(state["frames"])), flush=True)
    for stage in ("init", "prelude"):
        print(
            "{}_status={} cancelled={}".format(
                stage,
                state.get(stage + "_status") or "none",
                "yes" if state.get(stage + "_cancelled") else "no",
            ),
            flush=True,
        )
    stderr_stream = state["streams"]["stderr"]
    stderr = bytes(state["head"][stderr_stream]).decode("utf-8", "replace")
    print("stderr[{}]: {!r}".format(state["total"][stderr_stream], stderr), flush=True)
    code = state["returncode"]
    print(
        "exit={} signal={} wall={:.2f}s child_cpu={:.2f}s stage={} killed={}".format(
            code,
            -code if code is not None and code < 0 else 0,
            state["wall"],
            state["cpu"],
            state["stage"],
            state["kill_reason"] or "none",
        ),
        flush=True,
    )
    if state["stdin_error"]:
        print("stdin-error={}".format(state["stdin_error"]), flush=True)
    print("shutdown=exit-payload" if state["shutdown_sent"] else "shutdown=group-kill", flush=True)
    print("verdict={}".format(verdict(state)), flush=True)


def run_probe(label, cmd, timeout=CMD_TIMEOUT, env=None, cwd=None, null_output=False):
    """Bounded external command; same bounded-drain contract as the handshake."""
    print(f"=== {label}: {' '.join(cmd)}", flush=True)
    cpu_before = child_cpu()
    start = time.perf_counter()
    try:
        proc = subprocess.Popen(
            cmd,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL if null_output else subprocess.PIPE,
            stderr=subprocess.DEVNULL if null_output else subprocess.PIPE,
            env=env,
            cwd=cwd,
            start_new_session=True,
        )
    except OSError as exc:
        print(f"spawn-failed: {type(exc).__name__}: {exc}", flush=True)
        return
    streams = () if null_output else (proc.stdout, proc.stderr)
    state = {
        "head": {stream: bytearray() for stream in streams},
        "total": {stream: 0 for stream in streams},
    }
    live = set(streams)
    deadline = start + timeout
    timed_out = False
    sel = selectors.DefaultSelector()
    try:
        for stream in live:
            os.set_blocking(stream.fileno(), False)
            sel.register(stream, selectors.EVENT_READ)
        while live and not timed_out:
            remaining = deadline - time.perf_counter()
            if remaining <= 0:
                timed_out = True
                break
            for key, _ in sel.select(min(remaining, 0.5)):
                stream = key.fileobj
                try:
                    chunk = os.read(stream.fileno(), 65536)
                except BlockingIOError:
                    continue
                if chunk:
                    store(state, stream, chunk)
                else:
                    sel.unregister(stream)
                    live.discard(stream)
        if not timed_out:
            try:
                proc.wait(timeout=max(0.0, deadline - time.perf_counter()))
            except subprocess.TimeoutExpired:
                timed_out = True
    finally:
        sel.close()
        if timed_out or proc.poll() is None:
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                proc.wait(timeout=REAP_GRACE)
            except subprocess.TimeoutExpired:
                print("reap-timeout: still alive after SIGKILL", flush=True)
        for stream in streams:
            residual(state, stream)
    wall = time.perf_counter() - start
    print(f"exit={proc.returncode} timeout={timed_out} wall={wall:.2f}s child_cpu={child_cpu() - cpu_before:.2f}s", flush=True)
    for name, stream in (("stdout", proc.stdout), ("stderr", proc.stderr)):
        if stream not in state["total"]:
            continue
        head = bytes(state["head"][stream]).decode("utf-8", "replace")
        print(f"{name}[{state['total'][stream]}]: {head!r}", flush=True)


def safe(label, cmd, timeout=CMD_TIMEOUT, env=None, cwd=None, null_output=False):
    try:
        run_probe(label, cmd, timeout, env=env, cwd=cwd, null_output=null_output)
    except Exception:
        print(f"run-error: {label}", flush=True)
        traceback.print_exc()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo-root", default=None, help="repository root (default: script's parent)")
    parser.add_argument("--julia", default=None, help="julia executable (default: PATH lookup)")
    parser.add_argument("--fake-cmd", default=None, help="exercise the protocol with this command instead of Julia")
    args = parser.parse_args(argv)
    repo = Path(args.repo_root) if args.repo_root else Path(__file__).resolve().parent.parent

    loadavg = os.getloadavg() if hasattr(os, "getloadavg") else "unavailable"
    print(f"cpu_count={os.cpu_count()} loadavg={loadavg}", flush=True)

    env = jl_env(dict(os.environ))
    env.setdefault("GKSwstype", "100")  # inserted by Kernel::start for Jl
    print(
        "env=filtered(Jl)+GKSwstype keys={} (bridge extra_env not reproduced)".format(len(env)),
        flush=True,
    )

    prelude_path = repo / PRELUDE_REL
    prelude = prelude_path.read_text() if prelude_path.is_file() else ""
    kernel_id = "diag-{}".format(os.getpid())

    if args.fake_cmd:
        cmd = shlex.split(args.fake_cmd)
        print("mode=fake-cmd runner=external", flush=True)
        with tempfile.TemporaryDirectory(prefix="omp-jl-diag-") as cwd:
            report(handshake(cmd, env, cwd, prelude, kernel_id), cmd)
        return 0

    julia = args.julia or shutil.which("julia")
    if not julia:
        print(
            "julia: not on PATH — bootstrap diagnostic skipped; prerequisite is Julia on the "
            "runner image and this step never installs it",
            flush=True,
        )
        return 0
    real = os.path.realpath(julia)
    try:
        st = os.stat(real)
        print(f"julia_path={julia} realpath={real} size={st.st_size} mtime={int(st.st_mtime)}", flush=True)
    except OSError:
        print(f"julia_path={julia} realpath=unresolved", flush=True)
    safe("provenance", ["dpkg-query", "-S", real], 10)

    runner, matches, origin, stage_dir = find_runner(repo)
    try:
        if runner is None:
            print("runner: crates/pi-agent/src/eval/runners/runner.jl missing; handshake skipped", flush=True)
            return 0
        print(
            "runner={} origin={} content_matches_repo={}".format(runner, origin, "yes" if matches else "no"),
            flush=True,
        )

        cmd = [
            julia,
            "--startup-file=no",
            "--history-file=no",
            "--color=no",
            "--project=@.",
            str(runner),
        ]
        with tempfile.TemporaryDirectory(prefix="omp-jl-diag-") as cwd:
            print("cache=cold-relative-to-this-step (no julia run before the measurement)", flush=True)
            report(handshake(cmd, env, cwd, prelude, kernel_id), cmd)

            # The paired probes stay as provenance evidence, but run only now: they
            # warm the sysimage, so the measurement above is not a cold-boot proof.
            print("cache-note=minimal julia probes below run after the measurement and warm its cache", flush=True)
            probe_argv = [julia, "--startup-file=no", "--history-file=no", "--color=no", "-e", "exit(0)"]
            safe("version", [julia, "--startup-file=no", "--history-file=no", "--color=no", "--version"])
            safe("probe-inherited-env", probe_argv, cwd=cwd, null_output=True)
            safe("probe-filtered-env", probe_argv, env=env, cwd=cwd, null_output=True)
    finally:
        # Only the fallback copy is ours; a staged runner under $TMPDIR stays.
        if stage_dir is not None:
            stage_dir.cleanup()
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except SystemExit:
        raise
    except Exception:  # script bug must never mask the suite's verdict
        traceback.print_exc()
        sys.exit(0)
