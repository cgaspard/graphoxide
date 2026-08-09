use graphoxide_core::{Edge, Extraction, Node};
use graphoxide_extract::{
    extract,
    format_registry::{format_registry, FormatCapability},
};
use graphoxide_graph::{build_graph, cluster};
use serde_json::{Map, Value};
use std::{collections::BTreeMap, fs, io::ErrorKind, net::TcpListener, path::Path};

const DIRECTED: &[u8] = include_bytes!("fixtures/dot/directed.dot");
const UNDIRECTED: &[u8] = include_bytes!("fixtures/dot/undirected.dot");
const STRICT: &[u8] = include_bytes!("fixtures/dot/strict.dot");
const NESTED_DEFAULTS: &[u8] = include_bytes!("fixtures/dot/nested-defaults.dot");
const CHAINED_PORTS: &[u8] = include_bytes!("fixtures/dot/chained-ports.dot");
const HTML_LABELS: &[u8] = include_bytes!("fixtures/dot/html-labels.dot");
const COMMENTS_AND_IDS: &[u8] = include_bytes!("fixtures/dot/comments-and-ids.dot");
const MALFORMED: &[u8] = include_bytes!("fixtures/dot/malformed.dot");

fn extract_source(name: &str, bytes: &[u8]) -> Extraction {
    let project = tempfile::tempdir().expect("create DOT fixture directory");
    let path = project.path().join(name);
    fs::write(&path, bytes).expect("write DOT fixture");
    extract(&path).expect("extract DOT fixture")
}

fn extract_at(path: &Path, bytes: &[u8]) -> anyhow::Result<Extraction> {
    fs::write(path, bytes).expect("write DOT fixture");
    extract(path)
}

fn root(extraction: &Extraction) -> &Node {
    extraction
        .nodes
        .iter()
        .find(|node| node.extra.get("type").and_then(Value::as_str) == Some("diagram"))
        .expect("DOT diagram root")
}

fn object<'a>(value: Option<&'a Value>, context: &str) -> &'a Map<String, Value> {
    value
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("{context} must be a JSON object"))
}

fn array<'a>(value: Option<&'a Value>, context: &str) -> &'a Vec<Value> {
    value
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("{context} must be a JSON array"))
}

fn dot_nodes(extraction: &Extraction) -> impl Iterator<Item = &Node> {
    extraction
        .nodes
        .iter()
        .filter(|node| node.extra.get("dot_id").and_then(Value::as_str).is_some())
}

fn dot_node<'a>(extraction: &'a Extraction, id: &str) -> &'a Node {
    dot_nodes(extraction)
        .find(|node| node.extra.get("dot_id").and_then(Value::as_str) == Some(id))
        .unwrap_or_else(|| panic!("DOT node {id:?}"))
}

fn semantic_edges(extraction: &Extraction) -> impl Iterator<Item = &Edge> {
    extraction
        .edges
        .iter()
        .filter(|edge| matches!(edge.relation.as_str(), "flows_to" | "connected_to"))
}

