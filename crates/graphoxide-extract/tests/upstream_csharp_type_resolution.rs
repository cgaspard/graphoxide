//! One-to-one executable port of pinned Graphify
//! `tests/test_csharp_type_resolution.py`.

use graphoxide_core::{Edge, Extraction, Node};
use graphoxide_extract::extract_files;
use serde_json::Value;
use std::{collections::BTreeSet, fs, path::PathBuf};
use tempfile::TempDir;

fn corpus(files: &[(&str, &str)]) -> Extraction {
    let temp = TempDir::new().expect("temporary C# corpus");
    let mut paths = Vec::<PathBuf>::new();
    for (name, source) in files {
        let path = temp.path().join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create C# fixture directory");
        }
        fs::write(&path, source).expect("write C# fixture");
        paths.push(path);
    }
    let extractions = extract_files(&paths, Some(temp.path()), true)
        .expect("extract C# corpus")
        .extractions;
    Extraction {
        nodes: extractions
            .iter()
            .flat_map(|extraction| extraction.nodes.iter().cloned())
            .collect(),
        edges: extractions
            .iter()
            .flat_map(|extraction| extraction.edges.iter().cloned())
            .collect(),
        hyperedges: Vec::new(),
    }
}

fn node<'a>(result: &'a Extraction, id: &str) -> Option<&'a Node> {
    result.nodes.iter().find(|node| node.id == id)
}

fn defs<'a>(result: &'a Extraction, label: &str) -> Vec<&'a Node> {
    result
        .nodes
        .iter()
        .filter(|node| node.label == label && !node.source_file.is_empty())
        .collect()
}

fn targets<'a>(result: &'a Extraction, relation: &str, label: &str) -> Vec<&'a Node> {
    result
        .edges
        .iter()
        .filter(|edge| edge.relation == relation)
        .filter_map(|edge| node(result, edge.true_target()))
        .filter(|node| node.label == label)
        .collect()
}

fn metadata(node: &Node) -> &serde_json::Map<String, Value> {
    node.extra
        .get("metadata")
        .and_then(Value::as_object)
        .expect("node metadata")
}

fn edge_metadata(edge: &Edge) -> Option<&serde_json::Map<String, Value>> {
    edge.extra.get("metadata").and_then(Value::as_object)
}

fn namespace(node: &Node) -> Option<&str> {
    metadata(node).get("namespace").and_then(Value::as_str)
}

fn type_pairs(result: &Extraction, relation: &str) -> BTreeSet<(String, String)> {
    result
        .edges
        .iter()
        .filter(|edge| edge.relation == relation)
        .map(|edge| (edge.true_source().to_owned(), edge.true_target().to_owned()))
        .collect()
}

#[test]
fn test_csharp_declaration_nodes_carry_enclosing_namespace() {
    let result = corpus(&[
        (
            "block.cs",
            "namespace Game.Core { public class Damage {} }\n",
        ),
        (
            "nested.cs",
            "namespace Outer { namespace Inner { public class NestedDamage {} } }\n",
        ),
        (
            "file_scoped.cs",
            "namespace FileScoped.Core;\npublic class FileScopedDamage {}\n",
        ),
    ]);
    let damage = defs(&result, "Damage")[0];
    assert_eq!(namespace(damage), Some("Game.Core"));
    assert_eq!(
        namespace(defs(&result, "NestedDamage")[0]),
        Some("Outer.Inner")
    );
    assert_eq!(
        namespace(defs(&result, "FileScopedDamage")[0]),
        Some("FileScoped.Core")
    );
    assert!(
        metadata(damage)
            .get("scope_chain")
            .and_then(Value::as_array)
            .is_some_and(|scope| !scope.is_empty()),
        "lexical scope_chain must be stamped"
    );
}

#[test]
fn test_csharp_cross_file_inherits_resolves_to_real_def() {
    let result = corpus(&[
        (
            "core.cs",
            "namespace Game.Core { public class Damage { public int Calc() { return 1; } } }\n",
        ),
        (
            "combat.cs",
            "using Game.Core;\nnamespace Game.Combat { public class Weapon : Damage {} }\n",
        ),
    ]);
    let damage = targets(&result, "inherits", "Damage");
    assert!(!damage.is_empty());
    assert!(damage.iter().all(|node| !node.source_file.is_empty()));
}

