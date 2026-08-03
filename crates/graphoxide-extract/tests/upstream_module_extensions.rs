use graphoxide_core::Node;
use graphoxide_extract::{detect, extract, languages};
use std::{collections::BTreeSet, fs, path::Path};
use tempfile::TempDir;

const TYPESCRIPT_SOURCE: &str = "export type Mode = 'a' | 'b';\nexport interface Options { mode: Mode; retries: number; }\nexport function greet(name: string): string { return `hi ${name}`; }\nexport class Widget { render(): void {} }\n";
const COMMONJS_SOURCE: &str = "const path = require('path');\nconst { app, BrowserWindow } = require('electron');\nclass WindowManager { open() { return new BrowserWindow(); } }\nfunction createWindow() { const manager = new WindowManager(); return manager.open(); }\nmodule.exports = { createWindow };\n";

fn extract_source(extension: &str, source: &str) -> Vec<Node> {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join(format!("sample{extension}"));
    fs::write(&path, source).unwrap();
    extract(&path).unwrap().nodes
}

fn symbol_labels(extension: &str, source: &str) -> BTreeSet<String> {
    extract_source(extension, source)
        .into_iter()
        .filter(|node| node.extra.get("type").and_then(serde_json::Value::as_str) != Some("file"))
        .map(|node| node.label)
        .collect()
}

#[test]
fn test_mts_cts_registered_as_code() {
    for extension in ["mts", "cts"] {
        assert_eq!(
            detect::classify_file(Path::new(&format!("source.{extension}"))),
            Some(detect::FileType::Code)
        );
    }
}

#[test]
fn test_mts_cts_in_js_language_family() {
    for extension in ["mts", "cts"] {
        assert_eq!(
            languages::for_path(Path::new(&format!("source.{extension}"))).map(|lang| lang.name),
            Some("typescript")
        );
    }
}

#[test]
fn test_mts_cts_in_js_resolution_sets() {
    let temp = TempDir::new().unwrap();
    for extension in ["mts", "cts"] {
        let target = temp.path().join(format!("source.{extension}"));
        fs::write(&target, "").unwrap();
        assert_eq!(
            graphoxide_extract::resolve_js_module_path(&temp.path().join("source")),
            target
        );
        fs::remove_file(target).unwrap();
    }
}

#[test]
fn test_mts_uses_the_typescript_grammar() {
    let labels = symbol_labels(".mts", TYPESCRIPT_SOURCE);
    assert!(labels.iter().any(|label| label.contains("Mode")));
    assert!(labels.iter().any(|label| label.contains("Options")));
    assert_eq!(labels, symbol_labels(".ts", TYPESCRIPT_SOURCE));
}

#[test]
fn test_cts_uses_the_typescript_grammar() {
    let labels = symbol_labels(".cts", TYPESCRIPT_SOURCE);
    assert!(labels.iter().any(|label| label.contains("Mode")));
    assert!(labels.iter().any(|label| label.contains("Options")));
    assert_eq!(labels, symbol_labels(".ts", TYPESCRIPT_SOURCE));
}

#[test]
fn test_uppercase_typescript_extensions_use_typescript_grammar() {
    for extension in [".TS", ".TSX", ".MTS", ".CTS"] {
        let labels = symbol_labels(extension, TYPESCRIPT_SOURCE);
        assert!(
            labels.iter().any(|label| label.contains("Mode")),
            "{extension}"
        );
        assert!(
            labels.iter().any(|label| label.contains("Options")),
            "{extension}"
        );
    }
}

#[test]
fn test_mts_cts_route_to_extract_js() {
    for extension in [".mts", ".cts"] {
        assert!(!symbol_labels(extension, TYPESCRIPT_SOURCE).is_empty());
    }
}

#[test]
fn test_cjs_registered_as_code() {
    assert_eq!(
        detect::classify_file(Path::new("main.cjs")),
        Some(detect::FileType::Code)
    );
}

#[test]
fn test_cjs_in_extractor_dispatch() {
    let labels = symbol_labels(".cjs", COMMONJS_SOURCE);
    assert!(labels.iter().any(|label| label.contains("WindowManager")));
}

#[test]
fn test_cjs_in_js_language_family() {
    assert_eq!(
        languages::for_path(Path::new("main.cjs")).map(|lang| lang.name),
        Some("javascript")
    );
}

#[test]
fn test_cjs_in_js_resolution_sets() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("main.cjs");
    fs::write(&target, "").unwrap();
    assert_eq!(
        graphoxide_extract::resolve_js_module_path(&temp.path().join("main")),
        target
    );
}

#[test]
fn test_cjs_in_hook_source_exts() {
    // Native hooks trigger a code-only build; classification is the single
    // source-of-truth extension gate used by that build.
    assert_eq!(
        detect::classify_file(Path::new("electron/preload.cjs")),
        Some(detect::FileType::Code)
    );
}

#[test]
fn test_cjs_extracts_like_js() {
    let cjs = symbol_labels(".cjs", COMMONJS_SOURCE);
    assert!(cjs.iter().any(|label| label.contains("WindowManager")));
    assert!(cjs.iter().any(|label| label.contains("createWindow")));
    assert_eq!(cjs, symbol_labels(".js", COMMONJS_SOURCE));
}
