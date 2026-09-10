# Unreleased

These changes have no assigned release version. The CLI and VS Code 0.13.0
release notes retain the history recorded by the existing tag.

- Direct-source knowledgebases use `graphoxide wiki init`, `wiki source`, and
  `wiki live`. Source metadata contains logical pointers and content digests;
  external source bodies remain outside the knowledgebase's tracked content.
- Add and refresh explicitly author provisional derived Markdown using a
  configured author model. AI review and local human confirmation are separate
  operations. Initialization requires a secret-free authoring profile, with
  explicit consent before sending source material to a model.
- Source operations support local files, local pinned Git sources, and bounded
  HTTPS reads. Unsupported binary/non-UTF-8 evidence is rejected before model
  requests. Remote Git fetching is disabled until transfer, storage, and
  subprocess limits can be enforced.
- Windows accepts individual source files; bulk directory imports remain
  unavailable. Linux and macOS also support directory inputs.
- Knowledgebase MCP lifecycle tools operate on a bound project root; writes,
  network access, and model egress require explicit authorization. The VS Code
  extension includes this workflow through its bundled CLI.
- `wiki live` serves a local preview using a separately installed Hugo 0.165.0
  binary selected by `GRAPHOXIDE_HUGO_BINARY`. Graphoxide does not install Hugo.
- The previous wiki commands are removed without automatic migration. This
  workflow does not expose `wiki research`, `wiki publish`, `wiki schema`, or a
  managed Hugo command group.
- Rust integration tests share one executable per crate. Exact parity mappings
  follow the consolidated test names. Local Cargo builds retain platform
  linker defaults, and optional nextest acceleration has a Cargo fallback for
  the pre-push test-listing check.
