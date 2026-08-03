from __future__ import annotations

import copy
import json
import subprocess
import tempfile
import unittest
from pathlib import Path

from parity.verify import (
    DEFAULT_LOCK,
    DEFAULT_MANIFEST,
    DEFAULT_MAPPINGS,
    DEFAULT_DIVERGENCES,
    ParityError,
    _load_json,
    _validate_target,
    _verify_clean_pinned_checkout,
    calculate_coverage,
    canonical_nodeids_sha256,
    compare_upstream_collection,
    collect_upstream_nodeids,
    load_mapping_overlays,
    load_divergence_overlays,
    merge_source_inventory,
    source_nodeids,
    validate_inventory,
    verify_executable_targets,
    verify_repository,
)


def fixture_lock(nodeids: list[str]) -> dict:
    modules = {nodeid.split("::", 1)[0] for nodeid in nodeids}
    return {
        "schema_version": 1,
        "repository": "https://example.invalid/graphify.git",
        "tag": "v1",
        "version": "1",
        "commit": "a" * 40,
        "pytest": {
            "collect_command": ["pytest", "--collect-only", "-q"],
            "case_count": len(nodeids),
            "module_count": len(modules),
            "nodeids_sha256": canonical_nodeids_sha256(nodeids),
        },
    }


def fixture_manifest(lock: dict, nodeids: list[str]) -> dict:
    sources = sorted({nodeid.split("::", 1)[0] for nodeid in nodeids})
    return {
        "schema_version": 1,
        "upstream": {
            key: lock[key] for key in ("repository", "tag", "version", "commit")
        },
        "inventory": {
            key: lock["pytest"][key]
            for key in ("case_count", "module_count", "nodeids_sha256")
        },
        "modules": [
            {
                "module": source[:-3].replace("/", "."),
                "source": source,
                "case_count": sum(
                    nodeid.startswith(source + "::") for nodeid in nodeids
                ),
            }
            for source in sources
        ],
        "cases": [
            {
                "nodeid": nodeid,
                "source": nodeid.split("::", 1)[0],
                "module": nodeid.split("::", 1)[0][:-3].replace("/", "."),
                "status": "unmapped",
                "reason": "no_executable_parity_mapping",
            }
            for nodeid in nodeids
        ],
    }


