//! Bounded byte-only extraction for engineering, facility, and operational models.
//!
//! This module deliberately accepts an already-admitted byte slice.  It never
//! opens a sibling, follows an include, expands an archive, or executes a
//! format-specific helper.  Cross-file resolution belongs to the I/O-owned
//! project snapshot stage; the facts here are only what the document itself
//! states.

use graphoxide_core::{make_id, sanitize_label, Confidence, Edge, Extraction, Node};
use quick_xml::{events::Event, Reader};
use regex::Regex;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::LazyLock,
};

const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_NODES: usize = 4_096;
const MAX_EDGES: usize = 8_192;
const MAX_DEPTH: usize = 16;
const MAX_TEXT_ENTITY_BYTES: usize = 4_096;

static IFC_ENTITY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(#\d+)\s*=\s*([A-Z][A-Z0-9_]*)\s*\(([^;]{0,4096})\)\s*;")
        .expect("valid IFC STEP expression")
});
static STEP_REFERENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"#\d+").expect("valid STEP reference expression"));
static KICAD_ENTITY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\((symbol|footprint|module|net|sheet|component|property|pin|pad)\s+(?:\"([^\"]{1,256})\"|([^\s()]{1,256}))"#)
        .expect("valid KiCad S-expression")
});
static KICAD_NET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\(net\s+\d+\s+\"([^\"]{1,256})\"\)"#).expect("valid KiCad net expression")
});
static GEDA_COMPONENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^C\s+\S+\s+\S+\s+\S+\s+\S+\s+\S+\s+(\S+\.sym)\s*$")
        .expect("valid gEDA component expression")
});
static GEDA_NET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^N\s+\S+\s+\S+\s+\S+\s+\S+\s+\d+").expect("valid gEDA net expression")
});
static GERBER_ATTRIBUTE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)%T([FO])\.([A-Za-z][A-Za-z0-9_.-]*)(?:,([^*]{0,256}))?\*%")
        .expect("valid Gerber attribute expression")
});
static GERBER_NET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)%TO\.N,([^*]{1,256})\*%").expect("valid Gerber net expression")
});
static IDF_OBJECT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*([A-Za-z][A-Za-z0-9_: -]{0,120})\s*,")
        .expect("valid EnergyPlus object expression")
});
static MODELICA_DECLARATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*(?:partial\s+|encapsulated\s+|replaceable\s+)*(model|package|class|record|connector|block|function)\s+([A-Za-z_][A-Za-z0-9_]*)")
        .expect("valid Modelica declaration expression")
});
static MODELICA_EXTENDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*extends\s+([A-Za-z_][A-Za-z0-9_.]*)")
        .expect("valid Modelica extends expression")
});
static MODELICA_CONNECT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"connect\s*\(\s*([A-Za-z_][A-Za-z0-9_.]*)\s*,\s*([A-Za-z_][A-Za-z0-9_.]*)\s*\)")
        .expect("valid Modelica connect expression")
});
static FOAM_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*([A-Za-z_][A-Za-z0-9_.-]*)\s*\{").expect("valid OpenFOAM block expression")
});
static FOAM_INCLUDE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*#include(?:Etc)?\s+[\"<]([^\">]{1,512})[\">]"#)
        .expect("valid OpenFOAM include expression")
});
static YANG_DECLARATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*(module|submodule|container|list|leaf-list|leaf|grouping|typedef|rpc|notification|identity|augment)\s+([A-Za-z_][A-Za-z0-9_.-]*)")
        .expect("valid YANG declaration expression")
});
static YANG_IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*(?:import|include)\s+([A-Za-z_][A-Za-z0-9_.-]*)")
        .expect("valid YANG import expression")
});
static YANG_LEAFREF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)\bpath\s+[\"']([^\"']{1,512})[\"']"#).expect("valid YANG leafref expression")
});
static TURTLE_TRIPLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*(<[^>]{1,512}>|[A-Za-z_][A-Za-z0-9_-]*:[^\s;,.]{1,512})\s+(a|<[^>]{1,512}>|[A-Za-z_][A-Za-z0-9_-]*:[^\s;,.]{1,512})\s+(<[^>]{1,512}>|[A-Za-z_][A-Za-z0-9_-]*:[^\s;,.]{1,512}|\"[^\"]{0,256}\")\s*[;.]"#)
        .expect("valid Turtle triple expression")
});

