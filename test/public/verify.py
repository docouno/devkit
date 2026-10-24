#!/usr/bin/env python3
"""Verify unauthenticated public bytes and the complete native consumer matrix."""
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
BASE = "https://github.com/docouno/devkit/releases/download"
TARGET = "x86_64-unknown-linux-musl"
VERSION = re.compile(r"(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)(?:-[a-z0-9][a-z0-9.-]*)?")
HEX256 = re.compile(r"[a-f0-9]{64}")


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def sri(data):
    return "sha512-" + base64.b64encode(hashlib.sha512(data).digest()).decode()


def archive_files(path, prefix):
    """Compare regular bytes/modes without extracting any member."""
    result = {}
    names = set()
    total = 0
    with tarfile.open(path) as archive:
        for member in archive:
            total += member.size
            require(total <= 256 * 1024 * 1024 and len(names) < 16384,
                    "Oversized public release archive")
            name = member.name.rstrip("/") if member.isdir() else member.name
            parts = name.split("/")
            require(parts and parts[0] == prefix
                    and all(part not in ("", ".", "..") for part in parts)
                    and not any(ord(char) < 32 or ord(char) == 127 for char in name),
                    f"Unsafe public archive path: {name}")
            require(name not in names, f"Duplicate public archive entry: {name}")
            names.add(name)
            require(member.isfile() or member.isdir(), f"Non-regular archive entry: {name}")
            if member.isdir():
                require(member.mode == 0o755 and member.size == 0, f"Invalid directory: {name}")
                continue
            require(len(parts) > 1 and member.mode in (0o644, 0o755)
                    and member.size <= 64 * 1024 * 1024, f"Invalid file metadata: {name}")
            stream = archive.extractfile(member)
            require(stream is not None, f"Unreadable archive entry: {name}")
            digest = hashlib.sha256()
            count = 0
            while block := stream.read(1024 * 1024):
                digest.update(block)
                count += len(block)
            require(count == member.size, f"Truncated archive entry: {name}")
            result["/".join(parts[1:])] = {"mode": member.mode, "sha256": digest.hexdigest()}
    return result


def archive_json(path, prefix, name):
    with tarfile.open(path) as archive:
        member = archive.getmember(f"{prefix}/{name}")
        require(member.isfile() and member.size <= 4 * 1024 * 1024,
                f"Invalid archive metadata: {name}")
        stream = archive.extractfile(member)
        require(stream is not None, f"Missing archive metadata: {name}")
        raw = stream.read()
        return json.loads(raw), raw


