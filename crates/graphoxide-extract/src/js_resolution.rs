//! Project-aware JavaScript and TypeScript module and export resolution.
//!
//! The tree-sitter walker intentionally remains a per-file extractor. This
//! pass supplies the project context needed by ECMAScript module resolution:
//! extension/index probing, tsconfig paths, workspace packages, and recursive
//! barrel export resolution. All identities are minted from repo-relative
//! source paths so checkout prefixes cannot leak into a graph.

use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use regex::Regex;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
};

const JS_EXTENSIONS: &[&str] = &[
    "ts", "tsx", "mts", "cts", "svelte", "vue", "astro", "js", "jsx", "mjs", "cjs",
];
const JS_INDEX_FILES: &[&str] = &[
    "index.ts",
    "index.tsx",
    "index.svelte",
    "index.vue",
    "index.astro",
    "index.js",
    "index.jsx",
    "index.mjs",
];
const EXPORT_CONDITIONS: &[&str] = &[
    "source", "import", "module", "svelte", "types", "require", "default",
];

#[derive(Debug, Clone)]
struct ImportFact {
    specifier: String,
    resolved: bool,
    imported: Option<String>,
    local: Option<String>,
    line: usize,
}

#[derive(Debug, Clone)]
enum ExportBinding {
    Local(String),
    Reexport { source: String, imported: String },
    Namespace(String),
}

#[derive(Debug, Clone)]
struct ReexportFact {
    specifier: String,
    resolved: bool,
    imported: Option<String>,
    exported: Option<String>,
    namespace: bool,
    star: bool,
    line: usize,
}

