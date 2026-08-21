//! Deterministic corpus-level ghost, import, and call resolution.
use graphoxide_core::{make_id, normalize_id, Confidence, Extraction};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, VecDeque},
    mem::size_of,
};

#[derive(Debug)]
struct ResolutionOutputAdmission {
    byte_limit: usize,
    retained_bytes: usize,
    rejected_required_bytes: Option<usize>,
}

thread_local! {
    static RESOLUTION_OUTPUT_ADMISSION: RefCell<Option<ResolutionOutputAdmission>> =
        const { RefCell::new(None) };
}

struct ResolutionOutputGuard;

impl ResolutionOutputGuard {
    fn enter(retained_bytes: usize, byte_limit: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(
            retained_bytes <= byte_limit,
            "resolver input retains {retained_bytes} bytes, exceeding its {byte_limit}-byte output budget"
        );
        RESOLUTION_OUTPUT_ADMISSION.with(|slot| {
            let mut admission = slot.borrow_mut();
            assert!(
                admission.is_none(),
                "nested bounded corpus resolution is unsupported"
            );
            *admission = Some(ResolutionOutputAdmission {
                byte_limit,
                retained_bytes,
                rejected_required_bytes: None,
            });
        });
        Ok(Self)
    }

    fn checkpoint(&self, phase: &'static str, extractions: &[Extraction]) -> anyhow::Result<()> {
        let retained_bytes = crate::extractions_retained_bytes(extractions)?;
        let rejection = RESOLUTION_OUTPUT_ADMISSION.with(|slot| {
            let mut slot = slot.borrow_mut();
            let admission = slot
                .as_mut()
                .expect("bounded resolution admission must remain installed");
            admission.retained_bytes = admission.retained_bytes.max(retained_bytes);
            admission
                .rejected_required_bytes
                .map(|required| (required, admission.byte_limit))
                .or_else(|| {
                    (retained_bytes > admission.byte_limit)
                        .then_some((retained_bytes, admission.byte_limit))
                })
        });
        if let Some((required_bytes, byte_limit)) = rejection {
            anyhow::bail!(
                "corpus resolver {phase} requires at least {required_bytes} retained output bytes, exceeding its {byte_limit}-byte budget"
            );
        }
        Ok(())
    }
}

impl Drop for ResolutionOutputGuard {
    fn drop(&mut self) {
        RESOLUTION_OUTPUT_ADMISSION.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

fn reserve_resolution_output(bytes: usize) -> bool {
    RESOLUTION_OUTPUT_ADMISSION.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(admission) = slot.as_mut() else {
            return true;
        };
        let required = admission.retained_bytes.saturating_add(bytes);
        if required > admission.byte_limit {
            admission.rejected_required_bytes.get_or_insert(required);
            return false;
        }
        admission.retained_bytes = required;
        true
    })
}

/// Append a resolver-produced node only after its retained allocation and a
/// conservative vector-growth charge fit the active output budget. Unbounded
/// legacy resolver callers retain their existing behavior.
pub(crate) fn push_resolved_node(
    nodes: &mut Vec<graphoxide_core::Node>,
    node: graphoxide_core::Node,
) {
    let vector_growth = if nodes.len() == nodes.capacity() {
        nodes
            .capacity()
            .max(4)
            .saturating_mul(size_of::<graphoxide_core::Node>())
    } else {
        0
    };
    let charge = crate::resolver_node_admission_bytes(&node).saturating_add(vector_growth);
    if reserve_resolution_output(charge) {
        nodes.push(node);
    }
}

/// Append a resolver-produced edge under the active retained-output budget.
pub(crate) fn push_resolved_edge(
    edges: &mut Vec<graphoxide_core::Edge>,
    edge: graphoxide_core::Edge,
) {
    let vector_growth = if edges.len() == edges.capacity() {
        edges
            .capacity()
            .max(4)
            .saturating_mul(size_of::<graphoxide_core::Edge>())
    } else {
        0
    };
    let charge = crate::resolver_edge_admission_bytes(&edge).saturating_add(vector_growth);
    if reserve_resolution_output(charge) {
        edges.push(edge);
    }
}
pub fn resolve(extractions: &mut [Extraction]) {
    crate::resolver_registry::run_registered_language_resolvers(extractions);
    crate::ruby::resolve(extractions);
    crate::java::resolve_types(extractions);
    crate::php::resolve_types(extractions);
    crate::pascal::resolve_project_symbols(extractions);
    resolve_language_neutral(extractions);
    crate::csharp::resolve_types(extractions);
    crate::pascal::resolve_inherited_calls(extractions);
}

/// Resolve with access to the physical scan root. JavaScript-family module
/// resolution needs this context for extension probing, tsconfig aliases, and
/// workspace package manifests; all other callers retain [`resolve`].
pub fn resolve_with_root(extractions: &mut [Extraction], root: &std::path::Path) {
    crate::js_resolution::resolve(extractions, root);
    crate::resolver_registry::run_registered_language_resolvers(extractions);
    crate::ruby::resolve(extractions);
    crate::java::resolve_types(extractions);
    crate::php::resolve_types(extractions);
    crate::pascal::resolve_project_symbols(extractions);
    resolve_language_neutral(extractions);
    crate::csharp::resolve_types(extractions);
    crate::pascal::resolve_inherited_calls(extractions);
    resolve_idl_imports(extractions);
}

/// Resolve cross-file IDL import/include edges for Protobuf, Thrift, and
/// other text-based IDL formats.
///
/// The protocol extractor emits `imports` edges pointing at synthetic
/// `protocol_reference` nodes (one per import string). This pass builds a
/// corpus-wide index of `protocol_file` nodes keyed by their source-file
/// basename and suffix, then re-targets each synthetic `imports` edge to the
/// real `protocol_file` node when a unique match exists. Unresolved imports
/// are marked with an `unresolved` extra so the behavior is auditable.
pub(crate) fn resolve_idl_imports(extractions: &mut [Extraction]) {
    // Build an index: file basename (e.g. "common.proto") -> protocol_file node id.
    let mut file_by_basename: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut file_by_path: BTreeMap<String, String> = BTreeMap::new();

    for extraction in extractions.iter() {
        for node in &extraction.nodes {
            if node.extra.get("type").and_then(|v| v.as_str()) != Some("protocol_file") {
                continue;
            }
            let path = std::path::Path::new(&node.source_file);
            let basename = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_owned())
                .unwrap_or_default();
            if !basename.is_empty() {
                file_by_basename
                    .entry(basename)
                    .or_default()
                    .insert(node.id.clone());
            }
            let normalized = node.source_file.replace('\\', "/");
            file_by_path.insert(normalized, node.id.clone());
        }
    }

    if file_by_path.is_empty() {
        return;
    }

    for extraction in extractions.iter_mut() {
        let mut new_edges = Vec::new();
        for edge in extraction.edges.drain(..) {
            if edge.relation != "imports" {
                new_edges.push(edge);
                continue;
            }
            let target_name = edge
                .target
                .strip_prefix("protocol_reference_")
                .unwrap_or(&edge.target)
                .to_owned();

            let resolved = file_by_path.get(&target_name).cloned().or_else(|| {
                file_by_basename.get(&target_name).and_then(|ids| {
                    if ids.len() == 1 {
                        ids.iter().next().cloned()
                    } else {
                        None
                    }
                })
            });

            match resolved {
                Some(real_id) => {
                    let mut resolved_edge = edge.clone();
                    resolved_edge.target = real_id;
                    resolved_edge
                        .extra
                        .insert("resolved".into(), serde_json::Value::from(true));
                    new_edges.push(resolved_edge);
                }
                None => {
                    let mut unresolved_edge = edge.clone();
                    unresolved_edge
                        .extra
                        .insert("unresolved".into(), serde_json::Value::from(true));
                    new_edges.push(unresolved_edge);
                }
            }
        }
        extraction.edges = new_edges;
    }
}

/// Resolve an isolated project while admitting every newly retained resolver
/// fact against `byte_limit`. Checkpoints between language passes also catch
/// in-place metadata growth before another resolver can compound it.
#[cfg(test)]
pub(crate) fn resolve_with_snapshot_bounded(
    extractions: &mut [Extraction],
    snapshot: &crate::js_resolution::ProjectSnapshot,
    byte_limit: usize,
    cpu_arena_bytes: usize,
) -> anyhow::Result<()> {
    let retained_bytes = crate::extractions_retained_bytes(extractions)?;
    let admission = ResolutionOutputGuard::enter(retained_bytes, byte_limit)?;

    let fresh_count = extractions.len();
    crate::js_resolution::resolve_with_snapshot_prefix(
        extractions,
        snapshot,
        fresh_count,
        cpu_arena_bytes,
    )?;
    admission.checkpoint("javascript", extractions)?;
    crate::resolver_registry::run_registered_language_resolvers(extractions);
    admission.checkpoint("registered-language", extractions)?;
    crate::ruby::resolve(extractions);
    admission.checkpoint("ruby", extractions)?;
    crate::java::resolve_types(extractions);
    admission.checkpoint("java", extractions)?;
    crate::php::resolve_types(extractions);
    admission.checkpoint("php", extractions)?;
    crate::pascal::resolve_project_symbols(extractions);
    admission.checkpoint("pascal-project", extractions)?;
    resolve_language_neutral(extractions);
    admission.checkpoint("language-neutral", extractions)?;
    crate::csharp::resolve_types(extractions);
    admission.checkpoint("csharp", extractions)?;
    crate::pascal::resolve_inherited_calls(extractions);
    admission.checkpoint("pascal", extractions)?;
    Ok(())
}

/// Resolve fresh isolated output with read-only JavaScript lookup context.
///
/// The JS phase indexes borrowed baseline node chunks but mutates only fresh
/// extraction indexes. The context is then dropped before a fresh-only guard
/// runs every non-JS pass, so discarded lookup facts cannot consume resolver
/// output admission or be mutated into false low-budget failures.
pub(crate) fn resolve_with_snapshot_context_bounded(
    fresh: &mut [Extraction],
    context: Vec<Extraction>,
    snapshot: &crate::js_resolution::ProjectSnapshot,
    fresh_output_limit: usize,
    cpu_arena_bytes: usize,
) -> anyhow::Result<()> {
    let fresh_retained_bytes = crate::extractions_retained_bytes(fresh)?;
    {
        let admission = ResolutionOutputGuard::enter(fresh_retained_bytes, fresh_output_limit)?;
        crate::js_resolution::resolve_with_snapshot_partitions(
            fresh,
            &context,
            snapshot,
            cpu_arena_bytes,
        )?;
        admission.checkpoint("javascript", fresh)?;
    }
    drop(context);
    let fresh_retained_bytes = crate::extractions_retained_bytes(fresh)?;
    anyhow::ensure!(
        fresh_retained_bytes <= fresh_output_limit,
        "JavaScript resolver fresh output retains {fresh_retained_bytes} bytes, exceeding its {fresh_output_limit}-byte budget after lookup context was dropped"
    );

    let admission = ResolutionOutputGuard::enter(fresh_retained_bytes, fresh_output_limit)?;
    crate::resolver_registry::run_registered_language_resolvers(fresh);
    admission.checkpoint("registered-language", fresh)?;
    crate::ruby::resolve(fresh);
    admission.checkpoint("ruby", fresh)?;
    crate::java::resolve_types(fresh);
    admission.checkpoint("java", fresh)?;
    crate::php::resolve_types(fresh);
    admission.checkpoint("php", fresh)?;
    crate::pascal::resolve_project_symbols(fresh);
    admission.checkpoint("pascal-project", fresh)?;
    resolve_language_neutral(fresh);
    admission.checkpoint("language-neutral", fresh)?;
    crate::csharp::resolve_types(fresh);
    admission.checkpoint("csharp", fresh)?;
    crate::pascal::resolve_inherited_calls(fresh);
    admission.checkpoint("pascal", fresh)?;
    Ok(())
}

