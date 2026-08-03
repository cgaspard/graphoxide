use graphoxide_core::{make_id, Edge, Extraction, Node};
use graphoxide_extract::extract_project_with_options;
use std::{fs, path::Path};
use tempfile::TempDir;

struct Project {
    root: TempDir,
}

impl Project {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().expect("temporary JavaScript project"),
        }
    }

    fn write(&self, relative: &str, source: &str) {
        let path = self.root.path().join(relative);
        fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture parent");
        fs::write(path, source).expect("write JavaScript fixture");
    }

    fn extract(&self) -> Vec<Extraction> {
        extract_project_with_options(self.root.path(), true).expect("extract JavaScript project")
    }

    fn extract_cached(&self) -> Vec<Extraction> {
        extract_project_with_options(self.root.path(), false).expect("extract cached project")
    }

    fn path(&self) -> &Path {
        self.root.path()
    }
}

fn file_id(source: &str) -> String {
    make_id(&[&Path::new(source)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/")])
}

fn symbol_id(source: &str, symbol: &str) -> String {
    make_id(&[
        &Path::new(source)
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/"),
        symbol,
    ])
}

fn nodes(extractions: &[Extraction]) -> impl Iterator<Item = &Node> {
    extractions.iter().flat_map(|value| &value.nodes)
}

fn edges(extractions: &[Extraction]) -> impl Iterator<Item = &Edge> {
    extractions.iter().flat_map(|value| &value.edges)
}

fn has_edge(extractions: &[Extraction], source: &str, target: &str, relation: &str) -> bool {
    let source = file_id(source);
    let target = file_id(target);
    edges(extractions).any(|edge| {
        edge.true_source() == source && edge.true_target() == target && edge.relation == relation
    })
}

fn has_symbol_edge(
    extractions: &[Extraction],
    source: &str,
    target_file: &str,
    symbol: &str,
    relation: &str,
) -> bool {
    let source = file_id(source);
    let target = symbol_id(target_file, symbol);
    edges(extractions).any(|edge| {
        edge.true_source() == source && edge.true_target() == target && edge.relation == relation
    })
}

fn has_symbol_to_symbol_edge(
    extractions: &[Extraction],
    source_file: &str,
    source_symbol: &str,
    target_file: &str,
    target_symbol: &str,
    relation: &str,
) -> bool {
    let source = symbol_id(source_file, source_symbol);
    let target = symbol_id(target_file, target_symbol);
    edges(extractions).any(|edge| {
        edge.true_source() == source && edge.true_target() == target && edge.relation == relation
    })
}

fn basic_import(target: &str, importer: &str, statement: &str) -> Vec<Extraction> {
    let project = Project::new();
    project.write(target, "export const value = 1\n");
    project.write(importer, statement);
    project.extract()
}

#[test]
fn test_ts_bare_relative_import_resolves_existing_ts_file() {
    let result = basic_import(
        "src/lib/foo.ts",
        "src/lib/page.ts",
        "import { value } from './foo'\nconsole.log(value)\n",
    );
    assert!(has_edge(
        &result,
        "src/lib/page.ts",
        "src/lib/foo.ts",
        "imports_from"
    ));
}

#[test]
fn test_ts_directory_import_resolves_index_ts() {
    let result = basic_import(
        "src/lib/server/queue/index.ts",
        "src/lib/page.ts",
        "import { value } from './server/queue'\nconsole.log(value)\n",
    );
    assert!(has_edge(
        &result,
        "src/lib/page.ts",
        "src/lib/server/queue/index.ts",
        "imports_from"
    ));
}

fn named_barrel_fixture(target: &str, barrel: &str, consumer: &str) -> Vec<Extraction> {
    let project = Project::new();
    project.write("src/lib/foo.ts", target);
    project.write("src/lib/index.ts", barrel);
    project.write("src/routes/page.ts", consumer);
    project.extract()
}

#[test]
fn test_ts_named_reexport_alias_from_index_resolves_imported_symbol_to_origin() {
    let result = named_barrel_fixture(
        "export class InternalFoo { id = '' }\n",
        "export { InternalFoo as Foo } from './foo'\n",
        "import type { Foo } from '../lib/index'\nexport type X = Foo\n",
    );
    assert!(has_edge(
        &result,
        "src/lib/index.ts",
        "src/lib/foo.ts",
        "re_exports"
    ));
    assert!(has_symbol_edge(
        &result,
        "src/routes/page.ts",
        "src/lib/foo.ts",
        "InternalFoo",
        "imports"
    ));
}

#[test]
fn test_ts_export_star_from_index_resolves_imported_symbol_to_origin() {
    let result = named_barrel_fixture(
        "export class Foo { id = '' }\n",
        "export * from './foo'\n",
        "import type { Foo } from '../lib/index'\nexport type X = Foo\n",
    );
    assert!(has_symbol_edge(
        &result,
        "src/routes/page.ts",
        "src/lib/foo.ts",
        "Foo",
        "imports"
    ));
}

fn namespace_reexport(suffix: &str) {
    let project = Project::new();
    project.write(
        &format!("src/lib/foo.{suffix}"),
        "export class Foo { id = '' }\n",
    );
    project.write(
        &format!("src/lib/index.{suffix}"),
        "export * as ns from './foo'\n",
    );
    project.write(
        &format!("src/routes/page.{suffix}"),
        "import { ns } from '../lib/index'\nexport const use = () => ns.Foo\n",
    );
    let result = project.extract();
    let namespace = symbol_id(&format!("src/lib/index.{suffix}"), "ns");
    let node_ids = nodes(&result)
        .map(|node| node.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(node_ids.contains(namespace.as_str()));
    assert!(has_symbol_edge(
        &result,
        &format!("src/routes/page.{suffix}"),
        &format!("src/lib/index.{suffix}"),
        "ns",
        "imports"
    ));
    assert!(edges(&result).all(|edge| {
        node_ids.contains(edge.true_source()) && node_ids.contains(edge.true_target())
    }));
}

#[test]
fn test_js_namespace_reexport_import_targets_real_binding_ts() {
    namespace_reexport("ts");
}

#[test]
fn test_js_namespace_reexport_import_targets_real_binding_js() {
    namespace_reexport("js");
}

#[test]
fn test_ts_reexport_cycle_resolves_symbol_from_non_cycle_branch() {
    let project = Project::new();
    project.write("src/lib/foo.ts", "export class Foo { id = '' }\n");
    project.write(
        "src/lib/first.ts",
        "export * from './second'\nexport * from './foo'\n",
    );
    project.write("src/lib/second.ts", "export * from './first'\n");
    project.write(
        "src/routes/page.ts",
        "import type { Foo } from '../lib/first'\nexport type X = Foo\n",
    );
    let result = project.extract();
    assert!(has_symbol_edge(
        &result,
        "src/routes/page.ts",
        "src/lib/foo.ts",
        "Foo",
        "imports"
    ));
}

#[test]
fn test_ts_reexport_chain_beyond_sixteen_hops_resolves_origin() {
    let project = Project::new();
    project.write("src/lib/foo.ts", "export class Foo { id = '' }\n");
    let mut previous = "foo".to_owned();
    for index in 0..20 {
        project.write(
            &format!("src/lib/barrel_{index}.ts"),
            &format!("export * from './{previous}'\n"),
        );
        previous = format!("barrel_{index}");
    }
    project.write(
        "src/routes/page.ts",
        "import type { Foo } from '../lib/barrel_19'\nexport type X = Foo\n",
    );
    let result = project.extract();
    assert!(has_symbol_edge(
        &result,
        "src/routes/page.ts",
        "src/lib/foo.ts",
        "Foo",
        "imports"
    ));
}

#[test]
fn test_ts_import_alias_then_reexport_alias_resolves_imported_symbol_to_origin() {
    let result = named_barrel_fixture(
        "export class Foo { id = '' }\n",
        "import type { Foo as LocalFoo } from './foo'\nexport type { LocalFoo as PublicFoo }\n",
        "import type { PublicFoo } from '../lib/index'\nexport type X = PublicFoo\n",
    );
    assert!(has_symbol_edge(
        &result,
        "src/routes/page.ts",
        "src/lib/foo.ts",
        "Foo",
        "imports"
    ));
}

#[test]
fn test_ts_import_from_index_then_exported_type_alias_resolves_to_origin_symbol() {
    let result = named_barrel_fixture(
        "export class Foo { id = '' }\n",
        "export { Foo } from './foo'\n",
        "import type { Foo } from '../lib/index'\nexport type X = Foo\n",
    );
    assert!(has_symbol_edge(
        &result,
        "src/routes/page.ts",
        "src/lib/foo.ts",
        "Foo",
        "imports"
    ));
}

#[test]
fn test_ts_reexported_interface_resolves_imported_symbol_to_origin() {
    let result = named_barrel_fixture(
        "export interface Foo { id: string }\n",
        "export type { Foo } from './foo'\n",
        "import type { Foo } from '../lib/index'\nexport type X = Foo\n",
    );
    assert!(has_symbol_edge(
        &result,
        "src/routes/page.ts",
        "src/lib/foo.ts",
        "Foo",
        "imports"
    ));
}

#[test]
fn test_ts_reexported_type_alias_resolves_imported_symbol_to_origin() {
    let result = named_barrel_fixture(
        "export type Foo = { id: string }\n",
        "export type { Foo } from './foo'\n",
        "import type { Foo } from '../lib/index'\nexport type X = Foo\n",
    );
    assert!(has_symbol_edge(
        &result,
        "src/routes/page.ts",
        "src/lib/foo.ts",
        "Foo",
        "imports"
    ));
}

#[test]
fn test_ts_reexported_abstract_class_resolves_imported_symbol_to_origin() {
    let result = named_barrel_fixture(
        "export abstract class Foo { abstract run(): void }\n",
        "export { Foo } from './foo'\n",
        "import { Foo } from '../lib/index'\nclass Impl extends Foo { run() {} }\n",
    );
    assert!(has_symbol_edge(
        &result,
        "src/routes/page.ts",
        "src/lib/foo.ts",
        "Foo",
        "imports"
    ));
}

#[test]
fn test_ts_const_alias_reexport_resolves_imported_symbol_to_origin() {
    let result = named_barrel_fixture(
        "export class Foo { id = '' }\n",
        "import { Foo } from './foo'\nexport const PublicFoo = Foo\n",
        "import { PublicFoo } from '../lib/index'\nnew PublicFoo()\n",
    );
    assert!(has_symbol_edge(
        &result,
        "src/routes/page.ts",
        "src/lib/foo.ts",
        "Foo",
        "imports"
    ));
}

#[test]
fn test_ts_local_const_alias_then_named_reexport_resolves_imported_symbol_to_origin() {
    let result = named_barrel_fixture(
        "export function makeFoo() { return {} }\n",
        "import { makeFoo } from './foo'\nconst PublicFactory = makeFoo\nexport { PublicFactory }\n",
        "import { PublicFactory } from '../lib/index'\nPublicFactory()\n",
    );
    assert!(has_symbol_edge(
        &result,
        "src/routes/page.ts",
        "src/lib/foo.ts",
        "makeFoo",
        "imports"
    ));
}

#[test]
fn test_ts_arrow_function_call_through_barrel_targets_origin_symbol() {
    let project = Project::new();
    project.write("src/lib/foo.ts", "export function Foo() { return 1 }\n");
    project.write("src/other/foo.ts", "export function Foo() { return 2 }\n");
    project.write("src/lib/index.ts", "export { Foo } from './foo'\n");
    project.write(
        "src/routes/page.ts",
        "import { Foo } from '../lib/index'\nconst X = () => Foo()\n",
    );
    let result = project.extract();
    assert!(
        has_symbol_to_symbol_edge(
            &result,
            "src/routes/page.ts",
            "X",
            "src/lib/foo.ts",
            "Foo",
            "calls"
        ),
        "edges={:#?}",
        edges(&result).collect::<Vec<_>>()
    );
}

#[test]
fn test_ts_import_alias_does_not_affect_same_named_local_symbol_when_unused() {
    let result = named_barrel_fixture(
        "export function Foo() { return 1 }\n",
        "export { Foo } from './foo'\n",
        "import { Foo as Bar } from '../lib/index'\nconst Foo = () => {}\n",
    );
    assert!(!has_symbol_to_symbol_edge(
        &result,
        "src/routes/page.ts",
        "Foo",
        "src/lib/foo.ts",
        "Foo",
        "calls"
    ));
}

#[test]
fn test_ts_import_alias_call_from_same_named_local_symbol_targets_origin() {
    let result = named_barrel_fixture(
        "export function Foo() { return 1 }\n",
        "export { Foo } from './foo'\n",
        "import { Foo as Bar } from '../lib/index'\nconst Foo = () => Bar()\n",
    );
    assert!(
        has_symbol_to_symbol_edge(
            &result,
            "src/routes/page.ts",
            "Foo",
            "src/lib/foo.ts",
            "Foo",
            "calls"
        ),
        "edges={:#?}",
        edges(&result).collect::<Vec<_>>()
    );
}

#[test]
fn test_svelte_rune_import_resolves_svelte_ts_file() {
    let result = basic_import(
        "src/lib/hooks/is-mobile.svelte.ts",
        "src/routes/page.ts",
        "import { value } from '../lib/hooks/is-mobile.svelte'\nconsole.log(value)\n",
    );
    assert!(has_edge(
        &result,
        "src/routes/page.ts",
        "src/lib/hooks/is-mobile.svelte.ts",
        "imports_from"
    ));
}

#[test]
fn test_ts_dynamic_import_does_not_create_phantom_cycle() {
    let project = Project::new();
    project.write(
        "actions.ts",
        "export function doThing() {}\nexport async function lazy() {\n  const m = await import('./modal');\n  return m.openModal();\n}\n",
    );
    project.write(
        "modal.ts",
        "import { doThing } from './actions';\nexport function openModal() { doThing(); }\n",
    );
    let result = project.extract();
    let deferred = edges(&result)
        .filter(|edge| {
            edge.extra
                .get("deferred")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
        })
        .collect::<Vec<_>>();
    assert!(!deferred.is_empty());
    assert!(deferred.iter().all(|edge| edge.relation == "imports_from"));
    assert!(has_edge(&result, "modal.ts", "actions.ts", "imports_from"));
    assert!(!has_edge(&result, "actions.ts", "modal.ts", "imports_from"));
}

#[test]
fn test_tsconfig_alias_import_resolves_existing_ts_file() {
    let project = Project::new();
    project.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"baseUrl":".","paths":{"$lib/*":["src/lib/*"]}}}"#,
    );
    project.write(
        "src/lib/types/type-helpers.ts",
        "export type Helper = string\n",
    );
    project.write(
        "src/routes/page.ts",
        "import type { Helper } from '$lib/types/type-helpers'\nconst value: Helper = 'x'\n",
    );
    let result = project.extract();
    assert!(has_edge(
        &result,
        "src/routes/page.ts",
        "src/lib/types/type-helpers.ts",
        "imports_from"
    ));
}

