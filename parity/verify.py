#!/usr/bin/env python3
"""Verify Graphoxide's case-by-case parity ledger against pinned Graphify tests.

The verifier deliberately separates inventory from claims of equivalence:

* ``manifest.json`` is the immutable, pinned upstream test inventory. Every
  inventory row starts explicitly unmapped.
* ``mappings/*.json`` contains reviewed claims that an exact executable
  Graphoxide test covers an exact upstream pytest node ID.
* ``divergences/*.json`` contains reviewed, executable evidence for intentional
  behavioral differences. Divergences count as accounted inventory, never as
  mapped parity.

That split lets independent porting batches add mappings without rewriting the
3,978-row inventory or silently turning an unreviewed case into a parity claim.
"""

from __future__ import annotations

import argparse
import ast
import collections
import dataclasses
import difflib
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any, Callable, Iterable, Mapping, Sequence


PARITY_DIR = Path(__file__).resolve().parent
REPO_ROOT = PARITY_DIR.parent
DEFAULT_LOCK = PARITY_DIR / "upstream.lock.json"
DEFAULT_MANIFEST = PARITY_DIR / "manifest.json"
DEFAULT_MAPPINGS = PARITY_DIR / "mappings"
DEFAULT_DIVERGENCES = PARITY_DIR / "divergences"
SCHEMA_VERSION = 1
UNMAPPED_REASON = "no_executable_parity_mapping"


class ParityError(RuntimeError):
    """Raised when the parity inventory or an executable mapping is invalid."""


@dataclasses.dataclass(frozen=True)
class Inventory:
    nodeids: tuple[str, ...]
    sources: tuple[str, ...]
    module_counts: Mapping[str, int]


@dataclasses.dataclass(frozen=True)
class Coverage:
    total: int
    mapped: int
    expected_divergences: int
    unmapped: int
    by_module: Mapping[str, tuple[int, int, int]]


CommandRunner = Callable[..., subprocess.CompletedProcess[str]]


def _load_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as error:
        raise ParityError(f"missing parity file: {path}") from error
    except json.JSONDecodeError as error:
        raise ParityError(f"invalid JSON in {path}: {error}") from error
    if not isinstance(value, dict):
        raise ParityError(f"{path} must contain a JSON object")
    return value


def canonical_nodeids_sha256(nodeids: Iterable[str]) -> str:
    payload = "".join(f"{nodeid}\n" for nodeid in nodeids).encode("utf-8")
    return hashlib.sha256(payload).hexdigest()


def source_from_nodeid(nodeid: str) -> str:
    if not isinstance(nodeid, str) or "::" not in nodeid:
        raise ParityError(f"invalid pytest node ID: {nodeid!r}")
    source = nodeid.split("::", 1)[0]
    if not source.startswith("tests/") or not source.endswith(".py"):
        raise ParityError(f"pytest node ID is outside tests/*.py: {nodeid!r}")
    return source


def module_from_source(source: str) -> str:
    if not source.startswith("tests/") or not source.endswith(".py"):
        raise ParityError(f"invalid upstream test source: {source!r}")
    return source[:-3].replace("/", ".")


def validate_lock(lock: Mapping[str, Any]) -> None:
    if lock.get("schema_version") != SCHEMA_VERSION:
        raise ParityError("unsupported upstream lock schema_version")
    for key in ("repository", "tag", "version", "commit"):
        if not isinstance(lock.get(key), str) or not lock[key]:
            raise ParityError(f"upstream lock has invalid {key!r}")
    commit = lock["commit"]
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ParityError("upstream lock commit must be a full lowercase Git SHA")
    pytest = lock.get("pytest")
    if not isinstance(pytest, dict):
        raise ParityError("upstream lock is missing pytest metadata")
    command = pytest.get("collect_command")
    if not isinstance(command, list) or not command or not all(
        isinstance(part, str) and part for part in command
    ):
        raise ParityError("pytest.collect_command must be a non-empty string array")
    for key in ("case_count", "module_count"):
        if not isinstance(pytest.get(key), int) or pytest[key] <= 0:
            raise ParityError(f"pytest.{key} must be a positive integer")
    if not re.fullmatch(r"[0-9a-f]{64}", str(pytest.get("nodeids_sha256", ""))):
        raise ParityError("pytest.nodeids_sha256 must be a lowercase SHA-256")