#[derive(Debug, Clone)]
struct PythonImportFact {
    extraction: usize,
    source_file_id: String,
    target_module: String,
    imported_name: String,
    local_name: String,
    module_stem: String,
    edge: graphoxide_core::Edge,
}

#[derive(Debug, Clone)]
struct PythonAliasBinding {
    target: String,
    local_name: String,
    imported_name: String,
    module_stem: String,
    import_source_location: String,
}

fn resolve_python_origin(
    file_id: &str,
    name: &str,
    exports: &BTreeMap<(String, String), (String, String)>,
    symbols: &BTreeMap<(String, String), Vec<String>>,
    seen: &mut BTreeSet<(String, String)>,
) -> Option<String> {
    let key = (file_id.to_owned(), normalize_id(name));
    if !seen.insert(key.clone()) {
        return None;
    }
    if let Some((target_file, target_name)) = exports.get(&key) {
        return resolve_python_origin(target_file, target_name, exports, symbols, seen);
    }
    symbols
        .get(&key)
        .filter(|ids| ids.len() == 1)
        .and_then(|ids| ids.first())
        .cloned()
}

/// Resolve Python named imports through package `__init__.py` barrels.
///
/// The AST extractor deliberately emits portable logical IDs before the full
/// corpus is known.  This pass turns those facts into concrete file/symbol
/// edges, follows renamed re-exports, and uses the same binding to resolve bare
/// calls.  Keeping the operation corpus-level avoids filesystem/checkout IDs in
/// the serialized graph and makes multi-hop barrels deterministic.
pub(crate) fn resolve_python_imports(extractions: &mut [Extraction]) {
    let mut file_by_source = BTreeMap::<String, String>::new();
    let mut source_by_file = BTreeMap::<String, String>::new();
    let mut module_candidates = BTreeMap::<String, BTreeSet<String>>::new();
    let mut symbols = BTreeMap::<(String, String), Vec<String>>::new();
    let mut callable_ids = BTreeSet::<String>::new();

    for extraction in extractions.iter() {
        for node in &extraction.nodes {
            if !is_python(&node.source_file) {
                continue;
            }
            if node.extra.get("type").and_then(|value| value.as_str()) == Some("file") {
                file_by_source.insert(node.source_file.clone(), node.id.clone());
                source_by_file.insert(node.id.clone(), node.source_file.clone());
                let path = std::path::Path::new(&node.source_file);
                let package_init = matches!(
                    path.file_name().and_then(|value| value.to_str()),
                    Some("__init__.py" | "__init__.pyi")
                );
                let module_path = if package_init {
                    path.parent()
                        .unwrap_or_else(|| std::path::Path::new(""))
                        .to_string_lossy()
                        .replace('\\', "/")
                } else {
                    path.with_extension("").to_string_lossy().replace('\\', "/")
                };
                // Index every module suffix. This makes
                // `src/mypkg/core.py` discoverable as `mypkg.core`, while the
                // unique-only reduction below refuses to guess if two source
                // roots claim the same suffix.
                let parts = module_path
                    .split('/')
                    .filter(|part| !part.is_empty() && *part != ".")
                    .collect::<Vec<_>>();
                for start in 0..parts.len() {
                    let alias = parts[start..].join("/");
                    if !alias.is_empty() {
                        module_candidates
                            .entry(make_id(&[&alias]))
                            .or_default()
                            .insert(node.id.clone());
                    }
                }
            }
        }
    }
    if file_by_source.is_empty() {
        return;
    }

    let module_files = module_candidates
        .into_iter()
        .filter_map(|(module, files)| {
            (files.len() == 1).then(|| (module, files.into_iter().next().unwrap()))
        })
        .collect::<BTreeMap<_, _>>();

    for extraction in extractions.iter() {
        for node in &extraction.nodes {
            if node.file_type != "code"
                || node.extra.get("type").and_then(|value| value.as_str()) == Some("file")
                || !is_python(&node.source_file)
            {
                continue;
            }
            let Some(file_id) = file_by_source.get(&node.source_file) else {
                continue;
            };
            let name = normalize_id(node.label.trim_start_matches('.').trim_end_matches("()"));
            if !name.is_empty() {
                symbols
                    .entry((file_id.clone(), name))
                    .or_default()
                    .push(node.id.clone());
                if node.extra.get("type").and_then(Value::as_str) == Some("function") {
                    callable_ids.insert(node.id.clone());
                }
            }
        }
    }
    for ids in symbols.values_mut() {
        ids.sort();
        ids.dedup();
    }

    let mut facts = Vec::<PythonImportFact>::new();
    for (index, extraction) in extractions.iter().enumerate() {
        for edge in &extraction.edges {
            if !is_python(&edge.source_file) {
                continue;
            }
            // Function-local imports are lexical evidence, not file-wide
            // evidence. The extractor retains them as private resolution-only
            // facts for auditability, but a corpus pass must not use one to
            // justify calls in an unrelated function in the same module.
            if edge
                .extra
                .get("resolution_only")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
            {
                continue;
            }
            let Some(imported_name) = edge
                .extra
                .get("imported_name")
                .and_then(|value| value.as_str())
            else {
                continue;
            };
            let Some(target_module) = edge
                .extra
                .get("target_module")
                .and_then(|value| value.as_str())
            else {
                continue;
            };
            let local_name = edge
                .extra
                .get("local_alias")
                .and_then(|value| value.as_str())
                .unwrap_or(imported_name);
            facts.push(PythonImportFact {
                extraction: index,
                source_file_id: edge.true_source().to_owned(),
                target_module: target_module.to_owned(),
                imported_name: imported_name.to_owned(),
                local_name: local_name.to_owned(),
                module_stem: edge
                    .extra
                    .get("module_stem")
                    .and_then(|value| value.as_str())
                    .unwrap_or(target_module)
                    .to_owned(),
                edge: edge.clone(),
            });
        }
    }

    let module_file = |module: &str| {
        source_by_file
            .contains_key(module)
            .then(|| module.to_owned())
            .or_else(|| module_files.get(module).cloned())
    };
    let mut exports = BTreeMap::<(String, String), (String, String)>::new();
    let mut reexport_modules = BTreeSet::<(String, String)>::new();
    for fact in &facts {
        let Some(source) = source_by_file.get(&fact.source_file_id) else {
            continue;
        };
        if !matches!(
            std::path::Path::new(source)
                .file_name()
                .and_then(|value| value.to_str()),
            Some("__init__.py" | "__init__.pyi")
        ) {
            continue;
        }
        let Some(target_file) = module_file(&fact.target_module) else {
            continue;
        };
        if target_file == fact.source_file_id {
            continue;
        }
        exports.insert(
            (fact.source_file_id.clone(), normalize_id(&fact.local_name)),
            (target_file, fact.imported_name.clone()),
        );
    }

    let mut aliases = BTreeMap::<(String, String), BTreeMap<String, PythonAliasBinding>>::new();
    let mut generated = Vec::<(usize, graphoxide_core::Edge)>::new();
    let mut consumed_named_imports = BTreeSet::<(usize, String, String, String)>::new();
    for fact in &facts {
        let Some(target_file) = module_file(&fact.target_module) else {
            continue;
        };
        if target_file == fact.source_file_id {
            continue;
        }
        if exports.contains_key(&(fact.source_file_id.clone(), normalize_id(&fact.local_name))) {
            let mut edge = fact.edge.clone();
            edge.target = target_file.clone();
            edge.relation = "re_exports".into();
            edge.extra.insert("_tgt".into(), target_file.clone().into());
            edge.extra.insert("context".into(), "export".into());
            reexport_modules.insert((fact.source_file_id.clone(), fact.target_module.clone()));
            generated.push((fact.extraction, edge));
            consumed_named_imports.insert((
                fact.extraction,
                fact.edge.true_source().to_owned(),
                fact.edge.true_target().to_owned(),
                fact.imported_name.clone(),
            ));
        }
        let Some(target) = resolve_python_origin(
            &target_file,
            &fact.imported_name,
            &exports,
            &symbols,
            &mut BTreeSet::new(),
        ) else {
            continue;
        };
        aliases
            .entry((fact.source_file_id.clone(), normalize_id(&fact.local_name)))
            .or_default()
            .entry(target.clone())
            .or_insert_with(|| PythonAliasBinding {
                target: target.clone(),
                local_name: fact.local_name.clone(),
                imported_name: fact.imported_name.clone(),
                module_stem: fact.module_stem.clone(),
                import_source_location: fact
                    .edge
                    .extra
                    .get("source_location")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_owned(),
            });
        let mut edge = fact.edge.clone();
        edge.target = target.clone();
        edge.relation = "imports".into();
        edge.extra.insert("_tgt".into(), target.into());
        edge.extra.insert("context".into(), "import".into());
        generated.push((fact.extraction, edge));
        consumed_named_imports.insert((
            fact.extraction,
            fact.edge.true_source().to_owned(),
            fact.edge.true_target().to_owned(),
            fact.imported_name.clone(),
        ));
    }

    for (index, mut edge) in generated {
        // Keep the local spelling until the language-neutral pass has used it
        // for qualified receiver calls. That pass removes all private import
        // facts before serialization.
        for key in ["imported_name", "target_module"] {
            edge.extra.remove(key);
        }
        let duplicate = extractions[index].edges.iter().any(|existing| {
            existing.true_source() == edge.true_source()
                && existing.true_target() == edge.true_target()
                && existing.relation == edge.relation
        });
        if !duplicate {
            push_resolved_edge(&mut extractions[index].edges, edge);
        }
    }

    for (index, extraction) in extractions.iter_mut().enumerate() {
        extraction.edges.retain(|edge| {
            if !is_python(&edge.source_file) || edge.relation != "imports_from" {
                return true;
            }
            // `from module import name` initially emits portable facts for
            // both the module and symbol. Once the concrete `imports` edge is
            // generated above, retaining the raw named fact duplicates it as
            // an `imports_from` edge to the symbol. Package barrels likewise
            // expose `re_exports`, not a second file-import edge.
            let consumed = edge
                .extra
                .get("imported_name")
                .and_then(Value::as_str)
                .is_some_and(|imported_name| {
                    consumed_named_imports.contains(&(
                        index,
                        edge.true_source().to_owned(),
                        edge.true_target().to_owned(),
                        imported_name.to_owned(),
                    ))
                });
            !consumed
                && !edge
                    .extra
                    .get("target_module")
                    .and_then(Value::as_str)
                    .is_some_and(|module| {
                        reexport_modules
                            .contains(&(edge.true_source().to_owned(), module.to_owned()))
                    })
        });
        for edge in &mut extraction.edges {
            if is_python(&edge.source_file)
                && matches!(edge.relation.as_str(), "imports" | "imports_from")
            {
                let logical_target = edge.true_target().to_owned();
                if let Some(file_target) = module_files.get(&logical_target)
                    && file_target != edge.true_source()
                {
                    edge.target = file_target.clone();
                    edge.extra.insert("_tgt".into(), file_target.clone().into());
                }
            }
            if !edge
                .extra
                .get("unresolved_call")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
                || !is_python(&edge.source_file)
            {
                continue;
            }
            let Some(file_id) = file_by_source.get(&edge.source_file) else {
                continue;
            };
            let callee = edge
                .extra
                .get("callee")
                .and_then(|value| value.as_str())
                .map(normalize_id)
                .unwrap_or_default();
            let Some(bindings) = aliases
                .get(&(file_id.clone(), callee))
                .filter(|bindings| bindings.len() == 1)
            else {
                continue;
            };
            let binding = bindings.values().next().expect("one alias binding");
            let target = binding.target.clone();
            if edge.relation == "indirect_call" && !callable_ids.contains(&target) {
                // A named import may legitimately resolve a direct call to a
                // class constructor, but passing that class as a descriptor
                // is not callback dispatch.  Leave the indirect fact
                // unresolved so the language-neutral callable-only pass
                // discards it rather than manufacturing a class dependency.
                continue;
            }
            edge.target = target.clone();
            edge.extra.insert("_tgt".into(), target.into());
            edge.extra.remove("unresolved_call");
            edge.extra.remove("callee");
            edge.extra.remove("member_call");
            // The upstream resolver distinguishes import-backed direct calls
            // from the language-neutral raw-call pass in both context and
            // metadata. Indirect references retain their original expression
            // context and Graphify's stronger 0.8 inference score.
            if edge.relation == "calls" {
                edge.extra
                    .insert("context".into(), "import_guided_call".into());
            }
            let confidence_score = if edge.relation == "indirect_call" {
                0.8
            } else {
                1.0
            };
            edge.extra
                .insert("confidence_score".into(), confidence_score.into());
            let metadata = Map::from_iter([
                ("resolver".into(), Value::from("python_import_guided")),
                ("local_name".into(), Value::from(binding.local_name.clone())),
                (
                    "imported_name".into(),
                    Value::from(binding.imported_name.clone()),
                ),
                (
                    "module_stem".into(),
                    Value::from(binding.module_stem.clone()),
                ),
                (
                    "import_source_location".into(),
                    Value::from(binding.import_source_location.clone()),
                ),
            ]);
            edge.extra.insert(
                "metadata".into(),
                Value::Object(graphoxide_core::sanitize_metadata(Some(&metadata))),
            );
        }
    }
}

