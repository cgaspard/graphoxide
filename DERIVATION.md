# Derivation and attribution

Graphoxide is an independent Rust reimplementation inspired by and behaviorally
compatible with portions of [Graphify](https://github.com/Graphify-Labs/graphify),
created by Safi Shamsi and the Graphify contributors.

The reference source used during the conversion was Graphify revision
`00efd6e7969837ae4a9f11d8d504dcd3b20b09df`. The original project is distributed
under Apache-2.0 and retains an MIT license for portions contributed before its
relicensing. Graphoxide therefore retains the upstream copyright and attribution
notice in `NOTICE`, the Apache-2.0 terms in `LICENSE`, and the historical MIT text
in `LICENSE-MIT`.

Graphoxide is not a binary repackaging. It replaces the Python implementation
with a Rust workspace and adds a native CLI/MCP runtime, editor integration, and
website. Files containing logic derived from specific upstream modules identify
that origin in their module documentation where useful.

“Graphify” is used only to identify the project that inspired this work and to
describe schema and behavioral compatibility. Graphoxide is not affiliated with
or endorsed by Graphify or Graphify Labs.

Third-party dependency licenses are generated from the locked Cargo dependency
graph with `cargo about generate --workspace --locked --fail --output-file
THIRD_PARTY_LICENSES.html about.hbs`.