#[derive(Debug, Clone)]
struct ModuleFacts {
    extraction: usize,
    source_file: String,
    file_id: String,
    stem: String,
    definitions: BTreeMap<String, String>,
    aliases: BTreeMap<String, String>,
    imports: Vec<ImportFact>,
    reexports: Vec<ReexportFact>,
    exports: BTreeMap<String, Vec<ExportBinding>>,
    stars: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExportResolution {
    Resolved(String),
    Ambiguous,
    Missing,
}

/// Rebuild JavaScript-family import edges with project context before the
/// language-neutral resolver consumes them.
pub(crate) fn resolve(extractions: &mut [Extraction], root: &Path) {
    let root = fs::canonicalize(root).unwrap_or_else(|_| lexical_normalize(root));
    let mut modules = BTreeMap::<String, ModuleFacts>::new();

    for (index, extraction) in extractions.iter().enumerate() {
        let Some(file_node) = extraction.nodes.iter().find(|node| {
            node.extra.get("type").and_then(Value::as_str) == Some("file")
                && is_javascript_source(&node.source_file)
        }) else {
            continue;
        };
        let source_file = normalize_slashes(&file_node.source_file);
        let physical = root.join(&source_file);
        let Ok(text) = fs::read_to_string(&physical) else {
            continue;
        };
        let parse_source = crate::sfc::resolution_source(&physical, &text).unwrap_or(text);
        let facts = collect_module_facts(index, extraction, &source_file, &parse_source, &root);
        modules.insert(source_file, facts);
    }

    if modules.is_empty() {
        return;
    }

    let snapshot = modules.clone();
    for facts in modules.values_mut() {
        materialize_export_bindings(facts, &snapshot);
    }

    let snapshot = modules.clone();
    for facts in modules.values() {
        rebuild_module_edges(extractions, facts, &snapshot, &root);
    }
}

fn is_javascript_source(source: &str) -> bool {
    let lower = source.to_ascii_lowercase();
    [
        ".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts", ".svelte", ".vue", ".astro",
    ]
    .iter()
    .any(|suffix| lower.ends_with(suffix))
}

fn is_sfc_source(source: &str) -> bool {
    matches!(
        Path::new(source)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("vue" | "astro" | "svelte")
    )
}

fn unresolved_local_import_id(source_file: &str, specifier: &str) -> String {
    let base = Path::new(source_file)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let logical = lexical_normalize(&base.join(specifier));
    make_id(&[&logical.with_extension("").to_string_lossy()])
}

fn collect_module_facts(
    extraction: usize,
    value: &Extraction,
    source_file: &str,
    text: &str,
    root: &Path,
) -> ModuleFacts {
    let file_id = make_id(&[&path_without_extension(source_file)]);
    let stem = path_without_extension(source_file);
    let definitions = value
        .nodes
        .iter()
        .filter(|node| node.source_file == source_file && node.id != file_id)
        .filter_map(|node| {
            let label = node
                .label
                .trim()
                .trim_start_matches('.')
                .trim_end_matches("()")
                .to_owned();
            (!label.is_empty()).then(|| (label, node.id.clone()))
        })
        .collect::<BTreeMap<_, _>>();

    let mut imports = parse_imports(text);
    let mut reexports = parse_reexports(text);
    let aliases = parse_local_aliases(text);
    let mut exports = BTreeMap::<String, Vec<ExportBinding>>::new();

    for name in parse_direct_exports(text) {
        exports
            .entry(name.clone())
            .or_default()
            .push(ExportBinding::Local(name));
    }
    for (local, exported) in parse_local_export_clauses(text) {
        exports
            .entry(exported)
            .or_default()
            .push(ExportBinding::Local(local));
    }
    if let Some(default_name) = parse_default_export(text) {
        exports
            .entry("default".into())
            .or_default()
            .push(ExportBinding::Local(default_name));
    }

    // Resolve specifiers now while the physical importer location is known.
    let importer = root.join(source_file);
    for import in &mut imports {
        if let Some(target) = resolve_module_specifier(&import.specifier, &importer, root) {
            import.specifier = target;
            import.resolved = true;
        }
    }
    for reexport in &mut reexports {
        if let Some(target) = resolve_module_specifier(&reexport.specifier, &importer, root) {
            reexport.specifier = target;
            reexport.resolved = true;
        }
    }

    ModuleFacts {
        extraction,
        source_file: source_file.into(),
        file_id,
        stem,
        definitions,
        aliases,
        imports,
        reexports,
        exports,
        stars: Vec::new(),
    }
}

fn materialize_export_bindings(facts: &mut ModuleFacts, modules: &BTreeMap<String, ModuleFacts>) {
    for reexport in facts.reexports.clone() {
        if !reexport.resolved || !modules.contains_key(&reexport.specifier) {
            continue;
        }
        if reexport.star {
            facts.stars.push(reexport.specifier);
            continue;
        }
        let (Some(imported), Some(exported)) = (reexport.imported, reexport.exported) else {
            continue;
        };
        let binding = if reexport.namespace {
            ExportBinding::Namespace(reexport.specifier)
        } else {
            ExportBinding::Reexport {
                source: reexport.specifier,
                imported,
            }
        };
        facts.exports.entry(exported).or_default().push(binding);
    }
}

fn rebuild_module_edges(
    extractions: &mut [Extraction],
    facts: &ModuleFacts,
    modules: &BTreeMap<String, ModuleFacts>,
    root: &Path,
) {
    let extraction = &mut extractions[facts.extraction];
    let rebuilt_locations = facts
        .imports
        .iter()
        .map(|fact| fact.line)
        .chain(facts.reexports.iter().map(|fact| fact.line))
        .map(|line| format!("L{line}"))
        .collect::<BTreeSet<_>>();
    extraction.edges.retain(|edge| {
        !(edge.true_source() == facts.file_id
            && matches!(
                edge.relation.as_str(),
                "imports" | "imports_from" | "re_exports"
            )
            && edge
                .extra
                .get("source_location")
                .and_then(Value::as_str)
                .is_some_and(|location| rebuilt_locations.contains(location))
            && edge
                .extra
                .get("context")
                .and_then(Value::as_str)
                .is_some_and(|context| matches!(context, "import" | "re-export")))
    });

    for import in &facts.imports {
        let target_file = import
            .resolved
            .then(|| modules.get(&import.specifier))
            .flatten();
        let file_target = target_file
            .map(|target| target.file_id.clone())
            .unwrap_or_else(|| {
                if import.resolved {
                    make_id(&[&path_without_extension(&import.specifier)])
                } else if is_sfc_source(&facts.source_file) && import.specifier.starts_with('.') {
                    unresolved_local_import_id(&facts.source_file, &import.specifier)
                } else {
                    make_id(&["ref", &import.specifier])
                }
            });
        let mut file_edge = module_edge(
            &facts.file_id,
            &file_target,
            "imports_from",
            &facts.source_file,
            import.line,
            "import",
        );
        if let Some(target) = target_file {
            file_edge
                .extra
                .insert("target_file".into(), target.source_file.clone().into());
        } else if import.resolved {
            file_edge
                .extra
                .insert("target_file".into(), import.specifier.clone().into());
        }
        push_unique_edge(&mut extraction.edges, file_edge);
        let (Some(imported), Some(local), Some(target)) =
            (&import.imported, &import.local, target_file)
        else {
            continue;
        };
        let target_id = match resolve_export(modules, &target.source_file, imported) {
            ExportResolution::Resolved(id) => id,
            ExportResolution::Ambiguous | ExportResolution::Missing => {
                make_id(&[&target.stem, imported])
            }
        };
        let mut edge = module_edge(
            &facts.file_id,
            &target_id,
            "imports",
            &facts.source_file,
            import.line,
            "import",
        );
        edge.extra
            .insert("local_alias".into(), local.clone().into());
        edge.extra
            .insert("imported_name".into(), imported.clone().into());
        edge.extra
            .insert("target_file".into(), target.source_file.clone().into());
        push_unique_edge(&mut extraction.edges, edge);
    }

    for reexport in &facts.reexports {
        let Some(target) = reexport
            .resolved
            .then(|| modules.get(&reexport.specifier))
            .flatten()
        else {
            continue;
        };
        for relation in ["imports_from", "re_exports"] {
            let mut edge = module_edge(
                &facts.file_id,
                &target.file_id,
                relation,
                &facts.source_file,
                reexport.line,
                "re-export",
            );
            edge.extra
                .insert("target_file".into(), target.source_file.clone().into());
            push_unique_edge(&mut extraction.edges, edge);
        }
        if reexport.star {
            continue;
        }
        let (Some(imported), Some(exported)) = (&reexport.imported, &reexport.exported) else {
            continue;
        };
        if reexport.namespace {
            let namespace_id = make_id(&[&facts.stem, exported]);
            if extraction.nodes.iter().all(|node| node.id != namespace_id) {
                let mut extra = BTreeMap::new();
                extra.insert("_origin".into(), "ast".into());
                extra.insert("type".into(), "module".into());
                extra.insert("exported".into(), true.into());
                extraction.nodes.push(Node {
                    id: namespace_id.clone(),
                    label: exported.clone(),
                    file_type: "code".into(),
                    source_file: facts.source_file.clone(),
                    source_location: Some(format!("L{}", reexport.line)),
                    community: None,
                    extra,
                });
            }
            push_unique_edge(
                &mut extraction.edges,
                module_edge(
                    &facts.file_id,
                    &namespace_id,
                    "contains",
                    &facts.source_file,
                    reexport.line,
                    "re-export",
                ),
            );
            continue;
        }
        let target_id = match resolve_export(modules, &target.source_file, imported) {
            ExportResolution::Resolved(id) => id,
            ExportResolution::Ambiguous | ExportResolution::Missing => {
                make_id(&[&target.stem, imported])
            }
        };
        let mut edge = module_edge(
            &facts.file_id,
            &target_id,
            "re_exports",
            &facts.source_file,
            reexport.line,
            "re-export",
        );
        edge.extra
            .insert("exported_name".into(), exported.clone().into());
        edge.extra
            .insert("target_file".into(), target.source_file.clone().into());
        push_unique_edge(&mut extraction.edges, edge);
    }

    // Canonicalize and mark deferred import() facts emitted by the per-file
    // walker. A deferred dependency remains visible, but must not participate
    // in static import-cycle detection.
    let physical = root.join(&facts.source_file);
    for (line, specifier) in
        parse_dynamic_imports(&fs::read_to_string(&physical).unwrap_or_default())
    {
        let Some(target_source) = resolve_module_specifier(&specifier, &physical, root) else {
            continue;
        };
        let Some(target) = modules.get(&target_source) else {
            continue;
        };
        let mut updated = false;
        for edge in &mut extraction.edges {
            if edge.relation == "imports_from"
                && edge.extra.get("source_location").and_then(Value::as_str)
                    == Some(format!("L{line}").as_str())
            {
                edge.target = target.file_id.clone();
                edge.extra
                    .insert("_tgt".into(), target.file_id.clone().into());
                edge.extra.insert("deferred".into(), true.into());
                edge.extra
                    .insert("target_file".into(), target.source_file.clone().into());
                updated = true;
            }
        }
        if !updated {
            let mut edge = module_edge(
                &facts.file_id,
                &target.file_id,
                "dynamic_import",
                &facts.source_file,
                line,
                "import",
            );
            edge.extra.insert("deferred".into(), true.into());
            edge.extra
                .insert("target_file".into(), target.source_file.clone().into());
            push_unique_edge(&mut extraction.edges, edge);
        }
    }

    augment_typescript_type_edges(
        extraction,
        facts,
        modules,
        &fs::read_to_string(&physical).unwrap_or_default(),
    );
}

fn augment_typescript_type_edges(
    extraction: &mut Extraction,
    facts: &ModuleFacts,
    modules: &BTreeMap<String, ModuleFacts>,
    text: &str,
) {
    let lower = facts.source_file.to_ascii_lowercase();
    if ![".ts", ".tsx", ".mts", ".cts"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
    {
        return;
    }
    let declarations = Regex::new(
        r"(?m)(?:^|\n)\s*(?:export\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)([^\n{]*)",
    )
    .expect("TypeScript class relationship regex");
    let parent = Regex::new(r"\bextends\s+([A-Za-z_$][\w$]*)").expect("TypeScript extends regex");
    let interfaces = Regex::new(r"\bimplements\s+([^\n{]+)").expect("TypeScript implements regex");
    let type_name = Regex::new(r"[A-Za-z_$][\w$]*").expect("TypeScript type name regex");
    for capture in declarations.captures_iter(text) {
        let Some(owner) = facts.definitions.get(&capture[1]) else {
            continue;
        };
        let line = line_number(text, capture.get(0).expect("whole class").start());
        if let Some(name) = parent
            .captures(&capture[2])
            .and_then(|value| value.get(1))
            .map(|value| value.as_str())
        {
            if let Some(target) = resolve_visible_symbol(modules, facts, name) {
                push_unique_edge(
                    &mut extraction.edges,
                    typed_edge(owner, &target, "inherits", &facts.source_file, line, None),
                );
            }
        }
        if let Some(list) = interfaces
            .captures(&capture[2])
            .and_then(|value| value.get(1))
        {
            for interface in list.as_str().split(',') {
                let Some(name) = type_name.find(interface).map(|value| value.as_str()) else {
                    continue;
                };
                if let Some(target) = resolve_visible_symbol(modules, facts, name) {
                    push_unique_edge(
                        &mut extraction.edges,
                        typed_edge(owner, &target, "implements", &facts.source_file, line, None),
                    );
                }
            }
        }
    }

    let methods =
        Regex::new(r"(?m)([A-Za-z_$][\w$]*)\s*\(([^)]*)\)\s*:\s*([A-Za-z_$][\w$]*(?:\s*<[^>]+>)?)")
            .expect("TypeScript method signature regex");
    for capture in methods.captures_iter(text) {
        let Some(owner) = facts.definitions.get(&capture[1]) else {
            continue;
        };
        let line = line_number(text, capture.get(0).expect("whole method").start());
        for parameter in capture[2].split(',') {
            let Some((_, annotation)) = parameter.split_once(':') else {
                continue;
            };
            emit_type_expression(
                extraction,
                facts,
                modules,
                owner,
                annotation,
                "parameter_type",
                line,
            );
        }
        emit_type_expression(
            extraction,
            facts,
            modules,
            owner,
            &capture[3],
            "return_type",
            line,
        );
    }
}

fn emit_type_expression(
    extraction: &mut Extraction,
    facts: &ModuleFacts,
    modules: &BTreeMap<String, ModuleFacts>,
    owner: &str,
    expression: &str,
    context: &str,
    line: usize,
) {
    let names = Regex::new(r"[A-Za-z_$][\w$]*").expect("TypeScript type token regex");
    for (index, token) in names.find_iter(expression).enumerate() {
        let name = token.as_str();
        if matches!(
            name,
            "string" | "number" | "boolean" | "void" | "unknown" | "any"
        ) {
            continue;
        }
        let Some(target) = resolve_visible_symbol(modules, facts, name) else {
            continue;
        };
        push_unique_edge(
            &mut extraction.edges,
            typed_edge(
                owner,
                &target,
                "references",
                &facts.source_file,
                line,
                Some(if index == 0 { context } else { "generic_arg" }),
            ),
        );
    }
}

fn resolve_visible_symbol(
    modules: &BTreeMap<String, ModuleFacts>,
    module: &ModuleFacts,
    local: &str,
) -> Option<String> {
    let imports = module
        .imports
        .iter()
        .filter(|import| import.resolved)
        .filter(|import| import.local.as_deref() == Some(local))
        .filter_map(|import| Some((&import.specifier, import.imported.as_deref()?)))
        .collect::<Vec<_>>();
    if imports.len() == 1 {
        if let ExportResolution::Resolved(id) = resolve_export(modules, imports[0].0, imports[0].1)
        {
            return Some(id);
        }
    }
    module.definitions.get(local).cloned()
}

fn typed_edge(
    source: &str,
    target: &str,
    relation: &str,
    source_file: &str,
    line: usize,
    context: Option<&str>,
) -> Edge {
    let mut edge = module_edge(source, target, relation, source_file, line, "type");
    match context {
        Some(context) => {
            edge.extra.insert("context".into(), context.into());
        }
        None => {
            edge.extra.remove("context");
        }
    }
    edge
}

fn resolve_export(
    modules: &BTreeMap<String, ModuleFacts>,
    source: &str,
    exported: &str,
) -> ExportResolution {
    resolve_export_inner(modules, source, exported, &mut BTreeSet::new())
}

fn resolve_export_inner(
    modules: &BTreeMap<String, ModuleFacts>,
    source: &str,
    exported: &str,
    visiting: &mut BTreeSet<(String, String)>,
) -> ExportResolution {
    let key = (source.to_owned(), exported.to_owned());
    if !visiting.insert(key.clone()) {
        return ExportResolution::Missing;
    }
    let Some(module) = modules.get(source) else {
        visiting.remove(&key);
        return ExportResolution::Missing;
    };

    let mut candidates = BTreeSet::new();
    let mut ambiguous = false;
    if let Some(bindings) = module.exports.get(exported) {
        for binding in bindings {
            match resolve_binding(modules, module, binding, visiting) {
                ExportResolution::Resolved(id) => {
                    candidates.insert(id);
                }
                ExportResolution::Ambiguous => ambiguous = true,
                ExportResolution::Missing => {}
            }
        }
    }
    if candidates.is_empty() && !ambiguous {
        for star in &module.stars {
            match resolve_export_inner(modules, star, exported, visiting) {
                ExportResolution::Resolved(id) => {
                    candidates.insert(id);
                }
                ExportResolution::Ambiguous => ambiguous = true,
                ExportResolution::Missing => {}
            }
        }
    }
    visiting.remove(&key);
    if ambiguous || candidates.len() > 1 {
        ExportResolution::Ambiguous
    } else if let Some(id) = candidates.into_iter().next() {
        ExportResolution::Resolved(id)
    } else {
        ExportResolution::Missing
    }
}

fn resolve_binding(
    modules: &BTreeMap<String, ModuleFacts>,
    module: &ModuleFacts,
    binding: &ExportBinding,
    visiting: &mut BTreeSet<(String, String)>,
) -> ExportResolution {
    match binding {
        ExportBinding::Local(local) => resolve_local(modules, module, local, visiting),
        ExportBinding::Reexport { source, imported } => {
            resolve_export_inner(modules, source, imported, visiting)
        }
        ExportBinding::Namespace(_) => module
            .definitions
            .get(local_namespace_name(module, binding).unwrap_or_default())
            .cloned()
            .map(ExportResolution::Resolved)
            .unwrap_or_else(|| {
                let name = module.exports.iter().find_map(|(name, values)| {
                    values
                        .iter()
                        .any(|value| std::ptr::eq(value, binding))
                        .then_some(name)
                });
                name.map(|name| ExportResolution::Resolved(make_id(&[&module.stem, name])))
                    .unwrap_or(ExportResolution::Missing)
            }),
    }
}

fn local_namespace_name<'a>(
    module: &'a ModuleFacts,
    binding: &'a ExportBinding,
) -> Option<&'a str> {
    module.exports.iter().find_map(|(name, values)| {
        values
            .iter()
            .any(|value| match (value, binding) {
                (ExportBinding::Namespace(left), ExportBinding::Namespace(right)) => left == right,
                _ => false,
            })
            .then_some(name.as_str())
    })
}