fn rust_module_identity(source_file: &str) -> Option<String> {
    let source = source_file.replace('\\', "/");
    if source.starts_with('/') || source.contains(':') {
        return None;
    }
    let mut parts = source.split('/').map(str::to_owned).collect::<Vec<_>>();
    if parts
        .iter()
        .any(|part| part.is_empty() || matches!(part.as_str(), "." | ".."))
    {
        return None;
    }
    let filename = parts.pop()?;
    let stem = filename.strip_suffix(".rs")?;
    if stem.is_empty() {
        return None;
    }
    if !matches!(stem, "mod" | "lib" | "main") {
        parts.push(stem.to_owned());
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

pub(crate) fn resolve_rust_imports(extractions: &mut [Extraction]) {
    let mut modules = BTreeMap::<String, Vec<(String, String)>>::new();
    let mut nodes_by_source = BTreeMap::<String, BTreeSet<String>>::new();
    for node in extractions.iter().flat_map(|extraction| &extraction.nodes) {
        if !node.source_file.ends_with(".rs") {
            continue;
        }
        nodes_by_source
            .entry(node.source_file.clone())
            .or_default()
            .insert(node.id.clone());
        if node.extra.get("type").and_then(Value::as_str) == Some("file")
            && let Some(module) = rust_module_identity(&node.source_file)
        {
            modules
                .entry(module)
                .or_default()
                .push((node.id.clone(), node.source_file.clone()));
        }
    }
    for candidates in modules.values_mut() {
        candidates.sort();
        candidates.dedup();
    }

    for edge in extractions
        .iter_mut()
        .flat_map(|extraction| &mut extraction.edges)
    {
        if !edge.source_file.ends_with(".rs") || edge.relation != "imports_from" {
            continue;
        }
        let Some(path) = edge
            .extra
            .remove(crate::engine::RUST_IMPORT_PATH)
            .and_then(|value| value.as_str().map(str::to_owned))
        else {
            continue;
        };
        let mut candidates = modules.get(&path).cloned().unwrap_or_default();
        if let Some((module, _)) = path.rsplit_once('/')
            && let Some(module_files) = modules.get(module)
        {
            let expected = make_id(&[&path]);
            for (_, source) in module_files {
                if nodes_by_source
                    .get(source)
                    .is_some_and(|ids| ids.contains(&expected))
                {
                    candidates.push((expected.clone(), source.clone()));
                }
            }
        }
        candidates.sort();
        candidates.dedup();
        if let [candidate] = candidates.as_slice() {
            edge.target = candidate.0.clone();
            edge.extra.insert("_tgt".into(), candidate.0.clone().into());
            edge.extra
                .insert("target_file".into(), candidate.1.clone().into());
        }
    }
}

fn owner_hierarchy_distances(
    owner: &str,
    parents: &BTreeMap<String, BTreeSet<String>>,
    include_current: bool,
) -> BTreeMap<String, usize> {
    let mut distances = BTreeMap::from([(owner.to_owned(), 0usize)]);
    let mut pending = VecDeque::from([owner.to_owned()]);
    while let Some(current) = pending.pop_front() {
        let distance = distances[&current];
        for parent in parents.get(&current).into_iter().flatten() {
            if distances.contains_key(parent) {
                continue;
            }
            distances.insert(parent.clone(), distance + 1);
            pending.push_back(parent.clone());
        }
    }
    if !include_current {
        distances.remove(owner);
    }
    distances
}

fn resolve_language_neutral(extractions: &mut [Extraction]) {
    bind_exact_project_relative_placeholders(extractions);
    partition_incompatible_dangling_targets(extractions);
    disambiguate(extractions);
    crate::native::resolve_calls(extractions);
    let mut definitions: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut type_definitions: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut call_definitions: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut file_nodes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut source_by_file_id: BTreeMap<String, String> = BTreeMap::new();
    let mut source_by_node_id: BTreeMap<String, String> = BTreeMap::new();
    let mut node_labels: BTreeMap<String, String> = BTreeMap::new();
    let mut exact_node_labels: BTreeMap<String, String> = BTreeMap::new();
    for extraction in extractions.iter() {
        for node in &extraction.nodes {
            if node.source_file.is_empty() {
                continue;
            }
            node_labels.insert(node.id.clone(), normalize_id(&node.label));
            exact_node_labels.insert(
                node.id.clone(),
                node.label
                    .trim_start_matches('.')
                    .trim_end_matches("()")
                    .to_owned(),
            );
            source_by_node_id.insert(node.id.clone(), node.source_file.clone());
            if node.file_type != "code" {
                continue;
            }
            if node.extra.get("type").and_then(|v| v.as_str()) == Some("function") {
                let label = normalize_id(node.label.trim_start_matches('.').trim_end_matches("()"));
                call_definitions
                    .entry(label)
                    .or_default()
                    .push((node.id.clone(), node.source_file.clone()));
                continue;
            }
            let label = normalize_id(node.label.trim_start_matches('.').trim_end_matches("()"));
            definitions
                .entry(label)
                .or_default()
                .push((node.id.clone(), node.source_file.clone()));
            if node.extra.get("type").and_then(|v| v.as_str()) == Some("class") {
                type_definitions
                    .entry(normalize_id(
                        node.label.trim_start_matches('.').trim_end_matches("()"),
                    ))
                    .or_default()
                    .push((node.id.clone(), node.source_file.clone()));
            }
            if node.source_location.as_deref() == Some("L1") {
                let stem = std::path::Path::new(&node.source_file)
                    .file_stem()
                    .and_then(|v| v.to_str())
                    .unwrap_or("");
                file_nodes
                    .entry(normalize_id(stem))
                    .or_default()
                    .push(node.id.clone());
            }
            if node.extra.get("type").and_then(|value| value.as_str()) == Some("file") {
                source_by_file_id.insert(node.id.clone(), node.source_file.clone());
            }
        }
    }
    for values in definitions.values_mut() {
        values.sort();
        values.dedup_by(|left, right| left.0 == right.0)
    }
    for values in file_nodes.values_mut() {
        values.sort();
        values.dedup()
    }
    for values in call_definitions.values_mut() {
        values.sort();
        values.dedup();
    }
    for values in type_definitions.values_mut() {
        values.sort();
        values.dedup();
    }
    let mut method_owners = BTreeMap::new();
    let mut method_owner_ids = BTreeMap::new();
    let mut type_parents = BTreeMap::<String, BTreeSet<String>>::new();
    for extraction in extractions.iter() {
        for edge in &extraction.edges {
            if edge.relation == "method"
                && let Some(owner) = node_labels.get(edge.true_source())
            {
                method_owners.insert(edge.true_target().to_owned(), owner.clone());
                method_owner_ids
                    .insert(edge.true_target().to_owned(), edge.true_source().to_owned());
            }
            if matches!(
                edge.relation.as_str(),
                "inherits" | "implements" | "mixes_in" | "extends"
            ) {
                type_parents
                    .entry(edge.true_source().to_owned())
                    .or_default()
                    .insert(edge.true_target().to_owned());
            }
        }
    }
    let mut incoming_stub_uses: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for extraction in extractions.iter() {
        for edge in &extraction.edges {
            let source_file = if edge.source_file.is_empty() {
                source_by_node_id
                    .get(edge.true_source())
                    .cloned()
                    .unwrap_or_default()
            } else {
                edge.source_file.clone()
            };
            incoming_stub_uses
                .entry(edge.true_target().to_owned())
                .or_default()
                .push((edge.relation.clone(), source_file));
        }
    }
    let mut remap = BTreeMap::new();
    for extraction in extractions.iter() {
        for node in &extraction.nodes {
            let key = normalize_id(node.label.trim_start_matches('.').trim_end_matches("()"));
            if node.source_file.is_empty() {
                if node
                    .extra
                    .get(crate::csharp::MANAGED_NODE)
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    continue;
                }
                if let Some(ids) = definitions.get(&key) {
                    let origin_file = node
                        .extra
                        .get("origin_file")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    let compatible: Vec<_> = ids
                        .iter()
                        .filter(|(id, source)| {
                            same_language_family(origin_file, source)
                                && resolution_case_matches(
                                    origin_file,
                                    node.label.trim_start_matches('.').trim_end_matches("()"),
                                    exact_node_labels.get(id).map(String::as_str).unwrap_or(""),
                                )
                        })
                        .collect();
                    if compatible.len() == 1 {
                        remap.insert(node.id.clone(), compatible[0].0.clone());
                    }
                } else if node.label.trim().ends_with("()") {
                    let uses = incoming_stub_uses
                        .get(&node.id)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]);
                    let is_supertype = uses.iter().any(|(relation, _)| {
                        matches!(relation.as_str(), "inherits" | "implements" | "extends")
                    });
                    let source_files: Vec<_> = uses
                        .iter()
                        .map(|(_, source)| source)
                        .filter(|source| !source.is_empty())
                        .collect();
                    if !is_supertype && !source_files.is_empty() {
                        let candidates: Vec<_> = call_definitions
                            .get(&key)
                            .into_iter()
                            .flatten()
                            .filter(|(id, _)| !method_owners.contains_key(id))
                            .filter(|(id, _)| {
                                source_files.iter().all(|origin| {
                                    resolution_case_matches(
                                        origin,
                                        node.label.trim_start_matches('.').trim_end_matches("()"),
                                        exact_node_labels.get(id).map(String::as_str).unwrap_or(""),
                                    )
                                })
                            })
                            .filter(|(_, source)| {
                                source_files
                                    .iter()
                                    .all(|referrer| same_language_family(referrer, source))
                            })
                            .collect();
                        if candidates.len() == 1 {
                            remap.insert(node.id.clone(), candidates[0].0.clone());
                        }
                    }
                }
            } else if node.extra.get("type").and_then(|v| v.as_str()) == Some("module")
                && node.extra.get("swift_module").and_then(Value::as_bool) != Some(true)
                && node
                    .extra
                    .get(crate::project_path::EXACT_PROJECT_RELATIVE_PLACEHOLDER)
                    .and_then(Value::as_bool)
                    != Some(true)
                && let Some(ids) = file_nodes.get(&key)
                && ids.len() == 1
            {
                remap.insert(node.id.clone(), ids[0].clone());
            }
        }
    }
    for extraction in extractions {
        let imported_aliases: BTreeMap<String, Vec<String>> = extraction
            .edges
            .iter()
            .filter(|edge| matches!(edge.relation.as_str(), "imports" | "imports_from"))
            .filter(|edge| {
                !edge
                    .extra
                    .get("resolution_only")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false)
            })
            .filter_map(|edge| {
                edge.extra
                    .get("local_alias")
                    .and_then(|value| value.as_str())
                    .map(|alias| (normalize_id(alias), normalize_id(edge.true_target())))
            })
            .fold(BTreeMap::new(), |mut aliases, (alias, target)| {
                aliases.entry(alias).or_default().push(target);
                aliases
            });
        let import_targets: Vec<String> = extraction
            .edges
            .iter()
            .filter(|edge| matches!(edge.relation.as_str(), "imports" | "imports_from"))
            .filter(|edge| {
                !edge
                    .extra
                    .get("resolution_only")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false)
            })
            .map(|edge| normalize_id(edge.true_target()))
            .collect();
        let explicit_symbol_imports = extraction
            .edges
            .iter()
            .filter(|edge| edge.relation == "imports")
            .filter(|edge| {
                !edge
                    .extra
                    .get("resolution_only")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            })
            .map(|edge| edge.true_target().to_owned())
            .collect::<BTreeSet<_>>();
        let bash_import_sources: BTreeSet<String> = extraction
            .edges
            .iter()
            .filter(|edge| edge.relation == "imports_from")
            .filter_map(|edge| source_by_file_id.get(edge.true_target()).cloned())
            .collect();
        let raw_bash_calls: Vec<_> = extraction
            .edges
            .iter()
            .filter(|edge| edge.relation == "__bash_raw_call")
            .cloned()
            .collect();
        extraction
            .edges
            .retain(|edge| edge.relation != "__bash_raw_call");
        for mut raw in raw_bash_calls {
            if !is_bash(&raw.source_file) {
                continue;
            }
            let callee = raw
                .extra
                .get("callee")
                .and_then(|value| value.as_str())
                .map(normalize_id)
                .unwrap_or_default();
            let candidates: Vec<_> = call_definitions
                .get(&callee)
                .into_iter()
                .flatten()
                .filter(|(_, source)| bash_import_sources.contains(source))
                .collect();
            if candidates.len() != 1 {
                continue;
            }
            raw.target = candidates[0].0.clone();
            raw.relation = "calls".into();
            raw.extra.remove("callee");
            push_resolved_edge(&mut extraction.edges, raw);
        }
        extraction.edges.retain_mut(|edge| {
            if !edge
                .extra
                .get("unresolved_call")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return true;
            }
            let callee_name = edge
                .extra
                .get("callee")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned();
            let callee = normalize_id(&callee_name);
            let receiver_type = edge
                .extra
                .get("receiver_type")
                .and_then(|value| value.as_str())
                .map(normalize_id);
            let member_call = edge
                .extra
                .get("member_call")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let self_receiver =
                edge.extra.get("receiver").and_then(|value| value.as_str()) == Some("self");
            let receiver = edge
                .extra
                .get("receiver")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let receiver_owner = edge
                .extra
                .get("receiver_owner")
                .and_then(|value| value.as_str())
                .map(str::to_owned);
            let receiver_scope = edge
                .extra
                .get("receiver_scope")
                .and_then(|value| value.as_str())
                .unwrap_or("current");
            let receiver_owner_distances = receiver_owner.as_deref().map(|owner| {
                owner_hierarchy_distances(owner, &type_parents, receiver_scope != "super")
            });
            let receiver_key = normalize_id(receiver);
            let receiver_import_targets = imported_aliases
                .get(&receiver_key)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let class_receiver_keys: BTreeSet<String> = if receiver_type.is_some() {
                BTreeSet::new()
            } else if receiver.chars().next().is_some_and(char::is_uppercase) {
                let mut keys = BTreeSet::from([receiver_key.clone()]);
                if let Some(targets) = imported_aliases.get(&receiver_key) {
                    for key in type_definitions.keys() {
                        if targets
                            .iter()
                            .any(|target| target == key || target.ends_with(&format!("_{key}")))
                        {
                            keys.insert(key.clone());
                        }
                    }
                }
                keys.retain(|key| type_definitions.get(key).is_some_and(|ids| ids.len() == 1));
                keys
            } else {
                BTreeSet::new()
            };
            let python_call = is_python(&edge.source_file);
            // A JavaScript import alias is stronger evidence than a global
            // label match. It also covers renamed/default imports whose local
            // call spelling is intentionally different from the declaration
            // label (`import mk from './factory'; mk()`).
            let explicit_import_targets = imported_aliases
                .get(&callee)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let named_candidates: Vec<_> = if !member_call && !explicit_import_targets.is_empty() {
                call_definitions
                    .values()
                    .flatten()
                    .filter(|(id, _)| {
                        let id = normalize_id(id);
                        explicit_import_targets.iter().any(|target| target == &id)
                    })
                    .collect()
            } else {
                // Graphify's callable normalization is deliberately
                // case-folded. Case-sensitive identity protection applies to
                // type/value references above, not to this raw-call index.
                call_definitions
                    .get(&callee)
                    .into_iter()
                    .flatten()
                    .collect()
            };
            let candidates: Vec<_> = named_candidates
                .into_iter()
                .filter(|(_, source)| same_language_family(&edge.source_file, source))
                .filter(|(id, _)| id != edge.true_source())
                .filter(|(id, source)| {
                    if python_call {
                        if let Some(receiver_type) = &receiver_type {
                            return method_owners.get(id) == Some(receiver_type);
                        }
                        if self_receiver {
                            return false;
                        }
                        if member_call {
                            if !class_receiver_keys.is_empty() {
                                return method_owners.get(id).is_some_and(|owner| {
                                    class_receiver_keys.contains(owner)
                                        && (source == &edge.source_file
                                            || !receiver_import_targets.is_empty())
                                });
                            }
                            if !receiver_import_targets.is_empty()
                                && !method_owners.contains_key(id)
                            {
                                let module = std::path::Path::new(source)
                                    .with_extension("")
                                    .to_string_lossy()
                                    .replace('\\', "/");
                                let module = normalize_id(&module);
                                return receiver_import_targets.iter().any(|target| {
                                    target == &module || target.ends_with(&format!("_{module}"))
                                });
                            }
                            // An untyped `obj.method()` receiver is not enough
                            // evidence to bind to a same-named free function.
                            // Module member calls are likewise left unresolved
                            // until receiver/import facts model them explicitly.
                            return false;
                        }
                        return true;
                    }
                    if member_call {
                        if let Some(distances) = &receiver_owner_distances {
                            return method_owner_ids
                                .get(id)
                                .is_some_and(|owner| distances.contains_key(owner));
                        }
                        return receiver_type.as_ref().is_some_and(|receiver_type| {
                            method_owners.get(id) == Some(receiver_type)
                        });
                    }
                    if source == &edge.source_file {
                        return method_owners.get(id).is_none_or(|owner| {
                            method_owners.get(edge.true_source()) == Some(owner)
                        });
                    }
                    true
                })
                .filter(|(_, source)| {
                    if source == &edge.source_file {
                        return true;
                    }
                    if same_go_package(&edge.source_file, source) {
                        return true;
                    }
                    if same_jvm_package(&edge.source_file, source) {
                        return true;
                    }
                    if !is_javascript_family_path(&edge.source_file) {
                        // JS/TS modules have no implicit cross-module scope.
                        // Other language families retain Graphify's guarded
                        // single-candidate resolution for autoload, global
                        // functions, native headers, and Python modules.
                        return true;
                    }
                    let stem = std::path::Path::new(source)
                        .file_stem()
                        .and_then(|v| v.to_str())
                        .map(normalize_id)
                        .unwrap_or_default();
                    !stem.is_empty()
                        && import_targets
                            .iter()
                            .any(|target| target == &stem || target.ends_with(&format!("_{stem}")))
                })
                .collect();
            let nearest_owner_distance = receiver_owner_distances.as_ref().and_then(|distances| {
                candidates
                    .iter()
                    .filter_map(|(id, _)| {
                        method_owner_ids
                            .get(id)
                            .and_then(|owner| distances.get(owner))
                    })
                    .copied()
                    .min()
            });
            let candidates = if let Some(nearest) = nearest_owner_distance {
                candidates
                    .into_iter()
                    .filter(|(id, _)| {
                        method_owner_ids
                            .get(id)
                            .and_then(|owner| {
                                receiver_owner_distances
                                    .as_ref()
                                    .and_then(|distances| distances.get(owner))
                            })
                            .is_some_and(|distance| *distance == nearest)
                    })
                    .collect::<Vec<_>>()
            } else {
                candidates
            };
            let target = if receiver_owner_distances.is_some() {
                match candidates.as_slice() {
                    [only] => Some(only.0.as_str()),
                    _ => None,
                }
            } else {
                disambiguate_call_candidates(&candidates, &edge.source_file)
            }
            .map(ToOwned::to_owned);
            // Keep unresolved facts for raw extraction and audit output. Their
            // placeholder endpoint intentionally has no node; the graph builder
            // can omit it while diagnostics retain the source span and callee.
            let Some(target) = target else {
                // Unknown direct calls remain auditable unresolved facts. An
                // indirect reference, however, is only graph evidence after it
                // resolves to one real callable; retaining an unknown callback
                // manufactures a dependency that the source never established.
                // Java member syntax likewise carries a strict receiver
                // contract: after typed resolution fails, retaining a
                // name-shaped endpoint would manufacture a phantom method
                // dependency (and contradict Java overload/ambiguity rules).
                return edge.relation != "indirect_call"
                    && !(member_call && is_java(&edge.source_file));
            };
            let target_source = source_by_node_id.get(&target).map(String::as_str);
            let same_file = target_source == Some(edge.source_file.as_str());
            let import_backed = !explicit_import_targets.is_empty()
                || (member_call && !receiver_import_targets.is_empty());
            edge.target = target.clone();
            if edge.relation == "calls" {
                edge.confidence = if same_file || import_backed {
                    Confidence::Extracted
                } else {
                    Confidence::Inferred
                };
                if edge.confidence == Confidence::Inferred {
                    // Graphify's language-neutral raw-call resolver assigns a
                    // deliberately stronger score than generic inference.
                    edge.extra.insert("confidence_score".into(), 0.8.into());
                }
            }
            edge.extra.insert("_tgt".into(), target.into());
            edge.extra.remove("unresolved_call");
            edge.extra.remove("callee");
            edge.extra.remove("member_call");
            edge.extra.remove("receiver_owner");
            edge.extra.remove("receiver_scope");
            true
        });
        // Some extractors can already bind a raw call to the same portable
        // symbol ID that a later module pass resolves. The endpoint is sound,
        // but it must still be promoted from inferred to extracted when an
        // explicit symbol import proves that binding.
        for edge in &mut extraction.edges {
            if edge.relation == "calls"
                && !edge
                    .extra
                    .get("unresolved_call")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                && explicit_symbol_imports.contains(edge.true_target())
            {
                edge.confidence = Confidence::Extracted;
                edge.extra.insert("confidence_score".into(), 1.0.into());
            }
        }
        for edge in &mut extraction.edges {
            if let Some(id) = remap.get(edge.true_source()) {
                edge.source = id.clone();
                edge.extra.insert("_src".into(), id.clone().into());
            }
            if edge
                .extra
                .get(crate::project_path::EXACT_PROJECT_RELATIVE_PLACEHOLDER)
                .and_then(Value::as_bool)
                != Some(true)
                && let Some(id) = remap.get(edge.true_target())
            {
                edge.target = id.clone();
                edge.extra.insert("_tgt".into(), id.clone().into());
            }
            edge.extra.remove("local_alias");
            edge.extra.remove("imported_name");
            edge.extra.remove("target_module");
            edge.extra.remove("module_stem");
        }
        extraction.nodes.retain(|node| {
            node.extra
                .get(crate::project_path::EXACT_PROJECT_RELATIVE_PLACEHOLDER)
                .and_then(Value::as_bool)
                == Some(true)
                || !remap.contains_key(&node.id)
        });
        for node in &mut extraction.nodes {
            node.extra.remove("origin_file");
        }
        let mut seen_calls = BTreeSet::new();
        extraction.edges.retain(|edge| {
            !edge
                .extra
                .get("resolution_only")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
                && (!matches!(edge.relation.as_str(), "calls" | "indirect_call")
                    || seen_calls.insert((
                        edge.true_source().to_owned(),
                        edge.true_target().to_owned(),
                        edge.relation.clone(),
                        edge.extra
                            .get("unresolved_call")
                            .and_then(|value| value.as_bool())
                            .unwrap_or(false),
                        edge.extra
                            .get("member_call")
                            .and_then(|value| value.as_bool())
                            .unwrap_or(false),
                        edge.extra
                            .get("receiver")
                            .and_then(|value| value.as_str())
                            .unwrap_or("")
                            .to_owned(),
                        edge.extra
                            .get("receiver_type")
                            .and_then(|value| value.as_str())
                            .unwrap_or("")
                            .to_owned(),
                    )))
        });
    }
}

