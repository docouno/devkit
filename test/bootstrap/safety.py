#!/usr/bin/env python3
"""Bounded transport, archive and cancellation regressions for the bootstrap."""
from __future__ import annotations

from hashlib import sha256
from pathlib import Path
import io
import gzip
import json
import os
import resource
import shutil
import signal
import socket
import subprocess
import tarfile
import time

from support import Harness, AUTHOR


class Zeros:
    def read(self, size):
        return bytes(size)


class Safety(Harness):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.script.write_bytes(self.script_bytes)
        self.bin = Path(self.env["PATH"].split(os.pathsep)[0])
        self.audit = self.scratch / "stream-audit.jsonl"
        self.env["STREAM_AUDIT"] = str(self.audit)
        self.stage_audit = self.scratch / "stage-audit.jsonl"
        self.env["STAGE_AUDIT"] = str(self.stage_audit)
        self.stage_cursor = 0
        # Record bounded on-disk files at the real cleanup boundary.
        rm = self.bin / "rm"
        rm.write_text('#!/usr/bin/python3\nimport json,os,sys\nfrom pathlib import Path\n'
                      'for arg in sys.argv[1:]:\n'
                      ' p=Path(arg)\n'
                      ' if p.is_dir() and p.name.startswith("devkit-bootstrap."):\n'
                      '  sizes={f.name:f.stat().st_size for f in p.iterdir() if f.is_file()}\n'
                      '  with open(os.environ["STAGE_AUDIT"],"a") as out: out.write(json.dumps(sizes)+"\\n")\n'
                      f'os.execv({shutil.which("rm")!r},[{shutil.which("rm")!r}]+sys.argv[1:])\n')
        rm.chmod(0o755)
        # The observers wrap real native utilities; they count transferred bytes,
        # record cancellation, and always reap their own subprocesses.
        template = '''#!/usr/bin/python3
import json,os,signal,subprocess,sys
kind=KIND
args=sys.argv[1:]
if kind == 'tar' and '--list' not in args: os.execv(REAL,[REAL]+args)
child=None
total=0
cancelled=False
status=None
def cancel(sig,frame):
 global cancelled
 cancelled=True
 raise SystemExit(128+sig)
signal.signal(signal.SIGTERM,cancel)
try:
 child=subprocess.Popen([REAL]+args,stdout=subprocess.PIPE)
 while True:
  data=os.read(child.stdout.fileno(),8192)
  if not data: break
  total+=len(data)
  os.write(1,data)
 status=child.wait()
except BrokenPipeError:
 status=141
finally:
 signal.signal(signal.SIGTERM,signal.SIG_IGN)
 if child is not None and child.poll() is None:
  child.terminate()
  try: child.wait(timeout=3)
  except subprocess.TimeoutExpired: child.kill(); child.wait()
 with open(os.environ['STREAM_AUDIT'],'a') as f:
  f.write(json.dumps({'kind':kind,'bytes':total,'cancelled':cancelled,'status':status})+'\\n')
if status: sys.exit(status)
'''
        for kind in ("tar", "gzip"):
            file = self.bin / kind
            file.write_text(template.replace("KIND", repr(kind)).replace("REAL", repr(shutil.which(kind))))
            file.chmod(0o755)

    def record(self, *args, **kwargs):
        super().record(*args, **kwargs)
        rows = self.stage_audit.read_text().splitlines() if self.stage_audit.exists() else []
        stages = [json.loads(row) for row in rows[self.stage_cursor:]]
        self.stage_cursor = len(rows)
        for files in stages:
            for name, size in files.items():
                if name.endswith('.tar.gz'): assert size <= 33554433, files
                if name == 'checksum': assert size <= 257, files
                if name == 'preflight.metadata': assert size <= 4194305, files
                if name == 'metadata-version': assert size <= 129, files
        if stages:
            self.results[-1]['stageFileBytesBeforeCleanup'] = stages

    def archive(self, case, *, count=None, large=None, long_names=False):
        file = self.scratch / (case + ".tar.gz")
        with tarfile.open(file, "w:gz", format=tarfile.PAX_FORMAT if long_names else tarfile.USTAR_FORMAT) as tf:
            root = tarfile.TarInfo(f"devkit-{self.version}/")
            root.type, root.mode = tarfile.DIRTYPE, 0o755
            tf.addfile(root)
            if count:
                for i in range(count):
                    prefix = ("a" * 120 + "/") * 15 if long_names else ""
                    info = tarfile.TarInfo(f"devkit-{self.version}/" + prefix + "file-" + str(i))
                    info.mode = 0o644
                    tf.addfile(info, io.BytesIO())
            else:
                info = tarfile.TarInfo(f"devkit-{self.version}/large")
                info.mode, info.size = 0o644, large
                tf.addfile(info, Zeros())
        data = file.read_bytes()
        Path(str(file) + ".sha256").write_text(sha256(data).hexdigest() + f"  devkit-{self.version}.tar.gz\n")
        return file

    def resource_case(self, name, file, expected, maximum_decompressed=None):
        self.audit.unlink(missing_ok=True)
        started = time.monotonic()
        self.boot(name, "init", self.scratch / name, "--version", self.version, "--release-archive", file, expect=expected)
        entries = [json.loads(s) for s in self.audit.read_text().splitlines()]
        tar_entries = [e for e in entries if e["kind"] == "tar"]
        assert len(tar_entries) == 1, entries
        decompressed = sum(e["bytes"] for e in entries if e["kind"] == "gzip")
        if maximum_decompressed is not None:
            assert decompressed < maximum_decompressed, entries
        self.results[-1].update(streams=entries, decompressedBytes=decompressed,
                                elapsedSeconds=round(time.monotonic() - started, 3))

    def cancellation(self):
        original = (self.bin / "docker").read_text()
        state = self.scratch / "cancellation-state"
        state.mkdir()
        self.env["REVIEW_STATE"] = str(state)
        (self.bin / "docker").write_text('''#!/usr/bin/python3
import json,os,sys,time
from pathlib import Path
p=Path(os.environ['REVIEW_STATE']); a=sys.argv[1:]
if a[:2]==['image','inspect']:
 f=p/'inspects'; n=int(f.read_text())+1 if f.exists() else 1; f.write_text(str(n))
 sys.exit(0 if n==1 else 1)
if a[:1]==['pull']:
 (p/'pull-pid').write_text(str(os.getpid()))
 while True: time.sleep(.1)
if a[:1]==['run']: print('0.1.0',end=''); sys.exit(0)
if a[:2]==['container','ls']: sys.exit(0)
sys.exit(1)
'''.replace("0.1.0", self.version))
        output = self.scratch / "cancel.log"
        try:
            with output.open("w") as out:
                child = self.spawn(["bash", str(self.script), "init", str(self.scratch / "pre-guard"),
                                          "--version", self.version, "--release-archive", str(self.release_path)],
                                         env=self.env, cwd=self.scratch, stdout=out, stderr=subprocess.STDOUT)
                deadline = time.monotonic() + 15
                while not (state / "pull-pid").exists() and child.poll() is None and time.monotonic() < deadline:
                    time.sleep(.02)
                assert (state / "pull-pid").exists(), output.read_text()
                pid = int((state / "pull-pid").read_text())
                child.send_signal(signal.SIGTERM)
                code = child.wait(timeout=15)
            survived = Path(f"/proc/{pid}").exists()
            self.record("pre-guard-pull-process-group", code, output.read_text() + f"exit={code}\n", "exit=143",
                        descendantSurvived=survived)
            assert not survived, f"pull PID {pid} survived"
        finally:
            (self.bin / "docker").write_text(original)

    def startup_failures(self):
        limit = 256 * 1024 * 1024
        def restrict():
            resource.setrlimit(resource.RLIMIT_AS, (limit, limit))
        for name, environment, before_exec, expected in (
            ("producer-hard-rlimit-before-open", self.env, restrict, "cannot modify limit"),
            ("producer-exit-before-open", dict(self.env, **{
                "BASH_FUNC_ulimit%%": "() { printf 'producer failed before FIFO writer\\n' >&2; exit 47; }"
            }), None, "producer failed before FIFO writer"),
        ):
            started = time.monotonic()
            child = self.spawn(["bash", str(self.script), "init", str(self.scratch / name),
                                      "--version", self.version, "--release-archive",
                                      str(self.release_path)], env=environment,
                                     cwd=self.scratch, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                     text=True, start_new_session=True, preexec_fn=before_exec)
            try:
                output, _ = child.communicate(timeout=8)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGTERM)
                child.communicate(timeout=5)
                raise AssertionError(f"{name}: early producer failure did not return promptly")
            elapsed = time.monotonic() - started
            self.record(name, child.returncode, output, expected,
                        elapsedSeconds=round(elapsed, 3), inheritedRlimitAsBytes=limit if before_exec else None)
            assert elapsed < 5, elapsed
            assert child.returncode == (1 if before_exec else 47), child.returncode

    def execute(self):
        self.startup_failures()
        self.boot("startup-stream-positive", "init", self.scratch / "startup happy path", "--version", self.version,
                  "--template", "rust", "--package-source", "local")
        self.cancellation()
        # The server records the denied HTTP target. Binding all local interfaces
        # reproduces the review without touching an external server.
        ip = next(x for x in socket.gethostbyname_ex(socket.gethostname())[2] if not x.startswith("127."))
        seen = []
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
        from threading import Thread
        class Redirect(BaseHTTPRequestHandler):
            def log_message(self, *args): pass
            def do_GET(self):
                seen.append({"host": self.headers.get("Host"), "path": self.path})
                self.send_response(302 if self.path.startswith("/origin/") else 404)
                if self.path.startswith("/origin/"):
                    self.send_header("Location", f"http://{ip}:{self.server.server_address[1]}/destination")
                self.end_headers()
        server = ThreadingHTTPServer(("0.0.0.0", 0), Redirect)
        Thread(target=server.serve_forever, daemon=True).start()
        try:
            self.run("deny-nonloopback-http-redirect", ["bash", self.script, "init", self.scratch / "redirect",
                     "--version", self.version, "--release-base-url", f"http://127.0.0.1:{server.server_address[1]}/origin"], "Protocol")
            assert len(seen) == 1 and seen[0]["path"].startswith("/origin/"), seen
            self.results[-1]["requests"] = seen
        finally:
            server.shutdown(); server.server_close()
        self.resource_case("original-entry-limit", self.archive("entries-original", count=10001), "invalid archive file count")
        self.resource_case("amplified-entry-limit", self.archive("entries-amplified", count=100000),
                           "invalid archive file count", maximum_decompressed=20 * 1024 * 1024)
        self.resource_case("original-expanded-limit", self.archive("large-original", large=268435457),
                           "archive expands beyond the bootstrap limit", maximum_decompressed=2 * 1024 * 1024)
        self.resource_case("metadata-stream-limit", self.archive("long-names", count=3000, long_names=True),
                           "archive metadata exceeds 4194304 bytes", maximum_decompressed=16 * 1024 * 1024)
        # A declared giant GNU long-name header must not request unbounded heap.
        long_name = tarfile.TarInfo("././@LongLink")
        long_name.type, long_name.size = tarfile.GNUTYPE_LONGNAME, 1024 * 1024 * 1024
        malformed = self.scratch / "giant-header.tar.gz"
        malformed.write_bytes(gzip.compress(long_name.tobuf(format=tarfile.GNU_FORMAT) + bytes(10240)))
        Path(str(malformed) + ".sha256").write_text(sha256(malformed.read_bytes()).hexdigest() + f"  devkit-{self.version}.tar.gz\n")
        self.boot("extended-header-memory-bound", "init", self.scratch / "giant-header", "--version", self.version,
                  "--release-archive", malformed, expect="memory exhausted")
        # No Content-Length and no --max-filesize dependency; chunks are generated
        # lazily, so the sender itself does not allocate a large response.
        raw_server = self.server
        handler = raw_server.RequestHandlerClass
        previous = handler.do_GET
        response_bytes = {}
        def oversized(instance):
            if instance.path.startswith("/unbounded/"):
                self.requests.append(instance.path)
                instance.send_response(200)
                instance.end_headers()
                sent = 0
                try:
                    for _ in range(1024):
                        instance.wfile.write(b"x" * 65536)
                        instance.wfile.flush()
                        sent += 65536
                except (BrokenPipeError, ConnectionResetError): pass
                response_bytes[instance.path] = sent
            else: previous(instance)
        handler.do_GET = oversized
        try:
            self.boot("unknown-length-checksum-limit", "init", self.scratch / "big-checksum", "--version", self.version,
                      route="unbounded", expect="checksum metadata exceeds 256 bytes")
            # Serve the real checksum, then a >32MiB unknown-length archive.
            old = handler.do_GET
            def archive_only(instance):
                if instance.path.endswith(".sha256") and instance.path.startswith("/big-archive/"):
                    self.requests.append(instance.path)
                    data = self.routes[f"/releases/v{self.version}/devkit-{self.version}.tar.gz.sha256"]
                    instance.send_response(200); instance.end_headers(); instance.wfile.write(data)
                elif instance.path.startswith("/big-archive/"):
                    instance.path = instance.path.replace("/big-archive/", "/unbounded/")
                    oversized(instance)
                else: old(instance)
            handler.do_GET = archive_only
            self.boot("unknown-length-archive-limit", "init", self.scratch / "big-archive", "--version", self.version,
                      route="big-archive", expect="archive exceeds 33554432 bytes")
            deadline = time.monotonic() + 2
            while len(response_bytes) < 2 and time.monotonic() < deadline:
                time.sleep(.01)
            assert len(response_bytes) == 2, response_bytes
            assert all(n < 64 * 1024 * 1024 for n in response_bytes.values()), response_bytes
            self.results[-1]["responseBytesSent"] = response_bytes
        finally:
            handler.do_GET = previous
        manifest = self.scratch / "large manifest" / ".devkit"
        manifest.mkdir(parents=True)
        (manifest / "manifest.json").write_bytes(b" " * (4 * 1024 * 1024 + 1))
        self.boot("manifest-input-limit", "register", manifest.parent, "--app", "worker", "--template", "rust",
                  expect="manifest exceeds metadata limit")
        original_docker = (self.bin / "docker").read_text()
        # Simulate an overlong daemon/stdout response; host buffer still stops at 129 bytes.
        (self.bin / "docker").write_text('#!/usr/bin/python3\nimport os,sys\n'
                                       'if sys.argv[1:3] == ["image","inspect"]: sys.exit(0)\n'
                                       'if sys.argv[1:3] == ["container","ls"]: sys.exit(0)\n'
                                       'if sys.argv[1:2] == ["run"]: os.write(1,b"x"*1024); sys.exit(0)\n'
                                       'sys.exit(1)\n')
        try:
            self.boot("metadata-output-limit", "register", manifest.parent, "--app", "worker", "--template", "rust",
                      expect="version metadata exceeds 128 bytes")
        finally:
            (self.bin / "docker").write_text(original_docker)
        # A stalled native reader is bounded by the actual 60-second deadline.
        original_tar = (self.bin / "tar").read_text()
        stalled_pid = self.scratch / "stalled-tar.pid"
        (self.bin / "tar").write_text('#!/usr/bin/python3\nimport os,sys,time\nfrom pathlib import Path\n'
                                     f'if "--list" not in sys.argv: os.execv({shutil.which("tar")!r},[{shutil.which("tar")!r}]+sys.argv[1:])\n'
                                     f'Path({str(stalled_pid)!r}).write_text(str(os.getpid()))\n'
                                     'while True: time.sleep(.1)\n')
        started = time.monotonic()
        try:
            self.boot("preflight-deadline", "init", self.scratch / "stalled", "--version", self.version,
                      "--release-archive", self.release_path, expect="archive preflight timed out")
            elapsed = time.monotonic() - started
            assert 58 <= elapsed <= 75, elapsed
            pid = int(stalled_pid.read_text())
            assert not Path(f"/proc/{pid}").exists(), f"stalled worker {pid} survived"
            self.results[-1].update(elapsedSeconds=round(elapsed, 3), descendantSurvived=False)
        finally:
            (self.bin / "tar").write_text(original_tar)
        self.record("no-host-language-sdk", 0, "No host Node/npm/Rust executable was invoked.\n")
        assert not self.forbidden.exists()
        self.owned_cleanup()
