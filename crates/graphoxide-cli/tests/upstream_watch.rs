use filetime::{set_file_mtime, FileTime};
use graphoxide_cli::watch::*;
use graphoxide_core::{Confidence, Edge, KnowledgeGraph, Node};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::TempDir;

fn options(cwd: &Path, no_cluster: bool) -> RebuildOptions {
    RebuildOptions {
        no_cluster,
        acquire_lock: false,
        invocation_cwd: Some(cwd.to_path_buf()),
        ..Default::default()
    }
}

fn build(root: &Path, no_cluster: bool) -> RebuildResult {
    rebuild_project(root, &options(root, no_cluster)).unwrap()
}

fn build_from(cwd: &Path, target: &Path, no_cluster: bool) -> RebuildResult {
    rebuild_project(target, &options(cwd, no_cluster)).unwrap()
}

fn graph_path(root: &Path) -> PathBuf {
    root.join(OUTPUT_DIRECTORY).join("graph.json")
}

fn graph(root: &Path) -> KnowledgeGraph {
    graphoxide_core::read_graph(graph_path(root)).unwrap()
}

fn write_graph(root: &Path, graph: &KnowledgeGraph) {
    graphoxide_core::write_graph_atomic(graph_path(root), graph, true).unwrap();
}

fn write(path: impl AsRef<Path>, text: &str) {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn rewrite_with_new_mtime(path: impl AsRef<Path>, text: &str) {
    let path = path.as_ref();
    let previous = FileTime::from_last_modification_time(&fs::metadata(path).unwrap());
    write(path, text);
    set_file_mtime(
        path,
        FileTime::from_unix_time(previous.unix_seconds().saturating_add(2), 0),
    )
    .unwrap();
}

fn node(id: &str, source: &str, ast: bool) -> Node {
    Node {
        id: id.into(),
        label: id.into(),
        file_type: if ast { "code" } else { "concept" }.into(),
        source_file: source.into(),
        source_location: ast.then(|| "L1".into()),
        community: None,
        extra: if ast {
            BTreeMap::from([("_origin".into(), json!("ast"))])
        } else {
            BTreeMap::default()
        },
    }
}

fn edge(source: &str, target: &str, relation: &str, source_file: &str, ast: bool) -> Edge {
    Edge {
        source: source.into(),
        target: target.into(),
        relation: relation.into(),
        confidence: Confidence::Extracted,
        source_file: source_file.into(),
        extra: if ast {
            BTreeMap::from([("_origin".into(), json!("ast"))])
        } else {
            BTreeMap::default()
        },
    }
}

fn sources(graph: &KnowledgeGraph) -> BTreeSet<String> {
    graph
        .nodes
        .iter()
        .map(|node| node.source_file.clone())
        .collect()
}

fn ids(graph: &KnowledgeGraph) -> BTreeSet<String> {
    graph.nodes.iter().map(|node| node.id.clone()).collect()
}

fn labels(graph: &KnowledgeGraph) -> BTreeSet<String> {
    graph.nodes.iter().map(|node| node.label.clone()).collect()
}

fn seed_two_code_files(root: &Path) {
    write(root.join("keep.py"), "def keep_fn():\n    return 1\n");
    write(root.join("drop.py"), "def drop_fn():\n    return 2\n");
    assert!(build(root, true).succeeded());
}

fn semantic_pair(graph: &mut KnowledgeGraph, source: &str) {
    graph.nodes.push(node("docs_topic", source, false));
    graph.nodes.push(node("shared_concept", source, false));
    graph.links.push(edge(
        "docs_topic",
        "shared_concept",
        "related_to",
        source,
        false,
    ));
    graph.hyperedges.push(json!({
        "id": "semantic_context",
        "nodes": ["docs_topic", "shared_concept"],
        "source_file": source,
    }));
}

#[test]
fn test_notify_only_creates_flag() {
    let temp = tempfile::tempdir().unwrap();
    let flag = notify_only(temp.path()).unwrap();
    assert!(flag.is_file());
    assert_eq!(fs::read_to_string(flag).unwrap(), "1");
}

#[test]
fn test_notify_only_creates_flag_dir() {
    let temp = tempfile::tempdir().unwrap();
    assert!(!temp.path().join(OUTPUT_DIRECTORY).exists());
    notify_only(temp.path()).unwrap();
    assert!(temp.path().join(OUTPUT_DIRECTORY).is_dir());
}

#[test]
fn test_notify_only_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    notify_only(temp.path()).unwrap();
    notify_only(temp.path()).unwrap();
    assert_eq!(
        fs::read_to_string(temp.path().join(OUTPUT_DIRECTORY).join(NEEDS_UPDATE)).unwrap(),
        "1"
    );
}

#[test]
fn test_watched_extensions_includes_code() {
    for extension in [".py", ".ts", ".go", ".rs"] {
        assert!(is_watched_extension(extension));
    }
}

#[test]
fn test_watched_extensions_includes_docs() {
    for extension in [".md", ".txt", ".pdf"] {
        assert!(is_watched_extension(extension));
    }
}

#[test]
fn test_watched_extensions_includes_images() {
    for extension in [".png", ".jpg"] {
        assert!(is_watched_extension(extension));
    }
}

#[test]
fn test_watched_extensions_excludes_noise() {
    assert!(is_watched_extension(".json"));
    assert!(is_watched_extension(".sh"));
    assert!(!is_watched_extension(".pyc"));
    assert!(!is_watched_extension(".log"));
}

#[test]
fn test_watched_extension_projection_matches_the_shared_registry() {
    let expected: Vec<_> = graphoxide_extract::format_registry::format_registry()
        .watched_extensions()
        .map(|extension| format!(".{extension}"))
        .collect();
    let compatibility: Vec<_> = WATCHED_EXTENSIONS
        .iter()
        .map(|extension| (*extension).to_owned())
        .collect();
    assert_eq!(compatibility, expected);
}

#[test]
fn test_check_update_no_flag_returns_true() {
    let temp = tempfile::tempdir().unwrap();
    let notice = check_update(temp.path());
    assert!(!notice.pending);
    assert!(notice.message.is_none());
}

#[test]
fn test_check_update_with_flag_returns_true_and_prints() {
    let temp = tempfile::tempdir().unwrap();
    notify_only(temp.path()).unwrap();
    let notice = check_update(temp.path());
    assert!(notice.pending);
    assert!(notice.message.unwrap().contains("graphoxide update"));
}

#[test]
fn test_check_update_does_not_clear_flag() {
    let temp = tempfile::tempdir().unwrap();
    let flag = notify_only(temp.path()).unwrap();
    let _ = check_update(temp.path());
    assert!(flag.exists());
}

#[test]
fn test_watch_raises_without_watchdog() {
    let error = require_watch_backend(false).unwrap_err().to_string();
    assert!(error.contains("watch backend not installed"));
}

#[test]
fn test_rebuild_lock_writes_pid_with_newline() {
    let temp = tempfile::tempdir().unwrap();
    let guard = RebuildLockGuard::acquire(temp.path(), false)
        .unwrap()
        .unwrap();
    assert_eq!(
        fs::read_to_string(guard.path()).unwrap(),
        format!("{}\n", std::process::id())
    );
}

#[cfg(unix)]
#[test]
fn test_rebuild_lock_rejects_symlink_without_touching_external_target() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("output");
    fs::create_dir(&output).unwrap();
    let external = temp.path().join("external.txt");
    fs::write(&external, b"external lock target\n").unwrap();
    let lock = output.join(REBUILD_LOCK);
    symlink(&external, &lock).unwrap();

    let error = RebuildLockGuard::acquire(&output, false).unwrap_err();
    assert!(error.to_string().contains("unsafe rebuild lock"));
    assert_eq!(fs::read(&external).unwrap(), b"external lock target\n");
    assert!(lock.symlink_metadata().unwrap().file_type().is_symlink());
}

#[cfg(unix)]
#[test]
fn test_rebuild_lock_rejects_symlinked_output_directory() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let external = temp.path().join("external-output");
    fs::create_dir(&external).unwrap();
    let output = temp.path().join("output");
    symlink(&external, &output).unwrap();

    let error = RebuildLockGuard::acquire(&output, false).unwrap_err();
    assert!(error
        .to_string()
        .contains("unsafe rebuild output directory"));
    assert!(!external.join(REBUILD_LOCK).exists());
    assert!(output.symlink_metadata().unwrap().file_type().is_symlink());
}

#[test]
fn test_rebuild_lock_rejects_non_file_destination() {
    let temp = tempfile::tempdir().unwrap();
    let lock = temp.path().join(REBUILD_LOCK);
    fs::create_dir(&lock).unwrap();

    let error = RebuildLockGuard::acquire(temp.path(), false).unwrap_err();
    assert!(error.to_string().contains("unsafe rebuild lock"));
    assert!(lock.is_dir());
}

#[test]
fn test_rebuild_lock_rejects_hardlink_without_touching_external_target() {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("output");
    fs::create_dir(&output).unwrap();
    let external = temp.path().join("external.txt");
    fs::write(&external, b"external hardlink target\n").unwrap();
    let lock = output.join(REBUILD_LOCK);
    if let Err(error) = fs::hard_link(&external, &lock) {
        if matches!(
            error.kind(),
            std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
        ) {
            return;
        }
        panic!("create hardlink fixture: {error}");
    }

    let error = RebuildLockGuard::acquire(&output, false).unwrap_err();
    assert!(error.to_string().contains("multiply linked rebuild lock"));
    assert_eq!(fs::read(&external).unwrap(), b"external hardlink target\n");
    assert_eq!(fs::read(&lock).unwrap(), b"external hardlink target\n");
}

#[test]
fn test_rebuild_lock_inode_persists_after_release() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(REBUILD_LOCK);
    drop(
        RebuildLockGuard::acquire(temp.path(), false)
            .unwrap()
            .unwrap(),
    );
    assert!(path.is_file());
    assert!(RebuildLockGuard::acquire(temp.path(), false)
        .unwrap()
        .is_some());
}

#[test]
fn test_rebuild_lock_waiter_and_new_arrival_share_one_inode() {
    use fs2::FileExt as _;
    use std::{sync::mpsc, thread, time::Duration};

    let temp = tempfile::tempdir().unwrap();
    let holder = RebuildLockGuard::acquire(temp.path(), false)
        .unwrap()
        .unwrap();
    let waiter_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(holder.path())
        .unwrap();
    let (acquired_tx, acquired_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let waiter = thread::spawn(move || {
        waiter_file.lock_exclusive().unwrap();
        acquired_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        fs2::FileExt::unlock(&waiter_file).unwrap();
    });

    drop(holder);
    acquired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(RebuildLockGuard::acquire(temp.path(), false)
        .unwrap()
        .is_none());
    release_tx.send(()).unwrap();
    waiter.join().unwrap();
    assert!(RebuildLockGuard::acquire(temp.path(), false)
        .unwrap()
        .is_some());
}

#[test]
fn test_rebuild_lock_does_not_accumulate_pids_across_runs() {
    let temp = tempfile::tempdir().unwrap();
    for _ in 0..5 {
        let guard = RebuildLockGuard::acquire(temp.path(), false)
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::read_to_string(guard.path()).unwrap(),
            format!("{}\n", std::process::id())
        );
        drop(guard);
    }
}

#[test]
fn test_graphify_root_preserves_relative_when_invoked_with_relative_path() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("lib.py"), "def f():\n    pass\n");
    assert!(build_from(temp.path(), Path::new("."), true).succeeded());
    assert_eq!(
        fs::read_to_string(temp.path().join(OUTPUT_DIRECTORY).join(COMPAT_ROOT_MARKER)).unwrap(),
        "."
    );
}

