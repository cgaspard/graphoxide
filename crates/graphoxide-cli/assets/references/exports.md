# Exports

Export an existing Graphoxide graph without re-extracting the repository:

```bash
graphoxide export html --graph graphoxide-out/graph.json --output graphoxide-out/graph.html
graphoxide export graphml --graph graphoxide-out/graph.json --output graphoxide-out/graph.graphml
graphoxide export wiki --graph graphoxide-out/graph.json --output graphoxide-out/wiki
graphoxide export obsidian --graph graphoxide-out/graph.json --dir graphoxide-out/obsidian
graphoxide report --graph graphoxide-out/graph.json
```

Choose the smallest export that answers the user's need; the JSON graph remains the evidence-bearing source of truth.