def validate_inventory(
    lock: Mapping[str, Any], manifest: Mapping[str, Any]
) -> Inventory:
    validate_lock(lock)
    if manifest.get("schema_version") != SCHEMA_VERSION:
        raise ParityError("unsupported parity manifest schema_version")

    upstream = manifest.get("upstream")
    if not isinstance(upstream, dict):
        raise ParityError("manifest is missing upstream metadata")
    for key in ("repository", "tag", "version", "commit"):
        if upstream.get(key) != lock.get(key):
            raise ParityError(f"manifest upstream {key} drifted from upstream.lock.json")

    expected_inventory = {
        key: lock["pytest"][key]
        for key in ("case_count", "module_count", "nodeids_sha256")
    }
    if manifest.get("inventory") != expected_inventory:
        raise ParityError("manifest inventory metadata drifted from upstream.lock.json")

    rows = manifest.get("cases")
    if not isinstance(rows, list):
        raise ParityError("manifest.cases must be an array")

    nodeids: list[str] = []
    module_counts: collections.Counter[str] = collections.Counter()
    source_counts: collections.Counter[str] = collections.Counter()
    seen: set[str] = set()
    required_case_keys = {"nodeid", "source", "module", "status", "reason"}

    for index, row in enumerate(rows):
        if not isinstance(row, dict):
            raise ParityError(f"manifest case {index} must be an object")
        if set(row) != required_case_keys:
            raise ParityError(
                f"manifest case {index} must have exactly {sorted(required_case_keys)}"
            )
        nodeid = row.get("nodeid")
        if not isinstance(nodeid, str):
            raise ParityError(f"manifest case {index} has a non-string nodeid")
        if nodeid in seen:
            raise ParityError(f"duplicate upstream pytest node ID: {nodeid}")
        seen.add(nodeid)

        source = source_from_nodeid(nodeid)
        module = module_from_source(source)
        if row.get("source") != source:
            raise ParityError(f"source classification mismatch for {nodeid}")
        if row.get("module") != module:
            raise ParityError(f"module classification mismatch for {nodeid}")
        if row.get("status") != "unmapped" or row.get("reason") != UNMAPPED_REASON:
            raise ParityError(
                f"base inventory case {nodeid} must be explicitly unmapped; "
                "put executable claims in parity/mappings/*.json"
            )

        nodeids.append(nodeid)
        module_counts[module] += 1
        source_counts[source] += 1

    pytest = lock["pytest"]
    if len(nodeids) != pytest["case_count"]:
        raise ParityError(
            f"upstream case-count drift: expected {pytest['case_count']}, got {len(nodeids)}"
        )
    if len(module_counts) != pytest["module_count"]:
        raise ParityError(
            f"upstream module-count drift: expected {pytest['module_count']}, "
            f"got {len(module_counts)}"
        )
    digest = canonical_nodeids_sha256(nodeids)
    if digest != pytest["nodeids_sha256"]:
        raise ParityError(
            "upstream node-ID inventory drift: "
            f"expected {pytest['nodeids_sha256']}, got {digest}"
        )

    declared_modules = manifest.get("modules")
    if not isinstance(declared_modules, list):
        raise ParityError("manifest.modules must be an array")
    expected_modules = [
        {
            "module": module_from_source(source),
            "source": source,
            "case_count": source_counts[source],
        }
        for source in sorted(source_counts)
    ]
    if declared_modules != expected_modules:
        raise ParityError("manifest module index drifted from its case classifications")

    return Inventory(
        nodeids=tuple(nodeids),
        sources=tuple(sorted(source_counts)),
        module_counts=dict(sorted(module_counts.items())),
    )