#[test]
fn test_tsconfig_alias_with_subdirectory_baseurl_resolves_existing_ts_file() {
    let project = Project::new();
    project.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"baseUrl":"./src","paths":{"@services/*":["services/*"]}}}"#,
    );
    project.write(
        "src/services/foo/index.ts",
        "export class Foo { id = '' }\n",
    );
    project.write(
        "src/routes/page.ts",
        "import { Foo } from '@services/foo'\nnew Foo()\n",
    );
    let result = project.extract();
    assert!(has_edge(
        &result,
        "src/routes/page.ts",
        "src/services/foo/index.ts",
        "imports_from"
    ));
}

#[test]
fn test_tsconfig_array_extends_alias_resolves_existing_ts_file() {
    let project = Project::new();
    project.write(
        "tsconfig.base.json",
        r#"{"compilerOptions":{"strict":true}}"#,
    );
    project.write(
        "tsconfig.paths.json",
        r#"{"compilerOptions":{"baseUrl":".","paths":{"$lib/*":["src/lib/*"]}}}"#,
    );
    project.write(
        "tsconfig.json",
        r#"{"extends":["./tsconfig.base.json","./tsconfig.paths.json"]}"#,
    );
    project.write(
        "src/lib/types/type-helpers.ts",
        "export type Helper = string\n",
    );
    project.write(
        "src/routes/page.ts",
        "import type { Helper } from '$lib/types/type-helpers'\n",
    );
    let result = project.extract();
    assert!(has_edge(
        &result,
        "src/routes/page.ts",
        "src/lib/types/type-helpers.ts",
        "imports_from"
    ));
}

