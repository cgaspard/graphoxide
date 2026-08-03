# Adversarial resolution corpus

This corpus exercises graph facts that are easy to over-resolve or silently
drop across languages: source-layout Python imports, callback references,
external JavaScript packages, Go predeclared functions, PHP case folding,
Rust case sensitivity, and TypeScript builtin receiver types.

Run both pinned Graphify and Graphoxide on these exact files and retain the
artifacts for inspection:

```bash
python3 -m parity.differential.graph_diff run \
  --corpus parity/corpora/resolution-adversarial \
  --build \
  --fail-on-candidate-identity-hubs \
  --work-dir /tmp/graphoxide-resolution-adversarial \
  --output /tmp/graphoxide-resolution-adversarial/report.json
```

The safety invariants are stricter than raw equality: unresolved external
imports must remain `ref`-namespaced, case-sensitive identifiers must not
collapse, Go builtins must not bind to user methods, imported classes must not
become callbacks, and ambiguous package suffixes must remain unresolved.
