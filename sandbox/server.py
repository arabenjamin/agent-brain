#!/usr/bin/env python3
"""Code execution sandbox for the `execute_code` tool.

Runs untrusted, LLM-generated Python and returns what it printed. The isolation
that matters is not in this file — it is in docker-compose.yml, where this
service runs on an `internal: true` network with a read-only root filesystem,
all capabilities dropped, and none of the brain's credentials in its
environment. This process adds the per-execution limits Docker cannot express:
a wall clock, resource ceilings, and a scratch directory per run.

Deliberately stdlib-only for the server itself, so the attack surface is the
Python interpreter and nothing else. The compute libraries (numpy, sympy,
pandas) exist for submitted code to import, not for this module.
"""

import json
import os
import resource
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Ceilings. Every one of these is a guard against a runaway program, not
# against a hostile one — a determined escape is the network/filesystem
# isolation's job, and that lives in compose.
DEFAULT_TIMEOUT_MS = 30_000
MAX_TIMEOUT_MS = 120_000
MAX_CODE_BYTES = 256 * 1024
MAX_OUTPUT_BYTES = 64 * 1024
MAX_REQUEST_BYTES = 512 * 1024

ADDRESS_SPACE_BYTES = 2 * 1024 * 1024 * 1024  # numpy reserves virtual memory freely
CPU_SECONDS = 60
FILE_SIZE_BYTES = 32 * 1024 * 1024
MAX_PROCESSES = 64

LISTEN_PORT = int(os.environ.get("SANDBOX_PORT", "8000"))


def _apply_limits() -> None:
    """Run in the child between fork and exec."""
    # New session: the whole tree gets killed on timeout, not just the parent.
    os.setsid()
    resource.setrlimit(resource.RLIMIT_AS, (ADDRESS_SPACE_BYTES, ADDRESS_SPACE_BYTES))
    resource.setrlimit(resource.RLIMIT_CPU, (CPU_SECONDS, CPU_SECONDS))
    resource.setrlimit(resource.RLIMIT_FSIZE, (FILE_SIZE_BYTES, FILE_SIZE_BYTES))
    resource.setrlimit(resource.RLIMIT_NPROC, (MAX_PROCESSES, MAX_PROCESSES))
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))


def _truncate(raw: bytes) -> tuple[str, bool]:
    text = raw.decode("utf-8", errors="replace")
    if len(text) <= MAX_OUTPUT_BYTES:
        return text, False
    # Keep the tail: a traceback and the final printed result both land at the
    # end, and those are what the caller actually needs.
    return text[-MAX_OUTPUT_BYTES:], True


def run_code(code: str, timeout_ms: int) -> dict:
    workdir = tempfile.mkdtemp(prefix="exec-")
    started = time.monotonic()
    try:
        script = os.path.join(workdir, "program.py")
        with open(script, "w", encoding="utf-8") as fh:
            fh.write(code)

        # `-I` is isolated mode: ignore PYTHON* env vars and the user site
        # directory. Combined with the scrubbed environment below, submitted
        # code cannot be steered by anything outside this request.
        proc = subprocess.Popen(
            [sys.executable, "-I", "program.py"],
            cwd=workdir,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            preexec_fn=_apply_limits,
            env={
                "PATH": "/usr/local/bin:/usr/bin:/bin",
                "HOME": workdir,
                "TMPDIR": workdir,
                "LC_ALL": "C.UTF-8",
                "PYTHONDONTWRITEBYTECODE": "1",
                "MPLBACKEND": "Agg",
            },
        )

        timed_out = False
        try:
            out, err = proc.communicate(timeout=timeout_ms / 1000.0)
        except subprocess.TimeoutExpired:
            timed_out = True
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                proc.kill()
            out, err = proc.communicate()

        stdout, out_truncated = _truncate(out)
        stderr, err_truncated = _truncate(err)
        return {
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": proc.returncode,
            "timed_out": timed_out,
            "truncated": out_truncated or err_truncated,
            "duration_ms": int((time.monotonic() - started) * 1000),
        }
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _reply(self, status: int, payload: dict) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:
        if self.path == "/health":
            self._reply(200, {"status": "ok"})
        else:
            self._reply(404, {"error": "not found"})

    def do_POST(self) -> None:
        if self.path != "/exec":
            self._reply(404, {"error": "not found"})
            return

        length = int(self.headers.get("Content-Length") or 0)
        if length <= 0 or length > MAX_REQUEST_BYTES:
            self._reply(413, {"error": "request body missing or too large"})
            return

        try:
            payload = json.loads(self.rfile.read(length))
        except (ValueError, UnicodeDecodeError) as exc:
            self._reply(400, {"error": f"invalid JSON: {exc}"})
            return

        code = payload.get("code")
        if not isinstance(code, str) or not code.strip():
            self._reply(400, {"error": "`code` must be a non-empty string"})
            return
        if len(code.encode("utf-8")) > MAX_CODE_BYTES:
            self._reply(413, {"error": "`code` exceeds size limit"})
            return

        try:
            timeout_ms = int(payload.get("timeout_ms") or DEFAULT_TIMEOUT_MS)
        except (TypeError, ValueError):
            timeout_ms = DEFAULT_TIMEOUT_MS
        timeout_ms = max(1_000, min(timeout_ms, MAX_TIMEOUT_MS))

        try:
            self._reply(200, run_code(code, timeout_ms))
        except Exception as exc:  # noqa: BLE001 - never let one run kill the server
            self._reply(500, {"error": f"execution failed: {exc}"})

    def log_message(self, fmt: str, *args) -> None:
        sys.stderr.write("sandbox: " + (fmt % args) + "\n")


def main() -> None:
    server = ThreadingHTTPServer(("0.0.0.0", LISTEN_PORT), Handler)
    server.daemon_threads = True
    sys.stderr.write(f"sandbox: listening on 0.0.0.0:{LISTEN_PORT}\n")
    server.serve_forever()


if __name__ == "__main__":
    main()
