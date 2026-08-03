"""Build both implementations and enforce corpus-specific graph contracts."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
from typing import Any

from parity.differential.graph_diff import (
    DifferentialError,
    REPOSITORY,
    build_and_compare,
    canonical_graph,
)


DEFAULT_EXPECTATIONS = Path(__file__).with_name("corpus_expectations.json")
STRICT_BASELINE_FIELDS = {
    "reference": "reference_strict_sha256",
    "candidate": "candidate_strict_sha256",
}


def _load_object(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"expected a JSON object in {path}")
    return value


def _contains_record(records: list[dict[str, Any]], expected: dict[str, Any]) -> bool:
    return any(
        all(record.get(key) == value for key, value in expected.items())
        for record in records
    )


def strict_graph_digest(graph: dict[str, Any], *, corpus: Path) -> str:
    """Hash the deterministic, non-volatile strict serialization projection."""
    canonical = canonical_graph(graph, corpus=corpus, profile="strict")
    encoded = json.dumps(
        canonical,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def evaluate_expectation(
    report: dict[str, Any],
    reference_graph: dict[str, Any],
    candidate_graph: dict[str, Any],
    expectation: dict[str, Any],
    *,
    corpus: Path,
) -> dict[str, Any]:
    failures: list[str] = []
    digests = {
        "reference": strict_graph_digest(reference_graph, corpus=corpus),
        "candidate": strict_graph_digest(candidate_graph, corpus=corpus),
    }
    baseline_results: dict[str, Any] = {}
    for side, field in STRICT_BASELINE_FIELDS.items():
        expected = expectation.get(field)
        actual = digests[side]
        valid_expected = (
            isinstance(expected, str)
            and len(expected) == 64
            and all(character in "0123456789abcdef" for character in expected)
        )
        matched = valid_expected and actual == expected
        baseline_results[side] = {
            "field": field,
            "expected": expected,
            "actual": actual,
            "matched": matched,
        }
        if not valid_expected:
            failures.append(f"missing or invalid reviewed baseline {field}")
        elif not matched:
            failures.append(
                f"{side} strict graph digest was {actual}, expected {expected}"
            )

    for side in ("reference", "candidate"):
        violations = report["diagnostics"]["pre_normalization"][side][
            "violation_count"
        ]
        if violations:
            failures.append(
                f"{side} graph had {violations} pre-normalization schema/identity violations"
            )

    preservation = report["parity"]["reference_preservation"]["preserved"]
    expected_preservation = expectation.get("reference_preserved")
    if expected_preservation is not None and preservation != expected_preservation:
        failures.append(
            f"reference_preserved was {preservation}, expected {expected_preservation}"
        )

    identity_hubs = report["diagnostics"]["identity_hubs"]
    expected_candidate_hubs = expectation.get("candidate_identity_hubs")
    actual_candidate_hubs = identity_hubs["candidate"]["id_count"]
    if (
        expected_candidate_hubs is not None
        and actual_candidate_hubs != expected_candidate_hubs
    ):
        failures.append(
            f"candidate identity hubs were {actual_candidate_hubs}, expected {expected_candidate_hubs}"
        )
    minimum_reference_hubs = expectation.get("reference_identity_hubs_min")
    actual_reference_hubs = identity_hubs["reference"]["id_count"]
    if (
        minimum_reference_hubs is not None
        and actual_reference_hubs < minimum_reference_hubs
    ):
        failures.append(
            f"reference identity hubs were {actual_reference_hubs}, expected at least {minimum_reference_hubs}"
        )

    candidate = canonical_graph(candidate_graph, corpus=corpus, profile="structure")
    edges = candidate["edges"]
    for required in expectation.get("required_candidate_edges", []):
        if not _contains_record(edges, required):
            failures.append(f"required candidate edge missing: {required}")
    for forbidden in expectation.get("forbidden_candidate_edges", []):
        if _contains_record(edges, forbidden):
            failures.append(f"forbidden candidate edge present: {forbidden}")
    return {
        "passed": not failures,
        "failures": failures,
        "strict_baselines": baseline_results,
    }


def run_suite(args: argparse.Namespace) -> dict[str, Any]:
    configuration = _load_object(args.expectations)
    if set(configuration) != {"schema_version", "corpora"}:
        raise ValueError("expectation file keys must be ['corpora', 'schema_version']")
    if configuration.get("schema_version") != 1:
        raise ValueError("unsupported corpus expectation schema_version")
    corpora = configuration.get("corpora")
    if not isinstance(corpora, list) or not corpora:
        raise ValueError("expectation file must declare a non-empty corpora list")
    names: set[str] = set()
    for index, specification in enumerate(corpora):
        if not isinstance(specification, dict) or set(specification) != {
            "name",
            "path",
            "expect",
        }:
            raise ValueError(
                f"corpus specification {index} keys must be ['expect', 'name', 'path']"
            )
        name = specification["name"]
        if (
            not isinstance(name, str)
            or not name
            or Path(name).name != name
            or name in names
        ):
            raise ValueError(f"corpus specification {index} has an invalid/duplicate name")
        names.add(name)
        if not isinstance(specification["path"], str) or not specification["path"]:
            raise ValueError(f"corpus specification {index} has an invalid path")
        if not isinstance(specification["expect"], dict):
            raise ValueError(f"corpus specification {index} expect must be an object")
    if args.build:
        subprocess.run(
            ["cargo", "build", "-p", "graphoxide-cli", "--locked"],
            cwd=REPOSITORY,
            check=True,
        )

    retained_root = args.work_dir.resolve()
    retained_root.mkdir(parents=True, exist_ok=True)
    # A unique suite directory avoids stale reports without recursively deleting
    # any caller-owned retained-work path.
    suite_work = Path(tempfile.mkdtemp(prefix="suite-", dir=retained_root))
    results = []
    for specification in corpora:
        name = specification["name"]
        corpus = (REPOSITORY / specification["path"]).resolve()
        work = suite_work / name
        report = build_and_compare(
            SimpleNamespace(
                corpus=corpus,
                upstream=args.upstream,
                graphoxide_bin=args.graphoxide_bin,
                build=False,
                timeout=args.timeout,
                work_dir=work,
                profile="structure",
                max_examples=args.max_examples,
                fail_on_candidate_identity_hubs=True,
                contract="reference-preserving",
            )
        )
        reference_path = Path(report["artifacts"]["reference"])
        candidate_path = Path(report["artifacts"]["candidate"])
        evaluation = evaluate_expectation(
            report,
            _load_object(reference_path),
            _load_object(candidate_path),
            specification["expect"],
            corpus=corpus,
        )
        report["corpus_expectation"] = evaluation
        report_path = work / "report.json"
        report_path.write_text(
            json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        results.append(
            {
                "name": name,
                "passed": evaluation["passed"],
                "failures": evaluation["failures"],
                "exact_equal": report["equal"],
                "reference_preserved": report["parity"]["reference_preservation"][
                    "preserved"
                ],
                "candidate_identity_hubs": report["diagnostics"]["identity_hubs"][
                    "candidate"
                ]["id_count"],
                "strict_baselines": evaluation["strict_baselines"],
                "report": str(report_path),
            }
        )
    return {
        "passed": all(item["passed"] for item in results),
        "work_dir": str(suite_work),
        "corpora": results,
    }


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--expectations", type=Path, default=DEFAULT_EXPECTATIONS)
    parser.add_argument("--upstream", type=Path, default=REPOSITORY / "upstream")
    parser.add_argument(
        "--graphoxide-bin", type=Path, default=REPOSITORY / "target/debug/graphoxide"
    )
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--build", action="store_true")
    parser.add_argument("--timeout", type=int, default=900)
    parser.add_argument("--max-examples", type=int, default=100)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        result = run_suite(args)
    except (DifferentialError, OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"corpus differential suite failed: {error}", file=sys.stderr)
        return 2
    encoded = json.dumps(result, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded, encoding="utf-8")
    else:
        sys.stdout.write(encoded)
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