/// Drop an extractor-authored placeholder once the exact logical file it
/// names is present in the corpus. This happens before ID disambiguation so a
/// placeholder can never overwrite or merge with the real structural file
/// node merely because extraction order changed.
fn bind_exact_project_relative_placeholders(extractions: &mut [Extraction]) {
    let exact_files = extractions
        .iter()
        .flat_map(|extraction| &extraction.nodes)
        .filter(|node| {
            node.extra.get("type").and_then(Value::as_str) == Some("file")
                && node
                    .extra
                    .get(crate::project_path::EXACT_PROJECT_RELATIVE_PLACEHOLDER)
                    .and_then(Value::as_bool)
                    != Some(true)
        })
        .map(|node| (node.id.clone(), node.source_file.replace('\\', "/")))
        .collect::<BTreeSet<_>>();

    for extraction in extractions {
        extraction.nodes.retain(|node| {
            if node
                .extra
                .get(crate::project_path::EXACT_PROJECT_RELATIVE_PLACEHOLDER)
                .and_then(Value::as_bool)
                != Some(true)
            {
                return true;
            }
            let Some(target_file) = node.extra.get("target_file").and_then(Value::as_str) else {
                return true;
            };
            !exact_files.contains(&(node.id.clone(), target_file.replace('\\', "/")))
        });
    }
}