/// Extract engineering and operational facts from already-read source bytes.
///
/// The path is identity and classification metadata only.  It must not be used
/// for I/O: this function remains safe to call from an isolated CPU worker.
pub(crate) fn extract_engineering_bytes(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
) -> anyhow::Result<Extraction> {
    let mut format = format_for_path(path);
    let mut builder = Builder::new(path, source_file, format);
    if bytes.len() > MAX_SOURCE_BYTES {
        builder.inventory("source exceeds the engineering extractor byte limit");
        return Ok(builder.finish());
    }
    if bytes.contains(&0) {
        builder.inventory("binary representation requires a schema-aware or container adapter");
        return Ok(builder.finish());
    }
    let text = match crate::bytes::validate_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            builder.inventory("source is not valid UTF-8 text");
            return Ok(builder.finish());
        }
    };
    if format == "kicad" && (text.trim_start().starts_with("<?xml") || text.contains("<eagle")) {
        format = "eagle";
        builder.set_format(format);
    }
    if text.len() > MAX_TEXT_ENTITY_BYTES && format == "unknown" {
        builder.inventory("unrecognized engineering text format");
        return Ok(builder.finish());
    }

    let result = match format {
        "kicad" => parse_kicad(&mut builder, text),
        "eagle" | "ipc2581" | "ifcxml" | "bcf" | "ids" | "gbxml" | "citygml" | "landxml"
        | "dexpi" => parse_xml(&mut builder, text.as_bytes()),
        "geda" => parse_geda(&mut builder, text),
        "gerber" => parse_gerber(&mut builder, text),
        "dxf" => parse_dxf(&mut builder, text),
        "ifc" => parse_ifc_step(&mut builder, text),
        "energyplus" => parse_energyplus(&mut builder, text),
        "epjson" | "redfish" | "netbox" | "openconfig" => parse_structured_json(&mut builder, text),
        "netbox_yaml" | "openconfig_yaml" => parse_structured_mapping(&mut builder, text),
        "modelica" => parse_modelica(&mut builder, text),
        "openfoam" => parse_openfoam(&mut builder, text),
        "haystack" => parse_haystack(&mut builder, text),
        "brick" => parse_turtle(&mut builder, text),
        "yang" => parse_yang(&mut builder, text),
        "ifczip" | "unknown" => {
            builder
                .inventory("container or unrecognized representation requires a dedicated adapter");
            Ok(())
        }
        _ => {
            builder.inventory("recognized format has no textual representation adapter");
            Ok(())
        }
    };
    if let Err(error) = result {
        // Extraction from untrusted material must not turn a malformed document
        // into a failed indexing job.  Preserve the file inventory and attach a
        // bounded diagnostic instead.
        builder.inventory(&format!("malformed {format} document: {error}"));
    }
    Ok(builder.finish())
}

fn format_for_path(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "kicad_sch" | "kicad_pcb" | "kicad_pro" | "kicad_sym" | "kicad_mod" => "kicad",
        "sch" | "brd" | "lbr" if name.contains("eagle") => "eagle",
        "sch" | "brd" | "lbr" => "kicad",
        "net" => "geda",
        "ipc" | "ipc2581" => "ipc2581",
        "gbr" | "ger" | "gtl" | "gbl" | "gto" | "gbo" | "gts" | "gbs" => "gerber",
        "dxf" => "dxf",
        "ifc" => "ifc",
        "ifcxml" => "ifcxml",
        "ifczip" => "ifczip",
        "bcf" | "bcfxml" => "bcf",
        "ids" => "ids",
        "gbxml" => "gbxml",
        "citygml" | "gml" => "citygml",
        "landxml" => "landxml",
        "dexpi" | "pid" | "pfd" => "dexpi",
        "idf" => "energyplus",
        "epjson" => "epjson",
        "mo" | "modelica" => "modelica",
        "foam" | "openfoam" => "openfoam",
        "hayson" | "zinc" | "trio" | "skyarc" => "haystack",
        "haystack" => "haystack",
        "brick" => "brick",
        "redfish" => "redfish",
        "netbox" => "netbox",
        "openconfig" => "openconfig",
        "ttl" | "rdf" | "nq" => "brick",
        "yang" => "yang",
        "json" if name.contains("redfish") => "redfish",
        "json" if name.contains("netbox") => "netbox",
        "json" if name.contains("openconfig") => "openconfig",
        "yaml" | "yml" if name.contains("netbox") => "netbox_yaml",
        "yaml" | "yml" if name.contains("openconfig") => "openconfig_yaml",
        _ => "unknown",
    }
}

struct Builder<'a> {
    source_file: &'a str,
    stem: String,
    format: &'static str,
    file_id: String,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    aliases: BTreeMap<String, String>,
    seen_nodes: BTreeSet<String>,
    seen_edges: BTreeSet<(String, String, String)>,
    truncated: bool,
}

impl<'a> Builder<'a> {
    fn new(path: &Path, source_file: &'a str, format: &'static str) -> Self {
        let stem = Path::new(source_file)
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let file_id = make_id(&[&stem]);
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(source_file);
        let mut builder = Self {
            source_file,
            stem,
            format,
            file_id: file_id.clone(),
            nodes: Vec::new(),
            edges: Vec::new(),
            aliases: BTreeMap::new(),
            seen_nodes: BTreeSet::new(),
            seen_edges: BTreeSet::new(),
            truncated: false,
        };
        builder.add_node(file_id.clone(), file_name, "engineering_file", 1, false);
        if let Some(root) = builder.nodes.first_mut() {
            root.extra.insert("format".into(), format.into());
            root.extra
                .insert("capability".into(), "structural_partial".into());
            root.extra
                .insert("format_capability".into(), "structural_partial".into());
            root.extra.insert("parse_status".into(), "partial".into());
        }
        builder.aliases.insert("$file".into(), file_id);
        builder
    }

    fn finish(mut self) -> Extraction {
        if let (true, Some(root)) = (self.truncated, self.nodes.first_mut()) {
            root.extra.insert("truncated".into(), true.into());
        }
        Extraction {
            nodes: self.nodes,
            edges: self.edges,
            hyperedges: Vec::new(),
        }
    }

