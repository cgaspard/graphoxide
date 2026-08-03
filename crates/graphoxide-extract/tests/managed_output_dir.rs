use graphoxide_extract::{
    extract_project_with_options_and_output, extract_project_with_options_and_output_filtered,
};
use std::{
    fs,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

#[test]
fn explicit_output_dir_keeps_managed_state_out_of_the_corpus() {
    let parent = std::env::temp_dir().join(format!(
        "graphoxide-managed-output-{}-{}",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    let corpus = parent.join("corpus");
    let output = parent.join("elsewhere");
    fs::create_dir_all(&corpus).expect("create corpus");
    fs::write(corpus.join("demo.py"), "def demo():\n    return 1\n").expect("write source");

    let result = extract_project_with_options_and_output(&corpus, false, &output)
        .expect("extract with explicit output directory");

    assert!(!result.is_empty());
    assert!(output.join("manifest.json").is_file());
    assert!(!corpus.join("graphoxide-out").exists());
    fs::remove_dir_all(&parent).expect("remove fixture");
}

#[test]
fn code_only_filter_excludes_semantic_tiers_before_extraction() {
    let parent = tempfile::TempDir::new().expect("temporary project");
    let corpus = parent.path().join("corpus");
    fs::create_dir_all(&corpus).expect("create corpus");
    fs::write(corpus.join("demo.py"), "def demo():\n    return 1\n").expect("write code");
    fs::write(corpus.join("guide.md"), "# Deployment guide\n").expect("write document");

    let all = extract_project_with_options_and_output_filtered(
        &corpus,
        true,
        &parent.path().join("all-output"),
        false,
    )
    .expect("extract every tier");
    let all_sources = all
        .iter()
        .flat_map(|extraction| extraction.nodes.iter())
        .map(|node| node.source_file.clone())
        .collect::<Vec<_>>();
    assert!(
        all.iter().any(|extraction| {
            extraction
                .nodes
                .iter()
                .any(|node| node.source_file == "guide.md")
        }),
        "unfiltered sources: {all_sources:?}"
    );

    let code = extract_project_with_options_and_output_filtered(
        &corpus,
        true,
        &parent.path().join("code-output"),
        true,
    )
    .expect("extract code only");
    assert!(code.iter().all(|extraction| {
        extraction
            .nodes
            .iter()
            .all(|node| node.source_file != "guide.md")
    }));
    assert!(code.iter().any(|extraction| {
        extraction
            .nodes
            .iter()
            .any(|node| node.source_file == "demo.py")
    }));
}