#[test]
fn test_rebuild_code_writes_community_name() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path().join("a.py"),
        "def alpha():\n    return beta()\n\ndef beta():\n    return 1\n",
    );
    assert!(build(temp.path(), false).succeeded());
    let clustered = graph(temp.path())
        .nodes
        .into_iter()
        .filter(|node| node.community.is_some())
        .collect::<Vec<_>>();
    assert!(!clustered.is_empty());
    assert!(clustered.iter().all(|node| node
        .extra
        .get("community_name")
        .and_then(Value::as_str)
        .is_some_and(|name| !name.is_empty())));
}

#[test]
fn test_rebuild_code_drops_labels_whose_community_changed() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path().join("a.py"),
        "def alpha():\n    return beta()\n\ndef beta():\n    return 1\n",
    );
    assert!(build(temp.path(), false).succeeded());
    let out = temp.path().join(OUTPUT_DIRECTORY);
    let labels_path = out.join(".graphify_labels.json");
    let labels: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(&labels_path).unwrap()).unwrap();
    let named = labels
        .keys()
        .map(|id| (id.clone(), format!("Named-{id}")))
        .collect::<BTreeMap<_, _>>();
    graphoxide_core::write_json_atomic(&labels_path, &named, false).unwrap();
    for name in ["b.py", "c.py", "d.py"] {
        write(
            temp.path().join(name),
            &format!(
                "def {}_one():\n    return {}_two()\n\ndef {}_two():\n    return 2\n",
                &name[..1],
                &name[..1],
                &name[..1]
            ),
        );
    }
    assert!(build(temp.path(), false).succeeded());
    let after: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(&labels_path).unwrap()).unwrap();
    let signatures: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(out.join(".graphify_labels.json.sig")).unwrap()).unwrap();
    assert!(!signatures.is_empty());
    assert!(after
        .iter()
        .all(|(id, label)| !label.starts_with("Named-") || signatures.contains_key(id)));
}

#[test]
fn test_rebuild_code_keeps_a_visualization_when_over_the_viz_cap() {
    let temp = tempfile::tempdir().unwrap();
    for group in 0..4 {
        for file in 0..2 {
            write(
                temp.path().join(format!("g{group}_{file}.py")),
                &format!("def g{group}_{file}():\n    return {file}\n"),
            );
        }
    }
    let mut first = options(temp.path(), false);
    first.viz_node_limit = Some(100);
    assert!(rebuild_project(temp.path(), &first).unwrap().succeeded());
    let html = temp.path().join(OUTPUT_DIRECTORY).join("graph.html");
    assert!(html.is_file());
    let before = fs::read_to_string(&html).unwrap();
    write(temp.path().join("extra.py"), "def extra():\n    return 1\n");
    let community_count = graph(temp.path())
        .nodes
        .iter()
        .filter_map(|node| node.community)
        .collect::<BTreeSet<_>>()
        .len();
    let mut capped = options(temp.path(), false);
    capped.viz_node_limit = Some(community_count.max(2) + 1);
    assert!(rebuild_project(temp.path(), &capped).unwrap().succeeded());
    assert!(html.is_file());
    assert_ne!(fs::read_to_string(&html).unwrap(), before);
    write(
        temp.path().join("extra2.py"),
        "def extra2():\n    return 2\n",
    );
    capped.viz_node_limit = Some(0);
    assert!(rebuild_project(temp.path(), &capped).unwrap().succeeded());
    assert!(!html.exists());
}

#[test]
fn test_update_rebuilds_with_nested_star_gitignore() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("src/a.py"), "def app():\n    return 1\n");
    write(temp.path().join("main.py"), "def top():\n    return 2\n");
    write(temp.path().join("scratch/.gitignore"), "*\n");
    write(temp.path().join("scratch/junk.py"), "x = 1\n");
    assert!(build(temp.path(), true).succeeded());
    let source = sources(&graph(temp.path()));
    assert!(source.contains("src/a.py"));
    assert!(source.contains("main.py"));
    assert!(!source.contains("scratch/junk.py"));
}

#[test]
fn test_update_discovers_newly_added_files_and_dirs() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("src/a.py"), "def alpha():\n    return 1\n");
    assert!(build(temp.path(), true).succeeded());
    write(
        temp.path().join("src/new.py"),
        "def added():\n    return 2\n",
    );
    write(
        temp.path().join("monitor/dash.py"),
        "def board():\n    return 3\n",
    );
    assert!(build(temp.path(), true).succeeded());
    let source = sources(&graph(temp.path()));
    assert!(source.contains("src/new.py"));
    assert!(source.contains("monitor/dash.py"));
}

#[test]
fn test_rebuild_honors_persisted_excludes() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path().join("src/app.py"),
        "def keep():\n    return 1\n",
    );
    write(
        temp.path().join("vendor/lib.py"),
        "def vendored():\n    pass\n",
    );
    write_build_config(
        &temp.path().join(OUTPUT_DIRECTORY),
        Some(&["vendor".into()]),
        None,
    )
    .unwrap();
    assert!(build(temp.path(), true).succeeded());
    let source = sources(&graph(temp.path()));
    assert!(source.contains("src/app.py"));
    assert!(!source.contains("vendor/lib.py"));
}

#[test]
fn test_rebuild_honors_persisted_no_gitignore() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join(".gitignore"), "generated/\n");
    write(
        temp.path().join("generated/gen.py"),
        "def generated():\n    return 1\n",
    );
    write_build_config(&temp.path().join(OUTPUT_DIRECTORY), None, Some(false)).unwrap();
    assert!(build(temp.path(), true).succeeded());
    assert!(sources(&graph(temp.path())).contains("generated/gen.py"));
}

#[test]
fn test_graphify_root_preserves_absolute_when_user_supplied() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("lib.py"), "def f():\n    pass\n");
    assert!(build(temp.path(), true).succeeded());
    assert_eq!(
        fs::read_to_string(temp.path().join(OUTPUT_DIRECTORY).join(COMPAT_ROOT_MARKER)).unwrap(),
        temp.path().to_string_lossy()
    );
}

#[test]
fn test_rebuild_code_deleted_cwd_without_repo_root_returns_false() {
    let error = resolve_watch_context(Path::new("."), None, None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("current working directory no longer exists"));
}

#[test]
fn test_rebuild_code_deleted_cwd_uses_graphify_repo_root() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("lib.py"), "def f():\n    pass\n");
    let context = resolve_watch_context(Path::new("."), None, Some(temp.path())).unwrap();
    assert_eq!(context.watch_root, fs::canonicalize(temp.path()).unwrap());
    let mut opts = options(temp.path(), true);
    opts.invocation_cwd = None;
    opts.repo_root_fallback = Some(temp.path().to_path_buf());
    opts.changed_paths = Some(vec![PathBuf::from("lib.py")]);
    assert!(rebuild_project(Path::new("."), &opts).unwrap().succeeded());
    assert!(graph_path(temp.path()).is_file());
}

#[test]
fn test_rebuild_code_evicts_nodes_from_deleted_files() {
    let temp = tempfile::tempdir().unwrap();
    seed_two_code_files(temp.path());
    assert!(labels(&graph(temp.path()))
        .iter()
        .any(|label| label.contains("drop_fn")));
    fs::remove_file(temp.path().join("drop.py")).unwrap();
    assert!(build(temp.path(), true).succeeded());
    let after = labels(&graph(temp.path()));
    assert!(!after.iter().any(|label| label.contains("drop_fn")));
    assert!(after.iter().any(|label| label.contains("keep_fn")));
}

fn run_preserve_hyperedge_case(incremental: bool) {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path().join("doc.md"),
        "# Design\n\n## Flow\n\nDetails.\n",
    );
    assert!(build(temp.path(), true).succeeded());
    let mut before = graph(temp.path());
    let members = before
        .nodes
        .iter()
        .take(2)
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(members.len(), 2);
    let hyperedge = json!({
        "id": "doc_flow_group",
        "label": "Doc flow group",
        "nodes": members,
        "relation": "implements",
        "confidence": "EXTRACTED",
        "confidence_score": 1.0,
        "source_file": "doc.md",
    });
    before.hyperedges = vec![hyperedge.clone()];
    write_graph(temp.path(), &before);
    let mut opts = options(temp.path(), true);
    if incremental {
        opts.changed_paths = Some(vec!["doc.md".into()]);
    }
    assert!(rebuild_project(temp.path(), &opts).unwrap().succeeded());
    assert_eq!(graph(temp.path()).hyperedges, vec![hyperedge]);
}

#[test]
fn test_rebuild_code_preserves_hyperedges_for_rebuilt_surviving_source_full_update() {
    run_preserve_hyperedge_case(false);
}

#[test]
fn test_rebuild_code_preserves_hyperedges_for_rebuilt_surviving_source_incremental_doc_update() {
    run_preserve_hyperedge_case(true);
}

fn run_preserve_semantic_edge_case(incremental: bool) {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path().join("auth.md"),
        "# Token Validation\n\nDetails.\n",
    );
    write(
        temp.path().join("login.md"),
        "# Session Verification\n\nDetails.\n",
    );
    assert!(build(temp.path(), true).succeeded());
    let mut before = graph(temp.path());
    let auth = before
        .nodes
        .iter()
        .find(|node| node.source_file == "auth.md")
        .unwrap()
        .id
        .clone();
    let login = before
        .nodes
        .iter()
        .find(|node| node.source_file == "login.md")
        .unwrap()
        .id
        .clone();
    before.links.push(edge(
        &auth,
        &login,
        "semantically_similar_to",
        "auth.md",
        false,
    ));
    before
        .links
        .push(edge(&auth, &login, "stale_references", "auth.md", true));
    write_graph(temp.path(), &before);
    let mut opts = options(temp.path(), true);
    if incremental {
        opts.changed_paths = Some(vec!["auth.md".into()]);
    }
    assert!(rebuild_project(temp.path(), &opts).unwrap().succeeded());
    let relations = graph(temp.path())
        .links
        .into_iter()
        .map(|edge| (edge.source, edge.target, edge.relation))
        .collect::<BTreeSet<_>>();
    assert!(relations.contains(&(
        auth.clone(),
        login.clone(),
        "semantically_similar_to".into()
    )));
    assert!(!relations.contains(&(auth, login, "stale_references".into())));
}

#[test]
fn test_rebuild_code_preserves_semantic_edges_from_reextracted_doc_full_update() {
    run_preserve_semantic_edge_case(false);
}

#[test]
fn test_rebuild_code_preserves_semantic_edges_from_reextracted_doc_incremental_doc_update() {
    run_preserve_semantic_edge_case(true);
}

fn run_prune_final_file_case(incremental: bool) {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path().join("only.py"),
        "def only_fn():\n    return 1\n",
    );
    assert!(build(temp.path(), true).succeeded());
    let mut before = graph(temp.path());
    let code_id = before
        .nodes
        .iter()
        .find(|node| node.source_file == "only.py")
        .unwrap()
        .id
        .clone();
    semantic_pair(&mut before, "");
    before.hyperedges.push(json!({
        "id": "code_context",
        "nodes": [code_id],
        "source_file": "only.py",
    }));
    let mut stub = node("sourceless_ast_stub", "", true);
    stub.source_location = None;
    before.nodes.push(stub);
    write_graph(temp.path(), &before);
    fs::remove_file(temp.path().join("only.py")).unwrap();
    let mut opts = options(temp.path(), true);
    if incremental {
        opts.changed_paths = Some(vec!["only.py".into()]);
    }
    assert!(rebuild_project(temp.path(), &opts).unwrap().succeeded());
    let after = graph(temp.path());
    assert!(!sources(&after).contains("only.py"));
    assert!(ids(&after).is_superset(&BTreeSet::from([
        "docs_topic".into(),
        "shared_concept".into()
    ])));
    assert!(!ids(&after).contains("sourceless_ast_stub"));
    assert_eq!(
        after
            .hyperedges
            .iter()
            .filter_map(|value| value.get("id").and_then(Value::as_str))
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["semantic_context"])
    );
}

