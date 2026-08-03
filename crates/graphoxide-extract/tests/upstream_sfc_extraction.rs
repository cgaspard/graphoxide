use graphoxide_core::{make_id, Edge, Extraction, Node};
use graphoxide_extract::{
    detect, extract_files, extract_project_with_options, mask_vue_non_script,
};
use std::{collections::BTreeSet, fs, path::PathBuf};
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

    fn write(&self, relative: &str, body: &str) -> PathBuf {
        let path = self.root.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        path
    }

    fn raw(&self, relative: &str) -> Extraction {
        graphoxide_extract::extract(&self.root.path().join(relative)).unwrap()
    }

    fn extract(&self) -> Vec<Extraction> {
        extract_project_with_options(self.root.path(), true).unwrap()
    }

    fn extract_files(&self, relative: &[&str]) -> Vec<Extraction> {
        let paths = relative
            .iter()
            .map(|path| self.root.path().join(path))
            .collect::<Vec<_>>();
        extract_files(&paths, Some(self.root.path()), true)
            .unwrap()
            .extractions
    }
}

fn nodes(extractions: &[Extraction]) -> impl Iterator<Item = &Node> {
    extractions.iter().flat_map(|extraction| &extraction.nodes)
}

fn edges(extractions: &[Extraction]) -> impl Iterator<Item = &Edge> {
    extractions.iter().flat_map(|extraction| &extraction.edges)
}

fn targets(extraction: &Extraction, relation: &str) -> BTreeSet<String> {
    extraction
        .edges
        .iter()
        .filter(|edge| edge.relation == relation)
        .map(|edge| edge.true_target().to_owned())
        .collect()
}

fn raw_path_id(path: &std::path::Path) -> String {
    make_id(&[&path.to_string_lossy()])
}

fn file_id(source_file: &str) -> String {
    make_id(&[&std::path::Path::new(source_file)
        .with_extension("")
        .to_string_lossy()])
}

fn has_edge(extractions: &[Extraction], source: &str, target: &str, relation: &str) -> bool {
    edges(extractions).any(|edge| {
        edge.true_source() == source && edge.true_target() == target && edge.relation == relation
    })
}

#[test]
fn test_vue_is_in_code_extensions() {
    assert!(detect::is_supported_path(std::path::Path::new(
        "Component.vue"
    )));
}

#[test]
fn test_mask_preserves_line_numbers_and_blanks_markup() {
    let source = concat!(
        "<template>\n",
        "  <div>{{ msg }}</div>\n",
        "</template>\n",
        "\n",
        "<script setup lang=\"ts\">\n",
        "const msg = 'hi'\n",
        "</script>\n"
    );
    let (masked, language) = mask_vue_non_script(source);
    assert_eq!(language.as_deref(), Some("ts"));
    assert_eq!(masked.matches('\n').count(), source.matches('\n').count());
    assert!(!masked.contains("div"));
    assert!(masked.contains("const msg = 'hi'"));
    assert_eq!(masked.lines().nth(5).unwrap().trim(), "const msg = 'hi'");
}

#[test]
fn test_script_setup_ts_static_imports_resolve() {
    let project = Project::new();
    let child = project.write("Child.vue", "<template><div/></template>\n");
    let helper = project.write("utils/helper.ts", "export function helper(){}\n");
    project.write(
        "Comp.vue",
        "<template>\n  <Child />\n</template>\n\n<script setup lang=\"ts\">\nimport Child from './Child.vue'\nimport { helper } from './utils/helper'\nhelper()\n</script>\n",
    );
    let extraction = project.raw("Comp.vue");
    let imports = targets(&extraction, "imports_from");
    assert!(imports.contains(&raw_path_id(&child)));
    assert!(imports.contains(&raw_path_id(&helper)));
}

