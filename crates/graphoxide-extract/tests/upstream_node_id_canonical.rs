use graphoxide_core::{normalize_id, Edge, Node};
use graphoxide_extract::extract_files;
use std::{
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

fn write(root: &Path, name: &str, content: &str) -> PathBuf {
    let path = root.join(name);
    fs::write(&path, content).unwrap();
    path
}

fn extract(paths: &[PathBuf], cache_root: &Path) -> (Vec<Node>, Vec<Edge>) {
    let result = extract_files(paths, Some(cache_root), true).unwrap();
    let nodes = result
        .extractions
        .iter()
        .flat_map(|item| item.nodes.clone())
        .collect();
    let edges = result
        .extractions
        .iter()
        .flat_map(|item| item.edges.clone())
        .collect();
    (nodes, edges)
}

fn slug(root: &Path) -> String {
    normalize_id(root.file_name().unwrap().to_string_lossy().as_ref())
}

fn assert_no_slug(nodes: &[Node], edges: &[Edge], slug: &str) {
    assert!(!nodes
        .iter()
        .any(|node| node.id.to_lowercase().contains(slug)));
    assert!(!edges.iter().any(|edge| {
        edge.true_source().to_lowercase().contains(slug)
            || edge.true_target().to_lowercase().contains(slug)
    }));
}

#[test]
fn test_module_level_dispatch_indirect_call_source_is_canonical() {
    let temp = TempDir::new().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let handlers = write(&root, "handlers.py", "def handle_a():\n    return 1\n");
    let dispatch = write(
        &root,
        "disp.py",
        "from handlers import handle_a\n\nHANDLERS = {'a': handle_a}\n",
    );
    let (nodes, edges) = extract(&[dispatch, handlers], temp.path());
    let indirect = edges
        .iter()
        .filter(|edge| edge.relation == "indirect_call")
        .collect::<Vec<_>>();
    assert!(!indirect.is_empty());
    let ids = nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for edge in indirect {
        assert_eq!(edge.true_source(), "disp");
        assert!(ids.contains(edge.true_source()));
        assert_eq!(edge.true_target(), "handlers_handle_a");
    }
    assert_no_slug(&nodes, &edges, &slug(&root));
}

#[test]
fn test_bash_source_incremental_target_canonicalizes() {
    let temp = TempDir::new().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let a = write(&root, "a.sh", "#!/bin/bash\nsource ./b.sh\nbash ./c.sh\n");
    let b = write(&root, "b.sh", "#!/bin/bash\nhello() { echo hi; }\n");
    let c = write(&root, "c.sh", "#!/bin/bash\necho run\n");
    let (full_nodes, full_edges) = extract(&[a.clone(), b, c], temp.path());
    assert_no_slug(&full_nodes, &full_edges, &slug(&root));
    assert!(full_edges.iter().any(|edge| edge.relation == "imports_from"
        && edge.true_source() == "a"
        && edge.true_target() == "b"));
    assert!(
        full_edges.iter().any(|edge| {
            edge.relation == "calls"
                && edge.true_source() == "a_sh__entry"
                && edge.true_target() == "c_sh__entry"
                && edge.extra.get("context").and_then(|value| value.as_str())
                    == Some("script_invocation")
        }),
        "bash calls: {:?}",
        full_edges
            .iter()
            .map(|edge| (
                edge.true_source(),
                edge.true_target(),
                edge.relation.as_str(),
                edge.extra.get("context")
            ))
            .collect::<Vec<_>>()
    );
    let (incremental_nodes, incremental_edges) = extract(&[a], temp.path());
    assert_no_slug(&incremental_nodes, &incremental_edges, &slug(&root));
    assert!(incremental_edges
        .iter()
        .any(|edge| edge.relation == "imports_from"
            && edge.true_source() == "a"
            && edge.true_target() == "b"));
    assert!(incremental_edges.iter().any(|edge| edge.relation == "calls"
        && edge.true_source() == "a_sh__entry"
        && edge.true_target() == "c_sh__entry"));
    assert!(incremental_edges
        .iter()
        .chain(&full_edges)
        .all(|edge| !edge.extra.contains_key("target_file")));
}

#[test]
fn test_tsx_nested_handler_calls_source_is_canonical() {
    let temp = TempDir::new().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let row = write(
        &root,
        "row.tsx",
        "export const constructRowWithId = (id: string) => {\n  return { id };\n};\n",
    );
    let panel = write(
        &root,
        "panel.tsx",
        r#"import { constructRowWithId } from "./row";
export const PrepayBalanceContainer = () => {
  const InvoiceBalanceSubsection = () => {
    return <section className="invoice"><header>Balance</header><span>{constructRowWithId("invoice").id}</span></section>;
  };
  const handleApply = () => constructRowWithId("apply");
  const handleTabClick = (tab: string) => { return constructRowWithId(tab); };
  return <InvoiceBalanceSubsection />;
};
"#,
    );
    let (nodes, edges) = extract(&[panel, row], temp.path());
    assert_no_slug(&nodes, &edges, &slug(&root));
    let ids = nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let calls = edges
        .iter()
        .filter(|edge| matches!(edge.relation.as_str(), "calls" | "indirect_call"))
        .collect::<Vec<_>>();
    let imported = calls
        .iter()
        .filter(|edge| edge.true_target().ends_with("constructrowwithid"))
        .map(|edge| edge.true_target())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        imported,
        std::collections::BTreeSet::from(["row_constructrowwithid"])
    );
    assert!(calls.iter().all(|edge| ids.contains(edge.true_source())));
}

#[test]
fn test_extract_invariant_no_absolute_root_slug_anywhere() {
    let temp = TempDir::new().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let paths = vec![
        write(&root, "handlers.py", "def handle_a():\n    return 1\n"),
        write(
            &root,
            "disp.py",
            "from handlers import handle_a\n\nHANDLERS = {'a': handle_a}\n",
        ),
        write(&root, "main.py", "import handlers\n\nhandlers.handle_a()\n"),
        write(&root, "run.sh", "#!/bin/bash\nsource ./lib.sh\n./tool.sh\n"),
        write(&root, "lib.sh", "#!/bin/bash\ngreet() { echo hi; }\n"),
        write(&root, "tool.sh", "#!/bin/bash\necho tool\n"),
    ];
    let (nodes, edges) = extract(&paths, temp.path());
    assert_no_slug(&nodes, &edges, &slug(&root));
}
