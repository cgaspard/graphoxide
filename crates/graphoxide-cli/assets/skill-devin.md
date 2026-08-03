---
name: graphoxide
description: Query and maintain a deterministic code knowledge graph from Devin.
argument-hint: "[path or question]"
model: inherit
allowed-tools:
  - Bash
  - Read
  - Write
triggers:
  - user
  - model
---

# /graphoxide

Use an existing `graphoxide-out/graph.json` before broad source searches. Graphoxide is a
native executable, so invoke it directly; no Python interpreter or shell bootstrap is needed.

```text
graphoxide query "<question>"
graphoxide explain <node>
graphoxide path <a> <b>
```

Run `graphoxide audit . --json` when checking graph completeness. Build a missing graph with
`graphoxide extract .`, and rebuild it with `graphoxide update .` after structural changes.
Read `graphoxide-out/GRAPH_REPORT.md` only as a fallback for broad architecture review.