fn endpoint_ids(extraction: &Extraction, edge: &Edge) -> (String, String) {
    let graph_ids = dot_nodes(extraction)
        .map(|node| {
            (
                node.id.as_str(),
                node.extra
                    .get("dot_id")
                    .and_then(Value::as_str)
                    .expect("filtered DOT id"),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let source = edge
        .extra
        .get("_src")
        .and_then(Value::as_str)
        .unwrap_or(&edge.source);
    let target = edge
        .extra
        .get("_tgt")
        .and_then(Value::as_str)
        .unwrap_or(&edge.target);
    (
        graph_ids
            .get(source)
            .unwrap_or_else(|| panic!("semantic source node {source:?}"))
            .to_string(),
        graph_ids
            .get(target)
            .unwrap_or_else(|| panic!("semantic target node {target:?}"))
            .to_string(),
    )
}

fn semantic_edge<'a>(extraction: &'a Extraction, source: &str, target: &str) -> &'a Edge {
    semantic_edges(extraction)
        .find(|edge| endpoint_ids(extraction, edge) == (source.to_owned(), target.to_owned()))
        .unwrap_or_else(|| panic!("semantic edge {source:?} -> {target:?}"))
}

fn assert_source_range(value: Option<&Value>, context: &str) {
    let range = object(value, context);
    let start = object(range.get("start"), &format!("{context}.start"));
    let end = object(range.get("end"), &format!("{context}.end"));
    for (point, name) in [(start, "start"), (end, "end")] {
        assert!(
            point.get("byte").and_then(Value::as_u64).is_some(),
            "{context}.{name}.byte"
        );
        assert!(
            point.get("line").and_then(Value::as_u64).is_some(),
            "{context}.{name}.line"
        );
        assert!(
            point.get("column").and_then(Value::as_u64).is_some(),
            "{context}.{name}.column"
        );
    }
    assert!(
        start["byte"].as_u64() <= end["byte"].as_u64(),
        "{context} byte range must be ordered"
    );
}

#[test]
fn graph_kinds_defaults_and_strict_edge_merging_are_semantic() {
    let directed = extract_source("directed.dot", DIRECTED);
    let directed_root = root(&directed);
    assert_eq!(directed_root.extra["dot_graph_kind"], "digraph");
    assert_eq!(directed_root.extra["dot_graph_id"], "ServiceFlow");
    assert_eq!(directed_root.extra["dot_strict"], false);
    assert_eq!(directed_root.extra["parse_status"], "complete");
    assert_eq!(
        object(
            directed_root.extra.get("dot_attributes"),
            "graph attributes"
        )["label"],
        "Service flow"
    );
    assert_eq!(
        object(
            directed_root.extra.get("dot_node_defaults"),
            "node defaults"
        )["shape"],
        "box"
    );
    assert_eq!(
        object(
            directed_root.extra.get("dot_edge_defaults"),
            "edge defaults"
        )["color"],
        "blue"
    );
    assert_eq!(
        semantic_edge(&directed, "api", "database").relation,
        "flows_to"
    );

    let undirected = extract_source("undirected.gv", UNDIRECTED);
    let undirected_root = root(&undirected);
    assert_eq!(undirected_root.extra["dot_graph_kind"], "graph");
    assert_eq!(undirected_root.extra["dot_graph_id"], "Network");
    assert!(semantic_edges(&undirected).all(|edge| edge.relation == "connected_to"));

    let strict = extract_source("strict.graphviz", STRICT);
    let strict_root = root(&strict);
    assert_eq!(strict_root.extra["dot_graph_kind"], "graph");
    assert_eq!(strict_root.extra["dot_strict"], true);
    let edges = semantic_edges(&strict).collect::<Vec<_>>();
    assert_eq!(edges.len(), 1, "strict reverse edge must merge");
    assert_eq!(edges[0].extra["dot_occurrence_count"], 2);
    assert_eq!(edges[0].extra["dot_statement_count"], 2);
    assert_eq!(edges[0].extra["dot_parallel_count"], 1);
    assert_eq!(
        array(
            edges[0].extra.get("dot_occurrences"),
            "strict source occurrences"
        )
        .len(),
        2
    );
    let attrs = object(
        edges[0].extra.get("dot_attributes"),
        "strict edge attributes",
    );
    assert_eq!(attrs["color"], "red");
    assert_eq!(attrs["label"], "merged");
}

#[test]
fn nested_subgraphs_apply_lexical_defaults_without_leaking_scope() {
    let extraction = extract_source("nested-defaults.dot", NESTED_DEFAULTS);
    let outside_before = dot_node(&extraction, "outside_before");
    let outside_after = dot_node(&extraction, "outside_after");
    let api = dot_node(&extraction, "api");
    let database = dot_node(&extraction, "database");

    for outside in [outside_before, outside_after] {
        let attrs = object(outside.extra.get("dot_attributes"), "outer node attributes");
        assert_eq!(attrs["shape"], "box");
        assert_eq!(attrs["color"], "purple");
        assert!(array(outside.extra.get("dot_subgraphs"), "outer memberships").is_empty());
    }

    let api_attrs = object(api.extra.get("dot_attributes"), "api attributes");
    assert_eq!(api_attrs["shape"], "box");
    assert_eq!(api_attrs["color"], "green");
    assert_eq!(
        array(api.extra.get("dot_subgraphs"), "api memberships"),
        &[Value::String("cluster_services".into())]
    );

    let database_attrs = object(database.extra.get("dot_attributes"), "database attributes");
    assert_eq!(database_attrs["shape"], "cylinder");
    assert_eq!(database_attrs["color"], "green");
    assert_eq!(
        array(database.extra.get("dot_subgraphs"), "database memberships"),
        &[
            Value::String("cluster_services".into()),
            Value::String("persistence".into()),
        ]
    );
    assert_eq!(
        object(
            semantic_edge(&extraction, "api", "database")
                .extra
                .get("dot_attributes"),
            "nested edge attributes",
        )["color"],
        "blue"
    );
}

#[test]
fn chained_edges_ports_and_endpoint_subgraphs_expand_without_losing_occurrences() {
    let extraction = extract_source("chained-ports.dot", CHAINED_PORTS);
    let pairs = semantic_edges(&extraction)
        .map(|edge| endpoint_ids(&extraction, edge))
        .collect::<Vec<_>>();
    for expected in [
        ("source", "transform"),
        ("transform", "sink"),
        ("source", "audit"),
        ("source", "metrics"),
        ("transform", "audit"),
        ("transform", "metrics"),
    ] {
        assert!(
            pairs.contains(&(expected.0.to_owned(), expected.1.to_owned())),
            "missing expanded edge {expected:?}; got {pairs:?}"
        );
    }
    assert_eq!(pairs.len(), 6);

    let first = semantic_edge(&extraction, "source", "transform");
    let first_occurrence = array(
        first.extra.get("dot_occurrences"),
        "source edge occurrences",
    )
    .first()
    .and_then(Value::as_object)
    .expect("source edge occurrence object");
    assert_eq!(first_occurrence["source_port"], "output");
    assert_eq!(first_occurrence["source_compass"], "e");
    assert_eq!(first_occurrence["target_port"], "input");
    assert_eq!(first_occurrence["target_compass"], "w");

    let second = semantic_edge(&extraction, "transform", "sink");
    let second_occurrence = array(second.extra.get("dot_occurrences"), "sink edge occurrences")
        .first()
        .and_then(Value::as_object)
        .expect("sink edge occurrence object");
    assert_eq!(second_occurrence["source_port"], "input");
    assert_eq!(second_occurrence["source_compass"], "w");
    assert_eq!(second_occurrence["target_compass"], "n");
    assert_eq!(
        object(second.extra.get("dot_attributes"), "chain attributes")["label"],
        "pipeline"
    );
}

#[test]
fn quoted_numeric_and_html_ids_and_labels_survive_comment_lexing() {
    let labels = extract_source("html-labels.dot", HTML_LABELS);
    let service = dot_node(&labels, "service-api");
    let numeric = dot_node(&labels, "2.34");
    let service_attrs = object(service.extra.get("dot_attributes"), "HTML node attributes");
    let service_label = service_attrs["label"].as_str().expect("HTML label string");
    assert!(service_label.contains("<TABLE>"));
    assert!(service_label.contains("<B>API</B>"));
    assert_eq!(numeric.label, "Numeric ID");
    assert!(object(
        semantic_edge(&labels, "service-api", "2.34")
            .extra
            .get("dot_attributes"),
        "HTML edge attributes",
    )["label"]
        .as_str()
        .is_some_and(|label| label.contains("<I>flow</I>")));

    let comments = extract_source("comments-and-ids.dot", COMMENTS_AND_IDS);
    assert_eq!(root(&comments).extra["dot_graph_id"], "quoted graph");
    let url = dot_node(&comments, "https://example.invalid//service");
    assert_eq!(url.label, "not /* a comment */");
    let worker = dot_node(&comments, "_worker");
    assert!(worker.label.contains("worker // literal"));
    assert_eq!(semantic_edges(&comments).count(), 1);
    assert!(!dot_nodes(&comments).any(|node| {
        matches!(
            node.extra.get("dot_id").and_then(Value::as_str),
            Some("Comment" | "markers" | "inside" | "real" | "block")
        )
    }));
}

#[test]
fn html_text_apostrophes_and_tag_attribute_quotes_remain_balanced_data() {
    let extraction = extract_source(
        "html-quotes.dot",
        br#"digraph HtmlQuotes {
          apostrophe [label=<<B>don't</B>>]
          quotes [label=<<TABLE><TR><TD HREF='https://example.invalid/?q="quoted"'>say "hello"</TD></TR></TABLE>>]
        }"#,
    );
    let apostrophe = object(
        dot_node(&extraction, "apostrophe")
            .extra
            .get("dot_attributes"),
        "apostrophe HTML attributes",
    )["label"]
        .as_str()
        .expect("apostrophe HTML label");
    assert_eq!(apostrophe, "<<B>don't</B>>");

    let quotes = object(
        dot_node(&extraction, "quotes").extra.get("dot_attributes"),
        "quoted HTML tag attributes",
    )["label"]
        .as_str()
        .expect("quoted HTML label");
    assert!(quotes.contains("HREF='https://example.invalid/?q=\"quoted\"'"));
    assert!(quotes.contains("say \"hello\""));
    assert_eq!(root(&extraction).extra["parse_status"], "complete");
}

#[test]
fn quoted_ids_preserve_non_special_backslashes_and_join_continuations() {
    let escapes = extract_source(
        "quoted-escapes.dot",
        br#"digraph Escapes { a [label="prefix\N\l\\suffix\"quote"] }"#,
    );
    assert_eq!(dot_node(&escapes, "a").label, r#"prefix\N\l\\suffix"quote"#);

    let continued = extract_source(
        "quoted-continuations.dot",
        b"digraph Continued {\n  lf [label=\"left\\\ncontinued\" + \"-joined\"]\n  crlf [label=\"right\\\r\ncontinued\" + \"-joined\"]\n}\n",
    );
    assert_eq!(dot_node(&continued, "lf").label, "leftcontinued-joined");
    assert_eq!(dot_node(&continued, "crlf").label, "rightcontinued-joined");
    assert_eq!(root(&continued).extra["parse_status"], "complete");
}

#[test]
fn keywords_ignore_case_while_attribute_names_preserve_case() {
    let extraction = extract_source(
        "keyword-case.dot",
        br#"StRiCt DiGrApH MixedCase {
          NoDe [shape=box Color=Upper][color=green]
          A [label=lower][LABEL=UPPER Tooltip=kept]
          A -> B
        }"#,
    );
    let graph = root(&extraction);
    assert_eq!(graph.extra["dot_graph_kind"], "digraph");
    assert_eq!(graph.extra["dot_graph_id"], "MixedCase");
    assert_eq!(graph.extra["dot_strict"], true);

    let attrs = object(
        dot_node(&extraction, "A").extra.get("dot_attributes"),
        "case-sensitive node attributes",
    );
    assert_eq!(attrs["shape"], "box");
    assert_eq!(attrs["Color"], "Upper");
    assert_eq!(attrs["color"], "green");
    assert_eq!(attrs["label"], "lower");
    assert_eq!(attrs["LABEL"], "UPPER");
    assert_eq!(attrs["Tooltip"], "kept");
    assert_eq!(dot_node(&extraction, "A").label, "lower");
}

#[test]
fn numeric_ids_and_comma_endpoint_lists_expand_cartesian_products() {
    let extraction = extract_source(
        "numeric-list.dot",
        br#"digraph NumericLists {
          -.5 -> 2.
          a,b -> c,d [label=matrix]
        }"#,
    );
    assert_eq!(semantic_edge(&extraction, "-.5", "2.").relation, "flows_to");
    let pairs = semantic_edges(&extraction)
        .map(|edge| endpoint_ids(&extraction, edge))
        .collect::<Vec<_>>();
    for expected in [("a", "c"), ("a", "d"), ("b", "c"), ("b", "d")] {
        assert!(
            pairs.contains(&(expected.0.to_owned(), expected.1.to_owned())),
            "missing comma-list Cartesian edge {expected:?}; got {pairs:?}"
        );
        let edge = semantic_edge(&extraction, expected.0, expected.1);
        assert_eq!(edge.extra["dot_statement_count"], 1);
        assert_eq!(
            object(edge.extra.get("dot_attributes"), "list edge attrs")["label"],
            "matrix"
        );
    }
    assert_eq!(semantic_edges(&extraction).count(), 5);
}

