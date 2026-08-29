#![cfg(target_os = "linux")]

use graphoxide_cli::wiki;
use graphoxide_core::{KnowledgeGraph, Node};
use graphoxide_export::{derive_topic_tree, render_structured_wiki};
use serde_json::json;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
    fs::write(path, text).expect("write fixture");
}

fn config(root: &Path, output: &str) -> PathBuf {
    let path = root.join("wiki.json");
    write(
        &path,
        &format!(
            r#"{{"version":1,"roots":["docs"],"exclude":["docs/drafts"],"required_frontmatter":["title","sources"],"output":"{output}"}}"#
        ),
    );
    path
}

fn page(title: &str, sources: &[&str]) -> String {
    format!(
        "---\ntitle: {title}\nsources:\n{}---\n\n{title} body\n",
        sources
            .iter()
            .map(|source| format!("  - {source}\n"))
            .collect::<String>()
    )
}

fn structured_page(title: &str, kind: &str, graph_ref: &str, parent: &str, body: &str) -> String {
    format!(
        "---\ntitle: {title}\nkind: {kind}\ngraph_ref: {graph_ref}\nparent: {parent}\ninput_sha256: {}\nsources:\n---\n\n{body}\n",
        "0".repeat(64)
    )
}

fn catalog_node(
    id: &str,
    label: &str,
    source_file: &str,
    community: i64,
    source_id: &str,
    capture_id: &str,
    representation: &str,
) -> Node {
    Node {
        id: id.into(),
        label: label.into(),
        file_type: "document".into(),
        source_file: source_file.into(),
        source_location: None,
        community: Some(community),
        extra: BTreeMap::from([(
            "catalog".into(),
            json!({
                "source_id": source_id,
                "capture_id": capture_id,
                "sha256": format!("{community:064x}"),
                "representation": representation,
            }),
        )]),
    }
}

#[test]
fn index_renders_pages_in_deterministic_path_order() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "llms.txt");
    write(
        &temp.path().join("docs/z.md"),
        &page("Zeta", &["source-z#capture-z"]),
    );
    write(
        &temp.path().join("docs/a.md"),
        &page("Alpha", &["source-a#capture-a"]),
    );
    write(&temp.path().join("llms.txt"), "old output\n");

    wiki::index(temp.path(), &config_path).expect("index wiki");

    assert_eq!(
        fs::read_to_string(temp.path().join("llms.txt")).expect("generated output"),
        "# Wiki\n\n- [Alpha](docs/a.md)\n- [Zeta](docs/z.md)\n"
    );
    assert!(!fs::read_dir(temp.path())
        .expect("root entries")
        .any(|entry| entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .contains(".tmp")));
}

#[test]
fn index_renders_empty_sources_without_trailing_whitespace() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "llms.txt");
    write(&temp.path().join("docs/page.md"), &page("Page", &[]));

    wiki::index(temp.path(), &config_path).expect("index wiki");

    assert_eq!(
        fs::read_to_string(temp.path().join("llms.txt")).expect("generated output"),
        "# Wiki\n\n- [Page](docs/page.md)\n"
    );
}

#[test]
fn manual_kind_and_parent_fields_keep_legacy_index_behavior() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "llms.txt");
    write(
        &temp.path().join("docs/a.md"),
        "---\ntitle: Alpha\nkind: guide\nparent: handbook\nsources:\n---\n\nManual page.\n",
    );
    write(&temp.path().join("docs/z.md"), &page("Zeta", &[]));

    wiki::index(temp.path(), &config_path).expect("index manual wiki");

    assert_eq!(
        fs::read_to_string(temp.path().join("llms.txt")).expect("generated output"),
        "# Wiki\n\n- [Alpha](docs/a.md)\n- [Zeta](docs/z.md)\n"
    );
    wiki::check(temp.path(), &config_path, None).expect("check manual wiki");
}

