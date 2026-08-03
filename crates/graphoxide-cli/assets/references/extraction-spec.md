# Extraction specification

Graphoxide's structural extraction records deterministic nodes, typed edges, confidence, source files, and source locations. Build and immediately audit when completeness matters:

```bash
graphoxide extract . --force
graphoxide audit . --strict
```

Node IDs use the extension-free full repository-relative source path plus the entity name, normalized to lowercase underscore-separated text:

- `src/auth/session.py` + `ValidateToken` → `src_auth_session_validatetoken`
- `lib/utils/helpers.py` + `parse_url` → `lib_utils_helpers_parse_url`
- `tests/test_foo.py` + `_helper` → `tests_test_foo_helper`
- `docs/v1/api/README.md` + `getUser` → `docs_v1_api_readme_getuser`

Do not use a filename-only or immediate-parent-only ID; those forms collide across packages and split AST and semantic facts into disconnected nodes. Treat audit failures as real data-loss signals, and never invent missing nodes or edges.
