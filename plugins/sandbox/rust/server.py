#!/usr/bin/env python3
"""Rust compile/test sandbox for `execute_code(language: "rust")`.

Accepts a whole crate (a path -> content map) or a single `main.rs`, builds it
with cargo in a fresh scratch directory, and returns what cargo printed plus a
parsed test summary. The isolation that matters lives in docker-compose.yml
(no egress, no credentials, read-only root); this process adds per-run limits,
path validation, and the offline cargo environment.

Stdlib-only, like ../python/server.py.
"""

import json
import os
import re
import resource
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DEFAULT_TIMEOUT_MS = 120_000
MAX_TIMEOUT_MS = 300_000
MAX_OUTPUT_BYTES = 64 * 1024
MAX_REQUEST_BYTES = 1024 * 1024
MAX_FILES = 64
MAX_TOTAL_SOURCE_BYTES = 512 * 1024

# No RLIMIT_AS: rustc and lld reserve large virtual address ranges up front, so
# an address-space cap kills legitimate builds. The container's cgroup memory
# limit is the guard. Likewise no RLIMIT_NPROC; the cgroup pids_limit is.
CPU_SECONDS = 600
FILE_SIZE_BYTES = 512 * 1024 * 1024

# Concurrent builds. Each is a full cargo invocation with its own jobs; more
# than this contends the CPU for no throughput gain and risks the memory limit.
MAX_CONCURRENT_BUILDS = int(os.environ.get("RUST_SANDBOX_CONCURRENCY", "2"))
BUILD_JOBS = os.environ.get("RUST_SANDBOX_BUILD_JOBS", "4")

SCRATCH_ROOT = os.environ.get("RUST_SANDBOX_SCRATCH", "/work")
LISTEN_PORT = int(os.environ.get("SANDBOX_PORT", "8000"))

COMMANDS = {"test", "check", "clippy", "fmt", "run", "build"}
# One per rustc/clippy diagnostic that points at code ("  --> src/lib.rs:3:5").
# Cargo-level notices (an unused manifest key, "generated N warnings") carry no
# location and are not findings.
DIAGNOSTIC_LOCATION = re.compile(r"^\s*--> ", re.MULTILINE)
SAFE_PATH = re.compile(r"^[A-Za-z0-9_\-./]+$")
TEST_RESULT = re.compile(r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed")

DEFAULT_MANIFEST = """[package]
name = "{name}"
version = "0.1.0"
edition = "2021"

[dependencies]
"""

_build_slots = threading.BoundedSemaphore(MAX_CONCURRENT_BUILDS)


def _apply_limits() -> None:
    """Run in the child between fork and exec."""
    os.setsid()  # kill the whole cargo/rustc tree on timeout
    resource.setrlimit(resource.RLIMIT_CPU, (CPU_SECONDS, CPU_SECONDS))
    resource.setrlimit(resource.RLIMIT_FSIZE, (FILE_SIZE_BYTES, FILE_SIZE_BYTES))
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))


def _truncate(raw: bytes) -> tuple[str, bool]:
    text = raw.decode("utf-8", errors="replace")
    if len(text) <= MAX_OUTPUT_BYTES:
        return text, False
    # Keep the tail: the first compiler error is usually near the top, but the
    # test summary and the final `error: could not compile` land at the end.
    return text[-MAX_OUTPUT_BYTES:], True


def _available_crates() -> list[str]:
    """Crate names present in the offline registry cache."""
    cargo_home = os.environ.get("CARGO_HOME", "/usr/local/cargo")
    src = os.path.join(cargo_home, "registry", "cache")
    names = set()
    try:
        for index_dir in os.listdir(src):
            for crate_file in os.listdir(os.path.join(src, index_dir)):
                if crate_file.endswith(".crate"):
                    names.add(crate_file[: -len(".crate")])
    except OSError:
        return []
    return sorted(names)


def _rustc_version() -> str:
    try:
        return subprocess.run(
            ["rustc", "--version"], capture_output=True, text=True, timeout=10
        ).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return "unknown"


def validate_files(files: object) -> tuple[dict[str, str] | None, str | None]:
    """Return (files, None) when every path is a safe relative path, else (None, error)."""
    if not isinstance(files, dict) or not files:
        return None, "`files` must be a non-empty object of path -> content"
    if len(files) > MAX_FILES:
        return None, f"`files` has {len(files)} entries; the limit is {MAX_FILES}"
    total = 0
    for path, content in files.items():
        if not isinstance(path, str) or not isinstance(content, str):
            return None, "every `files` key and value must be a string"
        if (
            not path
            or path.startswith("/")
            or not SAFE_PATH.match(path)
            or any(part in ("", ".", "..") for part in path.split("/"))
        ):
            return None, f"unsafe path {path!r}: use a relative path like src/lib.rs"
        total += len(content.encode("utf-8"))
    if total > MAX_TOTAL_SOURCE_BYTES:
        return None, "`files` exceeds the total source size limit"
    return files, None


def build_command(command: str, include_ignored: bool) -> list[str]:
    if command == "fmt":
        # --check: report the diff rustfmt would apply (stdout, exit 1) and
        # change nothing. cargo fmt takes no --offline; it needs no network.
        return ["cargo", "fmt", "--check"]
    cmd = ["cargo", command, "--offline", "--color", "never"]
    if command == "clippy":
        cmd += ["--all-targets"]
    if command == "test" and include_ignored:
        cmd += ["--", "--include-ignored"]
    return cmd


def summarize_tests(output: str) -> tuple[int | None, int | None]:
    """Sum every `test result:` line cargo printed (unit, integration, doc)."""
    passed = failed = 0
    seen = False
    for match in TEST_RESULT.finditer(output):
        seen = True
        passed += int(match.group(2))
        failed += int(match.group(3))
    return (passed, failed) if seen else (None, None)