#[test]
fn test_csharp_collision_disambiguated_by_using() {
    let result = corpus(&[
        (
            "core.cs",
            "namespace Game.Core { public class WeaponData { public int Number; } }\n",
        ),
        (
            "ui.cs",
            "namespace Game.UI { public class WeaponData { public int Width; } }\n",
        ),
        (
            "combat.cs",
            "using Game.Core;\nnamespace Game.Combat { public class Holder { public WeaponData data; } }\n",
        ),
    ]);
    assert!(!result
        .nodes
        .iter()
        .any(|node| node.label == "WeaponData" && node.source_file.is_empty()));
    let resolved: Vec<_> = targets(&result, "references", "WeaponData")
        .into_iter()
        .filter(|node| !node.source_file.is_empty())
        .collect();
    assert!(!resolved.is_empty());
    assert!(resolved
        .iter()
        .all(|node| node.source_file.contains("core.cs")));
}

#[test]
fn test_csharp_global_using_and_global_namespace() {
    let result = corpus(&[
        ("gadget.cs", "public class Gadget {}\n"),
        (
            "user.cs",
            "global using System;\npublic class Widget : Gadget {}\n",
        ),
    ]);
    let gadgets = targets(&result, "inherits", "Gadget");
    assert!(!gadgets.is_empty());
    assert!(gadgets.iter().all(|node| !node.source_file.is_empty()));
}

#[test]
fn test_csharp_cross_namespace_enum_reference_resolves_to_real_def() {
    let result = corpus(&[
        (
            "core.cs",
            "namespace Game.Core { public enum Element { Fire, Ice } public class Damage {} }\n",
        ),
        (
            "combat.cs",
            "using Game.Core;\nnamespace Game.Combat { public class Spell { Element element; Damage dmg; } }\n",
        ),
    ]);
    assert!(defs(&result, "Element")
        .iter()
        .all(|node| node.source_file.contains("core.cs")));
    let refs: Vec<_> = targets(&result, "references", "Element")
        .into_iter()
        .filter(|node| !node.source_file.is_empty())
        .collect();
    assert!(!refs.is_empty());
    assert!(refs.iter().all(|node| node.source_file.contains("core.cs")));
}

#[test]
fn test_csharp_cross_namespace_struct_and_record_references_resolve() {
    let result = corpus(&[
        (
            "core.cs",
            "namespace Game.Core { public struct Coord { public int X; } public record Player(string Name); }\n",
        ),
        (
            "combat.cs",
            "using Game.Core;\nnamespace Game.Combat { public class Spell { Coord coord; Player player; } }\n",
        ),
    ]);
    for label in ["Coord", "Player"] {
        assert!(!defs(&result, label).is_empty());
        let refs: Vec<_> = targets(&result, "references", label)
            .into_iter()
            .filter(|node| !node.source_file.is_empty())
            .collect();
        assert!(!refs.is_empty(), "missing resolved {label} reference");
        assert!(refs.iter().all(|node| node.source_file.contains("core.cs")));
    }
}

#[test]
fn test_csharp_ambiguous_using_does_not_resolve() {
    let result = corpus(&[
        (
            "core.cs",
            "namespace Game.Core { public class WeaponData { public int Number; } }\n",
        ),
        (
            "ui.cs",
            "namespace Game.UI { public class WeaponData { public int Width; } }\n",
        ),
        (
            "holder.cs",
            "using Game.Core;\nusing Game.UI;\nnamespace Game.Combat { public class Holder { public WeaponData data; } }\n",
        ),
    ]);
    let refs = targets(&result, "references", "WeaponData");
    assert!(!refs.is_empty());
    assert!(refs.iter().all(|node| node.source_file.is_empty()));
}

#[test]
fn test_csharp_using_alias_resolves_to_aliased_type() {
    let result = corpus(&[
        ("core.cs", "namespace Game.Core { public class Damage {} }\n"),
        (
            "combat.cs",
            "using Dmg = Game.Core.Damage;\nnamespace Game.Combat { public class Weapon : Dmg {} }\n",
        ),
    ]);
    let damage = targets(&result, "inherits", "Damage");
    assert!(!damage.is_empty());
    assert!(damage
        .iter()
        .all(|node| node.source_file.contains("core.cs")));
}

