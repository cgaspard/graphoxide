use graphoxide_core::{Edge, Extraction, Node};
use graphoxide_extract::extract_project_with_options;
use std::{collections::BTreeSet, fs};
use tempfile::TempDir;

struct Project {
    root: TempDir,
}

impl Project {
    fn new() -> Self {
        Self {
            root: TempDir::new().unwrap(),
        }
    }

    fn write(&self, relative: &str, body: &str) {
        let path = self.root.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn extract(&self) -> Vec<Extraction> {
        extract_project_with_options(self.root.path(), true).unwrap()
    }
}

fn nodes(extractions: &[Extraction]) -> impl Iterator<Item = &Node> {
    extractions.iter().flat_map(|extraction| &extraction.nodes)
}

fn edges(extractions: &[Extraction]) -> impl Iterator<Item = &Edge> {
    extractions.iter().flat_map(|extraction| &extraction.edges)
}

fn file_id(extractions: &[Extraction], source_file: &str) -> String {
    nodes(extractions)
        .find(|node| {
            node.source_file == source_file
                && node.extra.get("type").and_then(serde_json::Value::as_str) == Some("file")
        })
        .unwrap_or_else(|| panic!("missing file node for {source_file}"))
        .id
        .clone()
}

fn import_targets(extractions: &[Extraction]) -> BTreeSet<String> {
    edges(extractions)
        .filter(|edge| {
            matches!(
                edge.relation.as_str(),
                "imports" | "imports_from" | "dynamic_import"
            )
        })
        .map(|edge| edge.true_target().to_owned())
        .collect()
}

fn rails_project(config_name: &str, statement: &str) -> Project {
    let project = Project::new();
    project.write(
        config_name,
        r#"{"compilerOptions":{"baseUrl":"app/javascript"}}"#,
    );
    project.write(
        "app/javascript/mods/Widget.js",
        "export default function Widget() {}\n",
    );
    project.write("app/javascript/packs/dashboard.js", statement);
    project
}

#[test]
fn test_jsconfig_baseurl_static_import_resolves() {
    let project = rails_project(
        "jsconfig.json",
        "import Widget from 'mods/Widget.js';\nexport default Widget;\n",
    );
    let result = project.extract();
    assert!(import_targets(&result).contains(&file_id(&result, "app/javascript/mods/Widget.js")));
}

#[test]
fn test_jsconfig_baseurl_dynamic_import_resolves() {
    let project = rails_project(
        "jsconfig.json",
        "export async function load() { return import('mods/Widget.js'); }\n",
    );
    let result = project.extract();
    assert!(import_targets(&result).contains(&file_id(&result, "app/javascript/mods/Widget.js")));
}

#[test]
fn test_jsconfig_baseurl_extensionless_specifier_resolves() {
    let project = rails_project(
        "jsconfig.json",
        "import Widget from 'mods/Widget';\nexport default Widget;\n",
    );
    let result = project.extract();
    assert!(import_targets(&result).contains(&file_id(&result, "app/javascript/mods/Widget.js")));
}

#[test]
fn test_tsconfig_baseurl_without_paths_also_resolves() {
    let project = rails_project(
        "tsconfig.json",
        "import Widget from 'mods/Widget.js';\nexport default Widget;\n",
    );
    let result = project.extract();
    assert!(import_targets(&result).contains(&file_id(&result, "app/javascript/mods/Widget.js")));
}

#[test]
fn test_declared_paths_alias_still_wins_over_baseurl() {
    let project = Project::new();
    project.write(
        "jsconfig.json",
        r#"{"compilerOptions":{"baseUrl":"app/javascript","paths":{"@mods/*":["real/*"]}}}"#,
    );
    project.write("app/javascript/real/Widget.js", "export default 1;\n");
    project.write("app/javascript/@mods/Widget.js", "export default 2;\n");
    project.write(
        "app/javascript/packs/d.js",
        "import W from '@mods/Widget.js';\nexport default W;\n",
    );
    let result = project.extract();
    let targets = import_targets(&result);
    assert!(targets.contains(&file_id(&result, "app/javascript/real/Widget.js")));
    assert!(!targets.contains(&file_id(&result, "app/javascript/@mods/Widget.js")));
}

#[test]
fn test_declared_directory_prefix_alias_still_wins() {
    let project = Project::new();
    project.write(
        "jsconfig.json",
        r#"{"compilerOptions":{"baseUrl":"app/javascript","paths":{"@lib":["src/lib"]}}}"#,
    );
    project.write("app/javascript/src/lib/util.js", "export default 1;\n");
    project.write("app/javascript/@lib/util.js", "export default 2;\n");
    project.write(
        "app/javascript/packs/p.js",
        "import u from '@lib/util.js';\nexport default u;\n",
    );
    let result = project.extract();
    let targets = import_targets(&result);
    assert!(targets.contains(&file_id(&result, "app/javascript/src/lib/util.js")));
    assert!(!targets.contains(&file_id(&result, "app/javascript/@lib/util.js")));
}

#[test]
fn test_tsconfig_paths_alias_unchanged() {
    let project = Project::new();
    project.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"baseUrl":"./src","paths":{"@services/*":["services/*"]}}}"#,
    );
    project.write("src/services/api.ts", "export const a = 1;\n");
    project.write(
        "src/app/main.ts",
        "import { a } from '@services/api';\nexport default a;\n",
    );
    let result = project.extract();
    assert!(import_targets(&result).contains(&file_id(&result, "src/services/api.ts")));
}

