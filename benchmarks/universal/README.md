# Universal indexing qualification

This directory contains the byte-exact, mixed-format corpus used to qualify
Graphoxide's universal indexing path. Qualification is a correctness and raw
evidence workflow. It never turns timing, throughput, memory, queue, or cache
observations into pass/fail performance thresholds.

## Content-addressed corpus

`catalog.json` selects a profile by the SHA-256 of its manifest. Each manifest
is stored as `manifests/<sha256>.json`; every source byte string is stored as
`objects/sha256/<first-two>/<sha256>`. The loader verifies the catalog,
manifest filename, object name, size, hash, single-link regular-file identity,
portable path ordering, case-fold collisions, and file/directory prefix
collisions before it creates a project.
The committed catalog intentionally admits exactly one profile, keeping all
manifest/object closure work inside the single 64 MiB corpus budget before any
object bytes are materialized. The optional large profile uses the separate
acknowledged generator path below.

The committed `ci-mixed-v1` profile is deliberately small. It covers source,
structured text, IDL, SVG, delimited data, a package manifest, a sensitive
filename, and an unsupported filename. Its incremental mutation replaces one
Rust const identifier with a distinct committed fixed-width identifier. The
large generator uses the same parser-visible mutation, so the small CI
qualification proves with real extraction that the graph digest changes. Git
attributes force LF metadata and disable text conversion for object bytes.

Safety ceilings are part of the contract:

- catalog: 1 MiB;
- manifest: 8 MiB;
- files: 4,096;
- one object: 16 MiB;
- active corpus: 64 MiB;
- stdout and stderr: 8 MiB each (the process is terminated at cap + 1);
- runtime and coverage JSON: 16 MiB each;
- complete report: 64 MiB, with a cumulative conservative evidence budget.

Materialization uses exclusive files, verifies bytes through the same open
descriptor, fixes timestamps at Unix second `946684800`, and never overwrites a
target. Qualification projects are intentionally retained for inspection; the
runner does not recursively clean trees that have received untrusted build
output. Their exact locations are recorded in the report and removal is a
manual operator action.

## Modes

- `cold`: a newly materialized project with no `graphoxide-out`; operating-system
  caches are explicitly recorded as uncontrolled.
- `warm`: preserves manifest, cache, coverage, and build policy from the cold
  pass and removes only `graphoxide-out/graph.json` before the measured rebuild.
- `incremental`: starts from warm state and applies the one fixed-size committed
  mutation before the measured incremental index.
- `controlled_os_cold`: a fresh project after an explicitly acknowledged cache
  control helper. It is Linux-only and must be requested alone. The helper must
  have a canonical absolute path, a pinned SHA-256, root-owned nonwritable
  ancestry, one link, and no writable or untrusted path component. It is invoked
  directly with no arguments, shell, `sudo`, `su`, or `PATH`.

Every command requests runtime telemetry schema v2 and retains the exact stdout
build report, runtime report, coverage outcomes, graph/manifest streaming
digests, environment, storage filesystem details, failures, and raw bounded
stdout/stderr bytes. The runner verifies the stdout and sidecar build objects
are identical and checks source-I/O, parser-work, cache, peak queue/transfer, and
peak-RSS evidence for internal consistency.

The runtime fields `peak_ready_bytes`, `peak_ready_items`, and
`peak_in_flight_transfer_bytes` are peak live reserved admission credits. They
include pre-open or maximum-bound reservations and do not claim resident payload
bytes or actual completed transfers. Completed cache-transfer totals are the
separate `payload_bytes_read`, `payload_bytes_written`, `artifact_bytes_read`,
and `artifact_bytes_written` counters. Qualification summaries therefore expose
the cache peak as `peak_cache_transfer_credit_bytes`. The separately sourced
`process.peak_rss_bytes` field is the resident-memory observation when the
operating system makes it available.

## Run the CI profile

Build the exact binary outside the measured region, then choose a new canonical
report path (existing reports are never overwritten). Qualification requires a
single-link executable so its pinned content identity cannot alias another
pathname. Cargo may hard-link its release artifact, so stage a verified
byte-copy with `install` in a new runner-owned `0700` directory:

```bash
cargo build --release --locked --bin graphoxide
stage_parent="$(mktemp -d "${TMPDIR:-/tmp}/graphoxide-qualification-binary.XXXXXX")"
chmod 700 "$stage_parent"
stage_parent="$(realpath "$stage_parent")"
staged_binary="$stage_parent/graphoxide"
install -m 0700 target/release/graphoxide "$staged_binary"
staged_binary="$(realpath "$staged_binary")"
STAGED_BINARY="$staged_binary" node --input-type=module -e \
  'import { lstatSync } from "node:fs"; const value = lstatSync(process.env.STAGED_BINARY); if (!value.isFile() || value.isSymbolicLink() || value.nlink !== 1) throw new Error("staged binary must be a single-link regular file")'
cmp -s target/release/graphoxide "$staged_binary"
node scripts/qualify-universal-indexing.mjs \
  --profile ci-mixed-v1 \
  --binary "$staged_binary" \
  --report "$(pwd -P)/qualification-ci.json"
```

## Optional 70 GiB profile

The opt-in `synthetic-70gib-v1` profile creates exactly 4,096 files of
18,350,080 bytes each: 75,161,927,680 bytes (70 GiB). Each file is a fixed Rust
header followed by an AES-256-CTR-derived hexadecimal block comment. Individual
generator buffers never exceed 1 MiB. The generated 4,096-entry manifest is
published under its own digest and records exact before/after mutation hashes.

The parent supplied as `--large-root` must already exist at a canonical absolute
path, be owned by the runner, and not be group/world writable. The runner
requires at least 210 GiB free before generation and preserves a 32 GiB reserve. It
creates one exclusive child, refuses any preexisting child, leaves a partial
marker on interruption, never resumes or overwrites partial output, and never
automatically removes the generated tree.

```bash
node scripts/qualify-universal-indexing.mjs \
  --profile synthetic-70gib-v1 \
  --large-root /canonical/preexisting/qualification-volume \
  --acknowledge-70-gib \
  --acknowledge-large-disk-use \
  --binary "$staged_binary" \
  --report /canonical/preexisting/reports/qualification-70gib.json
```

Controlled OS-cold mode additionally requires
`--controlled-os-cold-helper`, `--controlled-os-cold-helper-sha256`,
`--acknowledge-controlled-os-cold`, and `--acknowledge-host-cache-drop`.
Graphoxide never supplies or elevates to a privileged helper itself.
The manual qualification workflow is restricted to this repository's `main`
branch on the dedicated self-hosted runner and has an explicit 12-hour job
timeout. It exposes both the small controlled run and a combined
controlled-OS-cold 70 GiB operation; the combined operation requires all four
acknowledgements plus the large root and pinned helper inputs. Its canonical
report directory must be owned by the runner and not group/world writable. Both
hosted and manual workflows stage and verify the release binary before passing
its canonical private path to the qualification runner.