/// A dangling import/reference endpoint may intentionally name a file that is
/// not part of the scan. If another language happens to define the same global
/// id, leaving the raw endpoint untouched turns that unrelated definition into
/// the target when the graph is assembled. Partition only when every concrete
/// owner of the id is from an incompatible language; compatible file/import
/// identity remains unchanged.
fn partition_incompatible_dangling_targets(extractions: &mut [Extraction]) {
    let mut origins_by_id: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for extraction in extractions.iter() {
        for node in &extraction.nodes {
            let origin = if node.source_file.is_empty() {
                node.extra
                    .get("origin_file")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
            } else {
                &node.source_file
            };
            if !origin.is_empty() {
                origins_by_id
                    .entry(node.id.clone())
                    .or_default()
                    .insert(origin.to_owned());
            }
        }
    }
    for extraction in extractions {
        let local_ids: BTreeSet<_> = extraction
            .nodes
            .iter()
            .map(|node| node.id.clone())
            .collect();
        for edge in &mut extraction.edges {
            let target = edge.true_target().to_owned();
            if local_ids.contains(&target) || edge.source_file.is_empty() {
                continue;
            }
            let Some(origins) = origins_by_id.get(&target) else {
                continue;
            };
            if origins
                .iter()
                .all(|origin| !same_language_family(&edge.source_file, origin))
            {
                let partitioned = make_id(&[&edge.source_file, &target]);
                edge.target = partitioned.clone();
                edge.extra.insert("_tgt".into(), partitioned.into());
            }
        }
    }
}

fn is_test_path(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let normalized = path.replace('\\', "/");
    let mut segments: Vec<_> = normalized
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.iter().any(|segment| {
        matches!(
            segment.to_ascii_lowercase().as_str(),
            "tests" | "test" | "spec" | "specs" | "__tests__"
        )
    }) {
        return true;
    }
    let Some(filename) = segments.pop() else {
        return false;
    };
    let lower = filename.to_ascii_lowercase();
    lower.starts_with("test_")
        || lower.contains("_test.")
        || lower.contains(".test.")
        || lower.contains(".spec.")
        || lower.contains("_spec.")
        || lower.ends_with(".tests.ps1")
        || filename.ends_with("Test.java")
        || filename.ends_with("Tests.java")
        || filename.ends_with("Tests.cs")
}

fn normalized_parts(path: &str) -> Vec<&str> {
    path.split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .collect()
}

fn proximity_winner<'a>(call_site: &str, candidates: &[&'a (String, String)]) -> Option<&'a str> {
    if call_site.is_empty() {
        return None;
    }
    let call_normalized = call_site.replace('\\', "/");
    let same_file: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|(_, source)| source.replace('\\', "/") == call_normalized)
        .collect();
    if same_file.len() == 1 {
        return Some(same_file[0].0.as_str());
    }
    if same_file.len() > 1 {
        return None;
    }

    let call_parts = normalized_parts(&call_normalized);
    let call_directory = &call_parts[..call_parts.len().saturating_sub(1)];
    let same_directory: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|(_, source)| {
            let parts = normalized_parts(source);
            parts[..parts.len().saturating_sub(1)] == *call_directory
        })
        .collect();
    if same_directory.len() == 1 {
        return Some(same_directory[0].0.as_str());
    }
    if same_directory.len() > 1 {
        return None;
    }

    let mut scored: Vec<_> = candidates
        .iter()
        .copied()
        .map(|candidate| {
            let parts = normalized_parts(&candidate.1);
            let directory = &parts[..parts.len().saturating_sub(1)];
            let score = call_directory
                .iter()
                .zip(directory)
                .take_while(|(left, right)| left == right)
                .count();
            (candidate, score)
        })
        .collect();
    scored.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| left.0 .0.cmp(&right.0 .0))
    });
    let best = scored.first()?.1;
    (best > 0 && scored.iter().filter(|(_, score)| *score == best).count() == 1)
        .then(|| scored[0].0 .0.as_str())
}