fn default_import_fixture(target: &str, import: &str) -> Vec<Extraction> {
    let project = Project::new();
    project.write("src/lib/foo.ts", target);
    project.write("src/routes/page.ts", import);
    project.extract()
}

#[test]
fn test_default_import_resolves_to_default_exported_class() {
    let result = default_import_fixture(
        "export default class Foo { id = '' }\n",
        "import Foo from '../lib/foo'\nnew Foo()\n",
    );
    assert!(has_symbol_edge(
        &result,
        "src/routes/page.ts",
        "src/lib/foo.ts",
        "Foo",
        "imports"
    ));
}

#[test]
fn test_default_import_with_renamed_binding_resolves_to_origin() {
    let result = default_import_fixture(
        "export default class Foo { id = '' }\n",
        "import Renamed from '../lib/foo'\nnew Renamed()\n",
    );
    assert!(has_symbol_edge(
        &result,
        "src/routes/page.ts",
        "src/lib/foo.ts",
        "Foo",
        "imports"
    ));
}

#[test]
fn test_export_default_identifier_resolves_default_import() {
    let result = default_import_fixture(
        "class Foo { id = '' }\nexport default Foo\n",
        "import Foo from '../lib/foo'\nnew Foo()\n",
    );
    assert!(has_symbol_edge(
        &result,
        "src/routes/page.ts",
        "src/lib/foo.ts",
        "Foo",
        "imports"
    ));
}

