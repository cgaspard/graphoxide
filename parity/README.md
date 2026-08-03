# Upstream test-suite parity

This directory is the coverage-control plane for porting the complete Graphify
test suite. It records every test declared by the pinned reference release,
then requires each parity claim to name a test that Graphoxide can actually
discover and run.

The reference is Graphify `v0.9.32`, commit
`00efd6e7969837ae4a9f11d8d504dcd3b20b09df`. Live pytest collection plus the
AST fallback for four optional-dependency modules declares exactly **3,978
cases in 176 test modules**. The canonical ordered
node-ID stream has SHA-256
`f4abbb1c5f690e04d5a8bdaef643b372eb56ca7416fa7260c39b1bb52f69e975`.

Passing the inventory check alone does not claim behavioral parity. The ledger
currently records **3,975 executable parity mappings**, **3 reviewed expected
divergences**, and **0 unaccounted cases**. Every overlay points to an exact
executable Rust, VS Code, or differential test ID. Expected divergences are
visible release debt and are never counted as mapped parity.

## Files

- `upstream.lock.json` pins the repository, tag, full commit, collection
  command, counts, and node-ID digest.
- `manifest.json` contains all 3,978 node IDs and classifies each by source file
  and Python test module. Every base row is explicitly unmapped.
- `mappings/*.json` contains independent executable parity claims. Separate
  files let multiple porting batches proceed without editing the inventory.
- `divergences/*.json` records executable evidence and a reviewed reason for an
  intentional behavioral difference. A case cannot be present in both ledgers.
- `source-maps/*.json` retains the early, detailed migration provenance for the
  three original language-matrix batches. It is reference material, not a
  second mapping registry.
- `verify.py` validates the pin, inventory, mappings, expected divergences,
  executable target IDs, and optional live upstream collection.
- `tools/import_inventory.py` reproducibly rebuilds the manifest, but only from
  the exact pinned commit and only when every locked count/hash matches.
- `tests/` tests the verifier's failure gates.

## Routine commands

Validate the checked-in inventory and every mapped or divergent executable
test ID:

```bash
python3 parity/verify.py check
```

Show mapped/divergent/unaccounted coverage for all 176 upstream modules:

```bash
python3 parity/verify.py report
python3 parity/verify.py report --json
```

Run the verifier's own tests:

```bash
python3 -m unittest discover -s parity/tests -v
```

Require every upstream case to have either a parity mapping or a reviewed,
executable expected divergence:

```bash
python3 parity/verify.py check --require-complete
```

Rust target discovery lists tests without executing them. To execute every
distinct mapped or divergent Rust target as well, use:

```bash
python3 parity/verify.py check --execute-rust-targets
```

VS Code and differential targets are executed during target validation because
their runners do not provide an equivalent stable list-only protocol.

## End-to-end graph differential

Build both pinned Graphify and the current Graphoxide binary against the same
corpus, canonicalize their graph artifacts, and report missing, extra, or
field-mismatched nodes, directed edges, and hyperedges:

```bash
python3 -m parity.differential.graph_diff run \
  --corpus /absolute/path/to/corpus \
  --build \
  --output /tmp/graph-parity.json
```

The default `structure` profile ignores presentation-only clustering fields but
preserves graph identity, source/type metadata, edge direction and multiplicity,
confidence, call-site data, and hyperedges. Use `--profile strict` to compare all
non-volatile serialized fields. To inspect already-built artifacts without
running either implementation:

```bash
python3 -m parity.differential.graph_diff compare \
  upstream-graph.json graphoxide-graph.json \
  --corpus /absolute/path/to/corpus
```

The command exits 0 only for parity, 1 for a graph difference, and 2 when a
build or artifact fails. Pass `--work-dir` to retain both generated graphs for
forensics; each invocation creates a fresh `run-*` child so a prior artifact
can never satisfy a later run. The default run uses a temporary directory
outside the corpus. The reference checkout must remain at the exact pin with a
clean tracked/non-ignored worktree immediately before and after Graphify runs;
ignored environments are allowed, but modified reference inputs are rejected.

For the extension-aware release gate, require every normalized Graphify fact
to survive while keeping Graphoxide-only additions visible for audit:

```bash
python3 -m parity.differential.graph_diff run \
  --corpus /absolute/path/to/corpus \
  --build \
  --contract reference-preserving \
  --fail-on-candidate-identity-hubs \
  --output /tmp/graph-parity.json
```

The report always retains `equal` as the exact normalized verdict. The selected
contract's exit verdict is recorded under `gate`, and the asymmetric coverage
details live under `parity.reference_preservation`. Candidate-only records do
not satisfy missing reference multiplicity and are never substituted for
changed reference facts. The sole field refinement currently recognized by
this contract is upstream `context: call` becoming
`context: import_guided_call`; exact parity continues to report that change.

Run every checked-in adversarial corpus and enforce its named contract with:

```bash
python3 -m parity.differential.corpus_suite \
  --build \
  --work-dir /tmp/graphoxide-corpus-suite \
  --output /tmp/graphoxide-corpus-suite.json
```