    fn set_format(&mut self, format: &'static str) {
        self.format = format;
        if let Some(root) = self.nodes.first_mut() {
            root.extra.insert("format".into(), format.into());
        }
    }

    fn inventory(&mut self, diagnostic: &str) {
        if let Some(root) = self.nodes.first_mut() {
            root.extra
                .insert("capability".into(), "inventory_only".into());
            root.extra
                .insert("format_capability".into(), "inventory_only".into());
            root.extra
                .insert("parse_status".into(), "inventory_only".into());
            root.extra.insert(
                "diagnostic".into(),
                sanitize_label(diagnostic)
                    .chars()
                    .take(256)
                    .collect::<String>()
                    .into(),
            );
        }
    }

    fn entity(&mut self, kind: &str, key: &str, label: &str, line: usize) -> String {
        let label = sanitize_label(label);
        if label.is_empty() || key.is_empty() {
            return String::new();
        }
        let alias = canonical_key(key);
        if let Some(existing) = self.aliases.get(&alias) {
            return existing.clone();
        }
        if self.nodes.len() >= MAX_NODES {
            self.truncated = true;
            return String::new();
        }
        let id = make_id(&[&self.stem, kind, key]);
        if !self.add_node(id.clone(), &label, kind, line, false) {
            self.truncated = true;
            return String::new();
        }
        self.aliases.insert(alias, id.clone());
        self.aliases
            .entry(canonical_key(&label))
            .or_insert_with(|| id.clone());
        id
    }

    fn reference(&mut self, value: &str, line: usize) -> String {
        let label = semantic_label(value);
        if label.is_empty() {
            return String::new();
        }
        let key = canonical_key(&label);
        if let Some(existing) = self.aliases.get(&key) {
            return existing.clone();
        }
        if self.nodes.len() >= MAX_NODES {
            self.truncated = true;
            return String::new();
        }
        let id = make_id(&["engineering-reference", self.format, &key]);
        if !self.add_node(id.clone(), &label, "reference", line, true) {
            self.truncated = true;
            return String::new();
        }
        self.aliases.insert(key, id.clone());
        id
    }

    fn contains(&mut self, parent: &str, child: &str, line: usize) {
        self.edge(parent, child, "contains", line, Confidence::Extracted);
    }

    fn edge(
        &mut self,
        source: &str,
        target: &str,
        relation: &str,
        line: usize,
        confidence: Confidence,
    ) {
        if source.is_empty()
            || target.is_empty()
            || source == target
            || self.edges.len() >= MAX_EDGES
        {
            if self.edges.len() >= MAX_EDGES {
                self.truncated = true;
            }
            return;
        }
        if !crate::parser_budget::try_reserve_facts(1) {
            self.truncated = true;
            return;
        }
        let key = (source.to_owned(), target.to_owned(), relation.to_owned());
        if self.seen_edges.contains(&key) {
            return;
        }
        self.seen_edges.insert(key);
        self.edges.push(Edge {
            source: source.into(),
            target: target.into(),
            relation: relation.into(),
            confidence,
            source_file: self.source_file.into(),
            extra: BTreeMap::from([
                ("_src".into(), source.into()),
                ("_tgt".into(), target.into()),
                ("source_location".into(), format!("L{line}").into()),
                ("weight".into(), 1.0.into()),
                ("_origin".into(), "engineering".into()),
            ]),
        });
    }

    fn add_node(
        &mut self,
        id: String,
        label: &str,
        kind: &str,
        line: usize,
        external: bool,
    ) -> bool {
        if id.is_empty() || self.seen_nodes.contains(&id) {
            return false;
        }
        if !crate::parser_budget::try_reserve_facts(1) {
            self.truncated = true;
            return false;
        }
        self.seen_nodes.insert(id.clone());
        let mut extra = BTreeMap::from([
            ("_origin".into(), "engineering".into()),
            ("type".into(), kind.into()),
            ("format".into(), self.format.into()),
        ]);
        if external {
            extra.insert("external".into(), true.into());
        }
        self.nodes.push(Node {
            id,
            label: sanitize_label(label),
            file_type: "document".into(),
            source_file: self.source_file.into(),
            source_location: Some(format!("L{line}")),
            community: None,
            extra,
        });
        true
    }
}

fn parse_kicad(builder: &mut Builder<'_>, text: &str) -> anyhow::Result<()> {
    for capture in KICAD_ENTITY.captures_iter(text) {
        let whole = capture.get(0).expect("whole KiCad capture");
        let kind = format!("kicad_{}", &capture[1]);
        let value = capture
            .get(2)
            .or_else(|| capture.get(3))
            .map(|value| value.as_str())
            .unwrap_or_default();
        let entity = builder.entity(
            &kind,
            &format!("{kind}:{value}"),
            value,
            line_of(text, whole.start()),
        );
        let root = builder.file_id.clone();
        builder.contains(&root, &entity, line_of(text, whole.start()));
    }
    for capture in KICAD_NET.captures_iter(text) {
        let whole = capture.get(0).expect("whole KiCad net capture");
        let value = &capture[1];
        let entity = builder.entity(
            "net",
            &format!("net:{value}"),
            value,
            line_of(text, whole.start()),
        );
        let root = builder.file_id.clone();
        builder.contains(&root, &entity, line_of(text, whole.start()));
    }
    Ok(())
}

