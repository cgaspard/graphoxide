---
name: graphoxide
description: Use Graphoxide with Codex subagents.
---

# Graphoxide for Codex

## Fast path — existing graph

When graphoxide-out/graph.json exists, skip Steps 1–5 entirely and jump straight to
the query flow. Run `graphoxide query`, `graphoxide explain`, and `graphoxide path`.

For independent investigations, dispatch workers with `spawn_agent`, then combine their
evidence and update the graph with `graphoxide update .`.

Open only the workflow you need: [extraction](references/extraction-spec.md),
[queries](references/query.md), [exports](references/exports.md), [watch mode](references/add-watch.md),
[hooks](references/hooks.md), [GitHub/merge](references/github-and-merge.md),
[transcription](references/transcribe.md), or [updates](references/update.md).