#[test]
fn named_and_anonymous_subgraph_endpoints_expand_their_member_sets() {
    let extraction = extract_source(
        "subgraph-endpoints.dot",
        br#"digraph SubgraphEndpoints {
          subgraph named_sources { a b } -> { c d }
        }"#,
    );
    let pairs = semantic_edges(&extraction)
        .map(|edge| endpoint_ids(&extraction, edge))
        .collect::<Vec<_>>();
    for expected in [("a", "c"), ("a", "d"), ("b", "c"), ("b", "d")] {
        assert!(
            pairs.contains(&(expected.0.to_owned(), expected.1.to_owned())),
            "missing subgraph Cartesian edge {expected:?}; got {pairs:?}"
        );
    }
    assert_eq!(pairs.len(), 4);

    for id in ["a", "b"] {
        assert_eq!(
            array(
                dot_node(&extraction, id).extra.get("dot_subgraphs"),
                "named subgraph membership"
            ),
            &[Value::String("named_sources".into())]
        );
    }
    for id in ["c", "d"] {
        let memberships = array(
            dot_node(&extraction, id).extra.get("dot_subgraphs"),
            "anonymous subgraph membership",
        );
        assert_eq!(memberships.len(), 1);
        assert!(memberships[0]
            .as_str()
            .is_some_and(|name| name.starts_with("@anonymous:")));
    }
}