fn parse_geda(builder: &mut Builder<'_>, text: &str) -> anyhow::Result<()> {
    for capture in GEDA_COMPONENT.captures_iter(text) {
        let whole = capture.get(0).expect("whole gEDA component capture");
        let symbol = Path::new(&capture[1])
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or(&capture[1]);
        let entity = builder.entity(
            "component",
            &format!("component:{symbol}"),
            symbol,
            line_of(text, whole.start()),
        );
        let root = builder.file_id.clone();
        builder.contains(&root, &entity, line_of(text, whole.start()));
    }
    for capture in GEDA_NET.captures_iter(text) {
        let whole = capture.get(0).expect("whole gEDA net capture");
        let entity = builder.entity(
            "net_segment",
            whole.as_str(),
            "net segment",
            line_of(text, whole.start()),
        );
        let root = builder.file_id.clone();
        builder.contains(&root, &entity, line_of(text, whole.start()));
    }
    Ok(())
}

fn parse_gerber(builder: &mut Builder<'_>, text: &str) -> anyhow::Result<()> {
    for capture in GERBER_ATTRIBUTE.captures_iter(text) {
        let whole = capture.get(0).expect("whole Gerber attribute capture");
        let key = &capture[2];
        let value = capture.get(3).map(|value| value.as_str()).unwrap_or(key);
        let entity = builder.entity("gerber_attribute", key, value, line_of(text, whole.start()));
        let root = builder.file_id.clone();
        builder.contains(&root, &entity, line_of(text, whole.start()));
    }
    for capture in GERBER_NET.captures_iter(text) {
        let whole = capture.get(0).expect("whole Gerber net capture");
        let name = &capture[1];
        let entity = builder.entity(
            "net",
            &format!("net:{name}"),
            name,
            line_of(text, whole.start()),
        );
        let root = builder.file_id.clone();
        builder.contains(&root, &entity, line_of(text, whole.start()));
    }
    Ok(())
}

#[derive(Default)]
struct DxfEntity {
    kind: String,
    handle: String,
    layer: String,
    owner: String,
    line: usize,
}

fn parse_dxf(builder: &mut Builder<'_>, text: &str) -> anyhow::Result<()> {
    let mut entity = DxfEntity::default();
    let mut ordinal = 0usize;
    let mut lines = text.lines().enumerate();
    while let Some((code_line, code)) = lines.next() {
        let Some((_, value)) = lines.next() else {
            break;
        };
        let Ok(code) = code.trim().parse::<i32>() else {
            continue;
        };
        if code == 0 {
            flush_dxf_entity(builder, &mut entity, &mut ordinal);
            let kind = value.trim();
            if !matches!(
                kind,
                "SECTION" | "ENDSEC" | "EOF" | "TABLE" | "ENDTAB" | "BLOCK" | "ENDBLK"
            ) {
                entity.kind = kind.to_owned();
                entity.line = code_line + 1;
            }
            continue;
        }
        if entity.kind.is_empty() {
            continue;
        }
        match code {
            5 => entity.handle = value.trim().to_owned(),
            8 => entity.layer = value.trim().to_owned(),
            330 => entity.owner = value.trim().to_owned(),
            _ => {}
        }
    }
    flush_dxf_entity(builder, &mut entity, &mut ordinal);
    Ok(())
}

fn flush_dxf_entity(builder: &mut Builder<'_>, entity: &mut DxfEntity, ordinal: &mut usize) {
    if entity.kind.is_empty() {
        return;
    }
    *ordinal += 1;
    let key = if entity.handle.is_empty() {
        format!("{}:{ordinal}", entity.kind)
    } else {
        format!("{}:{}", entity.kind, entity.handle)
    };
    let label = if entity.handle.is_empty() {
        entity.kind.clone()
    } else {
        format!("{} ({})", entity.kind, entity.handle)
    };
    let id = builder.entity("dxf_entity", &key, &label, entity.line.max(1));
    let root = builder.file_id.clone();
    builder.contains(&root, &id, entity.line.max(1));
    if !entity.layer.is_empty() {
        let layer = builder.reference(&format!("layer:{}", entity.layer), entity.line.max(1));
        builder.edge(
            &id,
            &layer,
            "on_layer",
            entity.line.max(1),
            Confidence::Extracted,
        );
    }
    if !entity.owner.is_empty() {
        let owner = builder.reference(&format!("dxf:{}", entity.owner), entity.line.max(1));
        builder.edge(
            &id,
            &owner,
            "owned_by",
            entity.line.max(1),
            Confidence::Extracted,
        );
    }
    *entity = DxfEntity::default();
}

fn parse_ifc_step(builder: &mut Builder<'_>, text: &str) -> anyhow::Result<()> {
    for capture in IFC_ENTITY.captures_iter(text) {
        let whole = capture.get(0).expect("whole IFC entity capture");
        let number = &capture[1];
        let class = &capture[2];
        let label = format!("{class} {number}");
        let entity = builder.entity("ifc_entity", number, &label, line_of(text, whole.start()));
        let root = builder.file_id.clone();
        builder.contains(&root, &entity, line_of(text, whole.start()));
    }
    for capture in IFC_ENTITY.captures_iter(text) {
        let whole = capture.get(0).expect("whole IFC entity capture");
        let source = builder.reference(&capture[1], line_of(text, whole.start()));
        for reference in STEP_REFERENCE.find_iter(&capture[3]) {
            let target = builder.reference(
                reference.as_str(),
                line_of(text, whole.start() + reference.start()),
            );
            builder.edge(
                &source,
                &target,
                "references",
                line_of(text, whole.start()),
                Confidence::Extracted,
            );
        }
    }
    Ok(())
}

