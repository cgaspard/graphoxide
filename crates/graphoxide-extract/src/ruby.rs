//! Ruby-specific extraction facts and conservative corpus resolution.
//!
//! Ruby's call grammar keeps the receiver on the call node, and constants are
//! resolved through lexical namespaces rather than import aliases.  Keeping
//! those rules here prevents the language-neutral resolver from turning a
//! same-named method into a false call edge.

use crate::project_path::{source_relative_project_path, ProjectPath};
use graphoxide_core::{make_id, normalize_id, Confidence, Extraction};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tree_sitter::Node as TsNode;

pub(crate) const CALL_FACT: &str = "ruby_call";
pub(crate) const CONSTANT_RECEIVER: &str = "ruby_constant_receiver";
pub(crate) const DECLARATION_KIND: &str = "ruby_kind";
pub(crate) const SINGLETON_METHOD: &str = "ruby_singleton";
pub(crate) const RAW_MIXIN_RELATION: &str = "__ruby_mixin";
pub(crate) const MIXIN_NAME: &str = "ruby_mixin_name";
pub(crate) const MIXIN_KIND: &str = "ruby_mixin_kind";

#[derive(Debug)]
pub(crate) struct ReceiverFact {
    pub receiver: String,
    pub receiver_type: Option<String>,
    pub constant: bool,
}

#[derive(Debug)]
pub(crate) struct DynamicTypeFact<'tree> {
    pub name: String,
    pub kind: &'static str,
    pub superclass: Option<String>,
    pub block: Option<TsNode<'tree>>,
}

fn text(node: TsNode<'_>, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or("").trim().to_owned()
}

fn ruby_scope_boundary(kind: &str) -> bool {
    matches!(kind, "method" | "singleton_method" | "class" | "module")
}

fn constructor_type(call: TsNode<'_>, source: &[u8]) -> Option<String> {
    if call.kind() != "call" {
        return None;
    }
    let receiver = call.child_by_field_name("receiver")?;
    if !matches!(receiver.kind(), "constant" | "scope_resolution") {
        return None;
    }
    let method = call.child_by_field_name("method")?;
    (text(method, source) == "new").then(|| text(receiver, source))
}

