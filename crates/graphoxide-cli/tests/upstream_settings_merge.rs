//! Executable port of upstream `tests/test_settings_merge.py`.

use graphoxide_cli::install::{
    codebuddy_install, gemini_install, install_claude_hook_with_strict, install_codex_hook,
};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

#[derive(Clone, Copy)]
enum Installer {
    Claude,
    CodeBuddy,
    Codex,
    Gemini,
}

impl Installer {
    fn settings(self, root: &Path) -> PathBuf {
        root.join(match self {
            Self::Claude => ".claude/settings.json",
            Self::CodeBuddy => ".codebuddy/settings.json",
            Self::Codex => ".codex/hooks.json",
            Self::Gemini => ".gemini/settings.json",
        })
    }

    fn event(self) -> &'static str {
        match self {
            Self::Gemini => "BeforeTool",
            _ => "PreToolUse",
        }
    }

    fn install(self, root: &Path) -> anyhow::Result<()> {
        let executable = Path::new("graphoxide");
        match self {
            Self::Claude => install_claude_hook_with_strict(root, executable, false),
            Self::CodeBuddy => codebuddy_install(root, executable).map(|_| ()),
            Self::Codex => install_codex_hook(root, executable),
            Self::Gemini => gemini_install(root, executable),
        }
    }
}

fn seed_bytes(root: &Path, installer: Installer, bytes: &[u8]) -> PathBuf {
    let path = installer.settings(root);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    path
}

fn seed(root: &Path, installer: Installer, value: &Value) -> PathBuf {
    seed_bytes(root, installer, &serde_json::to_vec_pretty(value).unwrap())
}

fn read(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn backup(path: &Path) -> PathBuf {
    let file = path.file_name().unwrap().to_string_lossy();
    path.with_file_name(format!("{file}.graphify-bak"))
}

fn assert_bom_merge(installer: Installer) {
    let temp = TempDir::new().unwrap();
    let seeded = json!({"mcpServers":{"keep":{"command":"keep-me"}},"theme":"dark"});
    let mut bytes = b"\xef\xbb\xbf".to_vec();
    bytes.extend(serde_json::to_vec_pretty(&seeded).unwrap());
    let path = seed_bytes(temp.path(), installer, &bytes);
    installer.install(temp.path()).unwrap();
    let result = read(&path);
    assert_eq!(result["mcpServers"], seeded["mcpServers"]);
    assert_eq!(result["theme"], "dark");
    assert!(result["hooks"][installer.event()]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry.to_string().contains("graphoxide")));
}

fn assert_invalid_json(installer: Installer) {
    let temp = TempDir::new().unwrap();
    let path = seed_bytes(temp.path(), installer, b"{ not json");
    let before = fs::read(&path).unwrap();
    let error = installer.install(temp.path()).unwrap_err().to_string();
    assert!(error.contains(&path.display().to_string()), "{error}");
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(!backup(&path).exists());
}