fn disambiguate_call_candidates<'a>(
    candidates: &[&'a (String, String)],
    call_site_file: &str,
) -> Option<&'a str> {
    match candidates {
        [] => return None,
        [only] => return Some(only.0.as_str()),
        _ => {}
    }

    let test_candidates: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|(_, source)| is_test_path(source))
        .collect();
    let non_test: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|(_, source)| !is_test_path(source))
        .collect();
    let survivors = if is_test_path(call_site_file) {
        let normalized = call_site_file.replace('\\', "/");
        let local: Vec<_> = test_candidates
            .iter()
            .copied()
            .filter(|(_, source)| source.replace('\\', "/") == normalized)
            .collect();
        if local.len() == 1 {
            return Some(local[0].0.as_str());
        }
        if test_candidates.is_empty() {
            if non_test.is_empty() {
                candidates.to_vec()
            } else {
                non_test
            }
        } else {
            test_candidates
        }
    } else {
        non_test
    };
    if survivors.len() == 1 {
        return Some(survivors[0].0.as_str());
    }
    if survivors.is_empty() {
        return None;
    }
    proximity_winner(call_site_file, &survivors)
}

fn is_python(path: &str) -> bool {
    matches!(extension(std::path::Path::new(path)).as_str(), "py" | "pyi")
}

fn is_java(path: &str) -> bool {
    extension(std::path::Path::new(path)) == "java"
}

fn is_bash(path: &str) -> bool {
    matches!(
        extension(std::path::Path::new(path)).as_str(),
        "sh" | "bash" | "zsh" | "dash" | "ksh"
    )
}

fn same_go_package(a: &str, b: &str) -> bool {
    let a = std::path::Path::new(a);
    let b = std::path::Path::new(b);
    extension(a) == "go" && extension(b) == "go" && a.parent() == b.parent()
}

fn same_jvm_package(a: &str, b: &str) -> bool {
    let a = std::path::Path::new(a);
    let b = std::path::Path::new(b);
    is_jvm_extension(&extension(a)) && is_jvm_extension(&extension(b)) && a.parent() == b.parent()
}

fn is_javascript_family_path(path: &str) -> bool {
    matches!(
        extension(std::path::Path::new(path)).as_str(),
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" | "vue" | "svelte" | "astro"
    )
}

fn resolution_case_matches(origin_file: &str, reference: &str, definition: &str) -> bool {
    if origin_file.is_empty() {
        return true;
    }
    let case_insensitive = matches!(
        extension(std::path::Path::new(origin_file)).as_str(),
        "php"
            | "phtml"
            | "php3"
            | "php4"
            | "php5"
            | "php7"
            | "phps"
            | "ps1"
            | "psm1"
            | "psd1"
            | "f"
            | "f90"
            | "f95"
            | "f03"
            | "f08"
            | "pas"
            | "pp"
            | "dpr"
            | "dpk"
            | "lpr"
    );
    if case_insensitive {
        reference.eq_ignore_ascii_case(definition)
    } else {
        reference == definition
    }
}

fn same_language_family(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return true;
    }
    let a = extension(std::path::Path::new(a));
    let b = extension(std::path::Path::new(b));
    if a.is_empty() || b.is_empty() {
        return true;
    }
    if a == b {
        return true;
    }
    let both_in = |family: &[&str]| family.contains(&a.as_str()) && family.contains(&b.as_str());
    if both_in(&[
        "js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts", "vue", "svelte", "astro",
    ]) || both_in(&["java", "kt", "kts", "scala", "groovy", "gradle"])
        || both_in(&[
            "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx", "cu", "cuh", "metal", "m", "mm",
        ])
        || both_in(&["py", "pyi"])
        || both_in(&["rb", "rake"])
        || both_in(&["php", "phtml", "php3", "php4", "php5", "php7", "phps"])
        || both_in(&["cs", "razor", "cshtml", "xaml"])
        || both_in(&["sh", "bash", "zsh", "dash", "ksh"])
        || both_in(&["ps1", "psm1", "psd1"])
        || both_in(&["f", "f90", "f95", "f03", "f08"])
        || both_in(&["pas", "pp", "dpr", "dpk", "lpr", "inc"])
        || both_in(&["ex", "exs"])
        || both_in(&["dm", "dme", "dmi", "dmf", "dmm"])
    {
        return true;
    }
    // Swift extensions deliberately fold onto Objective-C declarations from
    // bridging headers or implementation units.
    (a == "swift" && matches!(b.as_str(), "h" | "m" | "mm"))
        || (b == "swift" && matches!(a.as_str(), "h" | "m" | "mm"))
}