#[test]
fn test_default_import_call_resolves_to_default_exported_function() {
    let result = default_import_fixture(
        "export default function makeFoo() { return 1 }\n",
        "import mk from '../lib/foo'\nconst X = () => mk()\n",
    );
    assert!(
        has_symbol_to_symbol_edge(
            &result,
            "src/routes/page.ts",
            "X",
            "src/lib/foo.ts",
            "makeFoo",
            "calls"
        ),
        "edges={:#?}",
        edges(&result).collect::<Vec<_>>()
    );
}

fn workspace_package_fixture(workspace_manifest: (&str, &str)) -> Vec<Extraction> {
    let project = Project::new();
    project.write(workspace_manifest.0, workspace_manifest.1);
    project.write(
        "packages/types/package.json",
        r#"{"name":"@workspace/types","exports":"./src/index.ts"}"#,
    );
    project.write(
        "packages/types/src/index.ts",
        "export interface SomeDto { id: string }\n",
    );
    project.write(
        "apps/web/src/page.ts",
        "import type { SomeDto } from '@workspace/types'\nconst dto: SomeDto = { id: '1' }\n",
    );
    project.extract()
}

#[test]
fn test_pnpm_workspace_package_import_resolves_package_entry() {
    let result = workspace_package_fixture((
        "pnpm-workspace.yaml",
        "packages:\n  - 'apps/*'\n  - 'packages/*'\n",
    ));
    assert!(has_edge(
        &result,
        "apps/web/src/page.ts",
        "packages/types/src/index.ts",
        "imports_from"
    ));
}