#[test]
fn index_renders_generated_pages_in_hierarchy_order() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "llms.txt");
    write(
        &temp.path().join("docs/sources/source.md"),
        &structured_page(
            "Source",
            "source",
            "source-one",
            "communities/7.md",
            "[Back](../communities/7.md)",
        ),
    );
    write(
        &temp.path().join("docs/communities/7.md"),
        &structured_page(
            "Community",
            "community",
            "7",
            "topics/topic.md",
            "[Source](../sources/source.md)",
        ),
    );
    write(
        &temp.path().join("docs/topics/topic.md"),
        &structured_page(
            "Topic",
            "topic",
            "topic",
            "index.md",
            "[Community](../communities/7.md)",
        ),
    );
    write(
        &temp.path().join("docs/index.md"),
        &structured_page(
            "Root",
            "root",
            "root",
            "root",
            "[Topic](topics/topic.md) [section](#local) [web](https://example.invalid) `[code](missing.md)`",
        ),
    );

    wiki::index(temp.path(), &config_path).expect("index structured wiki");

    assert_eq!(
        fs::read_to_string(temp.path().join("llms.txt")).expect("generated output"),
        "# Wiki\n\n- [Root](docs/index.md)\n  - [Topic](docs/topics/topic.md)\n    - [Community](docs/communities/7.md)\n      - [Source](docs/sources/source.md)\n"
    );
}

#[test]
fn generated_mixed_corpus_wiki_is_deterministic_and_checker_valid() {
    let graph = KnowledgeGraph {
        nodes: vec![
            catalog_node(
                "api",
                "API contract",
                "spec/openapi.json",
                1,
                "api-contract",
                "capture-api",
                "json",
            ),
            catalog_node(
                "firmware",
                "Firmware register guide",
                "hardware/firmware.md",
                2,
                "firmware-guide",
                "capture-firmware",
                "markdown",
            ),
            catalog_node(
                "simulation",
                "Simulation model",
                "physics/model.md",
                3,
                "simulation-model",
                "capture-simulation",
                "markdown",
            ),
            catalog_node(
                "pdf",
                "Hardware brief",
                "hardware/brief.pdf",
                4,
                "hardware-brief",
                "capture-pdf",
                "pdf",
            ),
            catalog_node(
                "office",
                "Test matrix",
                "verification/matrix.docx",
                5,
                "test-matrix",
                "capture-office",
                "office-document",
            ),
        ],
        ..KnowledgeGraph::default()
    };
    let expected = render_structured_wiki(&graph, &derive_topic_tree(&graph).expect("topics"))
        .expect("render structured wiki");
    let mut shuffled = graph;
    shuffled.nodes.reverse();
    let actual = render_structured_wiki(
        &shuffled,
        &derive_topic_tree(&shuffled).expect("shuffled topics"),
    )
    .expect("render shuffled structured wiki");
    assert_eq!(actual, expected);

    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "llms.txt");
    for page in &expected.pages {
        write(&temp.path().join("docs").join(&page.path), &page.markdown);
    }

    wiki::index(temp.path(), &config_path).expect("index generated mixed wiki");
    wiki::check(temp.path(), &config_path, None).expect("check generated mixed wiki");

    let output = fs::read_to_string(temp.path().join("llms.txt")).expect("hierarchical index");
    for title in [
        "API contract",
        "Firmware register guide",
        "Simulation model",
        "Hardware brief",
        "Test matrix",
    ] {
        assert!(output.contains(title), "missing {title} from {output}");
    }
}

#[test]
fn index_rejects_unsafe_generated_local_links() {
    for (name, link) in [
        ("escaping", "../../outside.md"),
        ("encoded escaping", "%2E%2E/%2E%2E/outside.md"),
        ("missing", "missing.md"),
    ] {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = config(temp.path(), "llms.txt");
        write(
            &temp.path().join("docs/index.md"),
            &structured_page(
                "Root",
                "root",
                "root",
                "root",
                &format!(r#"[unsafe \[link\]]({link})"#),
            ),
        );

        let error = wiki::index(temp.path(), &config_path).expect_err(name);
        assert!(error.to_string().contains("link"), "{name}: {error:#}");
    }
}

#[cfg(unix)]
#[test]
fn index_rejects_a_symlinked_generated_link_target() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside");
    let config_path = config(temp.path(), "llms.txt");
    write(&outside.path().join("asset.txt"), "outside");
    write(
        &temp.path().join("docs/index.md"),
        &structured_page("Root", "root", "root", "root", "[asset](asset.txt)"),
    );
    symlink(
        outside.path().join("asset.txt"),
        temp.path().join("docs/asset.txt"),
    )
    .expect("asset link");

    let error = wiki::index(temp.path(), &config_path).expect_err("symlinked target");
    assert!(error.to_string().contains("link"), "{error:#}");
}

