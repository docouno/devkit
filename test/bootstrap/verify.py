#!/usr/bin/env python3
"""Verify a rendered bootstrap against immutable standalone release archives."""
from __future__ import annotations

import argparse
import json
from pathlib import Path

from safety import Safety
from support import AUTHOR, Harness, require


class Migration(Harness):
    def execute(self):
        self.run("download-bootstrap-via-curl", ["curl", "-q", "--fail", "--silent", "--show-error", "--output",
                                                self.script, self.base + f"/releases/v{self.bootstrap_version}/bootstrap.sh"])
        target = self.scratch / "legacy consumer"
        self.boot("legacy-http-init", "init", target, "--version", self.version,
                  "--template", "rust", "--package-source", "local")
        source = target / "apps/api/src/main.rs"
        source.write_text(source.read_text() + "\n// Keep the product change during schema migration.\n")
        environment = target / ".env"
        environment.write_text("USER_CONFIGURATION=keep\n")
        before = {path: path.read_bytes() for path in (source, environment, target / "Makefile", target / "compose.yaml")}
        self.boot("legacy-http-update-to-current", "update", target, "--version", self.next_version)
        state = json.loads((target / ".devkit/manifest.json").read_text())
        require(state["version"] == self.next_version and state["schemaVersion"] >= 2, "Legacy update did not accept the current schema")
        require(all(path.read_bytes() == data for path, data in before.items()), "Legacy update changed product inputs")
        self.boot("migrated-http-recover-no-pending", "recover", target, "--version", self.next_version)
        require(not self.forbidden.exists(), "Migration invoked a host language SDK")
        self.owned_cleanup()


def check(harness_type, release, next_release, args):
    harness = harness_type(release, next_release, args.bootstrap, render_template=args.render_template)
    success = False
    try:
        harness.execute()
        success = True
    finally:
        harness.finish(success)


def main():
    version = (AUTHOR / "VERSION").read_text().strip()
    major, minor, patch = version.split("-", 1)[0].split(".")
    next_version = f"{major}.{minor}.{int(patch) + 1}"
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release", type=Path, default=AUTHOR / f".artifacts/devkit-{version}.tar.gz")
    parser.add_argument("--next-release", type=Path, default=AUTHOR / f".artifacts/devkit-{next_version}.tar.gz")
    parser.add_argument("--bootstrap", type=Path)
    parser.add_argument("--legacy-release", type=Path)
    parser.add_argument("--legacy-next-release", type=Path)
    parser.add_argument("--legacy-only", action="store_true", help="Use preserved 0.1 archives for transport development")
    parser.add_argument("--render-template", action="store_true", help="Explicit source-template test before packing")
    parser.add_argument("--safety-only", action="store_true")
    parser.add_argument("--skip-safety", action="store_true")
    args = parser.parse_args()
    require(not (args.safety_only and args.skip_safety), "Choose either --safety-only or --skip-safety")
    require(not (args.bootstrap and args.render_template), "Choose a built bootstrap or --render-template")
    if args.legacy_only:
        args.release = args.legacy_release or AUTHOR / ".artifacts/devkit-0.1.0.tar.gz"
        args.next_release = args.legacy_next_release or AUTHOR / ".artifacts/devkit-0.1.1.tar.gz"
    if not args.safety_only:
        check(Harness, args.release, args.next_release, args)
    if not args.skip_safety:
        check(Safety, args.release, args.next_release, args)
    if not args.legacy_only and not args.safety_only and (args.legacy_release or args.legacy_next_release):
        require(args.legacy_release and args.legacy_next_release, "Provide both legacy release archives")
        check(Harness, args.legacy_release, args.legacy_next_release, args)
        check(Migration, args.legacy_release, args.release, args)


if __name__ == "__main__":
    main()
