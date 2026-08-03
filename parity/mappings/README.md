# Executable parity mappings

Each JSON file in this directory is an independent batch of reviewed parity
claims. The verifier merges the files and rejects duplicate mappings, unknown
upstream IDs, empty targets, extra prose fields, and non-executable target IDs.
An assertion of opposite or intentionally unsupported behavior is not parity;
record it under `parity/divergences/` instead.

Use this exact shape:

```json
{
  "schema_version": 1,
  "mappings": [
    {
      "upstream": "tests/test_id_normalization_contract.py::test_make_id_joins_then_normalizes",
      "targets": [
        {
          "runner": "rust",
          "package": "graphoxide-core",
          "id": "ids::tests::matches_upstream_vectors"
        }
      ]
    }
  ]
}
```

The example illustrates syntax only; do not copy it as a parity claim without
reviewing whether the current Rust assertions cover that exact upstream case.

## Rust targets

```json
{
  "runner": "rust",
  "package": "graphoxide-extract",
  "id": "tests::extracts_python_symbols"
}
```

`package` is the exact Cargo package name. `id` is an exact line (without the
trailing `: test`) from:

```bash
cargo test -p graphoxide-extract -- --list --format terse
```

The standard verifier checks that the ID is present. Pass
`--execute-rust-targets` to execute each distinct mapped Rust ID too.

## VS Code targets

```json
{
  "runner": "vscode",
  "workspace": "editors/vscode",
  "id": "parses and indexes a Graphoxide graph"
}
```

`id` is the exact `node:test` title. The verifier compiles the extension, runs
the compiled test suite, and checks the emitted TAP subtest IDs.

## Differential targets

```json
{
  "runner": "differential",
  "id": "parity.differential.test_extract.ExtractParity.test_python_functions"
}
```

Differential tests use Python's standard `unittest` discovery and must live
under `parity/differential/`. The fully qualified `id` is executed directly by
the verifier. A differential test should run Graphify and Graphoxide on the same
fixture and compare the contract relevant to its mapped upstream cases.
