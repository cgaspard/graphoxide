# Knowledgebase

Graphoxide turns a Git repository into a direct-source knowledgebase. The
repository contains a pointer-only source index, taxonomy, derived pages, and
review records. It does not contain source bodies. Graphoxide reads a source
transiently only while an explicit command needs it, verifies the observed
digest, and then discards the bytes.

This is the direct-only workflow. Source operations are user-owned and manual:
run add or refresh deliberately, run an AI review deliberately, then make a
separate local confirmation when the reviewed result is ready.

Run every command from the knowledgebase Git root.

## Bootstrap once

Create a secret-free authoring profile in the repository. It names the
provider profile, author model, reviewer model, and explicit source-egress
consent; credentials stay in the provider's configured environment variable.

```bash
git init
graphoxide wiki init --authoring-profile config/authoring-input.json
```

`wiki init` creates the direct taxonomy and a source index. Commit those files
and the generated authoring configuration. Do not commit a credential or a
source body.

## Add sources

Use one command for files and directories. Inputs can be local paths, a local
Git worktree, or HTTPS URLs. A Git source records its remote, pinned commit,
and repository-relative path. A non-Git local source records a stable logical
binding and relative path; the physical root remains in ignored local
configuration.

On Windows, add individual files; directory imports are unavailable. Linux
and macOS also support directory inputs.

Authoring and review require UTF-8 text, up to 16 MiB per source. Binary
documents are rejected before a model request; convert them to a text file
before adding them. This workflow does not extract PDF or Office document
content. Git inputs must be clean, tracked files from a local worktree.

```bash
# One or more local files on any supported platform
graphoxide wiki source add docs/spec.md protocol.md --allow-model-egress

# Directory inputs on Linux and macOS
graphoxide wiki source add docs/ protocol-contracts/ --allow-model-egress

# An HTTPS source also requires one explicit transfer acknowledgement
graphoxide wiki source add --allow-network --allow-model-egress \
  https://docs.example.test/spec.md
```

Add publishes a clearly marked provisional derived page immediately. It also
records an explicit subject, facets, applicability, and stable source digest.
There is no default route and no catch-all category. If an add cannot finish,
Graphoxide rolls back source-index pointers created by that request. If index
publication itself fails, an ignored local binding may remain as an inert,
retry-safe mapping; it contains no indexed source or source body.

## Keep sources current

Every source stays pinned to its recorded digest. Refresh is the only command
that advances that pin. A changed source returns to provisional state and is
processed again; an unavailable source remains visible with a stale or error
status.

```bash
graphoxide wiki source status --json
graphoxide wiki source refresh src:SOURCE_ID --allow-model-egress
graphoxide wiki source refresh src:SOURCE_ID --allow-network --allow-model-egress
graphoxide wiki source retire src:SOURCE_ID
```

Use `--allow-network` when refreshing an HTTPS source.
`--allow-model-egress` acknowledges the transient authoring pass. Retiring
removes the pointer and Graphoxide-owned derived artifacts, never the external
source.

Git pointers are read from their pinned revision in a local binding. Network
Git fetching is disabled until transfer, storage, and process limits can be
enforced; `--allow-network` does not enable it. Keep the local Git binding
available, or add an HTTPS text source instead. HTTPS transfer has a 16 MiB
limit, a request deadline, and no redirects.

A changed refresh replaces the pointer, assignment, page, and review state
together. If authoring or publication fails, the last coherent revision is
preserved so the refresh can be retried.

Source commands on the same knowledgebase run one at a time. A concurrent
command reports that the knowledgebase is busy; retry after the active command
finishes.

## Review explicitly

Adding content never implies review. Invoke review for each source that should
receive the configured AI quality pass and recorded decision:

```bash
graphoxide wiki source review src:SOURCE_ID --allow-model-egress
graphoxide wiki source review src:SOURCE_ID --allow-network --allow-model-egress
graphoxide wiki source confirm src:SOURCE_ID
```

The review is an AI-only quality pass and binds the source digest, taxonomy
policy, that source's assignment, and derived page digest. Changes to other
sources preserve its approval. `confirm` is a separate local
human action after AI review and uses the same current binding; no reviewer
identity, source body, prompt, or model response is stored.

## Browse locally

`wiki live` serves the generated direct knowledgebase on one loopback port.
It exposes the taxonomy, provisional status, reviewed status, stale/error
state, and source navigation without exposing raw inputs.

```bash
export GRAPHOXIDE_HUGO_BINARY=/absolute/path/to/hugo
graphoxide wiki live . --port 1313 --open
```

Install Hugo **0.165.0** separately and set `GRAPHOXIDE_HUGO_BINARY` to that
executable. Graphoxide checks the configured version; it does not download or
manage Hugo automatically.

## MCP service capabilities

The MCP server is read-only unless a knowledgebase root and write permission
are both supplied. Network transfer and AI model egress are independent,
default-deny capabilities. An AI review also requires a per-request
acknowledgement; a local human confirmation carries neither capability.

```bash
graphoxide serve graphoxide-out/graph.json --transport stdio \
  --wiki-root . --allow-wiki-write \
  --allow-wiki-network --allow-wiki-model-egress
```

Only enable `--allow-wiki-network` when remote source transfers are intended.
Only enable `--allow-wiki-model-egress` when an explicitly requested AI review
may send transient source material to the configured reviewer.
