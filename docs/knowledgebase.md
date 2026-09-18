# Knowledgebase

Graphoxide turns a Git repository into a direct-source knowledgebase. It writes
a pointer-only source index, taxonomy, model-derived pages, and review records
that you can commit. It does not copy external source bodies into that tracked
state. Graphoxide reads a source transiently while an explicit command needs
it and verifies its digest. Derived Markdown contains information from the
source and is persisted; review that prose before committing or sharing it.

This is the direct-only workflow. Source operations are user-owned and manual:
run add or refresh deliberately, run an AI review deliberately, then make a
separate local confirmation when the reviewed result is ready.

Run every command from the knowledgebase Git root. In VS Code 0.16.0, the
Control Center and Command Palette expose this workflow, including source
generation and cancellation. The CLI setup below also works independently.

The pre-0.14 wiki commands and `wiki_*` MCP tools were removed without
automatic migration. Initialize a new direct-source knowledgebase and add its
sources explicitly. There is no `wiki research`, `wiki publish`, `wiki schema`,
or managed Hugo command group.

## Bootstrap once

Install Graphoxide and Git first. The examples assume `graphoxide` and `git`
are on `PATH`; model authoring additionally needs a running provider with the
model selected below. Hugo is needed only for the local preview step.

Create a knowledgebase repository separately from the external source files:

```bash
mkdir knowledgebase
cd knowledgebase
git init
mkdir -p config providers
```

Create `providers/local.json` with this secret-free provider profile. This
example uses a local Ollama server. Replace `replace-with-installed-model-id`
with the exact ID of an installed model that supports text generation and
structured JSON output, and start that server before adding or reviewing
sources. Initialization validates configuration without calling the model.

```json
{
  "version": 1,
  "id": "local",
  "protocol": "ollama-native",
  "endpoint": "http://127.0.0.1:11434",
  "source_egress_consent": "send-source-text-to-local-model",
  "models": [
    {
      "id": "local-text",
      "api_model": "replace-with-installed-model-id",
      "label": "Local author and reviewer",
      "capabilities": ["structured-output", "text-generation"]
    }
  ]
}
```

Create `config/authoring-input.json` to select the models used for authoring and
review. This example uses the same model for both passes; they remain separate
operations. `provider_profile` is relative to the knowledgebase root, model
values refer to the provider's `id` fields, and the consent strings must match
exactly.

```json
{
  "provider_profile": "providers/local.json",
  "author_model": "local-text",
  "reviewer_model": "local-text",
  "source_egress_consent": "send-source-text-to-local-model"
}
```

Provider profiles also support YAML. OpenAI-compatible and Anthropic Messages
profiles use `openai-compatible` or `anthropic-messages` respectively and require
a `credential_env` naming the environment variable that holds the API key.
Keep credentials out of the profile files. For those providers, configure your
own endpoint and exact API model IDs; the example above requires no API key.
Endpoints require HTTPS unless they point to loopback. The Wiki provider
configuration is separate from VS Code's community-labeling settings.

Initialize after both files exist:

```bash
graphoxide wiki init --authoring-profile config/authoring-input.json
```

`wiki init` creates `taxonomy/policy.json`, `sources/index.json`, and
`config/authoring-profile.json`, and excludes local runtime state through
`.gitignore`. Commit the provider profile, authoring input and generated
configuration, taxonomy, source index, and ignore rules. Initialization does
not commit them for you. Keep external source bodies outside this repository.

```bash
git add .gitignore config providers sources taxonomy
git commit -m "Initialize direct-source knowledgebase"
```

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
content. Git inputs must be clean, tracked files from a local worktree with a
`HEAD` commit and an `origin` remote using HTTPS or a standard SSH Git address.
Commit changes in the source repository before adding its files; untracked
files and files that differ from their committed contents are rejected. The
remote identifies the source; Graphoxide does not fetch it.

```bash
# One or more local files on any supported platform
graphoxide wiki source add ../source-docs/spec.md ../source-docs/protocol.md --allow-model-egress

# Directory inputs on Linux and macOS
graphoxide wiki source add ../source-docs/ ../protocol-contracts/ --allow-model-egress

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

Use the returned `source_id` in later commands; `src:SOURCE_ID` below is a
placeholder. The 16 MiB source ceiling is separate from the configured model's
prompt limit, so an admitted file can still be too large for authoring.

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

The default MCP service exposes graph and PR tools with read-only annotations;
PR tools can contact GitHub through `gh`. Knowledgebase lifecycle tools are
available only over stdio, with both `--wiki-root` and `--allow-wiki-write`.
They use that bound root rather than a per-call `project_path`.

The five lifecycle tools are:

| Tool | Operation |
| --- | --- |
| `knowledgebase_source_add` | Author sources selected by logical bound paths or HTTPS URLs |
| `knowledgebase_source_status` | Read source locator, digest, size, and status metadata |
| `knowledgebase_source_refresh` | Refresh a source and author changed content |
| `knowledgebase_source_review` | Run AI review (`mode: "ai"`) or record local human confirmation (`mode: "human-confirm"`) |
| `knowledgebase_source_retire` | Remove a source pointer and its derived artifacts |

Use `graphoxide wiki source confirm` locally for human confirmation after AI
review, or explicitly request the review tool's `human-confirm` mode with only
the source ID. There is no separate confirmation MCP tool. MCP inputs and outputs
use logical source metadata; they do not accept physical filesystem paths or
return raw source bodies. Use the CLI to establish local source bindings.

Network transfer and model egress are independent capabilities that are
disabled by default. Source add, refresh, and AI review require server model
authorization plus an explicit per-call model-egress acknowledgement. HTTPS
reads additionally require server network authorization and per-call consent.
Local human confirmation performs neither operation.

```bash
graphoxide serve graphoxide-out/graph.json --transport stdio \
  --wiki-root . --allow-wiki-write \
  --allow-wiki-network --allow-wiki-model-egress
```

Only enable `--allow-wiki-network` when remote source transfers are intended.
Only enable `--allow-wiki-model-egress` when explicitly requested authoring or
AI review may send transient source material to the configured model.

The lifecycle tools operate on an initialized knowledgebase and do not require
a graph file. Graph-query tools on the same server do require one: run
`graphoxide index /path/to/project` first and pass its graph path as the
positional argument to `serve`. The server can start before that file exists,
but graph queries will report the missing graph.

For CLI activity reporting, add `--progress=json` to `wiki` commands. Bounded
`[graphoxide-activity]` events appear on stderr and leave normal stdout results
unchanged. The default mode emits no activity events.
