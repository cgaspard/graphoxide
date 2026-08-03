"""Build both implementations and enforce corpus-specific graph contracts."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import subprocess
import sys
import tempfile
import unicodedata
from pathlib import Path, PureWindowsPath
from types import SimpleNamespace
from typing import Any

from parity.differential.graph_diff import (
    DifferentialError,
    REPOSITORY,
    _json_values_equal,
    _reject_duplicate_json_object,
    _reject_nonfinite_json_constant,
    _require_finite_json_numbers,
    build_and_compare,
    canonical_graph,
)


DEFAULT_EXPECTATIONS = Path(__file__).with_name("corpus_expectations.json")
STRICT_BASELINE_FIELDS = {
    "reference": "reference_strict_sha256",
    "candidate": "candidate_strict_sha256",
}
CORPUS_INPUT_BASELINE_FIELD = "corpus_input_sha256"
EDGE_CONTRACT_LIST_FIELDS = {
    "required_candidate_edges",
    "forbidden_candidate_edges",
}
EDGE_CONTRACT_FIELDS = {
    "source",
    "target",
    "relation",
    "confidence",
    "confidence_score",
    "source_file",
    "source_location",
    "line",
    "line_number",
    "context",
    "key",
}
EDGE_CONTRACT_REQUIRED_FIELDS = {"source", "target", "relation"}
EXPECTATION_REQUIRED_FIELDS = {
    CORPUS_INPUT_BASELINE_FIELD,
    *STRICT_BASELINE_FIELDS.values(),
}
EXPECTATION_OPTIONAL_FIELDS = {
    "reference_preserved",
    "candidate_cross_runtime_bindings",
    "reference_cross_runtime_bindings_min",
    *EDGE_CONTRACT_LIST_FIELDS,
}
EXPECTATION_FIELDS = EXPECTATION_REQUIRED_FIELDS | EXPECTATION_OPTIONAL_FIELDS


def _load_object(path: Path) -> dict[str, Any]:
    value = json.loads(
        path.read_text(encoding="utf-8"),
        parse_constant=_reject_nonfinite_json_constant,
        object_pairs_hook=_reject_duplicate_json_object,
    )
    _require_finite_json_numbers(value)
    if not isinstance(value, dict):
        raise ValueError(f"expected a JSON object in {path}")
    return value


def _contains_record(records: list[dict[str, Any]], expected: dict[str, Any]) -> bool:
    return any(
        all(
            key in record and _json_values_equal(record[key], value)
            for key, value in expected.items()
        )
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


def corpus_input_digest(corpus: Path) -> str:
    """Hash every corpus input path and byte without depending on its root."""
    if corpus.is_symlink():
        raise ValueError(f"corpus input symlinks are forbidden: {corpus}")
    corpus = corpus.absolute()
    if not corpus.is_dir():
        raise ValueError(f"corpus input root is not a directory: {corpus}")
    records: list[dict[str, Any]] = []
    paths = sorted(
        corpus.rglob("*"),
        key=lambda path: unicodedata.normalize(
            "NFC", path.relative_to(corpus).as_posix()
        ),
    )
    seen_paths: set[str] = set()
    for path in paths:
        relative = unicodedata.normalize("NFC", path.relative_to(corpus).as_posix())
        if relative in seen_paths:
            raise ValueError(f"duplicate normalized corpus input path: {relative}")
        seen_paths.add(relative)
        if path.is_symlink():
            raise ValueError(f"corpus input symlinks are forbidden: {relative}")
        elif path.is_file():
            content = path.read_bytes()
            records.append(
                {
                    "kind": "file",
                    "path": relative,
                    "size": len(content),
                    "sha256": hashlib.sha256(content).hexdigest(),
                }
            )
        elif path.is_dir():
            records.append({"kind": "directory", "path": relative})
        else:
            raise ValueError(f"unsupported corpus input type: {relative}")
    encoded = json.dumps(
        {"schema": "graphoxide-corpus-input-v1", "records": records},
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def _valid_sha256(value: Any) -> bool:
    return (
        isinstance(value, str)
        and len(value) == 64
        and all(character in "0123456789abcdef" for character in value)
    )


def _is_finite_json_number(value: Any) -> bool:
    if isinstance(value, bool):
        return False
    if isinstance(value, int):
        return True
    return isinstance(value, float) and math.isfinite(value)


def _valid_contract_identity(value: Any) -> bool:
    return isinstance(value, str) and bool(value)


def _validate_edge_contract_list(value: Any, *, field: str, context: str) -> None:
    if not isinstance(value, list):
        raise ValueError(f"{context} {field} must be an array")
    for index, record in enumerate(value):
        record_context = f"{context} {field}[{index}]"
        if not isinstance(record, dict):
            raise ValueError(f"{record_context} must be an object")
        unknown = set(record) - EDGE_CONTRACT_FIELDS
        if unknown:
            raise ValueError(
                f"{record_context} has unknown keys: {sorted(unknown)}"
            )
        missing = EDGE_CONTRACT_REQUIRED_FIELDS - set(record)
        if missing:
            raise ValueError(
                f"{record_context} is missing required keys: {sorted(missing)}"
            )
        for endpoint in ("source", "target"):
            if not _valid_contract_identity(record[endpoint]):
                raise ValueError(
                    f"{record_context} {endpoint} must be a non-empty string"
                )
        if not isinstance(record["relation"], str) or not record["relation"]:
            raise ValueError(f"{record_context} relation must be a non-empty string")
        for name in ("confidence", "source_file", "source_location", "context"):
            if name in record and record[name] is not None and not isinstance(
                record[name], str
            ):
                raise ValueError(f"{record_context} {name} must be a string or null")
        for name in ("line", "line_number"):
            if name in record and record[name] is not None and not (
                isinstance(record[name], int)
                and not isinstance(record[name], bool)
                and record[name] >= 0
            ):
                raise ValueError(
                    f"{record_context} {name} must be a non-negative integer or null"
                )
        if (
            "confidence_score" in record
            and record["confidence_score"] is not None
            and not _is_finite_json_number(record["confidence_score"])
        ):
            raise ValueError(
                f"{record_context} confidence_score must be a finite number or null"
            )
        if (
            "key" in record
            and record["key"] is not None
            and not (
                _valid_contract_identity(record["key"])
                or _is_finite_json_number(record["key"])
            )
        ):
            raise ValueError(
                f"{record_context} key must be a non-empty string, finite number, or null"
            )


def _validate_expectation(value: Any, *, context: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ValueError(f"{context} must be an object")
    unknown = set(value) - EXPECTATION_FIELDS
    if unknown:
        raise ValueError(f"{context} has unknown keys: {sorted(unknown)}")
    missing = EXPECTATION_REQUIRED_FIELDS - set(value)
    if missing:
        raise ValueError(f"{context} is missing required keys: {sorted(missing)}")

    for field in EXPECTATION_REQUIRED_FIELDS:
        if not _valid_sha256(value[field]):
            raise ValueError(f"{context} {field} must be a lowercase SHA-256 digest")
    if "reference_preserved" in value and type(value["reference_preserved"]) is not bool:
        raise ValueError(f"{context} reference_preserved must be a boolean")
    for field in (
        "candidate_cross_runtime_bindings",
        "reference_cross_runtime_bindings_min",
    ):
        if field in value and (type(value[field]) is not int or value[field] < 0):
            raise ValueError(f"{context} {field} must be a non-negative integer")
    for field in EDGE_CONTRACT_LIST_FIELDS:
        if field in value:
            _validate_edge_contract_list(value[field], field=field, context=context)
    return value


def _resolve_repository_corpus(raw: Any, *, index: int) -> Path:
    if (
        not isinstance(raw, str)
        or not raw
        or "\0" in raw
        or "\\" in raw
        or unicodedata.normalize("NFC", raw) != raw
    ):
        raise ValueError(f"corpus specification {index} has an invalid path")
    relative = Path(raw)
    if (
        relative.is_absolute()
        or PureWindowsPath(raw).is_absolute()
        or relative.as_posix() != raw
        or any(part in {"", ".", ".."} for part in relative.parts)
    ):
        raise ValueError(
            f"corpus specification {index} path must be canonical and repository-relative"
        )

    repository = REPOSITORY.resolve()
    cursor = repository
    for part in relative.parts:
        cursor /= part
        if cursor.is_symlink():
            raise ValueError(
                f"corpus specification {index} path traverses a symlink: {raw}"
            )
    corpus = cursor.resolve()
    try:
        corpus.relative_to(repository)
    except ValueError as error:
        raise ValueError(
            f"corpus specification {index} path escapes the repository: {raw}"
        ) from error
    if not corpus.is_dir():
        raise ValueError(f"corpus specification {index} is not a directory: {raw}")
    return corpus


def _validate_configuration(configuration: dict[str, Any]) -> list[dict[str, Any]]:
    if set(configuration) != {"schema_version", "corpora"}:
        raise ValueError("expectation file keys must be ['corpora', 'schema_version']")
    if (
        type(configuration["schema_version"]) is not int
        or configuration["schema_version"] != 2
    ):
        raise ValueError("unsupported corpus expectation schema_version")
    corpora = configuration["corpora"]
    if not isinstance(corpora, list) or not corpora:
        raise ValueError("expectation file must declare a non-empty corpora list")

    names: set[str] = set()
    corpus_paths: set[Path] = set()
    prepared: list[dict[str, Any]] = []
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
            or "\0" in name
            or "/" in name
            or "\\" in name
            or name in {".", ".."}
            or unicodedata.normalize("NFC", name) != name
            or name in names
        ):
            raise ValueError(f"corpus specification {index} has an invalid/duplicate name")
        corpus = _resolve_repository_corpus(specification["path"], index=index)
        if corpus in corpus_paths:
            raise ValueError(f"corpus specification {index} has a duplicate path")
        expectation = _validate_expectation(
            specification["expect"], context=f"corpus specification {index} expect"
        )
        names.add(name)
        corpus_paths.add(corpus)
        prepared.append({"name": name, "corpus": corpus, "expect": expectation})
    return prepared


def _reject_output_inside_corpora(
    path: Path, corpora: list[dict[str, Any]], *, field: str
) -> None:
    resolved = path.resolve()
    for specification in corpora:
        corpus = specification["corpus"]
        try:
            resolved.relative_to(corpus)
        except ValueError:
            continue
        raise ValueError(f"{field} must not be inside corpus input: {corpus}")


def _require_reviewed_input_digest(
    corpus: Path,
    expectation: dict[str, Any],
    *,
    phase: str,
) -> str:
    expected = expectation.get(CORPUS_INPUT_BASELINE_FIELD)
    if not _valid_sha256(expected):
        raise ValueError(
            f"missing or invalid reviewed baseline {CORPUS_INPUT_BASELINE_FIELD}"
        )
    actual = corpus_input_digest(corpus)
    if actual != expected:
        raise ValueError(
            f"{phase} corpus input digest was {actual}, expected {expected}: {corpus}"
        )
    return actual


def evaluate_expectation(
    report: dict[str, Any],
    reference_graph: dict[str, Any],
    candidate_graph: dict[str, Any],
    expectation: dict[str, Any],
    *,
    corpus: Path,
) -> dict[str, Any]:
    failures: list[str] = []
    expected_input_digest = expectation.get(CORPUS_INPUT_BASELINE_FIELD)
    actual_input_digest = corpus_input_digest(corpus)
    valid_expected_input_digest = _valid_sha256(expected_input_digest)
    input_digest_matched = (
        valid_expected_input_digest and actual_input_digest == expected_input_digest
    )
    input_baseline = {
        "field": CORPUS_INPUT_BASELINE_FIELD,
        "expected": expected_input_digest,
        "actual": actual_input_digest,
        "matched": input_digest_matched,
    }
    if not valid_expected_input_digest:
        failures.append(
            f"missing or invalid reviewed baseline {CORPUS_INPUT_BASELINE_FIELD}"
        )
    elif not input_digest_matched:
        failures.append(
            f"corpus input digest was {actual_input_digest}, expected {expected_input_digest}"
        )
    digests = {
        "reference": strict_graph_digest(reference_graph, corpus=corpus),
        "candidate": strict_graph_digest(candidate_graph, corpus=corpus),
    }
    baseline_results: dict[str, Any] = {}
    for side, field in STRICT_BASELINE_FIELDS.items():
        expected = expectation.get(field)
        actual = digests[side]
        valid_expected = _valid_sha256(expected)
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

    cross_runtime_bindings = report["diagnostics"]["cross_runtime_bindings"]
    expected_candidate_bindings = expectation.get(
        "candidate_cross_runtime_bindings"
    )
    actual_candidate_bindings = cross_runtime_bindings["candidate"][
        "endpoint_count"
    ]
    if (
        expected_candidate_bindings is not None
        and actual_candidate_bindings != expected_candidate_bindings
    ):
        failures.append(
            "candidate cross-runtime bindings were "
            f"{actual_candidate_bindings}, expected {expected_candidate_bindings}"
        )
    minimum_reference_bindings = expectation.get(
        "reference_cross_runtime_bindings_min"
    )
    actual_reference_bindings = cross_runtime_bindings["reference"][
        "endpoint_count"
    ]
    if (
        minimum_reference_bindings is not None
        and actual_reference_bindings < minimum_reference_bindings
    ):
        failures.append(
            "reference cross-runtime bindings were "
            f"{actual_reference_bindings}, expected at least {minimum_reference_bindings}"
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
        "corpus_input_baseline": input_baseline,
        "strict_baselines": baseline_results,
    }


def run_suite(args: argparse.Namespace) -> dict[str, Any]:
    configuration = _load_object(args.expectations)
    corpora = _validate_configuration(configuration)

    _reject_output_inside_corpora(args.work_dir, corpora, field="work directory")
    if args.output is not None:
        _reject_output_inside_corpora(args.output, corpora, field="output")
    for specification in corpora:
        _require_reviewed_input_digest(
            specification["corpus"],
            specification["expect"],
            phase="preflight",
        )

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
    completed = False
    try:
        for specification in corpora:
            name = specification["name"]
            corpus = specification["corpus"]
            expectation = specification["expect"]
            _require_reviewed_input_digest(corpus, expectation, phase="pre-run")
            work = suite_work / name
            try:
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
                        fail_on_candidate_cross_runtime_bindings=True,
                        contract="reference-preserving",
                    )
                )
            finally:
                _require_reviewed_input_digest(
                    corpus, expectation, phase="post-run"
                )
            reference_path = Path(report["artifacts"]["reference"])
            candidate_path = Path(report["artifacts"]["candidate"])
            evaluation = evaluate_expectation(
                report,
                _load_object(reference_path),
                _load_object(candidate_path),
                expectation,
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
                    "reference_preserved": report["parity"][
                        "reference_preservation"
                    ]["preserved"],
                    "candidate_cross_runtime_bindings": report["diagnostics"][
                        "cross_runtime_bindings"
                    ]["candidate"]["endpoint_count"],
                    "corpus_input_baseline": evaluation["corpus_input_baseline"],
                    "strict_baselines": evaluation["strict_baselines"],
                    "report": str(report_path),
                }
            )
        completed = True
    finally:
        # Preserve the original failure (especially the more specific post-run
        # mutation error). A final sweep is only needed after every corpus run
        # completed successfully, when a later run could have changed an earlier
        # corpus after that corpus's own post-run check.
        if completed:
            for specification in corpora:
                _require_reviewed_input_digest(
                    specification["corpus"],
                    specification["expect"],
                    phase="final",
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