def _validate_target(target: Any, context: str) -> dict[str, str]:
    if not isinstance(target, dict):
        raise ParityError(f"{context}: target must be an object, not prose")
    runner = target.get("runner")
    if runner == "rust":
        expected = {"runner", "package", "id"}
        package = target.get("package")
        test_id = target.get("id")
        if set(target) != expected:
            raise ParityError(f"{context}: Rust target keys must be {sorted(expected)}")
        if not isinstance(package, str) or not re.fullmatch(
            r"[a-z0-9][a-z0-9-]*", package
        ):
            raise ParityError(f"{context}: invalid Cargo package name")
        if not isinstance(test_id, str) or not test_id or any(
            character.isspace() for character in test_id
        ):
            raise ParityError(f"{context}: invalid exact Rust test ID")
    elif runner == "vscode":
        expected = {"runner", "workspace", "id"}
        if set(target) != expected:
            raise ParityError(f"{context}: VS Code target keys must be {sorted(expected)}")
        if target.get("workspace") != "editors/vscode":
            raise ParityError(f"{context}: unsupported VS Code workspace")
        test_id = target.get("id")
        if not isinstance(test_id, str) or not test_id.strip() or "\n" in test_id:
            raise ParityError(f"{context}: invalid exact VS Code test ID")
    elif runner == "differential":
        expected = {"runner", "id"}
        if set(target) != expected:
            raise ParityError(
                f"{context}: differential target keys must be {sorted(expected)}"
            )
        test_id = target.get("id")
        if not isinstance(test_id, str) or not re.fullmatch(
            r"parity\.differential(?:\.[A-Za-z_][A-Za-z0-9_]*){2,}", test_id
        ):
            raise ParityError(
                f"{context}: differential ID must be an exact unittest ID under "
                "parity.differential"
            )
    else:
        raise ParityError(
            f"{context}: target runner must be rust, vscode, or differential"
        )
    return {str(key): str(value) for key, value in target.items()}


def load_mapping_overlays(
    mapping_dir: Path, inventory: Inventory
) -> dict[str, tuple[dict[str, str], ...]]:
    known = set(inventory.nodeids)
    mapped: dict[str, tuple[dict[str, str], ...]] = {}
    if not mapping_dir.exists():
        raise ParityError(f"missing mapping directory: {mapping_dir}")

    for path in sorted(mapping_dir.glob("*.json")):
        document = _load_json(path)
        if set(document) != {"schema_version", "mappings"}:
            raise ParityError(
                f"{path}: mapping file keys must be ['mappings', 'schema_version']"
            )
        if document.get("schema_version") != SCHEMA_VERSION:
            raise ParityError(f"{path}: unsupported schema_version")
        rows = document.get("mappings")
        if not isinstance(rows, list):
            raise ParityError(f"{path}: mappings must be an array")

        for index, row in enumerate(rows):
            context = f"{path}: mapping {index}"
            if not isinstance(row, dict) or set(row) != {"upstream", "targets"}:
                raise ParityError(
                    f"{context}: keys must be exactly ['targets', 'upstream']"
                )
            upstream = row.get("upstream")
            if not isinstance(upstream, str) or upstream not in known:
                raise ParityError(f"{context}: unknown upstream pytest node ID {upstream!r}")
            if upstream in mapped:
                raise ParityError(f"duplicate executable mapping for {upstream}")
            raw_targets = row.get("targets")
            if not isinstance(raw_targets, list) or not raw_targets:
                raise ParityError(f"{context}: targets must be a non-empty array")
            targets = tuple(
                _validate_target(target, f"{context}, target {target_index}")
                for target_index, target in enumerate(raw_targets)
            )
            canonical = [json.dumps(target, sort_keys=True) for target in targets]
            if len(canonical) != len(set(canonical)):
                raise ParityError(f"{context}: duplicate executable target")
            mapped[upstream] = targets

    return mapped


