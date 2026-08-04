# Graph build benchmarks

Graphoxide includes a reproducible local benchmark for a fresh graph build and
a one-file incremental update. The benchmark records observations; it does not
enforce a performance threshold or claim that one machine's timings apply to
another environment.

## Run the baseline

From the repository root:

```bash
npm --silent run benchmark:graph-build
```

The runner builds the release binary before collecting samples, so compilation
is not included in any timed region. It defaults to five independent samples
and prints one JSON report. The `--silent` npm flag keeps command-wrapper text
out of standard output; build failures are reported on standard error.

Change the sample count or use a previously built binary and another fixture:

```bash
npm --silent run benchmark:graph-build -- --runs 10

node scripts/benchmark-graph-build.mjs \
  --runs 5 \
  --binary target/release/graphoxide \
  --fixture parity/corpora/language-matrix
```

Run `node scripts/benchmark-graph-build.mjs --help` for the supported options.
The default maximum of 100 samples keeps accidental runs bounded.

## Baseline fixture

The default fixture is [`parity/corpora/language-matrix`](parity/corpora/language-matrix).
It contains 33 files and exercises multiple compiled and deterministic fallback
extractors. Its complete input-tree digest is pinned by the parity corpus suite:

```text
1b0e49b8bbac8a7414a38bb36b6e52b4259e66d04cbc5b2e5663e93a88adf0ef
```

The digest covers every relative path, directory, and file byte, including
negative-control inputs that Graphoxide does not structurally index. The
benchmark reports the digest it actually measured, so results from different
fixture revisions are distinguishable.

Every sample receives a fresh temporary copy of the fixture. After the full
build, the runner appends a deterministic `GraphoxideBenchmarkMutation`
declaration to `jvm/app/Runner.java` in that copy only. The checked-in fixture is
never modified. A custom fixture uses the same preferred path when present, or
the first source file with a built-in deterministic mutation strategy in
normalized path order.

## Measured operations

Each independent sample performs these operations:

1. Copy the fixture into a new temporary directory. This setup is not timed.
2. Time a fresh-output build:

   ```bash
   graphoxide extract . --force --json
   ```

3. Apply the deterministic one-file mutation. This setup is not timed.
4. Time the incremental update against the graph created in step 2:

   ```bash
   graphoxide update . --json
   ```

5. Remove the temporary sample directory.

The report retains two timing domains for both operations:

- `external_wall_ms` measures the child process from launch through exit and
  therefore includes process startup.
- `reported_elapsed_ms` is the CLI's top-level `elapsed_ms` value and describes
  the work measured inside Graphoxide.

Raw CLI reports are preserved under every sample. Separate min, median, and max
summaries are computed for external and CLI-reported timings. The runner rejects
a successful command that does not produce one parseable JSON object with a
finite, non-negative `elapsed_ms`, the expected operation/mode/status, and—for
the incremental sample—exactly one processed changed file.

The JSON also records the fixture digest, Git commit and dirty state, binary
digest, Graphoxide and Rust versions, Node version, operating system,
architecture, CPU model/count, and memory size.

## Interpreting results

The benchmark creates a fresh Graphoxide output and extraction cache for every
full-build sample, but it does not flush or control operating-system filesystem
caches. Call this a **fresh-output build**, not a cold build.

Only compare results when the commit, fixture digest, command shape, release
profile, toolchain, and environment are materially equivalent. Background
load, filesystem state, virtualization, thermal behavior, and sample count can
all change measured durations. Keep raw reports with any analysis and describe
the environment; do not turn these local observations into universal latency or
speedup claims.

Timing values are intentionally excluded from CI pass/fail decisions. CI runs
only the benchmark runner's deterministic argument, digest, JSON-validation,
mutation, and summary tests.