#[test]
fn test_npm_workspace_package_import_resolves_package_entry() {
    let result =
        workspace_package_fixture(("package.json", r#"{"workspaces":["apps/*","packages/*"]}"#));
    assert!(has_edge(
        &result,
        "apps/web/src/page.ts",
        "packages/types/src/index.ts",
        "imports_from"
    ));
}

#[test]
fn test_yarn_workspace_package_import_resolves_package_entry() {
    let result = workspace_package_fixture((
        "package.json",
        r#"{"workspaces":{"packages":["apps/*","packages/*"]}}"#,
    ));
    assert!(has_edge(
        &result,
        "apps/web/src/page.ts",
        "packages/types/src/index.ts",
        "imports_from"
    ));
}

#[test]
fn test_pnpm_workspace_takes_precedence_over_package_json_workspaces() {
    let project = Project::new();
    project.write(
        "pnpm-workspace.yaml",
        "packages:\n  - 'apps/*'\n  - 'packages/*'\n",
    );
    project.write("package.json", r#"{"workspaces":["other/*"]}"#);
    project.write(
        "packages/types/package.json",
        r#"{"name":"@workspace/types","exports":"./src/index.ts"}"#,
    );
    project.write(
        "packages/types/src/index.ts",
        "export interface SomeDto { id: string }\n",
    );
    project.write(
        "apps/web/src/page.ts",
        "import type { SomeDto } from '@workspace/types'\n",
    );
    let result = project.extract();
    assert!(has_edge(
        &result,
        "apps/web/src/page.ts",
        "packages/types/src/index.ts",
        "imports_from"
    ));
}

fn workspace_subpath(package_json: &str, target: &str, specifier: &str) -> Vec<Extraction> {
    let project = Project::new();
    project.write(
        "pnpm-workspace.yaml",
        "packages:\n  - 'apps/*'\n  - 'packages/*'\n",
    );
    project.write("packages/pkg-a/package.json", package_json);
    project.write(target, "export const value = 'ok'\n");
    project.write(
        "apps/web/src/consumer.ts",
        &format!("import {{ value }} from '{specifier}'\nexport const v = value\n"),
    );
    project.extract()
}

#[test]
fn test_workspace_subpath_export_string_resolves() {
    let result = workspace_subpath(
        r#"{"name":"@example/pkg-a","exports":{".":"./src/index.ts","./browser":"./src/browser.ts"}}"#,
        "packages/pkg-a/src/browser.ts",
        "@example/pkg-a/browser",
    );
    assert!(has_edge(
        &result,
        "apps/web/src/consumer.ts",
        "packages/pkg-a/src/browser.ts",
        "imports_from"
    ));
}

#[test]
fn test_workspace_subpath_export_condition_object_resolves() {
    let result = workspace_subpath(
        r#"{"name":"@example/pkg-a","exports":{"./browser":{"source":"./src/browser.ts","import":"./dist/esm/browser.js","types":"./dist/types/browser.d.ts"}}}"#,
        "packages/pkg-a/src/browser.ts",
        "@example/pkg-a/browser",
    );
    assert!(has_edge(
        &result,
        "apps/web/src/consumer.ts",
        "packages/pkg-a/src/browser.ts",
        "imports_from"
    ));
}

#[test]
fn test_workspace_subpath_export_wildcard_resolves() {
    let result = workspace_subpath(
        r#"{"name":"@example/pkg-a","exports":{"./*":{"source":"./src/*.ts"}}}"#,
        "packages/pkg-a/src/utils.ts",
        "@example/pkg-a/utils",
    );
    assert!(has_edge(
        &result,
        "apps/web/src/consumer.ts",
        "packages/pkg-a/src/utils.ts",
        "imports_from"
    ));
}

#[test]
fn test_workspace_subpath_export_falls_back_to_filesystem() {
    let result = workspace_subpath(
        r#"{"name":"@example/pkg-a"}"#,
        "packages/pkg-a/browser.ts",
        "@example/pkg-a/browser",
    );
    assert!(has_edge(
        &result,
        "apps/web/src/consumer.ts",
        "packages/pkg-a/browser.ts",
        "imports_from"
    ));
}

#[test]
fn test_workspace_subpath_export_rejects_path_escape() {
    let result = workspace_subpath(
        r#"{"name":"@example/pkg-a","exports":{"./evil":"../../../../secret.ts"}}"#,
        "secret.ts",
        "@example/pkg-a/evil",
    );
    assert!(!has_edge(
        &result,
        "apps/web/src/consumer.ts",
        "secret.ts",
        "imports_from"
    ));
}

#[test]
fn test_workspace_subpath_export_default_consulted_last() {
    let project = Project::new();
    project.write(
        "pnpm-workspace.yaml",
        "packages:\n  - 'apps/*'\n  - 'packages/*'\n",
    );
    project.write(
        "packages/pkg-a/package.json",
        r#"{"name":"@example/pkg-a","exports":{"./browser":{"default":"./src/default-entry.ts","import":"./src/import-entry.ts"}}}"#,
    );
    project.write(
        "packages/pkg-a/src/import-entry.ts",
        "export const value = 'import'\n",
    );
    project.write(
        "packages/pkg-a/src/default-entry.ts",
        "export const value = 'default'\n",
    );
    project.write(
        "apps/web/src/consumer.ts",
        "import { value } from '@example/pkg-a/browser'\n",
    );
    let result = project.extract();
    assert!(has_edge(
        &result,
        "apps/web/src/consumer.ts",
        "packages/pkg-a/src/import-entry.ts",
        "imports_from"
    ));
    assert!(!has_edge(
        &result,
        "apps/web/src/consumer.ts",
        "packages/pkg-a/src/default-entry.ts",
        "imports_from"
    ));
}

#[test]
fn test_js_import_resolution_ignores_stale_importer_cache_when_target_appears() {
    let project = Project::new();
    project.write(
        "src/lib/page.ts",
        "import { foo } from './foo'\nconsole.log(foo)\n",
    );
    let first = project.extract_cached();
    assert!(!has_edge(
        &first,
        "src/lib/page.ts",
        "src/lib/foo.ts",
        "imports_from"
    ));
    project.write("src/lib/foo.ts", "export const foo = 1\n");
    let second = project.extract_cached();
    assert!(has_edge(
        &second,
        "src/lib/page.ts",
        "src/lib/foo.ts",
        "imports_from"
    ));
}

#[test]
fn test_workspace_package_cache_refreshes_between_extract_calls() {
    let project = Project::new();
    project.write(
        "pnpm-workspace.yaml",
        "packages:\n  - 'apps/*'\n  - 'packages/*'\n",
    );
    project.write(
        "apps/web/src/page.ts",
        "import type { SomeDto } from '@workspace/types'\n",
    );
    let first = project.extract_cached();
    assert!(!has_edge(
        &first,
        "apps/web/src/page.ts",
        "packages/types/src/index.ts",
        "imports_from"
    ));
    project.write(
        "packages/types/package.json",
        r#"{"name":"@workspace/types","exports":"./src/index.ts"}"#,
    );
    project.write(
        "packages/types/src/index.ts",
        "export interface SomeDto { id: string }\n",
    );
    let second = project.extract_cached();
    assert!(has_edge(
        &second,
        "apps/web/src/page.ts",
        "packages/types/src/index.ts",
        "imports_from"
    ));
}

#[test]
fn test_pnpm_workspace_dot_package_does_not_crash() {
    let project = Project::new();
    project.write(
        "pnpm-workspace.yaml",
        "packages:\n  - '.'\n  - 'examples/*'\n",
    );
    project.write("package.json", r#"{"name":"my-app"}"#);
    project.write("index.ts", "import { foo } from 'my-app';\n");
    let result = project.extract();
    assert!(!nodes(&result).collect::<Vec<_>>().is_empty());
}

#[test]
fn test_ts_type_relationships_and_contexts() {
    let project = Project::new();
    project.write(
        "src/lib/base.ts",
        "export interface IProcessor<T> { run(input: T): Result<T> }\nexport abstract class BaseProcessor {}\nexport type Result<T> = { value: T }\nexport class Payload {}\n",
    );
    project.write(
        "src/lib/impl.ts",
        "import type { IProcessor, BaseProcessor, Result, Payload } from './base'\nexport abstract class DataProcessor extends BaseProcessor implements IProcessor<Payload> {\n  current!: Result<Payload>\n  run(input: Payload): Result<Payload> { return this.current }\n}\n",
    );
    let result = project.extract();
    assert!(has_symbol_to_symbol_edge(
        &result,
        "src/lib/impl.ts",
        "DataProcessor",
        "src/lib/base.ts",
        "BaseProcessor",
        "inherits"
    ));
    assert!(has_symbol_to_symbol_edge(
        &result,
        "src/lib/impl.ts",
        "DataProcessor",
        "src/lib/base.ts",
        "IProcessor",
        "implements"
    ));
    let labels = nodes(&result)
        .map(|node| {
            (
                node.id.clone(),
                node.label.trim_matches(['.', '(', ')']).to_owned(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let contexts = edges(&result)
        .filter(|edge| edge.relation == "references")
        .map(|edge| {
            (
                labels.get(edge.true_source()).cloned().unwrap_or_default(),
                labels.get(edge.true_target()).cloned().unwrap_or_default(),
                edge.extra
                    .get("context")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    for expected in [
        ("run".into(), "Payload".into(), "parameter_type".into()),
        ("run".into(), "Result".into(), "return_type".into()),
        ("run".into(), "Payload".into(), "generic_arg".into()),
    ] {
        assert!(
            contexts.contains(&expected),
            "missing {expected:?}: {contexts:?}"
        );
    }
}

fn alias_targets_fixture(targets: &str, create: &[(&str, &str)]) -> Vec<Extraction> {
    let project = Project::new();
    project.write(
        "tsconfig.json",
        &format!(r#"{{"compilerOptions":{{"baseUrl":".","paths":{{"$lib/*":{targets}}}}}}}"#),
    );
    for (path, source) in create {
        project.write(path, source);
    }
    project.write(
        "src/routes/page.ts",
        "import { helper } from '$lib/utils'\nconsole.log(helper)\n",
    );
    project.extract()
}

#[test]
fn test_tsconfig_alias_resolves_second_target_when_first_missing() {
    let result = alias_targets_fixture(
        r#"["generated/*","src/lib/*"]"#,
        &[("src/lib/utils.ts", "export const helper = 1\n")],
    );
    assert!(has_edge(
        &result,
        "src/routes/page.ts",
        "src/lib/utils.ts",
        "imports_from"
    ));
}

#[test]
fn test_tsconfig_alias_first_target_wins_when_both_exist() {
    let result = alias_targets_fixture(
        r#"["generated/*","src/lib/*"]"#,
        &[
            ("generated/utils.ts", "export const helper = 1\n"),
            ("src/lib/utils.ts", "export const helper = 2\n"),
        ],
    );
    assert!(has_edge(
        &result,
        "src/routes/page.ts",
        "generated/utils.ts",
        "imports_from"
    ));
    assert!(!has_edge(
        &result,
        "src/routes/page.ts",
        "src/lib/utils.ts",
        "imports_from"
    ));
}

#[test]
fn test_tsconfig_alias_none_exist_creates_no_false_edge() {
    let result = alias_targets_fixture(
        r#"["generated/*","src/lib/*"]"#,
        &[("src/routes/other.ts", "export const x = 1\n")],
    );
    assert!(!has_edge(
        &result,
        "src/routes/page.ts",
        "generated/utils.ts",
        "imports_from"
    ));
    assert!(!has_edge(
        &result,
        "src/routes/page.ts",
        "src/lib/utils.ts",
        "imports_from"
    ));
}

fn wildcard_alias_fixture(
    pattern: &str,
    target_pattern: &str,
    target: &str,
    import: &str,
) -> Vec<Extraction> {
    let project = Project::new();
    project.write(
        "tsconfig.json",
        &format!(
            r#"{{"compilerOptions":{{"baseUrl":".","paths":{{"{pattern}":["{target_pattern}"]}}}}}}"#
        ),
    );
    project.write(target, "export const value = 1\n");
    project.write(
        "src/routes/page.ts",
        &format!("import {{ value }} from '{import}'\n"),
    );
    project.extract()
}

#[test]
fn test_tsconfig_wildcard_alias_substitutes_captured_path() {
    let result = wildcard_alias_fixture(
        "@*",
        "features/*/src/",
        "features/communicate/documentv2/src/index.ts",
        "@communicate/documentv2",
    );
    assert!(has_edge(
        &result,
        "src/routes/page.ts",
        "features/communicate/documentv2/src/index.ts",
        "imports_from"
    ));
}

#[test]
fn test_tsconfig_wildcard_alias_substitutes_before_suffix() {
    let result = wildcard_alias_fixture(
        "@*/interfaces",
        "features/*/src/interfaces.ts",
        "features/communicate/src/interfaces.ts",
        "@communicate/interfaces",
    );
    assert!(has_edge(
        &result,
        "src/routes/page.ts",
        "features/communicate/src/interfaces.ts",
        "imports_from"
    ));
}

#[test]
fn test_tsconfig_wildcard_alias_substitutes_before_normalizing_target() {
    let result = wildcard_alias_fixture(
        "@/*",
        "generated/*/../shared",
        "generated/feature/shared/index.ts",
        "@/feature/nested",
    );
    assert!(has_edge(
        &result,
        "src/routes/page.ts",
        "generated/feature/shared/index.ts",
        "imports_from"
    ));
}

#[test]
fn test_tsconfig_wildcard_alias_allows_empty_capture() {
    let result =
        wildcard_alias_fixture("app*", "src/config/index.ts", "src/config/index.ts", "app");
    assert!(has_edge(
        &result,
        "src/routes/page.ts",
        "src/config/index.ts",
        "imports_from"
    ));
}

#[test]
fn test_tsconfig_wildcard_alias_prefers_longest_matching_prefix() {
    let project = Project::new();
    project.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["fallback/*"],"@/common/integration/*":["preferred/*"]}}}"#,
    );
    project.write(
        "fallback/common/integration/foo.ts",
        "export const Foo = 1\n",
    );
    project.write("preferred/foo.ts", "export const Foo = 2\n");
    project.write(
        "src/routes/page.ts",
        "import { Foo } from '@/common/integration/foo'\n",
    );
    let result = project.extract();
    assert!(has_edge(
        &result,
        "src/routes/page.ts",
        "preferred/foo.ts",
        "imports_from"
    ));
    assert!(!has_edge(
        &result,
        "src/routes/page.ts",
        "fallback/common/integration/foo.ts",
        "imports_from"
    ));
}

#[test]
fn test_tsconfig_exact_alias_still_resolves() {
    let result = wildcard_alias_fixture(
        "app-config",
        "src/config/index.ts",
        "src/config/index.ts",
        "app-config",
    );
    assert!(has_edge(
        &result,
        "src/routes/page.ts",
        "src/config/index.ts",
        "imports_from"
    ));
}

fn alias_project(statement: &str) -> (Project, Vec<Extraction>) {
    let project = Project::new();
    project.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#,
    );
    project.write(
        "src/lib/utils.ts",
        "export function formatDate(d) { return d }\n",
    );
    project.write("src/components/Button.tsx", statement);
    let result = project.extract();
    (project, result)
}

#[test]
fn test_alias_import_edge_resolves_with_relative_input_paths() {
    let (_project, result) = alias_project(
        "import { formatDate } from '@/lib/utils'\nexport function Button() { return formatDate(1) }\n",
    );
    assert!(has_edge(
        &result,
        "src/components/Button.tsx",
        "src/lib/utils.ts",
        "imports_from"
    ));
    assert!(has_symbol_edge(
        &result,
        "src/components/Button.tsx",
        "src/lib/utils.ts",
        "formatDate",
        "imports"
    ));
    let node_ids = nodes(&result)
        .map(|node| node.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        edges(&result)
            .filter(|edge| matches!(edge.relation.as_str(), "imports" | "imports_from"))
            .all(|edge| node_ids.contains(edge.true_source())
                && node_ids.contains(edge.true_target()))
    );
}

#[test]
fn test_alias_import_symbol_resolves_from_parent_working_directory() {
    let (_project, result) = alias_project("import { formatDate } from '@/lib/utils'\n");
    assert!(has_symbol_edge(
        &result,
        "src/components/Button.tsx",
        "src/lib/utils.ts",
        "formatDate",
        "imports"
    ));
}

fn alias_reexport(statement: &str) -> Vec<Extraction> {
    let project = Project::new();
    project.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#,
    );
    project.write(
        "src/lib/utils.ts",
        "export function formatDate() { return 'ok' }\n",
    );
    project.write("src/lib/index.ts", statement);
    project.extract()
}

#[test]
fn test_alias_reexport_symbol_resolves_with_relative_input_paths_named() {
    let result = alias_reexport("export { formatDate } from '@/lib/utils'\n");
    assert!(has_symbol_edge(
        &result,
        "src/lib/index.ts",
        "src/lib/utils.ts",
        "formatDate",
        "re_exports"
    ));
}

#[test]
fn test_alias_reexport_symbol_resolves_with_relative_input_paths_renamed() {
    let result = alias_reexport("export { formatDate as displayDate } from '@/lib/utils'\n");
    assert!(has_symbol_edge(
        &result,
        "src/lib/index.ts",
        "src/lib/utils.ts",
        "formatDate",
        "re_exports"
    ));
}

#[test]
fn test_alias_reexport_symbol_resolves_from_parent_working_directory() {
    let result = alias_reexport("export { formatDate } from '@/lib/utils'\n");
    let node_ids = nodes(&result)
        .map(|node| node.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(edges(&result)
        .filter(|edge| edge.relation == "re_exports")
        .all(|edge| node_ids.contains(edge.true_target())));
}

#[test]
fn test_alias_reexport_does_not_rewrite_an_owned_symbol_id() {
    let project = Project::new();
    let absolute_prefix = make_id(&[&project.path().join("src/lib/utils").to_string_lossy()]);
    let mirror = format!("{absolute_prefix}.ts");
    project.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#,
    );
    project.write(
        "src/lib/utils.ts",
        "export function formatDate() { return 'target' }\n",
    );
    project.write(
        &mirror,
        "export function formatDate() { return 'mirror' }\n",
    );
    project.write(
        "src/lib/index.ts",
        "export { formatDate } from '@/lib/utils'\n",
    );
    let result = project.extract();
    let mirror_id = symbol_id(&mirror, "formatDate");
    assert!(nodes(&result).any(|node| node.id == mirror_id));
    assert!(has_symbol_edge(
        &result,
        "src/lib/index.ts",
        "src/lib/utils.ts",
        "formatDate",
        "re_exports"
    ));
}

#[test]
fn test_alias_import_does_not_remap_an_owned_symbol_id() {
    let project = Project::new();
    let absolute_prefix = make_id(&[&project.path().join("src/lib/utils").to_string_lossy()]);
    let mirror = format!("{absolute_prefix}.ts");
    project.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#,
    );
    project.write(
        "src/lib/utils.ts",
        "export function formatDate(d) { return d }\n",
    );
    project.write(&mirror, "export function formatDate(d) { return 999 }\n");
    project.write(
        "src/components/Button.tsx",
        "import { formatDate } from '@/lib/utils'\nexport const a = formatDate(1)\n",
    );
    project.write(
        "src/components/Mirror.tsx",
        &format!(
            "import {{ formatDate }} from '../../{}'\nexport const b = formatDate(2)\n",
            Path::new(&mirror)
                .file_stem()
                .expect("mirror stem")
                .to_string_lossy()
        ),
    );
    let result = project.extract();
    assert!(has_symbol_edge(
        &result,
        "src/components/Button.tsx",
        "src/lib/utils.ts",
        "formatDate",
        "imports"
    ));
    assert!(has_symbol_edge(
        &result,
        "src/components/Mirror.tsx",
        &mirror,
        "formatDate",
        "imports"
    ));
}

#[test]
fn test_alias_import_preserves_owned_same_line_symbol_edge() {
    let project = Project::new();
    let absolute_prefix = make_id(&[&project.path().join("src/lib/utils").to_string_lossy()]);
    let mirror = format!("{absolute_prefix}.ts");
    project.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#,
    );
    project.write(
        "src/lib/utils.ts",
        "export function formatDate(d) { return d }\n",
    );
    project.write(&mirror, "export function formatDate(d) { return 999 }\n");
    project.write(
        "src/components/Both.tsx",
        &format!(
            "import {{ formatDate as a }} from '@/lib/utils'; import {{ formatDate as b }} from '../../{}';\nexport const value = a(1) + b(2)\n",
            Path::new(&mirror)
                .file_stem()
                .expect("mirror stem")
                .to_string_lossy()
        ),
    );
    let result = project.extract();
    assert!(has_symbol_edge(
        &result,
        "src/components/Both.tsx",
        "src/lib/utils.ts",
        "formatDate",
        "imports"
    ));
    assert!(has_symbol_edge(
        &result,
        "src/components/Both.tsx",
        &mirror,
        "formatDate",
        "imports"
    ));
}

fn barrel_project(extra: &[(&str, &str)]) -> Vec<Extraction> {
    let project = Project::new();
    project.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#,
    );
    project.write(
        "src/lib/utils.ts",
        "export function formatDate() { return 'ok' }\n",
    );
    project.write(
        "src/lib/index.ts",
        "export { formatDate } from '@/lib/utils'\n",
    );
    for (path, source) in extra {
        project.write(path, source);
    }
    project.extract()
}

#[test]
fn test_alias_reexport_through_barrel_resolves_to_defining_symbol() {
    let result = barrel_project(&[("src/barrel2.ts", "export { formatDate } from '@/lib'\n")]);
    assert!(has_symbol_edge(
        &result,
        "src/barrel2.ts",
        "src/lib/utils.ts",
        "formatDate",
        "re_exports"
    ));
}

#[test]
fn test_alias_reexport_two_hop_barrel_chain_resolves() {
    let result = barrel_project(&[
        ("src/barrel2.ts", "export { formatDate } from '@/lib'\n"),
        ("src/barrel3.ts", "export { formatDate } from '@/barrel2'\n"),
    ]);
    assert!(has_symbol_edge(
        &result,
        "src/barrel3.ts",
        "src/lib/utils.ts",
        "formatDate",
        "re_exports"
    ));
}

#[test]
fn test_no_symbol_edge_target_contains_checkout_prefix() {
    let project = Project::new();
    project.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#,
    );
    project.write(
        "src/lib/utils.ts",
        "export function formatDate() { return 'ok' }\n",
    );
    project.write(
        "src/lib/index.ts",
        "export { formatDate } from '@/lib/utils'\n",
    );
    project.write("src/barrel2.ts", "export { formatDate } from '@/lib'\n");
    project.write(
        "src/consumer.ts",
        "import { formatDate } from '@/lib'\nexport function useIt() { return formatDate() }\n",
    );
    let checkout_prefix = make_id(&[&project.path().to_string_lossy()]);
    let result = project.extract();
    assert!(edges(&result)
        .filter(|edge| matches!(edge.relation.as_str(), "re_exports" | "imports"))
        .all(|edge| !edge.true_target().starts_with(&checkout_prefix)));
}

#[test]
fn test_ambiguous_barrel_reexport_chain_does_not_guess() {
    let project = Project::new();
    project.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#,
    );
    project.write("src/lib/a.ts", "export function dup() { return 'a' }\n");
    project.write("src/lib/b.ts", "export function dup() { return 'b' }\n");
    project.write(
        "src/lib/index.ts",
        "export { dup } from '@/lib/a'\nexport { dup } from '@/lib/b'\n",
    );
    project.write(
        "src/consumer.ts",
        "import { dup } from '@/lib'\nexport function useIt() { return dup() }\n",
    );
    let result = project.extract();
    let barrel_symbol = symbol_id("src/lib/index.ts", "dup");
    assert!(edges(&result).any(|edge| {
        edge.true_source() == file_id("src/consumer.ts")
            && edge.relation == "imports"
            && edge.true_target() == barrel_symbol
    }));
    for target in [
        symbol_id("src/lib/a.ts", "dup"),
        symbol_id("src/lib/b.ts", "dup"),
    ] {
        assert!(edges(&result).any(|edge| {
            edge.true_source() == file_id("src/lib/index.ts")
                && edge.relation == "re_exports"
                && edge.true_target() == target
        }));
    }
}