#[test]
fn test_external_package_not_fabricated_under_baseurl() {
    let project = Project::new();
    project.write(
        "jsconfig.json",
        r#"{"compilerOptions":{"baseUrl":"app/javascript"}}"#,
    );
    project.write(
        "app/javascript/packs/p.js",
        "import React from 'react';\nexport default React;\n",
    );
    let result = project.extract();
    assert!(!nodes(&result).any(|node| node.source_file == "app/javascript/react"));
    assert!(
        !import_targets(&result).contains(&graphoxide_core::make_id(&["app/javascript/react",]))
    );
}

#[test]
fn test_relative_import_unaffected_by_baseurl() {
    let project = Project::new();
    project.write(
        "jsconfig.json",
        r#"{"compilerOptions":{"baseUrl":"app/javascript"}}"#,
    );
    project.write("app/javascript/packs/Local.js", "export default 1;\n");
    project.write(
        "app/javascript/packs/main.js",
        "import L from './Local.js';\nexport default L;\n",
    );
    let result = project.extract();
    assert!(import_targets(&result).contains(&file_id(&result, "app/javascript/packs/Local.js")));
}

#[test]
fn test_no_baseurl_declared_changes_nothing() {
    let project = Project::new();
    project.write("jsconfig.json", r#"{"compilerOptions":{}}"#);
    project.write("mods/Widget.js", "export default 1;\n");
    project.write(
        "packs/d.js",
        "import W from 'mods/Widget.js';\nexport default W;\n",
    );
    let result = project.extract();
    assert!(!import_targets(&result).contains(&file_id(&result, "mods/Widget.js")));
}

#[test]
fn test_tsconfig_wins_when_both_configs_present() {
    let project = Project::new();
    project.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"baseUrl":"ts_root"}}"#,
    );
    project.write(
        "jsconfig.json",
        r#"{"compilerOptions":{"baseUrl":"js_root"}}"#,
    );
    project.write("ts_root/mods/W.js", "export default 1;\n");
    project.write("js_root/mods/W.js", "export default 2;\n");
    project.write(
        "ts_root/packs/d.js",
        "import W from 'mods/W.js';\nexport default W;\n",
    );
    let result = project.extract();
    let targets = import_targets(&result);
    assert!(targets.contains(&file_id(&result, "ts_root/mods/W.js")));
    assert!(!targets.contains(&file_id(&result, "js_root/mods/W.js")));
}