#[test]
fn index_rejects_orphaned_cyclic_and_duplicate_generated_pages() {
    for (name, pages) in [
        (
            "orphan",
            vec![
                (
                    "index.md",
                    structured_page("Root", "root", "root", "root", ""),
                ),
                (
                    "topics/orphan.md",
                    structured_page("Orphan", "topic", "orphan", "topics/missing.md", ""),
                ),
            ],
        ),
        (
            "cycle",
            vec![
                (
                    "index.md",
                    structured_page("Root", "root", "root", "root", ""),
                ),
                (
                    "topics/a.md",
                    structured_page("A", "topic", "a", "topics/b.md", ""),
                ),
                (
                    "topics/b.md",
                    structured_page("B", "topic", "b", "topics/a.md", ""),
                ),
            ],
        ),
        (
            "duplicate",
            vec![
                (
                    "index.md",
                    structured_page("Root", "root", "root", "root", ""),
                ),
                (
                    "topics/topic.md",
                    structured_page("Topic", "topic", "topic", "index.md", ""),
                ),
                (
                    "communities/a.md",
                    structured_page("A", "community", "7", "topics/topic.md", ""),
                ),
                (
                    "communities/b.md",
                    structured_page("B", "community", "7", "topics/topic.md", ""),
                ),
            ],
        ),
    ] {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = config(temp.path(), "llms.txt");
        for (path, text) in pages {
            write(&temp.path().join("docs").join(path), &text);
        }

        let error = wiki::index(temp.path(), &config_path).expect_err(name);
        assert!(
            error.to_string().contains("hierarchy")
                || error.to_string().contains("parent")
                || error.to_string().contains("duplicate"),
            "{name}: {error:#}"
        );
    }
}

#[test]
fn check_rejects_a_flat_stale_index_for_a_generated_hierarchy() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "llms.txt");
    write(
        &temp.path().join("docs/index.md"),
        &structured_page("Root", "root", "root", "root", "[Topic](topics/topic.md)"),
    );
    write(
        &temp.path().join("docs/topics/topic.md"),
        &structured_page("Topic", "topic", "topic", "index.md", "[Back](../index.md)"),
    );
    write(
        &temp.path().join("llms.txt"),
        "# Wiki\n\n- [Root](docs/index.md)\n- [Topic](docs/topics/topic.md)\n",
    );

    let error = wiki::check(temp.path(), &config_path, None).expect_err("flat stale output");
    assert!(error.to_string().contains("stale"), "{error:#}");
}

#[cfg(unix)]
#[test]
fn index_preserves_existing_output_mode() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "llms.txt");
    write(
        &temp.path().join("docs/page.md"),
        &page("Page", &["source#capture"]),
    );
    let output = temp.path().join("llms.txt");
    write(&output, "old\n");
    fs::set_permissions(&output, fs::Permissions::from_mode(0o600)).expect("set mode");

    wiki::index(temp.path(), &config_path).expect("index wiki");

    assert_eq!(
        fs::metadata(output)
            .expect("output metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn index_honors_gitignore_graphoxideignore_and_config_excludes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "llms.txt");
    write(&temp.path().join(".gitignore"), "docs/git-ignored.md\n");
    write(
        &temp.path().join(".graphoxideignore"),
        "docs/graphoxide-ignored.md\n",
    );
    write(
        &temp.path().join("docs/kept.md"),
        &page("Kept", &["source#capture"]),
    );
    write(
        &temp.path().join("docs/git-ignored.md"),
        &page("Git ignored", &["source#capture"]),
    );
    write(
        &temp.path().join("docs/graphoxide-ignored.md"),
        &page("Graphoxide ignored", &["source#capture"]),
    );
    write(
        &temp.path().join("docs/drafts/nope.md"),
        &page("Excluded", &["source#capture"]),
    );

    wiki::index(temp.path(), &config_path).expect("index wiki");
    let output = fs::read_to_string(temp.path().join("llms.txt")).expect("generated output");
    assert!(output.contains("Kept"), "{output}");
    assert!(
        !output.contains("ignored") && !output.contains("Excluded"),
        "{output}"
    );
}

#[test]
fn index_rejects_malformed_duplicate_and_missing_frontmatter() {
    for (name, text) in [
        ("malformed", "---\ntitle: Bad\n"),
        (
            "duplicate",
            "---\ntitle: One\ntitle: Two\nsources:\n  - source#capture\n---\n",
        ),
        ("missing", "---\ntitle: Missing sources\n---\n"),
    ] {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = config(temp.path(), "llms.txt");
        write(&temp.path().join(format!("docs/{name}.md")), text);
        let error = wiki::index(temp.path(), &config_path).expect_err(name);
        assert!(error.to_string().contains(name) || error.to_string().contains("sources"));
    }
}

#[test]
fn index_rejects_unsafe_config_paths_and_invalid_source_syntax() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = temp.path().join("wiki.json");
    write(
        &config_path,
        r#"{"version":1,"roots":["../outside"],"exclude":[],"required_frontmatter":["title","sources"],"output":"llms.txt"}"#,
    );
    assert!(wiki::index(temp.path(), &config_path).is_err());

    let config_path = config(temp.path(), "llms.txt");
    write(
        &temp.path().join("docs/bad.md"),
        &page("Bad source", &["-source#capture"]),
    );
    assert!(wiki::index(temp.path(), &config_path).is_err());
}

