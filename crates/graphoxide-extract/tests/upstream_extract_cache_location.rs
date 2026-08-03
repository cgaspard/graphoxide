use graphoxide_extract::{cache::file_hash, extract_files, extract_files_with};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
};
use tempfile::TempDir;

static CWD_LOCK: Mutex<()> = Mutex::new(());

struct CwdGuard(PathBuf);

impl CwdGuard {
    fn enter(path: &Path) -> Self {
        let old = std::env::current_dir().unwrap();
        std::env::set_current_dir(path).unwrap();
        Self(old)
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0).unwrap();
    }
}

fn corpus(root: &Path) -> PathBuf {
    let corpus = root.join("corpus");
    fs::create_dir(&corpus).unwrap();
    fs::write(
        corpus.join("a.py"),
        "class Base:\n    def hello(self):\n        return 1\n",
    )
    .unwrap();
    fs::write(
        corpus.join("b.py"),
        "from a import Base\n\nclass Sub(Base):\n    pass\n",
    )
    .unwrap();
    corpus
}

#[test]
fn test_default_cache_lands_in_cwd_not_source_tree() {
    let _lock = CWD_LOCK.lock().unwrap();
    let temp = TempDir::new().unwrap();
    let corpus = corpus(temp.path());
    let work = temp.path().join("work");
    fs::create_dir(&work).unwrap();
    let _cwd = CwdGuard::enter(&work);
    let result = extract_files(&[corpus.join("a.py"), corpus.join("b.py")], None, false).unwrap();
    assert!(result
        .extractions
        .iter()
        .any(|extraction| !extraction.nodes.is_empty()));
    assert!(!corpus.join("graphoxide-out").exists());
    assert!(work.join("graphoxide-out/cache").is_dir());
}

#[test]
fn test_default_cache_does_not_leave_stat_index_in_source_tree() {
    let _lock = CWD_LOCK.lock().unwrap();
    let temp = TempDir::new().unwrap();
    let corpus = corpus(temp.path());
    let work = temp.path().join("elsewhere");
    fs::create_dir(&work).unwrap();
    let _cwd = CwdGuard::enter(&work);
    extract_files(&[corpus.join("a.py"), corpus.join("b.py")], None, false).unwrap();
    assert!(!corpus.join("graphoxide-out").exists());
    // Graphoxide needs no mutable stat index; all managed state, including the
    // manifest and content cache, remains under the selected output root.
    assert!(work.join("graphoxide-out/manifest.json").is_file());
    assert!(work.join("graphoxide-out/cache").is_dir());
}

#[test]
fn test_explicit_cache_root_still_wins() {
    let _lock = CWD_LOCK.lock().unwrap();
    let temp = TempDir::new().unwrap();
    let corpus = corpus(temp.path());
    let work = temp.path().join("work");
    let out = temp.path().join("out");
    fs::create_dir(&work).unwrap();
    let _cwd = CwdGuard::enter(&work);
    extract_files(&[corpus.join("a.py")], Some(&out), false).unwrap();
    assert!(out.join("graphoxide-out/cache").is_dir());
    assert!(!corpus.join("graphoxide-out").exists());
    assert!(!work.join("graphoxide-out").exists());
}

#[test]
fn test_default_cache_round_trips_via_extract() {
    let _lock = CWD_LOCK.lock().unwrap();
    let temp = TempDir::new().unwrap();
    let corpus = corpus(temp.path());
    let work = temp.path().join("work");
    fs::create_dir(&work).unwrap();
    let _cwd = CwdGuard::enter(&work);
    let file = corpus.join("a.py");
    extract_files(std::slice::from_ref(&file), None, false).unwrap();
    let calls = AtomicUsize::new(0);
    let result = extract_files_with(&[file], None, false, |_, _| {
        calls.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("warm cache unexpectedly re-extracted")
    })
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!result.extractions[0].nodes.is_empty());
}

#[test]
fn test_cache_keys_stay_relative_for_out_of_cwd_corpus() {
    let _lock = CWD_LOCK.lock().unwrap();
    let temp = TempDir::new().unwrap();
    let corpus = corpus(temp.path());
    let work = temp.path().join("elsewhere/work");
    fs::create_dir_all(&work).unwrap();
    let _cwd = CwdGuard::enter(&work);
    let file = corpus.join("a.py");
    extract_files(std::slice::from_ref(&file), None, false).unwrap();
    let root = fs::canonicalize(&corpus).unwrap();
    let key = file_hash(&file, &root).unwrap();
    let raw = fs::read(&file).unwrap();
    let hash = |salt: &str| {
        let mut digest = Sha256::new();
        digest.update(&raw);
        digest.update(b"\0");
        digest.update(salt.as_bytes());
        hex::encode(digest.finalize())
    };
    assert_eq!(key, hash("a.py"));
    assert_ne!(
        key,
        hash(
            &fs::canonicalize(file)
                .unwrap()
                .to_string_lossy()
                .to_lowercase()
        )
    );
}
