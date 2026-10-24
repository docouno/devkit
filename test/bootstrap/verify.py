#!/usr/bin/env python3
"""Verify native initial transfer, migration and bounded hostile inputs."""
from __future__ import annotations
import argparse
from pathlib import Path
from safety import Safety
from support import Harness


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--build-set', type=Path, required=True)
    parser.add_argument('--legacy-release', type=Path, required=True, help='Preserved public 0.2.0 or 0.2.1 archive, with adjacent checksum')
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--safety-only', action='store_true')
    mode.add_argument('--skip-safety', action='store_true')
    args = parser.parse_args()
    for cls in ([Safety] if args.safety_only else [Harness] if args.skip_safety else [Harness, Safety]):
        harness = cls(args.build_set, args.legacy_release)
        passed = False
        try:
            harness.execute()
            passed = True
        finally:
            harness.finish(passed)


if __name__ == '__main__':
    main()
