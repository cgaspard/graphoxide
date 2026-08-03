use graphoxide_core::{Edge, Node};
use graphoxide_extract::extract_files;
use std::{fs, path::Path};
use tempfile::TempDir;

fn write(root: &Path, relative: &str, content: &str) -> std::path::PathBuf {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, content).unwrap();
    path
}

fn extract(root: &Path, paths: &[std::path::PathBuf]) -> (Vec<Node>, Vec<Edge>) {
    let result = extract_files(paths, Some(root), true).unwrap();
    (
        result
            .extractions
            .iter()
            .flat_map(|item| item.nodes.clone())
            .collect(),
        result
            .extractions
            .iter()
            .flat_map(|item| item.edges.clone())
            .collect(),
    )
}

#[test]
fn test_file_node_id_uses_parent_dir_and_stem_no_extension() {
    let temp = TempDir::new().unwrap();
    let file = write(
        temp.path(),
        "match/script/pipeline_step.py",
        "def run():\n    pass\n",
    );
    let (nodes, _) = extract(temp.path(), &[file]);
    let ids = nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<Vec<_>>();
    assert!(ids.contains(&"match_script_pipeline_step"), "{ids:?}");
    assert!(!ids.contains(&"match_script_pipeline_step_py"));
    assert!(!ids
        .iter()
        .any(|id| id.contains("pipeline_step") && id.ends_with("_py")));
}

#[test]
fn test_top_level_file_node_id_is_bare_stem() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "setup.py", "def configure():\n    pass\n");
    let (nodes, _) = extract(temp.path(), &[file]);
    let ids = nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<Vec<_>>();
    assert!(ids.contains(&"setup"), "{ids:?}");
    assert!(!ids.contains(&"setup_py"));
}

#[test]
fn test_top_level_file_symbol_ids_use_bare_stem() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "main.py", "def run():\n    return 1\n");
    let (nodes, edges) = extract(temp.path(), &[fs::canonicalize(file).unwrap()]);
    let ids = nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<Vec<_>>();
    assert!(ids.contains(&"main_run"), "{ids:?}");
    let root_name =
        graphoxide_core::normalize_id(temp.path().file_name().unwrap().to_string_lossy().as_ref());
    assert!(!ids.iter().any(|id| id.contains(&root_name)), "{ids:?}");
    assert!(edges.iter().any(|edge| {
        edge.relation == "contains"
            && edge.true_source() == "main"
            && edge.true_target() == "main_run"
    }));
}

#[test]
fn test_nested_file_symbol_ids_unchanged() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "sub/mod.py", "def work():\n    return 2\n");
    let (nodes, _) = extract(temp.path(), &[fs::canonicalize(file).unwrap()]);
    let ids = nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<Vec<_>>();
    assert!(ids.contains(&"sub_mod"), "{ids:?}");
    assert!(ids.contains(&"sub_mod_work"), "{ids:?}");
}

#[test]
fn test_symbol_and_file_ids_share_the_same_stem() {
    let temp = TempDir::new().unwrap();
    let file = write(
        temp.path(),
        "match/script/pipeline_step.py",
        "def run():\n    pass\n\nclass Stage:\n    pass\n",
    );
    let (nodes, edges) = extract(temp.path(), &[file]);
    let ids = nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<Vec<_>>();
    assert!(ids.contains(&"match_script_pipeline_step"), "{ids:?}");
    assert!(ids.contains(&"match_script_pipeline_step_stage"), "{ids:?}");
    assert!(edges.iter().any(|edge| {
        edge.relation == "contains"
            && edge.true_source() == "match_script_pipeline_step"
            && edge.true_target() == "match_script_pipeline_step_stage"
    }));
}

#[test]
fn test_cross_file_import_edges_stay_connected() {
    let temp = TempDir::new().unwrap();
    let models = write(temp.path(), "pkg/models.py", "class User:\n    pass\n");
    let auth = write(
        temp.path(),
        "pkg/auth.py",
        "from models import User\n\nclass Session:\n    def check(self):\n        return User()\n",
    );
    let (nodes, edges) = extract(temp.path(), &[models, auth]);
    let ids = nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(ids.contains("pkg_models"));
    assert!(ids.contains("pkg_auth"));
    for edge in &edges {
        assert!(!edge.true_source().ends_with("_py"));
        assert!(!edge.true_target().ends_with("_py"));
        if edge.relation == "imports_from" && edge.true_source() == "pkg_auth" {
            assert!(ids.contains(edge.true_target()) || edge.true_target().contains("models"));
        }
    }
}
