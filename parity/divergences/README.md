# Reviewed expected divergences

This directory accounts for pinned upstream cases whose behavior Graphoxide
intentionally does not match. A divergence is release-visible compatibility
debt, not mapped parity. Each row must name executable evidence for the current
Graphoxide behavior and one concise reviewed reason.

```json
{
  "schema_version": 1,
  "divergences": [
    {
      "upstream": "tests/test_example.py::test_hosted_mode",
      "targets": [
        {
          "runner": "rust",
          "package": "graphoxide-cli",
          "id": "test_hosted_mode_is_explicitly_unsupported"
        }
      ],
      "reason": "Graphoxide intentionally supports only the offline execution path."
    }
  ]
}
```

Target schemas are identical to `parity/mappings/`. The verifier rejects an
unknown upstream ID, missing executable target, blank or multiline reason,
duplicate divergence, or any case claimed by both ledgers.