#[test]
fn repeated_attribute_lists_and_separator_free_assignments_merge_in_order() {
    let extraction = extract_source(
        "attribute-lists.dot",
        br#"digraph AttrLists {
          graph [rankdir=LR bgcolor=white][label="Attributes"]
          node [shape=box Color=Upper][color=green]
          a [label=first tooltip=plain][label=second, style=filled]
          a -> b [weight=2 color=blue][label=edge]
        }"#,
    );
    let graph_attrs = object(root(&extraction).extra.get("dot_attributes"), "graph attrs");
    assert_eq!(graph_attrs["rankdir"], "LR");
    assert_eq!(graph_attrs["bgcolor"], "white");
    assert_eq!(graph_attrs["label"], "Attributes");

    let attrs = object(
        dot_node(&extraction, "a").extra.get("dot_attributes"),
        "repeated node attrs",
    );
    assert_eq!(attrs["shape"], "box");
    assert_eq!(attrs["Color"], "Upper");
    assert_eq!(attrs["color"], "green");
    assert_eq!(attrs["label"], "second");
    assert_eq!(attrs["tooltip"], "plain");
    assert_eq!(attrs["style"], "filled");
    assert_eq!(dot_node(&extraction, "a").label, "second");

    let edge_attrs = object(
        semantic_edge(&extraction, "a", "b")
            .extra
            .get("dot_attributes"),
        "repeated edge attrs",
    );
    assert_eq!(edge_attrs["weight"], "2");
    assert_eq!(edge_attrs["color"], "blue");
    assert_eq!(edge_attrs["label"], "edge");
}

#[test]
fn compass_names_are_not_misclassified_as_ports() {
    let extraction = extract_source(
        "ports-and-compass.dot",
        br#"digraph PortsAndCompass {
          a:n -> b:custom
          a:custom:sw -> b:_
        }"#,
    );
    let edge = semantic_edge(&extraction, "a", "b");
    let occurrences = array(edge.extra.get("dot_occurrences"), "port occurrences");
    assert_eq!(occurrences.len(), 2);
    let first = occurrences[0].as_object().expect("first port occurrence");
    assert_eq!(first["source_port"], Value::Null);
    assert_eq!(first["source_compass"], "n");
    assert_eq!(first["target_port"], "custom");
    assert_eq!(first["target_compass"], Value::Null);
    let second = occurrences[1].as_object().expect("second port occurrence");
    assert_eq!(second["source_port"], "custom");
    assert_eq!(second["source_compass"], "sw");
    assert_eq!(second["target_port"], Value::Null);
    assert_eq!(second["target_compass"], "_");
}

#[test]
fn every_semantic_fact_carries_an_ordered_source_range() {
    let extraction = extract_source("directed.dot", DIRECTED);
    assert_source_range(
        root(&extraction).extra.get("source_range"),
        "root source range",
    );
    for node in dot_nodes(&extraction) {
        assert_source_range(node.extra.get("source_range"), "node source range");
    }
    for edge in semantic_edges(&extraction) {
        assert_source_range(edge.extra.get("source_range"), "edge source range");
        for occurrence in array(edge.extra.get("dot_occurrences"), "edge occurrences") {
            let occurrence = occurrence.as_object().expect("edge occurrence object");
            assert_source_range(occurrence.get("source_range"), "occurrence source range");
        }
    }
}