def load_divergence_overlays(
    divergence_dir: Path, inventory: Inventory
) -> dict[str, tuple[dict[str, str], ...]]:
    """Load reviewed behavioral differences without treating them as parity."""
    known = set(inventory.nodeids)
    divergent: dict[str, tuple[dict[str, str], ...]] = {}
    if not divergence_dir.exists():
        raise ParityError(f"missing divergence directory: {divergence_dir}")

    for path in sorted(divergence_dir.glob("*.json")):
        document = _load_json(path)
        if set(document) != {"schema_version", "divergences"}:
            raise ParityError(
                f"{path}: divergence file keys must be "
                "['divergences', 'schema_version']"
            )
        if document.get("schema_version") != SCHEMA_VERSION:
            raise ParityError(f"{path}: unsupported schema_version")
        rows = document.get("divergences")
        if not isinstance(rows, list):
            raise ParityError(f"{path}: divergences must be an array")

        for index, row in enumerate(rows):
            context = f"{path}: divergence {index}"
            required = {"upstream", "targets", "reason"}
            if not isinstance(row, dict) or set(row) != required:
                raise ParityError(
                    f"{context}: keys must be exactly {sorted(required)}"
                )
            upstream = row.get("upstream")
            if not isinstance(upstream, str) or upstream not in known:
                raise ParityError(f"{context}: unknown upstream pytest node ID {upstream!r}")
            if upstream in divergent:
                raise ParityError(f"duplicate expected divergence for {upstream}")
            reason = row.get("reason")
            if (
                not isinstance(reason, str)
                or not reason.strip()
                or reason != reason.strip()
                or "\n" in reason
            ):
                raise ParityError(f"{context}: reason must be one reviewed text line")
            raw_targets = row.get("targets")
            if not isinstance(raw_targets, list) or not raw_targets:
                raise ParityError(f"{context}: targets must be a non-empty array")
            targets = tuple(
                _validate_target(target, f"{context}, target {target_index}")
                for target_index, target in enumerate(raw_targets)
            )
            canonical = [json.dumps(target, sort_keys=True) for target in targets]
            if len(canonical) != len(set(canonical)):
                raise ParityError(f"{context}: duplicate executable target")
            divergent[upstream] = targets

    return divergent


def calculate_coverage(
    inventory: Inventory,
    mappings: Mapping[str, Sequence[Mapping[str, str]]],
    divergences: Mapping[str, Sequence[Mapping[str, str]]] | None = None,
) -> Coverage:
    divergences = divergences or {}
    overlap = set(mappings) & set(divergences)
    if overlap:
        raise ParityError(
            "upstream case cannot be both mapped parity and expected divergence: "
            f"{sorted(overlap)[0]}"
        )
    totals: collections.Counter[str] = collections.Counter()
    mapped_counts: collections.Counter[str] = collections.Counter()
    divergence_counts: collections.Counter[str] = collections.Counter()
    for nodeid in inventory.nodeids:
        module = module_from_source(source_from_nodeid(nodeid))
        totals[module] += 1
        if nodeid in mappings:
            mapped_counts[module] += 1
        elif nodeid in divergences:
            divergence_counts[module] += 1
    by_module = {
        module: (mapped_counts[module], divergence_counts[module], total)
        for module, total in sorted(totals.items())
    }
    mapped = len(mappings)
    expected_divergences = len(divergences)
    return Coverage(
        total=len(inventory.nodeids),
        mapped=mapped,
        expected_divergences=expected_divergences,
        unmapped=len(inventory.nodeids) - mapped - expected_divergences,
        by_module=by_module,
    )


def _run(
    command: Sequence[str], cwd: Path, runner: CommandRunner = subprocess.run
) -> subprocess.CompletedProcess[str]:
    try:
        completed = runner(
            list(command),
            cwd=cwd,
            text=True,
            capture_output=True,
            check=False,
        )
    except OSError as error:
        raise ParityError(f"could not execute {command[0]!r}: {error}") from error
    if completed.returncode != 0:
        output = "\n".join(part for part in (completed.stdout, completed.stderr) if part)
        raise ParityError(
            f"command failed ({' '.join(command)}):\n{output[-4000:]}"
        )
    return completed


def _missing_target_message(
    kind: str, wanted: str, available: Iterable[str]
) -> str:
    nearby = difflib.get_close_matches(wanted, list(available), n=3, cutoff=0.45)
    suffix = f"; closest: {', '.join(nearby)}" if nearby else ""
    return f"missing executable {kind} test ID {wanted!r}{suffix}"


