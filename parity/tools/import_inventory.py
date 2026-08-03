#!/usr/bin/env python3
"""Reproduce the pinned Graphify test inventory.

This command intentionally refuses to collect a different commit or write an
inventory whose count/hash differs from ``upstream.lock.json``. Changing the
pin is a reviewed operation: update the lock first, then regenerate.

Live pytest collection remains authoritative for collected modules. Modules
skipped wholesale by optional ``importorskip`` guards are enumerated from their
test-source AST, including statically declared parametrizations, so missing an
optional dependency cannot erase upstream contracts from the ledger.
"""

from __future__ import annotations

import argparse
import collections
import json
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from parity.verify import (  # noqa: E402
    DEFAULT_LOCK,
    DEFAULT_MANIFEST,
    ParityError,
    _load_json,
    canonical_nodeids_sha256,
    collect_upstream_nodeids,
    module_from_source,
    source_from_nodeid,
    validate_lock,
)


def build_manifest(lock: dict, nodeids: tuple[str, ...]) -> dict:
    sources = [source_from_nodeid(nodeid) for nodeid in nodeids]
    counts = collections.Counter(sources)
    digest = canonical_nodeids_sha256(nodeids)
    expected = lock["pytest"]
    if len(nodeids) != expected["case_count"]:
        raise ParityError(
            f"collected {len(nodeids)} cases, lock requires {expected['case_count']}"
        )
    if len(counts) != expected["module_count"]:
        raise ParityError(
            f"collected {len(counts)} modules, lock requires {expected['module_count']}"
        )
    if digest != expected["nodeids_sha256"]:
        raise ParityError(
            f"collected node-ID SHA-256 {digest}, lock requires "
            f"{expected['nodeids_sha256']}"
        )

    return {
        "schema_version": 1,
        "upstream": {
            key: lock[key] for key in ("repository", "tag", "version", "commit")
        },
        "inventory": {
            "case_count": len(nodeids),
            "module_count": len(counts),
            "nodeids_sha256": digest,
        },
        "modules": [
            {
                "module": module_from_source(source),
                "source": source,
                "case_count": counts[source],
            }
            for source in sorted(counts)
        ],
        "cases": [
            {
                "nodeid": nodeid,
                "source": source_from_nodeid(nodeid),
                "module": module_from_source(source_from_nodeid(nodeid)),
                "status": "unmapped",
                "reason": "no_executable_parity_mapping",
            }
            for nodeid in nodeids
        ],
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--checkout", type=Path, required=True)
    parser.add_argument("--lock", type=Path, default=DEFAULT_LOCK)
    parser.add_argument("--output", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument(
        "--write",
        action="store_true",
        help="replace an existing output after every pinned invariant passes",
    )
    args = parser.parse_args()

    try:
        lock = _load_json(args.lock)
        validate_lock(lock)
        nodeids = collect_upstream_nodeids(args.checkout, lock)
        manifest = build_manifest(lock, nodeids)
        if args.output.exists() and not args.write:
            raise ParityError(f"refusing to overwrite {args.output}; pass --write")
        args.output.parent.mkdir(parents=True, exist_ok=True)
        temporary = args.output.with_suffix(args.output.suffix + ".tmp")
        temporary.write_text(
            json.dumps(manifest, ensure_ascii=False, indent=2) + "\n",
            encoding="utf-8",
        )
        temporary.replace(args.output)
    except ParityError as error:
        print(f"inventory import failed: {error}", file=sys.stderr)
        return 1

    print(
        f"wrote {args.output}: {manifest['inventory']['case_count']} cases in "
        f"{manifest['inventory']['module_count']} modules"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
