use graphoxide_extract::cache::{check_semantic_cache, save_semantic_cache, SemanticCacheOptions};
use serde_json::json;
use std::{fs, path::Path};
use tempfile::TempDir;

fn count_json(directory: &Path) -> usize {
    let Ok(entries) = fs::read_dir(directory) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                count_json(&path)
            } else {
                usize::from(
                    path.extension()
                        .is_some_and(|extension| extension == "json"),
                )
            }
        })
        .sum()
}

fn roots(temp: &TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let corpus = temp.path().join("corpus");
    let out = temp.path().join("out");
    fs::create_dir(&corpus).unwrap();
    fs::create_dir(&out).unwrap();
    (corpus, out)
}

fn with_cache_root(out: &Path) -> SemanticCacheOptions {
    SemanticCacheOptions {
        cache_root: Some(out.to_path_buf()),
        ..SemanticCacheOptions::default()
    }
}

#[test]
fn test_save_semantic_cache_writes_to_cache_root_not_corpus() {
    let temp = TempDir::new().unwrap();
    let (corpus, out) = roots(&temp);
    let doc = corpus.join("report.md");
    fs::write(&doc, "# Report\nSome content here.").unwrap();
    let report = save_semantic_cache(
        &[json!({"id": "n1", "label": "Report", "source_file": doc})],
        &[],
        &[],
        &corpus,
        &with_cache_root(&out),
    )
    .unwrap();
    assert_eq!(report.saved, 1);
    assert_eq!(count_json(&out.join("graphoxide-out/cache/semantic")), 1);
    assert_eq!(count_json(&corpus.join("graphoxide-out/cache/semantic")), 0);
}

#[test]
fn test_save_semantic_cache_no_corpus_graphify_out_created() {
    let temp = TempDir::new().unwrap();
    let (corpus, out) = roots(&temp);
    let doc = corpus.join("notes.md");
    fs::write(&doc, "Notes content.").unwrap();
    save_semantic_cache(
        &[json!({"id": "x", "label": "X", "source_file": doc})],
        &[],
        &[],
        &corpus,
        &with_cache_root(&out),
    )
    .unwrap();
    assert!(!corpus.join("graphoxide-out").exists());
}

#[test]
fn test_checkpoint_with_cache_root_is_found_by_check_semantic_cache() {
    let temp = TempDir::new().unwrap();
    let (corpus, out) = roots(&temp);
    let doc = corpus.join("paper.md");
    fs::write(&doc, "Some academic content.").unwrap();
    let mut options = with_cache_root(&out);
    options.merge_existing = true;
    options.allowed_source_files = Some([doc.clone()].into_iter().collect());
    save_semantic_cache(
        &[json!({"id": "p1", "label": "Paper", "source_file": doc})],
        &[],
        &[],
        &corpus,
        &options,
    )
    .unwrap();
    let cached = check_semantic_cache(&[corpus.join("paper.md")], &corpus, &options);
    assert!(cached.uncached.is_empty());
    assert!(cached.nodes.iter().any(|node| node["id"] == "p1"));
}

#[test]
fn test_final_save_with_out_root_populates_cache() {
    let temp = TempDir::new().unwrap();
    let (corpus, out) = roots(&temp);
    let doc = corpus.join("report.md");
    fs::write(&doc, "# Annual Report\nKey findings.").unwrap();
    let mut options = with_cache_root(&out);
    options.allowed_source_files = Some([doc].into_iter().collect());
    let report = save_semantic_cache(
        &[json!({"id": "r1", "label": "AnnualReport", "source_file": "report.md"})],
        &[],
        &[],
        &corpus,
        &options,
    )
    .unwrap();
    assert_eq!(report.saved, 1);
    assert_eq!(count_json(&out.join("graphoxide-out/cache/semantic")), 1);
}

#[test]
fn test_final_save_with_wrong_root_emits_warning() {
    let temp = TempDir::new().unwrap();
    let (corpus, out) = roots(&temp);
    fs::write(corpus.join("report.md"), "# Report").unwrap();
    let report = save_semantic_cache(
        &[json!({"id": "r1", "label": "R", "source_file": "report.md"})],
        &[],
        &[],
        &out,
        &SemanticCacheOptions::default(),
    )
    .unwrap();
    assert_eq!(report.saved, 0);
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("#1991")));
}

#[test]
fn test_save_semantic_cache_backward_compat_no_cache_root() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("project");
    fs::create_dir(&root).unwrap();
    let doc = root.join("main.md");
    fs::write(&doc, "Main content.").unwrap();
    let report = save_semantic_cache(
        &[json!({"id": "m1", "label": "Main", "source_file": doc})],
        &[],
        &[],
        &root,
        &SemanticCacheOptions::default(),
    )
    .unwrap();
    assert_eq!(report.saved, 1);
    assert_eq!(count_json(&root.join("graphoxide-out/cache/semantic")), 1);
}

#[test]
fn test_extract_corpus_parallel_accepts_cache_root_kwarg() {
    let temp = TempDir::new().unwrap();
    let options = SemanticCacheOptions {
        cache_root: Some(temp.path().join("managed-output")),
        ..SemanticCacheOptions::default()
    };
    assert_eq!(
        options.cache_root.as_deref(),
        Some(temp.path().join("managed-output").as_path())
    );
}