def verify_executable_targets(
    mappings: Mapping[str, Sequence[Mapping[str, str]]],
    *,
    repo_root: Path = REPO_ROOT,
    execute_rust: bool = False,
    runner: CommandRunner = subprocess.run,
) -> int:
    unique: dict[str, Mapping[str, str]] = {}
    for targets in mappings.values():
        for target in targets:
            key = json.dumps(target, sort_keys=True)
            unique[key] = target

    rust: dict[str, set[str]] = collections.defaultdict(set)
    vscode: dict[str, set[str]] = collections.defaultdict(set)
    differential: set[str] = set()
    for target in unique.values():
        if target["runner"] == "rust":
            rust[target["package"]].add(target["id"])
        elif target["runner"] == "vscode":
            vscode[target["workspace"]].add(target["id"])
        else:
            differential.add(target["id"])

    for package, wanted_ids in sorted(rust.items()):
        completed = _run(
            ["cargo", "test", "-p", package, "--", "--list", "--format", "terse"],
            repo_root,
            runner,
        )
        available = {
            line[: -len(": test")]
            for line in completed.stdout.splitlines()
            if line.endswith(": test")
        }
        for wanted in sorted(wanted_ids):
            if wanted not in available:
                raise ParityError(_missing_target_message("Rust", wanted, available))
            if execute_rust:
                _run(
                    ["cargo", "test", "-p", package, wanted, "--", "--exact"],
                    repo_root,
                    runner,
                )

    for workspace, wanted_ids in sorted(vscode.items()):
        workspace_path = repo_root / workspace
        _run(["npm", "run", "compile", "--silent"], workspace_path, runner)
        test_files = sorted((workspace_path / "dist" / "test").glob("*.test.js"))
        if not test_files:
            raise ParityError(f"no compiled VS Code tests found under {workspace}/dist/test")
        completed = _run(
            ["node", "--test", "--test-reporter=tap", *map(str, test_files)],
            workspace_path,
            runner,
        )
        available = {
            match.group(1)
            for line in completed.stdout.splitlines()
            if (match := re.match(r"^\s*# Subtest: (.+)$", line))
        }
        for wanted in sorted(wanted_ids):
            if wanted not in available:
                raise ParityError(_missing_target_message("VS Code", wanted, available))

    for test_id in sorted(differential):
        _run([sys.executable, "-m", "unittest", test_id], repo_root, runner)

    return len(unique)


def collect_upstream_nodeids(
    checkout: Path,
    lock: Mapping[str, Any],
    *,
    runner: CommandRunner = subprocess.run,
) -> tuple[str, ...]:
    checkout = checkout.resolve()
    if not (checkout / ".git").exists():
        raise ParityError(f"upstream checkout is not a Git worktree: {checkout}")
    _verify_clean_pinned_checkout(
        checkout, str(lock["commit"]), runner=runner
    )
    command = lock["pytest"]["collect_command"]
    completed = _run(command, checkout, runner)
    _verify_clean_pinned_checkout(
        checkout, str(lock["commit"]), runner=runner
    )
    collected = tuple(
        line
        for line in completed.stdout.splitlines()
        if line.startswith("tests/") and "::" in line
    )
    if len(collected) != len(set(collected)):
        raise ParityError("live upstream pytest collection returned duplicate node IDs")
    return merge_source_inventory(checkout, collected)


def _verify_clean_pinned_checkout(
    checkout: Path,
    expected_commit: str,
    *,
    runner: CommandRunner = subprocess.run,
) -> None:
    """Reject a reference checkout whose tracked/non-ignored state is impure."""
    head = _run(["git", "rev-parse", "HEAD"], checkout, runner).stdout.strip()
    if head != expected_commit:
        raise ParityError(
            f"upstream checkout is at {head}, expected pinned {expected_commit}"
        )
    status = _run(
        ["git", "status", "--porcelain=v1", "--untracked-files=all"],
        checkout,
        runner,
    ).stdout
    if status.strip():
        first = status.splitlines()[0]
        raise ParityError(
            "upstream checkout has staged, unstaged, or non-ignored untracked "
            f"changes; refusing a contaminated reference ({first})"
        )


