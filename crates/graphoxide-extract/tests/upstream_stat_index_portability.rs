use filetime::{set_file_mtime, FileTime};
use graphoxide_extract::cache::{
    cache_dir, cached_word_count, file_hash, flush_stat_index, load_cached_value, save_cached_value,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn index_path(root: &Path) -> PathBuf {
    root.join("graphoxide-out/cache/stat-index.json")
}

fn read_index(root: &Path) -> BTreeMap<String, Value> {
    serde_json::from_slice(&fs::read(index_path(root)).unwrap()).unwrap()
}

fn copy_file_preserving_mtime(source: &Path, target: &Path) {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::copy(source, target).unwrap();
    set_file_mtime(
        target,
        FileTime::from_last_modification_time(&source.metadata().unwrap()),
    )
    .unwrap();
}

#[test]
fn test_cache_hits_survive_corpus_move() {
    let temporary = tempdir().unwrap();
    let first = temporary.path().join("a");
    fs::create_dir_all(first.join("sub")).unwrap();
    fs::write(first.join("f1.py"), "x = 1\n").unwrap();
    fs::write(first.join("sub/f2.md"), "hello world one two\n").unwrap();
    let first_hash = file_hash(&first.join("f1.py"), &first).unwrap();
    let second_hash = file_hash(&first.join("sub/f2.md"), &first).unwrap();
    let word_count = cached_word_count(&first.join("f1.py"), &first, |path| {
        Ok(fs::read_to_string(path)?.split_whitespace().count())
    })
    .unwrap();
    flush_stat_index(&first).unwrap();
    let stored = read_index(&first);
    assert_eq!(
        stored.keys().map(String::as_str).collect::<Vec<_>>(),
        ["f1.py", "sub/f2.md"]
    );
    assert!(stored
        .keys()
        .all(|key| !Path::new(key).is_absolute() && !key.contains('\\')));

    let second = temporary.path().join("b");
    copy_file_preserving_mtime(&first.join("f1.py"), &second.join("f1.py"));
    copy_file_preserving_mtime(&first.join("sub/f2.md"), &second.join("sub/f2.md"));
    copy_file_preserving_mtime(&index_path(&first), &index_path(&second));

    // Corrupt the bytes without changing either stat component. A warm lookup
    // must still return the copied index values rather than reading them.
    let first_mtime =
        FileTime::from_last_modification_time(&second.join("f1.py").metadata().unwrap());
    fs::write(second.join("f1.py"), "y = 2\n").unwrap();
    set_file_mtime(second.join("f1.py"), first_mtime).unwrap();
    let markdown_mtime =
        FileTime::from_last_modification_time(&second.join("sub/f2.md").metadata().unwrap());
    fs::write(second.join("sub/f2.md"), "xxxxx xxxxx xxx xxx\n").unwrap();
    set_file_mtime(second.join("sub/f2.md"), markdown_mtime).unwrap();

    assert_eq!(
        file_hash(&second.join("f1.py"), &second).unwrap(),
        first_hash
    );
    assert_eq!(
        file_hash(&second.join("sub/f2.md"), &second).unwrap(),
        second_hash
    );
    assert_eq!(
        cached_word_count(&second.join("f1.py"), &second, |_| {
            anyhow::bail!("word count should be warm")
        })
        .unwrap(),
        word_count
    );
}

#[test]
fn test_deleted_entries_are_pruned_on_flush() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("a");
    fs::create_dir_all(&root).unwrap();
    let first = root.join("f1.py");
    let second = root.join("f2.py");
    fs::write(&first, "x = 1\n").unwrap();
    fs::write(&second, "y = 2\n").unwrap();
    file_hash(&first, &root).unwrap();
    file_hash(&second, &root).unwrap();
    assert_eq!(read_index(&root).len(), 2);
    fs::remove_file(second).unwrap();
    flush_stat_index(&root).unwrap();
    assert_eq!(read_index(&root).keys().collect::<Vec<_>>(), ["f1.py"]);
}

