use graphoxide_core::{Edge, Extraction, Node};
use graphoxide_extract::{extract_project_with_options, resolve_js_module_path};
use std::{fs, path::Path};
use tempfile::TempDir;

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

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
        write(&self.root.path().join(relative), body);
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

fn assert_imports_file(extractions: &[Extraction], target_file: &str) {
    let target = file_id(extractions, target_file);
    assert!(
        edges(extractions).any(|edge| {
            edge.true_target() == target
                && matches!(
                    edge.relation.as_str(),
                    "imports" | "imports_from" | "dynamic_import"
                )
        }),
        "no import edge targets {target_file}; edges={:#?}",
        edges(extractions).collect::<Vec<_>>()
    );
}

#[test]
fn test_resolve_returns_existing_path_unchanged() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("foo.ts");
    write(&target, "export const x = 1");
    assert_eq!(resolve_js_module_path(&target), target);
}

#[test]
fn test_resolve_bare_path_to_ts() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("foo.ts");
    write(&target, "");
    assert_eq!(resolve_js_module_path(&temp.path().join("foo")), target);
}

#[test]
fn test_resolve_bare_path_to_tsx() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("Component.tsx");
    write(&target, "");
    assert_eq!(
        resolve_js_module_path(&temp.path().join("Component")),
        target
    );
}

#[test]
fn test_resolve_bare_path_to_svelte() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("Card.svelte");
    write(&target, "<div></div>");
    assert_eq!(resolve_js_module_path(&temp.path().join("Card")), target);
}

#[test]
fn test_resolve_prefers_ts_over_svelte_when_both_exist() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("foo.ts");
    write(&target, "");
    write(&temp.path().join("foo.svelte"), "");
    assert_eq!(resolve_js_module_path(&temp.path().join("foo")), target);
}

#[test]
fn test_resolve_file_wins_over_sibling_directory() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("auth.ts");
    write(&target, "");
    write(&temp.path().join("auth/helpers.ts"), "");
    assert_eq!(resolve_js_module_path(&temp.path().join("auth")), target);
}

#[test]
fn test_resolve_directory_to_index_ts() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("queue/index.ts");
    write(&target, "");
    assert_eq!(resolve_js_module_path(&temp.path().join("queue")), target);
}

#[test]
fn test_resolve_directory_prefers_index_ts_over_index_js() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("queue/index.ts");
    write(&target, "");
    write(&temp.path().join("queue/index.js"), "");
    assert_eq!(resolve_js_module_path(&temp.path().join("queue")), target);
}

#[test]
fn test_resolve_svelte_to_svelte_ts_for_rune_files() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("is-mobile.svelte.ts");
    write(&target, "");
    assert_eq!(
        resolve_js_module_path(&temp.path().join("is-mobile.svelte")),
        target
    );
}

#[test]
fn test_resolve_svelte_to_svelte_js_for_javascript_rune_files() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("store.svelte.js");
    write(&target, "");
    assert_eq!(
        resolve_js_module_path(&temp.path().join("store.svelte")),
        target
    );
}

#[test]
fn test_resolve_svelte_prefers_svelte_ts_over_svelte_js() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("store.svelte.ts");
    write(&target, "");
    write(&temp.path().join("store.svelte.js"), "");
    assert_eq!(
        resolve_js_module_path(&temp.path().join("store.svelte")),
        target
    );
}

#[test]
fn test_resolve_real_svelte_file_wins_over_svelte_ts_sibling() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("Card.svelte");
    write(&target, "");
    write(&temp.path().join("Card.svelte.ts"), "");
    assert_eq!(resolve_js_module_path(&target), target);
}

#[test]
fn test_resolve_js_to_ts_when_real_file_is_ts() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("foo.ts");
    write(&target, "");
    assert_eq!(resolve_js_module_path(&temp.path().join("foo.js")), target);
}