#[test]
fn index_rejects_any_output_other_than_root_llms_txt() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "generated/llms.txt");
    write(
        &temp.path().join("docs/page.md"),
        &page("Page", &["source#capture"]),
    );

    let error = wiki::index(temp.path(), &config_path).expect_err("reject nested output");

    assert!(error.to_string().contains("must be llms.txt"), "{error:#}");
    assert!(!temp.path().join("generated/llms.txt").exists());
}

#[test]
fn index_rejects_a_config_outside_the_wiki_root() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("wiki");
    let config_path = temp.path().join("outside.json");
    write(
        &config_path,
        r#"{"version":1,"roots":["docs"],"exclude":[],"required_frontmatter":["title","sources"],"output":"llms.txt"}"#,
    );
    write(
        &root.join("docs/page.md"),
        &page("Page", &["source#capture"]),
    );

    assert!(wiki::index(&root, &config_path).is_err());
    assert!(!root.join("llms.txt").exists());
}

#[test]
fn index_resolves_a_relative_config_beneath_the_wiki_root() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("wiki");
    config(&root, "llms.txt");
    write(
        &root.join("docs/page.md"),
        &page("Page", &["source#capture"]),
    );

    wiki::index(&root, Path::new("wiki.json")).expect("index wiki");
    assert!(root.join("llms.txt").exists());
}

#[cfg(unix)]
#[test]
fn index_rejects_a_symlinked_wiki_config() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside");
    let root = temp.path().join("wiki");
    fs::create_dir_all(&root).expect("wiki root");
    let config_path = root.join("wiki.json");
    write(
        &outside.path().join("wiki.json"),
        r#"{"version":1,"roots":["docs"],"exclude":[],"required_frontmatter":["title","sources"],"output":"llms.txt"}"#,
    );
    symlink(outside.path().join("wiki.json"), &config_path).expect("config link");
    write(
        &root.join("docs/page.md"),
        &page("Page", &["source#capture"]),
    );

    assert!(wiki::index(&root, &config_path).is_err());
    assert!(!root.join("llms.txt").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn index_rejects_fifo_wiki_config_without_publishing_an_index() {
    use std::{ffi::CString, os::unix::ffi::OsStrExt as _};

    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = temp.path().join("wiki.json");
    write(
        &temp.path().join("docs/page.md"),
        &page("Page", &["source#capture"]),
    );
    let config_name = CString::new(config_path.as_os_str().as_bytes()).expect("config path");
    // SAFETY: the C string is NUL-terminated and remains valid for the call.
    assert_eq!(
        unsafe { libc::mkfifo(config_name.as_ptr(), 0o600) },
        0,
        "create FIFO"
    );

    assert!(wiki::index(temp.path(), &config_path).is_err());
    assert!(!temp.path().join("llms.txt").exists());
}

#[test]
fn index_rejects_output_aliasing_config_and_preserves_config() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = temp.path().join("llms.txt");
    write(
        &config_path,
        r#"{"version":1,"roots":["docs"],"exclude":[],"required_frontmatter":["title","sources"],"output":"llms.txt"}"#,
    );
    write(
        &temp.path().join("docs/page.md"),
        &page("Page", &["source#capture"]),
    );
    let before = fs::read(&config_path).expect("config before indexing");

    let error = wiki::index(temp.path(), &config_path).expect_err("reject config output alias");

    assert!(error.to_string().contains("must not replace its config"));
    assert_eq!(
        fs::read(config_path).expect("config after rejection"),
        before
    );
}