#[test]
fn test_csharp_namespace_nodes_canonical_and_discriminated() {
    let result = corpus(&[
        ("a.cs", "namespace N { class A {} }\n"),
        ("b.cs", "namespace N { class B {} }\n"),
        (
            "n.cs",
            "namespace Outer { namespace Inner { class C {} } }\n",
        ),
    ]);
    let namespaces: Vec<_> = result
        .nodes
        .iter()
        .filter(|node| node.extra.get("type").and_then(Value::as_str) == Some("namespace"))
        .collect();
    assert_eq!(
        namespaces.iter().filter(|node| node.label == "N").count(),
        1
    );
    assert!(namespaces.iter().any(|node| node.label == "Outer.Inner"));
    assert!(namespaces
        .iter()
        .all(|node| node.id.starts_with("csharp_namespace:")));
}

#[test]
fn test_csharp_import_edges_carry_using_kind() {
    let result = corpus(&[(
        "a.cs",
        "using Game.Core;\nusing static System.Math;\nglobal using System;\nusing X = Game.Core.Damage;\nclass Z {}\n",
    )]);
    let imports: BTreeSet<_> = result
        .edges
        .iter()
        .filter(|edge| edge.relation == "imports")
        .filter_map(edge_metadata)
        .map(|metadata| {
            (
                metadata["using_kind"].as_str().unwrap().to_owned(),
                metadata["target_fqn"].as_str().unwrap().to_owned(),
                metadata
                    .get("alias")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            )
        })
        .collect();
    assert!(imports.contains(&("namespace".into(), "Game.Core".into(), None)));
    assert!(imports.contains(&("namespace".into(), "System".into(), None)));
    assert!(imports.contains(&("static".into(), "System.Math".into(), None)));
    assert!(imports.contains(&("alias".into(), "Game.Core.Damage".into(), Some("X".into()))));
}

#[test]
fn test_csharp_import_edges_resolve_internal_namespace_and_alias() {
    let result = corpus(&[
        (
            "core.cs",
            "namespace Game.Core { public class Damage {} }\n",
        ),
        (
            "u.cs",
            concat!(
                "using Game.Core;\n",
                "using UnityEngine;\n",
                "using Dmg = Game.Core.Damage;\n",
                "using DMath = System.Math;\n",
                "using static Game.Core.Damage;\n",
                "class Z {}\n"
            ),
        ),
    ]);
    let imports: Vec<_> = result
        .edges
        .iter()
        .filter(|edge| edge.relation == "imports")
        .filter_map(|edge| Some((edge_metadata(edge)?, node(&result, edge.true_target()))))
        .collect();
    let find = |kind: &str, fqn: &str| {
        imports.iter().find_map(|(metadata, target)| {
            (metadata.get("using_kind").and_then(Value::as_str) == Some(kind)
                && metadata.get("target_fqn").and_then(Value::as_str) == Some(fqn))
            .then_some(*target)
        })
    };
    assert_eq!(
        find("namespace", "Game.Core")
            .flatten()
            .and_then(|node| node.extra.get("type"))
            .and_then(Value::as_str),
        Some("namespace")
    );
    assert!(find("namespace", "UnityEngine").flatten().is_none());
    assert_eq!(
        find("alias", "Game.Core.Damage")
            .flatten()
            .map(|node| node.label.as_str()),
        Some("Damage")
    );
    assert!(find("alias", "System.Math").flatten().is_none());
    assert!(find("static", "Game.Core.Damage").flatten().is_none());
    assert!(!result.nodes.iter().any(|node| {
        node.source_file.is_empty()
            && matches!(node.label.as_str(), "Game.Core" | "Game.Core.Damage")
    }));
}

#[test]
fn test_csharp_qualified_base_ref_is_flagged() {
    let result = corpus(&[("a.cs", "namespace N { class T {} class Use : B.T {} }\n")]);
    assert!(result.edges.iter().any(|edge| {
        edge_metadata(edge)
            .and_then(|metadata| metadata.get("qualified"))
            .and_then(Value::as_bool)
            == Some(true)
    }));
}

#[test]
fn test_csharp_one_file_same_name_no_collision_flag() {
    let result = corpus(&[(
        "dup.cs",
        "namespace A { class T {} } namespace B { class T {} }\n",
    )]);
    let types = defs(&result, "T");
    assert_eq!(
        types
            .iter()
            .map(|node| &node.id)
            .collect::<BTreeSet<_>>()
            .len(),
        2
    );
    assert!(types
        .iter()
        .all(|node| metadata(node).get("ns_collision").is_none()));
}