fn collect_local_binding_types(
    node: TsNode<'_>,
    variable: &str,
    source: &[u8],
    before_byte: usize,
    root: bool,
    types: &mut BTreeSet<String>,
) {
    if !root && ruby_scope_boundary(node.kind()) {
        return;
    }
    if node.start_byte() >= before_byte {
        return;
    }
    if node.kind() == "assignment"
        // An assignment is usable only after its right-hand side has been
        // evaluated.  This also excludes an assignment that contains the
        // receiver call itself, such as `service = service.process`.
        && node.end_byte() <= before_byte
        && node
            .child_by_field_name("left")
            .is_some_and(|left| left.kind() == "identifier" && text(left, source) == variable)
    {
        if let Some(kind) = node
            .child_by_field_name("right")
            .and_then(|right| constructor_type(right, source))
        {
            types.insert(kind);
        } else {
            // A non-constructor reassignment destroys certainty too.  The
            // sentinel can never be mistaken for a class constant.
            types.insert("\0unknown".into());
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_local_binding_types(child, variable, source, before_byte, false, types);
    }
}

pub(crate) fn receiver_fact(call: TsNode<'_>, source: &[u8]) -> Option<ReceiverFact> {
    let receiver = call.child_by_field_name("receiver")?;
    let receiver_text = text(receiver, source);
    let constant = matches!(receiver.kind(), "constant" | "scope_resolution");
    let receiver_type = if receiver.kind() == "identifier" {
        let mut ancestor = call.parent();
        let mut scope = None;
        while let Some(candidate) = ancestor {
            if matches!(candidate.kind(), "method" | "singleton_method") {
                scope = Some(candidate);
                break;
            }
            ancestor = candidate.parent();
        }
        scope.and_then(|scope| {
            let mut types = BTreeSet::new();
            collect_local_binding_types(
                scope,
                &receiver_text,
                source,
                call.start_byte(),
                true,
                &mut types,
            );
            (types.len() == 1)
                .then(|| types.into_iter().next().unwrap())
                .filter(|kind| kind != "\0unknown")
        })
    } else {
        None
    };
    Some(ReceiverFact {
        receiver: receiver_text,
        receiver_type,
        constant,
    })
}

pub(crate) fn qualify_declaration(name: &str, parent_label: Option<&str>) -> String {
    let name = name.trim();
    match parent_label {
        Some(parent) if !name.contains("::") => format!("{parent}::{name}"),
        _ => name.to_owned(),
    }
}

pub(crate) fn dynamic_type<'tree>(
    node: TsNode<'tree>,
    source: &[u8],
) -> Option<DynamicTypeFact<'tree>> {
    if node.kind() != "assignment" {
        return None;
    }
    let left = node.child_by_field_name("left")?;
    if !matches!(left.kind(), "constant" | "scope_resolution") {
        return None;
    }
    let right = node.child_by_field_name("right")?;
    if right.kind() != "call" {
        return None;
    }
    let receiver = right.child_by_field_name("receiver")?;
    let method = right.child_by_field_name("method")?;
    let receiver = text(receiver, source);
    let method = text(method, source);
    let (kind, superclass) = match (receiver.as_str(), method.as_str()) {
        ("Struct", "new") => ("struct", None),
        ("Class", "new") => {
            let superclass = right
                .child_by_field_name("arguments")
                .and_then(|arguments| arguments.named_child(0))
                .filter(|argument| matches!(argument.kind(), "constant" | "scope_resolution"))
                .map(|argument| text(argument, source));
            ("class", superclass)
        }
        ("Data", "define") => ("data", None),
        _ => return None,
    };
    let block = right.child_by_field_name("block").or_else(|| {
        let mut cursor = right.walk();

        right
            .named_children(&mut cursor)
            .find(|child| matches!(child.kind(), "do_block" | "block"))
    });
    Some(DynamicTypeFact {
        name: text(left, source),
        kind,
        superclass,
        block,
    })
}

pub(crate) fn method_is_singleton(node: TsNode<'_>, source: &[u8]) -> bool {
    if node.kind() == "singleton_method" {
        return true;
    }
    let mut sibling = node.prev_named_sibling();
    while let Some(previous) = sibling {
        if previous.kind() == "identifier" && text(previous, source) == "module_function" {
            return true;
        }
        sibling = previous.prev_named_sibling();
    }
    false
}

pub(crate) fn mixin_names(call: TsNode<'_>, source: &[u8]) -> Option<Vec<String>> {
    if call.kind() != "call" || call.child_by_field_name("receiver").is_some() {
        return None;
    }
    let method = call.child_by_field_name("method")?;
    if !matches!(
        text(method, source).as_str(),
        "include" | "extend" | "prepend"
    ) {
        return None;
    }
    let arguments = call.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    let names = arguments
        .named_children(&mut cursor)
        .filter(|argument| matches!(argument.kind(), "constant" | "scope_resolution"))
        .map(|argument| text(argument, source))
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    Some(names)
}

