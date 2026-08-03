"""Build Graphify and Graphoxide graphs from one corpus and compare them.

The default structural profile intentionally ignores presentation-only cluster
fields while retaining node identity/type/source data, directed edge facts and
multiplicity, confidence, source locations, and hyperedges.  The strict profile
compares every non-volatile serialized field.
"""

from __future__ import annotations

import argparse
from collections import Counter
import json
import math
from pathlib import Path, PureWindowsPath
import re
import subprocess
import sys
import tempfile
import unicodedata
from typing import Any, Iterable

from parity.upstream_oracle import ignored_executable_artifact


REPOSITORY = Path(__file__).resolve().parents[2]
VOLATILE_NODE_FIELDS = {
    "community",
    "community_name",
    "x",
    "y",
    "color",
    "degree",
    "size",
}
VOLATILE_GRAPH_FIELDS = {
    "graph",
    "built_at",
    "built_at_commit",
    "input_tokens",
    "output_tokens",
    "elapsed_seconds",
}
NODE_STRUCTURAL_FIELDS = {
    "id",
    "label",
    "name",
    "type",
    "node_type",
    "file_type",
    "source_file",
    "source_location",
    "line",
    "line_start",
    "line_end",
    "signature",
    "parent",
    "parent_id",
}
EDGE_STRUCTURAL_FIELDS = {
    "source",
    "target",
    "from",
    "to",
    "relation",
    "type",
    "confidence",
    "confidence_score",
    "source_file",
    "source_location",
    "line",
    "line_number",
    "context",
    "key",
}

# Keep this aligned with the extraction/runtime families.  The diagnostic below
# is deliberately stricter than comparing file extensions: Java/Kotlin and
# TypeScript/JavaScript are expected to share symbols, while Python/TypeScript
# are not. Runtime-owned declarative formats (for example XAML and Terraform)
# stay in their ecosystem so their identities cannot silently weld onto code
# from another runtime. Generic data/document formats such as JSON, YAML, TOML,
# XML, and Markdown remain unclassified because they legitimately describe many
# different runtimes.
LANGUAGE_FAMILY_BY_EXTENSION = {
    # JavaScript/TypeScript and script-bearing single-file components.
    ".js": "jsts",
    ".jsx": "jsts",
    ".mjs": "jsts",
    ".cjs": "jsts",
    ".ejs": "jsts",
    ".ets": "jsts",
    ".ts": "jsts",
    ".tsx": "jsts",
    ".mts": "jsts",
    ".cts": "jsts",
    ".vue": "jsts",
    ".svelte": "jsts",
    ".astro": "jsts",
    # JVM interop.
    ".java": "jvm",
    ".kt": "jvm",
    ".kts": "jvm",
    ".scala": "jvm",
    ".groovy": "jvm",
    ".gradle": "jvm",
    # Native languages that intentionally share headers/symbols.
    ".c": "native",
    ".h": "native",
    ".cc": "native",
    ".cpp": "native",
    ".cxx": "native",
    ".hpp": "native",
    ".hh": "native",
    ".hxx": "native",
    ".cu": "native",
    ".cuh": "native",
    ".metal": "native",
    ".m": "native",
    ".mm": "native",
    ".swift": "native",
    # Single-runtime families.
    ".py": "python",
    ".pyi": "python",
    ".go": "go",
    ".rs": "rust",
    ".rb": "ruby",
    ".rake": "ruby",
    ".php": "php",
    ".phtml": "php",
    ".php3": "php",
    ".php4": "php",
    ".php5": "php",
    ".php7": "php",
    ".phps": "php",
    ".cs": "dotnet",
    ".sln": "dotnet",
    ".slnx": "dotnet",
    ".csproj": "dotnet",
    ".fsproj": "dotnet",
    ".vbproj": "dotnet",
    ".razor": "dotnet",
    ".cshtml": "dotnet",
    ".xaml": "dotnet",
    ".lua": "lua",
    ".luau": "lua",
    ".toc": "lua",
    ".zig": "zig",
    ".ex": "elixir",
    ".exs": "elixir",
    ".jl": "julia",
    ".dart": "dart",
    ".sh": "shell",
    ".bash": "shell",
    ".zsh": "shell",
    ".dash": "shell",
    ".ksh": "shell",
    ".ps1": "powershell",
    ".psm1": "powershell",
    ".psd1": "powershell",
    ".f": "fortran",
    ".f90": "fortran",
    ".f95": "fortran",
    ".f03": "fortran",
    ".f08": "fortran",
    ".pas": "pascal",
    ".pp": "pascal",
    ".dpr": "pascal",
    ".dpk": "pascal",
    ".lpr": "pascal",
    ".inc": "pascal",
    ".dfm": "pascal",
    ".lfm": "pascal",
    ".lpk": "pascal",
    ".tf": "terraform",
    ".tfvars": "terraform",
    ".hcl": "terraform",
    ".sql": "sql",
    ".r": "r",
    ".v": "hardware",
    ".sv": "hardware",
    ".svh": "hardware",
    ".cls": "apex",
    ".trigger": "apex",
    ".dm": "dm",
    ".dme": "dm",
    ".dmi": "dm",
    ".dmm": "dm",
    ".dmf": "dm",
}

# These relations resolve or own an endpoint identity. Deliberately exclude
# analytical and configuration/data-flow facts such as `semantically_similar_to`,
# `depends_on`, `reads_from`, and the deliberately broad `uses`: those may connect
# distinct identities across runtimes and should be guarded by explicit corpus
# contracts instead of being mistaken for a welded symbol hub.
CROSS_RUNTIME_BINDING_RELATIONS = {
    "accesses",
    "bound_to",
    "calls",
    "case_of",
    "defines",
    "indirect_call",
    "imports",
    "imports_from",
    "instantiates",
    "listened_by",
    "re_exports",
    "references",
    "inherits",
    "implements",
    "extends",
    "contains",
    "method",
    "mixes_in",
    "requires",
}


class DifferentialError(RuntimeError):
    """The differential runner could not produce comparable graph artifacts."""


def _raw_edges(graph: dict[str, Any]) -> list[Any]:
    value = graph.get("links", graph.get("edges", []))
    return value if isinstance(value, list) else []


def _raw_records(graph: dict[str, Any], key: str) -> list[Any]:
    value = graph.get(key, [])
    return value if isinstance(value, list) else []


def _is_absolute_path(value: Any) -> bool:
    """Recognize native, Windows, UNC, and file-URI absolute paths."""
    if not isinstance(value, str) or not value:
        return False
    if value.casefold().startswith("file://"):
        return True
    return Path(value).is_absolute() or PureWindowsPath(value).is_absolute()


def _path_id_slug(value: str) -> str:
    """Approximate the path-derived ID form used by both implementations."""
    normalized = value.replace("\\", "/")
    normalized = re.sub(r"\.[^./]+$", "", normalized)
    return re.sub(r"[^0-9A-Za-z]+", "_", normalized).strip("_").casefold()