#[test]
fn test_rebuild_code_prunes_final_deleted_file_full_update() {
    run_prune_final_file_case(false);
}

#[test]
fn test_rebuild_code_prunes_final_deleted_file_incremental_update() {
    run_prune_final_file_case(true);
}

#[test]
fn test_rebuild_code_prunes_renamed_source_not_listed_by_hook() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("old.py"), "def old_fn():\n    return 1\n");
    assert!(build(temp.path(), true).succeeded());
    let mut seeded = graph(temp.path());
    semantic_pair(&mut seeded, "");
    write_graph(temp.path(), &seeded);
    fs::rename(temp.path().join("old.py"), temp.path().join("renamed.py")).unwrap();
    let mut opts = options(temp.path(), true);
    opts.changed_paths = Some(vec!["renamed.py".into()]);
    assert!(rebuild_project(temp.path(), &opts).unwrap().succeeded());
    let after = graph(temp.path());
    assert!(!sources(&after).contains("old.py"));
    assert!(sources(&after).contains("renamed.py"));
    assert!(ids(&after).contains("docs_topic"));
}

#[test]
fn test_rebuild_code_normalizes_preserved_source_paths() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("foo.py"), "def foo_fn():\n    return 1\n");
    write(temp.path().join("bar.py"), "def bar_fn():\n    return 1\n");
    assert!(build(temp.path(), true).succeeded());
    let mut seeded = graph(temp.path());
    for node in &mut seeded.nodes {
        if node.source_file == "foo.py" {
            node.source_file = "./foo.py".into();
        }
    }
    for edge in &mut seeded.links {
        if edge.source_file == "foo.py" {
            edge.source_file = "./foo.py".into();
        }
    }
    write_graph(temp.path(), &seeded);
    write(
        temp.path().join("bar.py"),
        "def updated_bar_fn():\n    return 2\n",
    );
    let mut opts = options(temp.path(), true);
    opts.changed_paths = Some(vec!["bar.py".into()]);
    assert!(rebuild_project(temp.path(), &opts).unwrap().succeeded());
    assert!(labels(&graph(temp.path()))
        .iter()
        .any(|label| label.contains("foo_fn")));
}

#[test]
fn test_rebuild_code_prunes_renamed_ast_backed_document() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("old.md"), "# Old heading\n");
    assert!(build(temp.path(), true).succeeded());
    fs::rename(temp.path().join("old.md"), temp.path().join("renamed.md")).unwrap();
    let mut opts = options(temp.path(), true);
    opts.changed_paths = Some(vec!["renamed.md".into()]);
    assert!(rebuild_project(temp.path(), &opts).unwrap().succeeded());
    let source = sources(&graph(temp.path()));
    assert!(!source.contains("old.md"));
    assert!(source.contains("renamed.md"));
}

#[test]
fn test_rebuild_code_evicts_removed_symbol_from_surviving_file() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path().join("a.py"),
        "def foo():\n    pass\n\ndef bar():\n    pass\n",
    );
    write(
        temp.path().join("b.py"),
        "from a import foo\n\ndef caller():\n    foo()\n",
    );
    assert!(build(temp.path(), true).succeeded());
    let mut seeded = graph(temp.path());
    let mut semantic = node("a_authconcept", "a.py", false);
    semantic.label = "AuthConcept".into();
    seeded.nodes.push(semantic);
    write_graph(temp.path(), &seeded);
    write(temp.path().join("a.py"), "def bar():\n    pass\n");
    assert!(build(temp.path(), true).succeeded());
    let after = labels(&graph(temp.path()));
    assert!(!after.iter().any(|label| label == "foo()"));
    assert!(after.iter().any(|label| label == "bar()"));
    assert!(after.contains("AuthConcept"));
}

#[test]
fn test_rebuild_code_preupgrade_marker_less_node_one_cycle_lag() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("a.py"), "def bar():\n    pass\n");
    assert!(build(temp.path(), true).succeeded());
    let mut seeded = graph(temp.path());
    for node in &mut seeded.nodes {
        node.extra.remove("_origin");
    }
    let mut stale = node("a_foo", "a.py", false);
    stale.label = "foo()".into();
    stale.file_type = "code".into();
    seeded.nodes.push(stale);
    write_graph(temp.path(), &seeded);
    let mut forced = options(temp.path(), true);
    forced.force = true;
    assert!(rebuild_project(temp.path(), &forced).unwrap().succeeded());
    let mut after = graph(temp.path());
    assert!(labels(&after).contains("foo()"));
    after
        .nodes
        .iter_mut()
        .find(|node| node.label == "foo()")
        .unwrap()
        .extra
        .insert("_origin".into(), json!("ast"));
    write_graph(temp.path(), &after);
    assert!(rebuild_project(temp.path(), &forced).unwrap().succeeded());
    let healed = labels(&graph(temp.path()));
    assert!(!healed.contains("foo()"));
    assert!(healed.contains("bar()"));
}

#[test]
fn test_rebuild_lock_non_blocking_does_not_clobber_holder() {
    let temp = tempfile::tempdir().unwrap();
    let outer = RebuildLockGuard::acquire(temp.path(), false)
        .unwrap()
        .unwrap();
    let held = fs::read_to_string(outer.path()).unwrap();
    assert!(RebuildLockGuard::acquire(temp.path(), false)
        .unwrap()
        .is_none());
    assert_eq!(fs::read_to_string(outer.path()).unwrap(), held);
}

#[test]
fn test_rebuild_code_is_idempotent_when_cluster_ids_flap() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path().join("app.py"),
        "def alpha():\n    return 1\n\ndef beta():\n    return alpha()\n",
    );
    assert!(build(temp.path(), false).succeeded());
    let graph_file = graph_path(temp.path());
    let report_file = temp.path().join(OUTPUT_DIRECTORY).join("GRAPH_REPORT.md");
    let first_graph = fs::read(&graph_file).unwrap();
    let first_report = fs::read(&report_file).unwrap();
    let mut flapped = graph(temp.path());
    for node in &mut flapped.nodes {
        node.community = node.community.map(|community| community + 100);
    }
    assert!(same_topology(&graph(temp.path()), &flapped));
    assert!(build(temp.path(), false).succeeded());
    assert_eq!(fs::read(graph_file).unwrap(), first_graph);
    assert_eq!(fs::read(report_file).unwrap(), first_report);
}

#[test]
fn test_rebuild_code_skips_cluster_when_topology_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path().join("app.py"),
        "def alpha():\n    return 1\n\ndef beta():\n    return alpha()\n",
    );
    assert_eq!(build(temp.path(), false).status, RebuildStatus::Rebuilt);
    assert_eq!(build(temp.path(), false).status, RebuildStatus::Unchanged);
}

#[test]
fn test_manual_incremental_no_change_reports_unchanged_with_telemetry() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("a.py"), "def alpha():\n    return 1\n");
    write(temp.path().join("b.py"), "def beta():\n    return 2\n");
    assert_eq!(build(temp.path(), true).status, RebuildStatus::Rebuilt);

    let mut opts = options(temp.path(), true);
    opts.scope = RebuildScope::Incremental;
    let result = rebuild_project(temp.path(), &opts).unwrap();

    assert_eq!(result.status, RebuildStatus::Unchanged);
    assert_eq!(result.scope, RebuildScope::Incremental);
    assert_eq!(result.stats.detected_files, 2);
    assert_eq!(result.stats.processed_files, 0);
    assert_eq!(result.stats.changed_files, 0);
    assert_eq!(result.stats.unchanged_files, 2);
    assert_eq!(result.stats.deleted_files, 0);
    assert_eq!(result.stats.nodes, graph(temp.path()).nodes.len());
    assert_eq!(result.stats.edges, graph(temp.path()).links.len());
    assert!(result.timings.total_ms >= result.timings.detect_ms);
    let timings = serde_json::to_value(&result.timings).unwrap();
    for field in [
        "detect_ms",
        "extract_ms",
        "build_ms",
        "cluster_ms",
        "write_ms",
        "total_ms",
    ] {
        assert!(timings[field].as_u64().is_some(), "missing integer {field}");
    }
}

#[test]
fn test_manual_incremental_processes_only_the_changed_file() {
    let temp = tempfile::tempdir().unwrap();
    let changed = temp.path().join("a.py");
    write(&changed, "def alpha():\n    return 1\n");
    write(temp.path().join("b.py"), "def beta():\n    return 2\n");
    assert!(build(temp.path(), true).succeeded());
    rewrite_with_new_mtime(
        &changed,
        "def alpha():\n    return 1\n\ndef added():\n    return alpha()\n",
    );

    let mut opts = options(temp.path(), true);
    opts.scope = RebuildScope::Incremental;
    let result = rebuild_project(temp.path(), &opts).unwrap();

    assert_eq!(result.status, RebuildStatus::Rebuilt);
    assert_eq!(result.scope, RebuildScope::Incremental);
    assert_eq!(result.stats.detected_files, 2);
    assert_eq!(result.stats.processed_files, 1);
    assert_eq!(result.stats.changed_files, 1);
    assert_eq!(result.stats.unchanged_files, 1);
    assert_eq!(result.stats.deleted_files, 0);
    assert!(sources(&graph(temp.path())).contains("b.py"));
}

#[test]
fn test_manual_incremental_prunes_a_deleted_file_without_reextracting_unchanged_files() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("keep.py"), "def keep():\n    return 1\n");
    write(temp.path().join("drop.py"), "def drop():\n    return 2\n");
    assert!(build(temp.path(), true).succeeded());
    fs::remove_file(temp.path().join("drop.py")).unwrap();

    let mut opts = options(temp.path(), true);
    opts.scope = RebuildScope::Incremental;
    let result = rebuild_project(temp.path(), &opts).unwrap();

    assert_eq!(result.status, RebuildStatus::Rebuilt);
    assert_eq!(result.scope, RebuildScope::Incremental);
    assert_eq!(result.stats.detected_files, 1);
    assert_eq!(result.stats.processed_files, 0);
    assert_eq!(result.stats.changed_files, 0);
    assert_eq!(result.stats.unchanged_files, 1);
    assert_eq!(result.stats.deleted_files, 1);
    assert!(!sources(&graph(temp.path())).contains("drop.py"));
}

#[test]
fn test_manual_incremental_without_a_baseline_falls_back_to_full() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("app.py"), "def app():\n    return 1\n");
    let mut opts = options(temp.path(), true);
    opts.scope = RebuildScope::Incremental;

    let result = rebuild_project(temp.path(), &opts).unwrap();

    assert_eq!(result.status, RebuildStatus::Rebuilt);
    assert_eq!(result.scope, RebuildScope::Full);
    assert_eq!(result.stats.detected_files, 1);
    assert_eq!(result.stats.processed_files, 1);
    assert!(result.stats.nodes > 0);
}