#[test]
fn test_csharp_type_parameter_emits_no_reference() {
    let result = corpus(&[(
        "a.cs",
        "namespace N { class T {} class Box<T> { T value; } }\n",
    )]);
    let real_t: BTreeSet<_> = defs(&result, "T")
        .into_iter()
        .map(|node| node.id.as_str())
        .collect();
    assert!(!result.edges.iter().any(|edge| {
        matches!(
            edge.relation.as_str(),
            "references" | "inherits" | "implements"
        ) && real_t.contains(edge.true_target())
            && edge.true_source().to_ascii_lowercase().contains("box")
    }));
}

#[test]
fn test_csharp_nested_type_carries_metadata() {
    let result = corpus(&[("a.cs", "namespace N { class Outer { class Inner {} } }\n")]);
    let inner = result
        .nodes
        .iter()
        .find(|node| node.label == "Inner")
        .unwrap();
    assert_eq!(
        metadata(inner)
            .get("is_nested_type")
            .and_then(Value::as_bool),
        Some(true)
    );
}

#[test]
fn test_csharp_cross_namespace_ref_not_misbound() {
    let result = corpus(&[(
        "x.cs",
        "namespace B { class Use : T {} } namespace C { class T {} }\n",
    )]);
    assert!(targets(&result, "inherits", "T")
        .iter()
        .all(|node| node.source_file.is_empty()));
}

#[test]
fn test_csharp_same_file_cross_namespace_ref_not_misbound() {
    let result = corpus(&[(
        "x.cs",
        "namespace B { class T {} } namespace C { class Use : T {} }\n",
    )]);
    assert!(targets(&result, "inherits", "T")
        .iter()
        .all(|node| node.source_file.is_empty()));
}

#[test]
fn test_csharp_inherits_does_not_bind_namespace_node() {
    let result = corpus(&[(
        "y.cs",
        "namespace Game { class Damage {} class Use : Game {} }\n",
    )]);
    let namespace_ids: BTreeSet<_> = result
        .nodes
        .iter()
        .filter(|node| node.extra.get("type").and_then(Value::as_str) == Some("namespace"))
        .map(|node| node.id.as_str())
        .collect();
    assert!(!result
        .edges
        .iter()
        .any(|edge| { edge.relation == "inherits" && namespace_ids.contains(edge.true_target()) }));
}

#[test]
fn test_csharp_qualified_ref_unknown_qualifier_dangles() {
    let result = corpus(&[("a.cs", "namespace A { class T {} class Use : B.T {} }\n")]);
    assert!(targets(&result, "inherits", "T")
        .iter()
        .all(|node| node.source_file.is_empty()));
}

#[test]
fn test_csharp_qualified_ref_known_namespace_resolves() {
    let result = corpus(&[
        ("n.cs", "namespace N { class T {} }\n"),
        ("m.cs", "namespace M { class Use : N.T {} }\n"),
    ]);
    let target = defs(&result, "T")[0];
    let source = defs(&result, "Use")[0];
    assert!(type_pairs(&result, "inherits").contains(&(source.id.clone(), target.id.clone())));
}

#[test]
fn test_csharp_qualified_generic_resolves_to_real_def() {
    let result = corpus(&[(
        "g.cs",
        "namespace N { class Box<TI> {} class Use { N.Box<int> b; } }\n",
    )]);
    let target = defs(&result, "Box")[0];
    let source = defs(&result, "Use")[0];
    assert!(type_pairs(&result, "references").contains(&(source.id.clone(), target.id.clone())));
    assert!(result.nodes.iter().all(|node| !node.label.contains('<')));
}

#[test]
fn test_csharp_qualified_alias_namespace_resolves() {
    let result = corpus(&[
        ("n.cs", "namespace X.Y { class T {} }\n"),
        (
            "m.cs",
            "using B = X.Y;\nnamespace M { class Use : B.T {} }\n",
        ),
    ]);
    let target = defs(&result, "T")[0];
    let source = defs(&result, "Use")[0];
    assert!(type_pairs(&result, "inherits").contains(&(source.id.clone(), target.id.clone())));
}