#[test]
fn test_script_setup_extracts_symbols_with_correct_lines() {
    let project = Project::new();
    project.write(
        "Widget.vue",
        "<template>\n  <button @click=\"onClick\">x</button>\n</template>\n\n<script setup lang=\"ts\">\nimport { ref } from 'vue'\n\nconst count = ref(0)\n\nfunction onClick(): void {\n  count.value += 1\n}\n</script>\n",
    );
    let extraction = project.raw("Widget.vue");
    let count = extraction
        .nodes
        .iter()
        .find(|node| node.label == "count")
        .expect("count variable");
    let on_click = extraction
        .nodes
        .iter()
        .find(|node| node.label == "onClick()")
        .expect("onClick function");
    assert_eq!(count.source_location.as_deref(), Some("L8"));
    assert_eq!(on_click.source_location.as_deref(), Some("L10"));
}

#[test]
fn test_typed_props_reference_imported_type() {
    let project = Project::new();
    let types = project.write("types.ts", "export interface Thing { id: number }\n");
    project.write(
        "Typed.vue",
        "<script setup lang=\"ts\">\nimport type { Thing } from './types'\n\ndefineProps<{ item: Thing }>()\n\nfunction use(x: Thing): Thing {\n  return x\n}\n</script>\n\n<template><div/></template>\n",
    );
    assert!(targets(&project.raw("Typed.vue"), "imports_from").contains(&raw_path_id(&types)));
}

#[test]
fn test_two_script_blocks_both_parsed() {
    let project = Project::new();
    let a = project.write("a.ts", "export const a = 1\n");
    let b = project.write("b.ts", "export const b = 2\n");
    project.write(
        "Dual.vue",
        "<script lang=\"ts\">\nimport { a } from './a'\nexport default { name: 'Dual' }\n</script>\n\n<script setup lang=\"ts\">\nimport { b } from './b'\n</script>\n\n<template><div/></template>\n",
    );
    let imports = targets(&project.raw("Dual.vue"), "imports_from");
    assert!(imports.contains(&raw_path_id(&a)));
    assert!(imports.contains(&raw_path_id(&b)));
}

#[test]
fn test_dynamic_import_recovered() {
    let project = Project::new();
    let lazy = project.write("Lazy.vue", "<template><div/></template>\n");
    project.write(
        "Host.vue",
        "<script setup lang=\"ts\">\nimport { defineAsyncComponent } from 'vue'\nconst Lazy = defineAsyncComponent(() => import('./Lazy.vue'))\n</script>\n\n<template><Lazy /></template>\n",
    );
    assert!(targets(&project.raw("Host.vue"), "dynamic_import").contains(&raw_path_id(&lazy)));
}

#[test]
fn test_plain_js_script_block() {
    let project = Project::new();
    let dependency = project.write("dep.js", "export const x = 1\n");
    project.write(
        "Legacy.vue",
        "<script>\nimport { x } from './dep'\nexport default { name: 'Legacy' }\n</script>\n\n<template><div/></template>\n",
    );
    assert!(targets(&project.raw("Legacy.vue"), "imports_from").contains(&raw_path_id(&dependency)));
}

#[test]
fn test_template_only_file_does_not_crash() {
    let project = Project::new();
    project.write("Static.vue", "<template>\n  <h1>hi</h1>\n</template>\n");
    let extraction = project.raw("Static.vue");
    assert_eq!(extraction.nodes.len(), 1);
    assert!(targets(&extraction, "imports_from").is_empty());
}

#[test]
fn test_whole_file_to_js_grammar_would_extract_nothing() {
    let project = Project::new();
    let dependency = project.write("dep.ts", "export const v = 1\n");
    project.write(
        "Guard.vue",
        "<template>\n  <div class=\"x\" :data-y=\"z\">markup that is not valid JS</div>\n</template>\n\n<script setup lang=\"ts\">\nimport { v } from './dep'\nconst z = v\n</script>\n",
    );
    assert!(targets(&project.raw("Guard.vue"), "imports_from").contains(&raw_path_id(&dependency)));
}

#[test]
fn test_vue_joins_cross_file_symbol_resolution() {
    let project = Project::new();
    project.write("helper.ts", "export function helper() {}\n");
    project.write(
        "Caller.vue",
        "<script setup lang=\"ts\">\nimport { helper } from './helper'\n\nfunction go(): void {\n  helper()\n}\n</script>\n\n<template><div @click=\"go\" /></template>\n",
    );
    let result = project.extract();
    let go = nodes(&result)
        .find(|node| node.label == "go()")
        .expect("go node")
        .id
        .clone();
    let helper = nodes(&result)
        .find(|node| node.label == "helper()")
        .expect("helper node")
        .id
        .clone();
    assert!(has_edge(&result, &go, &helper, "calls"));
}