#[test]
fn statement_ranges_include_every_repeated_attribute_list_through_closing_bracket() {
    let node_statement = "a [shape=box][label=node]";
    let edge_statement = "c -> d [color=blue][label=edge]";
    let source = format!("digraph Ranges {{ {node_statement}; {edge_statement}; }}");
    let extraction = extract_source("statement-ranges.dot", source.as_bytes());

    let node_start = source.find(node_statement).expect("node statement offset") as u64;
    let node_end = node_start + node_statement.len() as u64;
    let node_range = object(
        dot_node(&extraction, "a").extra.get("source_range"),
        "complete node statement range",
    );
    assert_eq!(node_range["start"]["byte"], node_start);
    assert_eq!(node_range["end"]["byte"], node_end);

    let edge_start = source.find(edge_statement).expect("edge statement offset") as u64;
    let edge_end = edge_start + edge_statement.len() as u64;
    let edge = semantic_edge(&extraction, "c", "d");
    let edge_range = object(
        edge.extra.get("source_range"),
        "complete edge statement range",
    );
    assert_eq!(edge_range["start"]["byte"], edge_start);
    assert_eq!(edge_range["end"]["byte"], edge_end);
    let occurrence = array(edge.extra.get("dot_occurrences"), "ranged occurrences")[0]
        .as_object()
        .expect("ranged occurrence object");
    let occurrence_range = object(
        occurrence.get("source_range"),
        "complete occurrence statement range",
    );
    assert_eq!(occurrence_range["start"]["byte"], edge_start);
    assert_eq!(occurrence_range["end"]["byte"], edge_end);
}

#[test]
fn non_strict_parallel_edges_retain_each_occurrence_while_strict_edges_merge() {
    let non_strict = extract_source(
        "parallel.dot",
        br#"digraph Parallel {
          a -> b [color=red]
          a -> b [label="second"]
        }"#,
    );
    let edge = semantic_edge(&non_strict, "a", "b");
    assert_eq!(semantic_edges(&non_strict).count(), 1);
    assert_eq!(edge.extra["dot_occurrence_count"], 2);
    assert_eq!(edge.extra["dot_statement_count"], 2);
    assert_eq!(edge.extra["dot_parallel_count"], 2);
    assert_eq!(
        array(edge.extra.get("dot_occurrences"), "parallel occurrences").len(),
        2
    );

    let strict = extract_source(
        "strict-parallel.dot",
        br#"strict digraph Parallel {
          a -> b [color=red]
          a -> b [label="second"]
        }"#,
    );
    let edge = semantic_edge(&strict, "a", "b");
    assert_eq!(semantic_edges(&strict).count(), 1);
    assert_eq!(edge.extra["dot_occurrence_count"], 2);
    assert_eq!(edge.extra["dot_statement_count"], 2);
    assert_eq!(edge.extra["dot_parallel_count"], 1);
    assert_eq!(
        array(edge.extra.get("dot_occurrences"), "strict occurrences").len(),
        2
    );
    let attrs = object(edge.extra.get("dot_attributes"), "merged strict attributes");
    assert_eq!(attrs["color"], "red");
    assert_eq!(attrs["label"], "second");
}

#[test]
fn changed_defaults_do_not_retroactively_update_existing_strict_or_keyed_edges() {
    let strict = extract_source(
        "strict-default-transition.dot",
        br#"strict digraph Defaults {
          edge [color=red]
          a -> b [weight=1]
          edge [color=blue style=dashed]
          a -> b [label=updated]
        }"#,
    );
    let strict_edge = semantic_edge(&strict, "a", "b");
    let strict_attrs = object(
        strict_edge.extra.get("dot_attributes"),
        "strict final attributes",
    );
    assert_eq!(strict_attrs["color"], "red");
    assert_eq!(strict_attrs["weight"], "1");
    assert_eq!(strict_attrs["label"], "updated");
    assert!(!strict_attrs.contains_key("style"));

    let keyed = extract_source(
        "keyed-default-transition.dot",
        br#"digraph Defaults {
          edge [color=red]
          a -> b [key=k weight=1]
          edge [color=blue style=dashed]
          a -> b [key=k label=updated]
        }"#,
    );
    let keyed_occurrences = array(
        semantic_edge(&keyed, "a", "b").extra.get("dot_occurrences"),
        "keyed default-transition occurrences",
    );
    let final_attrs = object(
        keyed_occurrences[1]
            .as_object()
            .and_then(|occurrence| occurrence.get("dot_attributes")),
        "keyed final attributes",
    );
    assert_eq!(final_attrs["color"], "red");
    assert_eq!(final_attrs["weight"], "1");
    assert_eq!(final_attrs["label"], "updated");
    assert!(!final_attrs.contains_key("style"));
}