#[test]
fn test_explicit_watch_change_without_a_baseline_builds_the_full_corpus() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("a.py"), "def alpha():\n    return 1\n");
    write(temp.path().join("b.py"), "def beta():\n    return 2\n");
    let mut opts = options(temp.path(), true);
    opts.changed_paths = Some(vec![PathBuf::from("a.py")]);

    let result = rebuild_project(temp.path(), &opts).unwrap();

    assert_eq!(result.status, RebuildStatus::Rebuilt);
    assert_eq!(result.scope, RebuildScope::Full);
    assert_eq!(result.stats.detected_files, 2);
    assert_eq!(result.stats.processed_files, 2);
    let source = sources(&graph(temp.path()));
    assert!(source.contains("a.py"));
    assert!(source.contains("b.py"));
}

#[test]
fn test_watch_cli_rejects_an_ancestor_output_before_reporting_readiness() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    write(root.join("app.py"), "def app():\n    return 1\n");

    let output = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .arg("watch")
        .arg(&root)
        .env("GRAPHOXIDE_OUT", temp.path())
        .env_remove("GRAPHIFY_OUT")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Watching"));
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("watched project root or one of its ancestors"));
    assert!(!temp.path().join(NEEDS_UPDATE).exists());
}

#[test]
fn test_watch_handler_honors_graphifyignore() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(".hidden-parent/corpus");
    write(root.join(".graphifyignore"), "node_modules/\nbuild/\n");
    fs::create_dir_all(root.join("node_modules")).unwrap();
    fs::create_dir_all(root.join("build")).unwrap();
    write(root.join("app.py"), "def f():\n    return 1\n");
    let filter = WatchEventFilter::new(&root, true);
    assert!(!filter.accepts(&root.join("node_modules/junk.js"), false));
    assert!(!filter.accepts(&root.join("build/out.py"), false));
    assert!(filter.accepts(&root.join("app.py"), false));
}

#[test]
fn test_watch_handler_ignores_a_custom_output_directory() {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("custom-output");
    let filter = WatchEventFilter::with_output_directory(temp.path(), true, Some(&output));

    assert!(!filter.accepts(&output.join("graph.json"), false));
    assert!(!filter.accepts(&output.join("manifest.json"), false));
    assert!(filter.accepts(&temp.path().join("app.py"), false));
}

#[test]
fn test_watch_handler_does_not_ignore_sources_when_output_is_an_ancestor() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    write(root.join("app.py"), "def app():\n    return 1\n");
    let filter = WatchEventFilter::with_output_directory(&root, true, Some(temp.path()));

    assert!(filter.accepts(&root.join("app.py"), false));
}

#[test]
fn test_rebuild_rejects_output_at_or_above_the_watch_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    write(root.join("app.py"), "def app():\n    return 1\n");

    for output in [root.clone(), temp.path().to_path_buf()] {
        let mut opts = options(&root, true);
        opts.output_directory = Some(output);
        let error = rebuild_project(&root, &opts).unwrap_err().to_string();

        assert!(error.contains("watched project root or one of its ancestors"));
    }
    assert!(!root.join("graph.json").exists());
    assert!(!temp.path().join("graph.json").exists());
}

#[test]
fn test_watch_loads_graphifyignore_once() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join(".graphifyignore"), "ignored/\n");
    fs::create_dir_all(temp.path().join("ignored")).unwrap();
    let filter = WatchEventFilter::new(temp.path(), true);
    write(
        temp.path().join(".graphifyignore"),
        "ignored/\nnewly_ignored/\n",
    );
    write(temp.path().join("newly_ignored/f.py"), "x = 1\n");
    for index in 0..50 {
        assert!(!filter.accepts(&temp.path().join(format!("ignored/f{index}.py")), false));
    }
    assert!(filter.accepts(&temp.path().join("newly_ignored/f.py"), false));
}

fn shrink_graph(count: usize, source: &str) -> KnowledgeGraph {
    KnowledgeGraph {
        nodes: (0..count)
            .map(|index| node(&format!("n{index}"), source, false))
            .collect(),
        ..Default::default()
    }
}

#[test]
fn test_check_shrink_blocks_silent_shrink() {
    let existing = shrink_graph(100, "old.py");
    let candidate = shrink_graph(80, "old.py");
    let decision = check_shrink(false, Some(&existing), &candidate, None, false, None, None);
    assert!(!decision.allowed);
    let warning = decision.warning.unwrap();
    assert!(warning.contains("80 nodes") && warning.contains("100"));
}

#[test]
fn test_check_shrink_allows_force_override() {
    assert!(
        check_shrink(
            true,
            Some(&shrink_graph(100, "old.py")),
            &shrink_graph(1, "old.py"),
            None,
            false,
            None,
            None,
        )
        .allowed
    );
}

#[test]
fn test_check_shrink_allows_explicit_deletions() {
    let decision = check_shrink(
        false,
        Some(&shrink_graph(100, "old.py")),
        &shrink_graph(80, "old.py"),
        None,
        true,
        None,
        None,
    );
    assert!(decision.allowed);
    assert!(decision.warning.is_none());
}

#[test]
fn test_check_shrink_allows_no_existing_data() {
    assert!(
        check_shrink(
            false,
            None,
            &shrink_graph(50, "a.py"),
            None,
            false,
            None,
            None,
        )
        .allowed
    );
}

#[test]
fn test_check_shrink_allows_shrink_within_rebuilt_sources() {
    let existing = KnowledgeGraph {
        nodes: vec![
            node("a", "m.py", false),
            node("b", "m.py", false),
            node("c", "other.py", false),
        ],
        ..KnowledgeGraph::default()
    };
    let candidate = KnowledgeGraph {
        nodes: vec![node("a", "m.py", false), node("c", "other.py", false)],
        ..KnowledgeGraph::default()
    };
    assert!(
        check_shrink(
            false,
            Some(&existing),
            &candidate,
            None,
            false,
            Some(&BTreeSet::from(["m.py".into()])),
            None,
        )
        .allowed
    );
}

#[test]
fn test_check_shrink_blocks_shrink_outside_rebuilt_sources() {
    let existing = KnowledgeGraph {
        nodes: vec![node("a", "m.py", false), node("z", "untouched.py", false)],
        ..KnowledgeGraph::default()
    };
    let candidate = KnowledgeGraph {
        nodes: vec![node("a", "m.py", false)],
        ..KnowledgeGraph::default()
    };
    assert!(
        !check_shrink(
            false,
            Some(&existing),
            &candidate,
            None,
            false,
            Some(&BTreeSet::from(["m.py".into()])),
            None,
        )
        .allowed
    );
}

#[test]
fn test_check_shrink_allows_growth() {
    assert!(
        check_shrink(
            false,
            Some(&shrink_graph(50, "a.py")),
            &shrink_graph(60, "a.py"),
            None,
            false,
            None,
            None,
        )
        .allowed
    );
}

#[test]
fn test_check_shrink_unlinks_tmp_on_refuse() {
    let temp = tempfile::tempdir().unwrap();
    let temporary = temp.path().join("graph.tmp.json");
    write(&temporary, "{}");
    assert!(
        !check_shrink(
            false,
            Some(&shrink_graph(100, "a.py")),
            &shrink_graph(80, "a.py"),
            Some(&temporary),
            false,
            None,
            None,
        )
        .allowed
    );
    assert!(!temporary.exists());
}

#[test]
fn test_check_shrink_keeps_tmp_when_deletions_declared() {
    let temp = tempfile::tempdir().unwrap();
    let temporary = temp.path().join("graph.tmp.json");
    write(&temporary, "{}");
    assert!(
        check_shrink(
            false,
            Some(&shrink_graph(100, "a.py")),
            &shrink_graph(80, "a.py"),
            Some(&temporary),
            true,
            None,
            None,
        )
        .allowed
    );
    assert!(temporary.exists());
}

#[test]
fn test_rebuild_code_prunes_deleted_file_nodes() {
    let temp = tempfile::tempdir().unwrap();
    seed_two_code_files(temp.path());
    fs::remove_file(temp.path().join("drop.py")).unwrap();
    let mut opts = options(temp.path(), true);
    opts.acquire_lock = true;
    opts.changed_paths = Some(vec!["drop.py".into()]);
    assert!(rebuild_project(temp.path(), &opts).unwrap().succeeded());
    let source = sources(&graph(temp.path()));
    assert!(!source.contains("drop.py"));
    assert!(source.contains("keep.py"));
}

#[test]
fn test_rebuild_code_accepts_repo_relative_changed_path_for_subdir_root() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path().join("src/app.py"),
        "def old_name():\n    return 1\n",
    );
    assert!(build_from(temp.path(), Path::new("src"), true).succeeded());
    write(
        temp.path().join("src/app.py"),
        "def new_name():\n    return 2\n",
    );
    let mut opts = options(temp.path(), true);
    opts.changed_paths = Some(vec!["src/app.py".into()]);
    opts.force = true;
    assert!(rebuild_project(Path::new("src"), &opts)
        .unwrap()
        .succeeded());
    let after = labels(&graph(&temp.path().join("src")));
    assert!(!after.contains("old_name()"));
    assert!(after.contains("new_name()"));
}

fn run_subdir_preserves_outside_case(incremental: bool) {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path().join("app.py"),
        "def outside_fn():\n    return 2\n",
    );
    write(
        temp.path().join("src/app.py"),
        "def inside_fn():\n    return 1\n",
    );
    assert!(build_from(temp.path(), Path::new("src"), true).succeeded());
    let src = temp.path().join("src");
    let mut seeded = graph(&src);
    let inside = seeded
        .nodes
        .iter()
        .find(|node| node.label == "inside_fn()")
        .unwrap()
        .id
        .clone();
    seeded.nodes.push(node("outside_ast", "app.py", true));
    seeded
        .nodes
        .push(node("stale_inside_ast", "src/deleted.py", true));
    seeded
        .links
        .push(edge("outside_ast", &inside, "calls", "app.py", true));
    write_graph(&src, &seeded);
    let mut opts = options(temp.path(), true);
    if incremental {
        opts.changed_paths = Some(vec!["src/app.py".into()]);
    }
    assert!(rebuild_project(Path::new("src"), &opts)
        .unwrap()
        .succeeded());
    let after = graph(&src);
    assert!(ids(&after).contains("outside_ast"));
    assert!(!ids(&after).contains("stale_inside_ast"));
    assert_eq!(
        after
            .nodes
            .iter()
            .find(|node| node.id == "outside_ast")
            .unwrap()
            .source_file,
        "app.py"
    );
}

#[test]
fn test_rebuild_code_subdir_preserves_outside_ast_nodes_full_update() {
    run_subdir_preserves_outside_case(false);
}

#[test]
fn test_rebuild_code_subdir_preserves_outside_ast_nodes_incremental_update() {
    run_subdir_preserves_outside_case(true);
}

#[test]
fn test_rebuild_code_subdir_survives_absolute_to_relative_invocation() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    write(src.join("old.py"), "def old_fn():\n    return 1\n");
    assert!(build(&src, true).succeeded());
    let mut seeded = graph(&src);
    seeded.nodes.push(node("local_semantic", "old.py", false));
    write_graph(&src, &seeded);
    assert!(build_from(temp.path(), Path::new("src"), true).succeeded());
    assert_eq!(
        graph(&src)
            .nodes
            .iter()
            .find(|node| node.id == "local_semantic")
            .unwrap()
            .source_file,
        "src/old.py"
    );
    fs::rename(src.join("old.py"), src.join("renamed.py")).unwrap();
    assert!(build_from(temp.path(), Path::new("src"), true).succeeded());
    let source = sources(&graph(&src));
    assert!(!source.contains("old.py"));
    assert!(!source.contains("src/old.py"));
    assert!(source.contains("src/renamed.py"));
}