#[test]
fn test_generic_component_open_tag_with_angle_brackets() {
    let project = Project::new();
    let helper = project.write("utils/helper.ts", "export function helper(){}\n");
    project.write(
        "Generic.vue",
        "<template><div/></template>\n<script setup lang=\"ts\" generic=\"T extends Record<string, unknown>\">\nimport { helper } from './utils/helper'\nconst value = helper()\n</script>\n",
    );
    let extraction = project.raw("Generic.vue");
    assert!(targets(&extraction, "imports_from").contains(&raw_path_id(&helper)));
    let source = fs::read_to_string(project.root.path().join("Generic.vue")).unwrap();
    let (masked, language) = mask_vue_non_script(&source);
    assert_eq!(language.as_deref(), Some("ts"));
    assert!(!masked.contains("generic=\"T extends Record"));
    assert!(masked.contains("import { helper }"));
}

#[test]
fn test_astro_is_in_code_extensions() {
    assert!(detect::is_supported_path(std::path::Path::new(
        "Page.astro"
    )));
}

#[test]
fn test_extract_astro_picks_up_frontmatter_static_imports() {
    let project = Project::new();
    let layout = project.write("src/layouts/Layout.astro", "---\n---\n<slot />\n");
    let hero = project.write("src/components/Hero.astro", "---\n---\n<h1>hi</h1>\n");
    project.write(
        "src/pages/index.astro",
        "---\nimport Layout from '../layouts/Layout.astro';\nimport Hero from '../components/Hero.astro';\nconst { title } = Astro.props;\n---\n\n<Layout title={title}>\n  <Hero />\n</Layout>\n",
    );
    let imports = targets(&project.raw("src/pages/index.astro"), "imports_from");
    assert!(imports.contains(&raw_path_id(&layout)));
    assert!(imports.contains(&raw_path_id(&hero)));
}

#[test]
fn test_extract_astro_handles_dynamic_import_in_frontmatter() {
    let project = Project::new();
    let other = project.write("src/pages/Other.astro", "---\n---\n<p>o</p>\n");
    project.write(
        "src/pages/lazy.astro",
        "---\nconst Mod = await import('./Other.astro');\n---\n\n<div>{Mod.default}</div>\n",
    );
    assert!(
        targets(&project.raw("src/pages/lazy.astro"), "dynamic_import")
            .contains(&raw_path_id(&other))
    );
}

#[test]
fn test_extract_astro_picks_up_client_side_script_imports() {
    let project = Project::new();
    let layout = project.write("src/layouts/Layout.astro", "---\n---\n<slot />\n");
    let hydrate = project.write("src/client/hydrate.ts", "export function hydrate(){}\n");
    project.write(
        "src/pages/with-script.astro",
        "---\nimport Layout from '../layouts/Layout.astro';\n---\n\n<Layout>\n  <button id=\"b\">click</button>\n</Layout>\n\n<script>\n  import { hydrate } from '../client/hydrate.ts';\n  hydrate(document.getElementById('b'));\n</script>\n",
    );
    let imports = targets(&project.raw("src/pages/with-script.astro"), "imports_from");
    assert!(imports.contains(&raw_path_id(&layout)));
    assert!(imports.contains(&raw_path_id(&hydrate)));
}

#[test]
fn test_extract_astro_no_frontmatter_does_not_crash() {
    let project = Project::new();
    project.write("src/pages/plain.astro", "<h1>no frontmatter here</h1>\n");
    let extraction = project.raw("src/pages/plain.astro");
    assert_eq!(extraction.nodes.len(), 1);
    assert!(targets(&extraction, "imports_from").is_empty());
}

