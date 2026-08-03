use graphoxide_core::{
    estimate_file_tokens, estimate_file_tokens_with, expand_oversized_files,
    try_pack_chunks_by_tokens, FileUnit, CHARS_PER_TOKEN, FILE_CHAR_CAP, PER_FILE_OVERHEAD_CHARS,
};
use graphoxide_extract::{
    cache::{load_cached_value, prompt_fingerprint, save_semantic_cache, SemanticCacheOptions},
    semantic_pipeline::{
        extract_corpus, extract_with_adaptive_retry, SemanticChunkResult, SemanticCorpusOptions,
    },
};
use serde_json::json;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

fn write(root: &Path, relative: &str, content: &str) -> PathBuf {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, content).unwrap();
    path
}

fn units(paths: &[PathBuf]) -> Vec<FileUnit> {
    paths.iter().cloned().map(FileUnit::Path).collect()
}

fn options(chunk_size: usize, max_concurrency: usize) -> SemanticCorpusOptions {
    SemanticCorpusOptions {
        token_budget: None,
        chunk_size,
        max_concurrency,
        checkpoint: false,
        ..SemanticCorpusOptions::default()
    }
}

fn stub_result(file_count: usize, index: usize) -> SemanticChunkResult {
    SemanticChunkResult {
        nodes: (0..file_count)
            .map(|item| json!({"id": format!("chunk_{index}_node_{item}")}))
            .collect(),
        input_tokens: 100 * file_count as u64,
        output_tokens: 50 * file_count as u64,
        finish_reason: "stop".into(),
        ..SemanticChunkResult::default()
    }
}

fn result_with_finish(file_count: usize, finish_reason: &str) -> SemanticChunkResult {
    SemanticChunkResult {
        nodes: (0..file_count)
            .map(|index| json!({"id": format!("n_{index}")}))
            .collect(),
        input_tokens: 100 * file_count as u64,
        output_tokens: 50 * file_count as u64,
        finish_reason: finish_reason.into(),
        ..SemanticChunkResult::default()
    }
}

#[test]
fn test_pack_chunks_packs_small_files_together() {
    let temp = TempDir::new().unwrap();
    let files = (0..20)
        .map(|index| write(temp.path(), &format!("small_{index}.py"), "x = 1\n"))
        .collect::<Vec<_>>();
    let chunks = try_pack_chunks_by_tokens(&units(&files), 10_000).unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].len(), files.len());
}

#[test]
fn test_pack_chunks_starts_new_chunk_when_budget_would_overflow() {
    let temp = TempDir::new().unwrap();
    let files = (0..5)
        .map(|index| {
            write(
                temp.path(),
                &format!("file_{index}.py"),
                &"x".repeat(10_000),
            )
        })
        .collect::<Vec<_>>();
    let chunks = try_pack_chunks_by_tokens(&units(&files), 6_000).unwrap();
    assert_eq!(chunks.iter().map(Vec::len).collect::<Vec<_>>(), [2, 2, 1]);
}

#[test]
fn test_pack_chunks_groups_by_directory() {
    let temp = TempDir::new().unwrap();
    let a1 = write(temp.path(), "a/x.py", "a");
    let a2 = write(temp.path(), "a/y.py", "a");
    let b1 = write(temp.path(), "b/x.py", "b");
    let b2 = write(temp.path(), "b/y.py", "b");
    let chunks =
        try_pack_chunks_by_tokens(&units(&[a1.clone(), b1, a2.clone(), b2]), 1_000_000).unwrap();
    assert_eq!(chunks.len(), 1);
    let parents = chunks[0]
        .iter()
        .map(|unit| {
            graphoxide_core::unit_path(unit)
                .parent()
                .unwrap()
                .to_path_buf()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        parents,
        [
            a1.parent().unwrap(),
            a2.parent().unwrap(),
            temp.path().join("b").as_path(),
            temp.path().join("b").as_path()
        ]
    );
}

#[test]
fn test_pack_chunks_oversized_file_gets_its_own_chunk() {
    let temp = TempDir::new().unwrap();
    let big = write(temp.path(), "big.py", &"x".repeat(200_000));
    let small = write(temp.path(), "small.py", "x");
    let chunks = try_pack_chunks_by_tokens(&units(&[big, small]), 1_000).unwrap();
    assert_eq!(chunks.iter().map(Vec::len).collect::<Vec<_>>(), [1, 1]);
}

#[test]
fn test_pack_chunks_rejects_non_positive_budget() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "x.py", "a");
    assert!(try_pack_chunks_by_tokens(&units(&[file]), 0).is_err());
}

