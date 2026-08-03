---
name: graphoxide
description: Query Graphoxide from OpenClaw.
---

# Graphoxide for OpenClaw

Run `graphoxide query "<question>"` before broad source searches. Use
graphoxide-out/GRAPH_REPORT.md only for broad architecture review.

**Step B2 — dispatch**

Use the Agent tool with `subagent_type="general-purpose"` and give each worker one graph chunk.

**Step B3 — consolidate**

Combine the cited source evidence and run `graphoxide query "<follow-up>"` again.

Open only the workflow you need: [extraction](references/extraction-spec.md),
[queries](references/query.md), [exports](references/exports.md), [watch mode](references/add-watch.md),
[hooks](references/hooks.md), [GitHub/merge](references/github-and-merge.md),
[transcription](references/transcribe.md), or [updates](references/update.md).