fn parse_energyplus(builder: &mut Builder<'_>, text: &str) -> anyhow::Result<()> {
    for capture in IDF_OBJECT.captures_iter(text) {
        let whole = capture.get(0).expect("whole EnergyPlus object capture");
        let object = capture[1].trim();
        let entity = builder.entity(
            "energyplus_object",
            &format!("{object}:{}", whole.start()),
            object,
            line_of(text, whole.start()),
        );
        let root = builder.file_id.clone();
        builder.contains(&root, &entity, line_of(text, whole.start()));
    }
    Ok(())
}

fn parse_modelica(builder: &mut Builder<'_>, text: &str) -> anyhow::Result<()> {
    let mut owners = Vec::new();
    for capture in MODELICA_DECLARATION.captures_iter(text) {
        let whole = capture.get(0).expect("whole Modelica declaration capture");
        let kind = &capture[1];
        let name = &capture[2];
        let entity = builder.entity(kind, name, name, line_of(text, whole.start()));
        let parent = owners
            .last()
            .cloned()
            .unwrap_or_else(|| builder.file_id.clone());
        builder.contains(&parent, &entity, line_of(text, whole.start()));
        owners.push(entity);
    }
    let owner = owners
        .last()
        .cloned()
        .unwrap_or_else(|| builder.file_id.clone());
    for capture in MODELICA_EXTENDS.captures_iter(text) {
        let whole = capture.get(0).expect("whole Modelica extends capture");
        let target = builder.reference(&capture[1], line_of(text, whole.start()));
        builder.edge(
            &owner,
            &target,
            "extends",
            line_of(text, whole.start()),
            Confidence::Extracted,
        );
    }
    for capture in MODELICA_CONNECT.captures_iter(text) {
        let whole = capture.get(0).expect("whole Modelica connect capture");
        let source = builder.reference(&capture[1], line_of(text, whole.start()));
        let target = builder.reference(&capture[2], line_of(text, whole.start()));
        builder.edge(
            &source,
            &target,
            "connects",
            line_of(text, whole.start()),
            Confidence::Extracted,
        );
    }
    Ok(())
}

fn parse_openfoam(builder: &mut Builder<'_>, text: &str) -> anyhow::Result<()> {
    for capture in FOAM_BLOCK.captures_iter(text) {
        let whole = capture.get(0).expect("whole OpenFOAM block capture");
        let name = &capture[1];
        let entity = builder.entity(
            "openfoam_dictionary",
            name,
            name,
            line_of(text, whole.start()),
        );
        let root = builder.file_id.clone();
        builder.contains(&root, &entity, line_of(text, whole.start()));
    }
    for capture in FOAM_INCLUDE.captures_iter(text) {
        let whole = capture.get(0).expect("whole OpenFOAM include capture");
        let target = builder.reference(&capture[1], line_of(text, whole.start()));
        let root = builder.file_id.clone();
        builder.edge(
            &root,
            &target,
            "includes",
            line_of(text, whole.start()),
            Confidence::Extracted,
        );
    }
    Ok(())
}

fn parse_yang(builder: &mut Builder<'_>, text: &str) -> anyhow::Result<()> {
    let mut declarations = Vec::new();
    for capture in YANG_DECLARATION.captures_iter(text) {
        let whole = capture.get(0).expect("whole YANG declaration capture");
        let kind = &capture[1];
        let name = &capture[2];
        let entity = builder.entity(
            &format!("yang_{kind}"),
            name,
            name,
            line_of(text, whole.start()),
        );
        let parent = declarations
            .last()
            .cloned()
            .unwrap_or_else(|| builder.file_id.clone());
        builder.contains(&parent, &entity, line_of(text, whole.start()));
        declarations.push(entity);
    }
    let owner = declarations
        .last()
        .cloned()
        .unwrap_or_else(|| builder.file_id.clone());
    for capture in YANG_IMPORT.captures_iter(text) {
        let whole = capture.get(0).expect("whole YANG import capture");
        let target = builder.reference(&capture[1], line_of(text, whole.start()));
        builder.edge(
            &owner,
            &target,
            "imports",
            line_of(text, whole.start()),
            Confidence::Extracted,
        );
    }
    for capture in YANG_LEAFREF.captures_iter(text) {
        let whole = capture.get(0).expect("whole YANG leafref capture");
        let target = builder.reference(&capture[1], line_of(text, whole.start()));
        builder.edge(
            &owner,
            &target,
            "references",
            line_of(text, whole.start()),
            Confidence::Extracted,
        );
    }
    Ok(())
}

