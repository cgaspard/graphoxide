from __future__ import annotations

from pathlib import Path
import re
import subprocess
import tempfile
import unittest

from parity.differential.graph_diff import (
    DifferentialError,
    _verify_clean_pinned_checkout,
    canonical_graph,
    compare_graphs,
)


class GraphDifferentialTests(unittest.TestCase):
    def test_reference_checkout_must_be_clean_but_may_create_ignored_environment(self):
        with tempfile.TemporaryDirectory() as temporary:
            checkout = Path(temporary)

            def git(*arguments: str) -> subprocess.CompletedProcess[str]:
                return subprocess.run(
                    ["git", *arguments],
                    cwd=checkout,
                    text=True,
                    capture_output=True,
                    check=True,
                )

            git("init", "-q")
            (checkout / ".gitignore").write_text(".venv/\n", encoding="utf-8")
            tracked = checkout / "uv.lock"
            tracked.write_text("pinned\n", encoding="utf-8")
            git("add", ".gitignore", "uv.lock")
            git(
                "-c",
                "user.name=Parity Test",
                "-c",
                "user.email=parity@example.invalid",
                "commit",
                "-qm",
                "fixture",
            )
            head = git("rev-parse", "HEAD").stdout.strip()

            _verify_clean_pinned_checkout(checkout, head)
            (checkout / ".venv").mkdir()
            (checkout / ".venv/marker").write_text("ignored\n", encoding="utf-8")
            _verify_clean_pinned_checkout(checkout, head)

            untracked = checkout / "new-source.py"
            untracked.write_text("print('unexpected')\n", encoding="utf-8")
            with self.assertRaisesRegex(DifferentialError, "contaminated reference"):
                _verify_clean_pinned_checkout(checkout, head)
            untracked.unlink()

            tracked.write_text("mutated\n", encoding="utf-8")
            with self.assertRaisesRegex(DifferentialError, "contaminated reference"):
                _verify_clean_pinned_checkout(checkout, head)
            git("add", "uv.lock")
            with self.assertRaisesRegex(DifferentialError, "contaminated reference"):
                _verify_clean_pinned_checkout(checkout, head)

    def test_order_and_cluster_presentation_do_not_create_structural_diff(self):
        reference = {
            "directed": True,
            "multigraph": False,
            "nodes": [
                {"id": "b", "label": "B", "file_type": "code", "community": 1},
                {"id": "a", "label": "A", "file_type": "code", "community": 1},
            ],
            "links": [
                {"source": "a", "target": "b", "relation": "calls", "confidence": "EXTRACTED"}
            ],
        }
        candidate = {
            "multigraph": False,
            "directed": True,
            "nodes": [
                {"id": "a", "label": "A", "file_type": "code", "community": 99},
                {"id": "b", "label": "B", "file_type": "code", "community": 42},
            ],
            "edges": [
                {"source": "a", "target": "b", "relation": "calls", "confidence": "EXTRACTED"}
            ],
        }
        self.assertTrue(compare_graphs(reference, candidate)["equal"])

    def test_true_direction_and_edge_multiplicity_are_preserved(self):
        reference = {
            "nodes": [{"id": "a"}, {"id": "b"}],
            "links": [
                {"source": "a", "target": "b", "relation": "calls"},
                {"source": "a", "target": "b", "relation": "calls"},
            ],
        }
        candidate = {
            "nodes": [{"id": "a"}, {"id": "b"}],
            "links": [{"source": "b", "target": "a", "relation": "calls"}],
        }
        report = compare_graphs(reference, candidate)
        self.assertFalse(report["equal"])
        self.assertEqual(report["edges"]["missing_count"], 2)
        self.assertEqual(report["edges"]["extra_count"], 1)

    def test_paths_are_reanchored_to_the_shared_corpus(self):
        with tempfile.TemporaryDirectory() as temporary:
            corpus = Path(temporary)
            source = corpus / "src/main.py"
            reference = {"nodes": [{"id": "main", "source_file": str(source)}]}
            candidate = {"nodes": [{"id": "main", "source_file": "src/main.py"}]}
            self.assertEqual(
                canonical_graph(reference, corpus=corpus),
                canonical_graph(candidate, corpus=corpus),
            )

    def test_node_field_mismatches_are_reported_by_id(self):
        reference = {"nodes": [{"id": "main", "label": "Main", "type": "function"}]}
        candidate = {"nodes": [{"id": "main", "label": "Main", "type": "class"}]}
        report = compare_graphs(reference, candidate)
        self.assertFalse(report["equal"])
        self.assertEqual(report["nodes"]["mismatched_count"], 1)
        self.assertIn("type", report["nodes"]["mismatched"][0]["fields"])

    def test_duplicate_node_ids_are_invalid_and_never_silently_collapsed(self):
        graph = {
            "nodes": [
                {"id": "duplicate", "label": "First"},
                {"id": "duplicate", "label": "Second"},
            ]
        }
        report = compare_graphs(graph, graph)
        duplicates = report["diagnostics"]["pre_normalization"]["reference"][
            "duplicate_node_ids"
        ]
        self.assertFalse(report["equal"])
        self.assertEqual(report["summary"]["reference"]["nodes"], 2)
        self.assertEqual(duplicates["id_count"], 1)
        self.assertEqual(duplicates["duplicate_occurrence_count"], 1)
        self.assertEqual(duplicates["ids"], [{"id": "duplicate", "count": 2}])
        self.assertFalse(report["parity"]["pre_normalization_valid"])

    def test_conflicting_serialized_edge_aliases_fail_before_normalization(self):
        reference = {
            "nodes": [{"id": "a"}, {"id": "b"}],
            "links": [{"source": "a", "target": "b", "relation": "calls"}],
        }
        candidate = {
            "nodes": [{"id": "a"}, {"id": "b"}],
            "links": [
                {
                    "_src": "a",
                    "source": "wrong-source",
                    "_tgt": "b",
                    "target": "wrong-target",
                    "relation": "calls",
                }
            ],
        }
        report = compare_graphs(
            reference, candidate, contract="reference-preserving"
        )
        conflicts = report["diagnostics"]["pre_normalization"]["candidate"][
            "alias_conflicts"
        ]
        self.assertFalse(report["equal"])
        self.assertFalse(report["gate"]["passed"])
        self.assertEqual(conflicts["count"], 2)

    def test_conflicting_node_and_edge_collection_aliases_fail(self):
        graph = {
            "nodes": [{"id": "a", "label": "A", "name": "Wrong"}],
            "links": [],
            "edges": [{"source": "a", "target": "a", "relation": "calls"}],
        }
        report = compare_graphs(graph, graph)
        conflicts = report["diagnostics"]["pre_normalization"]["candidate"][
            "alias_conflicts"
        ]
        self.assertFalse(report["gate"]["passed"])
        self.assertEqual(conflicts["count"], 2)

    def test_dangling_candidate_edge_fails_reference_preserving_contract(self):
        reference = {"nodes": [{"id": "a"}], "links": []}
        candidate = {
            "nodes": [{"id": "a"}],
            "links": [
                {"source": "a", "target": "missing", "relation": "calls"}
            ],
        }
        report = compare_graphs(
            reference, candidate, contract="reference-preserving"
        )
        dangling = report["diagnostics"]["pre_normalization"]["candidate"][
            "dangling_references"
        ]
        self.assertEqual(dangling["count"], 1)
        self.assertFalse(report["gate"]["passed"])

    def test_malformed_candidate_fact_fails_reference_preserving_contract(self):
        reference = {"nodes": [{"id": "a"}], "links": []}
        candidate = {"nodes": [{"id": "a"}], "links": ["not-an-edge"]}
        report = compare_graphs(
            reference, candidate, contract="reference-preserving"
        )
        malformed = report["diagnostics"]["pre_normalization"]["candidate"][
            "malformed_records"
        ]
        self.assertEqual(malformed["count"], 1)
        self.assertFalse(report["gate"]["passed"])

    def test_asymmetric_duplicate_records_are_compared_as_a_multiset(self):
        reference = {
            "nodes": [
                {"id": "duplicate", "label": "First"},
                {"id": "duplicate", "label": "Second"},
            ]
        }
        candidate = {"nodes": [{"id": "duplicate", "label": "First"}]}
        report = compare_graphs(reference, candidate)
        fields = report["nodes"]["mismatched"][0]["fields"]
        self.assertEqual(report["nodes"]["mismatched_count"], 1)
        self.assertEqual(
            fields["__occurrences__"], {"reference": 2, "candidate": 1}
        )
        self.assertEqual(fields["__records__"]["reference_only_count"], 1)

    def test_absolute_sources_and_path_derived_ids_are_diagnosed_before_reanchoring(self):
        with tempfile.TemporaryDirectory() as temporary:
            corpus = Path(temporary)
            source = corpus / "src/main.rs"
            absolute_stem = str(source.with_suffix(""))
            path_id = re.sub(r"[^0-9A-Za-z]+", "_", absolute_stem).strip("_").lower()
            reference = {
                "nodes": [
                    {
                        "id": path_id,
                        "label": "main.rs",
                        "source_file": str(source),
                    }
                ]
            }
            candidate = {
                "nodes": [
                    {
                        "id": path_id,
                        "label": "main.rs",
                        "source_file": "src/main.rs",
                    }
                ]
            }
            report = compare_graphs(reference, candidate, corpus=corpus)
            diagnostics = report["diagnostics"]["pre_normalization"]["reference"]
            self.assertFalse(report["equal"])
            self.assertEqual(diagnostics["absolute_source_paths"]["count"], 1)
            self.assertEqual(diagnostics["absolute_ids"]["count"], 1)
            self.assertEqual(
                diagnostics["absolute_ids"]["examples"][0]["reason"],
                "derived_from_absolute_source_file",
            )
            # The normalized node still compares equal; portability is reported
            # independently instead of being hidden by path reanchoring.
            self.assertEqual(report["nodes"]["mismatched_count"], 0)
            self.assertFalse(report["parity"]["pre_normalization_valid"])

    def test_absolute_windows_path_used_as_an_id_is_diagnosed(self):
        graph = {"nodes": [{"id": r"C:\repo\src\main.rs", "label": "main.rs"}]}
        report = compare_graphs(graph, graph)
        diagnostics = report["diagnostics"]["pre_normalization"]["candidate"]
        self.assertFalse(report["equal"])
        self.assertEqual(diagnostics["absolute_ids"]["count"], 1)
        self.assertEqual(
            diagnostics["absolute_ids"]["examples"][0]["reason"],
            "absolute_path_value",
        )

    def test_cross_family_identity_hub_is_diagnosed_without_changing_parity(self):
        graph = {
            "nodes": [
                {
                    "id": "base",
                    "label": "Base",
                    "source_file": "frontend/base.ts",
                },
                {
                    "id": "child",
                    "label": "Child",
                    "source_file": "backend/child.py",
                },
            ],
            "links": [
                {
                    "source": "child",
                    "target": "base",
                    "relation": "inherits",
                    "source_file": "backend/child.py",
                }
            ],
        }
        report = compare_graphs(graph, graph)
        hubs = report["diagnostics"]["identity_hubs"]["candidate"]
        self.assertTrue(report["equal"])
        self.assertEqual(hubs["id_count"], 1)
        self.assertEqual(hubs["ids"][0]["id"], "base")
        self.assertEqual(hubs["ids"][0]["families"], ["jsts", "python"])
        self.assertEqual(hubs["ids"][0]["relations"], ["inherits"])

        gated = compare_graphs(
            graph, graph, fail_on_candidate_identity_hubs=True
        )
        self.assertFalse(gated["equal"])
        self.assertTrue(
            gated["diagnostics"]["identity_hubs"]["candidate_gate_enabled"]
        )
        self.assertFalse(
            gated["diagnostics"]["identity_hubs"]["candidate_gate_passed"]
        )

    def test_real_interop_and_semantic_edges_are_not_identity_hubs(self):
        graph = {
            "nodes": [
                {"id": "shared", "source_file": "src/shared.js"},
                {"id": "caller", "source_file": "src/caller.ts"},
                {"id": "python", "source_file": "tools/check.py"},
            ],
            "links": [
                {
                    "source": "caller",
                    "target": "shared",
                    "relation": "calls",
                    "source_file": "src/caller.ts",
                },
                {
                    "source": "python",
                    "target": "shared",
                    "relation": "semantically_similar_to",
                    "source_file": "tools/check.py",
                },
            ],
        }
        hubs = compare_graphs(graph, graph)["diagnostics"]["identity_hubs"]
        self.assertEqual(hubs["reference"], {"id_count": 0, "ids": []})
        self.assertEqual(hubs["candidate"], {"id_count": 0, "ids": []})

    def test_mismatches_are_grouped_by_file_relation_and_field(self):
        reference = {
            "nodes": [
                {"id": "shared", "type": "function", "source_file": "src/a.rs"},
                {"id": "reference-only", "source_file": "src/ref.rs"},
            ],
            "links": [
                {
                    "source": "shared",
                    "target": "reference-only",
                    "relation": "calls",
                    "source_file": "src/ref.rs",
                }
            ],
        }
        candidate = {
            "nodes": [
                {"id": "shared", "type": "class", "source_file": "src/a.rs"},
                {"id": "candidate-only", "source_file": "src/ext.rs"},
            ],
            "links": [
                {
                    "source": "shared",
                    "target": "candidate-only",
                    "relation": "imports",
                    "source_file": "src/ext.rs",
                }
            ],
        }
        report = compare_graphs(reference, candidate)
        self.assertEqual(
            report["nodes"]["groups"]["missing_by_source_file"], {"src/ref.rs": 1}
        )
        self.assertEqual(
            report["nodes"]["groups"]["extra_by_source_file"], {"src/ext.rs": 1}
        )
        self.assertEqual(
            report["nodes"]["groups"]["mismatched_by_field"], {"type": 1}
        )
        self.assertEqual(
            report["edges"]["groups"]["missing_by_relation"], {"calls": 1}
        )
        self.assertEqual(
            report["edges"]["groups"]["extra_by_relation"], {"imports": 1}
        )
        self.assertEqual(
            report["edges"]["groups"]["missing_by_source_file"], {"src/ref.rs": 1}
        )

    def test_candidate_extensions_are_separate_from_shared_parity(self):
        reference = {
            "nodes": [{"id": "shared"}, {"id": "reference-only"}],
            "links": [
                {
                    "source": "reference-only",
                    "target": "shared",
                    "relation": "contains",
                }
            ],
            "hyperedges": [{"id": "reference-group", "nodes": ["shared", "reference-only"]}],
        }
        candidate = {
            "nodes": [{"id": "shared"}, {"id": "candidate-only"}],
            "links": [
                {
                    "source": "candidate-only",
                    "target": "shared",
                    "relation": "contains",
                }
            ],
            "hyperedges": [{"id": "candidate-group", "nodes": ["shared", "candidate-only"]}],
        }
        report = compare_graphs(reference, candidate)
        parity = report["parity"]
        self.assertFalse(report["equal"])
        self.assertTrue(parity["shared"]["equal"])
        self.assertEqual(parity["shared"]["node_id_count"], 1)
        self.assertEqual(parity["shared"]["missing_edge_count"], 0)
        self.assertEqual(parity["shared"]["extra_edge_count"], 0)
        self.assertEqual(parity["extensions"]["candidate_only_node_id_count"], 1)
        self.assertEqual(parity["extensions"]["candidate_extension_edge_count"], 1)
        self.assertEqual(parity["extensions"]["candidate_extension_hyperedge_count"], 1)
        self.assertEqual(parity["extensions"]["reference_only_node_id_count"], 1)
        self.assertEqual(parity["extensions"]["reference_only_edge_count"], 1)

    def test_extra_edge_between_shared_nodes_breaks_shared_parity(self):
        reference = {"nodes": [{"id": "a"}, {"id": "b"}]}
        candidate = {
            "nodes": [{"id": "a"}, {"id": "b"}],
            "links": [{"source": "a", "target": "b", "relation": "calls"}],
        }
        report = compare_graphs(reference, candidate)
        self.assertFalse(report["parity"]["shared"]["equal"])
        self.assertEqual(report["parity"]["shared"]["extra_edge_count"], 1)
        self.assertEqual(report["parity"]["extensions"]["candidate_extension_edge_count"], 0)

    def test_reference_preserving_contract_allows_audited_additions(self):
        reference = {
            "directed": True,
            "nodes": [{"id": "a", "label": "A"}, {"id": "b", "label": "B"}],
            "links": [{"source": "a", "target": "b", "relation": "calls"}],
        }
        candidate = {
            "directed": True,
            "nodes": [
                {"id": "a", "label": "A", "type": "function"},
                {"id": "b", "label": "B", "type": "function"},
                {"id": "c", "label": "C", "type": "constant"},
            ],
            "links": [
                {"source": "a", "target": "b", "relation": "calls"},
                {"source": "a", "target": "c", "relation": "references"},
            ],
        }
        report = compare_graphs(reference, candidate, contract="reference-preserving")
        self.assertFalse(report["equal"])
        self.assertTrue(report["parity"]["reference_preservation"]["preserved"])
        self.assertTrue(report["gate"]["passed"])

    def test_reference_preserving_contract_rejects_changed_reference_fact(self):
        reference = {"nodes": [{"id": "a", "type": "function"}]}
        candidate = {"nodes": [{"id": "a", "type": "class"}]}
        report = compare_graphs(reference, candidate, contract="reference-preserving")
        preservation = report["parity"]["reference_preservation"]
        self.assertFalse(preservation["preserved"])
        self.assertEqual(preservation["nodes"]["missing_or_changed_count"], 1)
        self.assertFalse(report["gate"]["passed"])

    def test_reference_preserving_contract_keeps_edge_multiplicity(self):
        reference = {
            "nodes": [{"id": "a"}, {"id": "b"}],
            "links": [
                {"source": "a", "target": "b", "relation": "calls"},
                {"source": "a", "target": "b", "relation": "calls"},
            ],
        }
        candidate = {
            "nodes": [{"id": "a"}, {"id": "b"}],
            "links": [{"source": "a", "target": "b", "relation": "calls"}],
        }
        report = compare_graphs(reference, candidate, contract="reference-preserving")
        preservation = report["parity"]["reference_preservation"]
        self.assertEqual(preservation["edges"]["matched_count"], 1)
        self.assertEqual(preservation["edges"]["missing_or_changed_count"], 1)
        self.assertFalse(report["gate"]["passed"])

    def test_reference_preserving_contract_accepts_named_context_refinement(self):
        reference = {
            "nodes": [{"id": "caller"}, {"id": "target"}],
            "links": [
                {
                    "source": "caller",
                    "target": "target",
                    "relation": "calls",
                    "context": "call",
                }
            ],
        }
        candidate = {
            "nodes": [{"id": "caller"}, {"id": "target"}],
            "links": [
                {
                    "source": "caller",
                    "target": "target",
                    "relation": "calls",
                    "context": "import_guided_call",
                }
            ],
        }
        report = compare_graphs(reference, candidate, contract="reference-preserving")
        self.assertFalse(report["equal"])
        self.assertTrue(report["parity"]["reference_preservation"]["preserved"])
        self.assertTrue(report["gate"]["passed"])


if __name__ == "__main__":
    unittest.main()