#[test]
fn test_legacy_absolute_index_migrates_gracefully() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("a");
    fs::create_dir_all(index_path(&root).parent().unwrap()).unwrap();
    let file = root.join("f1.py");
    fs::write(&file, "x = 1\n").unwrap();
    let metadata = file.metadata().unwrap();
    let mtime_ns = metadata
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let digest = hex::encode(Sha256::digest(
        [fs::read(&file).unwrap(), b"\0f1.py".to_vec()].concat(),
    ));
    let dead = temporary.path().join("dead/x.py");
    let legacy = json!({
        file.canonicalize().unwrap().display().to_string(): {
            "size": metadata.len(), "mtime_ns": mtime_ns, "hashes": {"f1.py": digest}
        },
        dead.display().to_string(): {"size": 1, "mtime_ns": 1, "hashes": {"x.py": "aa"}}
    });
    fs::write(index_path(&root), serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert_eq!(file_hash(&file, &root).unwrap(), digest);
    flush_stat_index(&root).unwrap();
    assert_eq!(read_index(&root).keys().collect::<Vec<_>>(), ["f1.py"]);
}

#[test]
fn test_out_of_root_key_round_trips_absolute() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("a");
    fs::create_dir_all(&root).unwrap();
    let outside = temporary.path().join("outside.txt");
    fs::write(&outside, "out of root\n").unwrap();
    let digest = file_hash(&outside, &root).unwrap();
    let keys = read_index(&root).keys().cloned().collect::<Vec<_>>();
    assert_eq!(
        keys,
        [outside.canonicalize().unwrap().display().to_string()]
    );
    assert_eq!(file_hash(&outside, &root).unwrap(), digest);
}

#[test]
fn test_relative_key_wins_over_colliding_legacy_absolute() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("a");
    fs::create_dir_all(index_path(&root).parent().unwrap()).unwrap();
    let file = root.join("f1.py");
    fs::write(&file, "x = 1\n").unwrap();
    let metadata = file.metadata().unwrap();
    let mtime_ns = metadata
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let raw = json!({
        file.canonicalize().unwrap().display().to_string(): {
            "size": metadata.len(), "mtime_ns": mtime_ns, "hashes": {"f1.py": "legacy"}
        },
        "f1.py": {
            "size": metadata.len(), "mtime_ns": mtime_ns, "hashes": {"f1.py": "fresh"}
        }
    });
    fs::write(index_path(&root), serde_json::to_vec(&raw).unwrap()).unwrap();
    assert_eq!(file_hash(&file, &root).unwrap(), "fresh");
}

#[test]
fn test_semantic_cache_normalizes_absolute_source_file() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("corpus");
    fs::create_dir_all(&root).unwrap();
    let file = root.join("m.py");
    fs::write(&file, "x = 1\n").unwrap();
    let source = file.canonicalize().unwrap().display().to_string();
    let value = json!({"nodes":[{"id":"m.x","type":"variable","source_file":source}]});
    save_cached_value(&file, &value, &root, "semantic", None).unwrap();
    assert_eq!(value["nodes"][0]["source_file"], source);
    let directory = cache_dir(&root, "semantic", None).unwrap();
    let entry = fs::read_dir(directory)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let persisted: Value = serde_json::from_slice(&fs::read(entry).unwrap()).unwrap();
    assert_eq!(persisted["nodes"][0]["source_file"], "m.py");
    let replay = load_cached_value(&file, &root, "semantic", None).unwrap();
    assert_eq!(replay["nodes"][0]["source_file"], source);
}

#[test]
fn test_semantic_cache_normalizes_backslash_poisoned_source_file() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("corpus");
    fs::create_dir_all(root.join("sub")).unwrap();
    let file = root.join("sub/n.py");
    fs::write(&file, "y = 2\n").unwrap();
    let poisoned = format!("{}\\sub\\n.py", root.canonicalize().unwrap().display());
    let value = json!({"nodes":[{"id":"n.y","type":"variable","source_file":poisoned}]});
    save_cached_value(&file, &value, &root, "semantic", None).unwrap();
    let directory = cache_dir(&root, "semantic", None).unwrap();
    let entry = fs::read_dir(directory)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let persisted: Value = serde_json::from_slice(&fs::read(entry).unwrap()).unwrap();
    assert_eq!(persisted["nodes"][0]["source_file"], "sub/n.py");
}

