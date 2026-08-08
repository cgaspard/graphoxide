use graphoxide_core::Extraction;
use graphoxide_extract::format_registry::{
    format_registry, ByteAdapterKind, FormatCapability, OFFICE_LIMITS, PDF_LIMITS,
};
use serde_json::Value;
use std::{fs, path::Path};

fn extract_fixture(name: &str, bytes: &[u8]) -> Extraction {
    let project = tempfile::tempdir().expect("create fixture directory");
    let path = project.path().join(name);
    fs::write(&path, bytes).expect("write representation-valid fixture");
    graphoxide_extract::extract(&path).expect("extract registered fixture")
}

fn has_semantic_domain_facts(extraction: &Extraction) -> bool {
    extraction.nodes.len() > 1
        && extraction.nodes.iter().any(|node| {
            node.extra.get("type").and_then(Value::as_str) != Some("structured_file")
                && node.extra.get("type").and_then(Value::as_str) != Some("format_inventory")
        })
        && !extraction.nodes.iter().any(|node| {
            node.extra.get("structured_unparsed") == Some(&Value::Bool(true))
                || matches!(
                    node.extra.get("parse_status").and_then(Value::as_str),
                    Some("partial" | "unrecognized" | "inventory_only" | "rejected")
                )
        })
}

fn assert_semantic_fixture(name: &str, bytes: &[u8], expected_label: &str) {
    let spec = format_registry()
        .find_by_path(Path::new(name))
        .expect("fixture format is registered");
    assert_eq!(
        spec.capability,
        FormatCapability::SemanticFull,
        "{name} must explicitly advertise semantic extraction"
    );
    let extraction = extract_fixture(name, bytes);
    assert!(
        has_semantic_domain_facts(&extraction),
        "{name} produced inventory or unparsed fallback facts"
    );
    assert!(
        extraction
            .nodes
            .iter()
            .any(|node| node.label == expected_label),
        "{name} did not emit the expected domain fact {expected_label:?}"
    );
}

fn assert_partial_root(name: &str, bytes: &[u8]) -> Extraction {
    let spec = format_registry()
        .find_by_path(Path::new(name))
        .expect("fixture format is registered");
    assert_eq!(
        spec.capability,
        FormatCapability::StructuralPartial,
        "{name}"
    );
    let extraction = extract_fixture(name, bytes);
    let root = extraction.nodes.first().expect("partial extraction root");
    assert_eq!(
        root.extra.get("format_capability").and_then(Value::as_str),
        Some("structural_partial"),
        "{name}"
    );
    assert_eq!(
        root.extra.get("parse_status").and_then(Value::as_str),
        Some("partial"),
        "{name}"
    );
    extraction
}

#[test]
fn semantic_full_registry_claims_are_backed_by_complete_adapters() {
    let registry = format_registry();
    let semantic_ids = registry
        .specs()
        .iter()
        .filter(|spec| spec.capability == FormatCapability::SemanticFull)
        .map(|spec| spec.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        semantic_ids,
        [
            "json",
            "json-variants",
            "json-lines",
            "toml",
            "xml",
            "named-json-configuration",
            "graphviz-dot",
        ]
    );
    assert!(registry.specs().iter().all(|spec| !matches!(
        spec.capability,
        FormatCapability::SchemaFull | FormatCapability::ContainerFull
    )));

    for id in [
        "source-code",
        "terraform-hcl",
        "markdown",
        "arrow-ipc",
        "parquet",
        "tar-archive",
    ] {
        assert_eq!(
            registry.find_by_id(id).map(|spec| spec.capability),
            Some(FormatCapability::StructuralPartial),
            "{id}"
        );
    }
    assert_eq!(
        registry
            .find_by_id("package-manifest")
            .map(|spec| spec.capability),
        Some(FormatCapability::InventoryOnly)
    );
}

#[test]
fn scanner_owned_families_publish_partial_capability_and_status() {
    for (name, bytes) in [
        ("guide.rst", b"Guide\n=====\n".as_slice()),
        ("service.ini", b"[service]\nreplicas=3\n".as_slice()),
        (
            "schema.xsd",
            br#"<schema><element name="service"/></schema>"#.as_slice(),
        ),
        ("table.csv", b"name,replicas\napi,3\n".as_slice()),
        (
            "api.proto",
            b"message Service {\n  string name = 1;\n}\n".as_slice(),
        ),
        ("plant.mo", b"model Plant\nend Plant;\n".as_slice()),
        (
            "robot.usda",
            b"#usda 1.0\ndef Xform \"Robot\" {}\n".as_slice(),
        ),
    ] {
        let _ = assert_partial_root(name, bytes);
    }
}

#[test]
fn compact_valid_protocol_declaration_does_not_masquerade_as_complete() {
    // A Protocol Buffers message body may be written on the declaration line.
    // The bounded line scanner intentionally recognizes the message but not
    // the inline field, which is direct evidence for structural_partial.
    let extraction = assert_partial_root(
        "compact.proto",
        b"message Compact { optional string inline_field = 1; }\n",
    );
    assert!(extraction.nodes.iter().any(|node| node.label == "Compact"));
    assert!(!extraction
        .nodes
        .iter()
        .any(|node| node.label == "inline_field"));
}