#[test]
fn test_rebuild_code_prunes_legacy_watch_relative_subdir_source() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    write(src.join("old.py"), "def old_fn():\n    return 1\n");
    assert!(build_from(temp.path(), Path::new("src"), true).succeeded());
    let mut seeded = graph(&src);
    for node in &mut seeded.nodes {
        node.source_file = node.source_file.trim_start_matches("src/").into();
    }
    for edge in &mut seeded.links {
        edge.source_file = edge.source_file.trim_start_matches("src/").into();
    }
    write_graph(&src, &seeded);
    fs::rename(src.join("old.py"), src.join("renamed.py")).unwrap();
    let mut opts = options(temp.path(), true);
    opts.changed_paths = Some(vec!["src/renamed.py".into()]);
    assert!(rebuild_project(Path::new("src"), &opts)
        .unwrap()
        .succeeded());
    let source = sources(&graph(&src));
    assert!(!source.contains("old.py"));
    assert!(source.contains("src/renamed.py"));
}

#[test]
fn test_rebuild_code_does_not_update_root_marker_when_write_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let out = temp.path().join(OUTPUT_DIRECTORY);
    fs::create_dir_all(&out).unwrap();
    let existing = shrink_graph(2, "untouched.py");
    write_graph(temp.path(), &existing);
    write(
        out.join(COMPAT_ROOT_MARKER),
        temp.path().to_string_lossy().as_ref(),
    );
    let candidate = shrink_graph(1, "other.py");
    assert!(!commit_candidate(
        &out,
        Some(&existing),
        &candidate,
        &CommitOptions {
            force: false,
            had_explicit_deletions: false,
            rebuilt_sources: Some(BTreeSet::from(["other.py".into()])),
            source_root: Some(temp.path().to_path_buf()),
            marker_value: ".".into(),
        },
    )
    .unwrap());
    assert_eq!(
        fs::read_to_string(out.join(COMPAT_ROOT_MARKER)).unwrap(),
        temp.path().to_string_lossy()
    );
}

#[cfg(unix)]
#[test]
fn test_rebuild_code_incremental_rename_preserves_symlink_source_path() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real");
    fs::create_dir_all(&real).unwrap();
    write(temp.path().join(".graphifyignore"), "real/\n");
    write(real.join("old.py"), "def linked_fn():\n    return 1\n");
    symlink(&real, temp.path().join("linked")).unwrap();
    let mut opts = options(temp.path(), true);
    opts.follow_symlinks = true;
    assert!(rebuild_project(temp.path(), &opts).unwrap().succeeded());
    fs::rename(real.join("old.py"), real.join("first.py")).unwrap();
    opts.changed_paths = Some(vec!["linked/first.py".into()]);
    assert!(rebuild_project(temp.path(), &opts).unwrap().succeeded());
    fs::rename(real.join("first.py"), real.join("second.py")).unwrap();
    opts.changed_paths = Some(vec!["linked/second.py".into()]);
    assert!(rebuild_project(temp.path(), &opts).unwrap().succeeded());
    let source = sources(&graph(temp.path()));
    assert!(!source.contains("linked/old.py"));
    assert!(!source.contains("linked/first.py"));
    assert!(source.contains("linked/second.py"));
}

#[test]
fn test_queue_and_drain_pending_round_trip() {
    let temp = tempfile::tempdir().unwrap();
    let paths = vec!["a.py".into(), "sub/b.py".into(), "c.md".into()];
    queue_pending(temp.path(), &paths).unwrap();
    assert_eq!(
        fs::read_to_string(temp.path().join(PENDING_CHANGES))
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        vec!["a.py", "sub/b.py", "c.md"]
    );
    assert_eq!(drain_pending(temp.path()).unwrap(), paths);
    assert!(!temp.path().join(PENDING_CHANGES).exists());
    assert!(drain_pending(temp.path()).unwrap().is_empty());
}

#[test]
fn test_pending_queue_and_drain_obey_stable_journal_lock() {
    use fs2::FileExt as _;
    use std::{
        sync::{mpsc, Arc, Barrier},
        thread,
        time::Duration,
    };

    let temp = tempfile::tempdir().unwrap();
    let lock_path = temp.path().join(PENDING_CHANGES_LOCK);
    let journal_lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    journal_lock.lock_exclusive().unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);
    let out = temp.path().to_path_buf();
    let (done_tx, done_rx) = mpsc::channel();
    let writer = thread::spawn(move || {
        worker_barrier.wait();
        done_tx
            .send(queue_pending(&out, &[PathBuf::from("late.py")]))
            .unwrap();
    });
    barrier.wait();
    assert!(done_rx.recv_timeout(Duration::from_millis(150)).is_err());
    fs2::FileExt::unlock(&journal_lock).unwrap();
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    writer.join().unwrap();

    journal_lock.lock_exclusive().unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);
    let out = temp.path().to_path_buf();
    let (done_tx, done_rx) = mpsc::channel();
    let drainer = thread::spawn(move || {
        worker_barrier.wait();
        done_tx.send(drain_pending(&out)).unwrap();
    });
    barrier.wait();
    assert!(done_rx.recv_timeout(Duration::from_millis(150)).is_err());
    fs2::FileExt::unlock(&journal_lock).unwrap();
    assert_eq!(
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap(),
        vec![PathBuf::from("late.py")]
    );
    drainer.join().unwrap();
    assert!(lock_path.is_file());
}

#[test]
fn test_deferred_target_extraction_does_not_publish_subset_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("app.py");
    write(&source, "def app():\n    return 1\n");
    let manifest = temp.path().join(OUTPUT_DIRECTORY).join("manifest.json");
    let previous = br#"{"old.py":{"mtime":1.0,"ast_hash":"old","semantic_hash":"semantic"}}"#;
    write(&manifest, std::str::from_utf8(previous).unwrap());

    graphoxide_extract::extract_files_deferred_manifest(
        std::slice::from_ref(&source),
        Some(temp.path()),
        true,
    )
    .unwrap()
    .discard_manifest();
    assert_eq!(fs::read(&manifest).unwrap(), previous);

    graphoxide_extract::extract_files_deferred_manifest(&[source], Some(temp.path()), true)
        .unwrap()
        .commit_manifest()
        .unwrap();
    let committed: Value = serde_json::from_slice(&fs::read(manifest).unwrap()).unwrap();
    assert!(committed.get("app.py").is_some());
    assert!(committed.get("old.py").is_none());
}

#[test]
#[cfg(unix)]
fn test_rebuild_walk_error_preserves_graph_and_manifest() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("app.py"), "def app():\n    return 1\n");
    let locked = temp.path().join("locked");
    write(locked.join("hidden.py"), "def hidden():\n    return 2\n");
    assert!(build(temp.path(), true).succeeded());
    let graph_path = graph_path(temp.path());
    let manifest_path = temp.path().join(OUTPUT_DIRECTORY).join("manifest.json");
    let graph_before = fs::read(&graph_path).unwrap();
    let manifest_before = fs::read(&manifest_path).unwrap();

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&locked).is_ok() {
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let result = rebuild_project(temp.path(), &options(temp.path(), true));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    let error = result.unwrap_err().to_string();
    assert!(error.contains("incomplete filesystem scan"), "{error}");
    assert!(error.contains("locked"), "{error}");
    assert_eq!(fs::read(graph_path).unwrap(), graph_before);
    assert_eq!(fs::read(manifest_path).unwrap(), manifest_before);
}

#[test]
fn test_drain_pending_dedupes_and_skips_blank_lines() {
    let temp = tempfile::tempdir().unwrap();
    queue_pending(temp.path(), &["a.py".into(), "b.py".into()]).unwrap();
    queue_pending(temp.path(), &["b.py".into(), "c.py".into()]).unwrap();
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(temp.path().join(PENDING_CHANGES))
        .unwrap();
    writeln!(file, "\n   ").unwrap();
    assert_eq!(
        drain_pending(temp.path()).unwrap(),
        vec![
            PathBuf::from("a.py"),
            PathBuf::from("b.py"),
            PathBuf::from("c.py")
        ]
    );
}

#[test]
fn test_queue_pending_noop_on_empty_list() {
    let temp = tempfile::tempdir().unwrap();
    queue_pending(temp.path(), &[]).unwrap();
    assert!(!temp.path().join(PENDING_CHANGES).exists());
}

#[test]
fn test_rebuild_code_queues_on_lock_contention() {
    let temp = tempfile::tempdir().unwrap();
    let out = temp.path().join(OUTPUT_DIRECTORY);
    let _holder = RebuildLockGuard::acquire(&out, false).unwrap().unwrap();
    let mut opts = options(temp.path(), true);
    opts.acquire_lock = true;
    opts.changed_paths = Some(vec!["a.py".into(), "b.py".into()]);
    let result = rebuild_project(temp.path(), &opts).unwrap();
    assert_eq!(result.status, RebuildStatus::Queued);
    assert_eq!(result.scope, RebuildScope::Incremental);
    assert_eq!(result.stats, RebuildStats::default());
    assert!(result.timings.total_ms >= result.timings.detect_ms);
    assert_eq!(
        fs::read_to_string(out.join(PENDING_CHANGES))
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        vec!["a.py", "b.py"]
    );
}

#[test]
fn test_external_executor_preserves_queued_lock_and_pending_journal() {
    let temp = tempfile::tempdir().unwrap();
    let out = temp.path().join(OUTPUT_DIRECTORY);
    let _holder = RebuildLockGuard::acquire(&out, false).unwrap().unwrap();
    let mut opts = options(temp.path(), true);
    opts.acquire_lock = true;
    opts.changed_paths = Some(vec!["a.py".into()]);
    let result = rebuild_project_with_executor(temp.path(), &opts, |_| {
        panic!("a queued rebuild must not invoke the executor")
    })
    .unwrap();
    assert_eq!(result.status, RebuildStatus::Queued);
    assert_eq!(
        fs::read_to_string(out.join(PENDING_CHANGES))
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        vec!["a.py"]
    );
}

#[test]
fn test_external_executor_receives_merged_pending_paths_and_drains_late_arrivals() {
    let temp = tempfile::tempdir().unwrap();
    let out = temp.path().join(OUTPUT_DIRECTORY);
    queue_pending(&out, &["queued.py".into()]).unwrap();
    let mut opts = options(temp.path(), true);
    opts.acquire_lock = true;
    opts.changed_paths = Some(vec!["own.py".into()]);
    let passes = RefCell::new(Vec::new());
    let result = rebuild_project_with_executor(temp.path(), &opts, |request| {
        let changed = request.changed_paths.clone().unwrap_or_default();
        passes.borrow_mut().push((request.pass, changed));
        if request.pass == 1 {
            queue_pending(&request.output_directory, &["late.py".into()]).unwrap();
        }
        Ok(RebuildResult {
            status: RebuildStatus::Rebuilt,
            scope: request.scope,
            graph_path: request.output_directory.join("graph.json"),
            manifest_path: request.output_directory.join("manifest.json"),
            passes: request.pass,
            clustered: false,
            warnings: Vec::new(),
            stats: RebuildStats {
                detected_files: 3,
                processed_files: 1,
                changed_files: 1,
                unchanged_files: 2,
                deleted_files: 0,
                nodes: request.pass,
                edges: request.pass,
            },
            timings: RebuildTimings::default(),
        })
    })
    .unwrap();
    assert_eq!(result.status, RebuildStatus::Rebuilt);
    assert_eq!(result.passes, 2);
    assert_eq!(
        passes.into_inner(),
        vec![
            (1, vec![PathBuf::from("own.py"), PathBuf::from("queued.py")]),
            (2, vec![PathBuf::from("late.py")]),
        ]
    );
    assert!(!out.join(PENDING_CHANGES).exists());
}