def merge_source_inventory(
    checkout: Path, collected: Sequence[str]
) -> tuple[str, ...]:
    """Add test modules skipped wholesale by optional imports to collection.

    A default pytest environment cannot report tests below a module-level
    ``pytest.importorskip``. Those cases are still upstream test contracts, so
    the authoritative inventory uses live pytest IDs where available and an
    AST-derived fallback only for wholly absent ``tests/test_*.py`` modules.
    Unsupported dynamic parametrization in a skipped module is an error instead
    of silently turning real tests into invisible debt.
    """

    by_source: dict[str, list[str]] = collections.defaultdict(list)
    for nodeid in collected:
        by_source[source_from_nodeid(nodeid)].append(nodeid)

    test_root = checkout / "tests"
    sources = [
        path.relative_to(checkout).as_posix()
        for path in sorted(test_root.glob("test_*.py"))
    ]
    merged: list[str] = []
    for source in sources:
        if source in by_source:
            merged.extend(by_source.pop(source))
        else:
            fallback = source_nodeids(checkout / source, source)
            if not fallback:
                raise ParityError(
                    f"pytest skipped {source} and source fallback found no tests"
                )
            merged.extend(fallback)
    if by_source:
        unknown = sorted(by_source)[0]
        raise ParityError(f"pytest collected an unexpected test source: {unknown}")
    if len(merged) != len(set(merged)):
        raise ParityError("merged upstream inventory contains duplicate node IDs")
    return tuple(merged)


def source_nodeids(path: Path, source: str) -> tuple[str, ...]:
    """Statically enumerate pytest IDs in one wholly skipped source module."""

    try:
        tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    except (OSError, SyntaxError, UnicodeError) as error:
        raise ParityError(f"could not parse skipped test module {source}: {error}") from error

    nodeids: list[str] = []

    def visit(body: Sequence[ast.stmt], parents: tuple[str, ...] = ()) -> None:
        for statement in body:
            if isinstance(statement, ast.ClassDef) and statement.name.startswith("Test"):
                visit(statement.body, (*parents, statement.name))
            elif isinstance(statement, (ast.FunctionDef, ast.AsyncFunctionDef)) and statement.name.startswith("test"):
                base = "::".join((source, *parents, statement.name))
                ids = _static_parametrize_ids(statement, source)
                nodeids.extend(base if suffix is None else f"{base}[{suffix}]" for suffix in ids)

    visit(tree.body)
    return tuple(nodeids)


def _static_parametrize_ids(
    function: ast.FunctionDef | ast.AsyncFunctionDef, source: str
) -> tuple[str | None, ...]:
    parameter_sets: list[tuple[str, ...]] = []
    for decorator in function.decorator_list:
        if not (
            isinstance(decorator, ast.Call)
            and _attribute_name(decorator.func).endswith(".parametrize")
        ):
            continue
        if len(decorator.args) < 2:
            raise ParityError(
                f"unsupported parametrize in skipped {source}::{function.name}"
            )
        names_value = _literal(decorator.args[0], source, function.name)
        if isinstance(names_value, str):
            names = tuple(name.strip() for name in names_value.split(","))
        elif isinstance(names_value, (tuple, list)) and all(
            isinstance(name, str) for name in names_value
        ):
            names = tuple(names_value)
        else:
            raise ParityError(
                f"non-static parameter names in skipped {source}::{function.name}"
            )
        raw_values = decorator.args[1]
        if not isinstance(raw_values, (ast.List, ast.Tuple)):
            raise ParityError(
                f"non-static parameter values in skipped {source}::{function.name}"
            )
        explicit_ids = next(
            (keyword.value for keyword in decorator.keywords if keyword.arg == "ids"),
            None,
        )
        ids_list: list[str] | None = None
        if explicit_ids is not None:
            literal_ids = _literal(explicit_ids, source, function.name)
            if not isinstance(literal_ids, (list, tuple)) or not all(
                item is None or isinstance(item, str) for item in literal_ids
            ):
                raise ParityError(
                    f"non-static parametrize ids in skipped {source}::{function.name}"
                )
            ids_list = ["" if item is None else item for item in literal_ids]

        one_decorator: list[str] = []
        for index, value_node in enumerate(raw_values.elts):
            pytest_id = _pytest_param_id(value_node, names, index, source, function.name)
            if ids_list is not None and index < len(ids_list) and ids_list[index]:
                pytest_id = ids_list[index]
            one_decorator.append(pytest_id)
        parameter_sets.append(tuple(one_decorator))

    if not parameter_sets:
        return (None,)
    combined: tuple[str, ...] = ("",)
    # Multiple decorators form a Cartesian product. The current pinned skipped
    # modules use one decorator, but supporting products avoids a quiet future
    # undercount when a skipped module grows another static parametrization.
    for values in parameter_sets:
        combined = tuple(
            "-".join(part for part in (prefix, value) if part)
            for prefix in combined
            for value in values
        )
    return combined