def _first_alias(record: dict[str, Any], aliases: tuple[str, ...]) -> Any:
    return next((record[field] for field in aliases if field in record), None)


def _json_values_equal(left: Any, right: Any) -> bool:
    """Compare JSON values without Python's bool/number type coercion."""
    if left is None or right is None:
        return left is None and right is None
    if isinstance(left, bool) or isinstance(right, bool):
        return isinstance(left, bool) and isinstance(right, bool) and left == right
    if isinstance(left, (int, float)) or isinstance(right, (int, float)):
        return (
            isinstance(left, (int, float))
            and not isinstance(left, bool)
            and isinstance(right, (int, float))
            and not isinstance(right, bool)
            and left == right
        )
    if isinstance(left, str) or isinstance(right, str):
        return isinstance(left, str) and isinstance(right, str) and left == right
    if isinstance(left, list) or isinstance(right, list):
        return (
            isinstance(left, list)
            and isinstance(right, list)
            and len(left) == len(right)
            and all(_json_values_equal(a, b) for a, b in zip(left, right))
        )
    if isinstance(left, dict) or isinstance(right, dict):
        return (
            isinstance(left, dict)
            and isinstance(right, dict)
            and left.keys() == right.keys()
            and all(_json_values_equal(left[key], right[key]) for key in left)
        )
    return type(left) is type(right) and left == right


def _alias_conflicts(
    kind: str, collection: str, index: int, record: dict[str, Any]
) -> list[dict[str, Any]]:
    """Report dual serialized forms that disagree before one is discarded."""
    groups: tuple[tuple[str, ...], ...]
    if kind == "node":
        groups = (("label", "name"), ("type", "node_type"))
    elif kind == "edge":
        groups = (
            ("_src", "source", "from"),
            ("_tgt", "target", "to"),
            ("relation", "type"),
        )
    else:
        groups = (("nodes", "members", "node_ids"),)

    conflicts: list[dict[str, Any]] = []
    for aliases in groups:
        present = {field: record[field] for field in aliases if field in record}
        if len(present) < 2:
            continue
        values = list(present.values())
        if kind == "hyperedge" and all(isinstance(value, list) for value in values):
            encoded = [
                sorted(
                    json.dumps(item, ensure_ascii=False, sort_keys=True)
                    for item in value
                )
                for value in values
            ]
            disagrees = any(value != encoded[0] for value in encoded[1:])
        else:
            disagrees = any(
                not _json_values_equal(value, values[0]) for value in values[1:]
            )
        if disagrees:
            conflicts.append(
                {
                    "kind": kind,
                    "collection": collection,
                    "index": index,
                    "aliases": list(aliases),
                    "values": present,
                }
            )
    return conflicts


def _valid_identity(value: Any) -> bool:
    return isinstance(value, str) and value != ""