pub(crate) fn mixin_kind(call: TsNode<'_>, source: &[u8]) -> Option<&'static str> {
    if call.kind() != "call" || call.child_by_field_name("receiver").is_some() {
        return None;
    }
    let method = call.child_by_field_name("method")?;
    match text(method, source).as_str() {
        "include" => Some("include"),
        "extend" => Some("extend"),
        "prepend" => Some("prepend"),
        _ => None,
    }
}

pub(crate) fn require_relative(call: TsNode<'_>, source: &[u8]) -> Option<String> {
    if call.kind() != "call" || call.child_by_field_name("receiver").is_some() {
        return None;
    }
    let method = call.child_by_field_name("method")?;
    if text(method, source) != "require_relative" {
        return None;
    }
    let argument = call
        .child_by_field_name("arguments")?
        .named_child(0)
        .filter(|argument| argument.kind() == "string")?;
    if argument.has_error() {
        return None;
    }
    let mut cursor = argument.walk();
    if argument
        .named_children(&mut cursor)
        .any(|child| child.kind() == "interpolation")
    {
        return None;
    }
    let module = text(argument, source)
        .trim_matches(['\'', '"'])
        .trim()
        .to_owned();
    (!module.is_empty()).then_some(module)
}

fn is_ruby(path: &str) -> bool {
    matches!(
        Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("rb" | "rake")
    )
}

#[derive(Clone)]
struct RubyType {
    id: String,
    label: String,
    kind: String,
}

#[derive(Clone)]
struct RubyMethod {
    id: String,
    name: String,
    singleton: bool,
}

fn short_constant(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

fn unique_type<'a>(
    name: &str,
    exact: &'a BTreeMap<String, Vec<RubyType>>,
    short: &'a BTreeMap<String, Vec<RubyType>>,
) -> Option<&'a RubyType> {
    let key = normalize_id(name);
    let candidates = if name.contains("::") {
        exact.get(&key)
    } else {
        exact
            .get(&key)
            .filter(|candidates| candidates.len() == 1)
            .or_else(|| short.get(&key))
    }?;
    (candidates.len() == 1).then(|| &candidates[0])
}

fn lexical_type<'a>(
    name: &str,
    owner: &str,
    exact: &'a BTreeMap<String, Vec<RubyType>>,
    short: &'a BTreeMap<String, Vec<RubyType>>,
) -> Option<&'a RubyType> {
    if name.contains("::") {
        return unique_type(name, exact, short);
    }
    let mut namespace = owner.split("::").collect::<Vec<_>>();
    namespace.pop();
    while !namespace.is_empty() {
        let qualified = format!("{}::{name}", namespace.join("::"));
        if let Some(candidates) = exact.get(&normalize_id(&qualified)) {
            return (candidates.len() == 1).then(|| &candidates[0]);
        }
        namespace.pop();
    }
    unique_type(name, exact, short)
}

fn lexical_mixin<'a>(
    name: &str,
    owner: &str,
    exact: &'a BTreeMap<String, Vec<RubyType>>,
    short: &'a BTreeMap<String, Vec<RubyType>>,
) -> Option<&'a RubyType> {
    lexical_type(name, owner, exact, short).filter(|candidate| candidate.kind == "module")
}

