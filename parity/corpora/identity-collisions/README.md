# Cross-language identity collision corpus

This deliberately small corpus gives Graphify and Graphoxide the same generic
names in incompatible language families. Run it through the differential
harness from the repository root:

```bash
python3 -m parity.differential.graph_diff run \
  --corpus parity/corpora/identity-collisions \
  --build \
  --fail-on-candidate-cross-runtime-bindings \
  --work-dir /tmp/graphoxide-identity-collisions \
  --output /tmp/graphoxide-identity-collisions/report.json
```

The hostile cases are:

- Julia and Fortran modules both named `Geometry`;
- a TypeScript import and a PowerShell manifest dependency both named `logger`;
- a Swift class/extension pair named `Foo`, plus an unrelated PowerShell
  `Import-Module Foo`;
- a Python implementation/stub (`.py`/`.pyi`) and a CUDA-header/C++ caller,
  both of which must resolve inside their real interop family;
- a C# `Child : Base` and an otherwise unrelated Python `Base` definition.

The Swift class/extension is a legitimate shared identity. Native, JVM, and
JavaScript/TypeScript interop families are likewise grouped by the cross-runtime
binding audit.
The C#/Python inheritance pair is a deliberate safety divergence from pinned
upstream behavior: Graphoxide keeps the unresolved C# supertype stub instead
of rewiring it to an unrelated Python class with the same label.
