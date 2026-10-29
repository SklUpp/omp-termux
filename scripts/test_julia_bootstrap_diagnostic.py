#!/usr/bin/env python3
"""Stdlib self-check for scripts/julia-bootstrap-diagnostic.py.

Run: python3 scripts/test_julia_bootstrap_diagnostic.py

Uses a fake TSV runner (never Julia) and a short TOTAL_TIMEOUT override, so it
covers the fail-closed verdict, the bounded stdin contract on a 8 KiB pipe, and
the fallback stage-dir lifetime.
"""

import importlib.util
import os
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

try:
    import fcntl
except ImportError:  # non-POSIX: the pipe-size case skips itself
    fcntl = None

SCRIPT = Path(__file__).resolve().parent / "julia-bootstrap-diagnostic.py"
_spec = importlib.util.spec_from_file_location("julia_bootstrap_diagnostic", SCRIPT)
jbd = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(jbd)

# Mirrors runner.jl: TSV in, compact JSON frames out, "exit" ends the loop.
FAKE_RUNNER = textwrap.dedent(
    """
    import fcntl, json, os, sys, time

    mode = os.environ.get("FAKE_MODE", "ok")

    def emit(**frame):
        sys.stdout.write(json.dumps(frame, separators=(",", ":")) + "\\n")
        sys.stdout.flush()

    if mode == "stall-small":
        fcntl.fcntl(0, getattr(fcntl, "F_SETPIPE_SZ", 1031), 8192)

    stdin = sys.stdin.buffer
    while True:
        line = stdin.readline()
        if not line:
            break
        parts = line.decode().rstrip("\\n").split("\\t")
        if parts[0] == "exit":
            break
        rid = parts[1]
        emit(type="started", id=rid)
        failed = (mode == "init_error" and "-init" in rid) or (
            mode == "prelude_error" and "prelude" in rid
        )
        if failed:
            emit(type="error", id=rid, ename="ErrorException", evalue="boom")
        frame = {
            "type": "done",
            "id": rid,
            "executionCount": 1,
            "cancelled": mode == "cancelled",
        }
        if mode != "no_status":
            frame["status"] = "error" if failed else "ok"
        emit(**frame)
        if mode == "stall-small":
            # Read the init, then hold the pipe open without draining it.
            time.sleep(60)
    """
)


class HandshakeTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="jbd-check-")
        self.dir = Path(self.tmp.name)
        self.runner = self.dir / "fake_runner.py"
        self.runner.write_text(FAKE_RUNNER)
        self.old_timeout = jbd.TOTAL_TIMEOUT

    def tearDown(self):
        jbd.TOTAL_TIMEOUT = self.old_timeout
        self.tmp.cleanup()

    def run_fake(self, mode, prelude="prelude-ok", timeout=5.0):
        jbd.TOTAL_TIMEOUT = timeout
        env = dict(os.environ)
        env["FAKE_MODE"] = mode
        cmd = [sys.executable, str(self.runner)]
        with tempfile.TemporaryDirectory(prefix="jbd-cwd-") as cwd:
            state = jbd.handshake(cmd, env, cwd, prelude, "check")
        self.assertIsNotNone(state["returncode"], "child was never reaped")
        return state

    def assert_reaped(self):
        # handshake must wait() every child; a leftover (zombie or running) shows up here.
        with self.assertRaises(ChildProcessError):
            os.waitpid(-1, os.WNOHANG)

    def test_happy_path_reports_full_ready(self):
        state = self.run_fake("ok")
        self.assertEqual(jbd.verdict(state), "full_ready")
        self.assertEqual(state["init_status"], "ok")
        self.assertEqual(state["prelude_status"], "ok")
        self.assertTrue(state["shutdown_sent"])
        self.assertEqual(state["returncode"], 0)
        self.assertFalse(state["timed_out"])
        self.assertIn("prelude_sent", state["mark"])
        self.assert_reaped()

    def test_init_error_fails_closed_without_prelude(self):
        state = self.run_fake("init_error")
        self.assertEqual(jbd.verdict(state), "init_failed")
        self.assertEqual(state["init_status"], "error")
        self.assertNotIn("prelude_sent", state["mark"])
        self.assertTrue(state["shutdown_sent"])
        self.assertEqual(state["returncode"], 0)
        self.assertFalse(state["timed_out"])
        self.assert_reaped()

    def test_prelude_error_fails_closed(self):
        state = self.run_fake("prelude_error")
        self.assertEqual(jbd.verdict(state), "prelude_failed")
        self.assertEqual(state["prelude_status"], "error")
        self.assertIn("prelude_sent", state["mark"])
        self.assertTrue(state["shutdown_sent"])
        self.assertEqual(state["returncode"], 0)
        self.assert_reaped()

    def test_missing_done_status_fails_closed(self):
        state = self.run_fake("no_status")
        self.assertIsNone(state["init_status"])
        self.assertEqual(jbd.verdict(state), "init_failed")
        self.assertNotIn("prelude_sent", state["mark"])
        self.assert_reaped()

    def test_cancelled_done_fails_closed(self):
        state = self.run_fake("cancelled")
        self.assertTrue(state["init_cancelled"])
        self.assertEqual(jbd.verdict(state), "init_failed")
        self.assertNotIn("prelude_sent", state["mark"])
        self.assert_reaped()

    def test_prelude_larger_than_pipe_stays_bounded(self):
        if fcntl is None:
            self.skipTest("fcntl unavailable")
        read_fd, write_fd = os.pipe()
        try:
            fcntl.fcntl(read_fd, fcntl.F_SETPIPE_SZ, 8192)
        except OSError as exc:
            self.skipTest("F_SETPIPE_SZ unsupported: {}".format(exc))
        finally:
            os.close(read_fd)
            os.close(write_fd)

        # The child shrinks its stdin to 8 KiB, reads init, then stops reading;
        # the ~16 KiB prelude must wait in the selector, not in a blocking write.
        state = self.run_fake("stall-small", prelude="x" * 12000, timeout=1.5)
        self.assertTrue(state["timed_out"])
        self.assertEqual(state["returncode"], -9, "child survived the deadline")
        self.assertEqual(jbd.verdict(state), "prelude_missing")
        self.assertIsNone(state["stdin_error"])
        self.assertLess(state["wall"], 1.5 + jbd.REAP_GRACE + 1.5)
        self.assert_reaped()

    def _repo_with_runner(self):
        repo = self.dir / "repo"
        src = repo / jbd.RUNNER_REL
        src.parent.mkdir(parents=True)
        src.write_text("# runner\n")
        return repo, src

    def _with_tmpdir(self):
        tmpdir = self.dir / "tmp"
        tmpdir.mkdir()
        return tmpdir

    def test_find_runner_fallback_is_cleaned_up(self):
        repo, src = self._repo_with_runner()
        tmpdir = self._with_tmpdir()
        old = os.environ.get("TMPDIR")
        os.environ["TMPDIR"] = str(tmpdir)
        # tempfile.gettempdir() is cached, so the fallback copy may land outside
        # the patched TMPDIR: assert ownership + cleanup, not its parent path.
        before = set(Path(tempfile.gettempdir()).glob("omp-jl-stage-*"))
        stage_dir = None
        try:
            path, matches, origin, stage_dir = jbd.find_runner(repo)
            self.assertEqual(origin, "fallback-copy")
            self.assertTrue(matches)
            self.assertIsNotNone(stage_dir, "fallback copy must be owned for cleanup")
            self.assertEqual(path.parent, Path(stage_dir.name))
            self.assertTrue(path.is_file())
        finally:
            if stage_dir is not None:
                stage_dir.cleanup()
            if old is None:
                os.environ.pop("TMPDIR", None)
            else:
                os.environ["TMPDIR"] = old
        self.assertFalse(path.parent.exists(), "fallback stage dir leaked")
        self.assertEqual(set(Path(tempfile.gettempdir()).glob("omp-jl-stage-*")) - before, set())

    def test_find_runner_never_touches_staged_runner(self):
        repo, src = self._repo_with_runner()
        tmpdir = self._with_tmpdir()
        staged = tmpdir / "omp-jl-runner"
        staged.mkdir()
        runner = staged / "runner-1.jl"
        runner.write_text(src.read_text())
        old = os.environ.get("TMPDIR")
        os.environ["TMPDIR"] = str(tmpdir)
        before = set(Path(tempfile.gettempdir()).glob("omp-jl-stage-*"))
        try:
            path, matches, origin, stage_dir = jbd.find_runner(repo)
            self.assertEqual(origin, "rust-staged")
            self.assertEqual(path, runner)
            self.assertTrue(matches)
            self.assertIsNone(stage_dir)
            self.assertTrue(runner.exists(), "staged runner was deleted")
            self.assertEqual(list(tmpdir.glob("omp-jl-stage-*")), [])
            self.assertEqual(set(Path(tempfile.gettempdir()).glob("omp-jl-stage-*")) - before, set())
        finally:
            if old is None:
                os.environ.pop("TMPDIR", None)
            else:
                os.environ["TMPDIR"] = old


if __name__ == "__main__":
    unittest.main()