fn resolve_local(
    modules: &BTreeMap<String, ModuleFacts>,
    module: &ModuleFacts,
    local: &str,
    visiting: &mut BTreeSet<(String, String)>,
) -> ExportResolution {
    let mut current = local;
    let mut aliases_seen = BTreeSet::new();
    while let Some(alias) = module.aliases.get(current) {
        if !aliases_seen.insert(current.to_owned()) {
            return ExportResolution::Ambiguous;
        }
        current = alias;
    }
    let matching_imports = module
        .imports
        .iter()
        .filter(|import| import.resolved)
        .filter(|import| import.local.as_deref() == Some(current))
        .filter_map(|import| Some((&import.specifier, import.imported.as_deref()?)))
        .collect::<Vec<_>>();
    if matching_imports.len() == 1 {
        return resolve_export_inner(
            modules,
            matching_imports[0].0,
            matching_imports[0].1,
            visiting,
        );
    }
    if matching_imports.len() > 1 {
        return ExportResolution::Ambiguous;
    }
    module
        .definitions
        .get(current)
        .cloned()
        .map(ExportResolution::Resolved)
        .unwrap_or(ExportResolution::Missing)
}

fn parse_imports(text: &str) -> Vec<ImportFact> {
    let re =
        Regex::new(r#"(?m)(?:^|;)\s*import\s+(?:type\s+)?([^;\n]+?)\s+from\s+['\"]([^'\"]+)['\"]"#)
            .expect("JavaScript import regex");
    let mut facts = Vec::new();
    for capture in re.captures_iter(text) {
        let whole = capture.get(0).expect("whole import");
        let line = line_number(text, whole.start());
        let clause = capture[1].trim();
        let specifier = capture[2].to_owned();
        if let (Some(open), Some(close)) = (clause.find('{'), clause.rfind('}')) {
            for item in clause[open + 1..close].split(',') {
                let item = item.trim().trim_start_matches("type ").trim();
                if item.is_empty() {
                    continue;
                }
                let words = item.split_whitespace().collect::<Vec<_>>();
                let imported = words[0];
                let local = if words.get(1) == Some(&"as") {
                    words.get(2).copied().unwrap_or(imported)
                } else {
                    imported
                };
                facts.push(ImportFact {
                    specifier: specifier.clone(),
                    resolved: false,
                    imported: Some(imported.into()),
                    local: Some(local.into()),
                    line,
                });
            }
            let prefix = clause[..open].trim().trim_end_matches(',').trim();
            if !prefix.is_empty() && !prefix.starts_with('*') {
                facts.push(ImportFact {
                    specifier: specifier.clone(),
                    resolved: false,
                    imported: Some("default".into()),
                    local: Some(prefix.into()),
                    line,
                });
            }
        } else if let Some(namespace) = clause.strip_prefix("* as ") {
            facts.push(ImportFact {
                specifier,
                resolved: false,
                imported: None,
                local: Some(namespace.trim().into()),
                line,
            });
        } else {
            facts.push(ImportFact {
                specifier,
                resolved: false,
                imported: Some("default".into()),
                local: Some(clause.trim().into()),
                line,
            });
        }
    }
    let side_effect = Regex::new(r#"(?m)(?:^|;)\s*import\s*['\"]([^'\"]+)['\"]"#)
        .expect("JavaScript side-effect import regex");
    for capture in side_effect.captures_iter(text) {
        facts.push(ImportFact {
            specifier: capture[1].into(),
            resolved: false,
            imported: None,
            local: None,
            line: line_number(
                text,
                capture.get(0).expect("whole side-effect import").start(),
            ),
        });
    }
    let import_require = Regex::new(
        r#"(?m)(?:^|;)\s*import\s+([A-Za-z_$][\w$]*)\s*=\s*require\s*\(\s*['\"]([^'\"]+)['\"]\s*\)"#,
    )
    .expect("TypeScript import-require regex");
    for capture in import_require.captures_iter(text) {
        facts.push(ImportFact {
            specifier: capture[2].into(),
            resolved: false,
            imported: None,
            local: Some(capture[1].into()),
            line: line_number(text, capture.get(0).expect("whole import-require").start()),
        });
    }
    facts.sort_by(|left, right| {
        left.line
            .cmp(&right.line)
            .then_with(|| left.specifier.cmp(&right.specifier))
            .then_with(|| left.imported.cmp(&right.imported))
            .then_with(|| left.local.cmp(&right.local))
    });
    facts
}

fn parse_reexports(text: &str) -> Vec<ReexportFact> {
    let mut facts = Vec::new();
    let named =
        Regex::new(r#"(?m)^\s*export\s+(?:type\s+)?\{([^}]*)\}\s+from\s+['\"]([^'\"]+)['\"]"#)
            .expect("named re-export regex");
    for capture in named.captures_iter(text) {
        let line = line_number(text, capture.get(0).expect("whole re-export").start());
        for item in capture[1].split(',') {
            let item = item.trim().trim_start_matches("type ").trim();
            if item.is_empty() {
                continue;
            }
            let words = item.split_whitespace().collect::<Vec<_>>();
            let imported = words[0];
            let exported = if words.get(1) == Some(&"as") {
                words.get(2).copied().unwrap_or(imported)
            } else {
                imported
            };
            facts.push(ReexportFact {
                specifier: capture[2].into(),
                resolved: false,
                imported: Some(imported.into()),
                exported: Some(exported.into()),
                namespace: false,
                star: false,
                line,
            });
        }
    }
    let namespace =
        Regex::new(r#"(?m)^\s*export\s+\*\s+as\s+([A-Za-z_$][\w$]*)\s+from\s+['\"]([^'\"]+)['\"]"#)
            .expect("namespace re-export regex");
    for capture in namespace.captures_iter(text) {
        facts.push(ReexportFact {
            specifier: capture[2].into(),
            resolved: false,
            imported: Some("*".into()),
            exported: Some(capture[1].into()),
            namespace: true,
            star: false,
            line: line_number(
                text,
                capture.get(0).expect("whole namespace export").start(),
            ),
        });
    }
    let star = Regex::new(r#"(?m)^\s*export\s+\*\s+from\s+['\"]([^'\"]+)['\"]"#)
        .expect("star re-export regex");
    for capture in star.captures_iter(text) {
        facts.push(ReexportFact {
            specifier: capture[1].into(),
            resolved: false,
            imported: None,
            exported: None,
            namespace: false,
            star: true,
            line: line_number(text, capture.get(0).expect("whole star export").start()),
        });
    }
    facts
}

fn parse_direct_exports(text: &str) -> BTreeSet<String> {
    let re = Regex::new(
        r"(?m)^\s*export\s+(?:default\s+)?(?:declare\s+)?(?:abstract\s+)?(?:class|interface|type|function|const|let|var)\s+([A-Za-z_$][\w$]*)",
    )
    .expect("direct export regex");
    re.captures_iter(text)
        .map(|capture| capture[1].to_owned())
        .collect()
}

fn parse_local_export_clauses(text: &str) -> Vec<(String, String)> {
    let re = Regex::new(r"(?m)^\s*export\s+(?:type\s+)?\{([^}]*)\}\s*;?\s*$")
        .expect("local export regex");
    let mut exports = Vec::new();
    for capture in re.captures_iter(text) {
        for item in capture[1].split(',') {
            let item = item.trim().trim_start_matches("type ").trim();
            if item.is_empty() {
                continue;
            }
            let words = item.split_whitespace().collect::<Vec<_>>();
            let local = words[0];
            let exported = if words.get(1) == Some(&"as") {
                words.get(2).copied().unwrap_or(local)
            } else {
                local
            };
            exports.push((local.into(), exported.into()));
        }
    }
    exports
}

fn parse_default_export(text: &str) -> Option<String> {
    let declaration = Regex::new(
        r"(?m)^\s*export\s+default\s+(?:(?:abstract\s+)?class|function)\s+([A-Za-z_$][\w$]*)",
    )
    .expect("default declaration regex");
    if let Some(capture) = declaration.captures(text) {
        return Some(capture[1].into());
    }
    let identifier = Regex::new(r"(?m)^\s*export\s+default\s+([A-Za-z_$][\w$]*)\s*;?\s*$")
        .expect("default identifier regex");
    identifier.captures(text).map(|capture| capture[1].into())
}

fn parse_local_aliases(text: &str) -> BTreeMap<String, String> {
    Regex::new(
        r"(?m)^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*([A-Za-z_$][\w$]*)\s*;?\s*$",
    )
    .expect("module alias regex")
    .captures_iter(text)
    .map(|capture| (capture[1].into(), capture[2].into()))
    .collect()
}

fn parse_dynamic_imports(text: &str) -> Vec<(usize, String)> {
    let re = Regex::new(r#"import\s*\(\s*['\"]([^'\"]+)['\"]\s*\)"#).expect("dynamic import regex");
    re.captures_iter(text)
        .map(|capture| {
            (
                line_number(text, capture.get(0).expect("whole dynamic import").start()),
                capture[1].into(),
            )
        })
        .collect()
}

fn resolve_module_specifier(specifier: &str, importer: &Path, root: &Path) -> Option<String> {
    let importer_dir = importer.parent()?;
    let resolved = if specifier.starts_with('.') {
        resolve_js_path(&lexical_normalize(&importer_dir.join(specifier)))
    } else {
        resolve_tsconfig(specifier, importer_dir)
            .or_else(|| resolve_workspace(specifier, importer_dir))
    }?;
    let resolved = fs::canonicalize(&resolved).unwrap_or_else(|_| lexical_normalize(&resolved));
    let relative = resolved.strip_prefix(root).ok()?;
    Some(normalize_slashes(relative.to_string_lossy().as_ref()))
}

pub(crate) fn resolve_import_path(specifier: &str, importer: &Path) -> Option<PathBuf> {
    let importer_dir = importer.parent()?;
    if specifier.starts_with('.') {
        resolve_js_path(&lexical_normalize(&importer_dir.join(specifier)))
    } else {
        resolve_tsconfig(specifier, importer_dir)
            .or_else(|| resolve_workspace(specifier, importer_dir))
    }
}

/// Resolve a JavaScript-family module path using Graphify's source-oriented
/// extension and directory-index precedence. Missing paths are returned
/// unchanged so callers can retain an explicit external/phantom reference.
pub fn resolve_js_module_path(candidate: &Path) -> PathBuf {
    let candidate = lexical_normalize(candidate);
    resolve_js_path(&candidate).unwrap_or(candidate)
}

fn resolve_js_path(candidate: &Path) -> Option<PathBuf> {
    let candidate = lexical_normalize(candidate);
    if candidate.is_file() {
        return Some(candidate);
    }
    match candidate.extension().and_then(|value| value.to_str()) {
        Some("js") => {
            let value = candidate.with_extension("ts");
            if value.is_file() {
                return Some(value);
            }
        }
        Some("jsx") => {
            let value = candidate.with_extension("tsx");
            if value.is_file() {
                return Some(value);
            }
        }
        _ => {}
    }
    let name = candidate.file_name()?.to_string_lossy();
    for extension in JS_EXTENSIONS {
        let value = candidate.with_file_name(format!("{name}.{extension}"));
        if value.is_file() {
            return Some(value);
        }
    }
    if candidate.is_dir() {
        for index in JS_INDEX_FILES {
            let value = candidate.join(index);
            if value.is_file() {
                return Some(value);
            }
        }
    }
    None
}

#[derive(Default)]
struct TsConfig {
    aliases: BTreeMap<String, Vec<PathBuf>>,
    base_url: Option<PathBuf>,
}

type MatchedAlias<'a> = ((u8, usize), String, bool, &'a [PathBuf]);

fn resolve_tsconfig(specifier: &str, start: &Path) -> Option<PathBuf> {
    let config = find_config(start)?;
    let parsed = read_tsconfig(&config, &mut BTreeSet::new());
    let mut best: Option<MatchedAlias<'_>> = None;
    for (pattern, targets) in &parsed.aliases {
        let Some((score, captured, wildcard)) = match_alias(specifier, pattern) else {
            continue;
        };
        if best.as_ref().is_none_or(|current| score < current.0) {
            best = Some((score, captured, wildcard, targets));
        }
    }
    if let Some((_, captured, wildcard, targets)) = best {
        for target in targets {
            let candidate = if wildcard && !captured.is_empty() {
                PathBuf::from(target.to_string_lossy().replacen('*', &captured, 1))
            } else if captured.is_empty() {
                target.clone()
            } else {
                target.join(captured.as_str())
            };
            if let Some(resolved) = resolve_js_path(&lexical_normalize(&candidate)) {
                return Some(resolved);
            }
        }
        return None;
    }
    parsed
        .base_url
        .and_then(|base| resolve_js_path(&lexical_normalize(&base.join(specifier))))
}

fn find_config(start: &Path) -> Option<PathBuf> {
    for directory in start.ancestors() {
        for name in ["tsconfig.json", "jsconfig.json"] {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn read_tsconfig(path: &Path, seen: &mut BTreeSet<PathBuf>) -> TsConfig {
    let path = fs::canonicalize(path).unwrap_or_else(|_| lexical_normalize(path));
    if !seen.insert(path.clone()) {
        return TsConfig::default();
    }
    let Some(data) = read_jsonc(&path) else {
        return TsConfig::default();
    };
    let base = path.parent().unwrap_or_else(|| Path::new(""));
    let mut result = TsConfig::default();
    let parents = match data.get("extends") {
        Some(Value::String(value)) => vec![value.as_str()],
        Some(Value::Array(values)) => values.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    for parent in parents {
        if parent.starts_with('@') {
            continue;
        }
        let mut extended = lexical_normalize(&base.join(parent));
        if extended.extension().is_none() {
            extended.set_extension("json");
        }
        if extended.is_file() {
            let inherited = read_tsconfig(&extended, seen);
            result.aliases.extend(inherited.aliases);
            if inherited.base_url.is_some() {
                result.base_url = inherited.base_url;
            }
        }
    }
    let options = data.get("compilerOptions").and_then(Value::as_object);
    let local_base = options
        .and_then(|options| options.get("baseUrl"))
        .and_then(Value::as_str)
        .map(|value| lexical_normalize(&base.join(value)));
    if local_base.is_some() {
        result.base_url = local_base.clone();
    }
    let paths_base = local_base.unwrap_or_else(|| base.to_path_buf());
    if let Some(paths) = options
        .and_then(|options| options.get("paths"))
        .and_then(Value::as_object)
    {
        for (alias, targets) in paths {
            let values = targets
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(|value| paths_base.join(value))
                .collect::<Vec<_>>();
            if !values.is_empty() {
                result.aliases.insert(alias.clone(), values);
            }
        }
    }
    result
}

fn match_alias(raw: &str, pattern: &str) -> Option<((u8, usize), String, bool)> {
    if let Some((prefix, suffix)) = pattern.split_once('*') {
        if pattern.matches('*').count() != 1 || !raw.starts_with(prefix) || !raw.ends_with(suffix) {
            return None;
        }
        let end = raw.len().checked_sub(suffix.len())?;
        if end < prefix.len() {
            return None;
        }
        return Some((
            (1, usize::MAX - prefix.len()),
            raw[prefix.len()..end].into(),
            true,
        ));
    }
    if raw == pattern {
        return Some(((0, usize::MAX - pattern.len()), String::new(), false));
    }
    let prefix = pattern.trim_end_matches('/');
    raw.strip_prefix(prefix)
        .and_then(|tail| tail.strip_prefix('/'))
        .map(|captured| ((2, usize::MAX - prefix.len()), captured.into(), false))
}

fn resolve_workspace(specifier: &str, start: &Path) -> Option<PathBuf> {
    let root = find_workspace_root(start)?;
    for package in workspace_package_dirs(&root) {
        let Some(data) = read_jsonc(&package.join("package.json")) else {
            continue;
        };
        let Some(name) = data.get("name").and_then(Value::as_str) else {
            continue;
        };
        let subpath = if specifier == name {
            ""
        } else if let Some(value) = specifier.strip_prefix(&format!("{name}/")) {
            value
        } else {
            continue;
        };
        for candidate in package_entry_candidates(&package, &data, subpath) {
            if let Some(resolved) = resolve_js_path(&candidate) {
                return Some(resolved);
            }
        }
    }
    None
}

fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    for directory in start.ancestors() {
        if directory.join("pnpm-workspace.yaml").is_file() {
            return Some(directory.into());
        }
        let package = directory.join("package.json");
        if read_jsonc(&package).is_some_and(|data| data.get("workspaces").is_some()) {
            return Some(directory.into());
        }
    }
    None
}

fn workspace_package_dirs(root: &Path) -> Vec<PathBuf> {
    let patterns = if root.join("pnpm-workspace.yaml").is_file() {
        parse_pnpm_patterns(
            &fs::read_to_string(root.join("pnpm-workspace.yaml")).unwrap_or_default(),
        )
    } else {
        let data = read_jsonc(&root.join("package.json")).unwrap_or(Value::Null);
        match data.get("workspaces") {
            Some(Value::Array(values)) => values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            Some(Value::Object(value)) => value
                .get("packages")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        }
    };
    let mut directories = Vec::new();
    for pattern in patterns
        .into_iter()
        .filter(|pattern| !pattern.starts_with('!'))
    {
        if matches!(pattern.as_str(), "." | "./") {
            directories.push(root.to_path_buf());
            continue;
        }
        let Some(star) = pattern.find('*') else {
            directories.push(root.join(pattern));
            continue;
        };
        let prefix = pattern[..star].trim_end_matches('/');
        let suffix = pattern[star + 1..].trim_start_matches('/');
        let parent = root.join(prefix);
        let Ok(entries) = fs::read_dir(parent) else {
            continue;
        };
        for entry in entries.flatten().filter(|entry| entry.path().is_dir()) {
            let candidate = if suffix.is_empty() {
                entry.path()
            } else {
                entry.path().join(suffix)
            };
            if candidate.is_dir() {
                directories.push(candidate);
            }
        }
    }
    directories.sort();
    directories.dedup();
    directories
}

fn parse_pnpm_patterns(text: &str) -> Vec<String> {
    let mut patterns = Vec::new();
    let mut packages = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with("packages:") {
            packages = true;
        } else if packages && line.starts_with('-') {
            let value = line[1..].trim().trim_matches(['\'', '"']);
            if !value.is_empty() {
                patterns.push(value.into());
            }
        } else if packages && !raw.starts_with([' ', '\t']) && !line.is_empty() {
            break;
        }
    }
    patterns
}

fn package_entry_candidates(package: &Path, data: &Value, subpath: &str) -> Vec<PathBuf> {
    if !subpath.is_empty() {
        if let Some(exports) = data.get("exports").and_then(Value::as_object) {
            let key = format!("./{subpath}");
            if let Some(target) = exports.get(&key).and_then(resolve_export_target) {
                let candidate = lexical_normalize(&package.join(target));
                if path_contained(&candidate, package) {
                    return vec![candidate];
                }
            }
            for (pattern, value) in exports {
                let Some((prefix, suffix)) = pattern.split_once('*') else {
                    continue;
                };
                if pattern.matches('*').count() != 1
                    || !key.starts_with(prefix)
                    || !key.ends_with(suffix)
                {
                    continue;
                }
                let end = key.len().saturating_sub(suffix.len());
                let captured = &key[prefix.len()..end];
                let Some(target) = resolve_export_target(value) else {
                    continue;
                };
                let candidate = lexical_normalize(&package.join(target.replacen('*', captured, 1)));
                if path_contained(&candidate, package) {
                    return vec![candidate];
                }
            }
        }
        return vec![package.join(subpath)];
    }
    if let Some(exports) = data.get("exports") {
        if let Some(target) = exports.as_str() {
            return vec![package.join(target)];
        }
        if let Some(target) = exports
            .as_object()
            .and_then(|values| values.get("."))
            .and_then(resolve_export_target)
        {
            return vec![package.join(target)];
        }
    }
    let mut candidates = ["svelte", "module", "main", "types"]
        .iter()
        .filter_map(|key| data.get(key).and_then(Value::as_str))
        .map(|value| package.join(value))
        .collect::<Vec<_>>();
    candidates.push(package.join("src/index"));
    candidates.push(package.join("index"));
    candidates
}

fn resolve_export_target(value: &Value) -> Option<String> {
    if let Some(value) = value.as_str() {
        return Some(value.into());
    }
    let object = value.as_object()?;
    for condition in EXPORT_CONDITIONS {
        if let Some(target) = object.get(*condition).and_then(resolve_export_target) {
            return Some(target);
        }
    }
    None
}

fn path_contained(candidate: &Path, package: &Path) -> bool {
    let candidate = fs::canonicalize(candidate).unwrap_or_else(|_| lexical_normalize(candidate));
    let package = fs::canonicalize(package).unwrap_or_else(|_| lexical_normalize(package));
    candidate.starts_with(package)
}

fn read_jsonc(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    graphoxide_core::parse_jsonc(&text).ok()
}

fn module_edge(
    source: &str,
    target: &str,
    relation: &str,
    source_file: &str,
    line: usize,
    context: &str,
) -> Edge {
    let mut extra = BTreeMap::new();
    extra.insert("_src".into(), source.into());
    extra.insert("_tgt".into(), target.into());
    extra.insert("source_location".into(), format!("L{line}").into());
    extra.insert("weight".into(), 1.0.into());
    extra.insert("context".into(), context.into());
    Edge {
        source: source.into(),
        target: target.into(),
        relation: relation.into(),
        confidence: Confidence::Extracted,
        source_file: source_file.into(),
        extra,
    }
}

fn push_unique_edge(edges: &mut Vec<Edge>, edge: Edge) {
    if edges.iter().any(|existing| {
        existing.true_source() == edge.true_source()
            && existing.true_target() == edge.true_target()
            && existing.relation == edge.relation
            && existing.extra.get("source_location") == edge.extra.get("source_location")
            && existing.extra.get("context") == edge.extra.get("context")
    }) {
        return;
    }
    edges.push(edge);
}

fn line_number(text: &str, offset: usize) -> usize {
    text[..offset].bytes().filter(|byte| *byte == b'\n').count() + 1
}

fn path_without_extension(value: &str) -> String {
    normalize_slashes(
        Path::new(value)
            .with_extension("")
            .to_string_lossy()
            .as_ref(),
    )
}

fn normalize_slashes(value: &str) -> String {
    value.replace('\\', "/")
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() {
                    output.push(component.as_os_str());
                }
            }
            _ => output.push(component.as_os_str()),
        }
    }
    output
}
