from __future__ import annotations

from pathlib import Path
import re
import subprocess
import tempfile
import unittest
import unicodedata

from parity.differential.graph_diff import (
    DifferentialError,
    _language_family,
    _load_object,
    _verify_clean_pinned_checkout,
    canonical_graph,
    compare_graphs,
)


class GraphDifferentialTests(unittest.TestCase):
    def test_reference_checkout_rejects_ignored_source_but_allows_caches(self):
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
            (checkout / ".gitignore").write_text(
                ".venv/\n.pytest_cache/\n__pycache__/\n*.so\n",
                encoding="utf-8",
            )
            tracked = checkout / "uv.lock"
            tracked.write_text("pinned\n", encoding="utf-8")
            package = checkout / "graphify"
            package.mkdir()
            source = package / "__init__.py"
            source.write_text("VERSION = 'pinned'\n", encoding="utf-8")
            git("add", ".gitignore", "uv.lock", "graphify/__init__.py")
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
            (checkout / ".pytest_cache").mkdir()
            (checkout / ".pytest_cache/marker").write_text(
                "ignored cache\n", encoding="utf-8"
            )
            bytecode = package / "__pycache__"
            bytecode.mkdir()
            (bytecode / "__init__.cpython-test.pyc").write_bytes(b"cache")
            _verify_clean_pinned_checkout(checkout, head)

            ignored_native = package / "oracle.so"
            ignored_native.write_bytes(b"ignored shadow")
            with self.assertRaisesRegex(
                DifferentialError,
                r"ignored executable-source artifact.*graphify/oracle\.so",
            ):
                _verify_clean_pinned_checkout(checkout, head)
            ignored_native.unlink()

            orphan_cache = bytecode / "orphan.cpython-test.pyc"
            orphan_cache.write_bytes(b"not backed by tracked source")
            with self.assertRaisesRegex(
                DifferentialError,
                r"ignored executable-source artifact.*graphify/__pycache__/orphan",
            ):
                _verify_clean_pinned_checkout(checkout, head)
            orphan_cache.unlink()

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
            "hyperedges": [],
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
            "hyperedges": [],
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

    def test_multigraph_edge_keys_are_structural_and_strict_facts(self):
        reference = {
            "directed": True,
            "multigraph": True,
            "nodes": [{"id": "a"}],
            "links": [
                {
                    "source": "a",
                    "target": "a",
                    "relation": "calls",
                    "key": "calls:a.py:L1",
                }
            ],
            "hyperedges": [],
        }
        candidate = {
            **reference,
            "links": [
                {
                    **reference["links"][0],
                    "key": "calls:a.py:L2",
                }
            ],
        }
        for profile in ("structure", "strict"):
            with self.subTest(profile=profile):
                report = compare_graphs(reference, candidate, profile=profile)
                self.assertFalse(report["equal"])
                self.assertEqual(report["edges"]["missing_count"], 1)
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

    def test_portable_paths_normalize_unicode_roots_and_relative_paths_to_nfc(self):
        with tempfile.TemporaryDirectory() as temporary:
            root_name = unicodedata.normalize("NFD", "café-root")
            corpus = Path(temporary) / root_name
            corpus.mkdir()
            relative_nfd = unicodedata.normalize("NFD", "src/café.py")
            relative_nfc = unicodedata.normalize("NFC", relative_nfd)
            absolute_nfc = unicodedata.normalize(
                "NFC", str(corpus / relative_nfd)
            )
            reference = {
                "nodes": [{"id": "main", "source_file": absolute_nfc}]
            }
            candidate = {
                "nodes": [{"id": "main", "source_file": relative_nfd}]
            }
            reference_graph = canonical_graph(reference, corpus=corpus)
            candidate_graph = canonical_graph(candidate, corpus=corpus)
            self.assertEqual(reference_graph, candidate_graph)
            self.assertEqual(
                reference_graph["nodes"][0]["source_file"], relative_nfc
            )

    def test_graph_artifact_loader_rejects_nonfinite_json_numbers(self):
        with tempfile.TemporaryDirectory() as temporary:
            graph_path = Path(temporary) / "graph.json"
            for literal in ("NaN", "Infinity", "-Infinity", "1e9999"):
                with self.subTest(literal=literal):
                    graph_path.write_text(
                        '{"nodes":[{"id":"a","score":'
                        + literal
                        + '}],"links":[],"hyperedges":[]}',
                        encoding="utf-8",
                    )
                    with self.assertRaisesRegex(
                        DifferentialError, "non-finite JSON number"
                    ):
                        _load_object(graph_path)

    def test_graph_artifact_loader_rejects_duplicate_json_keys(self):
        documents = [
            '{"nodes": [], "nodes": []}',
            '{"nodes": [{"id": "a", "id": "b"}]}',
        ]
        with tempfile.TemporaryDirectory() as temporary:
            graph_path = Path(temporary) / "graph.json"
            for document in documents:
                with self.subTest(document=document):
                    graph_path.write_text(document, encoding="utf-8")
                    with self.assertRaisesRegex(
                        DifferentialError, "duplicate JSON object key"
                    ):
                        _load_object(graph_path)

    def test_node_field_mismatches_are_reported_by_id(self):
        reference = {"nodes": [{"id": "main", "label": "Main", "type": "function"}]}
        candidate = {"nodes": [{"id": "main", "label": "Main", "type": "class"}]}
        report = compare_graphs(reference, candidate)
        self.assertFalse(report["equal"])
        self.assertEqual(report["nodes"]["mismatched_count"], 1)
        self.assertIn("type", report["nodes"]["mismatched"][0]["fields"])

    def test_missing_and_null_node_fields_are_not_equal(self):
        reference = {
            "nodes": [{"id": "main", "type": None}],
            "links": [],
            "hyperedges": [],
        }
        candidate = {"nodes": [{"id": "main"}], "links": [], "hyperedges": []}
        report = compare_graphs(reference, candidate)
        self.assertFalse(report["equal"])
        change = report["nodes"]["mismatched"][0]["fields"]["type"]
        self.assertIsNone(change["reference"])
        self.assertTrue(change["reference_present"])
        self.assertFalse(change["candidate_present"])

    def test_json_booleans_are_not_equal_to_numbers(self):
        base = {"links": [], "hyperedges": []}
        node_report = compare_graphs(
            {**base, "nodes": [{"id": "main", "line": False}]},
            {**base, "nodes": [{"id": "main", "line": 0}]},
        )
        self.assertFalse(node_report["equal"])
        self.assertIn("line", node_report["nodes"]["mismatched"][0]["fields"])

        metadata_report = compare_graphs(
            {**base, "nodes": [], "directed": False},
            {**base, "nodes": [], "directed": 0},
        )
        self.assertFalse(metadata_report["equal"])
        self.assertIn("directed", metadata_report["metadata"])

    def test_integer_node_id_does_not_satisfy_string_edge_endpoint(self):
        graph = {
            "nodes": [{"id": 1}],
            "links": [{"source": "1", "target": "1", "relation": "calls"}],
            "hyperedges": [],
        }
        report = compare_graphs(graph, graph)
        diagnostics = report["diagnostics"]["pre_normalization"]["candidate"]
        self.assertFalse(report["gate"]["passed"])
        self.assertEqual(diagnostics["malformed_records"]["count"], 1)
        self.assertEqual(diagnostics["dangling_references"]["count"], 2)
        self.assertEqual(
            diagnostics["malformed_records"]["examples"][0]["reason"],
            "missing_or_invalid_id",
        )

    def test_every_graph_identity_position_requires_a_nonempty_string(self):
        def invalid_graph(position, value):
            graph = {
                "nodes": [{"id": "a"}, {"id": "b"}],
                "links": [],
                "hyperedges": [],
            }
            if position == "node":
                graph["nodes"][0]["id"] = value
            elif position == "edge":
                graph["links"] = [
                    {"source": value, "target": "b", "relation": "calls"}
                ]
            elif position == "hyperedge_id":
                graph["hyperedges"] = [{"id": value, "nodes": ["a"]}]
            else:
                graph["hyperedges"] = [{"id": "flow", "nodes": [value]}]
            return graph

        for value_name, value in (
            ("empty", ""),
            ("integer", 1),
            ("boolean", True),
            ("nonfinite", float("nan")),
        ):
            for position in ("node", "edge", "hyperedge_id", "hyperedge_member"):
                with self.subTest(value=value_name, position=position):
                    graph = invalid_graph(position, value)
                    report = compare_graphs(graph, graph)
                    diagnostics = report["diagnostics"]["pre_normalization"][
                        "candidate"
                    ]
                    self.assertGreater(diagnostics["violation_count"], 0)
                    self.assertFalse(report["gate"]["passed"])

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
        reference = {"nodes": [{"id": "a"}], "links": [], "hyperedges": []}
        candidate = {
            "nodes": [{"id": "a"}],
            "links": ["not-an-edge"],
            "hyperedges": [],
        }
        report = compare_graphs(
            reference, candidate, contract="reference-preserving"
        )
        malformed = report["diagnostics"]["pre_normalization"]["candidate"][
            "malformed_records"
        ]
        self.assertEqual(malformed["count"], 1)
        self.assertFalse(report["gate"]["passed"])

    def test_missing_required_graph_collections_are_malformed(self):
        reference = {"nodes": [], "links": [], "hyperedges": []}
        report = compare_graphs(reference, {})
        malformed = report["diagnostics"]["pre_normalization"]["candidate"][
            "malformed_records"
        ]
        self.assertFalse(report["gate"]["passed"])
        self.assertEqual(malformed["count"], 3)
        self.assertEqual(
            {item["collection"] for item in malformed["examples"]},
            {"nodes", "links/edges", "hyperedges"},
        )

    def test_hyperedge_ids_are_required_and_unique(self):
        missing_id = {
            "nodes": [{"id": "a"}],
            "links": [],
            "hyperedges": [{"nodes": ["a"]}],
        }
        missing_report = compare_graphs(missing_id, missing_id)
        malformed = missing_report["diagnostics"]["pre_normalization"]["candidate"][
            "malformed_records"
        ]
        self.assertFalse(missing_report["gate"]["passed"])
        self.assertEqual(malformed["count"], 1)
        self.assertEqual(malformed["examples"][0]["reason"], "missing_or_invalid_id")

        duplicate_id = {
            "nodes": [{"id": "a"}],
            "links": [],
            "hyperedges": [
                {"id": "flow", "nodes": ["a"]},
                {"id": "flow", "nodes": ["a"]},
            ],
        }
        duplicate_report = compare_graphs(duplicate_id, duplicate_id)
        duplicates = duplicate_report["diagnostics"]["pre_normalization"][
            "candidate"
        ]["duplicate_hyperedge_ids"]
        self.assertFalse(duplicate_report["gate"]["passed"])
        self.assertEqual(duplicates["id_count"], 1)
        self.assertEqual(duplicates["duplicate_occurrence_count"], 1)
        self.assertEqual(duplicates["ids"], [{"id": "flow", "count": 2}])

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

    def test_cross_runtime_binding_is_reported_without_changing_exact_equality(self):
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
                    "relation": "references",
                    "source_file": "backend/child.py",
                }
            ],
            "hyperedges": [],
        }
        report = compare_graphs(graph, graph)
        bindings = report["diagnostics"]["cross_runtime_bindings"]["candidate"]
        self.assertTrue(report["equal"])
        self.assertTrue(report["gate"]["passed"])
        self.assertEqual(bindings["endpoint_count"], 1)
        self.assertEqual(bindings["endpoints"][0]["id"], "base")
        self.assertEqual(
            bindings["endpoints"][0]["families"], ["jsts", "python"]
        )
        self.assertEqual(bindings["endpoints"][0]["relations"], ["references"])

        gated = compare_graphs(
            graph, graph, fail_on_candidate_cross_runtime_bindings=True
        )
        self.assertTrue(gated["equal"])
        self.assertFalse(gated["gate"]["passed"])
        self.assertTrue(
            gated["diagnostics"]["cross_runtime_bindings"][
                "candidate_gate_enabled"
            ]
        )
        self.assertFalse(
            gated["diagnostics"]["cross_runtime_bindings"][
                "candidate_gate_passed"
            ]
        )

    def test_missing_edge_source_uses_directed_source_node_provenance(self):
        graph = {
            "nodes": [
                {"id": "base", "source_file": "frontend/base.ts"},
                {"id": "child", "source_file": "backend/child.py"},
            ],
            "links": [
                {
                    "source": "child",
                    "target": "base",
                    "relation": "inherits",
                }
            ],
            "hyperedges": [],
        }
        report = compare_graphs(graph, graph)
        candidate = report["diagnostics"]["pre_normalization"]["candidate"]
        bindings = report["diagnostics"]["cross_runtime_bindings"]["candidate"]
        self.assertEqual(candidate["violation_count"], 0)
        self.assertTrue(report["equal"])
        self.assertEqual(bindings["endpoint_count"], 1)
        self.assertEqual(bindings["endpoints"][0]["id"], "base")
        self.assertEqual(
            bindings["endpoints"][0]["families"], ["jsts", "python"]
        )
        self.assertEqual(
            bindings["endpoints"][0]["source_files"],
            ["backend/child.py", "frontend/base.ts"],
        )

        gated = compare_graphs(
            graph, graph, fail_on_candidate_cross_runtime_bindings=True
        )
        self.assertTrue(gated["equal"])
        self.assertFalse(gated["gate"]["passed"])
        self.assertFalse(
            gated["diagnostics"]["cross_runtime_bindings"][
                "candidate_gate_passed"
            ]
        )

    def test_missing_edge_source_keeps_legitimate_runtime_interop_grouped(self):
        graph = {
            "nodes": [
                {"id": "base", "source_file": "jvm/Base.java"},
                {"id": "child", "source_file": "jvm/Child.kt"},
            ],
            "links": [
                {
                    "source": "child",
                    "target": "base",
                    "relation": "inherits",
                }
            ],
            "hyperedges": [],
        }
        report = compare_graphs(
            graph, graph, fail_on_candidate_cross_runtime_bindings=True
        )
        self.assertTrue(report["equal"])
        self.assertTrue(report["gate"]["passed"])
        self.assertEqual(
            report["diagnostics"]["cross_runtime_bindings"]["candidate"],
            {"endpoint_count": 0, "endpoints": []},
        )

    def test_supported_runtime_extensions_have_explicit_families(self):
        expected = {
            "jsts": [
                "js", "jsx", "mjs", "cjs", "ejs", "ets", "ts", "tsx",
                "mts", "cts", "vue", "svelte", "astro",
            ],
            "jvm": ["java", "kt", "kts", "scala", "groovy", "gradle"],
            "native": [
                "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx", "cu",
                "cuh", "metal", "m", "mm", "swift",
            ],
            "python": ["py", "pyi"],
            "go": ["go"],
            "rust": ["rs"],
            "ruby": ["rb", "rake"],
            "php": ["php", "phtml", "php3", "php4", "php5", "php7", "phps"],
            "dotnet": [
                "cs", "sln", "slnx", "csproj", "fsproj", "vbproj", "razor",
                "cshtml", "xaml",
            ],
            "lua": ["lua", "luau", "toc"],
            "zig": ["zig"],
            "elixir": ["ex", "exs"],
            "julia": ["jl"],
            "dart": ["dart"],
            "shell": ["sh", "bash", "zsh", "dash", "ksh"],
            "powershell": ["ps1", "psm1", "psd1"],
            "fortran": ["f", "f90", "f95", "f03", "f08"],
            "pascal": [
                "pas", "pp", "dpr", "dpk", "lpr", "inc", "dfm", "lfm", "lpk",
            ],
            "terraform": ["tf", "tfvars", "hcl"],
            "sql": ["sql"],
            "r": ["r"],
            "hardware": ["v", "sv", "svh"],
            "apex": ["cls", "trigger"],
            "dm": ["dm", "dme", "dmi", "dmm", "dmf"],
        }
        for family, extensions in expected.items():
            for extension in extensions:
                with self.subTest(family=family, extension=extension):
                    self.assertEqual(_language_family(f"src/example.{extension}"), family)
                    self.assertEqual(_language_family(f"src/example.{extension.upper()}"), family)
        for generic in ["package.json", "compose.yaml", "settings.toml", "view.xml"]:
            with self.subTest(generic=generic):
                self.assertIsNone(_language_family(generic))

    def test_symbol_binding_relations_report_cross_runtime_endpoints(self):
        for relation in [
            "accesses",
            "bound_to",
            "case_of",
            "defines",
            "instantiates",
            "listened_by",
            "mixes_in",
            "requires",
        ]:
            graph = {
                "nodes": [
                    {"id": "target", "source_file": "frontend/target.ts"},
                    {"id": "origin", "source_file": "backend/origin.py"},
                ],
                "links": [
                    {
                        "source": "origin",
                        "target": "target",
                        "relation": relation,
                        "source_file": "backend/origin.py",
                    }
                ],
            }
            with self.subTest(relation=relation):
                bindings = compare_graphs(graph, graph)["diagnostics"][
                    "cross_runtime_bindings"
                ]
                self.assertEqual(bindings["candidate"]["endpoint_count"], 1)
                self.assertEqual(
                    bindings["candidate"]["endpoints"][0]["id"], "target"
                )

    def test_config_and_data_flow_relations_do_not_create_cross_runtime_bindings(self):
        for relation in [
            "depends_on",
            "reads_from",
            "uses",
            "implemented_by",
            "semantically_similar_to",
        ]:
            graph = {
                "nodes": [
                    {"id": "target", "source_file": "infra/target.tf"},
                    {"id": "origin", "source_file": "backend/origin.py"},
                ],
                "links": [
                    {
                        "source": "origin",
                        "target": "target",
                        "relation": relation,
                        "source_file": "backend/origin.py",
                    }
                ],
            }
            with self.subTest(relation=relation):
                bindings = compare_graphs(graph, graph)["diagnostics"][
                    "cross_runtime_bindings"
                ]
                self.assertEqual(
                    bindings["candidate"],
                    {"endpoint_count": 0, "endpoints": []},
                )

    def test_real_interop_and_semantic_edges_are_not_cross_runtime_bindings(self):
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
        bindings = compare_graphs(graph, graph)["diagnostics"][
            "cross_runtime_bindings"
        ]
        self.assertEqual(
            bindings["reference"], {"endpoint_count": 0, "endpoints": []}
        )
        self.assertEqual(
            bindings["candidate"], {"endpoint_count": 0, "endpoints": []}
        )

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
            "hyperedges": [],
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
            "hyperedges": [],
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
            "hyperedges": [],
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
            "hyperedges": [],
        }
        report = compare_graphs(reference, candidate, contract="reference-preserving")
        self.assertFalse(report["equal"])
        self.assertTrue(report["parity"]["reference_preservation"]["preserved"])
        self.assertTrue(report["gate"]["passed"])

    def test_context_refinement_is_limited_to_edge_records(self):
        reference = {
            "nodes": [{"id": "a"}],
            "links": [],
            "hyperedges": [
                {"id": "flow", "nodes": ["a"], "context": "call"}
            ],
        }
        candidate = {
            "nodes": [{"id": "a"}],
            "links": [],
            "hyperedges": [
                {
                    "id": "flow",
                    "nodes": ["a"],
                    "context": "import_guided_call",
                }
            ],
        }
        report = compare_graphs(reference, candidate, contract="reference-preserving")
        preservation = report["parity"]["reference_preservation"]
        self.assertFalse(report["equal"])
        self.assertFalse(preservation["preserved"])
        self.assertEqual(preservation["hyperedges"]["missing_or_changed_count"], 1)
        self.assertFalse(report["gate"]["passed"])


if __name__ == "__main__":
    unittest.main()