#[cfg(unix)]
#[test]
fn index_rejects_symlinked_pages() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "llms.txt");
    write(
        &temp.path().join("outside.md"),
        &page("Outside", &["source#capture"]),
    );
    fs::create_dir_all(temp.path().join("docs")).expect("docs");
    symlink(
        temp.path().join("outside.md"),
        temp.path().join("docs/link.md"),
    )
    .expect("link");

    assert!(wiki::index(temp.path(), &config_path).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn index_rejects_fifo_pages_without_publishing_an_index() {
    use std::{ffi::CString, os::unix::ffi::OsStrExt as _};

    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "llms.txt");
    let page = temp.path().join("docs/page.md");
    fs::create_dir_all(page.parent().expect("page parent")).expect("page parent");
    let page = CString::new(page.as_os_str().as_bytes()).expect("page path");
    // SAFETY: the C string is NUL-terminated and remains valid for the call.
    assert_eq!(
        unsafe { libc::mkfifo(page.as_ptr(), 0o600) },
        0,
        "create FIFO"
    );

    assert!(wiki::index(temp.path(), &config_path).is_err());
    assert!(!temp.path().join("llms.txt").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn check_rejects_fifo_output_without_writing_it() {
    use std::{ffi::CString, os::unix::ffi::OsStrExt as _};

    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "llms.txt");
    write(
        &temp.path().join("docs/page.md"),
        &page("Page", &["source#capture"]),
    );
    wiki::index(temp.path(), &config_path).expect("index wiki");
    let output = temp.path().join("llms.txt");
    fs::remove_file(&output).expect("remove output");
    let output_name = CString::new(output.as_os_str().as_bytes()).expect("output path");
    // SAFETY: the C string is NUL-terminated and remains valid for the call.
    assert_eq!(
        unsafe { libc::mkfifo(output_name.as_ptr(), 0o600) },
        0,
        "create FIFO"
    );

    assert!(wiki::check(temp.path(), &config_path, None).is_err());
}

#[cfg(unix)]
#[test]
fn index_rejects_a_symlinked_output_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside");
    let config_path = config(temp.path(), "llms.txt");
    write(
        &temp.path().join("docs/page.md"),
        &page("Page", &["source#capture"]),
    );
    let outside_output = outside.path().join("llms.txt");
    write(&outside_output, "outside");
    symlink(&outside_output, temp.path().join("llms.txt")).expect("link");

    assert!(wiki::index(temp.path(), &config_path).is_err());
    assert_eq!(
        fs::read_to_string(outside_output).expect("outside output"),
        "outside"
    );
}

#[test]
fn check_detects_stale_output_and_never_writes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "llms.txt");
    let page_path = temp.path().join("docs/page.md");
    write(&page_path, &page("Original", &["source#capture"]));
    wiki::index(temp.path(), &config_path).expect("index wiki");
    let before = fs::read(temp.path().join("llms.txt")).expect("output");

    write(&page_path, &page("Changed", &["source#capture"]));
    assert!(wiki::check(temp.path(), &config_path, None).is_err());
    assert_eq!(
        fs::read(temp.path().join("llms.txt")).expect("output"),
        before
    );
}

#[test]
fn check_optionally_validates_citations() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "llms.txt");
    write(
        &temp.path().join("docs/page.md"),
        &page("Page", &["source#capture"]),
    );
    wiki::index(temp.path(), &config_path).expect("index wiki");

    let known = BTreeSet::from(["source#capture".to_owned()]);
    wiki::check(temp.path(), &config_path, Some(&known)).expect("known citation");

    let missing = BTreeSet::new();
    assert!(wiki::check(temp.path(), &config_path, Some(&missing)).is_err());
}

#[test]
fn index_rejects_an_unsupported_output_and_preserves_existing_page() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "docs/page.md");
    let output = temp.path().join("docs/page.md");
    write(&output, &page("Page", &["source#capture"]));
    let before = fs::read(&output).expect("source page");

    let error = wiki::index(temp.path(), &config_path).expect_err("reject source page output");

    assert!(error.to_string().contains("must be llms.txt"), "{error:#}");
    assert_eq!(
        fs::read(&output).expect("source page after rejection"),
        before
    );
}