def _attribute_name(node: ast.expr) -> str:
    if isinstance(node, ast.Name):
        return node.id
    if isinstance(node, ast.Attribute):
        prefix = _attribute_name(node.value)
        return f"{prefix}.{node.attr}" if prefix else node.attr
    return ""


def _literal(node: ast.AST, source: str, function: str) -> Any:
    try:
        return ast.literal_eval(node)
    except (ValueError, TypeError, SyntaxError) as error:
        raise ParityError(
            f"non-static parametrization in skipped {source}::{function}"
        ) from error


def _pytest_param_id(
    node: ast.expr,
    names: Sequence[str],
    index: int,
    source: str,
    function: str,
) -> str:
    if isinstance(node, ast.Call) and _attribute_name(node.func).endswith(".param"):
        explicit = next(
            (keyword.value for keyword in node.keywords if keyword.arg == "id"), None
        )
        if explicit is not None:
            value = _literal(explicit, source, function)
            if not isinstance(value, str):
                raise ParityError(
                    f"non-string pytest.param id in skipped {source}::{function}"
                )
            return value
        values = [_literal(value, source, function) for value in node.args]
    else:
        literal = _literal(node, source, function)
        values = list(literal) if len(names) > 1 and isinstance(literal, (list, tuple)) else [literal]
    if len(values) != len(names):
        raise ParityError(
            f"parameter arity mismatch in skipped {source}::{function}"
        )
    return "-".join(
        _pytest_value_id(value, names[position], index)
        for position, value in enumerate(values)
    )


def _pytest_value_id(value: Any, name: str, index: int) -> str:
    if value is None:
        return "None"
    if isinstance(value, bool):
        return str(value)
    if isinstance(value, (str, int, float)):
        return str(value)
    if isinstance(value, bytes):
        return value.decode("ascii", errors="backslashreplace")
    return f"{name}{index}"


def compare_upstream_collection(
    expected: Sequence[str], actual: Sequence[str]
) -> None:
    if tuple(expected) == tuple(actual):
        return
    expected_set = set(expected)
    actual_set = set(actual)
    missing = sorted(expected_set - actual_set)
    added = sorted(actual_set - expected_set)
    if missing or added:
        detail = []
        if missing:
            detail.append(f"missing {len(missing)} (first: {missing[0]})")
        if added:
            detail.append(f"added {len(added)} (first: {added[0]})")
        raise ParityError("pinned upstream inventory drift: " + "; ".join(detail))
    first = next(
        index
        for index, (left, right) in enumerate(zip(expected, actual))
        if left != right
    )
    raise ParityError(
        "pinned upstream inventory order drift at index "
        f"{first}: expected {expected[first]!r}, got {actual[first]!r}"
    )