fn assert_non_object(installer: Installer) {
    let temp = TempDir::new().unwrap();
    let path = seed_bytes(temp.path(), installer, br#"["not","an","object"]"#);
    let before = fs::read(&path).unwrap();
    let error = installer.install(temp.path()).unwrap_err().to_string();
    assert!(error.contains(&path.display().to_string()), "{error}");
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(!backup(&path).exists());
}

fn assert_backup_stable(installer: Installer) {
    let temp = TempDir::new().unwrap();
    let path = seed(
        temp.path(),
        installer,
        &json!({"theme":"dark","mcpServers":{"keep":{}}}),
    );
    let before = fs::read(&path).unwrap();
    installer.install(temp.path()).unwrap();
    let merged = fs::read(&path).unwrap();
    assert_ne!(merged, before);
    assert_eq!(fs::read(backup(&path)).unwrap(), before);
    installer.install(temp.path()).unwrap();
    assert_eq!(fs::read(&path).unwrap(), merged);
    assert_eq!(fs::read(backup(&path)).unwrap(), before);
}

fn assert_legacy_entry(installer: Installer) {
    let temp = TempDir::new().unwrap();
    let path = seed(
        temp.path(),
        installer,
        &json!({"hooks":{installer.event():["legacy-string"]}}),
    );
    installer.install(temp.path()).unwrap();
    let entries = read(&path)["hooks"][installer.event()]
        .as_array()
        .unwrap()
        .clone();
    assert!(entries.contains(&json!("legacy-string")));
    assert!(entries
        .iter()
        .any(|entry| entry.to_string().contains("graphoxide")));
}

macro_rules! installer_cases {
    ($bom:ident, $invalid:ident, $non_object:ident, $backup:ident, $legacy:ident, $kind:expr) => {
        #[test]
        fn $bom() {
            assert_bom_merge($kind);
        }
        #[test]
        fn $invalid() {
            assert_invalid_json($kind);
        }
        #[test]
        fn $non_object() {
            assert_non_object($kind);
        }
        #[test]
        fn $backup() {
            assert_backup_stable($kind);
        }
        #[test]
        fn $legacy() {
            assert_legacy_entry($kind);
        }
    };
}

installer_cases!(
    test_bom_settings_are_merged_not_clobbered_claude,
    test_invalid_json_aborts_without_clobbering_claude,
    test_non_object_top_level_aborts_without_clobbering_claude,
    test_backup_written_before_modify_and_stable_on_reinstall_claude,
    test_non_dict_hook_entry_is_preserved_not_fatal_claude,
    Installer::Claude
);
installer_cases!(
    test_bom_settings_are_merged_not_clobbered_codebuddy,
    test_invalid_json_aborts_without_clobbering_codebuddy,
    test_non_object_top_level_aborts_without_clobbering_codebuddy,
    test_backup_written_before_modify_and_stable_on_reinstall_codebuddy,
    test_non_dict_hook_entry_is_preserved_not_fatal_codebuddy,
    Installer::CodeBuddy
);
installer_cases!(
    test_bom_settings_are_merged_not_clobbered_codex,
    test_invalid_json_aborts_without_clobbering_codex,
    test_non_object_top_level_aborts_without_clobbering_codex,
    test_backup_written_before_modify_and_stable_on_reinstall_codex,
    test_non_dict_hook_entry_is_preserved_not_fatal_codex,
    Installer::Codex
);
installer_cases!(
    test_bom_settings_are_merged_not_clobbered_gemini,
    test_invalid_json_aborts_without_clobbering_gemini,
    test_non_object_top_level_aborts_without_clobbering_gemini,
    test_backup_written_before_modify_and_stable_on_reinstall_gemini,
    test_non_dict_hook_entry_is_preserved_not_fatal_gemini,
    Installer::Gemini
);

#[test]
fn test_claude_install_preserves_existing_settings() {
    let temp = TempDir::new().unwrap();
    let seeded = json!({
        "mcpServers":{"context7":{"command":"npx","args":["context7"]}},
        "enabledPlugins":["my-plugin@marketplace"],
        "theme":"dark",
        "hooks":{
            "PostToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"my-formatter"}]}],
            "PreToolUse":[{"matcher":"Write","hooks":[{"type":"command","command":"my-write-guard"}]}]
        }
    });
    let path = seed(temp.path(), Installer::Claude, &seeded);
    install_claude_hook_with_strict(temp.path(), Path::new("graphoxide"), true).unwrap();
    let result = read(&path);
    assert_eq!(result["mcpServers"], seeded["mcpServers"]);
    assert_eq!(result["enabledPlugins"], seeded["enabledPlugins"]);
    assert_eq!(result["theme"], "dark");
    assert_eq!(
        result["hooks"]["PostToolUse"],
        seeded["hooks"]["PostToolUse"]
    );
    let entries = result["hooks"]["PreToolUse"].as_array().unwrap();
    assert!(entries.contains(&seeded["hooks"]["PreToolUse"][0]));
    let owned = entries
        .iter()
        .filter(|entry| entry.to_string().contains("graphoxide"))
        .collect::<Vec<_>>();
    assert_eq!(owned.len(), 2);
    assert!(owned.iter().any(|entry| entry["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .ends_with("--strict")));
}

#[test]
fn test_non_dict_hooks_section_aborts() {
    let temp = TempDir::new().unwrap();
    let path = seed(
        temp.path(),
        Installer::Claude,
        &json!({"hooks":"oops","theme":"dark"}),
    );
    let before = fs::read(&path).unwrap();
    let error = Installer::Claude
        .install(temp.path())
        .unwrap_err()
        .to_string();
    assert!(error.contains(&path.display().to_string()), "{error}");
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(!backup(&path).exists());
}

#[test]
fn test_no_backup_on_fresh_install() {
    let temp = TempDir::new().unwrap();
    Installer::Claude.install(temp.path()).unwrap();
    let path = Installer::Claude.settings(temp.path());
    assert!(path.is_file());
    assert!(!backup(&path).exists());
}