Every corpus pins reviewed SHA-256 digests of the strict, deterministic
canonical Graphify and Graphoxide graphs. Any unreviewed candidate-only node,
edge, type change, weight, or metadata change therefore fails even when the
asymmetric reference-preservation contract would otherwise allow an extension.
Updating a digest is an explicit review action, not automatic blessing. The
resolution corpus additionally requires complete reference preservation. The
collision corpus instead requires Graphoxide to eliminate the three unsafe
upstream cross-runtime hubs and asserts the exact safe replacement edges; this
preserves the intentional divergence without allowing unrelated drift.
The language-matrix corpus exercises Bash, Dart, PHP, Ruby, JVM, .NET, Pascal,
native, Terraform, SQL, and container/config inputs. It requires zero candidate
identity hubs, asserts 33 reviewed semantic edges, forbids eight known-wrong
cross-language or reversed edges, and records Graphify's two reference identity
hubs as an expected upstream defect rather than treating them as parity.

### Differential contract and diagnostics

Release parity is **normalized structural parity**, not byte-for-byte JSON
identity. Ordering, checkout-root spelling, and presentation-only community
fields may differ. Stable node identity and type/source facts, directed edge
endpoints and multiplicity, relations, confidence and call-site metadata, and
hyperedge membership remain significant. The `strict` profile is useful for
serialization forensics, but additive metadata such as extractor provenance can
make it differ even when the normalized topology is sound.

The report retains the original `summary`, `nodes`, `edges`, and `hyperedges`
fields and adds three diagnostic views:

- `diagnostics.pre_normalization` reports absolute persisted source paths,
  IDs that are absolute or derived from an absolute source path, and duplicate
  node IDs. It also rejects malformed collections and records, dangling edge or
  hyperedge endpoints, and conflicting serialized aliases such as `_src`
  disagreeing with `source` or `links` disagreeing with `edges`. These are
  portability/schema/identity violations and make the overall result unequal
  even when canonicalization would otherwise hide them. Duplicate IDs are
  compared as record multisets; no record is silently discarded by a
  dictionary conversion.
- `diagnostics.identity_hubs` reports a unique endpoint whose node/incident-edge
  provenance spans multiple incompatible runtime families. This catches a
  Python `Base` reference welded onto an unrelated TypeScript `Base` even though
  the finished graph contains only one (formally unique) node ID. It is an audit
  signal rather than an automatic parity failure because explicit bridges can be
  legitimate; JVM, JS/TS, and native interop extensions are grouped together.
  Pass `--fail-on-candidate-identity-hubs` to turn this candidate-side audit
  into a CI gate after reviewing any explicit bridges in the corpus.
- Entity `groups` aggregate node differences by source file and changed field,
  and edge differences by source file and relation. Edge counts retain
  multiplicity.
- `parity.shared` evaluates only identities present on both sides.
  `parity.extensions` separately counts candidate-only identities and facts
  touching them, as well as reference-only coverage. “Candidate-only” is a
  mechanical classification, not a claim that an addition is desirable or
  supported. Extra facts between two shared identities remain shared-topology
  differences, not extensions.

The top-level `equal` field remains the complete gate: metadata, shared
differences, reference-only coverage, candidate-only additions, malformed or
unresolved differences, and pre-normalization validity failures all keep it
false.

## Reproduce the upstream inventory

`uv` is required because the pinned project uses `uv.lock` to freeze its pytest
collection environment.

```bash
git clone https://github.com/Graphify-Labs/graphify.git upstream/graphify-v0.9.32
git -C upstream/graphify-v0.9.32 checkout 00efd6e7969837ae4a9f11d8d504dcd3b20b09df
python3 parity/verify.py check \
  --upstream-checkout upstream/graphify-v0.9.32
```

The live check verifies `HEAD` and a clean tracked/non-ignored worktree both
before and after running the locked command `uv run --frozen python -m pytest
--collect-only -q`; ignored environments such as `.venv` remain allowed.
Modules reported by pytest retain pytest's exact IDs. A module skipped wholly
by an optional dependency is enumerated from its source AST; unsupported
dynamic parametrization fails rather than disappearing. The complete ordered
node-ID stream is then compared with `manifest.json`. Added, removed,
duplicated, reordered, or renamed cases fail the check.

To reproduce the checked-in manifest byte-for-byte after reviewing a deliberate
pin change:

```bash
python3 parity/tools/import_inventory.py \
  --checkout upstream/graphify-v0.9.32 \
  --write
python3 parity/verify.py check \
  --upstream-checkout upstream/graphify-v0.9.32
```

Never update the digest merely to silence a drift failure. Inspect the upstream
test additions/removals and create corresponding executable parity mappings or
reviewed expected divergences first.

## Mapping policy

A mapping is a behavioral assertion, not a similarity label. Add one only when
the named target executes the behavior and assertions represented by that exact
upstream node ID. It is valid for one Graphoxide test to cover several
parameterized upstream cases if it exercises every corresponding vector. It is
not valid to map an entire module to a broad smoke test.

See [mappings/README.md](mappings/README.md) for parity claims and
[divergences/README.md](divergences/README.md) for reviewed differences.
