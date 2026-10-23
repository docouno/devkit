#!/usr/bin/env python3
"""Verify released GitHub assets, real npm bytes and public lifecycle consumers."""
from __future__ import annotations

import argparse
import base64
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import signal
import shutil
import subprocess
import tarfile
import tempfile
import time
import uuid

REPO = Path(__file__).resolve().parents[2]
DEFAULT_BASE = "https://github.com/docouno/devkit/releases/download"


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def main():
    def interrupted(signum, _frame):
        raise InterruptedError(f"Public verification interrupted by signal {signum}")

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--next-version", required=True)
    parser.add_argument("--release-base-url", default=DEFAULT_BASE)
    args = parser.parse_args()
    for version in (args.version, args.next_version):
        require(re.fullmatch(r"(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)(?:-[a-z0-9][a-z0-9.-]*)?", version), "Use exact release versions")
    require(args.version != args.next_version, "Provide distinct versions for the upgrade proof")
    base = args.release_base_url.rstrip("/")
    require(base.startswith("https://") and not any(c in base for c in "@?#\r\n\t "), "Use a public HTTPS release base")
    token = uuid.uuid4().hex
    evidence = REPO / ".artifacts/public" / token
    evidence.mkdir(parents=True)
    scratch = Path(tempfile.mkdtemp(prefix="devkit-public-"))
    results = []
    success = False
    error = None
    consumer = None
    print(f"Public evidence: {evidence}", flush=True)

    def download(url, target, limit):
        started = time.monotonic()
        command = ["curl", "-q", "--fail", "--silent", "--show-error", "--location", "--globoff",
                   "--proto", "=https", "--proto-redir", "=https", "--max-redirs", "5",
                   "--connect-timeout", "15", "--max-time", "180", "--max-filesize", str(limit),
                   "--output", str(target), "--", url]
        process = subprocess.run(command, text=True, capture_output=True, timeout=190)
        require(process.returncode == 0, f"Public download failed: {url}: {process.stderr}")
        data = target.read_bytes()
        require(len(data) <= limit, f"Public artifact exceeds limit: {url}")
        results.append({"url": url, "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data),
                        "seconds": round(time.monotonic() - started, 3), "passed": True})
        return data

    try:
        releases = {}
        for version in (args.version, args.next_version):
            name = f"devkit-{version}.tar.gz"
            archive = scratch / name
            checksum = download(f"{base}/v{version}/{name}.sha256", scratch / (name + ".sha256"), 256)
            data = download(f"{base}/v{version}/{name}", archive, 32 * 1024 * 1024)
            require(checksum.decode() == f"{hashlib.sha256(data).hexdigest()}  {name}\n", "Public archive checksum mismatch")
            with tarfile.open(archive) as packed:
                record = packed.extractfile(f"devkit-{version}/release.json")
                require(record is not None, "Release metadata missing")
                manifest = json.load(record)
            require(manifest["version"] == version, "Public release version mismatch")
            metadata = json.loads(download(f"https://registry.npmjs.org/@docouno%2fenv/{version}",
                                           scratch / f"npm-{version}.json", 4 * 1024 * 1024))
            require(metadata["name"] == "@docouno/env" and metadata["version"] == version, "Public npm identity mismatch")
            require(metadata["dist"]["tarball"].startswith("https://registry.npmjs.org/"), "Unexpected npm distribution origin")
            npm_bytes = download(metadata["dist"]["tarball"], scratch / f"env-{version}.tgz", 32 * 1024 * 1024)
            integrity = "sha512-" + base64.b64encode(hashlib.sha512(npm_bytes).digest()).decode()
            require(integrity == metadata["dist"]["integrity"] == manifest["environment"]["integrity"], "Public npm integrity differs from the release")
            releases[version] = archive

        bootstrap = scratch / "bootstrap.sh"
        data = download(f"{base}/v{args.next_version}/bootstrap.sh", bootstrap, 1024 * 1024)
        checksum_name = f"devkit-{args.next_version}.sh.sha256"
        checksum = download(f"{base}/v{args.next_version}/{checksum_name}", scratch / checksum_name, 256)
        require(checksum.decode() == f"{hashlib.sha256(data).hexdigest()}  devkit-{args.next_version}.sh\n", "Public bootstrap checksum mismatch")

        spec = importlib.util.spec_from_file_location("devkit_packaged_verification", REPO / "test/packaged/verify.py")
        require(spec is not None and spec.loader is not None, "Packaged verifier unavailable")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        # On the canonical endpoint omit both the option and environment override:
        # this exercises the published bootstrap's actual default release URL.
        override = None if base == DEFAULT_BASE else base
        consumer = module.Verification(releases[args.version], releases[args.next_version],
                                       package_source="registry", public_bootstrap=bootstrap,
                                       release_base_url=override)
        consumer.verify()
        success = True
    except BaseException as failure:
        error = str(failure)
        raise
    finally:
        summary = {"success": success, "error": error, "version": args.version,
                   "nextVersion": args.next_version, "releaseBase": base,
                   "defaultReleaseBaseTested": base == DEFAULT_BASE, "downloads": results,
                   "consumerReceipt": str(consumer.receipt / "receipt.json") if consumer else None,
                   "scratch": str(scratch), "authenticationUsedForDownloads": False,
                   "harnessSourceSha256": {str(path.relative_to(REPO)): hashlib.sha256(path.read_bytes()).hexdigest()
                                          for path in (Path(__file__), REPO / "test/packaged/verify.py")}}
        (evidence / "receipt.json").write_text(json.dumps(summary, indent=2) + "\n")
        print(f"Public receipt: {evidence / 'receipt.json'}", flush=True)
        if success:
            shutil.rmtree(scratch)


if __name__ == "__main__":
    main()