#[test]
fn test_resolve_jsx_to_tsx_when_real_file_is_tsx() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("Component.tsx");
    write(&target, "");
    assert_eq!(
        resolve_js_module_path(&temp.path().join("Component.jsx")),
        target
    );
}

#[test]
fn test_resolve_returns_unchanged_when_nothing_matches() {
    let temp = TempDir::new().unwrap();
    let missing = temp.path().join("does_not_exist");
    assert_eq!(resolve_js_module_path(&missing), missing);
}

#[test]
fn test_resolve_real_js_stays_js_when_ts_does_not_exist() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("foo.js");
    write(&target, "");
    assert_eq!(resolve_js_module_path(&target), target);
}

#[test]
fn test_bare_path_import_resolves_in_ts_file() {
    let project = Project::new();
    project.write("type-helpers.ts", "export type GetNestedType<T> = T");
    project.write(
        "page.ts",
        "import type { GetNestedType } from './type-helpers'\n",
    );
    assert_imports_file(&project.extract(), "type-helpers.ts");
}

#[test]
fn test_directory_import_resolves_to_index_ts() {
    let project = Project::new();
    project.write("queue/index.ts", "export const enqueue = () => {}");
    project.write("page.ts", "import { enqueue } from './queue'\n");
    assert_imports_file(&project.extract(), "queue/index.ts");
}

#[test]
fn test_dot_svelte_import_resolves_to_dot_svelte_ts() {
    let project = Project::new();
    project.write("is-mobile.svelte.ts", "export const isMobile = () => true");
    project.write("page.ts", "import { isMobile } from './is-mobile.svelte'\n");
    assert_imports_file(&project.extract(), "is-mobile.svelte.ts");
}

#[test]
fn test_explicit_ts_import_still_works() {
    let project = Project::new();
    project.write("foo.ts", "export const x = 1");
    project.write("page.ts", "import { x } from './foo.ts'\n");
    assert_imports_file(&project.extract(), "foo.ts");
}

#[test]
fn test_explicit_svelte_import_still_works() {
    let project = Project::new();
    project.write("Card.svelte", "<div></div>");
    project.write("page.ts", "import Card from './Card.svelte'\n");
    assert_imports_file(&project.extract(), "Card.svelte");
}

#[test]
fn test_external_module_unchanged() {
    let project = Project::new();
    project.write("page.ts", "import _ from 'lodash-es'\n");
    let extractions = project.extract();
    assert!(edges(&extractions).any(|edge| {
        matches!(edge.relation.as_str(), "imports" | "imports_from")
            && edge.true_target().contains("lodash")
    }));
}

fn alias_project(target: &str, statement: &str) -> Vec<Extraction> {
    let project = Project::new();
    project.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"paths":{"$lib":["./src/lib"],"$lib/*":["./src/lib/*"]}}}"#,
    );
    project.write(
        target,
        "export const X = 1\nexport const enqueue = () => {}\n",
    );
    project.write("src/routes/page.ts", statement);
    project.extract()
}

#[test]
fn test_alias_import_with_bare_path_resolves() {
    let result = alias_project(
        "src/lib/type-helpers.ts",
        "import { X } from '$lib/type-helpers'\n",
    );
    assert_imports_file(&result, "src/lib/type-helpers.ts");
}

#[test]
fn test_type_only_import_with_bare_path_resolves() {
    let project = Project::new();
    project.write("type-helpers.ts", "export type GetNestedType<T> = T");
    project.write(
        "page.ts",
        "import type { GetNestedType } from './type-helpers'\n",
    );
    assert_imports_file(&project.extract(), "type-helpers.ts");
}

#[test]
fn test_named_imports_emit_symbol_edges_after_resolution() {
    let project = Project::new();
    project.write("utils.ts", "export const foo = 1\nexport const bar = 2\n");
    project.write("page.ts", "import { foo, bar } from './utils'\n");
    let result = project.extract();
    let targets = nodes(&result)
        .filter(|node| matches!(node.label.as_str(), "foo" | "bar"))
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 2);
    assert!(targets.iter().all(|target| edges(&result)
        .any(|edge| { edge.relation == "imports" && edge.true_target() == target })));
}