/// Resolve Ruby receiver and mixin facts after the complete corpus is known.
/// Every successful edge is backed by one unique type and (where applicable)
/// one method on that type; ambiguous or external mixins are discarded.
pub(crate) fn resolve(extractions: &mut [Extraction]) {
    let mut types_by_id = BTreeMap::<String, RubyType>::new();
    for extraction in extractions.iter() {
        for node in &extraction.nodes {
            if !is_ruby(&node.source_file) {
                continue;
            }
            let Some(kind) = node
                .extra
                .get(DECLARATION_KIND)
                .and_then(|value| value.as_str())
            else {
                continue;
            };
            types_by_id.insert(
                node.id.clone(),
                RubyType {
                    id: node.id.clone(),
                    label: node.label.clone(),
                    kind: kind.to_owned(),
                },
            );
        }
    }

    let mut exact = BTreeMap::<String, Vec<RubyType>>::new();
    let mut short = BTreeMap::<String, Vec<RubyType>>::new();
    for ruby_type in types_by_id.values() {
        exact
            .entry(normalize_id(&ruby_type.label))
            .or_default()
            .push(ruby_type.clone());
        short
            .entry(normalize_id(short_constant(&ruby_type.label)))
            .or_default()
            .push(ruby_type.clone());
    }
    for candidates in exact.values_mut().chain(short.values_mut()) {
        candidates.sort_by(|left, right| left.id.cmp(&right.id));
        candidates.dedup_by(|left, right| left.id == right.id);
    }

    let singleton_by_id = extractions
        .iter()
        .flat_map(|extraction| &extraction.nodes)
        .map(|node| {
            (
                node.id.clone(),
                node.extra
                    .get(SINGLETON_METHOD)
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let labels_by_id = extractions
        .iter()
        .flat_map(|extraction| &extraction.nodes)
        .map(|node| (node.id.clone(), node.label.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut methods_by_owner = BTreeMap::<String, Vec<RubyMethod>>::new();
    let mut method_owner_by_id = BTreeMap::<String, String>::new();
    for extraction in extractions.iter() {
        for edge in &extraction.edges {
            if edge.relation != "method" || !is_ruby(&edge.source_file) {
                continue;
            }
            let Some(label) = labels_by_id.get(edge.true_target()) else {
                continue;
            };
            methods_by_owner
                .entry(edge.true_source().to_owned())
                .or_default()
                .push(RubyMethod {
                    id: edge.true_target().to_owned(),
                    name: normalize_id(label.trim_start_matches('.').trim_end_matches("()")),
                    singleton: singleton_by_id
                        .get(edge.true_target())
                        .copied()
                        .unwrap_or(false),
                });
            method_owner_by_id.insert(edge.true_target().to_owned(), edge.true_source().to_owned());
        }
    }

    // `include` and `prepend` add instance methods to the receiving class.
    // Keep this lookup separate from the public mixes_in edge so `extend`
    // cannot accidentally justify an instance call.
    let mut instance_mixins_by_owner = BTreeMap::<String, BTreeSet<String>>::new();
    for extraction in extractions.iter() {
        for edge in &extraction.edges {
            if edge.relation != RAW_MIXIN_RELATION
                || !matches!(
                    edge.extra.get(MIXIN_KIND).and_then(|value| value.as_str()),
                    Some("include" | "prepend")
                )
            {
                continue;
            }
            let Some(name) = edge.extra.get(MIXIN_NAME).and_then(|value| value.as_str()) else {
                continue;
            };
            let owner = types_by_id
                .get(edge.true_source())
                .map(|ruby_type| ruby_type.label.as_str())
                .unwrap_or("");
            let Some(target) = lexical_mixin(name, owner, &exact, &short) else {
                continue;
            };
            instance_mixins_by_owner
                .entry(edge.true_source().to_owned())
                .or_default()
                .insert(target.id.clone());
        }
    }

    for extraction in extractions {
        extraction.edges.retain_mut(|edge| {
            if edge.relation == "inherits" && is_ruby(&edge.source_file) {
                let unresolved = !types_by_id.contains_key(edge.true_target());
                if unresolved {
                    let owner = types_by_id
                        .get(edge.true_source())
                        .map(|ruby_type| ruby_type.label.as_str())
                        .unwrap_or("");
                    let name = labels_by_id
                        .get(edge.true_target())
                        .map(String::as_str)
                        .unwrap_or("");
                    if let Some(target) = lexical_type(name, owner, &exact, &short)
                        .filter(|candidate| candidate.kind != "module")
                    {
                        edge.target = target.id.clone();
                        edge.extra.insert("_tgt".into(), target.id.clone().into());
                    }
                }
                return true;
            }
            if edge.relation == RAW_MIXIN_RELATION {
                let Some(name) = edge.extra.get(MIXIN_NAME).and_then(|value| value.as_str()) else {
                    return false;
                };
                let owner = types_by_id
                    .get(edge.true_source())
                    .map(|ruby_type| ruby_type.label.as_str())
                    .unwrap_or("");
                let Some(target) = lexical_mixin(name, owner, &exact, &short) else {
                    return false;
                };
                edge.target = target.id.clone();
                edge.relation = "mixes_in".into();
                edge.confidence = Confidence::Extracted;
                edge.extra.insert("_tgt".into(), target.id.clone().into());
                edge.extra.remove(MIXIN_NAME);
                edge.extra.remove(MIXIN_KIND);
                return true;
            }

            if edge.extra.get(CALL_FACT).and_then(|value| value.as_bool()) != Some(true)
                || edge
                    .extra
                    .get("unresolved_call")
                    .and_then(|value| value.as_bool())
                    != Some(true)
            {
                return true;
            }
            let callee = edge
                .extra
                .get("callee")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let receiver = edge
                .extra
                .get("receiver")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let receiver_type = edge
                .extra
                .get("receiver_type")
                .and_then(|value| value.as_str());
            let constant_receiver = edge
                .extra
                .get(CONSTANT_RECEIVER)
                .and_then(|value| value.as_bool())
                .unwrap_or(false);

            let normalized_callee = normalize_id(callee);
            let resolved = if let Some(receiver_type) = receiver_type {
                unique_type(receiver_type, &exact, &short).and_then(|ruby_type| {
                    let candidates = methods_by_owner
                        .get(&ruby_type.id)
                        .into_iter()
                        .flatten()
                        .filter(|method| !method.singleton && method.name == normalized_callee)
                        .collect::<Vec<_>>();
                    (candidates.len() == 1).then(|| candidates[0].id.clone())
                })
            } else if constant_receiver {
                unique_type(receiver, &exact, &short).map(|ruby_type| {
                    if callee == "new" {
                        return ruby_type.id.clone();
                    }
                    let candidates = methods_by_owner
                        .get(&ruby_type.id)
                        .into_iter()
                        .flatten()
                        .filter(|method| method.singleton && method.name == normalized_callee)
                        .collect::<Vec<_>>();
                    if candidates.len() == 1 {
                        candidates[0].id.clone()
                    } else {
                        // Framework-provided class APIs such as ActiveRecord's
                        // `where` have no local method node.  The unique class
                        // remains useful, explicit blast-radius evidence.
                        ruby_type.id.clone()
                    }
                })
            } else if receiver.is_empty() || receiver == "self" {
                method_owner_by_id
                    .get(edge.true_source())
                    .and_then(|owner| instance_mixins_by_owner.get(owner))
                    .and_then(|mixins| {
                        let candidates = mixins
                            .iter()
                            .flat_map(|mixin| methods_by_owner.get(mixin).into_iter().flatten())
                            .filter(|method| !method.singleton && method.name == normalized_callee)
                            .collect::<Vec<_>>();
                        (candidates.len() == 1).then(|| candidates[0].id.clone())
                    })
            } else {
                None
            };
            let Some(target) = resolved else {
                return true;
            };
            edge.target = target.clone();
            edge.confidence = Confidence::Extracted;
            edge.extra.insert("_tgt".into(), target.into());
            edge.extra.remove("unresolved_call");
            edge.extra.remove("callee");
            edge.extra.remove("member_call");
            true
        });
    }
}

pub(crate) fn require_target(source_file: &str, module: &str) -> Option<String> {
    let module = module.replace('\\', "/");
    if module
        .split('/')
        .next_back()
        .is_none_or(|component| matches!(component, "" | "." | ".."))
    {
        return None;
    }
    match source_relative_project_path(source_file, &module)? {
        ProjectPath::Contained(logical) => Some(make_id(&[&logical])),
        ProjectPath::EscapesRoot(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::require_target;
    use graphoxide_core::make_id;

    #[test]
    fn require_relative_targets_only_portable_contained_paths() {
        for (module, logical) in [
            ("./workers/job.rb", "app/workers/job.rb"),
            ("../shared/job.rb", "shared/job.rb"),
            (r".\workers\job.rb", "app/workers/job.rb"),
            ("./naïve/東京.rb", "app/naïve/東京.rb"),
        ] {
            assert_eq!(
                require_target("app/main.rb", module),
                Some(make_id(&[logical])),
                "module={module:?}",
            );
        }

        for module in [
            "../../escape.rb",
            "/absolute.rb",
            r"\\server\share\worker.rb",
            "C:/absolute.rb",
            "C:relative.rb",
            "./dir/node:worker.rb",
            "./NUL.rb",
            "./worker.rb.",
            "./dir//worker.rb",
            ".",
            "..",
        ] {
            assert_eq!(
                require_target("app/main.rb", module),
                None,
                "unsafe module={module:?}",
            );
        }
        assert_eq!(require_target("/absolute/main.rb", "./worker.rb"), None);
    }
}