def _pre_normalization_diagnostics(
    graph: dict[str, Any], *, max_examples: int
) -> dict[str, Any]:
    """Find malformed, ambiguous, dangling, or non-portable graph facts."""
    nodes = _raw_records(graph, "nodes")
    collections: list[tuple[str, str, list[Any]]] = [("node", "nodes", nodes)]
    for edge_key in ("links", "edges"):
        value = graph.get(edge_key)
        if isinstance(value, list):
            collections.append(("edge", edge_key, value))
    hyperedges = _raw_records(graph, "hyperedges")
    collections.append(("hyperedge", "hyperedges", hyperedges))

    absolute_source_paths: list[dict[str, Any]] = []
    absolute_ids: list[dict[str, Any]] = []
    alias_conflicts: list[dict[str, Any]] = []
    malformed_records: list[dict[str, Any]] = []
    dangling_references: list[dict[str, Any]] = []
    for key in ("nodes", "hyperedges"):
        if key not in graph:
            malformed_records.append(
                {
                    "kind": "collection",
                    "collection": key,
                    "reason": "missing_required_collection",
                }
            )
    if "links" not in graph and "edges" not in graph:
        malformed_records.append(
            {
                "kind": "collection",
                "collection": "links/edges",
                "reason": "missing_required_edge_collection",
            }
        )
    for key in ("nodes", "links", "edges", "hyperedges"):
        if key in graph and not isinstance(graph[key], list):
            malformed_records.append(
                {
                    "kind": "collection",
                    "collection": key,
                    "reason": "must_be_an_array",
                }
            )

    if isinstance(graph.get("links"), list) and isinstance(graph.get("edges"), list):
        links = Counter(
            json.dumps(
                _canonical_edge(record, None, "strict"),
                ensure_ascii=False,
                sort_keys=True,
                separators=(",", ":"),
            )
            for record in graph["links"]
        )
        edges = Counter(
            json.dumps(
                _canonical_edge(record, None, "strict"),
                ensure_ascii=False,
                sort_keys=True,
                separators=(",", ":"),
            )
            for record in graph["edges"]
        )
        if links != edges:
            alias_conflicts.append(
                {
                    "kind": "collection",
                    "collection": "links/edges",
                    "aliases": ["links", "edges"],
                    "reason": "dual_edge_collections_disagree",
                }
            )

    node_ids = {
        str(node["id"])
        for node in nodes
        if isinstance(node, dict) and _valid_identity(node.get("id"))
    }
    id_fields = {
        "node": ("id",),
        "edge": ("source", "target", "from", "to", "_src", "_tgt"),
        "hyperedge": ("id",),
    }
    for kind, collection, records in collections:
        for index, record in enumerate(records):
            if not isinstance(record, dict):
                malformed_records.append(
                    {
                        "kind": kind,
                        "collection": collection,
                        "index": index,
                        "reason": "record_must_be_an_object",
                    }
                )
                continue
            alias_conflicts.extend(_alias_conflicts(kind, collection, index, record))
            source_file = record.get("source_file")
            if _is_absolute_path(source_file):
                absolute_source_paths.append(
                    {
                        "kind": kind,
                        "collection": collection,
                        "index": index,
                        "field": "source_file",
                        "value": source_file,
                    }
                )
            for field in id_fields[kind]:
                value = record.get(field)
                if _is_absolute_path(value):
                    absolute_ids.append(
                        {
                            "kind": kind,
                            "collection": collection,
                            "index": index,
                            "field": field,
                            "value": value,
                            "reason": "absolute_path_value",
                        }
                    )

            # An ID can leak an absolute checkout path after punctuation has
            # been slugged away. Detect that form while the raw source path is
            # still available; canonical path comparison cannot recover it.
            node_id = record.get("id") if kind == "node" else None
            if (
                isinstance(node_id, str)
                and isinstance(source_file, str)
                and _is_absolute_path(source_file)
                and not _is_absolute_path(node_id)
            ):
                source_slug = _path_id_slug(source_file)
                node_slug = _path_id_slug(node_id)
                if source_slug and (
                    node_slug == source_slug or node_slug.startswith(f"{source_slug}_")
                ):
                    absolute_ids.append(
                        {
                            "kind": kind,
                            "collection": collection,
                            "index": index,
                            "field": "id",
                            "value": node_id,
                            "reason": "derived_from_absolute_source_file",
                            "source_file": source_file,
                        }
                    )

            if kind == "node":
                if not _valid_identity(record.get("id")):
                    malformed_records.append(
                        {
                            "kind": kind,
                            "collection": collection,
                            "index": index,
                            "reason": "missing_or_invalid_id",
                        }
                    )
            elif kind == "edge":
                source = _first_alias(record, ("_src", "source", "from"))
                target = _first_alias(record, ("_tgt", "target", "to"))
                relation = _first_alias(record, ("relation", "type"))
                invalid_fields = [
                    field
                    for field, value in (
                        ("source", source),
                        ("target", target),
                    )
                    if not _valid_identity(value)
                ]
                if not isinstance(relation, str) or not relation:
                    invalid_fields.append("relation")
                if invalid_fields:
                    malformed_records.append(
                        {
                            "kind": kind,
                            "collection": collection,
                            "index": index,
                            "reason": "missing_or_invalid_edge_fields",
                            "fields": invalid_fields,
                        }
                    )
                else:
                    for field, endpoint in (("source", source), ("target", target)):
                        if str(endpoint) not in node_ids:
                            dangling_references.append(
                                {
                                    "kind": kind,
                                    "collection": collection,
                                    "index": index,
                                    "field": field,
                                    "value": endpoint,
                                }
                            )
            else:
                if not _valid_identity(record.get("id")):
                    malformed_records.append(
                        {
                            "kind": kind,
                            "collection": collection,
                            "index": index,
                            "reason": "missing_or_invalid_id",
                        }
                    )
                members = _first_alias(record, ("nodes", "members", "node_ids"))
                if not isinstance(members, list) or not members:
                    malformed_records.append(
                        {
                            "kind": kind,
                            "collection": collection,
                            "index": index,
                            "reason": "missing_or_invalid_members",
                        }
                    )
                else:
                    for member_index, member in enumerate(members):
                        if not _valid_identity(member) or str(member) not in node_ids:
                            dangling_references.append(
                                {
                                    "kind": kind,
                                    "collection": collection,
                                    "index": index,
                                    "field": f"members[{member_index}]",
                                    "value": member,
                                }
                            )

    node_id_counts: Counter[str] = Counter()
    for node in nodes:
        if isinstance(node, dict) and _valid_identity(node.get("id")):
            node_id_counts[str(node["id"])] += 1
    duplicate_ids = [
        {"id": node_id, "count": count}
        for node_id, count in sorted(node_id_counts.items())
        if count > 1
    ]
    duplicate_occurrences = sum(item["count"] - 1 for item in duplicate_ids)
    hyperedge_id_counts: Counter[str] = Counter()
    for hyperedge in hyperedges:
        if isinstance(hyperedge, dict) and _valid_identity(hyperedge.get("id")):
            hyperedge_id_counts[str(hyperedge["id"])] += 1
    duplicate_hyperedge_ids = [
        {"id": hyperedge_id, "count": count}
        for hyperedge_id, count in sorted(hyperedge_id_counts.items())
        if count > 1
    ]
    duplicate_hyperedge_occurrences = sum(
        item["count"] - 1 for item in duplicate_hyperedge_ids
    )
    result = {
        "absolute_source_paths": {
            "count": len(absolute_source_paths),
            "examples": absolute_source_paths[:max_examples],
        },
        "absolute_ids": {
            "count": len(absolute_ids),
            "examples": absolute_ids[:max_examples],
        },
        "duplicate_node_ids": {
            "id_count": len(duplicate_ids),
            "duplicate_occurrence_count": duplicate_occurrences,
            "ids": duplicate_ids[:max_examples],
        },
        "duplicate_hyperedge_ids": {
            "id_count": len(duplicate_hyperedge_ids),
            "duplicate_occurrence_count": duplicate_hyperedge_occurrences,
            "ids": duplicate_hyperedge_ids[:max_examples],
        },
        "alias_conflicts": {
            "count": len(alias_conflicts),
            "examples": alias_conflicts[:max_examples],
        },
        "malformed_records": {
            "count": len(malformed_records),
            "examples": malformed_records[:max_examples],
        },
        "dangling_references": {
            "count": len(dangling_references),
            "examples": dangling_references[:max_examples],
        },
    }
    result["violation_count"] = (
        result["absolute_source_paths"]["count"]
        + result["absolute_ids"]["count"]
        + result["duplicate_node_ids"]["duplicate_occurrence_count"]
        + result["duplicate_hyperedge_ids"]["duplicate_occurrence_count"]
        + result["alias_conflicts"]["count"]
        + result["malformed_records"]["count"]
        + result["dangling_references"]["count"]
    )
    return result


def _language_family(source_file: Any) -> str | None:
    if not isinstance(source_file, str) or not source_file:
        return None
    normalized = source_file.replace("\\", "/")
    return LANGUAGE_FAMILY_BY_EXTENSION.get(Path(normalized).suffix.casefold())


def _cross_runtime_binding_diagnostics(
    graph: dict[str, Any], *, max_examples: int
) -> dict[str, Any]:
    """Find binding endpoints whose evidence spans incompatible runtimes.

    For each persisted endpoint, combine its own language family with the source
    provenance of incident symbol-binding edges. Two or more runtime families are
    an audit signal: they can reveal a bad cross-runtime resolution, but do not by
    themselves prove that identities were collapsed because explicit bridges are
    legitimate. Callers must review the reported relations and sources.
    """
    records_by_id: dict[str, list[dict[str, Any]]] = {}
    families_by_id: dict[str, set[str]] = {}
    sources_by_id: dict[str, set[str]] = {}
    relations_by_id: dict[str, set[str]] = {}
    for raw in _raw_records(graph, "nodes"):
        if not isinstance(raw, dict) or raw.get("id") is None:
            continue
        node_id = str(raw["id"])
        records_by_id.setdefault(node_id, []).append(raw)
        source_file = raw.get("source_file")
        family = _language_family(source_file)
        if family is not None:
            families_by_id.setdefault(node_id, set()).add(family)
            sources_by_id.setdefault(node_id, set()).add(str(source_file))

    # Keep node-owned provenance immutable while incident edges enrich the
    # diagnostic maps below. Falling back to an already-enriched endpoint would
    # make the result edge-order-dependent and could spread a false family
    # transitively across an otherwise valid graph.
    node_families_by_id = {
        node_id: set(families) for node_id, families in families_by_id.items()
    }
    node_sources_by_id = {
        node_id: set(sources) for node_id, sources in sources_by_id.items()
    }

    for raw in _raw_edges(graph):
        if not isinstance(raw, dict):
            continue
        relation = str(raw.get("relation", raw.get("type", "")))
        if relation not in CROSS_RUNTIME_BINDING_RELATIONS:
            continue
        source = raw.get("_src", raw.get("source", raw.get("from")))
        target = raw.get("_tgt", raw.get("target", raw.get("to")))
        edge_source_file = raw.get("source_file")
        edge_family = _language_family(edge_source_file)
        if edge_family is not None:
            edge_families = {edge_family}
            edge_sources = {str(edge_source_file)}
        elif not isinstance(edge_source_file, str) or not edge_source_file:
            # Built graphs from older/third-party producers may omit edge-level
            # source provenance. Symbol-binding relations are directed, so the
            # source endpoint's own immutable node provenance is the conservative
            # substitute. Never infer from the target: that would reverse caller,
            # importer, child, or owner provenance and create false attribution.
            source_id = str(source)
            edge_families = set(node_families_by_id.get(source_id, set()))
            edge_sources = set(node_sources_by_id.get(source_id, set()))
        else:
            # A present but deliberately unclassified document/data source is
            # not missing provenance and must not be relabeled from an endpoint.
            continue
        if not edge_families:
            continue
        for endpoint in (source, target):
            node_id = str(endpoint)
            if endpoint is None or node_id not in records_by_id:
                continue
            families_by_id.setdefault(node_id, set()).update(edge_families)
            sources_by_id.setdefault(node_id, set()).update(edge_sources)
            relations_by_id.setdefault(node_id, set()).add(relation)

    endpoints: list[dict[str, Any]] = []
    for node_id in sorted(records_by_id):
        families = sorted(families_by_id.get(node_id, set()))
        if len(families) < 2:
            continue
        labels = sorted(
            {
                str(record.get("label", record.get("name", "")))
                for record in records_by_id[node_id]
            }
        )
        endpoints.append(
            {
                "id": node_id,
                "labels": labels,
                "families": families,
                "source_files": sorted(sources_by_id.get(node_id, set())),
                "relations": sorted(relations_by_id.get(node_id, set())),
            }
        )
    return {
        "endpoint_count": len(endpoints),
        "endpoints": endpoints[:max_examples],
    }