#[test]
fn test_alias_directory_import_resolves_to_index_ts() {
    let result = alias_project(
        "src/lib/queue/index.ts",
        "import { enqueue } from '$lib/queue'\n",
    );
    assert_imports_file(&result, "src/lib/queue/index.ts");
}

#[test]
fn test_resolve_does_not_match_partial_directory_name() {
    let temp = TempDir::new().unwrap();
    write(&temp.path().join("foo-extra.ts"), "");
    let bare = temp.path().join("foo");
    assert_eq!(resolve_js_module_path(&bare), bare);
}

#[test]
fn test_resolve_directory_without_index_returns_unchanged() {
    let temp = TempDir::new().unwrap();
    write(&temp.path().join("pkg/not-index.ts"), "");
    let directory = temp.path().join("pkg");
    assert_eq!(resolve_js_module_path(&directory), directory);
}

#[test]
fn test_resolve_handles_subpath_into_directory_with_index() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("foo/sub/index.ts");
    write(&target, "");
    assert_eq!(resolve_js_module_path(&temp.path().join("foo/sub")), target);
}

#[test]
fn test_resolve_does_not_treat_dotfile_as_extension() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join(".env-types.ts");
    write(&target, "");
    assert_eq!(resolve_js_module_path(&target), target);
}

#[test]
fn test_resolve_multi_dot_helper_file() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("tag-action.shared.ts");
    write(&target, "");
    assert_eq!(
        resolve_js_module_path(&temp.path().join("tag-action.shared")),
        target
    );
}

#[test]
fn test_resolve_multi_dot_with_explicit_extension_still_works() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("foo.shared.ts");
    write(&target, "");
    assert_eq!(resolve_js_module_path(&target), target);
}

#[test]
fn test_resolve_ambient_d_ts_via_bare_path() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("ambient.d.ts");
    write(&target, "");
    assert_eq!(
        resolve_js_module_path(&temp.path().join("ambient.d")),
        target
    );
}

#[test]
fn test_end_to_end_multi_dot_import_resolves() {
    let project = Project::new();
    project.write("tag-action.shared.ts", "export const apply = () => {}");
    project.write("page.ts", "import { apply } from './tag-action.shared'\n");
    assert_imports_file(&project.extract(), "tag-action.shared.ts");
}

#[test]
fn test_resolve_chain_alias_and_extension_compose() {
    let result = alias_project(
        "src/lib/hooks/is-mobile.svelte.ts",
        "import { X } from '$lib/hooks/is-mobile.svelte'\n",
    );
    assert_imports_file(&result, "src/lib/hooks/is-mobile.svelte.ts");
}

#[test]
fn test_ts_dynamic_import_bare_path_resolves() {
    let project = Project::new();
    project.write(
        "profanity.ts",
        "export const hasProfanity = (s: string) => false",
    );
    project.write(
        "auth-validators.ts",
        "export async function validate(name: string) { const m = await import('./profanity'); return m.hasProfanity(name) }",
    );
    assert_imports_file(&project.extract(), "profanity.ts");
}

#[test]
fn test_ts_dynamic_import_alias_with_bare_path_resolves() {
    let result = alias_project(
        "src/lib/lazy-module.ts",
        "export async function load() { const m = await import('$lib/lazy-module'); return m.X }",
    );
    assert_imports_file(&result, "src/lib/lazy-module.ts");
}

#[test]
fn test_dynamic_import_bare_path_resolves() {
    let project = Project::new();
    project.write("Heavy.svelte.ts", "export const heavy = () => 1");
    project.write(
        "page.svelte",
        "<script>const lazy = () => import('./Heavy.svelte')</script>",
    );
    assert_imports_file(&project.extract(), "Heavy.svelte.ts");
}