def verify_repository(
    *,
    lock_path: Path = DEFAULT_LOCK,
    manifest_path: Path = DEFAULT_MANIFEST,
    mapping_dir: Path = DEFAULT_MAPPINGS,
    divergence_dir: Path = DEFAULT_DIVERGENCES,
    upstream_checkout: Path | None = None,
    require_complete: bool = False,
    check_targets: bool = True,
    execute_rust: bool = False,
    repo_root: Path = REPO_ROOT,
) -> tuple[Coverage, int]:
    lock = _load_json(lock_path)
    manifest = _load_json(manifest_path)
    inventory = validate_inventory(lock, manifest)
    mappings = load_mapping_overlays(mapping_dir, inventory)
    divergences = load_divergence_overlays(divergence_dir, inventory)
    coverage = calculate_coverage(inventory, mappings, divergences)

    if upstream_checkout is not None:
        actual = collect_upstream_nodeids(upstream_checkout, lock)
        compare_upstream_collection(inventory.nodeids, actual)
    if require_complete and coverage.unmapped:
        first_unmapped = next(
            nodeid
            for nodeid in inventory.nodeids
            if nodeid not in mappings and nodeid not in divergences
        )
        raise ParityError(
            f"parity ledger incomplete: {coverage.unmapped} upstream cases lack an "
            "executable parity mapping or reviewed expected divergence "
            f"(first: {first_unmapped})"
        )
    executable_claims = {**mappings, **divergences}
    checked_targets = (
        verify_executable_targets(
            executable_claims,
            repo_root=repo_root,
            execute_rust=execute_rust,
        )
        if check_targets
        else 0
    )
    return coverage, checked_targets


def _report_payload(coverage: Coverage) -> dict[str, Any]:
    return {
        "cases": coverage.total,
        "mapped": coverage.mapped,
        "expected_divergences": coverage.expected_divergences,
        "unmapped": coverage.unmapped,
        "modules": [
            {
                "module": module,
                "mapped": counts[0],
                "expected_divergences": counts[1],
                "unmapped": counts[2] - counts[0] - counts[1],
                "total": counts[2],
            }
            for module, counts in coverage.by_module.items()
        ],
    }


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lock", type=Path, default=DEFAULT_LOCK)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--mappings", type=Path, default=DEFAULT_MAPPINGS)
    parser.add_argument("--divergences", type=Path, default=DEFAULT_DIVERGENCES)
    subparsers = parser.add_subparsers(dest="command", required=True)

    check = subparsers.add_parser("check", help="verify inventory and executable mappings")
    check.add_argument("--upstream-checkout", type=Path)
    check.add_argument("--require-complete", action="store_true")
    check.add_argument("--skip-target-checks", action="store_true")
    check.add_argument("--execute-rust-targets", action="store_true")

    report = subparsers.add_parser("report", help="report case and module parity debt")
    report.add_argument("--json", action="store_true")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        coverage, checked = verify_repository(
            lock_path=args.lock,
            manifest_path=args.manifest,
            mapping_dir=args.mappings,
            divergence_dir=args.divergences,
            upstream_checkout=getattr(args, "upstream_checkout", None),
            require_complete=getattr(args, "require_complete", False),
            check_targets=(
                args.command == "check" and not getattr(args, "skip_target_checks", False)
            ),
            execute_rust=getattr(args, "execute_rust_targets", False),
        )
    except ParityError as error:
        print(f"parity verification failed: {error}", file=sys.stderr)
        return 1

    if args.command == "report":
        payload = _report_payload(coverage)
        if args.json:
            print(json.dumps(payload, indent=2, sort_keys=True))
        else:
            print("mapped  divergent  unmapped  total  module")
            for row in payload["modules"]:
                print(
                    f"{row['mapped']:>6}  {row['expected_divergences']:>9}  "
                    f"{row['unmapped']:>8}  "
                    f"{row['total']:>5}  {row['module']}"
                )
            print(
                f"\n{coverage.mapped}/{coverage.total} mapped parity; "
                f"{coverage.expected_divergences} reviewed expected divergences; "
                f"{coverage.unmapped} unaccounted"
            )
    else:
        print(
            f"parity inventory OK: {coverage.total} cases in "
            f"{len(coverage.by_module)} modules; {coverage.mapped} mapped, "
            f"{coverage.expected_divergences} expected divergences, "
            f"{coverage.unmapped} unaccounted; "
            f"{checked} executable targets checked"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