fn extension(path: &std::path::Path) -> String {
    path.extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn is_jvm_extension(extension: &str) -> bool {
    matches!(
        extension,
        "java" | "kt" | "kts" | "scala" | "groovy" | "gradle"
    )
}

fn disambiguate(extractions: &mut [Extraction]) {
    let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut file_ids = BTreeSet::new();
    for extraction in extractions.iter() {
        for node in &extraction.nodes {
            if node
                .extra
                .get(crate::csharp::NAMESPACE_NODE)
                .and_then(Value::as_bool)
                == Some(true)
            {
                continue;
            }
            // Swift import targets are corpus-wide anchors. Their stable ID is
            // intentionally shared by every importing file, so source-based
            // collision disambiguation would split one module into N phantom
            // nodes. Declared modules in other languages remain source-backed
            // definitions and still need normal collision partitioning.
            if node.extra.get("type").and_then(Value::as_str) == Some("module")
                && node.extra.get("swift_module").and_then(Value::as_bool) == Some(true)
            {
                continue;
            }
            if node.extra.get("type").and_then(|value| value.as_str()) == Some("file") {
                file_ids.insert(node.id.clone());
            }
            let source_key = if node.source_file.is_empty() {
                node.extra
                    .get("origin_file")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
            } else {
                &node.source_file
            };
            if source_key.is_empty() {
                continue;
            }
            groups
                .entry(node.id.clone())
                .or_default()
                .insert(source_key.to_owned());
        }
    }
    let ambiguous_sources: BTreeMap<_, _> = groups
        .into_iter()
        .filter(|(id, sources)| {
            sources.len() > 1 && (file_ids.contains(id) || !header_implementation_pair(sources))
        })
        .collect();
    let ambiguous: BTreeSet<_> = ambiguous_sources.keys().cloned().collect();
    if ambiguous.is_empty() {
        for edge in extractions
            .iter_mut()
            .flat_map(|extraction| &mut extraction.edges)
        {
            edge.extra.remove("target_file");
        }
        return;
    }
    let mut colliding_proposals: BTreeSet<(String, String)> = BTreeSet::new();
    for (id, sources) in &ambiguous_sources {
        let mut proposals: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for source in sources {
            proposals
                .entry(make_id(&[source, id]))
                .or_default()
                .push(source.clone());
        }
        for sources in proposals.into_values().filter(|sources| sources.len() > 1) {
            for source in sources {
                colliding_proposals.insert((id.clone(), source));
            }
        }
    }
    let mut remap_by_source = BTreeMap::<(String, String), String>::new();
    let mut local_remaps = Vec::with_capacity(extractions.len());
    for extraction in extractions.iter() {
        let mut local = BTreeMap::new();
        for node in &extraction.nodes {
            if ambiguous.contains(&node.id) {
                let old = node.id.clone();
                let source_key = if node.source_file.is_empty() {
                    node.extra
                        .get("origin_file")
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                } else {
                    &node.source_file
                };
                if source_key.is_empty() {
                    continue;
                }
                let proposed = make_id(&[source_key, &old]);
                let new = if colliding_proposals.contains(&(old.clone(), source_key.to_owned())) {
                    let digest = Sha256::digest(source_key.as_bytes());
                    let digest = format!("{digest:x}");
                    make_id(&[&proposed, &digest[..8]])
                } else {
                    proposed
                };
                remap_by_source.insert((old.clone(), source_key.to_owned()), new.clone());
                local.insert(old, new);
            }
        }
        local_remaps.push(local);
    }
    for (extraction, local) in extractions.iter_mut().zip(local_remaps) {
        for node in &mut extraction.nodes {
            if let Some(new) = local.get(&node.id) {
                node.id = new.clone();
            }
        }
        for edge in &mut extraction.edges {
            if let Some(new) = local.get(edge.true_source()) {
                edge.source = new.clone();
                edge.extra.insert("_src".into(), new.clone().into());
            }
            let target_source = edge
                .extra
                .remove("target_file")
                .and_then(|value| value.as_str().map(|source| source.replace('\\', "/")));
            let target = edge.true_target().to_owned();
            let remapped = target_source
                .as_ref()
                .and_then(|source| remap_by_source.get(&(target.clone(), source.clone())))
                .or_else(|| local.get(&target));
            if let Some(new) = remapped {
                edge.target = new.clone();
                edge.extra.insert("_tgt".into(), new.clone().into());
            }
        }
    }
}

fn header_implementation_pair(sources: &BTreeSet<String>) -> bool {
    let mut stems = BTreeSet::new();
    let mut header = false;
    let mut implementation = false;
    for source in sources {
        let path = std::path::Path::new(source);
        let mut stem = path.to_path_buf();
        stem.set_extension("");
        stems.insert(stem.to_string_lossy().replace('\\', "/"));
        match path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "h" | "hh" | "hpp" | "hxx" => header = true,
            "c" | "cc" | "cpp" | "cxx" | "m" | "mm" => implementation = true,
            _ => return false,
        }
    }
    stems.len() == 1 && header && implementation
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphoxide_core::Node;

    fn javascript_file(source_file: &str) -> Extraction {
        Extraction {
            nodes: vec![Node {
                id: make_id(&[&std::path::Path::new(source_file)
                    .with_extension("")
                    .to_string_lossy()]),
                label: source_file.into(),
                file_type: "code".into(),
                source_file: source_file.into(),
                source_location: Some("L1".into()),
                community: None,
                extra: BTreeMap::from([
                    ("_origin".into(), "ast".into()),
                    ("type".into(), "file".into()),
                ]),
            }],
            ..Extraction::default()
        }
    }

    #[test]
    fn empty_is_safe() {
        let mut values = [];
        resolve(&mut values);
    }

    #[test]
    fn byte_project_resolution_binds_bash_source_and_call_without_path_io() {
        let mut values = vec![
            crate::bash::extract_bash_bytes(
                std::path::Path::new("/graphoxide-missing-project/bash/runner.sh"),
                "bash/runner.sh",
                b"source ./lib.sh\nmain() { worker_run; }\n",
            )
            .expect("extract runner bytes"),
            crate::bash::extract_bash_bytes(
                std::path::Path::new("/graphoxide-missing-project/bash/lib.sh"),
                "bash/lib.sh",
                b"worker_run() { :; }\n",
            )
            .expect("extract library bytes"),
        ];
        resolve(&mut values);
        let edges = values
            .iter()
            .flat_map(|extraction| &extraction.edges)
            .collect::<Vec<_>>();
        assert!(edges.iter().any(|edge| {
            edge.relation == "imports_from"
                && edge.true_source() == "bash_runner"
                && edge.true_target() == "bash_lib"
        }));
        assert!(edges.iter().any(|edge| {
            edge.relation == "calls"
                && edge.true_source() == "bash_runner_main"
                && edge.true_target() == "bash_lib_worker_run"
        }));
    }

    #[test]
    fn byte_project_resolution_rejects_unbacked_bash_source_expansions() {
        let mut values = vec![
            crate::bash::extract_bash_bytes(
                std::path::Path::new("/graphoxide-missing-project/scripts/runner.sh"),
                "scripts/runner.sh",
                b"ROOT=.\nNAME=.\nsource \"$UNSET/lib.sh\"\nsource \"$ROOT$NAME/lib.sh\"\nsource \"${ROOT}${NAME}/lib.sh\"\nmain() { worker_run; }\n",
            )
            .expect("extract unsafe runner expansions"),
            crate::bash::extract_bash_bytes(
                std::path::Path::new("/graphoxide-missing-project/scripts/lib.sh"),
                "scripts/lib.sh",
                b"worker_run() { :; }\n",
            )
            .expect("extract independently admitted library bytes"),
        ];
        resolve(&mut values);
        let edges = values
            .iter()
            .flat_map(|extraction| &extraction.edges)
            .collect::<Vec<_>>();
        assert!(!edges.iter().any(|edge| {
            edge.relation == "imports_from" && edge.true_target() == "scripts_lib"
        }));
        assert!(!edges.iter().any(|edge| {
            edge.relation == "calls"
                && edge.true_source() == "scripts_runner_main"
                && edge.true_target() == "scripts_lib_worker_run"
        }));
    }

    #[test]
    fn byte_project_resolution_rejects_dynamic_powershell_import_bindings() {
        let extract = |path: &str, source_file: &str, source: &[u8]| {
            crate::engine::extract_as_bytes(std::path::Path::new(path), source_file, source)
                .expect("extract admitted PowerShell bytes")
        };
        let mut values = vec![
            extract(
                "/graphoxide-missing-project/consumer.ps1",
                "consumer.ps1",
                br#"Import-Module $Module
Import-Module "$Root/module.psm1"
Import-Module "${Module}"
Import-Module "$(Get-ModuleName)"
Import-Module "$Root$Name/module.psm1"
. $Root/script.ps1
. "${Root}/script.ps1"
"#,
            ),
            extract(
                "/graphoxide-missing-project/module.psm1",
                "module.psm1",
                b"\nfunction Invoke-ModuleThing { }\n",
            ),
            extract(
                "/graphoxide-missing-project/script.ps1",
                "script.ps1",
                b"\nfunction Invoke-ScriptThing { }\n",
            ),
        ];
        resolve(&mut values);
        assert!(values.iter().flat_map(|value| &value.edges).all(|edge| {
            edge.relation != "imports_from" || !matches!(edge.true_target(), "module" | "script")
        }));

        let mut literals = vec![
            extract(
                "/graphoxide-missing-project/literal.ps1",
                "literal.ps1",
                b"Import-Module .\\module.psm1\n. ./script.ps1\n",
            ),
            extract(
                "/graphoxide-missing-project/module.psm1",
                "module.psm1",
                b"\nfunction Invoke-ModuleThing { }\n",
            ),
            extract(
                "/graphoxide-missing-project/script.ps1",
                "script.ps1",
                b"\nfunction Invoke-ScriptThing { }\n",
            ),
        ];
        let target_files = literals
            .iter()
            .flat_map(|value| &value.edges)
            .filter(|edge| edge.relation == "imports_from")
            .filter_map(|edge| edge.extra.get("target_file")?.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(target_files, BTreeSet::from(["module.psm1", "script.ps1"]));
        resolve(&mut literals);
        let targets = literals
            .iter()
            .flat_map(|value| &value.edges)
            .filter(|edge| edge.relation == "imports_from")
            .map(|edge| edge.true_target().to_owned())
            .collect::<BTreeSet<_>>();
        let admitted_file_targets = literals
            .iter()
            .flat_map(|value| &value.nodes)
            .filter(|node| {
                matches!(node.source_file.as_str(), "module.psm1" | "script.ps1")
                    && node.extra.get("type").and_then(Value::as_str) == Some("file")
            })
            .map(|node| node.id.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(targets, admitted_file_targets);
    }

    #[test]
    fn byte_project_resolution_rejects_unsafe_dart_uri_collisions() {
        let dart = |source_file: &str, source: &[u8]| {
            crate::dart::extract_dart_bytes(
                std::path::Path::new("/graphoxide-missing-project")
                    .join(source_file)
                    .as_path(),
                source_file,
                source,
            )
            .expect("extract admitted Dart bytes")
        };
        let mut values = vec![
            dart(
                "lib/parts/dynamic.dart",
                b"part of '${page}.dart';\nclass DynamicChild {}\n",
            ),
            dart(
                "lib/parts/escape.dart",
                b"part of '../../../page.dart';\nclass EscapeChild {}\n",
            ),
            dart(
                "lib/consumer.dart",
                b"import '${page}.dart';\nexport '$target.dart';\nclass Consumer {}\n",
            ),
            dart(
                "lib/static_consumer.dart",
                b"import '../page.dart';\nclass StaticConsumer {}\n",
            ),
            dart("page.dart", b"class RootPage {}\n"),
            dart("lib/page.dart", b"class Page {}\n"),
            dart(
                "lib/parts/static.dart",
                b"part of '.././page.dart';\nclass StaticChild {}\n",
            ),
        ];
        resolve(&mut values);

        let nodes = values
            .iter()
            .flat_map(|extraction| &extraction.nodes)
            .collect::<Vec<_>>();
        assert!(nodes.iter().any(|node| {
            node.source_file == "lib/parts/dynamic.dart"
                && node.id == "lib_parts_dynamic_dynamicchild"
        }));
        assert!(nodes.iter().any(|node| {
            node.source_file == "lib/parts/escape.dart" && node.id == "lib_parts_escape_escapechild"
        }));
        assert!(nodes.iter().any(|node| {
            node.source_file == "lib/parts/static.dart" && node.id == "lib_page_staticchild"
        }));
        assert!(values.iter().flat_map(|value| &value.edges).all(|edge| {
            edge.source_file != "lib/consumer.dart"
                || !matches!(edge.relation.as_str(), "imports" | "exports")
        }));
        let static_imports = values
            .iter()
            .flat_map(|value| &value.edges)
            .filter(|edge| {
                edge.source_file == "lib/static_consumer.dart" && edge.relation == "imports"
            })
            .map(|edge| edge.true_target())
            .collect::<BTreeSet<_>>();
        assert_eq!(static_imports, BTreeSet::from(["page"]));
        assert!(!static_imports.contains("lib_page"));
    }

    #[test]
    fn byte_project_resolution_rejects_dynamic_markdown_link_collisions() {
        let markdown = |source_file: &str, source: &[u8]| {
            crate::fallback::extract_text_bytes(
                std::path::Path::new("/graphoxide-missing-project")
                    .join(source_file)
                    .as_path(),
                source_file,
                source,
            )
            .expect("extract admitted Markdown bytes")
        };
        let mut values = vec![
            markdown(
                "docs/index.md",
                br#"[inline](${page}.md)
[[{{page}}]]
[reference]: $page.md
[escape](../../page.md)
[absolute](/docs/page.md)
[drive](C:docs/page.md)
[unc](//server/share/page.md)
"#,
            ),
            markdown("docs/page.md", b"# Docs page\n"),
            markdown("page.md", b"# Root page\n"),
            markdown("docs/nested/static.md", b"[static](../page.md#section)\n"),
        ];
        resolve(&mut values);
        assert!(values
            .iter()
            .flat_map(|value| &value.edges)
            .all(|edge| { edge.source_file != "docs/index.md" || edge.relation != "references" }));
        let static_targets = values
            .iter()
            .flat_map(|value| &value.edges)
            .filter(|edge| {
                edge.source_file == "docs/nested/static.md" && edge.relation == "references"
            })
            .map(|edge| edge.true_target())
            .collect::<BTreeSet<_>>();
        assert_eq!(static_targets, BTreeSet::from(["docs_page"]));
        assert!(!static_targets.contains("page"));
    }

    #[test]
    fn byte_project_resolution_rejects_unquoted_generic_call_import_collisions() {
        let generic = |source_file: &str, source: &[u8]| {
            crate::fallback::extract_text_bytes(
                std::path::Path::new("/graphoxide-missing-project")
                    .join(source_file)
                    .as_path(),
                source_file,
                source,
            )
            .expect("extract admitted generic bytes")
        };
        let mut values = vec![
            generic(
                "consumer.lua",
                b"require(module_name)\ninclude(module_name)\nrequire(get_module())\n",
            ),
            generic("module_name.lua", b"function module_name() end\n"),
        ];
        resolve(&mut values);
        assert!(values
            .iter()
            .flat_map(|value| &value.edges)
            .all(|edge| edge.source_file != "consumer.lua" || edge.relation != "imports"));
    }

    #[test]
    fn generic_relative_import_binds_exact_path_and_rejects_unsafe_collisions() {
        let generic = |source_file: &str, source: &[u8]| {
            crate::fallback::extract_text_bytes(
                std::path::Path::new("/graphoxide-missing-project")
                    .join(source_file)
                    .as_path(),
                source_file,
                source,
            )
            .expect("extract admitted generic bytes")
        };
        let mut values = vec![
            generic("src/lib/worker.lua", b"function selected() end\n"),
            generic(
                "src/deep/consumer.lua",
                br#"include "../lib/worker.lua"
include "../../../victim.lua"
include "./aux:worker.lua"
"#,
            ),
            generic("other/worker.lua", b"function decoy() end\n"),
            generic("victim.lua", b"function victim() end\n"),
            generic("src/deep/aux_worker.lua", b"function collision() end\n"),
        ];

        resolve(&mut values);

        let file_id = |source_file: &str| {
            values
                .iter()
                .flat_map(|extraction| &extraction.nodes)
                .find(|node| {
                    node.source_file == source_file
                        && node.extra.get("type").and_then(Value::as_str) == Some("file")
                })
                .map(|node| node.id.clone())
                .unwrap_or_else(|| panic!("missing admitted file node for {source_file}"))
        };
        let selected = file_id("src/lib/worker.lua");
        let decoy = file_id("other/worker.lua");
        let victim = file_id("victim.lua");
        let normalized_collision = file_id("src/deep/aux_worker.lua");
        let imports = values
            .iter()
            .flat_map(|extraction| &extraction.edges)
            .filter(|edge| {
                edge.source_file == "src/deep/consumer.lua" && edge.relation == "imports"
            })
            .collect::<Vec<_>>();

        assert_eq!(imports.len(), 1, "unsafe static paths emitted import facts");
        assert_eq!(imports[0].true_target(), selected);
        assert!(![decoy, victim, normalized_collision]
            .iter()
            .any(|id| imports[0].true_target() == id));
        let selected_nodes = values
            .iter()
            .flat_map(|extraction| &extraction.nodes)
            .filter(|node| node.id == selected)
            .collect::<Vec<_>>();
        assert_eq!(
            selected_nodes.len(),
            1,
            "exact file retained a duplicate stub"
        );
        assert_eq!(selected_nodes[0].source_file, "src/lib/worker.lua");
        assert_eq!(
            selected_nodes[0].extra.get("type").and_then(Value::as_str),
            Some("file")
        );
        assert_ne!(
            selected_nodes[0]
                .extra
                .get(crate::project_path::EXACT_PROJECT_RELATIVE_PLACEHOLDER)
                .and_then(Value::as_bool),
            Some(true),
        );
    }

    #[test]
    fn byte_project_resolution_binds_objective_c_header_without_path_io() {
        let mut values = vec![
            crate::compat::extract_objc_bytes(
                std::path::Path::new(
                    "/graphoxide-missing-project/native/LegacyWorker.m",
                ),
                "#import \"LegacyWorker.h\"\n@implementation LegacyWorker\n- (id)process:(id)value { return value; }\n@end\n",
                "native/LegacyWorker.m",
            )
            .expect("extract implementation bytes"),
            crate::compat::extract_objc_bytes(
                std::path::Path::new(
                    "/graphoxide-missing-project/native/LegacyWorker.h",
                ),
                "#import <Foundation/Foundation.h>\n@interface LegacyWorker : NSObject\n- (id)process:(id)value;\n@end\n",
                "native/LegacyWorker.h",
            )
            .expect("extract header bytes"),
        ];
        resolve(&mut values);
        let edges = values
            .iter()
            .flat_map(|extraction| &extraction.edges)
            .collect::<Vec<_>>();
        assert!(edges.iter().any(|edge| {
            edge.relation == "imports"
                && edge.true_source() == "native_legacyworker_m_native_legacyworker"
                && edge.true_target() == "native_legacyworker_h_native_legacyworker"
        }));
        assert!(edges.iter().any(|edge| {
            edge.relation == "method"
                && edge.true_target() == "native_legacyworker_legacyworker_process"
        }));
        assert!(edges.iter().any(|edge| {
            edge.relation == "inherits"
                && edge.true_source() == "native_legacyworker_legacyworker"
                && edge.true_target().ends_with("nsobject")
        }));
    }

    #[test]
    fn bounded_snapshot_resolution_rejects_fact_growth_before_append() {
        let mut snapshot = crate::js_resolution::ProjectSnapshot::with_byte_limit(1024);
        snapshot
            .insert_owned("a.ts".into(), b"import './b';".to_vec())
            .expect("admit importer");
        snapshot
            .insert_owned("b.ts".into(), b"export const value = 1;".to_vec())
            .expect("admit dependency");
        let original = vec![javascript_file("a.ts"), javascript_file("b.ts")];
        let retained = crate::extractions_retained_bytes(&original).expect("measure fixture");

        let mut constrained = original.clone();
        let error =
            resolve_with_snapshot_bounded(&mut constrained, &snapshot, retained, 64 * 1024 * 1024)
                .expect_err("new import facts require separately admitted output memory");
        assert!(error.to_string().contains("corpus resolver javascript"));

        // An error must remove the thread-local admission guard so a later
        // independent resolution on the same worker can proceed.
        let mut admitted = original;
        resolve_with_snapshot_bounded(
            &mut admitted,
            &snapshot,
            retained.saturating_add(1024 * 1024),
            64 * 1024 * 1024,
        )
        .expect("sufficient output budget");
        assert!(admitted[0]
            .edges
            .iter()
            .any(|edge| edge.relation == "imports_from"));
    }

    #[test]
    fn real_interop_suffixes_share_a_resolution_family() {
        for (left, right) in [
            ("api/model.py", "api/model.pyi"),
            ("gpu/kernel.cu", "native/kernel.cpp"),
            ("gpu/kernel.cuh", "native/kernel.cc"),
            ("gpu/shader.metal", "native/shader.hpp"),
            ("web/App.vue", "web/state.ts"),
            ("web/App.svelte", "web/state.js"),
            ("web/App.astro", "web/state.tsx"),
            ("tasks/build.rake", "lib/build.rb"),
            ("views/page.phtml", "src/page.php"),
            ("pascal/base.pp", "pascal/derived.pas"),
        ] {
            assert!(
                same_language_family(left, right),
                "{left} and {right} should interoperate"
            );
        }
    }

    #[test]
    fn unrelated_runtime_suffixes_do_not_share_a_resolution_family() {
        for (left, right) in [
            ("backend/model.py", "frontend/model.ts"),
            ("native/base.cpp", "services/base.cs"),
            ("scripts/logger.ps1", "src/logger.rs"),
            ("math/geometry.jl", "math/geometry.f90"),
        ] {
            assert!(
                !same_language_family(left, right),
                "{left} and {right} must remain partitioned"
            );
        }
    }

    #[test]
    fn language_suffix_checks_are_case_insensitive() {
        assert!(is_python("stubs/Model.PYI"));
        assert!(same_go_package("pkg/a.GO", "pkg/b.go"));
        assert!(same_language_family("src/a.TSX", "src/b.Js"));
    }

    // ── IDL import resolution ────────────────────────────────────────────

    fn proto_file_node(source_file: &str) -> (String, graphoxide_core::Node) {
        let stem = source_file
            .rsplit_once('.')
            .map(|(s, _)| s)
            .unwrap_or(source_file);
        let id = graphoxide_core::make_id(&[stem]);
        let node = graphoxide_core::Node {
            id: id.clone(),
            label: source_file.to_owned(),
            file_type: "code".into(),
            source_file: source_file.to_owned(),
            source_location: None,
            community: None,
            extra: std::collections::BTreeMap::from([
                ("type".into(), "protocol_file".into()),
                ("_origin".into(), "protocols".into()),
            ]),
        };
        (id, node)
    }

    fn synthetic_imports_edge(source: &str, target_name: &str) -> graphoxide_core::Edge {
        let target = format!("protocol_reference_{}", target_name);
        graphoxide_core::Edge {
            source: source.to_owned(),
            target,
            relation: "imports".into(),
            confidence: graphoxide_core::Confidence::Extracted,
            source_file: source.to_owned(),
            extra: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn idl_imports_resolve_to_real_protocol_file_nodes() {
        let (common_id, common_node) = proto_file_node("common.proto");
        let (main_id, main_node) = proto_file_node("main.proto");

        let common = graphoxide_core::Extraction {
            nodes: vec![common_node],
            edges: Vec::new(),
            hyperedges: Vec::new(),
        };
        let main = graphoxide_core::Extraction {
            nodes: vec![main_node],
            edges: vec![synthetic_imports_edge(&main_id, "common.proto")],
            hyperedges: Vec::new(),
        };

        let mut extractions = vec![common, main];
        resolve_idl_imports(&mut extractions);

        // The imports edge should now point at the real common.proto node.
        let edge = &extractions[1].edges[0];
        assert_eq!(edge.target, common_id);
        assert_eq!(
            edge.extra.get("resolved").unwrap(),
            &serde_json::Value::from(true)
        );
        assert!(!edge.extra.contains_key("unresolved"));
    }

    #[test]
    fn idl_imports_unresolved_when_target_not_in_corpus() {
        let (main_id, main_node) = proto_file_node("main.proto");

        let main = graphoxide_core::Extraction {
            nodes: vec![main_node],
            edges: vec![synthetic_imports_edge(&main_id, "missing.proto")],
            hyperedges: Vec::new(),
        };

        let mut extractions = vec![main];
        resolve_idl_imports(&mut extractions);

        let edge = &extractions[0].edges[0];
        assert_eq!(
            edge.extra.get("unresolved").unwrap(),
            &serde_json::Value::from(true)
        );
        assert!(!edge.extra.contains_key("resolved"));
    }

    #[test]
    fn idl_imports_ambiguous_basename_keeps_unresolved() {
        // Two files with the same basename "types.proto" in different dirs.
        let (a_id, a_node) = proto_file_node("a/types.proto");
        let (b_id, b_node) = proto_file_node("b/types.proto");
        let (main_id, main_node) = proto_file_node("main.proto");

        let a = graphoxide_core::Extraction {
            nodes: vec![a_node],
            edges: Vec::new(),
            hyperedges: Vec::new(),
        };
        let b = graphoxide_core::Extraction {
            nodes: vec![b_node],
            edges: Vec::new(),
            hyperedges: Vec::new(),
        };
        let main = graphoxide_core::Extraction {
            nodes: vec![main_node],
            edges: vec![synthetic_imports_edge(&main_id, "types.proto")],
            hyperedges: Vec::new(),
        };

        let _ = (a_id, b_id);
        let mut extractions = vec![a, b, main];
        resolve_idl_imports(&mut extractions);

        // Ambiguous: two files named types.proto -> unresolved.
        let edge = &extractions[2].edges[0];
        assert_eq!(
            edge.extra.get("unresolved").unwrap(),
            &serde_json::Value::from(true)
        );
    }

    #[test]
    fn idl_imports_exact_path_match_wins_over_basename() {
        let (common_id, common_node) = proto_file_node("vendor/common.proto");
        let (local_id, local_node) = proto_file_node("local/common.proto");
        let (main_id, main_node) = proto_file_node("main.proto");

        let vendor = graphoxide_core::Extraction {
            nodes: vec![common_node],
            edges: Vec::new(),
            hyperedges: Vec::new(),
        };
        let local = graphoxide_core::Extraction {
            nodes: vec![local_node],
            edges: Vec::new(),
            hyperedges: Vec::new(),
        };
        let main = graphoxide_core::Extraction {
            nodes: vec![main_node],
            // Import by full path -> should resolve to vendor/common.proto.
            edges: vec![synthetic_imports_edge(&main_id, "vendor/common.proto")],
            hyperedges: Vec::new(),
        };

        let _ = local_id;
        let mut extractions = vec![vendor, local, main];
        resolve_idl_imports(&mut extractions);

        let edge = &extractions[2].edges[0];
        assert_eq!(edge.target, common_id);
        assert_eq!(
            edge.extra.get("resolved").unwrap(),
            &serde_json::Value::from(true)
        );
    }
}
