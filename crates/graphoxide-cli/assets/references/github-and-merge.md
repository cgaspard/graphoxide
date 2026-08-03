# GitHub and graph merging

Clone repositories with Git, build each Graphoxide graph, then merge the graph files explicitly:

```bash
git clone https://github.com/OWNER/REPOSITORY.git
graphoxide extract REPOSITORY
graphoxide merge-graphs repo-a/graphoxide-out/graph.json repo-b/graphoxide-out/graph.json --output combined/graph.json
```

Keep repository roots stable so source paths remain portable and comparable across machines.