#[test]
fn keyed_parallel_edges_preserve_identity_and_every_source_occurrence() {
    let extraction = extract_source(
        "keyed-parallel.dot",
        br#"digraph Keyed {
          a -> b [key=discarded key=k1 color=red label=first]
          a -> b [key=k1 label=updated]
          a -> b [key=k2 color=blue label=second]
          a -> b [Key=ordinary label=anonymous]
        }"#,
    );
    let edge = semantic_edge(&extraction, "a", "b");
    assert_eq!(semantic_edges(&extraction).count(), 1);
    assert_eq!(edge.extra["dot_occurrence_count"], 4);
    assert_eq!(edge.extra["dot_statement_count"], 4);
    assert_eq!(
        edge.extra["dot_parallel_count"], 3,
        "reusing key=k1 addresses one identity while uppercase Key is anonymous"
    );
    let occurrences = array(edge.extra.get("dot_occurrences"), "keyed occurrences");
    assert_eq!(occurrences.len(), 4);
    for (index, (key, label)) in [
        (Some("k1"), "first"),
        (Some("k1"), "updated"),
        (Some("k2"), "second"),
        (None, "anonymous"),
    ]
    .into_iter()
    .enumerate()
    {
        let occurrence = occurrences[index]
            .as_object()
            .expect("keyed occurrence object");
        assert_eq!(occurrence["dot_key"].as_str(), key);
        let attrs = object(
            occurrence.get("dot_attributes"),
            "per-occurrence keyed attributes",
        );
        assert!(
            !attrs.contains_key("key"),
            "lowercase key is a CGraph pseudo-attribute, not an ordinary attribute"
        );
        assert_eq!(attrs["label"], label);
    }
    let first = object(
        occurrences[0]
            .as_object()
            .and_then(|occurrence| occurrence.get("dot_attributes")),
        "first k1 state",
    );
    let updated = object(
        occurrences[1]
            .as_object()
            .and_then(|occurrence| occurrence.get("dot_attributes")),
        "updated k1 state",
    );
    assert_eq!(first["color"], "red");
    assert_eq!(updated["color"], "red", "key=k1 retains prior edge state");
    assert_eq!(updated["label"], "updated");
    let anonymous = object(
        occurrences[3]
            .as_object()
            .and_then(|occurrence| occurrence.get("dot_attributes")),
        "uppercase Key attributes",
    );
    assert_eq!(anonymous["Key"], "ordinary");
}

#[test]
fn default_key_is_not_an_edge_identity_and_duplicate_explicit_key_uses_last_value() {
    let extraction = extract_source(
        "key-rules.dot",
        br#"digraph KeyRules {
          edge [key=default-key color=gray]
          a -> b [label=anonymous]
          a -> b [key=first key=last label=explicit]
        }"#,
    );
    let edge = semantic_edge(&extraction, "a", "b");
    assert_eq!(edge.extra["dot_occurrence_count"], 2);
    assert_eq!(edge.extra["dot_parallel_count"], 2);
    let occurrences = array(edge.extra.get("dot_occurrences"), "key-rule occurrences");
    let anonymous = occurrences[0]
        .as_object()
        .expect("default-key occurrence object");
    assert_eq!(anonymous["dot_key"], Value::Null);
    let anonymous_attrs = object(
        anonymous.get("dot_attributes"),
        "default-key ordinary attributes",
    );
    assert!(!anonymous_attrs.contains_key("key"));
    assert_eq!(anonymous_attrs["color"], "gray");

    let explicit = occurrences[1]
        .as_object()
        .expect("explicit-key occurrence object");
    assert_eq!(explicit["dot_key"], "last");
    let explicit_attrs = object(
        explicit.get("dot_attributes"),
        "explicit-key ordinary attributes",
    );
    assert!(!explicit_attrs.contains_key("key"));
    assert_eq!(explicit_attrs["color"], "gray");
    assert_eq!(explicit_attrs["label"], "explicit");
}

#[test]
fn non_strict_undirected_reverse_edges_aggregate_as_parallel_occurrences() {
    let extraction = extract_source(
        "undirected-reverse.dot",
        br#"graph Reverse {
          a -- b [label=forward]
          b -- a [label=reverse]
        }"#,
    );
    let edges = semantic_edges(&extraction).collect::<Vec<_>>();
    assert_eq!(
        edges.len(),
        1,
        "simple output must retain reverse parallels in metadata"
    );
    assert_eq!(
        endpoint_ids(&extraction, edges[0]),
        ("a".into(), "b".into())
    );
    assert_eq!(edges[0].extra["dot_occurrence_count"], 2);
    assert_eq!(edges[0].extra["dot_statement_count"], 2);
    assert_eq!(edges[0].extra["dot_parallel_count"], 2);
    let occurrences = array(
        edges[0].extra.get("dot_occurrences"),
        "undirected reverse occurrences",
    );
    assert_eq!(occurrences.len(), 2);
    assert_eq!(
        object(
            occurrences[0]
                .as_object()
                .and_then(|occurrence| occurrence.get("dot_attributes")),
            "forward attributes",
        )["label"],
        "forward"
    );
    assert_eq!(
        object(
            occurrences[1]
                .as_object()
                .and_then(|occurrence| occurrence.get("dot_attributes")),
            "reverse attributes",
        )["label"],
        "reverse"
    );
}

#[test]
fn malformed_input_recovers_useful_facts_and_caps_diagnostics_deterministically() {
    let recovered = extract_source("malformed.dot", MALFORMED);
    assert!(semantic_edge(&recovered, "good", "retained").relation == "flows_to");
    assert!(semantic_edge(&recovered, "after", "recovery").relation == "flows_to");
    let diagnostics = array(
        root(&recovered).extra.get("dot_diagnostics"),
        "DOT diagnostics",
    );
    assert!(!diagnostics.is_empty());
    assert!(
        diagnostics.iter().all(Value::is_object),
        "diagnostics must be structured"
    );
    assert_ne!(root(&recovered).extra["parse_status"], "inventory_only");

    let mut noisy = String::from("digraph Noisy { good -> retained;\n");
    for _ in 0..10_000 {
        noisy.push_str("@;\n");
    }
    noisy.push_str("after -> recovery; }");
    let project = tempfile::tempdir().expect("create noisy DOT project");
    let path = project.path().join("noisy.dot");
    let first = extract_at(&path, noisy.as_bytes()).expect("first noisy extraction");
    let second = extract(&path).expect("second noisy extraction");
    let first_diagnostics = array(
        root(&first).extra.get("dot_diagnostics"),
        "bounded diagnostics",
    );
    assert!(
        first_diagnostics.len() <= 64,
        "diagnostic growth must be explicitly bounded: {}",
        first_diagnostics.len()
    );
    assert_eq!(
        serde_json::to_vec(&first).expect("serialize first recovery"),
        serde_json::to_vec(&second).expect("serialize second recovery"),
        "bounded recovery must be deterministic"
    );
}

