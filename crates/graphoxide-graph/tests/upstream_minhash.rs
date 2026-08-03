use graphoxide_graph::{optimal_lsh_params, MinHash, MinHashLsh};

fn minhash_for(text: &str, num_perm: usize) -> MinHash {
    let mut sketch = MinHash::new(num_perm).unwrap();
    let chars = text.chars().collect::<Vec<_>>();
    for window in chars.windows(3) {
        sketch.update(window.iter().collect::<String>().as_bytes());
    }
    sketch
}

fn overlap(left: &MinHash, right: &MinHash) -> f64 {
    left.hashvalues()
        .iter()
        .zip(right.hashvalues())
        .filter(|(a, b)| a == b)
        .count() as f64
        / left.hashvalues().len() as f64
}

#[test]
fn test_identical_texts_produce_identical_hashvalues() {
    assert_eq!(
        minhash_for("graphextractor", 128).hashvalues(),
        minhash_for("graphextractor", 128).hashvalues()
    );
}

#[test]
fn test_similar_texts_share_most_hashvalues() {
    assert!(
        overlap(
            &minhash_for("authentication manager", 128),
            &minhash_for("authentication managers", 128)
        ) > 0.5
    );
}

#[test]
fn test_unrelated_texts_share_few_hashvalues() {
    assert!(
        overlap(
            &minhash_for("authentication manager", 128),
            &minhash_for("file system watcher", 128)
        ) < 0.3
    );
}

#[test]
fn test_update_mutates_hashvalues() {
    let mut sketch = MinHash::new(64).unwrap();
    let before = sketch.hashvalues().to_vec();
    sketch.update(b"hello");
    assert_ne!(sketch.hashvalues(), before);
}

#[test]
fn test_near_duplicates_are_candidates() {
    let first = minhash_for("authentication manager", 128);
    let second = minhash_for("authentication managers", 128);
    let mut index = MinHashLsh::new(0.5, 128).unwrap();
    index.insert("a", first.clone()).unwrap();
    index.insert("b", second).unwrap();
    assert!(index.query(&first).contains(&"b".to_owned()));
}

#[test]
fn test_unrelated_strings_not_candidates() {
    let first = minhash_for("authentication manager", 128);
    let second = minhash_for("file system watcher", 128);
    let mut index = MinHashLsh::new(0.5, 128).unwrap();
    index.insert("a", first.clone()).unwrap();
    index.insert("b", second).unwrap();
    assert!(!index.query(&first).contains(&"b".to_owned()));
}

#[test]
fn test_query_always_returns_self() {
    let sketch = minhash_for("graphextractor", 128);
    let mut index = MinHashLsh::new(0.5, 128).unwrap();
    index.insert("x", sketch.clone()).unwrap();
    assert!(index.query(&sketch).contains(&"x".to_owned()));
}

#[test]
fn test_duplicate_insert_raises() {
    let sketch = minhash_for("foo", 128);
    let mut index = MinHashLsh::new(0.5, 128).unwrap();
    index.insert("key", sketch.clone()).unwrap();
    assert!(index
        .insert("key", sketch)
        .unwrap_err()
        .to_string()
        .contains("already exists"));
}

#[test]
fn test_optimal_params_within_budget() {
    let (bands, rows) = optimal_lsh_params(0.5, 128);
    assert!(bands >= 1 && rows >= 1 && bands * rows <= 128);
}

#[test]
fn test_optimal_params_cached() {
    assert_eq!(optimal_lsh_params(0.7, 128), optimal_lsh_params(0.7, 128));
}

#[test]
fn test_dedup_import_does_not_pull_scipy_or_numpy_testing() {
    let manifest = include_str!("../Cargo.toml");
    assert!(!manifest.contains("scipy"));
    assert!(!manifest.contains("numpy"));
}