fn parse_haystack(builder: &mut Builder<'_>, text: &str) -> anyhow::Result<()> {
    for (index, line) in text.lines().enumerate() {
        let mut entity_id = None;
        let mut display = None;
        let mut refs = Vec::new();
        for token in line.split_whitespace() {
            let Some((key, value)) = token.split_once(':') else {
                continue;
            };
            let value = value.trim_matches('"');
            if key.eq_ignore_ascii_case("id") {
                entity_id = Some(value.trim_start_matches('@').to_owned());
            } else if matches!(
                key.to_ascii_lowercase().as_str(),
                "dis" | "display" | "name"
            ) {
                display = Some(value.to_owned());
            } else if key.to_ascii_lowercase().ends_with("ref") {
                refs.push(value.trim_start_matches('@').to_owned());
            }
        }
        let Some(key) = entity_id else { continue };
        let label = display.as_deref().unwrap_or(&key);
        let entity = builder.entity("haystack_entity", &key, label, index + 1);
        let root = builder.file_id.clone();
        builder.contains(&root, &entity, index + 1);
        for reference in refs {
            let target = builder.reference(&reference, index + 1);
            builder.edge(
                &entity,
                &target,
                "references",
                index + 1,
                Confidence::Extracted,
            );
        }
    }
    Ok(())
}

fn parse_turtle(builder: &mut Builder<'_>, text: &str) -> anyhow::Result<()> {
    for capture in TURTLE_TRIPLE.captures_iter(text) {
        let whole = capture.get(0).expect("whole Turtle triple capture");
        let line = line_of(text, whole.start());
        let subject_label = semantic_label(&capture[1]);
        let subject = builder.entity("brick_subject", &subject_label, &subject_label, line);
        let root = builder.file_id.clone();
        builder.contains(&root, &subject, line);
        let predicate = semantic_label(&capture[2]);
        let object = semantic_label(&capture[3]);
        if &capture[2] == "a" {
            let class = builder.reference(&object, line);
            builder.edge(&subject, &class, "instance_of", line, Confidence::Extracted);
        } else if !object.starts_with('"') {
            let target = builder.reference(&object, line);
            builder.edge(&subject, &target, &predicate, line, Confidence::Extracted);
        }
    }
    Ok(())
}

fn parse_structured_json(builder: &mut Builder<'_>, text: &str) -> anyhow::Result<()> {
    let value: serde_json::Value = serde_json::from_str(text)?;
    let root = builder.file_id.clone();
    walk_json(builder, &value, &root, "$", 0, 1);
    Ok(())
}

/// Extract the portable YAML subset used by inventory exports without adding a
/// second YAML parser.  Complex YAML constructs (anchors, tags, flow style,
/// and multiline scalars) intentionally remain inventory-only in the generic
/// structured adapter; this recognises ordinary mapping/list records and their
/// explicit references only.
fn parse_structured_mapping(builder: &mut Builder<'_>, text: &str) -> anyhow::Result<()> {
    let mut owner = builder.file_id.clone();
    for (index, raw_line) in text.lines().enumerate() {
        if raw_line.len() > 4_096 {
            continue;
        }
        let line = index + 1;
        let trimmed = raw_line.trim_start();
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with('-') && !trimmed.contains(':')
        {
            continue;
        }
        let trimmed = trimmed.strip_prefix("- ").unwrap_or(trimmed);
        let Some((key, raw_value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = raw_value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .trim_end_matches('#')
            .trim();
        if value.is_empty() || value.len() > 512 || value.starts_with('|') || value.starts_with('>')
        {
            continue;
        }
        if matches!(
            key.to_ascii_lowercase().as_str(),
            "id" | "name" | "slug" | "display_name"
        ) {
            let entity = builder.entity(
                "structured_entity",
                &format!("yaml:{line}:{value}"),
                value,
                line,
            );
            let root = builder.file_id.clone();
            builder.contains(&root, &entity, line);
            owner = entity;
        } else if is_reference_key(key) {
            let target = builder.reference(value, line);
            builder.edge(
                &owner,
                &target,
                json_relation(key),
                line,
                Confidence::Extracted,
            );
        }
    }
    Ok(())
}

fn walk_json(
    builder: &mut Builder<'_>,
    value: &serde_json::Value,
    owner: &str,
    path: &str,
    depth: usize,
    line: usize,
) {
    if depth >= MAX_DEPTH || builder.nodes.len() >= MAX_NODES {
        builder.truncated = true;
        return;
    }
    match value {
        serde_json::Value::Object(map) => {
            let identity = ["Id", "id", "Name", "name", "slug", "@odata.id", "url"]
                .iter()
                .find_map(|key| map.get(*key).and_then(json_scalar));
            let kind = ["@odata.type", "type", "kind", "model", "object_type"]
                .iter()
                .find_map(|key| map.get(*key).and_then(json_scalar))
                .unwrap_or("structured_entity");
            let current = identity
                .map(|identity| builder.entity(kind, &format!("{path}:{identity}"), identity, line))
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| owner.to_owned());
            if current != owner {
                builder.contains(owner, &current, line);
            }
            for (key, child) in map {
                let child_path = format!("{path}/{key}");
                if is_reference_key(key) {
                    add_json_references(builder, &current, key, child, line);
                }
                walk_json(builder, child, &current, &child_path, depth + 1, line);
            }
        }
        serde_json::Value::Array(values) => {
            for (index, child) in values.iter().enumerate().take(MAX_NODES) {
                walk_json(
                    builder,
                    child,
                    owner,
                    &format!("{path}[{index}]"),
                    depth + 1,
                    line,
                );
            }
        }
        _ => {}
    }
}

fn add_json_references(
    builder: &mut Builder<'_>,
    source: &str,
    key: &str,
    value: &serde_json::Value,
    line: usize,
) {
    match value {
        serde_json::Value::String(value) => {
            let target = builder.reference(value, line);
            builder.edge(
                source,
                &target,
                json_relation(key),
                line,
                Confidence::Extracted,
            );
        }
        serde_json::Value::Array(values) => {
            for value in values.iter().take(256) {
                add_json_references(builder, source, key, value, line);
            }
        }
        serde_json::Value::Object(map) => {
            if let Some(value) = ["@odata.id", "Id", "id", "Name", "name", "url"]
                .iter()
                .find_map(|key| map.get(*key).and_then(json_scalar))
            {
                let target = builder.reference(value, line);
                builder.edge(
                    source,
                    &target,
                    json_relation(key),
                    line,
                    Confidence::Extracted,
                );
            }
        }
        _ => {}
    }
}

fn json_scalar(value: &serde_json::Value) -> Option<&str> {
    value
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= 512)
}