#[test]
fn test_rebuild_code_merges_pending_on_acquire() {
    let temp = tempfile::tempdir().unwrap();
    for name in ["own.py", "queued1.py", "queued2.py"] {
        write(
            temp.path().join(name),
            &format!("def {}():\n    return 1\n", name.trim_end_matches(".py")),
        );
    }
    let out = temp.path().join(OUTPUT_DIRECTORY);
    queue_pending(&out, &["queued1.py".into(), "queued2.py".into()]).unwrap();
    let mut opts = options(temp.path(), true);
    opts.acquire_lock = true;
    opts.changed_paths = Some(vec!["own.py".into(), "queued1.py".into()]);
    assert!(rebuild_project(temp.path(), &opts).unwrap().succeeded());
    let source = sources(&graph(temp.path()));
    assert!(source.is_superset(&BTreeSet::from([
        "own.py".into(),
        "queued1.py".into(),
        "queued2.py".into()
    ])));
    assert!(!out.join(PENDING_CHANGES).exists());
}

#[test]
fn test_rebuild_code_drains_late_arrivals() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("own.py"), "def own():\n    return 1\n");
    write(temp.path().join("late.py"), "def late():\n    return 2\n");
    assert!(build(temp.path(), true).succeeded());
    write(
        temp.path().join("own.py"),
        "def own():\n    return 1\n\ndef own_added():\n    return own()\n",
    );
    write(
        temp.path().join("late.py"),
        "def late():\n    return 2\n\ndef late_added():\n    return late()\n",
    );
    let mut opts = options(temp.path(), true);
    opts.acquire_lock = true;
    opts.changed_paths = Some(vec!["own.py".into()]);
    let result = rebuild_project_with_observer(temp.path(), &opts, |pass, out| {
        if pass == 1 {
            queue_pending(out, &["late.py".into()]).unwrap();
        }
    })
    .unwrap();
    assert_eq!(result.passes, 2);
    assert_eq!(result.scope, RebuildScope::Incremental);
    assert_eq!(result.stats.detected_files, 2);
    assert_eq!(result.stats.processed_files, 2);
    assert_eq!(result.stats.changed_files, 2);
    assert_eq!(result.stats.unchanged_files, 0);
    assert_eq!(result.status, RebuildStatus::Rebuilt);
    assert!(result.stats.nodes > 0);
    assert!(result.timings.total_ms >= result.timings.extract_ms);
    let source = sources(&graph(temp.path()));
    assert!(source.contains("own.py") && source.contains("late.py"));
}

#[test]
fn test_rebuild_code_deduplicates_file_stats_across_passes() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("own.py"), "def own():\n    return 1\n");
    write(
        temp.path().join("steady.py"),
        "def steady():\n    return 2\n",
    );
    assert!(build(temp.path(), true).succeeded());
    write(
        temp.path().join("own.py"),
        "def own():\n    return 1\n\ndef added():\n    return own()\n",
    );

    let mut opts = options(temp.path(), true);
    opts.acquire_lock = true;
    opts.changed_paths = Some(vec![PathBuf::from("own.py")]);
    let result = rebuild_project_with_observer(temp.path(), &opts, |pass, out| {
        if pass == 1 {
            queue_pending(out, &[PathBuf::from("own.py")]).unwrap();
        }
    })
    .unwrap();

    assert_eq!(result.passes, 2);
    assert_eq!(result.status, RebuildStatus::Rebuilt);
    assert_eq!(result.stats.detected_files, 2);
    assert_eq!(result.stats.processed_files, 1);
    assert_eq!(result.stats.changed_files, 1);
    assert_eq!(result.stats.unchanged_files, 1);
}

#[test]
fn test_late_irrelevant_pass_does_not_mask_a_completed_rebuild() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("own.py"), "def own():\n    return 1\n");
    write(temp.path().join("notes.txt"), "not structurally indexed\n");
    assert!(build(temp.path(), true).succeeded());
    write(
        temp.path().join("own.py"),
        "def own():\n    return 1\n\ndef own_added():\n    return own()\n",
    );
    let mut opts = options(temp.path(), true);
    opts.acquire_lock = true;
    opts.changed_paths = Some(vec![PathBuf::from("own.py")]);

    let result = rebuild_project_with_observer(temp.path(), &opts, |pass, out| {
        if pass == 1 {
            queue_pending(out, &[PathBuf::from("notes.txt")]).unwrap();
        }
    })
    .unwrap();

    assert_eq!(result.passes, 2);
    assert_eq!(result.status, RebuildStatus::Rebuilt);
    assert_eq!(result.stats.detected_files, 1);
    assert_eq!(result.stats.processed_files, 1);
    assert_eq!(result.stats.changed_files, 1);
    assert_eq!(result.stats.unchanged_files, 0);
    assert_eq!(result.stats.deleted_files, 0);
}

#[test]
fn test_rebuild_result_serde_round_trip_excludes_private_aggregation_state() {
    let result = RebuildResult {
        status: RebuildStatus::Rebuilt,
        scope: RebuildScope::Incremental,
        graph_path: PathBuf::from("out/graph.json"),
        manifest_path: PathBuf::from("out/manifest.json"),
        passes: 2,
        clustered: true,
        warnings: vec!["example warning".into()],
        stats: RebuildStats {
            detected_files: 2,
            processed_files: 1,
            changed_files: 1,
            unchanged_files: 1,
            deleted_files: 0,
            nodes: 3,
            edges: 2,
        },
        timings: RebuildTimings {
            detect_ms: 1,
            extract_ms: 2,
            build_ms: 3,
            cluster_ms: 4,
            write_ms: 5,
            total_ms: 15,
        },
    };

    let encoded = serde_json::to_value(&result).unwrap();
    assert!(encoded.get("file_sets").is_none());
    assert_eq!(
        serde_json::from_value::<RebuildResult>(encoded).unwrap(),
        result
    );
}

#[test]
fn test_rebuild_code_full_corpus_skips_pending_queue() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("app.py"), "def app():\n    return 1\n");
    let out = temp.path().join(OUTPUT_DIRECTORY);
    queue_pending(&out, &["earlier.py".into()]).unwrap();
    let mut opts = options(temp.path(), true);
    opts.acquire_lock = true;
    opts.changed_paths = None;
    let result = rebuild_project_with_observer(temp.path(), &opts, |pass, _| {
        assert_eq!(pass, 1);
    })
    .unwrap();
    assert_eq!(result.passes, 1);
    assert!(!out.join(PENDING_CHANGES).exists());
}

#[test]
fn test_merge_changed_paths_dedupes_in_order() {
    let first = vec!["a.py".into(), "b.py".into()];
    let second = vec!["b.py".into(), "c.py".into()];
    let third = vec!["a.py".into()];
    assert_eq!(
        merge_changed_paths(&[Some(&first), None, Some(&second), Some(&third)]),
        vec![
            PathBuf::from("a.py"),
            PathBuf::from("b.py"),
            PathBuf::from("c.py")
        ]
    );
}

#[test]
fn test_rebuild_code_preserves_nodes_from_excluded_but_alive_file() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("auth.py"), "def login():\n    pass\n");
    write(
        temp.path().join("notes/brainstorm.md"),
        "# Brainstorm\n\nA local design note.\n",
    );
    assert!(build(temp.path(), true).succeeded());
    assert!(labels(&graph(temp.path())).contains("brainstorm.md"));
    write(temp.path().join(".graphifyignore"), "notes/\n");
    let mut opts = options(temp.path(), true);
    opts.changed_paths = Some(vec!["auth.py".into()]);
    let result = rebuild_project(temp.path(), &opts).unwrap();
    assert!(result.succeeded());
    assert!(result
        .warnings
        .iter()
        .any(|warning| warning.contains("fail-closed: kept")));
    assert!(labels(&graph(temp.path())).contains("brainstorm.md"));
}

/// A file that stops being extractable must not take its existing graph
/// records down with it, and must not stop the rest of the pass (#4).
#[test]
fn test_rebuild_keeps_records_from_a_file_that_no_longer_extracts() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("auth.py"), "def login():\n    pass\n");
    write(
        temp.path().join("tsconfig.json"),
        r#"{"extends": "./base.json"}"#,
    );
    assert!(build(temp.path(), true).succeeded());
    assert!(labels(&graph(temp.path())).contains("tsconfig.json"));

    write(temp.path().join("tsconfig.json"), "{\"extends\": [broken\n");
    write(
        temp.path().join("auth.py"),
        "def login():\n    pass\n\n\ndef logout():\n    pass\n",
    );
    let mut opts = options(temp.path(), true);
    opts.changed_paths = Some(vec!["auth.py".into(), "tsconfig.json".into()]);
    let result = rebuild_project(temp.path(), &opts).unwrap();

    assert!(result.succeeded());
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("skipped") && warning.contains("tsconfig.json")),
        "{:?}",
        result.warnings
    );
    let labels = labels(&graph(temp.path()));
    // The healthy file's new symbol landed...
    assert!(labels.contains("logout()"), "{labels:?}");
    // ...and the unextractable file kept the records it already had.
    assert!(labels.contains("tsconfig.json"), "{labels:?}");
}

#[test]
fn test_rebuild_code_still_evicts_when_excluded_file_is_also_deleted() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("auth.py"), "def login():\n    pass\n");
    write(temp.path().join("notes/brainstorm.md"), "# Brainstorm\n");
    assert!(build(temp.path(), true).succeeded());
    fs::remove_file(temp.path().join("notes/brainstorm.md")).unwrap();
    let mut opts = options(temp.path(), true);
    opts.changed_paths = Some(vec!["auth.py".into()]);
    assert!(rebuild_project(temp.path(), &opts).unwrap().succeeded());
    let after = labels(&graph(temp.path()));
    assert!(!after.contains("brainstorm.md"));
    assert!(after.contains("login()"));
}

const AST_GUIDE_IDS: [&str; 4] = ["guide", "guide_overview", "guide_setup", "guide_usage"];

#[derive(Clone, Copy)]
enum SemanticDocShape {
    Document,
    ConceptOnly,
    CodeOnly,
}

fn seed_semantic_doc(root: &Path, shape: SemanticDocShape) -> usize {
    write(root.join("app.py"), "def handle_login():\n    return 1\n");
    assert!(build(root, true).succeeded());
    write(
        root.join("guide.md"),
        "# Overview\n\nIntro.\n\n## Setup\n\nSteps.\n\n## Usage\n\nMore.\n",
    );
    let mut seeded = graph(root);
    let code_id = seeded
        .nodes
        .iter()
        .find(|node| node.source_file == "app.py")
        .unwrap()
        .id
        .clone();
    match shape {
        SemanticDocShape::Document => {
            let mut guide = node("guide_doc", "guide.md", false);
            guide.file_type = "document".into();
            seeded.nodes.push(guide);
            seeded.nodes.push(node("auth_flow", "guide.md", false));
            seeded.nodes.push(node("session_model", "guide.md", false));
            seeded.links.push(edge(
                "guide_doc",
                "auth_flow",
                "explains",
                "guide.md",
                false,
            ));
        }
        SemanticDocShape::ConceptOnly => {
            seeded.nodes.push(node("auth_flow", "guide.md", false));
            let mut rationale = node("session_model", "guide.md", false);
            rationale.file_type = "rationale".into();
            seeded.nodes.push(rationale);
            seeded.links.push(edge(
                "auth_flow",
                "session_model",
                "explains",
                "guide.md",
                false,
            ));
        }
        SemanticDocShape::CodeOnly => {
            let mut parse = node("parse_config", "guide.md", false);
            parse.file_type = "code".into();
            let mut load = node("load_settings", "guide.md", false);
            load.file_type = "code".into();
            seeded.nodes.extend([parse, load]);
        }
    }
    let source = match shape {
        SemanticDocShape::Document | SemanticDocShape::ConceptOnly => "auth_flow",
        SemanticDocShape::CodeOnly => "parse_config",
    };
    seeded
        .links
        .push(edge(source, &code_id, "implemented_by", "guide.md", false));
    let count = seeded.nodes.len();
    write_graph(root, &seeded);
    count
}

