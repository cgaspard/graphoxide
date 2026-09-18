# How Graphoxide works

Graphoxide's indexing pipeline is deterministic and offline. `index`, `extract`,
`update`, and `watch` use local parsers and do not invoke a model provider, even
when the input includes documents or media.

Discovery applies ignore rules, skips sensitive and generated paths, and
classifies admitted files. Bounded workers parse supported code, structured
data, documents, and archive members. Unsupported content remains visible as
inventory or a diagnostic rather than being treated as extracted text.
`graphoxide formats --json` reports the supported families and parser limits.

Graph construction resolves cross-file relationships, merges and deduplicates
facts, and applies deterministic clustering. The resulting graph retains source
locations for queries, reports, exports, and MCP clients. `graphoxide index`
also publishes a file coverage report tied to the accepted graph's digest;
incremental manifests and extraction caches support later updates.

Model operations are separate, explicit workflows:

- `label` sends bounded graph labels to a configured provider to name communities.
- `enrich` can summarize user-supplied media transcripts through the
  `media-transcript-summary-v1` profile. It does not transcribe or upload media.
- `wiki` authors derived Markdown from explicitly added UTF-8 sources, with
  separate AI review and local human confirmation. Source text can be sent to
  the configured model only with explicit model-egress consent.

In 0.16.0, `label` and `wiki` accept `--progress=json` for bounded activity
events on stderr. Graph build progress uses `[graphoxide-progress]`; model and
Wiki activity uses `[graphoxide-activity]`. These streams carry fixed phases,
run nonces, and aggregate counts separately from command results. Progress
does not alter deterministic graph bytes.

See the [main workflow guide](../README.md#typical-workflow) and
[knowledgebase setup](knowledgebase.md) for commands and data boundaries.
