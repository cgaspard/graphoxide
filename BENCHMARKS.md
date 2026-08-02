# Benchmarks

Measured 2026-08-01 on an Apple M5 Max (18 logical CPUs), macOS Darwin 25.5.0, Rust 1.95, release profile (`lto = "thin"`, one codegen unit). The Python oracle is the committed `upstream/` checkout executed from its `uv` environment.

## Reference corpus

The corpus is the upstream Graphify project's own `graphify/` package: 80 Python source files. Both implementations ran code-only forced extraction into fresh temporary directories.

| Measurement | Python | Rust | Result |
|---|---:|---:|---:|
| Full extract/build/cluster wall time | 5.40 s | 1.45 s | Rust 3.72× faster |
| Cached structural update | — | 1.19 s | deterministic no-change rebuild |
| Raw nodes | 2,174 | 2,174 | exact count and ID set |
| Raw edges | 4,985 | 4,908 | Rust −1.54% |
| Built nodes | 2,168 | 2,162 | Rust −0.28% |
| Built edges | 4,351 | 4,300 | Rust −1.17% |

The raw relation difference is concentrated in conservative call/import resolution. Structural containment, method, inheritance, rationale, and import-from counts match; Python emits a small number of additional ambiguous/indirect relationships.

Python's test environment did not include optional `graspologic`, so it used its NetworkX Louvain fallback (105 communities). Rust always uses the pinned `network_partitions` Leiden implementation required by the port contract (125 communities on the Rust-built graph); those partition counts are intentionally not treated as an algorithm-parity comparison.

## Query startup

Twenty independent CLI processes queried a roughly 2.1k-node graph with `how does extraction resolve calls`; stdout/stderr were discarded. Values below are wall-clock process latency.

| Executable | Median | Mean | Minimum |
|---|---:|---:|---:|
| Upstream Python Graphify query | 123.949 ms | 128.162 ms | 123.065 ms |
| Rust Graphoxide query | 19.940 ms | 20.071 ms | 18.773 ms |

Rust median cold-query latency is 6.22× lower. The Rust in-process `benchmark` command completed 1,000 full ranked/traversal queries in 8,205.709 ms (8.206 ms/query); this deliberately includes index construction for each independent query call.

## Determinism and artifacts

Two consecutive `graphoxide update` runs produced the identical SHA-256:

```text
5ce67b016f25e9f82fc51d9e29195e9006bf116bbc73b6b92b81044002e1844c
```

Release binary sizes on this machine:

| Binary | Size |
|---|---:|
| `graphoxide` | 27 MiB |

Numbers are local engineering measurements, not universal claims; filesystem cache state, optional Python clustering dependencies, CPU, linker, and corpus shape all affect results.