fn assert_no_ast_guide(graph: &KnowledgeGraph) {
    let graph_ids = ids(graph);
    assert!(AST_GUIDE_IDS.iter().all(|id| !graph_ids.contains(*id)));
}

#[test]
fn test_rebuild_code_semantic_doc_not_double_represented_on_full_rebuild() {
    let temp = tempfile::tempdir().unwrap();
    let before = seed_semantic_doc(temp.path(), SemanticDocShape::Document);
    assert!(build(temp.path(), true).succeeded());
    let after = graph(temp.path());
    assert!(ids(&after).is_superset(&BTreeSet::from([
        "guide_doc".into(),
        "auth_flow".into(),
        "session_model".into()
    ])));
    assert_no_ast_guide(&after);
    assert_eq!(after.nodes.len(), before);
}

#[test]
fn test_rebuild_code_concept_only_semantic_doc_not_double_represented_on_full_rebuild() {
    let temp = tempfile::tempdir().unwrap();
    let before = seed_semantic_doc(temp.path(), SemanticDocShape::ConceptOnly);
    assert!(build(temp.path(), true).succeeded());
    let after = graph(temp.path());
    assert!(ids(&after).is_superset(&BTreeSet::from([
        "auth_flow".into(),
        "session_model".into()
    ])));
    assert_no_ast_guide(&after);
    assert_eq!(after.nodes.len(), before);
}

fn run_incremental_semantic_doc(shape: SemanticDocShape, include_code: bool) {
    let temp = tempfile::tempdir().unwrap();
    seed_semantic_doc(temp.path(), shape);
    let mut changed = vec![PathBuf::from("guide.md")];
    if include_code {
        changed.push("app.py".into());
    }
    let mut opts = options(temp.path(), true);
    opts.changed_paths = Some(changed);
    assert!(rebuild_project(temp.path(), &opts).unwrap().succeeded());
    let after = graph(temp.path());
    assert_no_ast_guide(&after);
    let relations = after
        .links
        .iter()
        .map(|edge| {
            (
                edge.source.as_str(),
                edge.target.as_str(),
                edge.relation.as_str(),
            )
        })
        .collect::<BTreeSet<_>>();
    match shape {
        SemanticDocShape::Document => {
            assert!(ids(&after).is_superset(&BTreeSet::from([
                "guide_doc".into(),
                "auth_flow".into(),
                "session_model".into()
            ])));
            assert!(relations.contains(&("guide_doc", "auth_flow", "explains")));
        }
        SemanticDocShape::ConceptOnly => {
            assert!(ids(&after).is_superset(&BTreeSet::from([
                "auth_flow".into(),
                "session_model".into()
            ])));
            assert!(relations.contains(&("auth_flow", "session_model", "explains")));
        }
        SemanticDocShape::CodeOnly => unreachable!(),
    }
    assert!(relations
        .iter()
        .any(|(source, _, relation)| *source == "auth_flow" && *relation == "implemented_by"));
}

#[test]
fn test_rebuild_code_incremental_preserves_semantic_doc_nodes_and_edges_doc_only() {
    run_incremental_semantic_doc(SemanticDocShape::Document, false);
}

#[test]
fn test_rebuild_code_incremental_preserves_semantic_doc_nodes_and_edges_doc_plus_code() {
    run_incremental_semantic_doc(SemanticDocShape::Document, true);
}

#[test]
fn test_rebuild_code_incremental_preserves_concept_only_semantic_doc_nodes_and_edges_doc_only() {
    run_incremental_semantic_doc(SemanticDocShape::ConceptOnly, false);
}

#[test]
fn test_rebuild_code_incremental_preserves_concept_only_semantic_doc_nodes_and_edges_doc_plus_code()
{
    run_incremental_semantic_doc(SemanticDocShape::ConceptOnly, true);
}

#[test]
fn test_rebuild_code_quick_scans_doc_without_semantic_nodes() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("app.py"), "def f():\n    return 1\n");
    write(temp.path().join("notes.md"), "# Alpha\n\n## Beta\n");
    for _ in 0..2 {
        assert!(build(temp.path(), true).succeeded());
        assert!(ids(&graph(temp.path())).is_superset(&BTreeSet::from([
            "notes".into(),
            "notes_alpha".into(),
            "notes_beta".into()
        ])));
    }
}

#[test]
fn test_full_rebuild_preserves_semantic_backed_doc_ast_layer() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("app.py"), "def app():\n    return 1\n");
    write(
        temp.path().join("guide.md"),
        "# Overview\n\n## Setup\n\n## Usage\n",
    );
    assert!(build(temp.path(), true).succeeded());
    let mut seeded = graph(temp.path());
    assert!(ids(&seeded).is_superset(&AST_GUIDE_IDS.iter().map(|id| (*id).into()).collect()));
    let before = seeded
        .nodes
        .iter()
        .filter(|node| node.source_file == "guide.md")
        .count();
    seeded.nodes.push(node("auth_flow", "guide.md", false));
    write_graph(temp.path(), &seeded);
    assert!(build(temp.path(), true).succeeded());
    let after = graph(temp.path());
    assert!(ids(&after).contains("auth_flow"));
    assert!(ids(&after).is_superset(&AST_GUIDE_IDS.iter().map(|id| (*id).into()).collect()));
    assert_eq!(
        after
            .nodes
            .iter()
            .filter(|node| node.source_file == "guide.md" && node.id != "auth_flow")
            .count(),
        before
    );
}

#[test]
fn test_full_rebuild_regenerates_docs_with_legacy_unstamped_nodes() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("app.py"), "def app():\n    return 1\n");
    write(
        temp.path().join("guide.md"),
        "# Overview\n\n## Setup\n\n## Usage\n",
    );
    assert!(build(temp.path(), true).succeeded());
    let mut seeded = graph(temp.path());
    seeded
        .nodes
        .iter_mut()
        .find(|node| node.id == "guide_overview")
        .unwrap()
        .extra
        .remove("_origin");
    write_graph(temp.path(), &seeded);
    assert!(build(temp.path(), true).succeeded());
    let after = graph(temp.path());
    for id in AST_GUIDE_IDS {
        let node = after.nodes.iter().find(|node| node.id == id).unwrap();
        assert!(node
            .extra
            .get("_origin")
            .and_then(Value::as_str)
            .is_some_and(|origin| matches!(origin, "ast" | "fallback")));
    }
}

#[test]
fn test_full_rebuild_drops_stale_ast_for_reextracted_code() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path().join("app.py"),
        "def old_name():\n    return 1\n",
    );
    assert!(build(temp.path(), true).succeeded());
    assert!(ids(&graph(temp.path())).contains("app_old_name"));
    write(
        temp.path().join("app.py"),
        "def new_name():\n    return 1\n",
    );
    assert!(build(temp.path(), true).succeeded());
    let after = ids(&graph(temp.path()));
    assert!(after.contains("app_new_name"));
    assert!(!after.contains("app_old_name"));
}

#[test]
fn test_full_rebuild_replaces_every_structural_origin_and_preserves_semantic_records() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path().join("app.py"),
        "def alpha():\n    return 1\n\ndef beta():\n    return 2\n",
    );
    assert!(build(temp.path(), true).succeeded());
    let mut seeded = graph(temp.path());

    for origin in ["fallback", "terraform", "sql", "dotnet", "scip"] {
        let id = format!("stale_{origin}");
        let mut stale = node(&id, "app.py", false);
        stale.file_type = "code".into();
        stale.extra.insert("_origin".into(), origin.into());
        seeded.nodes.push(stale);

        let relation = format!("stale_{origin}_edge");
        let mut stale_edge = edge("app_alpha", "app_beta", &relation, "app.py", false);
        stale_edge.extra.insert("_origin".into(), origin.into());
        seeded.links.push(stale_edge);
    }

    let mut semantic = node("semantic_keep", "app.py", false);
    semantic.source_location = Some("L99".into());
    semantic.extra.insert("_origin".into(), "semantic".into());
    seeded.nodes.push(semantic);
    let mut semantic_edge = edge(
        "app_alpha",
        "app_beta",
        "semantic_keep_edge",
        "app.py",
        false,
    );
    semantic_edge
        .extra
        .insert("source_location".into(), "L99".into());
    semantic_edge
        .extra
        .insert("_origin".into(), "semantic".into());
    seeded.links.push(semantic_edge);

    let mut future_structural = node("future_structural", "app.py", false);
    future_structural.source_location = Some("L88".into());
    future_structural
        .extra
        .insert("_origin".into(), "future-parser".into());
    seeded.nodes.push(future_structural);
    let mut future_semantic = node("future_semantic", "app.py", false);
    future_semantic
        .extra
        .insert("_origin".into(), "future-parser".into());
    seeded.nodes.push(future_semantic);
    write_graph(temp.path(), &seeded);

    assert!(build(temp.path(), true).succeeded());
    let after = graph(temp.path());
    let after_ids = ids(&after);
    for origin in ["fallback", "terraform", "sql", "dotnet", "scip"] {
        assert!(!after_ids.contains(&format!("stale_{origin}")));
        assert!(!after
            .links
            .iter()
            .any(|edge| edge.relation == format!("stale_{origin}_edge")));
    }
    assert!(!after_ids.contains("future_structural"));
    assert!(after_ids.contains("semantic_keep"));
    assert!(after_ids.contains("future_semantic"));
    assert!(after
        .links
        .iter()
        .any(|edge| edge.relation == "semantic_keep_edge"));
}

#[test]
fn test_rebuild_code_code_only_semantic_doc_not_double_represented_on_full_rebuild() {
    let temp = tempfile::tempdir().unwrap();
    seed_semantic_doc(temp.path(), SemanticDocShape::CodeOnly);
    assert!(build(temp.path(), true).succeeded());
    let after = graph(temp.path());
    assert!(ids(&after).is_superset(&BTreeSet::from([
        "parse_config".into(),
        "load_settings".into()
    ])));
    assert_no_ast_guide(&after);
}

#[test]
fn test_rebuild_code_evicts_semantic_nodes_from_deleted_non_ast_source() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("app.py"), "def handle():\n    return 1\n");
    write(temp.path().join("kept.txt"), "kept\n");
    write(temp.path().join("gone.txt"), "gone\n");
    assert!(build(temp.path(), true).succeeded());
    let mut seeded = graph(temp.path());
    seeded.nodes.push(node("kept_concept", "kept.txt", false));
    seeded.nodes.push(node("gone_concept", "gone.txt", false));
    write_graph(temp.path(), &seeded);
    fs::remove_file(temp.path().join("gone.txt")).unwrap();
    assert!(build(temp.path(), true).succeeded());
    let after = ids(&graph(temp.path()));
    assert!(!after.contains("gone_concept"));
    assert!(after.contains("kept_concept"));
}