#[test]
fn deeply_nested_subgraphs_with_large_inherited_defaults_truncate_at_the_depth_limit() {
    let mut source = format!(
        "digraph NestedLimit {{ node [tooltip=\"{}\"] ",
        "x".repeat(4_000)
    );
    for depth in 0..80 {
        source.push_str(&format!("subgraph s{depth} {{ "));
    }
    source.push_str("leaf ");
    for _ in 0..80 {
        source.push_str("} ");
    }
    source.push('}');

    let extraction = extract_source("nested-resource-limit.dot", source.as_bytes());
    let graph = root(&extraction);
    assert_eq!(graph.extra["parse_status"], "partial");
    assert_eq!(graph.extra["truncated"], true);
    let diagnostics = array(
        graph.extra.get("dot_diagnostics"),
        "nested resource diagnostics",
    );
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.get("code").and_then(Value::as_str) == Some("dot_nesting_limit")
    }));
    assert!(
        extraction.nodes.len() < 100,
        "depth truncation must stop nested fact amplification"
    );
    assert!(
        serde_json::to_vec(&extraction)
            .expect("serialize bounded nested extraction")
            .len()
            < 2 * 1024 * 1024,
        "large inherited defaults must remain within a bounded result"
    );
}

#[test]
fn extraction_is_byte_deterministic_and_invalid_utf8_becomes_rejected_inventory() {
    let project = tempfile::tempdir().expect("create DOT project");
    let path = project.path().join("stable.dot");
    let first = extract_at(&path, CHAINED_PORTS).expect("first extraction");
    let second = extract(&path).expect("second extraction");
    assert_eq!(
        serde_json::to_vec(&first).expect("serialize first extraction"),
        serde_json::to_vec(&second).expect("serialize second extraction")
    );

    let invalid = project.path().join("invalid.dot");
    let rejected = extract_at(&invalid, b"digraph G { a -> \xff }")
        .expect("registered adapter must retain a rejected inventory fact");
    assert_eq!(rejected.nodes.len(), 1);
    assert!(rejected.edges.is_empty());
    let rejected_root = &rejected.nodes[0];
    assert_eq!(rejected_root.extra["type"], "format_inventory");
    assert_eq!(rejected_root.extra["parse_status"], "rejected");
    assert_eq!(rejected_root.extra["format_capability"], "inventory_only");
    assert_eq!(rejected_root.extra["diagnostic"], "diagram_parse_failed");
}

#[test]
fn utf8_bom_and_declared_latin1_are_decoded_semantically() {
    let mut bom = vec![0xef, 0xbb, 0xbf];
    bom.extend_from_slice("digraph Unicode { λ [label=\"文\"] }".as_bytes());
    let utf8 = extract_source("unicode-bom.dot", &bom);
    assert_eq!(root(&utf8).extra["parse_status"], "complete");
    assert_eq!(dot_node(&utf8, "λ").label, "文");

    let mut latin1 = b"digraph Latin1 { graph [charset=\"latin1\"] cafe [label=\"caf".to_vec();
    latin1.push(0xe9);
    latin1.extend_from_slice(b"\"] }");
    let decoded = extract_source("declared-latin1.dot", &latin1);
    assert_eq!(root(&decoded).extra["parse_status"], "complete");
    assert_eq!(
        object(
            root(&decoded).extra.get("dot_attributes"),
            "Latin-1 graph attributes"
        )["charset"],
        "latin1"
    );
    assert_eq!(dot_node(&decoded, "cafe").label, "café");

    for alias in [
        "LaTiN1",
        "LaTiN-1",
        "IsO-8859-1",
        "IsO_8859-1",
        "IsO8859-1",
        "IsO-Ir-100",
        "L1",
    ] {
        let mut bytes =
            format!("digraph Latin1 {{ graph [charset=\"{alias}\"] cafe [label=\"caf").into_bytes();
        bytes.push(0xe9);
        bytes.extend_from_slice(b"\"] }");
        let extraction = extract_source("latin1-alias.dot", &bytes);
        assert_eq!(
            dot_node(&extraction, "cafe").label,
            "café",
            "Latin-1 alias {alias:?}"
        );
        assert_eq!(root(&extraction).extra["parse_status"], "complete");
    }

    // C3 A9 is valid UTF-8 for é but is the two-character sequence Ã© in
    // ISO-8859-1. An explicit root charset declaration takes precedence over
    // UTF-8 sniffing, matching Graphviz's input contract.
    let ambiguous = extract_source(
        "declared-latin1-valid-utf8.dot",
        "digraph Latin1 { graph [charset=\"latin1\"] cafe [label=\"café\"] }".as_bytes(),
    );
    assert_eq!(dot_node(&ambiguous, "cafe").label, "cafÃ©");

    let utf8_control = extract_source(
        "utf8-without-charset.dot",
        "digraph Utf8 { cafe [label=\"café\"] }".as_bytes(),
    );
    assert_eq!(dot_node(&utf8_control, "cafe").label, "café");

    let mut late = b"digraph Late { cafe [label=\"caf".to_vec();
    late.push(0xe9);
    late.extend_from_slice(b"\"] graph [charset=\"latin1\"] }");
    let late = extract_source("late-root-latin1.dot", &late);
    assert_eq!(dot_node(&late, "cafe").label, "café");

    let mut node_local = b"digraph Local { cafe [charset=\"latin1\" label=\"caf".to_vec();
    node_local.push(0xe9);
    node_local.extend_from_slice(b"\"] }");
    let rejected = extract_source("node-local-latin1.dot", &node_local);
    assert_eq!(rejected.nodes.len(), 1);
    assert!(rejected.edges.is_empty());
    assert_eq!(rejected.nodes[0].extra["type"], "format_inventory");
    assert_eq!(rejected.nodes[0].extra["parse_status"], "rejected");
}

