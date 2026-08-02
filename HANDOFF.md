# HANDOFF — Graphoxide Rust port

**Goal:** a faster, single-binary Rust implementation derived from
[Graphify](https://github.com/Graphify-Labs/graphify) that requires **no Python runtime**,
uses the same graph schema in its renamed `graphoxide-out/` directory, and covers the
offline core first (extract → build → cluster → analyze → report/export → query → MCP).

The Python original is cloned at `./upstream/` (gitignored) — treat it as the reference
implementation and read it side-by-side while porting. It is ~55k lines; this document
distills the parts that matter so you don't have to re-derive them.

---

## 1. Current state (what's already done)

> **Port completion update (2026-08-01):** The offline Rust conversion described by
> this handoff is implemented end to end. The workspace now contains production
> implementations for schema/I/O, queries, parallel extraction and caches, corpus
> resolution, build/dedup/Leiden/analyze, reports and exports, MCP stdio, watch,
> hooks/Claude integration, global/merge workflows, benchmarks, structured fallback
> extraction for the remaining language matrix, and optional HTTP community labeling.
> See `README.md` for the current command surface and `BENCHMARKS.md` for measured
> differential and performance results. The bullets immediately below describe the
> original scaffold at handoff time and are retained as historical context.

- Cargo workspace scaffolded and **compiling** (`cargo build` passes, Rust 1.95).
- `graphoxide-cli` binary parses the core subcommand surface; `graphoxide extract <path>`
  already walks files gitignore-aware via the `ignore` crate (216 files on the upstream
  Graphify checkout).
- 12 tier-1 tree-sitter grammars compile and link (python, js, ts/tsx, go, rust, java,
  c, cpp, ruby, c-sharp, bash, json) against `tree-sitter = "0.25"`.
- Everything else is `todo!()` stubs with pointers into this document.
- git repo initialized on `main`; **nothing committed yet**.

```
crates/
  graphoxide-core/      schema types, ids, sanitize, validation      (ports ids.py, validate.py, security.py)
  graphoxide-extract/   detect + tree-sitter engine + resolution     (ports detect.py, extract.py, extractors/*, cache.py, manifest)
  graphoxide-graph/     build/merge, dedup, cluster, analyze         (ports build.py, dedup.py, _minhash.py, cluster.py, analyze.py)
  graphoxide-query/     query/path/explain/affected/god-nodes        (ports the cli.py query commands + serve.py scoring/BFS + affected.py, benchmark.py)
  graphoxide-export/    report, obsidian vault, html viewers         (ports report.py, export.py, exporters/html.py, tree_html.py, callflow_html.py)
  graphoxide-mcp/       MCP stdio server                             (ports serve.py)
  graphoxide-cli/       the `graphoxide` binary                        (ports __main__.py / cli.py dispatch)
```

## 2. Scope decisions

**In scope (the Python-free core — everything upstream can do with no API key):**
extract `--code-only`, update, cluster-only, query, path, explain, affected, god-nodes,
report/export (json/obsidian/html/graphml/cypher), tree, benchmark, watch, check-update,
merge-graphs, global graph, MCP server, git hooks, hook-guard.

**Later (still Python-free, just not first):** LLM features (semantic extraction of
docs/PDFs/images, community labeling, dedup tiebreak) — upstream talks to
OpenAI/Anthropic-compatible HTTP APIs; in Rust this is plain `reqwest`, no SDK needed.
Community labeling already degrades gracefully upstream (hub-based names, "Community N"
placeholders) so the port is fully usable without it.

**Out of scope initially:** video transcription (faster-whisper), office/Google-Workspace
conversion, PostgreSQL introspection, neo4j/falkordb live export, the ~20 platform
installers (keep `claude install` + `hook install` only at first), SVG export
(matplotlib upstream — either skip or write a small deterministic spring layout later).

**Where the speed comes from:**
1. Extraction: upstream uses a `ProcessPoolExecutor` (only when ≥20 uncached files) to
   dodge the GIL, paying subprocess spawn + JSON pickle per file. Rust: rayon in-process,
   zero serialization. This is the single biggest win.
2. Query startup: `graphoxide query` runs on *every agent question*; interpreter + import
   time dominates small queries. A Rust binary is ~5 ms to first instruction.
3. Trigram/IDF indexes are rebuilt per process upstream (cached only inside the MCP
   server); Rust can build them 10-50x faster and/or persist them.

## 3. graph.json schema (byte-compat contract)

Authority: upstream `export.to_json` (`export.py:232`) — NetworkX node-link format.
The `worked/*/graph.json` files in upstream are **stale-format**; don't use as reference.

- Top-level keys: `directed` (bool), `multigraph` (always false), `graph` (attrs object),
  `nodes`, **`links`** (not `edges` — readers must accept both), `hyperedges` (always
  written, `[]` when none), optional `built_at_commit` (git HEAD). **No version field
  anywhere** — compatibility is tolerant-reader-based.
- The raw `--no-cluster` writer (`cli.py:3481`) instead dumps
  `{nodes, edges, hyperedges, input_tokens, output_tokens}` — key is `edges` there.
- **Node** required: `id`, `label`, `file_type` ∈ {code, document, paper, image,
  rationale, concept}, `source_file` (repo-relative, forward slashes). Optional:
  `source_location` ("L42"), `community` (int), `community_name`, `norm_label`
  (diacritic-stripped lowercase, added at export), `_origin` ∈ {ast, semantic},
  `repo`, `local_id`.
- **Edge** required: `source`, `target`, `relation`, `confidence` ∈
  {EXTRACTED, INFERRED, AMBIGUOUS}. `confidence_score` backfilled from
  {1.0, 0.5, 0.2}. `weight` default 1.0. Transient keys `target_file`/`local_alias`
  are stripped at build.
- **`_src`/`_tgt` is load-bearing:** storage graph is *simple* and usually undirected;
  true direction is stamped per-edge as `_src`/`_tgt` in memory, popped and restored
  onto `source`/`target` at write time (undirected NetworkX canonicalizes endpoint
  order, #563). Current-format files on disk have no `_src`/`_tgt`.
- Hyperedges: `{id, label, nodes: [...], relation ∈ participate_in|implement|form,
  confidence, confidence_score, source_file}`; member-key aliases `members`/`node_ids`
  folded on read.
- Write safety: atomic write (tmp + rename); refuse to overwrite when new graph has
  fewer nodes than existing (unless `--force`/pruning); **fail closed** on unparseable
  existing file. 512 MiB read cap (`GRAPHOXIDE_MAX_GRAPH_BYTES` override).

## 4. IDs and determinism (read this before writing any code)

`ids.py` is 50 lines and the single source of truth:
- `normalize_id(s)`: NFKC → replace `[^\w]+` (unicode word class — CJK survives) with
  `_` → collapse `_+` → strip edge `_` → `casefold()`. Idempotent.
- `make_id(*parts)`: strip `_`/`.` from part edges, join `_`, normalize.
- Conventional node id shape: `<slugified ext-stripped repo-relative path>_<entity>`,
  e.g. `docs_v1_api_readme_parse`.

Upstream has explicitly designed out Python hash-order nondeterminism (#1753, #1090,
#1851). The port must preserve the *explicit sorts*, not rely on HashMap order — use
`BTreeMap`/sorted iteration at: build's ghost-merge and edge loop (sorted
`(source,target,relation)`), cluster's graph rebuild, community re-index (total order:
`(-size, sorted member tuple)`), dedup's collision rank. Random seeds: cluster seed 42,
MinHash coefficients from `numpy RandomState(1)` (see §7).

## 5. Extraction engine (crates/graphoxide-extract)

### detect (from detect.py, 1932 lines)
- Extension allow-lists only — **there is no binary sniffing**. `CODE_EXTENSIONS` is 94
  suffixes; DOC/PAPER/IMAGE/OFFICE/VIDEO sets exist but only CODE hits the AST path.
- Shebang sniffing for extensionless files (`env`-aware, 256-byte head).
- Ignore handling is full gitignore semantics, last-match-wins: per-dir `.gitignore` +
  `.graphoxideignore` (graphoxideignore wins), ancestor walk to VCS root, `$GIT_DIR/info/exclude`,
  nested ignores during walk, `--exclude` appended last. The `ignore` crate covers ~all
  of this; wire `.graphoxideignore` via `add_custom_ignore_filename` (already done) and
  verify precedence matches.
- Noise pruning independent of ignores: `_SKIP_DIRS`, lockfiles, evidence-gated dirs
  (`env` needs venv markers, `coverage` needs artifacts, `snapshots` needs `.snap`).
- Sensitive-file screen (3-stage: credential dirs → filename patterns → keywords).
- No size cap on source files (only office/PDF zip-bomb caps + 2 MiB XML caps).

### extraction (from extract.py 5805 + extractors/ 16k lines)
- **No tree-sitter queries anywhere** — upstream is 100% manual AST walks driven by
  per-language `LanguageConfig` (node-type sets + field names + 3 hooks). Port
  `LanguageConfig` as a struct and `_extract_generic` (`extractors/engine.py:2282`)
  as the shared driver; 15 languages are config-driven, the rest are bespoke walkers.
- Extractor contract: `fn(path) -> {nodes, edges, raw_calls?, <type tables>?}`.
  Missing grammar returns an empty extraction with an `error` note — never panics.
- Per-file results feed a **large shared second pass** (extract.py:4778-5723, order
  matters): symbol-resolution facts (JS/Python), decl/def header merge (C/C++/ObjC),
  id remaps, barrel re-export repointing, per-language import resolution
  (Python/Java/C#/Bash), then the shared call-resolution pass, then 9 registered
  member-call resolvers (swift, python, ruby, ts, cpp, objc, csharp, java, pascal),
  then relativization + `_origin: "ast"` stamping.
- Shared call pass rules worth preserving exactly: candidate by exact label (case-fold
  only for case-insensitive langs); disambiguate via unique symbol-import → unique
  module-import → path proximity; cross-language family guard; JS/TS calls with no
  import evidence are **dropped**; `EXTRACTED`/1.0 with import evidence else
  `INFERRED`/0.8; `indirect_call` always INFERRED.
- Parallelism: replace the subprocess pool with rayon `par_iter` over files. Upstream's
  `_PARALLEL_THRESHOLD = 20` and worker caps become irrelevant.

### Language matrix (port order)
| Tier | Languages | How |
|---|---|---|
| 1 (wired) | python, js/jsx, ts/tsx, go, rust, java, c, cpp, ruby, c#, bash, json | crates.io grammar crates (already in Cargo.toml) |
| 2 | kotlin, scala, php, swift, lua, groovy, elixir, zig, julia, fortran, verilog, objc, powershell, hcl/terraform, sql | crates.io where available; else vendor grammar C via `cc` build script or git dep |
| 3 (regex ports, no grammar) | apex, dart, markdown links, blade, razor, .sln, .dfm/.lfm, pascal fallback | port the Python regexes to the `regex` crate |
| XML | .slnx, .csproj/.fsproj/.vbproj, .xaml (incl. MVVM inference), .lpk | `quick-xml`, keep the 2 MiB cap |
| Container | .vue, .svelte, .astro | mask non-script regions, reuse JS/TS grammar |
| Manifests | pyproject.toml, go.mod, pom.xml, apm.yml | `toml`, hand parser, `quick-xml`, `serde_yaml`; one `pkg_<name>` node + `depends_on` edges, no stub nodes for externals |
| Special routing | `.h` ObjC/C++/C content sniff; `.m` ObjC-vs-MATLAB guard; MCP config files | port the marker lists |

### Incremental + caches (from cache.py, detect.py:1603-1932)
Two independent tiers — keep them independent:
- **Manifest gate** (`graphoxide-out/manifest.json`): decides *what to dispatch*.
  Entry `{mtime, ast_hash, semantic_hash}`; mtime `!=` (not `>`) is the fast path,
  MD5 confirms; missing hash ⇒ changed; NFC-normalize paths (macOS NFD, #2221);
  deleted vs excluded rows split. Two hash columns because AST and semantic tiers
  re-extract independently.
- **AST cache** (`cache/ast/v<version>/<sha256>.json`): decides *whether dispatched work
  is already done*. Key = sha256(content + `\0` + lowercased relative path salt);
  version-namespaced, stale versions swept. **JS/TS/Vue/Svelte are never AST-cached**
  (their extraction depends on sibling files/tsconfig). Never cache zero-node or
  errored results. Portable via `$graphoxide-root$` sentinel rewriting.
- Word-count stat index (`cache/stat-index.json`): `(size, mtime_ns)`-validated.
- Semantic cache: only needed with LLM features; prompt-fingerprint-namespaced,
  never version-swept, `partial: true` = miss.

## 6. Build / merge / dedup / cluster / analyze (crates/graphoxide-graph)

### build (build.py:612-1094) — port in this exact order
1. Alias folds (`links`→`edges`, `name`→`label`, `type`→`relation`, file_type synonyms,
   numeric-id coercion), validation with dangling-edge downgrade to warnings.
2. Semantic-id remap: non-AST node ids re-derived from their `source_file` in code
   (never trust LLM id strings); doc-twin merge.
3. Nodes: last-writer-wins on attributes (AST first, semantic overwrites).
4. Ghost merge keyed `(normalized source_file, label)`: AST nodes canonical, two AST
   nodes on one key = ambiguous = no merge; sorted two-pass.
5. Edge loop in sorted `(source,target,relation)` order: endpoint repair via normalized
   id index (+ legacy-stem alias index, committed only when unambiguous); silently drop
   still-dangling; cross-language phantom guard (`calls` INFERRED dropped cross-family;
   `imports`/`references` dropped only when both families known and differ); drop
   self-loops for import relations, keep recursive `calls`; stash `_src`/`_tgt`;
   dedup = same `(u,v)` overwrites + first-wins reverse-direction guard.
6. Hyperedges: validate members against built nodes, drop empties; disambiguate
   colliding file-node labels (labels only).
- Incremental: **tier-scoped replace** — a re-extracted file drops its prior
  contribution only for the tier (AST vs semantic) present in the new chunks;
  replace beats a contradictory delete; shrink guard unless dedup/pruning active;
  `directed=None` inherits on-disk flag.

### dedup (dedup.py, runs only inside `build()`)
- Pass 0: exact-id collisions — survivor by total-order rank (defines-own-id, label
  len, label, source_file); gap-fill from same-source losers only.
- Pass 1: exact normalized label — **code nodes never label-deduped**; same-file merges
  unconditional; cross-file only for concepts with entropy ≥ 2.5.
- Pass 2: MinHash LSH (threshold 0.7, 128 perms, 3-gram shingles) → Jaro (cross-file
  ≥12 chars) / JaroWinkler (else), merge ≥ 92.0, +5 same-community boost, with variant/
  numeric/prefix-extension/short-label blockers. `rapidfuzz` crate has all three metrics.
- The MinHash coefficients come from `numpy RandomState(1)` (MT19937). For identical
  merge results you'd need numpy's exact generation sequence; recommended: implement
  MT19937 (or use a port crate), verify coefficients against Python once, snapshot them
  as constants in the source. Otherwise dedup is deterministic-but-divergent — decide
  and document.

### cluster (cluster.py)
- Deterministic rebuild (sorted nodes/edges) → **Leiden, seed 42, trials 1** via
  graspologic. Punchline: graspologic's Leiden **is already Rust** —
  [`graspologic-native`](https://github.com/graspologic-org/graspologic-native), crate
  `network_partitions`. It is NOT on crates.io → use a git dependency (verify license,
  MIT) or vendor. This gives partition parity with upstream for free.
- Fallback upstream is NetworkX Louvain seed 42 — skip porting it; always use Leiden.
- Post passes: hub exclusion (degree percentile, majority-vote reattach), oversize
  split (> max(10, 0.25·|V|)), cohesion split (≥50 nodes & cohesion < 0.05), total-order
  re-index (cid 0 = largest).
- Hub-based labeler (no LLM): highest-degree member, ties by node id string.
- `remap_communities_to_previous`: greedy intersection matching — needed for stable
  incremental rebuilds; membership sha256 sigs detect stale LLM labels.

### analyze (analyze.py)
- god_nodes: plain degree rank, filtered (file/concept/json-key nodes, builtin noise
  labels). surprises: additive integer heuristic (confidence bonus 3/2/1, cross-category
  +2, cross-topdir +2, cross-community +1, ×1.5 semantic-similar, peripheral→hub +1,
  suppressions for cross-language INFERRED); single-source fallback to community-bridge
  edges; betweenness only < 5000 nodes. questions: 5 generators in fixed order.
  Straightforward petgraph ports; betweenness sampled k=min(100,|V|) seed 42 above 1000
  nodes — match or document divergence.

## 7. Query engine (crates/graphoxide-query) — hottest path

From serve.py (shared by CLI `query` and MCP `query_graph`):
- Terms: whitespace → `\w+` lowercase; CJK via jieba (use `jieba-rs`, feature-gate) else
  bigrams; drop pure-ASCII tokens ≤ 2 chars; large multilingual stopword list (copy it
  verbatim from serve.py:215-244); fall back to unfiltered if all stopwords.
- IDF: `ln(1 + N/(1+df))`, df = nodes whose norm_label contains term.
- Trigram inverted index over `norm_label \0 label_tokens \0 id \0 source \0 source_tokens`;
  candidate bail-out when needle < 3 chars or rarest trigram > 10% of nodes → full scan.
- Scoring constants (copy exactly): EXACT=1000, PREFIX=100, SUBSTRING=1, SOURCE=0.5;
  joined-query tier ×10 × max-constituent-IDF; strongest tier only per term; final
  `score += tiered * (matched/n_terms)²`; sort `(-score, label_len, id)`.
- Seeds: take while `score ≥ top·0.2`, max 3, label-deduped, plus best singleton per
  token guaranteed.
- Traversal: BFS level-sync depth 2 (CLI) / 3 (MCP, cap 6); hub threshold
  `max(50, degree at 99th percentile)` — hubs not expanded unless seeds; DFS variant;
  induced-edge completion pass afterwards.
- Output: `char_budget = token_budget × 3`; NODE/EDGE line formats (copy from
  serve.py:930-1056 exactly — agents parse these); truncation banners outside budget;
  every field through `sanitize_label` (control-char strip, 256 cap).
- `path`: directed load, endpoint scoring + token-superset preference, ambiguity warning
  at 10%, deterministic rebuild before shortest path. `explain`: find_node tier order
  source_exact → exact → prefix → substring, ambiguity across files = error. `affected`:
  reverse BFS over in-edges filtered to a fixed relation list (affected.py:12-26).
- Query logging (querylog.py) + query stamp (used by hook-guard TTL) — small, port them.

## 8. MCP server (crates/graphoxide-mcp)

Use `rmcp` (crates.io v3, official SDK), stdio transport first. 10 tools:
`query_graph`, `get_node`, `get_neighbors`, `get_community`, `god_nodes`, `graph_stats`,
`shortest_path`, `list_prs`, `get_pr_impact`, `triage_prs` (prs tools shell out to `gh` —
keep that). 6 resources: report/stats/god-nodes/surprises/audit/questions. Every tool
gets an injected optional `project_path` param → per-project graph context; LRU cache of
8 contexts keyed `(mtime_ns, size)` for hot reload; warm the trigram index at load; all
tool errors return text, never raise. Output is plain text (never JSON) — byte-match the
formats. HTTP transport (axum) + API-key middleware later.

## 9. Watch, hooks, CLI shell

- watch: `notify` crate (native FSEvents on macOS — upstream had to force polling there;
  free win). Debounce 3 s; code change → full/incremental rebuild, non-code → write
  `graphoxide-out/needs_update` flag; rebuild lock = flock with PID + pending-changes
  queue file (port as-is); persisted build config `.graphoxide_build.json`.
- git hooks (hooks.py): post-commit (changed-paths incremental rebuild, detached,
  guards for rebase/merge/worktrees), post-checkout (branch switches), merge driver for
  graph.json union-merge. The hook scripts get *simpler* in the port: they just call the
  binary — no Python interpreter detection needed.
- hook-guard (PreToolUse nudges for Claude Code etc.): pure stdin/stdout JSON, fails
  open, session-dedup marker files, query-stamp TTL. Small and worth early porting —
  it's what makes agents actually use the graph.
- CLI: match upstream's exit codes and stdout formats where agents parse them (query,
  path, explain, god-nodes `--json`). Broken-pipe = success (upstream #1807) — in Rust,
  handle `EPIPE` on stdout writes.

## 10. Porting phases

| Phase | Deliverable | Definition of done |
|---|---|---|
| 0 ✅ | Workspace scaffold | `cargo build` green, CLI parses, file walk works |
| 1 ✅ | `graphoxide-core`: schema + tolerant reader/writer + ids + sanitize | Round-trip a Python-built graph.json byte-stable (key order aside); `normalize_id` property tests vs Python vectors |
| 2 ✅ | Query engine on existing graphs | `query/path/explain/affected/god-nodes` output byte-identical to Python on a fixture graph |
| 3 ✅ | Extraction tier 1 (python, js/ts, go, rust, java) + detect + second pass | Node/edge sets match Python `extract --code-only` on fixture corpora (see §11) |
| 4 ✅ | build + dedup + cluster (network_partitions) + analyze + report/export | `graphoxide extract --code-only` end-to-end parity on upstream's own repo |
| 5 ✅ | MCP server + watch + hooks + incremental manifest | MCP protocol compatibility with upstream Graphify; hook-driven rebuilds |
| 6 ✅ | Language tiers 2-3, global graph, merge-graphs, benchmark, LLM labeling via reqwest | feature parity for the offline surface + `BENCHMARKS.md` numbers vs Python |

## 11. Testing strategy: differential against upstream

The Python reference is right there in `upstream/` — use it as the oracle:
1. `cd upstream && uv sync` (uv.lock is committed) to get the runnable upstream Python Graphify.
2. Build fixture corpora (upstream `tests/fixtures/` has per-language ones; upstream's
   own `graphoxide/` package is the big realistic corpus).
3. Golden tests: run Python `extract --code-only` → snapshot `graph.json` → assert the
   Rust pipeline produces the same node set, edge set, and (phase 4+) partition.
   Compare as canonicalized JSON (sorted keys, sorted node/edge lists), not raw bytes.
4. Port upstream's unit tests selectively — `tests/` has 180+ files; the highest-value
   ones are `test_build_merge_hyperedges_and_prune.py`, `test_dedup.py`,
   `test_symbol_resolution.py`, `test_languages.py`, `test_query_cli.py`,
   `test_hypergraph.py`, `test_atomic_writes.py`.
5. Determinism tests: run every pipeline stage twice, assert identical output.

## 12. Crate choices (verified 2026-08-01)

| Need | Crate | Note |
|---|---|---|
| Graph structure | petgraph 0.8 | storage model is our own structs; petgraph for algorithms |
| Parallelism | rayon | replaces ProcessPoolExecutor |
| File walk + ignores | ignore 0.4 | gitignore semantics + custom ignore file |
| Parsing | tree-sitter 0.25 + grammar crates | 12 wired; rest per language matrix |
| Fuzzy match (dedup only) | rapidfuzz 0.5 | Jaro/JaroWinkler/DamerauLevenshtein ✔ |
| Leiden | `network_partitions` via git: graspologic-org/graspologic-native | NOT on crates.io; same code Python uses |
| MCP | rmcp 3.x | official SDK; stdio then HTTP |
| Watch | notify + notify-debouncer-full | native FSEvents |
| CJK segmentation | jieba-rs 0.10 | feature-gate `chinese` |
| CLI | clap 4 derive | |
| Serialization | serde/serde_json | tolerant readers via `#[serde(flatten)]` + custom Deserialize |
| HTTP (LLM later, ingest) | reqwest (or ureq for CLI-only) | OpenAI/Anthropic-compatible endpoints |
| XML | quick-xml | csproj/xaml/slnx/pom |
| Hashing | sha2, md-5 | manifest=MD5, cache=salted SHA-256 — keep both |
| MT19937 (minhash coeffs) | `mt19937` crate or snapshot constants | see §6 dedup |

## 13. Gotchas (each one cost upstream a numbered issue)

- `links` vs `edges`; `_src`/`_tgt`; no schema version — tolerant readers everywhere.
- `normalize_id` must use the **unicode** word class (CJK ids, #811) and be idempotent.
- mtime compare is `!=` not `>` (git checkout of older commits, #1859).
- NFC-normalize all manifest paths (macOS NFD walk, #2221).
- Never AST-cache JS/TS/Vue/Svelte; never cache zero-node/errored results.
- `.m` is ObjC only after content sniff (MATLAB, #1702); `.h` sniffs ObjC → C++ → C.
- JaroWinkler's prefix bonus causes destructive cross-file merges — plain Jaro for
  cross-file labels ≥ 12 chars (#1243). Code nodes never label-dedup (#1205).
- Community re-index must be a total order or identical partitions get different cids (#1090).
- Shrink guard: refuse to overwrite a bigger graph unless pruning/dedup/--force; fail
  closed on unparseable existing graph.
- vis-network HTML loads from unpkg CDN with SRI hash — consider embedding the lib in
  the binary instead (it's ~700 KB; a real improvement over upstream).
- Broken pipe on stdout = success, exit 0 (#1807).
- BFS hub threshold and the 0.2 seed gap ratio are the difference between useful and
  garbage query output — copy the constants, don't tune them during the port.
- Windows: atomic rename needs a copy fallback; keep paths forward-slashed in all
  serialized output.

## 14. Immediate next steps

1. Phase 1: implement the tolerant graph.json reader/writer in `graphoxide-core` and
   round-trip a real Python-built graph (`cd upstream && uv sync && uv run graphify
   extract --code-only .` produces one).
2. Port `ids.rs` + generate test vectors from Python (`python -c "from graphify.ids
   import normalize_id; ..."` across a unicode corpus).
3. Phase 2 query engine against that fixture graph — it delivers user-visible value
   immediately (fast queries on graphs built by the Python tool) and locks the output
   formats before extraction work starts.
