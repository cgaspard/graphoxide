from __future__ import annotations

from pathlib import Path
import unittest

from parity.differential.corpus_suite import evaluate_expectation, strict_graph_digest


def report(*, preserved: bool, reference_hubs: int = 0, candidate_hubs: int = 0):
    return {
        "parity": {"reference_preservation": {"preserved": preserved}},
        "diagnostics": {
            "pre_normalization": {
                "reference": {"violation_count": 0},
                "candidate": {"violation_count": 0},
            },
            "identity_hubs": {
                "reference": {"id_count": reference_hubs},
                "candidate": {"id_count": candidate_hubs},
            }
        },
    }


def reviewed(
    expectation: dict,
    reference_graph: dict,
    candidate_graph: dict,
    *,
    corpus: Path = Path("."),
) -> dict:
    return {
        **expectation,
        "reference_strict_sha256": strict_graph_digest(
            reference_graph, corpus=corpus
        ),
        "candidate_strict_sha256": strict_graph_digest(
            candidate_graph, corpus=corpus
        ),
    }


class CorpusExpectationTests(unittest.TestCase):
    def test_reference_preservation_expectation(self):
        graph = {"nodes": [], "links": []}
        result = evaluate_expectation(
            report(preserved=True),
            graph,
            graph,
            reviewed(
                {"reference_preserved": True, "candidate_identity_hubs": 0},
                graph,
                graph,
            ),
            corpus=Path("."),
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
            report(preserved=False, reference_hubs=3),
            graph,
            graph,
            reviewed(
                {
                    "candidate_identity_hubs": 0,
                    "reference_identity_hubs_min": 3,
                    "required_candidate_edges": [
                        {"source": "child", "target": "safe", "relation": "inherits"}
                    ],
                    "forbidden_candidate_edges": [
                        {"source": "child", "target": "unsafe", "relation": "inherits"}
                    ],
                },
                graph,
                graph,
            ),
            corpus=Path("."),
        )
        self.assertTrue(result["passed"])

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
            ),
            corpus=Path("."),
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
            reviewed({}, reviewed_graph, reviewed_graph),
            corpus=Path("."),
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
            reviewed({}, reference, reviewed_candidate),
            corpus=Path("."),
        )
        self.assertFalse(result["passed"])
        self.assertIn("candidate strict graph digest", result["failures"][0])


if __name__ == "__main__":
    unittest.main()