#[test]
fn graphviz_registry_claim_is_semantic_full_for_every_dot_extension() {
    let registry = format_registry();
    let spec = registry
        .find_by_id("graphviz-dot")
        .expect("Graphviz registry entry");
    assert_eq!(spec.capability, FormatCapability::SemanticFull);
    for extension in ["dot", "gv", "graphviz"] {
        let name = format!("diagram.{extension}");
        let by_path = registry
            .find_by_path(Path::new(&name))
            .unwrap_or_else(|| panic!("registry entry for .{extension}"));
        assert_eq!(by_path.id.as_str(), "graphviz-dot");
        assert_eq!(by_path.capability, FormatCapability::SemanticFull);
    }
}

#[test]
fn reference_like_attributes_and_process_strings_are_inert() {
    let project = tempfile::tempdir().expect("create adversarial DOT project");
    let secret_path = project.path().join("private.txt");
    let marker_path = project.path().join("must-not-exist");
    let stylesheet_path = project.path().join("style.css");
    let secret = "TOP_SECRET_DOT_REFERENCE_CONTENT";
    fs::write(&secret_path, secret).expect("write referenced secret");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind network tripwire");
    listener
        .set_nonblocking(true)
        .expect("make network tripwire nonblocking");
    let address = listener.local_addr().expect("network tripwire address");
    let source = format!(
        r#"digraph Inert {{
          graph [stylesheet="{}", URL="http://{address}/graph"]
          source [image="{}", shapefile="{}", href="file://{}", tooltip="$(touch {})", label=<<TABLE><TR><TD>safe HTML</TD></TR></TABLE>>]
          source -> sink [URL="http://{address}/edge", label="`touch {}`; curl http://{address}/process"]
        }}"#,
        stylesheet_path.display(),
        secret_path.display(),
        secret_path.display(),
        secret_path.display(),
        marker_path.display(),
        marker_path.display(),
    );
    let path = project.path().join("inert.dot");
    let extraction = extract_at(&path, source.as_bytes()).expect("extract inert DOT references");

    assert!(
        !marker_path.exists(),
        "DOT strings must never launch a process"
    );
    assert!(
        !stylesheet_path.exists(),
        "DOT stylesheet references must never be created"
    );
    assert_eq!(
        fs::read_to_string(&secret_path).expect("read test-owned secret"),
        secret,
        "DOT reference handling must not alter referenced files"
    );
    let serialized = serde_json::to_string(&extraction).expect("serialize adversarial extraction");
    assert!(
        !serialized.contains(secret),
        "referenced file contents must never enter extraction facts"
    );
    match listener.accept() {
        Err(error) if error.kind() == ErrorKind::WouldBlock => {}
        Err(error) => panic!("inspect network tripwire: {error}"),
        Ok((_, peer)) => panic!("DOT extraction made a network connection from {peer}"),
    }
}

#[test]
fn dot_identity_collisions_shared_labels_and_declared_self_loops_survive_graph_build() {
    let extraction = extract_source(
        "identity.dot",
        br#"digraph Identity {
          "Service-A" [label="Shared display label"]
          service_a [label="Shared display label"]
          "Service-A" -> service_a
          "Service-A" -> "Service-A" [label="retry"]
        }"#,
    );
    let mut graph = build_graph(&[extraction]).expect("build extracted DOT graph");
    cluster(&mut graph).expect("cluster extracted DOT graph");

    let dot_nodes = graph
        .nodes
        .iter()
        .filter(|node| {
            node.extra.get("diagram_format").and_then(Value::as_str) == Some("graphviz")
                && node.extra.get("dot_id").and_then(Value::as_str).is_some()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        dot_nodes.len(),
        2,
        "declared DOT identities must not deduplicate"
    );
    assert_eq!(dot_nodes[0].label, dot_nodes[1].label);
    assert_ne!(
        dot_nodes[0].id, dot_nodes[1].id,
        "case/punctuation-normalized DOT IDs require deterministic collision IDs"
    );
    let service = dot_nodes
        .iter()
        .find(|node| node.extra.get("dot_id").and_then(Value::as_str) == Some("Service-A"))
        .expect("case-sensitive Service-A node");
    assert!(graph.links.iter().any(|edge| {
        edge.relation == "flows_to"
            && edge.true_source() == service.id.as_str()
            && edge.true_target() == service.id.as_str()
    }));
}