#[test]
fn test_estimate_file_tokens_uses_tiktoken_when_available() {
    let temp = TempDir::new().unwrap();
    let text = "def hello():\n    return 'world'\n".repeat(50);
    let file = write(temp.path(), "sample.py", &text);
    let observed = Mutex::new(String::new());
    let count = estimate_file_tokens_with(&FileUnit::Path(file), |content| {
        *observed.lock().unwrap() = content.to_owned();
        999
    });
    assert_eq!(*observed.lock().unwrap(), text);
    assert_eq!(count, 999 + PER_FILE_OVERHEAD_CHARS / CHARS_PER_TOKEN);
}

#[test]
fn test_estimate_file_tokens_falls_back_to_chars_when_no_tokenizer() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "sample.py", &"x".repeat(1_000));
    assert_eq!(
        estimate_file_tokens(&FileUnit::Path(file)),
        (1_000 + PER_FILE_OVERHEAD_CHARS) / CHARS_PER_TOKEN
    );
}

#[test]
fn test_corpus_parallel_runs_chunks_concurrently() {
    let temp = TempDir::new().unwrap();
    let files = (0..8)
        .map(|index| write(temp.path(), &format!("f{index}.py"), "x"))
        .collect::<Vec<_>>();
    let active = AtomicUsize::new(0);
    let peak = AtomicUsize::new(0);
    let start = Instant::now();
    let result = extract_corpus(
        &files,
        temp.path(),
        &options(2, 4),
        &|chunk| {
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(current, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(120));
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(stub_result(chunk.len(), 0))
        },
        None,
    )
    .unwrap();
    assert!(peak.load(Ordering::SeqCst) > 1);
    assert!(start.elapsed() < Duration::from_millis(400));
    assert_eq!(result.nodes.len(), 8);
}

