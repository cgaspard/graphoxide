# Query

Query the existing Graphoxide graph before reading a repository broadly:

```bash
graphoxide query "authentication flow" --budget 2000
graphoxide path AuthService Database
graphoxide explain AuthService
graphoxide affected AuthService --depth 3
graphoxide god-nodes --top 10
```

Ground conclusions in returned labels, relationships, and source locations. If no graph exists, run `graphoxide extract .` first.
