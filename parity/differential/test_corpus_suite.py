from __future__ import annotations

import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

from parity.differential.corpus_suite import (
    _contains_record,
    _load_object as load_corpus_object,
    _reject_output_inside_corpora,
    _resolve_repository_corpus,
    _validate_edge_contract_list,
    corpus_input_digest,
    evaluate_expectation,
    run_suite,
    strict_graph_digest,
)


def report(
    *,
    preserved: bool,
    reference_bindings: int = 0,
    candidate_bindings: int = 0,
):
    return {
        "parity": {"reference_preservation": {"preserved": preserved}},
        "diagnostics": {
            "pre_normalization": {
                "reference": {"violation_count": 0},
                "candidate": {"violation_count": 0},
            },
            "cross_runtime_bindings": {
                "reference": {"endpoint_count": reference_bindings},
                "candidate": {"endpoint_count": candidate_bindings},
            },
        },
    }


def reviewed(
    expectation: dict,
    reference_graph: dict,
    candidate_graph: dict,
    *,
    corpus: Path,
) -> dict:
    return {
        **expectation,
        "corpus_input_sha256": corpus_input_digest(corpus),
        "reference_strict_sha256": strict_graph_digest(
            reference_graph, corpus=corpus
        ),
        "candidate_strict_sha256": strict_graph_digest(
            candidate_graph, corpus=corpus
        ),
    }


class CorpusExpectationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.corpus = Path(self.temporary.name)
        (self.corpus / "fixture.txt").write_text("fixture\n", encoding="utf-8")

    def test_reference_preservation_expectation(self):
        graph = {"nodes": [], "links": []}
        result = evaluate_expectation(
            report(preserved=True),
            graph,
            graph,
            reviewed(
                {
                    "reference_preserved": True,
                    "candidate_cross_runtime_bindings": 0,
                },
                graph,
                graph,
                corpus=self.corpus,
            ),
            corpus=self.corpus,
        )
        self.assertTrue(result["passed"])

    def test_required_and_forbidden_edges_are_semantic_partial_matches(self):
        graph = {
            "nodes": [{"id": "child"}, {"id": "safe"}],
            "links": [
                {
                    "source": "child",
                    "target": "safe",
                    "relation": "inherits",
                    "confidence": "EXTRACTED",
                }
            ],
        }
        result = evaluate_expectation(
            report(preserved=False, reference_bindings=3),
            graph,
            graph,
            reviewed(
                {
                    "candidate_cross_runtime_bindings": 0,
                    "reference_cross_runtime_bindings_min": 3,
                    "required_candidate_edges": [
                        {"source": "child", "target": "safe", "relation": "inherits"}
                    ],
                    "forbidden_candidate_edges": [
                        {"source": "child", "target": "unsafe", "relation": "inherits"}
                    ],
                },
                graph,
                graph,
                corpus=self.corpus,
            ),
            corpus=self.corpus,
        )
        self.assertTrue(result["passed"])

    def test_partial_match_requires_present_keys_and_strict_json_types(self):
        self.assertFalse(_contains_record([{"source": "a"}], {"context": None}))
        self.assertTrue(_contains_record([{"context": None}], {"context": None}))
        self.assertFalse(_contains_record([{"line": 1}], {"line": True}))
        self.assertFalse(_contains_record([{"line": True}], {"line": 1}))
        self.assertTrue(_contains_record([{"line": 1}], {"line": 1.0}))

    def test_edge_contract_records_are_fully_typed(self):
        _validate_edge_contract_list(
            [
                {
                    "source": "a",
                    "target": "b",
                    "relation": "calls",
                    "confidence": "EXTRACTED",
                    "confidence_score": 1.0,
                    "source_file": None,
                    "source_location": "L4",
                    "line": 4,
                    "line_number": None,
                    "context": "call",
                    "key": 0,
                }
            ],
            field="required_candidate_edges",
            context="corpus fixture expect",
        )
        invalid = [
            (None, "must be an array"),
            (["edge"], "must be an object"),
            ([{"source": "a", "target": "b", "relation": "calls", "typo": 1}], "unknown keys"),
            ([{"source": "a", "target": "b"}], "missing required keys"),
            ([{"source": True, "target": "b", "relation": "calls"}], "source must"),
            ([{"source": "a", "target": 2, "relation": "calls"}], "target must"),
            ([{"source": "a", "target": "b", "relation": 1}], "relation must"),
            ([{"source": "a", "target": "b", "relation": "calls", "context": 1}], "context must"),
            ([{"source": "a", "target": "b", "relation": "calls", "line": True}], "line must"),
            ([{"source": "a", "target": "b", "relation": "calls", "confidence_score": float("inf")}], "confidence_score must"),
            ([{"source": "a", "target": "b", "relation": "calls", "key": []}], "key must"),
        ]
        for value, message in invalid:
            with self.subTest(value=value):
                with self.assertRaisesRegex(ValueError, message):
                    _validate_edge_contract_list(
                        value,
                        field="required_candidate_edges",
                        context="corpus fixture expect",
                    )

        _validate_edge_contract_list(
            [
                {
                    "source": "a",
                    "target": "b",
                    "relation": "calls",
                    "confidence_score": 10**1000,
                    "key": 10**1000,
                }
            ],
            field="required_candidate_edges",
            context="corpus fixture expect",
        )

    def test_configured_corpus_path_is_canonical_contained_and_link_free(self):
        with tempfile.TemporaryDirectory() as temporary:
            repository = Path(temporary) / "repository"
            repository.mkdir()
            (repository / "corpus").mkdir()
            with mock.patch(
                "parity.differential.corpus_suite.REPOSITORY", repository
            ):
                self.assertEqual(
                    _resolve_repository_corpus("corpus", index=0),
                    (repository / "corpus").resolve(),
                )
                invalid = [
                    "",
                    "./corpus",
                    "corpus/../corpus",
                    "../outside",
                    str((repository.parent / "outside").resolve()),
                    r"C:\outside\corpus",
                    "missing",
                ]
                for raw in invalid:
                    with self.subTest(raw=raw):
                        with self.assertRaises(ValueError):
                            _resolve_repository_corpus(raw, index=0)

                outside = repository.parent / "outside"
                outside.mkdir()
                (repository / "linked").symlink_to(outside, target_is_directory=True)
                with self.assertRaisesRegex(ValueError, "traverses a symlink"):
                    _resolve_repository_corpus("linked", index=0)

    def test_outputs_inside_corpus_input_are_rejected(self):
        corpora = [{"corpus": self.corpus.resolve()}]
        for path in (self.corpus, self.corpus / "work", self.corpus / "report.json"):
            with self.subTest(path=path):
                with self.assertRaisesRegex(ValueError, "must not be inside corpus"):
                    _reject_output_inside_corpora(path, corpora, field="output")
        _reject_output_inside_corpora(
            self.corpus.parent / "outside-report.json", corpora, field="output"
        )

    def test_expectation_loader_rejects_duplicate_json_keys(self):
        expectations = self.corpus.parent / "duplicate-expectations.json"
        expectations.write_text(
            '{"schema_version":2,"schema_version":2,"corpora":[]}',
            encoding="utf-8",
        )
        with self.assertRaisesRegex(ValueError, "duplicate JSON object key"):
            load_corpus_object(expectations)

    def test_run_suite_wires_validated_cross_runtime_contract(self):
        with tempfile.TemporaryDirectory() as temporary:
            repository = Path(temporary) / "repository"
            repository.mkdir()
            corpus = repository / "corpus"
            corpus.mkdir()
            (corpus / "fixture.py").write_text("def fixture(): pass\n", encoding="utf-8")
            graph = {"nodes": [], "links": []}
            expectation = reviewed(
                {
                    "reference_preserved": True,
                    "candidate_cross_runtime_bindings": 0,
                },
                graph,
                graph,
                corpus=corpus,
            )
            expectations = repository / "expectations.json"
            expectations.write_text(
                json.dumps(
                    {
                        "schema_version": 2,
                        "corpora": [
                            {"name": "fixture", "path": "corpus", "expect": expectation}
                        ],
                    }
                ),
                encoding="utf-8",
            )
            work_root = Path(temporary) / "work"

            def fake_build(arguments):
                self.assertEqual(arguments.corpus, corpus.resolve())
                self.assertTrue(arguments.fail_on_candidate_cross_runtime_bindings)
                self.assertEqual(arguments.contract, "reference-preserving")
                arguments.work_dir.mkdir(parents=True, exist_ok=True)
                reference = arguments.work_dir / "reference.json"
                candidate = arguments.work_dir / "candidate.json"
                reference.write_text(json.dumps(graph), encoding="utf-8")
                candidate.write_text(json.dumps(graph), encoding="utf-8")
                result = report(preserved=True)
                result.update(
                    {
                        "equal": True,
                        "artifacts": {
                            "reference": str(reference),
                            "candidate": str(candidate),
                        },
                    }
                )
                return result

            arguments = SimpleNamespace(
                expectations=expectations,
                upstream=repository / "upstream",
                graphoxide_bin=repository / "graphoxide",
                build=False,
                timeout=10,
                work_dir=work_root,
                output=Path(temporary) / "summary.json",
                max_examples=5,
            )
            with (
                mock.patch("parity.differential.corpus_suite.REPOSITORY", repository),
                mock.patch(
                    "parity.differential.corpus_suite.build_and_compare",
                    side_effect=fake_build,
                ),
            ):
                result = run_suite(arguments)
            self.assertTrue(result["passed"])
            self.assertEqual(
                result["corpora"][0]["candidate_cross_runtime_bindings"], 0
            )

    def test_run_suite_rejects_corpus_mutation_during_extraction(self):
        with tempfile.TemporaryDirectory() as temporary:
            repository = Path(temporary) / "repository"
            repository.mkdir()
            corpus = repository / "corpus"
            corpus.mkdir()
            fixture = corpus / "fixture.py"
            fixture.write_text("before\n", encoding="utf-8")
            graph = {"nodes": [], "links": []}
            expectation = reviewed({}, graph, graph, corpus=corpus)
            expectations = repository / "expectations.json"
            expectations.write_text(
                json.dumps(
                    {
                        "schema_version": 2,
                        "corpora": [
                            {"name": "fixture", "path": "corpus", "expect": expectation}
                        ],
                    }
                ),
                encoding="utf-8",
            )

            def mutating_build(arguments):
                fixture.write_text("after\n", encoding="utf-8")
                return {
                    "artifacts": {
                        "reference": str(arguments.work_dir / "reference.json"),
                        "candidate": str(arguments.work_dir / "candidate.json"),
                    }
                }

            arguments = SimpleNamespace(
                expectations=expectations,
                upstream=repository / "upstream",
                graphoxide_bin=repository / "graphoxide",
                build=False,
                timeout=10,
                work_dir=Path(temporary) / "work",
                output=None,
                max_examples=5,
            )
            with (
                mock.patch("parity.differential.corpus_suite.REPOSITORY", repository),
                mock.patch(
                    "parity.differential.corpus_suite.build_and_compare",
                    side_effect=mutating_build,
                ),
            ):
                with self.assertRaisesRegex(ValueError, "post-run corpus input digest"):
                    run_suite(arguments)

    def test_run_suite_final_sweep_rejects_late_cross_corpus_mutation(self):
        with tempfile.TemporaryDirectory() as temporary:
            repository = Path(temporary) / "repository"
            repository.mkdir()
            corpora = []
            graph = {"nodes": [], "links": []}
            for name in ("first", "second"):
                corpus = repository / name
                corpus.mkdir()
                (corpus / "fixture.py").write_text(
                    f"# {name}\n", encoding="utf-8"
                )
                corpora.append(
                    {
                        "name": name,
                        "path": name,
                        "expect": reviewed({}, graph, graph, corpus=corpus),
                    }
                )
            expectations = repository / "expectations.json"
            expectations.write_text(
                json.dumps({"schema_version": 2, "corpora": corpora}),
                encoding="utf-8",
            )

            def late_mutating_build(arguments):
                if arguments.corpus == (repository / "second").resolve():
                    (repository / "first" / "fixture.py").write_text(
                        "# changed later\n", encoding="utf-8"
                    )
                arguments.work_dir.mkdir(parents=True, exist_ok=True)
                reference = arguments.work_dir / "reference.json"
                candidate = arguments.work_dir / "candidate.json"
                reference.write_text(json.dumps(graph), encoding="utf-8")
                candidate.write_text(json.dumps(graph), encoding="utf-8")
                result = report(preserved=True)
                result.update(
                    {
                        "equal": True,
                        "artifacts": {
                            "reference": str(reference),
                            "candidate": str(candidate),
                        },
                    }
                )
                return result

            arguments = SimpleNamespace(
                expectations=expectations,
                upstream=repository / "upstream",
                graphoxide_bin=repository / "graphoxide",
                build=False,
                timeout=10,
                work_dir=Path(temporary) / "work",
                output=None,
                max_examples=5,
            )
            with (
                mock.patch("parity.differential.corpus_suite.REPOSITORY", repository),
                mock.patch(
                    "parity.differential.corpus_suite.build_and_compare",
                    side_effect=late_mutating_build,
                ),
            ):
                with self.assertRaisesRegex(ValueError, "final corpus input digest"):
                    run_suite(arguments)

    def test_forbidden_edge_and_missing_required_edge_fail(self):
        graph = {
            "nodes": [{"id": "child"}, {"id": "unsafe"}],
            "links": [
                {"source": "child", "target": "unsafe", "relation": "inherits"}
            ],
        }
        result = evaluate_expectation(
            report(preserved=False),
            graph,
            graph,
            reviewed(
                {
                    "required_candidate_edges": [
                        {"source": "child", "target": "safe", "relation": "inherits"}
                    ],
                    "forbidden_candidate_edges": [
                        {"source": "child", "target": "unsafe", "relation": "inherits"}
                    ],
                },
                graph,
                graph,
                corpus=self.corpus,
            ),
            corpus=self.corpus,
        )
        self.assertFalse(result["passed"])
        self.assertEqual(len(result["failures"]), 2)

    def test_reviewed_baseline_rejects_bogus_reverse_edge(self):
        reviewed_graph = {
            "nodes": [{"id": "a"}, {"id": "b"}],
            "links": [{"source": "a", "target": "b", "relation": "calls"}],
        }
        candidate = {
            **reviewed_graph,
            "links": [
                *reviewed_graph["links"],
                {"source": "b", "target": "a", "relation": "calls"},
            ],
        }
        result = evaluate_expectation(
            report(preserved=True),
            reviewed_graph,
            candidate,
            reviewed({}, reviewed_graph, reviewed_graph, corpus=self.corpus),
            corpus=self.corpus,
        )
        self.assertFalse(result["passed"])
        self.assertIn("candidate strict graph digest", result["failures"][0])

    def test_reviewed_baseline_rejects_wrong_candidate_types(self):
        reference = {"nodes": [{"id": "a"}, {"id": "b"}], "links": []}
        reviewed_candidate = {
            "nodes": [
                {"id": "a", "type": "function"},
                {"id": "b", "type": "class"},
            ],
            "links": [],
        }
        wrong_candidate = {
            "nodes": [
                {"id": "a", "type": "database"},
                {"id": "b", "type": "database"},
            ],
            "links": [],
        }
        result = evaluate_expectation(
            report(preserved=True),
            reference,
            wrong_candidate,
            reviewed({}, reference, reviewed_candidate, corpus=self.corpus),
            corpus=self.corpus,
        )
        self.assertFalse(result["passed"])
        self.assertIn("candidate strict graph digest", result["failures"][0])

    def test_corpus_input_digest_is_root_independent_and_path_sensitive(self):
        nested = self.corpus / "nested"
        nested.mkdir()
        (nested / "control.yaml").write_bytes(b"enabled: false\n")
        (nested / "caf\N{LATIN SMALL LETTER E WITH ACUTE}.txt").write_bytes(b"same\n")
        first = corpus_input_digest(self.corpus)
        with tempfile.TemporaryDirectory() as temporary:
            other = Path(temporary)
            (other / "fixture.txt").write_text("fixture\n", encoding="utf-8")
            (other / "nested").mkdir()
            (other / "nested/control.yaml").write_bytes(b"enabled: false\n")
            (other / "nested/cafe\N{COMBINING ACUTE ACCENT}.txt").write_bytes(b"same\n")
            self.assertEqual(first, corpus_input_digest(other))
            (other / "nested/control.yaml").rename(other / "nested/renamed.yaml")
            self.assertNotEqual(first, corpus_input_digest(other))

    def test_corpus_input_digest_includes_empty_directories(self):
        empty = self.corpus / "empty"
        empty.mkdir()
        first = corpus_input_digest(self.corpus)
        empty.rename(self.corpus / "renamed-empty")
        self.assertNotEqual(first, corpus_input_digest(self.corpus))

    def test_corpus_input_digest_rejects_file_directory_and_root_symlinks(self):
        target = self.corpus / "target.txt"
        target.write_text("target\n", encoding="utf-8")
        file_link = self.corpus / "file-link.txt"
        file_link.symlink_to(target.name)
        with self.assertRaisesRegex(ValueError, "symlinks are forbidden"):
            corpus_input_digest(self.corpus)
        file_link.unlink()

        directory = self.corpus / "directory"
        directory.mkdir()
        directory_link = self.corpus / "directory-link"
        directory_link.symlink_to(directory.name, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "symlinks are forbidden"):
            corpus_input_digest(self.corpus)
        directory_link.unlink()

        root_link = self.corpus.parent / f"{self.corpus.name}-link"
        root_link.symlink_to(self.corpus.name, target_is_directory=True)
        self.addCleanup(root_link.unlink, missing_ok=True)
        with self.assertRaisesRegex(ValueError, "symlinks are forbidden"):
            corpus_input_digest(root_link)

    def test_deleted_unextracted_fixture_fails_input_baseline_with_same_graphs(self):
        control = self.corpus / "compose.yaml"
        control.write_text("services: {}\n", encoding="utf-8")
        graph = {"nodes": [], "links": []}
        expectation = reviewed({}, graph, graph, corpus=self.corpus)
        control.unlink()
        result = evaluate_expectation(
            report(preserved=True),
            graph,
            graph,
            expectation,
            corpus=self.corpus,
        )
        self.assertFalse(result["passed"])
        self.assertIn("corpus input digest", result["failures"][0])
        self.assertFalse(result["corpus_input_baseline"]["matched"])

    def test_edited_unextracted_fixture_fails_input_baseline_with_same_graphs(self):
        control = self.corpus / "Dockerfile"
        control.write_text("FROM scratch\n", encoding="utf-8")
        graph = {"nodes": [], "links": []}
        expectation = reviewed({}, graph, graph, corpus=self.corpus)
        control.write_text("FROM alpine\n", encoding="utf-8")
        result = evaluate_expectation(
            report(preserved=True),
            graph,
            graph,
            expectation,
            corpus=self.corpus,
        )
        self.assertFalse(result["passed"])
        self.assertIn("corpus input digest", result["failures"][0])

    def test_missing_corpus_input_baseline_is_rejected(self):
        graph = {"nodes": [], "links": []}
        expectation = reviewed({}, graph, graph, corpus=self.corpus)
        del expectation["corpus_input_sha256"]
        result = evaluate_expectation(
            report(preserved=True),
            graph,
            graph,
            expectation,
            corpus=self.corpus,
        )
        self.assertFalse(result["passed"])
        self.assertIn("corpus_input_sha256", result["failures"][0])


if __name__ == "__main__":
    unittest.main()