#[test]
fn index_percent_encodes_markdown_link_destinations() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "llms.txt");
    write(
        &temp.path().join("docs/a b#c?d).md"),
        &page("Escaped", &["source#capture"]),
    );

    wiki::index(temp.path(), &config_path).expect("index wiki");

    let output = fs::read_to_string(temp.path().join("llms.txt")).expect("output");
    assert!(
        output.contains("[Escaped](docs/a%20b%23c%3Fd%29.md)"),
        "{output}"
    );
}

#[test]
fn index_rejects_duplicate_source_entries() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "llms.txt");
    write(
        &temp.path().join("docs/page.md"),
        &page("Page", &["source#capture", "source#capture"]),
    );

    assert!(wiki::index(temp.path(), &config_path).is_err());
}

#[test]
fn index_rejects_crlf_frontmatter_that_exceeds_its_byte_limit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "llms.txt");
    let text = format!(
        "---\r\ntitle: {}\r\nsources:\r\n  - source#capture\r\n---\r\n",
        "x".repeat(65_492)
    );
    write(&temp.path().join("docs/page.md"), &text);

    assert!(wiki::index(temp.path(), &config_path).is_err());
}

#[cfg(unix)]
#[test]
fn index_rejects_non_utf8_wiki_paths() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "llms.txt");
    write(
        &temp
            .path()
            .join("docs")
            .join(OsString::from_vec(b"page-\xff.md".to_vec())),
        &page("Page", &["source#capture"]),
    );

    assert!(wiki::index(temp.path(), &config_path).is_err());
}

#[test]
fn check_rejects_overlong_output_without_writing_it() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "llms.txt");
    write(
        &temp.path().join("docs/page.md"),
        &page("Page", &["source#capture"]),
    );
    wiki::index(temp.path(), &config_path).expect("index wiki");
    let output_path = temp.path().join("llms.txt");
    let stale = fs::read_to_string(&output_path).expect("output") + &"x".repeat(1024 * 1024);
    fs::write(&output_path, &stale).expect("stale output");

    assert!(wiki::check(temp.path(), &config_path, None).is_err());
    assert_eq!(fs::read_to_string(output_path).expect("output"), stale);
}

#[test]
fn config_rejects_wrong_version() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = temp.path().join("wiki.json");
    write(
        &config_path,
        r#"{"version":2,"roots":["docs"],"exclude":[],"required_frontmatter":["title","sources"],"output":"llms.txt"}"#,
    );
    assert!(wiki::index(temp.path(), &config_path).is_err());
}

#[test]
fn config_rejects_unknown_fields() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = temp.path().join("wiki.json");
    write(
        &config_path,
        r#"{"version":1,"roots":["docs"],"exclude":[],"required_frontmatter":["title","sources"],"output":"llms.txt","unknown":true}"#,
    );
    assert!(wiki::index(temp.path(), &config_path).is_err());
}

#[test]
fn index_rejects_oversized_config_and_page() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = temp.path().join("wiki.json");
    write(
        &config_path,
        &(r#"{"version":1,"roots":["docs"],"exclude":[],"required_frontmatter":["title","sources"],"output":"llms.txt"}"#.to_owned()
            + &" ".repeat(1024 * 1024)),
    );
    assert!(wiki::index(temp.path(), &config_path).is_err());

    let config_path = config(temp.path(), "llms.txt");
    write(
        &temp.path().join("docs/page.md"),
        &(page("Page", &["source#capture"]) + &"x".repeat(8 * 1024 * 1024)),
    );
    assert!(wiki::index(temp.path(), &config_path).is_err());
}

#[test]
fn index_failure_preserves_existing_output_for_invalid_config_and_page() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = config(temp.path(), "llms.txt");
    let page_path = temp.path().join("docs/page.md");
    write(&page_path, &page("Page", &["source#capture"]));
    wiki::index(temp.path(), &config_path).expect("index wiki");
    let output_path = temp.path().join("llms.txt");
    let before = fs::read(&output_path).expect("output");

    write(
        &config_path,
        r#"{"version":1,"roots":["docs"],"exclude":[],"required_frontmatter":["title","sources"],"output":"llms.txt","unknown":true}"#,
    );
    assert!(wiki::index(temp.path(), &config_path).is_err());
    assert_eq!(fs::read(&output_path).expect("output"), before);

    let config_path = config(temp.path(), "llms.txt");
    write(&page_path, "---\ntitle: Broken\n");
    assert!(wiki::index(temp.path(), &config_path).is_err());
    assert_eq!(fs::read(&output_path).expect("output"), before);
}