#[test]
fn test_extract_astro_handles_tsconfig_path_alias() {
    let project = Project::new();
    project.write(
        "tsconfig.json",
        "{\n  \"compilerOptions\": {\n    \"baseUrl\": \".\",\n    \"paths\": { \"@components/*\": [\"src/components/*\"] }\n  }\n}\n",
    );
    let hero = project.write("src/components/Hero.astro", "---\n---\n<h1>h</h1>\n");
    project.write(
        "src/pages/alias.astro",
        "---\nimport Hero from '@components/Hero.astro';\n---\n\n<Hero />\n",
    );
    let extraction = project.raw("src/pages/alias.astro");
    let imports = targets(&extraction, "imports_from");
    assert!(
        imports.contains(&raw_path_id(&hero)),
        "imports={imports:?}, expected={}",
        raw_path_id(&hero)
    );
}

fn astro_identity_project() -> Project {
    let project = Project::new();
    project.write(
        "src/pages/work/index.astro",
        "---\nimport { projects } from '../../lib/content';\nimport { SITE } from '../../config';\nimport '../styles/global.css';\n---\n<h1>{SITE}</h1>\n",
    );
    project.write("src/lib/content.ts", "export const projects = [1];\n");
    project.write("src/config.ts", "export const SITE = 'x';\n");
    project.write("src/pages/styles/global.css", "body { margin: 0 }\n");
    project
}

fn assert_no_root_slug(extractions: &[Extraction], root: &std::path::Path) {
    let root_slug = make_id(&[&root.to_string_lossy()]);
    assert!(nodes(extractions).all(|node| !node.id.contains(&root_slug)));
    assert!(edges(extractions).all(|edge| {
        !edge.true_source().contains(&root_slug) && !edge.true_target().contains(&root_slug)
    }));
}

fn assert_astro_identity(extractions: &[Extraction]) {
    let content_nodes = nodes(extractions)
        .filter(|node| node.source_file == "src/lib/content.ts" && node.id == "src_lib_content")
        .collect::<Vec<_>>();
    assert_eq!(content_nodes.len(), 1);
    let index = file_id("src/pages/work/index.astro");
    assert!(has_edge(
        extractions,
        &index,
        "src_lib_content",
        "imports_from"
    ));
    assert!(has_edge(extractions, &index, "src_config", "imports_from"));
    assert!(has_edge(
        extractions,
        &index,
        "src_pages_styles_global",
        "imports_from"
    ));
}

#[test]
fn test_astro_absolute_inputs_no_ghost_import_nodes() {
    let project = astro_identity_project();
    let result = project.extract_files(&[
        "src/pages/work/index.astro",
        "src/lib/content.ts",
        "src/config.ts",
    ]);
    assert_no_root_slug(&result, project.root.path());
    assert_astro_identity(&result);
}

#[test]
fn test_astro_relative_inputs_keep_canonical_ids() {
    let project = astro_identity_project();
    let result = project.extract();
    assert_no_root_slug(&result, project.root.path());
    assert_astro_identity(&result);
}

#[test]
fn test_svelte_absolute_inputs_no_ghost_import_nodes() {
    let project = Project::new();
    project.write(
        "src/routes/page.svelte",
        "<script>\n  import { projects } from '../lib/content';\n  const lazy = () => import('../lib/content');\n</script>\n<h1>{projects.length}</h1>\n",
    );
    project.write("src/lib/content.ts", "export const projects = [1];\n");
    let result = project.extract_files(&["src/routes/page.svelte", "src/lib/content.ts"]);
    assert_no_root_slug(&result, project.root.path());
    assert_eq!(
        nodes(&result)
            .filter(|node| node.id == "src_lib_content")
            .count(),
        1
    );
    let page = file_id("src/routes/page.svelte");
    assert!(has_edge(&result, &page, "src_lib_content", "imports_from"));
    assert!(has_edge(
        &result,
        &page,
        "src_lib_content",
        "dynamic_import"
    ));
}

#[test]
fn test_astro_unresolved_relative_import_id_still_portable() {
    let project = Project::new();
    project.write(
        "src/pages/index.astro",
        "---\nimport { gone } from '../missing/nowhere';\n---\n<p>x</p>\n",
    );
    let result = project.extract_files(&["src/pages/index.astro"]);
    assert_no_root_slug(&result, project.root.path());
    let stubs = nodes(&result)
        .filter(|node| node.label == "../missing/nowhere")
        .collect::<Vec<_>>();
    assert_eq!(stubs.len(), 1);
    assert_eq!(stubs[0].id, "src_missing_nowhere");
}