def main():
    def interrupted(signum, _frame):
        raise InterruptedError(f"Public verification interrupted by signal {signum}")

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True, choices=("0.2.0", "0.2.1"))
    parser.add_argument("--next-version", required=True)
    parser.add_argument("--expected-index-sha256", required=True,
                        help="Digest of the frozen build-set index before publication")
    args = parser.parse_args()
    require(VERSION.fullmatch(args.next_version) and args.version != args.next_version,
            "Use an exact, distinct native target version")
    require(HEX256.fullmatch(args.expected_index_sha256), "Use a frozen SHA256 index identity")
    evidence = REPO / ".artifacts/public" / uuid.uuid4().hex
    evidence.mkdir(parents=True)
    scratch = Path(tempfile.mkdtemp(prefix="devkit-public-"))
    downloads = []
    proofs = []
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
        result = subprocess.run(command, text=True, capture_output=True, timeout=190)
        require(result.returncode == 0, f"Public download failed: {url}: {result.stderr}")
        data = target.read_bytes()
        require(len(data) <= limit, f"Public artifact exceeds limit: {url}")
        downloads.append({"url": url, "sha256": sha256(data), "bytes": len(data),
                          "seconds": round(time.monotonic() - started, 3), "passed": True})
        return data

    def npm_package(name, version, expected_sri, expected_bytes=None, expected_sha1=None):
        stem = name.split("/")[-1]
        encoded = name.replace("/", "%2f")
        metadata = json.loads(download(f"https://registry.npmjs.org/{encoded}/{version}",
                                       scratch / f"npm-{stem}-{version}.json", 4 * 1024 * 1024))
        require(metadata["name"] == name and metadata["version"] == version,
                f"Public npm identity mismatch: {name}")
        require(metadata["dist"]["tarball"].startswith("https://registry.npmjs.org/"),
                "Unexpected npm distribution origin")
        path = scratch / f"{stem}-{version}.tgz"
        data = download(metadata["dist"]["tarball"], path, 128 * 1024 * 1024)
        require(sri(data) == metadata["dist"]["integrity"] == expected_sri,
                f"Public npm integrity differs: {name}")
        require(hashlib.sha1(data).hexdigest() == metadata["dist"]["shasum"],
                f"Public npm SHA1 differs: {name}")
        if expected_sha1 is not None:
            require(metadata["dist"]["shasum"] == expected_sha1, "Npm/index SHA1 differs")
        if expected_bytes is not None:
            require(data == expected_bytes, f"GitHub/npm bytes differ: {name}")
        return path, metadata

    try:
        old = args.version
        old_name = f"devkit-{old}.tar.gz"
        old_archive = scratch / old_name
        checksum = download(f"{BASE}/v{old}/{old_name}.sha256", scratch / (old_name + ".sha256"), 256)
        old_bytes = download(f"{BASE}/v{old}/{old_name}", old_archive, 128 * 1024 * 1024)
        require(checksum.decode() == f"{sha256(old_bytes)}  {old_name}\n", "Legacy archive checksum mismatch")
        old_manifest, old_raw = archive_json(old_archive, f"devkit-{old}", "release.json")
        legacy_identity = {
            "0.2.0": "39b085a22605e99e457c8c945866dc8f987afa4ef493dbe188d4dd2b59f21092",
            "0.2.1": "7f9dea1455027e0a0637077c7b040dbd7420a7fbc554d83d4d80ca47e9e849e0",
        }
        require(old_manifest["version"] == old and sha256(old_raw) == legacy_identity[old],
                "Genuine public baseline identity changed")
        npm_package("@docouno/env", old, old_manifest["environment"]["integrity"])

        target = args.next_version
        raw_index = download(f"{BASE}/v{target}/release-index.json", scratch / "release-index.json",
                             4 * 1024 * 1024)
        require(sha256(raw_index) == args.expected_index_sha256, "Public index differs from frozen build-set")
        index = json.loads(raw_index)
        require(index["schemaVersion"] == 1 and index["version"] == target
                and index["target"] == TARGET and index["publicationEligible"] is True,
                "Public target is not a publication-eligible native build-set")
        require(re.fullmatch(r"[a-f0-9]{40}", index["sourceCommit"])
                and re.fullmatch(r"[a-f0-9]{40}", index["sourceTree"])
                and index["buildRun"], "Public target lacks source/run attribution")
        require(1 <= len(index["files"]) <= 32, "Invalid public asset set")
        assets = {}
        for name, identity in sorted(index["files"].items()):
            require(re.fullmatch(r"[A-Za-z0-9._-]+", name) and name not in (".", "..", "release-index.json"),
                    f"Unsafe public asset name: {name}")
            require(HEX256.fullmatch(identity["sha256"])
                    and type(identity["size"]) is int and 0 <= identity["size"] <= 128 * 1024 * 1024,
                    f"Invalid public asset identity: {name}")
            data = download(f"{BASE}/v{target}/{name}", scratch / name, 128 * 1024 * 1024)
            require(sha256(data) == identity["sha256"] and len(data) == identity["size"],
                    f"Public asset differs from frozen index: {name}")
            assets[name] = data
        archive_name = f"devkit-{target}.tar.gz"
        require(index["archive"] == archive_name and index["engine"] == f"devkit-{TARGET}",
                "Unexpected public native asset names")
        for name in (archive_name, index["engine"], "release.json", "install.sh", "bootstrap.sh", "checksums.txt"):
            require(name in assets, f"Missing required public asset: {name}")
        manifest = json.loads(assets["release.json"])
        require(manifest["version"] == target and sha256(assets["release.json"]) == index["releaseIdentity"],
                "Public manifest identity differs")
        archive = scratch / archive_name
        archive_manifest, archive_raw = archive_json(archive, f"devkit-{target}", "release.json")
        require(archive_raw == assets["release.json"] and archive_manifest == manifest,
                "Archive/index manifest differs")
        expected = dict(manifest["files"])
        expected["release.json"] = {"mode": 0o644, "sha256": index["releaseIdentity"]}
        require(archive_files(archive, f"devkit-{target}") == expected,
                "Public tar payload differs from canonical manifest")
        require(manifest["files"]["devkit"]["sha256"] == sha256(assets[index["engine"]])
                and assets[index["engine"]][:4] == b"\x7fELF", "Raw engine/payload identity differs")
        require(set(index["packages"]) == {"cli", "environment"}, "Unexpected npm artifact set")
        npm_proofs = {}
        for kind, name in (("environment", "@docouno/env"), ("cli", "@docouno/devkit")):
            package = index["packages"][kind]
            require(package["name"] == name and package["version"] == target
                    and package["archive"] in assets, f"Wrong npm/index identity: {kind}")
            npm_path, metadata = npm_package(name, target, package["integrity"],
                                             assets[package["archive"]], package["shasum"])
            if kind == "cli":
                require(archive_files(npm_path, "package") == expected,
                        "Public npm CLI payload differs from canonical tar")
            else:
                require(package["integrity"] == manifest["environment"]["integrity"],
                        "Environment manifest/index SRI differs")
            npm_proofs[kind] = {"integrity": package["integrity"], "sha256": sha256(npm_path.read_bytes()),
                                "attestations": metadata["dist"].get("attestations")}
        require(assets["install.sh"] == assets["bootstrap.sh"], "Bootstrap alias bytes differ")
        release = json.loads(download(f"https://api.github.com/repos/docouno/devkit/releases/tags/v{target}",
                                      scratch / "github-release.json", 4 * 1024 * 1024))
        require(release["immutable"] is True and release["draft"] is False,
                "Public target release is not immutable")
        gh_assets = {asset["name"]: asset for asset in release["assets"]}
        require(set(gh_assets) == set(assets) | {"release-index.json"}, "GitHub asset set differs")
        for name, data in {**assets, "release-index.json": raw_index}.items():
            require(gh_assets[name]["digest"] == "sha256:" + sha256(data),
                    f"GitHub's stored asset digest differs: {name}")
        proofs.append({"version": target, "releaseIdentity": index["releaseIdentity"],
                       "sourceCommit": index["sourceCommit"], "sourceTree": index["sourceTree"],
                       "buildRun": index["buildRun"], "indexSha256": sha256(raw_index),
                       "tarAndNpmCanonicalPayloadEqual": True, "packages": npm_proofs})

        spec = importlib.util.spec_from_file_location("devkit_packaged_verification", REPO / "test/packaged/verify.py")
        require(spec is not None and spec.loader is not None, "Packaged verifier unavailable")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        consumer = module.Verification(old_archive, archive, package_source="registry",
                                       public_bootstrap=scratch / "bootstrap.sh")
        consumer.verify()
        success = True
    except BaseException as failure:
        error = str(failure)
        raise
    finally:
        summary = {"schemaVersion": 3, "success": success, "error": error, "version": args.version,
                   "nextVersion": args.next_version, "releaseBase": BASE, "defaultReleaseBaseTested": True,
                   "expectedIndexSha256": args.expected_index_sha256, "downloads": downloads,
                   "identities": proofs,
                   "consumerReceipt": str(consumer.receipt / "receipt.json") if consumer else None,
                   "scratch": str(scratch), "authenticationUsedForDownloads": False,
                   "limits": ["Actual public npx install/TTY is a separate delivery gate",
                              "Synthetic recovery journals do not reproduce physical power loss"],
                   "harnessSourceSha256": {str(path.relative_to(REPO)): sha256(path.read_bytes())
                                          for path in (Path(__file__), REPO / "test/packaged/verify.py")}}
        (evidence / "receipt.json").write_text(json.dumps(summary, indent=2) + "\n")
        print(f"Public receipt: {evidence / 'receipt.json'}", flush=True)
        if success:
            shutil.rmtree(scratch)


if __name__ == "__main__":
    main()