#[test]
fn test_csharp_qualified_out_of_scope_alias_falls_through_to_namespace() {
    let result = corpus(&[
        ("b.cs", "namespace B { class T {} }\n"),
        (
            "m.cs",
            "namespace A { using B = X.Y; }\nnamespace M { class Use : B.T {} }\n",
        ),
    ]);
    let target = defs(&result, "T")[0];
    let source = defs(&result, "Use")[0];
    assert!(type_pairs(&result, "inherits").contains(&(source.id.clone(), target.id.clone())));
}

#[test]
fn test_csharp_qualified_in_scope_alias_shadows_namespace() {
    let result = corpus(&[
        ("xy.cs", "namespace X.Y { class T {} }\n"),
        ("b.cs", "namespace B { class T {} }\n"),
        (
            "use.cs",
            "namespace A { using B = X.Y; class Good : B.T {} }\nnamespace C { using B = Z.Q; }\n",
        ),
    ]);
    let xy_t = defs(&result, "T")
        .into_iter()
        .find(|node| namespace(node) == Some("X.Y"))
        .unwrap();
    let b_t = defs(&result, "T")
        .into_iter()
        .find(|node| namespace(node) == Some("B"))
        .unwrap();
    let good = defs(&result, "Good")[0];
    let edges = type_pairs(&result, "inherits");
    assert!(edges.contains(&(good.id.clone(), xy_t.id.clone())));
    assert!(!edges.contains(&(good.id.clone(), b_t.id.clone())));
}

#[test]
fn test_csharp_one_file_same_name_binds_own_namespace() {
    let result = corpus(&[(
        "c.cs",
        "namespace A { class T {} } namespace B { class T {} class Use : T {} }\n",
    )]);
    let a_t = defs(&result, "T")
        .into_iter()
        .find(|node| namespace(node) == Some("A"))
        .unwrap();
    let b_t = defs(&result, "T")
        .into_iter()
        .find(|node| namespace(node) == Some("B"))
        .unwrap();
    let source = defs(&result, "Use")[0];
    let edges = type_pairs(&result, "inherits");
    assert!(edges.contains(&(source.id.clone(), b_t.id.clone())));
    assert!(!edges.contains(&(source.id.clone(), a_t.id.clone())));
}

#[test]
fn test_csharp_nested_type_not_importable_via_using() {
    let result = corpus(&[
        ("a.cs", "namespace N { class Outer { class Inner {} } }\n"),
        ("b.cs", "using N;\nnamespace M { class Use { Inner x; } }\n"),
    ]);
    assert!(targets(&result, "references", "Inner")
        .iter()
        .all(|node| node.source_file.is_empty()));
}

#[test]
fn test_csharp_generic_alias_resolves_to_base_type() {
    let result = corpus(&[
        ("core.cs", "namespace N { class Box {} }\n"),
        ("use.cs", "using Bx = N.Box<int>;\nclass Use : Bx {}\n"),
    ]);
    assert!(targets(&result, "inherits", "Box")
        .iter()
        .any(|node| !node.source_file.is_empty()));
}

#[test]
fn test_csharp_type_ref_never_targets_a_file_label() {
    let result = corpus(&[
        ("core.cs", "namespace N { class Box {} }\n"),
        ("b.cs", "using B = N.Box;\nclass Use : B {}\n"),
    ]);
    assert!(!result.edges.iter().any(|edge| {
        matches!(
            edge.relation.as_str(),
            "inherits" | "implements" | "references"
        ) && node(&result, edge.true_target()).is_some_and(|target| target.label.ends_with(".cs"))
    }));
}

#[test]
fn test_csharp_type_ref_edges_carry_ref_token() {
    let result = corpus(&[
        ("core.cs", "namespace N { class Base {} }\n"),
        ("use.cs", "using N;\nnamespace M { class Use : Base {} }\n"),
    ]);
    assert!(result.edges.iter().any(|edge| {
        edge.relation == "inherits"
            && edge.true_source().to_ascii_lowercase().contains("use")
            && edge_metadata(edge)
                .and_then(|metadata| metadata.get("ref_token"))
                .and_then(Value::as_str)
                == Some("Base")
    }));
}

#[test]
fn test_csharp_alias_matching_file_stem_resolves_via_token() {
    let result = corpus(&[
        ("core.cs", "namespace N { class Box {} }\n"),
        ("b.cs", "using B = N.Box;\nclass Use : B {}\n"),
    ]);
    assert!(targets(&result, "inherits", "Box")
        .iter()
        .any(|node| !node.source_file.is_empty()));
}