class InventoryTests(unittest.TestCase):
    def setUp(self) -> None:
        self.nodeids = [
            "tests/test_alpha.py::test_one",
            "tests/test_alpha.py::test_many[first]",
            "tests/test_beta.py::TestGroup::test_two",
        ]
        self.lock = fixture_lock(self.nodeids)
        self.manifest = fixture_manifest(self.lock, self.nodeids)

    def test_checked_in_inventory_has_every_pinned_case(self) -> None:
        inventory = validate_inventory(
            _load_json(DEFAULT_LOCK), _load_json(DEFAULT_MANIFEST)
        )
        self.assertEqual(len(inventory.nodeids), 3978)
        self.assertEqual(len(inventory.module_counts), 176)
        self.assertEqual(len(set(inventory.nodeids)), 3978)

    def test_valid_inventory_preserves_module_classification(self) -> None:
        inventory = validate_inventory(self.lock, self.manifest)
        self.assertEqual(inventory.module_counts["tests.test_alpha"], 2)
        self.assertEqual(inventory.module_counts["tests.test_beta"], 1)

    def test_missing_upstream_case_fails_even_if_rows_are_well_formed(self) -> None:
        self.manifest["cases"].pop()
        with self.assertRaisesRegex(ParityError, "case-count drift"):
            validate_inventory(self.lock, self.manifest)

    def test_changed_nodeid_fails_pinned_hash(self) -> None:
        row = self.manifest["cases"][0]
        row["nodeid"] = "tests/test_alpha.py::test_changed"
        with self.assertRaisesRegex(ParityError, "node-ID inventory drift"):
            validate_inventory(self.lock, self.manifest)

    def test_duplicate_nodeid_fails(self) -> None:
        self.manifest["cases"][1] = copy.deepcopy(self.manifest["cases"][0])
        with self.assertRaisesRegex(ParityError, "duplicate upstream pytest node ID"):
            validate_inventory(self.lock, self.manifest)

    def test_wrong_module_classification_fails(self) -> None:
        self.manifest["cases"][0]["module"] = "tests.test_beta"
        with self.assertRaisesRegex(ParityError, "module classification mismatch"):
            validate_inventory(self.lock, self.manifest)

    def test_inventory_cannot_claim_mapping_without_overlay(self) -> None:
        self.manifest["cases"][0]["status"] = "mapped"
        with self.assertRaisesRegex(ParityError, "must be explicitly unmapped"):
            validate_inventory(self.lock, self.manifest)

    def test_upstream_comparison_detects_set_and_order_drift(self) -> None:
        with self.assertRaisesRegex(ParityError, "missing 1"):
            compare_upstream_collection(self.nodeids, self.nodeids[:-1])
        reordered = [self.nodeids[1], self.nodeids[0], self.nodeids[2]]
        with self.assertRaisesRegex(ParityError, "order drift"):
            compare_upstream_collection(self.nodeids, reordered)

    def test_checkout_purity_rejects_tracked_or_nonignored_changes(self) -> None:
        outputs = {
            "rev-parse": self.lock["commit"] + "\n",
            "status": " M uv.lock\n?? new_source.py\n",
            "ls-files": "",
        }

        def fake_runner(command, **_kwargs):
            operation = command[1]
            return subprocess.CompletedProcess(
                args=command,
                returncode=0,
                stdout=outputs[operation],
                stderr="",
            )

        with self.assertRaisesRegex(ParityError, "contaminated reference"):
            _verify_clean_pinned_checkout(
                Path("."), self.lock["commit"], runner=fake_runner
            )

    def test_live_collection_rechecks_purity_after_pytest(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            checkout = Path(temporary)
            (checkout / ".git").mkdir()
            statuses = iter(("", " M uv.lock\n"))

            def fake_runner(command, **_kwargs):
                if command[:3] == ["git", "rev-parse", "HEAD"]:
                    stdout = self.lock["commit"] + "\n"
                elif command[:2] == ["git", "status"]:
                    stdout = next(statuses)
                elif command[:2] == ["git", "ls-files"]:
                    stdout = ""
                else:
                    stdout = self.nodeids[0] + "\n"
                return subprocess.CompletedProcess(
                    args=command,
                    returncode=0,
                    stdout=stdout,
                    stderr="",
                )

            with self.assertRaisesRegex(ParityError, "contaminated reference"):
                collect_upstream_nodeids(
                    checkout, self.lock, runner=fake_runner
                )

    def test_source_fallback_expands_static_parametrization_exactly(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            tests = root / "tests"
            tests.mkdir()
            source = tests / "test_optional.py"
            source.write_text(
                """\
import pytest
pytest.importorskip('optional_dependency')

def test_plain():
    pass

@pytest.mark.parametrize(('value', 'expected'), [(None, 8), ('', 8), ('bad', 8), (0, 1), (-4, 1), (3, 3)])
def test_many(value, expected):
    pass
""",
                encoding="utf-8",
            )
            self.assertEqual(
                source_nodeids(source, "tests/test_optional.py"),
                (
                    "tests/test_optional.py::test_plain",
                    "tests/test_optional.py::test_many[None-8]",
                    "tests/test_optional.py::test_many[-8]",
                    "tests/test_optional.py::test_many[bad-8]",
                    "tests/test_optional.py::test_many[0-1]",
                    "tests/test_optional.py::test_many[-4-1]",
                    "tests/test_optional.py::test_many[3-3]",
                ),
            )
            self.assertEqual(
                merge_source_inventory(root, ["tests/test_optional.py::test_plain"]),
                ("tests/test_optional.py::test_plain",),
            )


class MappingTests(unittest.TestCase):
    def setUp(self) -> None:
        self.nodeids = [
            "tests/test_alpha.py::test_one",
            "tests/test_beta.py::test_two",
        ]
        self.lock = fixture_lock(self.nodeids)
        self.inventory = validate_inventory(
            self.lock, fixture_manifest(self.lock, self.nodeids)
        )

    def write_mapping(self, directory: Path, document: dict, name: str = "one.json") -> None:
        (directory / name).write_text(json.dumps(document), encoding="utf-8")

    def mapping_document(self, upstream: str | None = None) -> dict:
        return {
            "schema_version": 1,
            "mappings": [
                {
                    "upstream": upstream or self.nodeids[0],
                    "targets": [
                        {
                            "runner": "rust",
                            "package": "graphoxide-core",
                            "id": "ids::tests::matches_upstream_vectors",
                        }
                    ],
                }
            ],
        }

    def divergence_document(self, upstream: str | None = None) -> dict:
        return {
            "schema_version": 1,
            "divergences": [
                {
                    "upstream": upstream or self.nodeids[0],
                    "targets": [
                        {
                            "runner": "rust",
                            "package": "graphoxide-core",
                            "id": "ids::tests::documents_intentional_difference",
                        }
                    ],
                    "reason": "Graphoxide intentionally keeps the safer offline behavior.",
                }
            ],
        }

    def test_overlay_turns_one_explicit_debt_row_into_mapped_coverage(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            self.write_mapping(directory, self.mapping_document())
            mappings = load_mapping_overlays(directory, self.inventory)
        coverage = calculate_coverage(self.inventory, mappings)
        self.assertEqual((coverage.mapped, coverage.unmapped), (1, 1))

    def test_unknown_upstream_id_fails(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            self.write_mapping(
                directory,
                self.mapping_document("tests/test_missing.py::test_missing"),
            )
            with self.assertRaisesRegex(ParityError, "unknown upstream pytest node ID"):
                load_mapping_overlays(directory, self.inventory)

    def test_expected_divergence_is_accounted_but_never_mapped_parity(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            mappings_dir = root / "mappings"
            divergences_dir = root / "divergences"
            mappings_dir.mkdir()
            divergences_dir.mkdir()
            self.write_mapping(mappings_dir, self.mapping_document(self.nodeids[0]))
            (divergences_dir / "one.json").write_text(
                json.dumps(self.divergence_document(self.nodeids[1])),
                encoding="utf-8",
            )
            mappings = load_mapping_overlays(mappings_dir, self.inventory)
            divergences = load_divergence_overlays(
                divergences_dir, self.inventory
            )
        coverage = calculate_coverage(self.inventory, mappings, divergences)
        self.assertEqual(coverage.mapped, 1)
        self.assertEqual(coverage.expected_divergences, 1)
        self.assertEqual(coverage.unmapped, 0)
        self.assertEqual(coverage.by_module["tests.test_alpha"], (1, 0, 1))
        self.assertEqual(coverage.by_module["tests.test_beta"], (0, 1, 1))

    def test_mapping_and_divergence_overlap_fails(self) -> None:
        target = self.mapping_document()["mappings"][0]["targets"]
        with self.assertRaisesRegex(ParityError, "both mapped parity"):
            calculate_coverage(
                self.inventory,
                {self.nodeids[0]: tuple(target)},
                {self.nodeids[0]: tuple(target)},
            )

    def test_divergence_requires_a_reviewed_reason(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            document = self.divergence_document()
            document["divergences"][0]["reason"] = ""
            (directory / "one.json").write_text(
                json.dumps(document), encoding="utf-8"
            )
            with self.assertRaisesRegex(ParityError, "reviewed text line"):
                load_divergence_overlays(directory, self.inventory)

    def test_duplicate_mapping_across_batches_fails(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            document = self.mapping_document()
            self.write_mapping(directory, document, "a.json")
            self.write_mapping(directory, document, "b.json")
            with self.assertRaisesRegex(ParityError, "duplicate executable mapping"):
                load_mapping_overlays(directory, self.inventory)

    def test_prose_is_not_an_executable_target(self) -> None:
        with self.assertRaisesRegex(ParityError, "not prose"):
            _validate_target("covered by the parser tests", "fixture")

    def test_all_target_runners_require_exact_identifiers(self) -> None:
        self.assertEqual(
            _validate_target(
                {
                    "runner": "vscode",
                    "workspace": "editors/vscode",
                    "id": "parses and indexes a Graphoxide graph",
                },
                "fixture",
            )["runner"],
            "vscode",
        )
        self.assertEqual(
            _validate_target(
                {
                    "runner": "differential",
                    "id": "parity.differential.test_extract.ExtractParity.test_python",
                },
                "fixture",
            )["runner"],
            "differential",
        )

    def test_missing_rust_test_id_fails_target_discovery(self) -> None:
        mappings = {
            self.nodeids[0]: (
                {
                    "runner": "rust",
                    "package": "graphoxide-core",
                    "id": "ids::tests::missing",
                },
            )
        }

        def fake_runner(*_args, **_kwargs):
            return subprocess.CompletedProcess(
                args=[],
                returncode=0,
                stdout="ids::tests::matches_upstream_vectors: test\n",
                stderr="",
            )

        with self.assertRaisesRegex(ParityError, "missing executable Rust test ID"):
            verify_executable_targets(mappings, runner=fake_runner)

    def test_strict_gate_accepts_checked_in_complete_registry(self) -> None:
        coverage, checked = verify_repository(
            lock_path=DEFAULT_LOCK,
            manifest_path=DEFAULT_MANIFEST,
            mapping_dir=DEFAULT_MAPPINGS,
            divergence_dir=DEFAULT_DIVERGENCES,
            require_complete=True,
            check_targets=False,
        )
        self.assertEqual(coverage.total, 3978)
        self.assertEqual(coverage.mapped, 3975)
        self.assertEqual(coverage.expected_divergences, 3)
        self.assertEqual(coverage.unmapped, 0)
        self.assertEqual(checked, 0)


if __name__ == "__main__":
    unittest.main()
