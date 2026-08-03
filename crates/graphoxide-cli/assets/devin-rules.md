## graphoxide

This project has a Graphoxide knowledge graph at `graphoxide-out/`.

Rules:
- For codebase or architecture questions, when `graphoxide-out/graph.json` exists, first run `graphoxide query "<question>"` (or `graphoxide path "<A>" "<B>"` / `graphoxide explain "<concept>"`). These return a scoped subgraph, usually much smaller than `GRAPH_REPORT.md` or broad source-search output.
- If `graphoxide-out/wiki/index.md` exists, navigate it instead of reading every raw file.
- Read `graphoxide-out/GRAPH_REPORT.md` only for broad architecture review or when query/path/explain do not surface enough context.
- After modifying code files in this session, run `graphoxide update .` to keep the graph current.