#[test]
fn test_corpus_parallel_sequential_when_max_concurrency_is_one() {
    let temp = TempDir::new().unwrap();
    let files = (0..3)
        .map(|index| write(temp.path(), &format!("f{index}.py"), "x"))
        .collect::<Vec<_>>();
    let calls = Mutex::new(Vec::new());
    extract_corpus(
        &files,
        temp.path(),
        &options(1, 1),
        &|chunk| {
            calls.lock().unwrap().push(
                chunk
                    .iter()
                    .map(|unit| {
                        graphoxide_core::unit_path(unit)
                            .file_name()
                            .unwrap()
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect::<Vec<_>>(),
            );
            Ok(stub_result(chunk.len(), 0))
        },
        None,
    )
    .unwrap();
    assert_eq!(
        *calls.lock().unwrap(),
        vec![
            vec!["f0.py".to_owned()],
            vec!["f1.py".to_owned()],
            vec!["f2.py".to_owned()]
        ]
    );
}

#[test]
fn test_corpus_parallel_merge_order_is_submission_order_not_completion() {
    let temp = TempDir::new().unwrap();
    let files = (0..4)
        .map(|index| write(temp.path(), &format!("f{index}.py"), "x"))
        .collect::<Vec<_>>();
    let result = extract_corpus(
        &files,
        temp.path(),
        &options(1, 4),
        &|chunk| {
            let name = graphoxide_core::unit_path(&chunk[0])
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let index = name.as_bytes()[1] - b'0';
            thread::sleep(Duration::from_millis(20 * u64::from(4 - index)));
            Ok(SemanticChunkResult {
                nodes: vec![json!({"id": format!("node_from_{name}")})],
                edges: vec![json!({"source": format!("node_from_{name}"), "target": "t"})],
                finish_reason: "stop".into(),
                ..SemanticChunkResult::default()
            })
        },
        None,
    )
    .unwrap();
    assert_eq!(
        result
            .nodes
            .iter()
            .map(|node| node["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "node_from_f0.py",
            "node_from_f1.py",
            "node_from_f2.py",
            "node_from_f3.py"
        ]
    );
    assert_eq!(
        result
            .edges
            .iter()
            .map(|edge| edge["source"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "node_from_f0.py",
            "node_from_f1.py",
            "node_from_f2.py",
            "node_from_f3.py"
        ]
    );
}

#[test]
fn test_corpus_parallel_continues_after_chunk_failure() {
    let temp = TempDir::new().unwrap();
    let files = (0..4)
        .map(|index| write(temp.path(), &format!("f{index}.py"), "x"))
        .collect::<Vec<_>>();
    let calls = AtomicUsize::new(0);
    let result = extract_corpus(
        &files,
        temp.path(),
        &options(1, 1),
        &|chunk| {
            let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call == 2 {
                anyhow::bail!("simulated API error");
            }
            Ok(stub_result(chunk.len(), call))
        },
        None,
    )
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(result.nodes.len(), 3);
    assert_eq!(result.failed_chunks, 1);
    assert!(result
        .warnings
        .iter()
        .any(|warning| warning.contains("simulated API error")));
}

#[test]
fn test_checkpoint_scopes_cache_writes_to_chunk_files() {
    let temp = TempDir::new().unwrap();
    let a = write(temp.path(), "A.py", "def a(): pass");
    let b = write(temp.path(), "B.py", "def b(): pass");
    save_semantic_cache(
        &[json!({"id": "b_real", "source_file": "B.py", "file_type": "code"})],
        &[],
        &[],
        temp.path(),
        &SemanticCacheOptions::default(),
    )
    .unwrap();
    let before = load_cached_value(&b, temp.path(), "semantic", None).unwrap();
    assert_eq!(before["nodes"][0]["id"], "b_real");

    let prompt = "chunk extraction prompt";
    let mut run = options(1, 1);
    run.checkpoint = true;
    run.cache.prompt = Some(prompt.into());
    extract_corpus(
        std::slice::from_ref(&a),
        temp.path(),
        &run,
        &|_| {
            Ok(SemanticChunkResult {
                nodes: vec![
                    json!({"id": "a_ok", "source_file": "A.py", "file_type": "code"}),
                    json!({"id": "b_stray", "source_file": "B.py", "file_type": "code"}),
                ],
                finish_reason: "stop".into(),
                ..SemanticChunkResult::default()
            })
        },
        None,
    )
    .unwrap();

    let after = load_cached_value(&b, temp.path(), "semantic", None).unwrap();
    assert_eq!(after["nodes"][0]["id"], "b_real");
    let fingerprint = prompt_fingerprint(prompt);
    let a_cache = load_cached_value(&a, temp.path(), "semantic", Some(&fingerprint)).unwrap();
    assert!(a_cache["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| node["id"] == "a_ok"));
}

#[test]
fn test_truncated_chunk_is_cached_partial_and_missed_on_reload() {
    let temp = TempDir::new().unwrap();
    let doc = write(temp.path(), "doc.md", "# Heading\nlots of prose\n");
    let prompt = "partial extraction prompt";
    let mut run = options(1, 1);
    run.checkpoint = true;
    run.cache.prompt = Some(prompt.into());
    extract_corpus(
        std::slice::from_ref(&doc),
        temp.path(),
        &run,
        &|_| {
            Ok(SemanticChunkResult {
                nodes: vec![json!({"id": "n1", "source_file": "doc.md", "file_type": "document"})],
                finish_reason: "length".into(),
                ..SemanticChunkResult::default()
            })
        },
        None,
    )
    .unwrap();
    let fingerprint = prompt_fingerprint(prompt);
    assert!(load_cached_value(&doc, temp.path(), "semantic", Some(&fingerprint)).is_none());
}

#[test]
fn test_checkpoint_writes_deep_namespace_in_deep_mode() {
    let temp = TempDir::new().unwrap();
    let doc = write(temp.path(), "doc.md", "# Doc\n\nsome content\n");
    let prompt = "deep extraction prompt";
    let mut run = options(1, 1);
    run.checkpoint = true;
    run.cache.mode = Some("deep".into());
    run.cache.prompt = Some(prompt.into());
    extract_corpus(
        std::slice::from_ref(&doc),
        temp.path(),
        &run,
        &|_| {
            Ok(SemanticChunkResult {
                nodes: vec![json!({"id": "d1", "source_file": "doc.md", "file_type": "document"})],
                finish_reason: "stop".into(),
                ..SemanticChunkResult::default()
            })
        },
        None,
    )
    .unwrap();
    let fingerprint = prompt_fingerprint(prompt);
    let deep = load_cached_value(&doc, temp.path(), "semantic-deep", Some(&fingerprint)).unwrap();
    assert_eq!(deep["nodes"][0]["id"], "d1");
    assert!(load_cached_value(&doc, temp.path(), "semantic", Some(&fingerprint)).is_none());
}

#[test]
fn test_omitted_documents_are_reconciled_and_warned() {
    let temp = TempDir::new().unwrap();
    let docs = (0..4)
        .map(|index| write(temp.path(), &format!("doc{index}.md"), "# Doc\n"))
        .collect::<Vec<_>>();
    let result = extract_corpus(&docs, temp.path(), &options(1, 1), &|chunk| {
        let name = graphoxide_core::unit_path(&chunk[0]).file_name().unwrap().to_string_lossy();
        let index = name.as_bytes()[3] - b'0';
        Ok(SemanticChunkResult {
            nodes: index
                .is_multiple_of(2)
                .then(|| json!({"id": format!("n{index}"), "source_file": name, "file_type": "document"}))
                .into_iter()
                .collect(),
            finish_reason: "stop".into(),
            ..SemanticChunkResult::default()
        })
    }, None)
    .unwrap();
    let names = result
        .uncovered_files
        .iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(names, BTreeSet::from(["doc1.md".into(), "doc3.md".into()]));
    assert!(result
        .warnings
        .iter()
        .any(|warning| warning.contains("produced no nodes")));
}

#[test]
fn test_out_of_scope_nodes_are_dropped_from_merged_result() {
    let temp = TempDir::new().unwrap();
    let a = write(temp.path(), "A.md", "# a\n");
    let c = write(temp.path(), "C.md", "# c\n");
    write(temp.path(), "B.py", "def b(): pass\n");
    let result = extract_corpus(&[a, c], temp.path(), &options(2, 1), &|_| {
        Ok(SemanticChunkResult {
            nodes: vec![
                json!({"id": "a_ok", "source_file": "A.md", "file_type": "document"}),
                json!({"id": "c_sibling", "source_file": "C.md", "file_type": "document"}),
                json!({"id": "b_stray", "source_file": "B.py", "file_type": "code"}),
                json!({"id": "auth_flow", "source_file": "auth flow", "file_type": "concept"}),
            ],
            edges: vec![
                json!({"source": "a_ok", "target": "c_sibling", "source_file": "A.md"}),
                json!({"source": "a_ok", "target": "b_stray", "source_file": "A.md"}),
            ],
            hyperedges: vec![
                json!({"id": "h_bad", "nodes": ["a_ok", "c_sibling", "b_stray"], "source_file": "A.md"}),
                json!({"id": "h_ok", "nodes": ["a_ok", "c_sibling", "auth_flow"], "source_file": "A.md"}),
            ],
            finish_reason: "stop".into(),
            ..SemanticChunkResult::default()
        })
    }, None)
    .unwrap();
    let ids = result
        .nodes
        .iter()
        .map(|node| node["id"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert!(!ids.contains("b_stray"));
    assert!(ids.is_superset(&BTreeSet::from(["a_ok", "c_sibling", "auth_flow"])));
    assert_eq!(result.out_of_scope_dropped, 1);
    assert_eq!(result.edges.len(), 1);
    assert_eq!(result.hyperedges.len(), 1);
    assert_eq!(result.hyperedges[0]["id"], "h_ok");
    assert!(result
        .warnings
        .iter()
        .any(|warning| warning.contains("out-of-scope") && warning.contains("B.py")));
    assert!(result.uncovered_files.is_empty());
}

#[test]
fn test_out_of_scope_drop_count_is_zero_when_all_in_scope() {
    let temp = TempDir::new().unwrap();
    let a = write(temp.path(), "A.md", "# a\n");
    let result = extract_corpus(
        &[a],
        temp.path(),
        &options(1, 1),
        &|_| {
            Ok(SemanticChunkResult {
                nodes: vec![json!({"id": "a_ok", "source_file": "A.md", "file_type": "document"})],
                finish_reason: "stop".into(),
                ..SemanticChunkResult::default()
            })
        },
        None,
    )
    .unwrap();
    assert_eq!(result.out_of_scope_dropped, 0);
    assert_eq!(result.nodes[0]["id"], "a_ok");
    assert!(!result
        .warnings
        .iter()
        .any(|warning| warning.contains("out-of-scope")));
}

#[test]
fn test_checkpoint_caches_sliced_document_chunks() {
    let temp = TempDir::new().unwrap();
    let content = format!(
        "# Title\n{}\n## Section\n{}",
        "word ".repeat(12_000),
        "more ".repeat(12_000)
    );
    let doc = write(temp.path(), "big.md", &content);
    let expanded = expand_oversized_files(std::slice::from_ref(&doc), FILE_CHAR_CAP);
    assert!(
        expanded.len() > 1
            && expanded
                .iter()
                .all(|unit| matches!(unit, FileUnit::Slice(_)))
    );
    let prompt = "sliced extraction prompt";
    let mut run = options(1, 1);
    run.checkpoint = true;
    run.cache.prompt = Some(prompt.into());
    let result = extract_corpus(
        std::slice::from_ref(&doc),
        temp.path(),
        &run,
        &|chunk| {
            assert!(chunk.iter().any(|unit| matches!(unit, FileUnit::Slice(_))));
            Ok(SemanticChunkResult {
                nodes: vec![
                    json!({"id": "big_title", "source_file": "big.md", "file_type": "document"}),
                ],
                finish_reason: "stop".into(),
                ..SemanticChunkResult::default()
            })
        },
        None,
    )
    .unwrap();
    assert!(!result
        .warnings
        .iter()
        .any(|warning| warning.contains("incremental cache checkpoint failed")));
    let fingerprint = prompt_fingerprint(prompt);
    let cached = load_cached_value(&doc, temp.path(), "semantic", Some(&fingerprint)).unwrap();
    assert!(cached["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| node["id"] == "big_title"));
}

#[test]
fn test_corpus_parallel_legacy_mode_when_token_budget_is_none() {
    let temp = TempDir::new().unwrap();
    let files = (0..45)
        .map(|index| write(temp.path(), &format!("f{index}.py"), "x"))
        .collect::<Vec<_>>();
    let seen = Mutex::new(Vec::new());
    extract_corpus(
        &files,
        temp.path(),
        &options(20, 1),
        &|chunk| {
            seen.lock().unwrap().push(chunk.len());
            Ok(stub_result(chunk.len(), 0))
        },
        None,
    )
    .unwrap();
    assert_eq!(*seen.lock().unwrap(), [20, 20, 5]);
}

#[test]
fn test_corpus_parallel_token_budget_default_packs_files() {
    let temp = TempDir::new().unwrap();
    let files = (0..50)
        .map(|index| write(temp.path(), &format!("f{index}.py"), "x = 1\n"))
        .collect::<Vec<_>>();
    let seen = Mutex::new(Vec::new());
    let run = SemanticCorpusOptions {
        max_concurrency: 1,
        checkpoint: false,
        ..SemanticCorpusOptions::default()
    };
    extract_corpus(
        &files,
        temp.path(),
        &run,
        &|chunk| {
            seen.lock().unwrap().push(chunk.len());
            Ok(stub_result(chunk.len(), 0))
        },
        None,
    )
    .unwrap();
    assert_eq!(*seen.lock().unwrap(), [50]);
}

#[test]
fn test_adaptive_retry_returns_directly_when_not_truncated() {
    let temp = TempDir::new().unwrap();
    let files = (0..4)
        .map(|index| write(temp.path(), &format!("f{index}.py"), "x"))
        .collect::<Vec<_>>();
    let calls = Mutex::new(Vec::new());
    let result = extract_with_adaptive_retry(&units(&files), 3, &|chunk| {
        calls.lock().unwrap().push(chunk.len());
        Ok(result_with_finish(chunk.len(), "stop"))
    })
    .unwrap();
    assert_eq!(*calls.lock().unwrap(), [4]);
    assert_eq!(result.nodes.len(), 4);
}

#[test]
fn test_adaptive_retry_splits_when_finish_reason_length() {
    let temp = TempDir::new().unwrap();
    let files = (0..4)
        .map(|index| write(temp.path(), &format!("f{index}.py"), "x"))
        .collect::<Vec<_>>();
    let calls = Mutex::new(Vec::new());
    let result = extract_with_adaptive_retry(&units(&files), 3, &|chunk| {
        calls.lock().unwrap().push(chunk.len());
        Ok(result_with_finish(
            chunk.len(),
            if chunk.len() == 4 { "length" } else { "stop" },
        ))
    })
    .unwrap();
    assert_eq!(*calls.lock().unwrap(), [4, 2, 2]);
    assert_eq!(result.nodes.len(), 4);
    assert_eq!(result.finish_reason, "stop");
}

#[test]
fn test_adaptive_retry_recurses_for_persistent_truncation() {
    let temp = TempDir::new().unwrap();
    let files = (0..8)
        .map(|index| write(temp.path(), &format!("f{index}.py"), "x"))
        .collect::<Vec<_>>();
    let calls = Mutex::new(Vec::new());
    let result = extract_with_adaptive_retry(&units(&files), 3, &|chunk| {
        calls.lock().unwrap().push(chunk.len());
        Ok(result_with_finish(
            chunk.len(),
            if chunk.len() > 2 { "length" } else { "stop" },
        ))
    })
    .unwrap();
    let mut observed = calls.into_inner().unwrap();
    observed.sort_unstable();
    assert_eq!(observed, [2, 2, 2, 2, 4, 4, 8]);
    assert_eq!(result.nodes.len(), 8);
}

#[test]
fn test_adaptive_retry_caps_at_max_depth() {
    let temp = TempDir::new().unwrap();
    let files = (0..8)
        .map(|index| write(temp.path(), &format!("f{index}.py"), "x"))
        .collect::<Vec<_>>();
    let calls = AtomicUsize::new(0);
    let result = extract_with_adaptive_retry(&units(&files), 2, &|chunk| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(result_with_finish(chunk.len(), "length"))
    })
    .unwrap();
    assert!(calls.load(Ordering::SeqCst) <= 7);
    assert!(result
        .warnings
        .iter()
        .any(|warning| warning.contains("still truncated")));
}

#[test]
fn test_adaptive_retry_single_file_truncation_does_not_recurse() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "huge.py", "x");
    let calls = AtomicUsize::new(0);
    let result = extract_with_adaptive_retry(&units(&[file]), 3, &|chunk| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(result_with_finish(chunk.len(), "length"))
    })
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(result
        .warnings
        .iter()
        .any(|warning| warning.contains("single-file chunk") && warning.contains("truncated")));
}

#[test]
fn test_adaptive_retry_marks_single_file_truncation_partial() {
    let temp = TempDir::new().unwrap();
    let file = write(temp.path(), "huge.py", "x");
    let result = extract_with_adaptive_retry(&units(&[file]), 3, &|chunk| {
        Ok(result_with_finish(chunk.len(), "length"))
    })
    .unwrap();
    assert!(!result.nodes.is_empty());
    assert!(result.nodes.iter().all(|node| node["_partial"] == true));
}

#[test]
fn test_adaptive_retry_marks_max_depth_giveup_partial() {
    let temp = TempDir::new().unwrap();
    let files = (0..8)
        .map(|index| write(temp.path(), &format!("f{index}.py"), "x"))
        .collect::<Vec<_>>();
    let result = extract_with_adaptive_retry(&units(&files), 2, &|chunk| {
        Ok(result_with_finish(chunk.len(), "length"))
    })
    .unwrap();
    assert!(!result.nodes.is_empty());
    assert!(result.nodes.iter().all(|node| node["_partial"] == true));
}

#[test]
fn test_adaptive_retry_successful_split_is_not_marked_partial() {
    let temp = TempDir::new().unwrap();
    let files = (0..4)
        .map(|index| write(temp.path(), &format!("f{index}.py"), "x"))
        .collect::<Vec<_>>();
    let result = extract_with_adaptive_retry(&units(&files), 3, &|chunk| {
        Ok(result_with_finish(
            chunk.len(),
            if chunk.len() == 4 { "length" } else { "stop" },
        ))
    })
    .unwrap();
    assert!(!result.nodes.is_empty());
    assert!(result
        .nodes
        .iter()
        .all(|node| node.get("_partial").is_none()));
}

#[test]
fn test_corpus_parallel_uses_adaptive_retry() {
    let temp = TempDir::new().unwrap();
    let files = (0..4)
        .map(|index| write(temp.path(), &format!("f{index}.py"), "x"))
        .collect::<Vec<_>>();
    let calls = Mutex::new(Vec::new());
    let callbacks = Mutex::new(Vec::new());
    let callback = |index: usize, total: usize, result: &SemanticChunkResult| {
        callbacks
            .lock()
            .unwrap()
            .push((index, total, result.nodes.len()));
    };
    let result = extract_corpus(
        &files,
        temp.path(),
        &options(4, 1),
        &|chunk| {
            calls.lock().unwrap().push(chunk.len());
            Ok(result_with_finish(
                chunk.len(),
                if chunk.len() == 4 { "length" } else { "stop" },
            ))
        },
        Some(&callback),
    )
    .unwrap();
    assert_eq!(*calls.lock().unwrap(), [4, 2, 2]);
    assert_eq!(*callbacks.lock().unwrap(), [(0, 1, 4)]);
    assert_eq!(result.nodes.len(), 4);
}

#[test]
fn test_estimate_file_tokens_handles_tiktoken_special_token() {
    let temp = TempDir::new().unwrap();
    let file = write(
        temp.path(),
        "tokenizer-notes.md",
        "The GPT end-of-text token is <|endoftext|> in the vocab.\n",
    );
    let count = estimate_file_tokens_with(&FileUnit::Path(file), |content| {
        assert!(content.contains("<|endoftext|>"));
        content.split_whitespace().count()
    });
    assert!(count > 0);
}

#[test]
fn test_pack_chunks_with_special_token_doc_does_not_crash() {
    let temp = TempDir::new().unwrap();
    let doc = write(
        temp.path(),
        "doc.md",
        "see <|endoftext|> and <|im_start|> tokens\n",
    );
    let code = write(temp.path(), "code.py", "def f():\n    return 1\n");
    assert!(!try_pack_chunks_by_tokens(&units(&[doc, code]), 60_000)
        .unwrap()
        .is_empty());
}