def run_crate(files: dict[str, str], command: str, include_ignored: bool, timeout_ms: int) -> dict:
    workdir = tempfile.mkdtemp(prefix="rust-", dir=SCRATCH_ROOT)
    started = time.monotonic()
    try:
        crate = os.path.join(workdir, "crate")
        for path, content in files.items():
            dest = os.path.join(crate, path)
            os.makedirs(os.path.dirname(dest), exist_ok=True)
            with open(dest, "w", encoding="utf-8") as fh:
                fh.write(content)
        manifest = os.path.join(crate, "Cargo.toml")
        if not os.path.exists(manifest):
            name = "sandbox_bin" if "src/main.rs" in files else "sandbox_crate"
            with open(manifest, "w", encoding="utf-8") as fh:
                fh.write(DEFAULT_MANIFEST.format(name=name))

        tmp = os.path.join(workdir, "tmp")
        os.makedirs(tmp)

        proc = subprocess.Popen(
            build_command(command, include_ignored),
            cwd=crate,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            preexec_fn=_apply_limits,
            env={
                "PATH": "/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin",
                "CARGO_HOME": os.environ.get("CARGO_HOME", "/usr/local/cargo"),
                "RUSTUP_HOME": os.environ.get("RUSTUP_HOME", "/usr/local/rustup"),
                # HOME and TMPDIR must be writable: the linker writes temp files
                # and fails on the read-only root without them.
                "HOME": workdir,
                "TMPDIR": tmp,
                "CARGO_TARGET_DIR": os.path.join(workdir, "target"),
                "CARGO_NET_OFFLINE": "true",
                "CARGO_BUILD_JOBS": BUILD_JOBS,
                "CARGO_TERM_COLOR": "never",
                # No RUST_BACKTRACE: a panic message already names the failing
                # file:line, and a backtrace per failing test buried the actual
                # failures (28 failing tests filled an 8000-char budget with
                # stack frames in the first benchmark smoke run).
                "RUST_BACKTRACE": "0",
                "LC_ALL": "C.UTF-8",
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
        tests_passed, tests_failed = summarize_tests(stdout)
        compile_failed = "could not compile" in stderr or "error[E" in stderr
        # clippy exits 0 on warnings, so its findings are counted, not read
        # from the exit code. None for every other command.
        lint_findings = (
            len(DIAGNOSTIC_LOCATION.findall(stderr)) if command == "clippy" else None
        )

        # Two shapes of the same offline miss: an unknown crate name, and a
        # known crate at a version that isn't cached ("failed to download ...
        # but --offline was specified").
        if any(
            marker in stderr
            for marker in (
                "no matching package named",
                "failed to select a version",
                "but --offline was specified",
            )
        ):
            stderr += (
                "\n\nsandbox: this sandbox is OFFLINE. Only these crates are available: "
                + ", ".join(_available_crates())
            )

        return {
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": proc.returncode,
            "timed_out": timed_out,
            "truncated": out_truncated or err_truncated,
            "duration_ms": int((time.monotonic() - started) * 1000),
            "command": command,
            "tests_passed": tests_passed,
            "tests_failed": tests_failed,
            "compile_failed": compile_failed,
            "lint_findings": lint_findings,
        }
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def parse_request(payload: dict) -> tuple[dict | None, str | None]:
    """Normalise a request into run_crate arguments, or return an error."""
    code = payload.get("code")
    files = payload.get("files")
    if files is not None and code is not None:
        return None, "pass either `code` or `files`, not both"
    if files is None:
        if not isinstance(code, str) or not code.strip():
            return None, "pass `code` (a main.rs) or `files` (a crate)"
        files = {"src/main.rs": code}
        default_command = "run"
    else:
        default_command = "test"

    files, error = validate_files(files)
    if error:
        return None, error

    command = payload.get("command") or default_command
    if command not in COMMANDS:
        return None, f"`command` must be one of {sorted(COMMANDS)}"

    try:
        timeout_ms = int(payload.get("timeout_ms") or DEFAULT_TIMEOUT_MS)
    except (TypeError, ValueError):
        timeout_ms = DEFAULT_TIMEOUT_MS
    timeout_ms = max(1_000, min(timeout_ms, MAX_TIMEOUT_MS))

    return {
        "files": files,
        "command": command,
        "include_ignored": bool(payload.get("include_ignored", False)),
        "timeout_ms": timeout_ms,
    }, None


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    rustc = "unknown"

    def _reply(self, status: int, payload: dict) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:
        if self.path == "/health":
            self._reply(200, {"status": "ok", "rustc": Handler.rustc, "crates": _available_crates()})
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
        if not isinstance(payload, dict):
            self._reply(400, {"error": "request body must be a JSON object"})
            return

        args, error = parse_request(payload)
        if error:
            self._reply(400, {"error": error})
            return

        try:
            with _build_slots:
                self._reply(200, run_crate(**args))
        except Exception as exc:  # noqa: BLE001 - never let one run kill the server
            self._reply(500, {"error": f"execution failed: {exc}"})

    def log_message(self, fmt: str, *args) -> None:
        sys.stderr.write("rust-sandbox: " + (fmt % args) + "\n")


def main() -> None:
    os.makedirs(SCRATCH_ROOT, exist_ok=True)
    Handler.rustc = _rustc_version()
    server = ThreadingHTTPServer(("0.0.0.0", LISTEN_PORT), Handler)
    server.daemon_threads = True
    sys.stderr.write(f"rust-sandbox: {Handler.rustc}, listening on 0.0.0.0:{LISTEN_PORT}\n")
    server.serve_forever()


if __name__ == "__main__":
    main()