def _reject_nonfinite_json_constant(value: str) -> None:
    raise ValueError(f"non-finite JSON number {value}")


def _reject_duplicate_json_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"duplicate JSON object key {key!r}")
        value[key] = item
    return value


def _require_finite_json_numbers(value: Any) -> None:
    if isinstance(value, float) and not math.isfinite(value):
        raise ValueError(f"non-finite JSON number {value}")
    if isinstance(value, list):
        for item in value:
            _require_finite_json_numbers(item)
    elif isinstance(value, dict):
        for item in value.values():
            _require_finite_json_numbers(item)


def _load_object(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(
            path.read_text(encoding="utf-8"),
            parse_constant=_reject_nonfinite_json_constant,
            object_pairs_hook=_reject_duplicate_json_object,
        )
        _require_finite_json_numbers(value)
    except (OSError, UnicodeError, ValueError) as error:
        raise DifferentialError(f"could not load graph {path}: {error}") from error
    if not isinstance(value, dict):
        raise DifferentialError(f"graph {path} is not a JSON object")
    return value


def _portable_path(value: Any, corpus: Path | None) -> Any:
    if not isinstance(value, str):
        return value
    normalized = unicodedata.normalize("NFC", value.replace("\\", "/"))
    if corpus is None:
        return normalized
    path = Path(normalized)
    if not path.is_absolute():
        return normalized.removeprefix("./")
    try:
        resolved = Path(unicodedata.normalize("NFC", path.resolve().as_posix()))
        resolved_corpus = Path(
            unicodedata.normalize("NFC", corpus.resolve().as_posix())
        )
        return unicodedata.normalize(
            "NFC", resolved.relative_to(resolved_corpus).as_posix()
        )
    except (OSError, ValueError):
        return normalized


def _canonical_value(value: Any, corpus: Path | None) -> Any:
    if isinstance(value, dict):
        return {
            key: _canonical_value(item, corpus)
            for key, item in sorted(value.items())
        }
    if isinstance(value, list):
        return [_canonical_value(item, corpus) for item in value]
    return value


def _canonical_node(
    raw: Any, corpus: Path | None, profile: str
) -> dict[str, Any]:
    if not isinstance(raw, dict):
        return {"__malformed__": _canonical_value(raw, corpus)}
    node = dict(raw)
    if "label" not in node and "name" in node:
        node["label"] = node["name"]
    node.pop("name", None)
    if "type" not in node and "node_type" in node:
        node["type"] = node["node_type"]
    node.pop("node_type", None)
    if "source_file" in node:
        node["source_file"] = _portable_path(node["source_file"], corpus)
    if profile == "structure":
        node = {key: value for key, value in node.items() if key in NODE_STRUCTURAL_FIELDS}
    else:
        node = {key: value for key, value in node.items() if key not in VOLATILE_NODE_FIELDS}
    return _canonical_value(node, corpus)


def _canonical_edge(
    raw: Any, corpus: Path | None, profile: str
) -> dict[str, Any]:
    if not isinstance(raw, dict):
        return {"__malformed__": _canonical_value(raw, corpus)}
    edge = dict(raw)
    source = edge.get("_src", edge.get("source", edge.get("from")))
    target = edge.get("_tgt", edge.get("target", edge.get("to")))
    relation = edge.get("relation", edge.get("type", ""))
    edge["source"] = source
    edge["target"] = target
    edge["relation"] = relation
    for alias in ["from", "to", "type", "_src", "_tgt"]:
        edge.pop(alias, None)
    if "source_file" in edge:
        edge["source_file"] = _portable_path(edge["source_file"], corpus)
    if profile == "structure":
        edge = {key: value for key, value in edge.items() if key in EDGE_STRUCTURAL_FIELDS}
    return _canonical_value(edge, corpus)


def _canonical_hyperedge(raw: Any, corpus: Path | None) -> Any:
    if not isinstance(raw, dict):
        return _canonical_value(raw, corpus)
    hyperedge = dict(raw)
    if not isinstance(hyperedge.get("nodes"), list):
        for alias in ("members", "node_ids"):
            if isinstance(hyperedge.get(alias), list):
                hyperedge["nodes"] = hyperedge[alias]
                break
    hyperedge.pop("members", None)
    hyperedge.pop("node_ids", None)
    if isinstance(hyperedge.get("nodes"), list):
        hyperedge["nodes"] = sorted(
            hyperedge["nodes"], key=lambda item: json.dumps(item, sort_keys=True)
        )
    if "source_file" in hyperedge:
        hyperedge["source_file"] = _portable_path(hyperedge["source_file"], corpus)
    return _canonical_value(hyperedge, corpus)


def canonical_graph(
    graph: dict[str, Any], *, corpus: Path | None = None, profile: str = "structure"
) -> dict[str, Any]:
    """Return an order-independent, direction-preserving comparison form."""
    if profile not in {"structure", "strict"}:
        raise ValueError(f"unknown comparison profile: {profile}")
    nodes = [
        _canonical_node(node, corpus, profile)
        for node in _raw_records(graph, "nodes")
    ]
    raw_edges = _raw_edges(graph)
    edges = [_canonical_edge(edge, corpus, profile) for edge in raw_edges]
    hyperedges = [
        _canonical_hyperedge(hyperedge, corpus)
        for hyperedge in _raw_records(graph, "hyperedges")
    ]
    metadata = {
        key: _canonical_value(value, corpus)
        for key, value in graph.items()
        if key not in {"nodes", "links", "edges", "hyperedges"}
        and (profile == "structure" and key in {"directed", "multigraph"}
             or profile == "strict" and key not in VOLATILE_GRAPH_FIELDS)
    }
    return {
        "metadata": metadata,
        "nodes": sorted(nodes, key=_sort_key),
        "edges": sorted(edges, key=_sort_key),
        "hyperedges": sorted(hyperedges, key=_sort_key),
    }


def _sort_key(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def _counter(values: Iterable[Any]) -> Counter[str]:
    return Counter(_sort_key(value) for value in values)


def _examples(counter: Counter[str], limit: int) -> list[Any]:
    examples: list[Any] = []
    for encoded, count in sorted(counter.items()):
        for _ in range(min(count, limit - len(examples))):
            examples.append(json.loads(encoded))
        if len(examples) >= limit:
            break
    return examples


def _counter_items(counter: Counter[str]) -> Iterable[tuple[Any, int]]:
    for encoded, count in sorted(counter.items()):
        yield json.loads(encoded), count


def _group_name(value: Any) -> str:
    if value is None or value == "":
        return "<empty>"
    return str(value)


def _sorted_counts(counts: Counter[str]) -> dict[str, int]:
    return {key: counts[key] for key in sorted(counts)}


def _counter_grouped_by(counter: Counter[str], field: str) -> dict[str, int]:
    groups: Counter[str] = Counter()
    for value, count in _counter_items(counter):
        field_value = value.get(field) if isinstance(value, dict) else None
        groups[_group_name(field_value)] += count
    return _sorted_counts(groups)


def _node_groups(nodes: Iterable[dict[str, Any]]) -> dict[str, list[dict[str, Any]]]:
    groups: dict[str, list[dict[str, Any]]] = {}
    for node in nodes:
        groups.setdefault(str(node.get("id")), []).append(node)
    return groups


def _node_source_groups(
    node_ids: Iterable[str], groups: dict[str, list[dict[str, Any]]]
) -> dict[str, int]:
    counts: Counter[str] = Counter()
    for node_id in node_ids:
        for node in groups[node_id]:
            counts[_group_name(node.get("source_file"))] += 1
    return _sorted_counts(counts)


def _mismatch_groups(
    mismatched: Iterable[dict[str, Any]],
    left_nodes: dict[str, list[dict[str, Any]]],
    right_nodes: dict[str, list[dict[str, Any]]],
) -> dict[str, Any]:
    fields: Counter[str] = Counter()
    left_sources: Counter[str] = Counter()
    right_sources: Counter[str] = Counter()
    for mismatch in mismatched:
        node_id = mismatch["id"]
        fields.update(mismatch["fields"].keys())
        for node in left_nodes[node_id]:
            left_sources[_group_name(node.get("source_file"))] += 1
        for node in right_nodes[node_id]:
            right_sources[_group_name(node.get("source_file"))] += 1
    return {
        "mismatched_by_field": _sorted_counts(fields),
        "mismatched_by_source_file": {
            "reference": _sorted_counts(left_sources),
            "candidate": _sorted_counts(right_sources),
        },
    }


def _compare_node_group(
    reference: list[dict[str, Any]], candidate: list[dict[str, Any]]
) -> dict[str, Any]:
    """Compare every record for one ID without collapsing duplicate IDs."""
    if len(reference) == 1 and len(candidate) == 1:
        return _field_changes(reference[0], candidate[0])
    left_counter = _counter(reference)
    right_counter = _counter(candidate)
    changed: dict[str, Any] = {}
    if len(reference) != len(candidate):
        changed["__occurrences__"] = {
            "reference": len(reference),
            "candidate": len(candidate),
        }
    missing = left_counter - right_counter
    extra = right_counter - left_counter
    if missing or extra:
        changed["__records__"] = {
            "reference_only_count": sum(missing.values()),
            "candidate_only_count": sum(extra.values()),
        }
    return changed


def _edge_partition(
    difference: Counter[str], *, shared_ids: set[str], side_only_ids: set[str]
) -> dict[str, int]:
    """Partition edge differences by whether their endpoint identities are shared."""
    counts: Counter[str] = Counter()
    for edge, count in _counter_items(difference):
        if not isinstance(edge, dict):
            counts["unresolved"] += count
            continue
        endpoints = {str(edge.get("source")), str(edge.get("target"))}
        if endpoints <= shared_ids:
            counts["shared"] += count
        elif endpoints & side_only_ids:
            counts["side_only"] += count
        else:
            counts["unresolved"] += count
    return {
        "shared": counts["shared"],
        "side_only": counts["side_only"],
        "unresolved": counts["unresolved"],
    }


def _hyperedge_partition(
    difference: Counter[str], *, shared_ids: set[str], side_only_ids: set[str]
) -> dict[str, int]:
    counts: Counter[str] = Counter()
    for hyperedge, count in _counter_items(difference):
        members = hyperedge.get("nodes") if isinstance(hyperedge, dict) else None
        if not isinstance(members, list) or not members:
            counts["unresolved"] += count
            continue
        member_ids = {str(member) for member in members}
        if member_ids <= shared_ids:
            counts["shared"] += count
        elif member_ids & side_only_ids:
            counts["side_only"] += count
        else:
            counts["unresolved"] += count
    return {
        "shared": counts["shared"],
        "side_only": counts["side_only"],
        "unresolved": counts["unresolved"],
    }


def _field_changes(reference: dict[str, Any], candidate: dict[str, Any]) -> dict[str, Any]:
    changed: dict[str, Any] = {}
    for key in sorted(reference.keys() | candidate.keys()):
        reference_present = key in reference
        candidate_present = key in candidate
        reference_value = reference.get(key)
        candidate_value = candidate.get(key)
        if reference_present == candidate_present and _json_values_equal(
            reference_value, candidate_value
        ):
            continue
        change = {
            "reference": reference_value,
            "candidate": candidate_value,
        }
        if not reference_present or not candidate_present:
            change.update(
                {
                    "reference_present": reference_present,
                    "candidate_present": candidate_present,
                }
            )
        changed[key] = change
    return changed


def _value_preserved(
    reference: Any,
    candidate: Any,
    *,
    allow_edge_context_refinement: bool = False,
) -> bool:
    """Return whether the candidate contains the complete reference value.

    Object fields may be additive in the candidate. Lists and scalar values
    remain exact because their order/multiplicity can carry graph meaning.
    """
    if isinstance(reference, dict):
        if not isinstance(candidate, dict):
            return False
        for key, value in reference.items():
            if key not in candidate:
                return False
            if (
                allow_edge_context_refinement
                and key == "context"
                and value == "call"
                and candidate[key] == "import_guided_call"
            ):
                continue
            if not _value_preserved(value, candidate[key]):
                return False
        return True
    return _json_values_equal(reference, candidate)


def _preserved_record_count(
    reference: list[dict[str, Any]],
    candidate: list[dict[str, Any]],
    *,
    allow_edge_context_refinement: bool = False,
) -> int:
    """Maximum multiplicity-aware matching of reference records to candidates."""
    compatible = [
        [
            candidate_index
            for candidate_index, candidate_record in enumerate(candidate)
            if _value_preserved(
                reference_record,
                candidate_record,
                allow_edge_context_refinement=allow_edge_context_refinement,
            )
        ]
        for reference_record in reference
    ]
    matched_reference_by_candidate: dict[int, int] = {}

    def augment(reference_index: int, seen: set[int]) -> bool:
        for candidate_index in compatible[reference_index]:
            if candidate_index in seen:
                continue
            seen.add(candidate_index)
            previous = matched_reference_by_candidate.get(candidate_index)
            if previous is None or augment(previous, seen):
                matched_reference_by_candidate[candidate_index] = reference_index
                return True
        return False

    matched = 0
    for reference_index in sorted(
        range(len(reference)), key=lambda index: len(compatible[index])
    ):
        matched += int(augment(reference_index, set()))
    return matched


def _reference_preservation(
    *,
    left: dict[str, Any],
    right: dict[str, Any],
    pre_normalization_valid: bool,
) -> dict[str, Any]:
    collections = {}
    for key in ("nodes", "edges", "hyperedges"):
        matched = _preserved_record_count(
            left[key],
            right[key],
            allow_edge_context_refinement=key == "edges",
        )
        collections[key] = {
            "preserved": matched == len(left[key]),
            "reference_count": len(left[key]),
            "matched_count": matched,
            "missing_or_changed_count": len(left[key]) - matched,
        }
    metadata_preserved = _value_preserved(left["metadata"], right["metadata"])
    preserved = all(
        [
            pre_normalization_valid,
            metadata_preserved,
            *(collection["preserved"] for collection in collections.values()),
        ]
    )
    return {
        "preserved": preserved,
        "metadata_preserved": metadata_preserved,
        "pre_normalization_valid": pre_normalization_valid,
        **collections,
    }


def _parity_partition(
    *,
    left: dict[str, Any],
    right: dict[str, Any],
    left_nodes: dict[str, list[dict[str, Any]]],
    right_nodes: dict[str, list[dict[str, Any]]],
    mismatched: list[dict[str, Any]],
    edge_missing: Counter[str],
    edge_extra: Counter[str],
    hyperedge_missing: Counter[str],
    hyperedge_extra: Counter[str],
    metadata_changed: bool,
    portability_or_identity_violations: bool,
) -> dict[str, Any]:
    """Separate common-identity parity from neutral candidate additions.

    A candidate-only record is called an extension only in the mechanical sense:
    it touches an identity absent from the reference. The label does not assert
    that the addition is desirable or correct.
    """
    left_ids = set(left_nodes)
    right_ids = set(right_nodes)
    shared_ids = left_ids & right_ids
    reference_only_ids = left_ids - right_ids
    candidate_only_ids = right_ids - left_ids
    left_edge_partition = _edge_partition(
        edge_missing,
        shared_ids=shared_ids,
        side_only_ids=reference_only_ids,
    )
    right_edge_partition = _edge_partition(
        edge_extra,
        shared_ids=shared_ids,
        side_only_ids=candidate_only_ids,
    )
    left_hyperedge_partition = _hyperedge_partition(
        hyperedge_missing,
        shared_ids=shared_ids,
        side_only_ids=reference_only_ids,
    )
    right_hyperedge_partition = _hyperedge_partition(
        hyperedge_extra,
        shared_ids=shared_ids,
        side_only_ids=candidate_only_ids,
    )

    left_shared_edges = _counter(
        edge
        for edge in left["edges"]
        if {str(edge.get("source")), str(edge.get("target"))} <= shared_ids
    )
    right_shared_edges = _counter(
        edge
        for edge in right["edges"]
        if {str(edge.get("source")), str(edge.get("target"))} <= shared_ids
    )
    matching_shared_edges = sum((left_shared_edges & right_shared_edges).values())
    left_shared_hyperedges = _counter(
        hyperedge
        for hyperedge in left["hyperedges"]
        if isinstance(hyperedge, dict)
        and isinstance(hyperedge.get("nodes"), list)
        and hyperedge["nodes"]
        and {str(member) for member in hyperedge["nodes"]} <= shared_ids
    )
    right_shared_hyperedges = _counter(
        hyperedge
        for hyperedge in right["hyperedges"]
        if isinstance(hyperedge, dict)
        and isinstance(hyperedge.get("nodes"), list)
        and hyperedge["nodes"]
        and {str(member) for member in hyperedge["nodes"]} <= shared_ids
    )
    matching_shared_hyperedges = sum(
        (left_shared_hyperedges & right_shared_hyperedges).values()
    )
    mismatched_ids = {item["id"] for item in mismatched}
    shared_equal = not any(
        [
            metadata_changed,
            mismatched,
            left_edge_partition["shared"],
            right_edge_partition["shared"],
            left_hyperedge_partition["shared"],
            right_hyperedge_partition["shared"],
        ]
    )
    return {
        "contract": "normalized_structural",
        "pre_normalization_valid": not portability_or_identity_violations,
        "shared": {
            "equal": shared_equal,
            "node_id_count": len(shared_ids),
            "matching_node_id_count": len(shared_ids - mismatched_ids),
            "mismatched_node_id_count": len(mismatched_ids),
            "reference_edge_count": sum(left_shared_edges.values()),
            "candidate_edge_count": sum(right_shared_edges.values()),
            "matching_edge_count": matching_shared_edges,
            "missing_edge_count": left_edge_partition["shared"],
            "extra_edge_count": right_edge_partition["shared"],
            "reference_hyperedge_count": sum(left_shared_hyperedges.values()),
            "candidate_hyperedge_count": sum(right_shared_hyperedges.values()),
            "matching_hyperedge_count": matching_shared_hyperedges,
            "missing_hyperedge_count": left_hyperedge_partition["shared"],
            "extra_hyperedge_count": right_hyperedge_partition["shared"],
        },
        "extensions": {
            "classification": (
                "candidate_only is additive relative to the reference; it is not an "
                "assertion that the addition is supported or correct"
            ),
            "candidate_only_node_id_count": len(candidate_only_ids),
            "candidate_only_node_record_count": sum(
                len(right_nodes[node_id]) for node_id in candidate_only_ids
            ),
            "candidate_extension_edge_count": right_edge_partition["side_only"],
            "candidate_extension_hyperedge_count": right_hyperedge_partition["side_only"],
            "candidate_unresolved_extra_edge_count": right_edge_partition["unresolved"],
            "candidate_unresolved_extra_hyperedge_count": right_hyperedge_partition[
                "unresolved"
            ],
            "reference_only_node_id_count": len(reference_only_ids),
            "reference_only_node_record_count": sum(
                len(left_nodes[node_id]) for node_id in reference_only_ids
            ),
            "reference_only_edge_count": left_edge_partition["side_only"],
            "reference_only_hyperedge_count": left_hyperedge_partition["side_only"],
            "reference_unresolved_missing_edge_count": left_edge_partition["unresolved"],
            "reference_unresolved_missing_hyperedge_count": left_hyperedge_partition[
                "unresolved"
            ],
        },
    }


def compare_graphs(
    reference: dict[str, Any],
    candidate: dict[str, Any],
    *,
    corpus: Path | None = None,
    profile: str = "structure",
    max_examples: int = 20,
    fail_on_candidate_cross_runtime_bindings: bool = False,
    contract: str = "exact",
) -> dict[str, Any]:
    """Return a machine-readable, multiplicity-aware parity report."""
    if contract not in {"exact", "reference-preserving"}:
        raise ValueError(f"unknown comparison contract: {contract}")
    max_examples = max(0, max_examples)
    pre_normalization = {
        "reference": _pre_normalization_diagnostics(
            reference, max_examples=max_examples
        ),
        "candidate": _pre_normalization_diagnostics(
            candidate, max_examples=max_examples
        ),
    }
    has_pre_normalization_violation = any(
        side["violation_count"] for side in pre_normalization.values()
    )
    cross_runtime_bindings = {
        "reference": _cross_runtime_binding_diagnostics(
            reference, max_examples=max_examples
        ),
        "candidate": _cross_runtime_binding_diagnostics(
            candidate, max_examples=max_examples
        ),
    }
    candidate_cross_runtime_binding_violation = (
        fail_on_candidate_cross_runtime_bindings
        and cross_runtime_bindings["candidate"]["endpoint_count"] > 0
    )
    left = canonical_graph(reference, corpus=corpus, profile=profile)
    right = canonical_graph(candidate, corpus=corpus, profile=profile)
    report: dict[str, Any] = {
        "profile": profile,
        "equal": not has_pre_normalization_violation,
        "summary": {},
        "metadata": {},
        "nodes": {},
        "edges": {},
        "hyperedges": {},
        "diagnostics": {
            "pre_normalization": pre_normalization,
            "cross_runtime_bindings": {
                **cross_runtime_bindings,
                "candidate_gate_enabled": fail_on_candidate_cross_runtime_bindings,
                "candidate_gate_passed": not candidate_cross_runtime_binding_violation,
            },
        },
        "parity": {},
        "gate": {"contract": contract, "passed": False},
    }
    report["metadata"] = _field_changes(left["metadata"], right["metadata"])
    if report["metadata"]:
        report["equal"] = False

    left_nodes = _node_groups(left["nodes"])
    right_nodes = _node_groups(right["nodes"])
    missing_ids = sorted(left_nodes.keys() - right_nodes.keys())
    extra_ids = sorted(right_nodes.keys() - left_nodes.keys())
    mismatched: list[dict[str, Any]] = []
    for node_id in sorted(left_nodes.keys() & right_nodes.keys()):
        changes = _compare_node_group(left_nodes[node_id], right_nodes[node_id])
        if changes:
            mismatched.append({"id": node_id, "fields": changes})
    node_groups = {
        "missing_by_source_file": _node_source_groups(missing_ids, left_nodes),
        "extra_by_source_file": _node_source_groups(extra_ids, right_nodes),
        **_mismatch_groups(mismatched, left_nodes, right_nodes),
    }
    report["nodes"] = {
        "missing_count": len(missing_ids),
        "extra_count": len(extra_ids),
        "mismatched_count": len(mismatched),
        "missing": missing_ids[:max_examples],
        "extra": extra_ids[:max_examples],
        "mismatched": mismatched[:max_examples],
        "duplicates": {
            "reference": pre_normalization["reference"]["duplicate_node_ids"],
            "candidate": pre_normalization["candidate"]["duplicate_node_ids"],
        },
        "groups": node_groups,
    }
    if missing_ids or extra_ids or mismatched:
        report["equal"] = False

    left_edges = _counter(left["edges"])
    right_edges = _counter(right["edges"])
    edge_missing = left_edges - right_edges
    edge_extra = right_edges - left_edges
    report["edges"] = {
        "missing_count": sum(edge_missing.values()),
        "extra_count": sum(edge_extra.values()),
        "missing": _examples(edge_missing, max_examples),
        "extra": _examples(edge_extra, max_examples),
        "groups": {
            "missing_by_source_file": _counter_grouped_by(
                edge_missing, "source_file"
            ),
            "extra_by_source_file": _counter_grouped_by(edge_extra, "source_file"),
            "missing_by_relation": _counter_grouped_by(edge_missing, "relation"),
            "extra_by_relation": _counter_grouped_by(edge_extra, "relation"),
        },
    }
    if edge_missing or edge_extra:
        report["equal"] = False

    left_hyperedges = _counter(left["hyperedges"])
    right_hyperedges = _counter(right["hyperedges"])
    hyperedge_missing = left_hyperedges - right_hyperedges
    hyperedge_extra = right_hyperedges - left_hyperedges
    report["hyperedges"] = {
        "missing_count": sum(hyperedge_missing.values()),
        "extra_count": sum(hyperedge_extra.values()),
        "missing": _examples(hyperedge_missing, max_examples),
        "extra": _examples(hyperedge_extra, max_examples),
        "groups": {
            "missing_by_source_file": _counter_grouped_by(
                hyperedge_missing, "source_file"
            ),
            "extra_by_source_file": _counter_grouped_by(
                hyperedge_extra, "source_file"
            ),
        },
    }
    if hyperedge_missing or hyperedge_extra:
        report["equal"] = False

    report["summary"] = {
        "reference": {key: len(left[key]) for key in ("nodes", "edges", "hyperedges")},
        "candidate": {key: len(right[key]) for key in ("nodes", "edges", "hyperedges")},
    }
    report["parity"] = _parity_partition(
        left=left,
        right=right,
        left_nodes=left_nodes,
        right_nodes=right_nodes,
        mismatched=mismatched,
        edge_missing=edge_missing,
        edge_extra=edge_extra,
        hyperedge_missing=hyperedge_missing,
        hyperedge_extra=hyperedge_extra,
        metadata_changed=bool(report["metadata"]),
        portability_or_identity_violations=has_pre_normalization_violation,
    )
    report["parity"]["reference_preservation"] = _reference_preservation(
        left=left,
        right=right,
        pre_normalization_valid=not has_pre_normalization_violation,
    )
    contract_passed = (
        report["equal"]
        if contract == "exact"
        else report["parity"]["reference_preservation"]["preserved"]
    )
    report["gate"]["passed"] = (
        contract_passed and not candidate_cross_runtime_binding_violation
    )
    return report


def _run(command: list[str], cwd: Path, timeout: int) -> None:
    completed = subprocess.run(
        command,
        cwd=cwd,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=timeout,
        check=False,
    )
    if completed.returncode:
        rendered = " ".join(command)
        raise DifferentialError(
            f"command failed ({completed.returncode}): {rendered}\n{completed.stdout}"
        )


def _verify_clean_pinned_checkout(upstream: Path, expected: str) -> None:
    """Require the exact commit and an executable-source-pure worktree."""
    completed = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=upstream,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    actual = completed.stdout.strip()
    if completed.returncode or actual != expected:
        raise DifferentialError(
            f"upstream checkout is at {actual or '<unknown>'}, expected pinned {expected}"
        )
    status = subprocess.run(
        ["git", "status", "--porcelain=v1", "--untracked-files=all"],
        cwd=upstream,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if status.returncode:
        raise DifferentialError(
            f"could not verify upstream checkout cleanliness: {status.stderr.strip()}"
        )
    if status.stdout.strip():
        first = status.stdout.splitlines()[0]
        raise DifferentialError(
            "upstream checkout has staged, unstaged, or non-ignored untracked changes; "
            f"refusing a contaminated reference ({first})"
        )
    ignored = subprocess.run(
        [
            "git",
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ],
        cwd=upstream,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if ignored.returncode:
        raise DifferentialError(
            "could not inspect ignored upstream artifacts: "
            f"{ignored.stderr.strip()}"
        )
    artifact = ignored_executable_artifact(upstream, ignored.stdout.split("\0"))
    if artifact is not None:
        raise DifferentialError(
            "upstream checkout has an ignored executable-source artifact that can "
            f"shadow the pinned oracle; refusing a contaminated reference ({artifact})"
        )


def _verify_upstream_pin(upstream: Path) -> None:
    lock = _load_object(REPOSITORY / "parity/upstream.lock.json")
    _verify_clean_pinned_checkout(upstream, str(lock.get("commit", "")))


def build_and_compare(args: argparse.Namespace) -> dict[str, Any]:
    corpus = args.corpus.resolve()
    upstream = args.upstream.resolve()
    if not corpus.is_dir():
        raise DifferentialError(f"corpus is not a directory: {corpus}")
    if not (upstream / ".git").exists():
        raise DifferentialError(f"upstream is not a Git checkout: {upstream}")
    _verify_upstream_pin(upstream)

    binary = args.graphoxide_bin.resolve()
    if args.build or not binary.is_file():
        _run(
            ["cargo", "build", "-p", "graphoxide-cli", "--locked"],
            REPOSITORY,
            args.timeout,
        )
    if not binary.is_file():
        raise DifferentialError(f"Graphoxide binary was not produced: {binary}")

    def execute(work: Path, *, retained: bool) -> dict[str, Any]:
        reference_root = work / "reference"
        candidate_root = work / "candidate"
        reference_root.mkdir(parents=True, exist_ok=True)
        candidate_root.mkdir(parents=True, exist_ok=True)
        _verify_upstream_pin(upstream)
        with tempfile.TemporaryDirectory(
            prefix="graphoxide-reference-pycache-"
        ) as reference_pycache:
            _run(
                [
                    "uv",
                    "run",
                    "--isolated",
                    "--no-editable",
                    "--frozen",
                    "--project",
                    str(upstream),
                    "python",
                    "-I",
                    "-X",
                    f"pycache_prefix={reference_pycache}",
                    "-m",
                    "graphify",
                    "extract",
                    str(corpus),
                    "--code-only",
                    "--force",
                    "--out",
                    str(reference_root),
                ],
                reference_root,
                args.timeout,
            )
        # `uv run --frozen` may populate ignored environments/caches, but it
        # must not alter or create any reference source used by the oracle.
        _verify_upstream_pin(upstream)
        _run(
            [
                str(binary),
                "extract",
                str(corpus),
                "--code-only",
                "--force",
                "--out",
                str(candidate_root),
            ],
            REPOSITORY,
            args.timeout,
        )
        reference_graph = reference_root / "graphify-out/graph.json"
        candidate_graph = candidate_root / "graphoxide-out/graph.json"
        report = compare_graphs(
            _load_object(reference_graph),
            _load_object(candidate_graph),
            corpus=corpus,
            profile=args.profile,
            max_examples=args.max_examples,
            fail_on_candidate_cross_runtime_bindings=(
                args.fail_on_candidate_cross_runtime_bindings
            ),
            contract=args.contract,
        )
        report["artifacts"] = {"retained": retained, "work_dir": str(work)}
        if retained:
            report["artifacts"].update(
                {
                    "reference": str(reference_graph),
                    "candidate": str(candidate_graph),
                }
            )
        return report

    if args.work_dir:
        retained_root = args.work_dir.resolve()
        retained_root.mkdir(parents=True, exist_ok=True)
        # Never trust or recursively delete a caller-provided retained directory.
        # A fresh child makes stale artifacts impossible while preserving every
        # prior run for investigation.
        fresh = Path(tempfile.mkdtemp(prefix="run-", dir=retained_root))
        return execute(fresh, retained=True)
    with tempfile.TemporaryDirectory(prefix="graphoxide-differential-") as temporary:
        return execute(Path(temporary), retained=False)


def _write_report(report: dict[str, Any], output: Path | None) -> None:
    encoded = json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    if output is None:
        sys.stdout.write(encoded)
    else:
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(encoded, encoding="utf-8")


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    compare = subparsers.add_parser("compare", help="compare two existing graph.json files")
    compare.add_argument("reference", type=Path)
    compare.add_argument("candidate", type=Path)
    compare.add_argument("--corpus", type=Path)
    run = subparsers.add_parser("run", help="build both implementations, then compare")
    run.add_argument("--corpus", type=Path, required=True)
    run.add_argument("--upstream", type=Path, default=REPOSITORY / "upstream")
    run.add_argument(
        "--graphoxide-bin",
        type=Path,
        default=REPOSITORY / "target/debug/graphoxide",
    )
    run.add_argument("--build", action="store_true")
    run.add_argument("--work-dir", type=Path)
    run.add_argument("--timeout", type=int, default=900)
    for child in (compare, run):
        child.add_argument("--profile", choices=("structure", "strict"), default="structure")
        child.add_argument(
            "--contract",
            choices=("exact", "reference-preserving"),
            default="exact",
            help=(
                "exact requires identical normalized graphs; reference-preserving "
                "allows audited candidate-only additions"
            ),
        )
        child.add_argument("--max-examples", type=int, default=20)
        child.add_argument(
            "--fail-on-candidate-cross-runtime-bindings",
            "--fail-on-candidate-identity-hubs",
            dest="fail_on_candidate_cross_runtime_bindings",
            action="store_true",
            help=(
                "fail when a candidate symbol-binding endpoint carries evidence "
                "from incompatible runtime families"
            ),
        )
        child.add_argument("--output", type=Path)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        if args.command == "compare":
            report = compare_graphs(
                _load_object(args.reference),
                _load_object(args.candidate),
                corpus=args.corpus,
                profile=args.profile,
                max_examples=args.max_examples,
                fail_on_candidate_cross_runtime_bindings=(
                    args.fail_on_candidate_cross_runtime_bindings
                ),
                contract=args.contract,
            )
        else:
            report = build_and_compare(args)
        _write_report(report, args.output)
        return 0 if report["gate"]["passed"] else 1
    except (DifferentialError, subprocess.TimeoutExpired) as error:
        print(f"graph differential failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
