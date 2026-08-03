# Update

Refresh a managed Graphoxide graph after files change:

```bash
graphoxide update .
graphoxide audit . --strict
```

Graphoxide refuses suspicious graph shrinkage. Pass `--force` only after confirming that deleted files or relationships explain the reduction.