fn is_reference_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.ends_with("ref")
        || key.contains("link")
        || key.contains("member")
        || key.contains("depend")
        || key.contains("connect")
        || key.contains("parent")
        || key.contains("related")
        || key.contains("contain")
        || key == "interfaces"
        || key == "cables"
}

fn json_relation(key: &str) -> &'static str {
    let key = key.to_ascii_lowercase();
    if key.contains("contain") || key.contains("member") {
        "contains"
    } else if key.contains("depend") {
        "depends_on"
    } else if key.contains("connect") || key.contains("cable") || key == "interfaces" {
        "connects"
    } else if key.contains("parent") {
        "parent_of"
    } else {
        "references"
    }
}

fn parse_xml(builder: &mut Builder<'_>, bytes: &[u8]) -> anyhow::Result<()> {
    let lower = bytes.iter().map(u8::to_ascii_lowercase).collect::<Vec<_>>();
    anyhow::ensure!(
        !lower.windows(9).any(|window| window == b"<!doctype")
            && !lower.windows(8).any(|window| window == b"<!entity"),
        "DOCTYPE and ENTITY declarations are not permitted"
    );
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let line_index = crate::bytes::LineIndex::new(bytes);
    let mut stack: Vec<Option<String>> = Vec::new();
    loop {
        match reader.read_event()? {
            Event::Start(event) => {
                let line = line_index.line_of(reader.buffer_position() as usize);
                let entity = xml_entity(
                    builder,
                    &event,
                    line,
                    stack.iter().rev().find_map(|id| id.as_deref()),
                );
                stack.push(entity);
            }
            Event::Empty(event) => {
                let line = line_index.line_of(reader.buffer_position() as usize);
                let _ = xml_entity(
                    builder,
                    &event,
                    line,
                    stack.iter().rev().find_map(|id| id.as_deref()),
                );
            }
            Event::End(_) => {
                let _ = stack.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(())
}

fn xml_entity(
    builder: &mut Builder<'_>,
    event: &quick_xml::events::BytesStart<'_>,
    line: usize,
    parent: Option<&str>,
) -> Option<String> {
    let tag = xml_name(event.name().as_ref());
    let attributes = xml_attributes(event).ok()?;
    let kind = xml_kind(&tag, &attributes)?;
    let identity = xml_identity(&attributes)?;
    let entity = builder.entity(&kind, &format!("{tag}:{identity}"), &identity, line);
    let parent = parent.unwrap_or(&builder.file_id).to_owned();
    builder.contains(&parent, &entity, line);
    for (key, value) in &attributes {
        if let Some(relation) = xml_relation(key) {
            let target = builder.reference(value, line);
            builder.edge(&entity, &target, relation, line, Confidence::Extracted);
        }
    }
    (!entity.is_empty()).then_some(entity)
}

fn xml_attributes(
    event: &quick_xml::events::BytesStart<'_>,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for attribute in event.attributes().with_checks(false) {
        let attribute = attribute?;
        let key = xml_name(attribute.key.as_ref());
        let value = crate::decode_xml_attribute(attribute.value.as_ref())?.into_owned();
        if !value.is_empty() && value.len() <= 512 {
            result.insert(key, value);
        }
    }
    Ok(result)
}

fn xml_name(name: &[u8]) -> String {
    String::from_utf8_lossy(name)
        .rsplit(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn xml_kind(tag: &str, attributes: &BTreeMap<String, String>) -> Option<String> {
    let kind = if tag.contains("component")
        || tag.contains("device")
        || tag.contains("part")
        || tag.contains("equipment")
        || tag.contains("asset")
        || tag.contains("rack")
        || tag.contains("server")
        || tag.contains("panel")
        || tag.contains("board")
    {
        "component"
    } else if tag.contains("net")
        || tag.contains("signal")
        || tag.contains("cable")
        || tag.contains("circuit")
        || tag.contains("pipe")
        || tag.contains("line")
    {
        "network"
    } else if tag.contains("pin")
        || tag.contains("pad")
        || tag.contains("port")
        || tag.contains("terminal")
        || tag.contains("interface")
        || tag.contains("point")
    {
        "port"
    } else if tag.contains("building")
        || tag.contains("facility")
        || tag.contains("site")
        || tag.contains("space")
        || tag.contains("storey")
        || tag.contains("floor")
        || tag.contains("zone")
    {
        "spatial"
    } else if tag.contains("topic") || tag.contains("issue") || tag.contains("viewpoint") {
        "issue"
    } else if attributes.keys().any(|key| {
        matches!(
            key.as_str(),
            "id" | "guid" | "globalid" | "uuid" | "name" | "refdes" | "tag"
        )
    }) {
        "xml_entity"
    } else {
        return None;
    };
    Some(kind.into())
}

fn xml_identity(attributes: &BTreeMap<String, String>) -> Option<String> {
    [
        "id",
        "guid",
        "globalid",
        "uuid",
        "name",
        "refdes",
        "tag",
        "identifier",
        "number",
        "objecttype",
        "longname",
    ]
    .iter()
    .find_map(|key| attributes.get(*key))
    .map(|value| semantic_label(value))
    .filter(|value| !value.is_empty())
}

fn xml_relation(key: &str) -> Option<&'static str> {
    let key = key.to_ascii_lowercase();
    if matches!(
        key.as_str(),
        "source" | "from" | "origin" | "target" | "to" | "destination" | "connectsto"
    ) {
        Some("connects")
    } else if key == "parent" || key == "parentid" {
        Some("parent_of")
    } else if key.ends_with("ref")
        || matches!(
            key.as_str(),
            "ref" | "idref" | "href" | "equipmentref" | "xlink:href"
        )
    {
        Some("references")
    } else {
        None
    }
}

fn canonical_key(value: &str) -> String {
    semantic_label(value).to_ascii_lowercase()
}

fn semantic_label(value: &str) -> String {
    let value = value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim_start_matches('<')
        .trim_end_matches('>');
    let value = value.rsplit(['/', '#']).next().unwrap_or(value);
    sanitize_label(value).chars().take(256).collect()
}

fn line_of(text: &str, byte_offset: usize) -> usize {
    text.as_bytes()[..byte_offset.min(text.len())]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(extension: &str, source: &str) -> Extraction {
        extract_engineering_bytes(
            Path::new(&format!("design.{extension}")),
            &format!("design.{extension}"),
            source.as_bytes(),
        )
        .expect("extract engineering fixture")
    }

    #[test]
    fn kicad_emits_components_and_nets() {
        let result = extract(
            "kicad_pcb",
            r#"(footprint "Connector")
               (net 1 "GND")
               (pad "1" smd)"#,
        );
        assert!(result.nodes.iter().any(|node| node.label == "Connector"));
        assert!(result.nodes.iter().any(|node| node.label == "GND"));
        assert!(result.edges.iter().any(|edge| edge.relation == "contains"));
    }

    #[test]
    fn ifc_preserves_step_references() {
        let result = extract(
            "ifc",
            "#1=IFCWALL('wall',#2);\n#2=IFCBUILDING('building',$);\n",
        );
        assert!(result.nodes.iter().any(|node| node.label == "IFCWALL #1"));
        assert!(result
            .edges
            .iter()
            .any(|edge| edge.relation == "references"));
    }

    #[test]
    fn xml_extracts_components_and_connections_without_dtd() {
        let result = extract(
            "gbxml",
            r#"<Building id="b1"><Equipment id="ahu1" connectsTo="zone1"/></Building>"#,
        );
        assert!(result.nodes.iter().any(|node| node.label == "ahu1"));
        assert!(result.edges.iter().any(|edge| edge.relation == "connects"));
    }

    #[test]
    fn redfish_json_creates_entities_and_links() {
        let path = Path::new("redfish-inventory.json");
        let result = extract_engineering_bytes(
            path,
            "redfish-inventory.json",
            br#"{"Id":"Rack1","Links":{"Members":[{"@odata.id":"/redfish/v1/Systems/1"}]}}"#,
        )
        .expect("extract Redfish fixture");
        assert!(result.nodes.iter().any(|node| node.label == "Rack1"));
        assert!(result.edges.iter().any(|edge| edge.relation == "contains"));
    }

    #[test]
    fn binary_input_is_inventory_only() {
        let result =
            extract_engineering_bytes(Path::new("asset.ifczip"), "asset.ifczip", b"PK\0\x03binary")
                .expect("inventory result");
        assert_eq!(result.nodes.len(), 1);
        assert_eq!(result.nodes[0].extra["capability"], "inventory_only");
    }

    #[test]
    fn path_classification_does_not_take_over_generic_json() {
        let registry = crate::format_registry::format_registry();
        assert_eq!(
            registry
                .find_by_path(Path::new("rack.redfish.json"))
                .map(|spec| spec.adapter()),
            Some(crate::format_registry::ByteAdapterKind::Engineering)
        );
        assert_ne!(
            registry
                .find_by_path(Path::new("package.json"))
                .map(|spec| spec.adapter()),
            Some(crate::format_registry::ByteAdapterKind::Engineering)
        );
        assert_eq!(
            registry.find_by_extension("ifc").map(|spec| spec.adapter()),
            Some(crate::format_registry::ByteAdapterKind::Engineering)
        );
    }
}