#[test]
fn test_rebuild_code_preserves_remote_source_across_repeated_updates() {
    for remote in [
        "gdoc://abc",
        "gdoc:/abc",
        "s3://bucket/key",
        "https://example.com/doc",
    ] {
        assert!(is_remote_source(remote));
    }
    for local in ["src/app.py", "notes.txt", "C:/Users/x/a.py"] {
        assert!(!is_remote_source(local));
    }
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("app.py"), "def handle():\n    return 1\n");
    assert!(build(temp.path(), true).succeeded());
    let mut seeded = graph(temp.path());
    seeded
        .nodes
        .push(node("remote_doc", "gdoc://team/spec", false));
    write_graph(temp.path(), &seeded);
    for _ in 0..3 {
        assert!(build(temp.path(), true).succeeded());
        assert!(ids(&graph(temp.path())).contains("remote_doc"));
    }
}

#[test]
fn test_rebuild_code_incremental_preserves_present_non_ast_source() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("app.py"), "def handle():\n    return 1\n");
    write(temp.path().join("spec.txt"), "semantic spec\n");
    assert!(build(temp.path(), true).succeeded());
    let mut seeded = graph(temp.path());
    seeded.nodes.push(node("spec_concept", "spec.txt", false));
    write_graph(temp.path(), &seeded);
    let mut opts = options(temp.path(), true);
    opts.changed_paths = Some(vec!["spec.txt".into(), "app.py".into()]);
    assert!(rebuild_project(temp.path(), &opts).unwrap().succeeded());
    assert!(ids(&graph(temp.path())).contains("spec_concept"));
}

fn seed_graph_with_semantic_layer(root: &Path) -> Vec<u8> {
    write(root.join("a.py"), "def alpha():\n    return 1\n");
    write(root.join("notes.txt"), "Design notes.\n");
    assert!(build(root, true).succeeded());
    let mut seeded = graph(root);
    let mut document = node("notes_doc", "notes.txt", false);
    document.file_type = "document".into();
    seeded.nodes.push(document);
    seeded.nodes.push(node("notes_concept", "notes.txt", false));
    seeded.links.push(edge(
        "notes_concept",
        "notes_doc",
        "described_in",
        "notes.txt",
        false,
    ));
    write_graph(root, &seeded);
    fs::read(graph_path(root)).unwrap()
}

#[test]
fn test_rebuild_refuses_overwrite_when_existing_graph_over_size_cap() {
    let temp = tempfile::tempdir().unwrap();
    let before = seed_graph_with_semantic_layer(temp.path());
    let mut opts = options(temp.path(), true);
    opts.changed_paths = Some(vec!["a.py".into()]);
    opts.max_graph_bytes = Some(100);
    let error = rebuild_project(temp.path(), &opts).unwrap_err().to_string();
    assert!(error.contains("exceeds 100-byte cap"));
    assert_eq!(fs::read(graph_path(temp.path())).unwrap(), before);
}

fn run_corrupt_graph_case(no_cluster: bool, force: bool) {
    let temp = tempfile::tempdir().unwrap();
    let before = seed_graph_with_semantic_layer(temp.path());
    let truncated = before[..40].to_vec();
    fs::write(graph_path(temp.path()), &truncated).unwrap();
    let mut opts = options(temp.path(), no_cluster);
    opts.changed_paths = Some(vec!["a.py".into()]);
    opts.force = force;
    let error = rebuild_project(temp.path(), &opts).unwrap_err().to_string();
    assert!(error.contains("corrupted") || error.contains("expected") || error.contains("EOF"));
    assert_eq!(fs::read(graph_path(temp.path())).unwrap(), truncated);
}

#[test]
fn test_rebuild_refuses_overwrite_when_existing_graph_corrupt_no_cluster() {
    run_corrupt_graph_case(true, false);
}

#[test]
fn test_rebuild_refuses_overwrite_when_existing_graph_corrupt_clustered() {
    run_corrupt_graph_case(false, false);
}

#[test]
fn test_rebuild_force_does_not_clobber_unreadable_graph() {
    run_corrupt_graph_case(true, true);
}

#[test]
fn test_rebuild_readable_graph_still_preserves_semantic_nodes() {
    let temp = tempfile::tempdir().unwrap();
    seed_graph_with_semantic_layer(temp.path());
    let mut opts = options(temp.path(), true);
    opts.changed_paths = Some(vec!["a.py".into()]);
    assert!(rebuild_project(temp.path(), &opts).unwrap().succeeded());
    let after = graph(temp.path());
    assert!(ids(&after).is_superset(&BTreeSet::from([
        "notes_doc".into(),
        "notes_concept".into()
    ])));
    assert!(after.links.iter().any(|edge| {
        edge.source == "notes_concept"
            && edge.target == "notes_doc"
            && edge.relation == "described_in"
    }));
}

#[test]
fn test_rebuild_code_inherits_directed_flag_clustered() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path().join("a.py"),
        "def alpha():\n    return beta()\n\ndef beta():\n    return 1\n",
    );
    assert!(build(temp.path(), false).succeeded());
    let mut seeded = graph(temp.path());
    seeded.directed = true;
    let before = seeded.nodes.len();
    write_graph(temp.path(), &seeded);
    write(
        temp.path().join("a.py"),
        "def alpha():\n    return beta() + gamma()\n\ndef beta():\n    return 1\n\ndef gamma():\n    return 2\n",
    );
    assert!(build(temp.path(), false).succeeded());
    let after = graph(temp.path());
    assert!(after.nodes.len() > before);
    assert!(after.directed);
    assert!(after.links.iter().any(|edge| {
        edge.relation == "calls"
            && edge.true_source().ends_with("alpha")
            && edge.true_target().ends_with("beta")
    }));
}

#[test]
fn test_rebuild_code_inherits_directed_flag_no_cluster() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("a.py"), "def f():\n    pass\n");
    assert!(build(temp.path(), true).succeeded());
    let mut seeded = graph(temp.path());
    seeded.directed = true;
    let before = seeded.nodes.len();
    write_graph(temp.path(), &seeded);
    write(
        temp.path().join("a.py"),
        "def f():\n    pass\n\ndef g():\n    pass\n",
    );
    assert!(build(temp.path(), true).succeeded());
    let after = graph(temp.path());
    assert!(after.nodes.len() > before);
    assert!(after.directed);
}

#[test]
fn test_rebuild_code_keeps_undirected_graph_undirected() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("a.py"), "def f():\n    pass\n");
    assert!(build(temp.path(), false).succeeded());
    assert!(!graph(temp.path()).directed);
    write(temp.path().join("a.py"), "def f():\n    return 1\n");
    assert!(build(temp.path(), false).succeeded());
    assert!(!graph(temp.path()).directed);
}

#[test]
fn test_rebuild_code_fresh_build_defaults_undirected() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path().join("a.py"), "def f():\n    pass\n");
    assert!(build(temp.path(), false).succeeded());
    assert!(!graph(temp.path()).directed);
}

fn two_projects() -> (TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let project_a = temp.path().join("proja");
    let project_b = temp.path().join("projb");
    write(
        project_a.join("proja_file.py"),
        "def alpha():\n    return helper_alpha()\n\ndef helper_alpha():\n    return 1\n",
    );
    write(
        project_b.join("projb_file.py"),
        "def beta():\n    return helper_beta()\n\ndef helper_beta():\n    return 1\n",
    );
    (temp, project_a, project_b)
}

fn run_manifest_target_case(no_cluster: bool) {
    let (_temp, project_a, project_b) = two_projects();
    assert!(build_from(&project_a, &project_b, no_cluster).succeeded());
    let target = project_b.join(OUTPUT_DIRECTORY).join("manifest.json");
    let wrong = project_a.join(OUTPUT_DIRECTORY).join("manifest.json");
    assert!(target.is_file());
    assert!(!wrong.exists());
    let rows: BTreeMap<String, Value> = serde_json::from_slice(&fs::read(target).unwrap()).unwrap();
    assert!(rows.keys().any(|key| key.contains("projb_file.py")));
}

#[test]
fn test_manifest_lands_in_the_target_not_the_cwd_clustered() {
    run_manifest_target_case(false);
}

#[test]
fn test_manifest_lands_in_the_target_not_the_cwd_no_cluster() {
    run_manifest_target_case(true);
}

#[test]
fn test_second_run_reaches_the_same_topology_early_return() {
    let (_temp, project_a, project_b) = two_projects();
    assert!(build_from(&project_a, &project_b, false).succeeded());
    let manifest = project_b.join(OUTPUT_DIRECTORY).join("manifest.json");
    fs::remove_file(&manifest).unwrap();
    let result = build_from(&project_a, &project_b, false);
    assert_eq!(result.status, RebuildStatus::Unchanged);
    assert!(manifest.is_file());
    assert!(!project_a
        .join(OUTPUT_DIRECTORY)
        .join("manifest.json")
        .exists());
}

#[test]
fn test_update_does_not_destroy_the_cwd_projects_own_manifest() {
    let (_temp, project_a, project_b) = two_projects();
    assert!(build_from(&project_a, Path::new("."), false).succeeded());
    let own = project_a.join(OUTPUT_DIRECTORY).join("manifest.json");
    let before = fs::read(&own).unwrap();
    assert!(build_from(&project_a, &project_b, false).succeeded());
    assert_eq!(fs::read(own).unwrap(), before);
}

#[test]
fn test_relative_target_manifest_keys_stay_portable() {
    let (_temp, project_a, project_b) = two_projects();
    assert!(build_from(&project_a, Path::new("../projb"), false).succeeded());
    let rows: BTreeMap<String, Value> = serde_json::from_slice(
        &fs::read(project_b.join(OUTPUT_DIRECTORY).join("manifest.json")).unwrap(),
    )
    .unwrap();
    assert!(rows.keys().all(|key| !Path::new(key).is_absolute()));
    assert!(rows.contains_key("projb_file.py"));
}

fn init_repo(root: &Path, message: &str) -> String {
    let command = |arguments: &[&str]| {
        Command::new("git")
            .args(arguments)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap()
    };
    assert!(command(&["init", "-q", "."]).status.success());
    assert!(command(&["add", "-A"]).status.success());
    assert!(command(&["commit", "-qm", message]).status.success());
    let output = command(&["rev-parse", "HEAD"]);
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().into()
}

#[test]
fn test_built_at_commit_comes_from_the_target_repo() {
    let (_temp, project_a, project_b) = two_projects();
    let head_a = init_repo(&project_a, "a");
    let head_b = init_repo(&project_b, "b");
    assert_ne!(head_a, head_b);
    assert!(build_from(&project_a, &project_b, false).succeeded());
    assert_eq!(
        graph(&project_b)
            .extra
            .get("built_at_commit")
            .and_then(Value::as_str),
        Some(head_b.as_str())
    );
}

#[test]
fn test_relative_target_manifest_is_consumable_by_detect_incremental() {
    let (_temp, project_a, project_b) = two_projects();
    assert!(build_from(&project_a, Path::new("../projb"), false).succeeded());
    let incremental = graphoxide_extract::detect::detect_incremental(
        &project_b,
        &project_b.join(OUTPUT_DIRECTORY).join("manifest.json"),
        &graphoxide_extract::detect::DetectOptions::default(),
        graphoxide_extract::detect::ManifestKind::Ast,
    )
    .unwrap();
    assert_eq!(incremental.new_total, 0);
    assert!(incremental.new_files.values().all(Vec::is_empty));
}