#[test]
fn compact_valid_usda_relationship_does_not_masquerade_as_complete() {
    // The prim declaration is retained, while a relationship on the same line
    // is outside the scanner's conservative subset.
    let extraction = assert_partial_root(
        "compact.usda",
        b"#usda 1.0\ndef Xform \"Robot\" { rel material:binding = </Materials/Red> }\n",
    );
    assert!(extraction.nodes.iter().any(|node| node.label == "Robot"));
    assert!(!extraction
        .nodes
        .iter()
        .any(|node| node.label == "/Materials/Red"));
}

#[test]
fn flow_style_api_yaml_is_truthfully_partial_when_subset_scanner_omits_endpoint() {
    let extraction = assert_partial_root(
        "service.openapi",
        b"openapi: 3.1.0\npaths: { /health: { get: { operationId: health } } }\n",
    );
    assert!(!extraction
        .nodes
        .iter()
        .any(|node| { matches!(node.label.as_str(), "/health" | "health") }));
}

#[test]
fn json_lines_aliases_produce_parsed_domain_facts_end_to_end() {
    for name in ["events.jsonl", "events.ndjson"] {
        assert_semantic_fixture(
            name,
            b"{\"service\":\"api\",\"replicas\":2}\n{\"service\":\"worker\",\"replicas\":1}\n",
            "replicas",
        );
    }
}

#[test]
fn json5_and_yaml_publish_and_emit_partial_structure() {
    for (name, bytes, expected_label) in [
        (
            "settings.json5",
            b"{\n  unquoted: 'value',\n  nested: { item: 1 },\n}\n".as_slice(),
            "unquoted",
        ),
        (
            "service.yaml",
            b"services:\n  - name: api\n    replicas: 3\n".as_slice(),
            "services",
        ),
    ] {
        let spec = format_registry()
            .find_by_path(Path::new(name))
            .expect("fixture format is registered");
        assert_eq!(
            spec.capability,
            FormatCapability::StructuralPartial,
            "{name}"
        );

        let extraction = extract_fixture(name, bytes);
        assert!(
            extraction.nodes.iter().any(|node| {
                node.label == expected_label
                    && node.extra.get("structured_unparsed") == Some(&Value::Bool(true))
            }),
            "{name} did not retain its bounded structural fact"
        );
    }
}

#[test]
fn inventory_only_output_cannot_pass_the_semantic_evidence_check() {
    let spec = format_registry()
        .find_by_path(Path::new("configuration.cue"))
        .expect("CUE inventory format is registered");
    assert_eq!(spec.capability, FormatCapability::InventoryOnly);

    let extraction = extract_fixture("configuration.cue", b"service: { replicas: 3 }\n");
    assert!(!has_semantic_domain_facts(&extraction));
    assert!(extraction.nodes.iter().any(|node| {
        node.extra.get("parse_status").and_then(Value::as_str) == Some("inventory_only")
    }));
}

#[test]
fn dot_is_semantic_full_while_other_diagram_scanners_remain_partial() {
    let diagram_specs = format_registry()
        .specs()
        .iter()
        .filter(|spec| spec.adapter() == ByteAdapterKind::Diagram)
        .collect::<Vec<_>>();
    assert!(!diagram_specs.is_empty());
    for spec in diagram_specs {
        let expected = if spec.id.as_str() == "graphviz-dot" {
            FormatCapability::SemanticFull
        } else {
            FormatCapability::StructuralPartial
        };
        assert_eq!(spec.capability, expected, "{}", spec.id.as_str());
    }

    let extraction = extract_fixture("architecture.dot", b"digraph G { api -> database; }");
    let root = extraction
        .nodes
        .iter()
        .find(|node| node.extra.get("type").and_then(Value::as_str) == Some("diagram"))
        .expect("diagram root");
    assert_eq!(
        root.extra.get("parse_status").and_then(Value::as_str),
        Some("complete")
    );
    assert_eq!(
        root.extra.get("format_capability").and_then(Value::as_str),
        Some("semantic_full")
    );
    assert!(has_semantic_domain_facts(&extraction));
}

#[test]
fn pdf_registry_promises_only_bounded_structural_page_extraction() {
    let spec = format_registry()
        .find_by_path(Path::new("document.pdf"))
        .expect("PDF format is registered");
    assert_eq!(spec.capability, FormatCapability::StructuralPartial);
    assert_eq!(spec.limits, PDF_LIMITS);
    assert_eq!(spec.limits.max_input_bytes, 16 * 1024 * 1024);
    assert_eq!(spec.limits.max_records, 1_025);
    assert_eq!(spec.limits.max_expansion_ratio, 64);
}

#[test]
fn document_package_registry_publishes_the_bounded_structural_contract() {
    for extension in ["docx", "xlsx", "pptx", "odt", "ods", "odp", "epub"] {
        let name = format!("document.{extension}");
        let spec = format_registry()
            .find_by_path(Path::new(&name))
            .expect("document package format is registered");
        assert_eq!(
            spec.capability,
            FormatCapability::StructuralPartial,
            "{name}"
        );
        assert_eq!(spec.adapter(), ByteAdapterKind::Office, "{name}");
        assert_eq!(spec.limits, OFFICE_LIMITS, "{name}");
    }
    assert_eq!(OFFICE_LIMITS.max_input_bytes, 16 * 1024 * 1024);
    assert_eq!(OFFICE_LIMITS.max_nesting, 128);
    assert_eq!(OFFICE_LIMITS.max_records, 4_096);
    assert_eq!(OFFICE_LIMITS.max_container_members, 1_024);
    assert_eq!(OFFICE_LIMITS.max_recursion_depth, 1);
    assert_eq!(OFFICE_LIMITS.max_expansion_ratio, 64);
}