#[test]
fn test_csharp_same_name_diff_namespace_have_distinct_ids() {
    let result = corpus(&[(
        "x.cs",
        "namespace A { class T {} } namespace B { class T {} }\n",
    )]);
    assert_eq!(
        defs(&result, "T")
            .iter()
            .map(|node| node.id.as_str())
            .collect::<BTreeSet<_>>()
            .len(),
        2
    );
}

#[test]
fn test_csharp_global_scope_id_unchanged() {
    let result = corpus(&[("g.cs", "class Glob {}\n")]);
    let glob = defs(&result, "Glob")[0];
    assert_eq!(glob.id, graphoxide_core::make_id(&["g", "Glob"]));
    assert!(metadata(glob).get("namespace").is_none());
}

#[test]
fn test_csharp_namespaced_id_carries_namespace_segment() {
    let result = corpus(&[("n.cs", "namespace Game.Core { class Order {} }\n")]);
    let order = defs(&result, "Order")[0];
    assert!(order.id.ends_with("order") && order.id.contains("game_core"));
    assert_eq!(namespace(order), Some("Game.Core"));
}

#[test]
fn test_csharp_two_namespaces_each_resolve_own_type() {
    let result = corpus(&[(
        "two.cs",
        "namespace A { class T {} class UseA : T {} } namespace B { class T {} class UseB : T {} }\n",
    )]);
    let by = |label: &str, ns: &str| {
        result
            .nodes
            .iter()
            .find(|node| node.label == label && namespace(node) == Some(ns))
            .unwrap()
    };
    let (a_t, b_t, use_a, use_b) = (by("T", "A"), by("T", "B"), by("UseA", "A"), by("UseB", "B"));
    let edges = type_pairs(&result, "inherits");
    assert!(edges.contains(&(use_a.id.clone(), a_t.id.clone())));
    assert!(edges.contains(&(use_b.id.clone(), b_t.id.clone())));
    assert!(!edges.contains(&(use_a.id.clone(), b_t.id.clone())));
    assert!(!edges.contains(&(use_b.id.clone(), a_t.id.clone())));
}

#[test]
fn test_csharp_file_level_using_applies_across_blocks() {
    let result = corpus(&[
        ("n.cs", "namespace N { class T {} }\n"),
        (
            "u.cs",
            "using N;\nnamespace A { class X : T {} } namespace B { class Y : T {} }\n",
        ),
    ]);
    assert!(
        targets(&result, "inherits", "T")
            .iter()
            .filter(|node| !node.source_file.is_empty())
            .count()
            >= 2
    );
}

#[test]
fn test_csharp_namespace_scoped_using_isolated_to_sibling_block() {
    let result = corpus(&[
        ("n.cs", "namespace N { class T {} }\n"),
        (
            "u.cs",
            "namespace A { using N; class Good : T {} }\nnamespace A { class Bad : T {} }\n",
        ),
    ]);
    let good = defs(&result, "Good")[0];
    let bad = defs(&result, "Bad")[0];
    let target = defs(&result, "T")[0];
    let edges = type_pairs(&result, "inherits");
    assert!(edges.contains(&(good.id.clone(), target.id.clone())));
    assert!(!edges.contains(&(bad.id.clone(), target.id.clone())));
}

#[test]
fn test_csharp_using_flows_into_nested_block() {
    let result = corpus(&[
        ("n.cs", "namespace N { class T {} }\n"),
        (
            "u.cs",
            "namespace A { using N; namespace B { class Inner : T {} } }\n",
        ),
    ]);
    assert!(targets(&result, "inherits", "T")
        .iter()
        .any(|node| !node.source_file.is_empty()));
}

#[test]
fn test_csharp_alias_using_scoped_to_its_block() {
    let result = corpus(&[
        ("n.cs", "namespace N { class T {} }\n"),
        (
            "u.cs",
            "namespace A { using AliasT = N.T; class Good : AliasT {} }\nnamespace A { class Bad : AliasT {} }\n",
        ),
    ]);
    let good = defs(&result, "Good")[0];
    let bad = defs(&result, "Bad")[0];
    let target = defs(&result, "T")[0];
    let edges = type_pairs(&result, "inherits");
    assert!(edges.contains(&(good.id.clone(), target.id.clone())));
    assert!(!edges.contains(&(bad.id.clone(), target.id.clone())));
}