#[test]
fn test_word_count_cached_until_file_changes() {
    let temporary = tempdir().unwrap();
    let file = temporary.path().join("doc.txt");
    fs::write(&file, "one two three four five").unwrap();
    let calls = Cell::new(0);
    let compute = |path: &Path| {
        calls.set(calls.get() + 1);
        Ok(fs::read_to_string(path)?.split_whitespace().count())
    };
    assert_eq!(
        cached_word_count(&file, temporary.path(), compute).unwrap(),
        5
    );
    assert_eq!(
        cached_word_count(&file, temporary.path(), compute).unwrap(),
        5
    );
    assert_eq!(calls.get(), 1);
    fs::write(&file, "only three words now").unwrap();
    assert_eq!(
        cached_word_count(&file, temporary.path(), compute).unwrap(),
        4
    );
    assert_eq!(calls.get(), 2);
}

#[test]
fn test_word_count_augments_existing_hash_entry() {
    let temporary = tempdir().unwrap();
    let file = temporary.path().join("m.py");
    fs::write(&file, "x = 1\n").unwrap();
    let digest = file_hash(&file, temporary.path()).unwrap();
    assert_eq!(
        cached_word_count(&file, temporary.path(), |path| {
            Ok(fs::read_to_string(path)?.split_whitespace().count())
        })
        .unwrap(),
        3
    );
    let entry = &read_index(temporary.path())["m.py"];
    assert_eq!(entry["hashes"]["m.py"], digest);
    assert_eq!(entry["word_count"], 3);
}

#[test]
fn test_file_hash_is_order_independent_across_roots() {
    let temporary = tempdir().unwrap();
    let first_root = temporary.path().join("a");
    let second_root = temporary.path().join("b");
    fs::create_dir_all(&first_root).unwrap();
    fs::create_dir_all(&second_root).unwrap();
    let file = first_root.join("doc.txt");
    fs::write(&file, "hello world\n").unwrap();
    let content = fs::read(&file).unwrap();
    let relative = hex::encode(Sha256::digest(
        [content.clone(), b"\0doc.txt".to_vec()].concat(),
    ));
    let absolute_salt = file
        .canonicalize()
        .unwrap()
        .display()
        .to_string()
        .replace('\\', "/")
        .to_lowercase();
    let absolute = hex::encode(Sha256::digest(
        [
            content,
            [b"\0".as_slice(), absolute_salt.as_bytes()].concat(),
        ]
        .concat(),
    ));
    assert_eq!(file_hash(&file, &first_root).unwrap(), relative);
    assert_eq!(file_hash(&file, &second_root).unwrap(), absolute);
    assert_eq!(file_hash(&file, &first_root).unwrap(), relative);
}

#[test]
fn test_file_hash_ignores_legacy_unsalted_entry() {
    let temporary = tempdir().unwrap();
    fs::create_dir_all(index_path(temporary.path()).parent().unwrap()).unwrap();
    let file = temporary.path().join("m.py");
    fs::write(&file, "x = 1\n").unwrap();
    let metadata = file.metadata().unwrap();
    let mtime_ns = metadata
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    fs::write(
        index_path(temporary.path()),
        serde_json::to_vec(&json!({"m.py": {
            "size": metadata.len(), "mtime_ns": mtime_ns, "hash": "deadbeef"
        }}))
        .unwrap(),
    )
    .unwrap();
    let expected = hex::encode(Sha256::digest(
        [fs::read(&file).unwrap(), b"\0m.py".to_vec()].concat(),
    ));
    assert_eq!(file_hash(&file, temporary.path()).unwrap(), expected);
    let entry = &read_index(temporary.path())["m.py"];
    assert!(entry.get("hash").is_none());
    assert_eq!(entry["hashes"]["m.py"], expected);
}
