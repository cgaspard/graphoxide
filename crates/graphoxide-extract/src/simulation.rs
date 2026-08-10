//! Byte-only structured extraction for simulation and digital-twin assets.
//!
//! This module deliberately accepts source bytes supplied by the indexing I/O
//! plane. It never opens a path, follows a reference, invokes a renderer, or
//! decompresses an asset container. That keeps untrusted simulation inputs on
//! the CPU extraction plane and makes unsupported binary/container formats
//! truthful inventory records rather than speculative semantic parses.

use anyhow::Context as _;
use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use quick_xml::{events::Event, Reader};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
};

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_XML_DEPTH: usize = 128;
const MAX_XML_EVENTS: usize = 200_000;
const MAX_JSON_DEPTH: usize = 128;
const MAX_GLTF_ITEMS: usize = 10_000;
const MAX_NODES: usize = 50_000;
const MAX_EDGES: usize = 100_000;
const MAX_ATTRIBUTE_VALUE_BYTES: usize = 8 * 1024;
const MAX_REFERENCES_PER_LINE: usize = 256;

/// Simulation asset formats covered by the byte-only extractor.
///
/// The binary variants are intentionally recognized separately so callers can
/// surface a deterministic inventory result without claiming that their
/// contents were interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SimulationFormat {
    Usda,
    UsdInventory,
    UsdcInventory,
    UsdzInventory,
    MaterialX,
    Gltf,
    GlbInventory,
    Urdf,
    Sdf,
    Mjcf,
    FmiModelDescription,
    FmuInventory,
    OpenDrive,
    OpenScenario,
}

impl SimulationFormat {
    pub(crate) const fn capability(self) -> &'static str {
        match self {
            Self::UsdInventory
            | Self::UsdcInventory
            | Self::UsdzInventory
            | Self::GlbInventory
            | Self::FmuInventory => "inventory_only",
            Self::Usda
            | Self::MaterialX
            | Self::Gltf
            | Self::Urdf
            | Self::Sdf
            | Self::Mjcf
            | Self::FmiModelDescription
            | Self::OpenDrive
            | Self::OpenScenario => "structural_partial",
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Usda => "openusd_ascii",
            Self::UsdInventory => "openusd",
            Self::UsdcInventory => "openusd_binary",
            Self::UsdzInventory => "openusd_package",
            Self::MaterialX => "materialx",
            Self::Gltf => "gltf",
            Self::GlbInventory => "glb",
            Self::Urdf => "urdf",
            Self::Sdf => "sdf",
            Self::Mjcf => "mjcf",
            Self::FmiModelDescription => "fmi_model_description",
            Self::FmuInventory => "fmu",
            Self::OpenDrive => "opendrive",
            Self::OpenScenario => "openscenario",
        }
    }
}

/// Return a format only for extensions/names whose interpretation is stable.
/// Generic `.xml` files are intentionally not claimed by this adapter.
pub(crate) fn format_for_path(path: &Path) -> Option<SimulationFormat> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name == "modeldescription.xml" {
        return Some(SimulationFormat::FmiModelDescription);
    }
    match path
        .extension()
        .and_then(|extension| extension.to_str())?
        .to_ascii_lowercase()
        .as_str()
    {
        "usda" => Some(SimulationFormat::Usda),
        "usd" => Some(SimulationFormat::UsdInventory),
        "usdc" => Some(SimulationFormat::UsdcInventory),
        "usdz" => Some(SimulationFormat::UsdzInventory),
        "mtlx" | "materialx" => Some(SimulationFormat::MaterialX),
        "gltf" => Some(SimulationFormat::Gltf),
        "glb" => Some(SimulationFormat::GlbInventory),
        "urdf" => Some(SimulationFormat::Urdf),
        "sdf" => Some(SimulationFormat::Sdf),
        "mjcf" => Some(SimulationFormat::Mjcf),
        "fmu" => Some(SimulationFormat::FmuInventory),
        "xodr" => Some(SimulationFormat::OpenDrive),
        "xosc" => Some(SimulationFormat::OpenScenario),
        _ => None,
    }
}

/// Extract one simulation asset from I/O-owned bytes.
///
/// `path` contributes only an extension/name classification and stable graph
/// identities. This function performs no filesystem, archive, or network I/O.
pub(crate) fn extract_simulation_bytes(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
) -> anyhow::Result<Extraction> {
    let format = format_for_path(path)
        .ok_or_else(|| anyhow::anyhow!("unsupported simulation asset path: {}", path.display()))?;
    extract_simulation_format_bytes(format, path, source_file, bytes)
}

/// Extract an explicitly classified simulation asset from I/O-owned bytes.
///
/// Registry adapters use this form when magic detection classified a file
/// without relying on a suffix. The implementation remains bounded even when
/// called directly in tests or from future container dispatch.
pub(crate) fn extract_simulation_format_bytes(
    format: SimulationFormat,
    path: &Path,
    source_file: &str,
    bytes: &[u8],
) -> anyhow::Result<Extraction> {
    ensure_size(bytes)?;
    match format {
        SimulationFormat::Usda => extract_usda(path, source_file, bytes),
        SimulationFormat::Gltf => extract_gltf(path, source_file, bytes),
        SimulationFormat::MaterialX
        | SimulationFormat::Urdf
        | SimulationFormat::Sdf
        | SimulationFormat::Mjcf
        | SimulationFormat::FmiModelDescription
        | SimulationFormat::OpenDrive
        | SimulationFormat::OpenScenario => extract_xml(format, path, source_file, bytes),
        SimulationFormat::UsdInventory
        | SimulationFormat::UsdcInventory
        | SimulationFormat::UsdzInventory
        | SimulationFormat::GlbInventory
        | SimulationFormat::FmuInventory => Ok(inventory_only(format, path, source_file, bytes)),
    }
}

fn ensure_size(bytes: &[u8]) -> anyhow::Result<()> {
    anyhow::ensure!(
        bytes.len() <= MAX_BYTES,
        "simulation asset is larger than the {MAX_BYTES}-byte extraction limit"
    );
    Ok(())
}

struct Builder<'a> {
    source_file: &'a str,
    stem: String,
    file_id: String,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    node_ids: HashSet<String>,
    edge_ids: HashSet<(String, String, String)>,
    duplicate_counts: HashMap<String, usize>,
    truncated: bool,
}

impl<'a> Builder<'a> {
    fn new(path: &Path, source_file: &'a str, format: SimulationFormat) -> Self {
        let stem = Path::new(source_file)
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let file_id = make_id(&[&stem]);
        let mut result = Self {
            source_file,
            stem,
            file_id: file_id.clone(),
            nodes: Vec::new(),
            edges: Vec::new(),
            node_ids: HashSet::new(),
            edge_ids: HashSet::new(),
            duplicate_counts: HashMap::new(),
            truncated: false,
        };
        let capability = format.capability();
        result.push_node(
            file_id,
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(source_file),
            "simulation_asset",
            1,
            BTreeMap::from([
                ("format".into(), format.name().into()),
                ("format_capability".into(), capability.into()),
                (
                    "parse_status".into(),
                    if capability == "structural_partial" {
                        "partial"
                    } else {
                        "inventory_only"
                    }
                    .into(),
                ),
            ]),
        );
        result
    }

    fn push_node(
        &mut self,
        id: String,
        label: &str,
        kind: &str,
        line: usize,
        mut extra: BTreeMap<String, serde_json::Value>,
    ) -> String {
        if id.is_empty() || self.node_ids.contains(&id) || self.nodes.len() >= MAX_NODES {
            return id;
        }
        if !crate::parser_budget::try_reserve_facts(1) {
            self.truncated = true;
            return String::new();
        }
        extra.insert("_origin".into(), "simulation".into());
        extra.insert("type".into(), kind.into());
        self.node_ids.insert(id.clone());
        self.nodes.push(Node {
            id: id.clone(),
            label: truncate(label, MAX_ATTRIBUTE_VALUE_BYTES),
            file_type: "document".into(),
            source_file: self.source_file.into(),
            source_location: Some(format!("L{line}")),
            community: None,
            extra,
        });
        id
    }

    fn owned_node(
        &mut self,
        kind: &str,
        label: &str,
        line: usize,
        extra: BTreeMap<String, serde_json::Value>,
    ) -> String {
        let base = make_id(&[&self.stem, kind, label]);
        let id = self.unique_id(base);
        self.push_node(id, label, kind, line, extra)
    }

    fn reference_node(&mut self, kind: &str, label: &str, line: usize) -> String {
        let id = make_id(&["simulation_reference", kind, label]);
        self.push_node(
            id,
            label,
            kind,
            line,
            BTreeMap::from([("external_reference".into(), true.into())]),
        )
    }

    fn unique_id(&mut self, base: String) -> String {
        if !self.node_ids.contains(&base) {
            return base;
        }
        let count = self.duplicate_counts.entry(base.clone()).or_insert(1);
        loop {
            *count += 1;
            let candidate = format!("{base}_{count}");
            if !self.node_ids.contains(&candidate) {
                return candidate;
            }
        }
    }

    fn edge(&mut self, source: &str, target: &str, relation: &str, line: usize) {
        if source.is_empty()
            || target.is_empty()
            || source == target
            || self.edges.len() >= MAX_EDGES
        {
            return;
        }
        if !crate::parser_budget::try_reserve_facts(1) {
            self.truncated = true;
            return;
        }
        let identity = (source.into(), target.into(), relation.into());
        if self.edge_ids.contains(&identity) {
            return;
        }
        self.edge_ids.insert(identity);
        self.edges.push(Edge {
            source: source.into(),
            target: target.into(),
            relation: relation.into(),
            confidence: Confidence::Extracted,
            source_file: self.source_file.into(),
            extra: BTreeMap::from([
                ("_src".into(), source.into()),
                ("_tgt".into(), target.into()),
                ("source_location".into(), format!("L{line}").into()),
                ("weight".into(), 1.0.into()),
            ]),
        });
    }

    fn finish(mut self) -> Extraction {
        if self.truncated
            && let Some(root) = self.nodes.first_mut()
        {
            root.extra.insert("truncated".into(), true.into());
        }
        Extraction {
            nodes: self.nodes,
            edges: self.edges,
            hyperedges: Vec::new(),
        }
    }
}

fn inventory_only(
    format: SimulationFormat,
    path: &Path,
    source_file: &str,
    bytes: &[u8],
) -> Extraction {
    let mut builder = Builder::new(path, source_file, format);
    if let Some(file) = builder.nodes.first_mut() {
        file.extra.insert("byte_length".into(), bytes.len().into());
        file.extra.insert(
            "diagnostic".into(),
            format!(
                "{} is recognized but remains inventory-only: no verified byte-only decoder/schema binding is available",
                format.name()
            )
            .into(),
        );
        file.extra
            .insert("semantic_extraction".into(), false.into());
        file.extra
            .insert("magic".into(), inventory_magic(format, bytes).into());
    }
    builder.finish()
}

fn inventory_magic(format: SimulationFormat, bytes: &[u8]) -> &'static str {
    match format {
        SimulationFormat::UsdcInventory if bytes.starts_with(b"PXR-USDC") => "usdc",
        SimulationFormat::UsdzInventory if bytes.starts_with(b"PK\x03\x04") => "zip",
        SimulationFormat::GlbInventory if bytes.starts_with(b"glTF") => "glb",
        SimulationFormat::FmuInventory if bytes.starts_with(b"PK\x03\x04") => "zip",
        _ => "unverified",
    }
}

fn text(bytes: &[u8]) -> anyhow::Result<&str> {
    crate::bytes::validate_utf8(bytes).context("simulation text must be UTF-8")
}

fn line_of(bytes: &[u8], offset: usize) -> usize {
    crate::bytes::line_number(bytes, offset)
}

fn truncate(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.into();
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

fn extract_usda(path: &Path, source_file: &str, bytes: &[u8]) -> anyhow::Result<Extraction> {
    let source = text(bytes)?;
    let mut builder = Builder::new(path, source_file, SimulationFormat::Usda);
    let mut prim_stack = Vec::<(usize, String)>::new();
    let mut braces = 0usize;
    let mut simready = false;
    let mut simready_requirements = HashSet::<String>::new();

    for (line_index, raw_line) in source.lines().enumerate() {
        let line_number = line_index + 1;
        let line = strip_usda_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if lower.contains("simready") || lower.contains("sim_ready") {
            simready = true;
        }
        for requirement in [
            "rigidbody",
            "collision",
            "mass",
            "physics",
            "semantic:label",
            "semantics:label",
            "articulation",
            "joint",
            "sensor",
        ] {
            if lower.contains(requirement) {
                simready_requirements.insert(requirement.into());
            }
        }

        while prim_stack.last().is_some_and(|(depth, _)| *depth > braces) {
            prim_stack.pop();
        }

        if let Some((specifier, prim_type, name)) = usda_declaration(line) {
            let kind = usd_kind(&prim_type);
            let id = builder.owned_node(
                kind,
                &name,
                line_number,
                BTreeMap::from([
                    ("usd_specifier".into(), specifier.into()),
                    ("usd_type".into(), prim_type.into()),
                ]),
            );
            let parent = prim_stack
                .last()
                .map(|(_, id)| id.clone())
                .unwrap_or_else(|| builder.file_id.clone());
            builder.edge(&parent, &id, "contains", line_number);
            let opens = count_usda_braces(line).0;
            let closes = count_usda_braces(line).1;
            braces = braces.saturating_add(opens).saturating_sub(closes);
            if opens > closes {
                anyhow::ensure!(
                    prim_stack.len() < MAX_XML_DEPTH,
                    "OpenUSD ASCII exceeds nesting limit"
                );
                prim_stack.push((braces, id));
            }
            continue;
        }

        let source = prim_stack
            .last()
            .map(|(_, id)| id.clone())
            .unwrap_or_else(|| builder.file_id.clone());
        for reference in usda_asset_references(line) {
            let target = builder.reference_node("asset_reference", &reference, line_number);
            builder.edge(&source, &target, "references", line_number);
        }
        for path_reference in usda_path_references(line) {
            let target = builder.reference_node("usd_prim_path", &path_reference, line_number);
            let relation = if lower.contains("material:binding") {
                "uses_material"
            } else if lower.starts_with("rel ") {
                "relates_to"
            } else {
                "references"
            };
            builder.edge(&source, &target, relation, line_number);
        }
        let (opens, closes) = count_usda_braces(line);
        braces = braces.saturating_add(opens).saturating_sub(closes);
        while prim_stack.last().is_some_and(|(depth, _)| *depth > braces) {
            prim_stack.pop();
        }
    }

    if simready {
        let simready_id = builder.owned_node(
            "simready_metadata",
            "SimReady metadata (unverified)",
            1,
            BTreeMap::from([
                ("certification".into(), false.into()),
                ("interpretation".into(), "metadata_only".into()),
            ]),
        );
        let file_id = builder.file_id.clone();
        builder.edge(&file_id, &simready_id, "contains", 1);
        for requirement in simready_requirements {
            let id = builder.reference_node("simready_requirement", &requirement, 1);
            builder.edge(&simready_id, &id, "declares_requirement", 1);
        }
    }
    Ok(builder.finish())
}

fn strip_usda_comment(line: &str) -> &str {
    let mut quote = None;
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\'' | b'"' => {
                if quote == Some(bytes[index]) {
                    quote = None;
                } else if quote.is_none() {
                    quote = Some(bytes[index]);
                }
            }
            b'#' if quote.is_none() => return &line[..index],
            _ => {}
        }
        index += 1;
    }
    line
}

fn usda_declaration(line: &str) -> Option<(&str, String, String)> {
    let mut words = line.split_whitespace();
    let specifier = words.next()?;
    if !matches!(specifier, "def" | "over" | "class") {
        return None;
    }
    let prim_type = words.next()?.trim_matches(['<', '>']).to_owned();
    let remainder = words.collect::<Vec<_>>().join(" ");
    let start = remainder.find('"')? + 1;
    let end = remainder[start..].find('"')? + start;
    let name = remainder[start..end].trim();
    (!name.is_empty()).then(|| (specifier, prim_type, name.into()))
}

fn count_usda_braces(line: &str) -> (usize, usize) {
    let mut opens = 0;
    let mut closes = 0;
    let mut quote = None;
    for byte in line.bytes() {
        match byte {
            b'\'' | b'"' if quote == Some(byte) => quote = None,
            b'\'' | b'"' if quote.is_none() => quote = Some(byte),
            b'{' if quote.is_none() => opens += 1,
            b'}' if quote.is_none() => closes += 1,
            _ => {}
        }
    }
    (opens, closes)
}

fn usda_asset_references(line: &str) -> Vec<String> {
    let mut references = Vec::new();
    let mut remaining = line;
    while let Some(start) = remaining.find('@') {
        if references.len() == MAX_REFERENCES_PER_LINE {
            break;
        }
        let tail = &remaining[start + 1..];
        let Some(end) = tail.find('@') else { break };
        let reference = tail[..end].trim();
        if !reference.is_empty() && reference.len() <= MAX_ATTRIBUTE_VALUE_BYTES {
            references.push(reference.into());
        }
        remaining = &tail[end + 1..];
    }
    references
}

fn usda_path_references(line: &str) -> Vec<String> {
    let mut references = Vec::new();
    let mut remaining = line;
    while let Some(start) = remaining.find('<') {
        if references.len() == MAX_REFERENCES_PER_LINE {
            break;
        }
        let tail = &remaining[start + 1..];
        let Some(end) = tail.find('>') else { break };
        let reference = tail[..end].trim();
        if reference.starts_with('/') && reference.len() <= MAX_ATTRIBUTE_VALUE_BYTES {
            references.push(reference.into());
        }
        remaining = &tail[end + 1..];
    }
    references
}

fn usd_kind(prim_type: &str) -> &'static str {
    match prim_type.to_ascii_lowercase().as_str() {
        "xform" | "scope" => "usd_prim",
        "mesh" | "basiscurves" | "points" => "mesh",
        "material" | "shader" | "nodegraph" => "material",
        "camera" => "camera",
        "light" | "distantlight" | "disklight" | "rectlight" | "spherelight" => "light",
        "rigidbody" | "collision" | "physicsmaterial" | "articulationrootapi" => "physics",
        "joint" | "revolutejoint" | "prismaticjoint" | "fixedjoint" => "joint",
        "sensor" => "sensor",
        _ => "usd_prim",
    }
}

fn extract_gltf(path: &Path, source_file: &str, bytes: &[u8]) -> anyhow::Result<Extraction> {
    ensure_json_depth(bytes)?;
    let value: serde_json::Value =
        serde_json::from_slice(bytes).with_context(|| format!("parse glTF JSON {source_file}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("glTF root must be an object"))?;
    let mut builder = Builder::new(path, source_file, SimulationFormat::Gltf);
    mark_gltf_truncation(&mut builder, object);
    let model_id = builder.owned_node(
        "gltf_asset",
        object
            .get("asset")
            .and_then(serde_json::Value::as_object)
            .and_then(|asset| asset.get("generator"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("glTF asset"),
        1,
        BTreeMap::new(),
    );
    let file_id = builder.file_id.clone();
    builder.edge(&file_id, &model_id, "contains", 1);

    let mut images = Vec::new();
    for (index, image) in array(object, "images")
        .iter()
        .take(MAX_GLTF_ITEMS)
        .enumerate()
    {
        let label = name_or(image, "image", index);
        let id = builder.owned_node("image", &label, 1, BTreeMap::new());
        builder.edge(&model_id, &id, "contains", 1);
        if let Some(uri) = string(image, "uri") {
            let reference = builder.reference_node("asset_reference", uri, 1);
            builder.edge(&id, &reference, "references", 1);
        }
        images.push(id);
    }

    let mut textures = Vec::new();
    for (index, texture) in array(object, "textures")
        .iter()
        .take(MAX_GLTF_ITEMS)
        .enumerate()
    {
        let label = name_or(texture, "texture", index);
        let id = builder.owned_node("texture", &label, 1, BTreeMap::new());
        builder.edge(&model_id, &id, "contains", 1);
        if let Some(source) = index_value(texture, "source").and_then(|index| images.get(index)) {
            builder.edge(&id, source, "references", 1);
        }
        textures.push(id);
    }

    let mut materials = Vec::new();
    for (index, material) in array(object, "materials")
        .iter()
        .take(MAX_GLTF_ITEMS)
        .enumerate()
    {
        let label = name_or(material, "material", index);
        let id = builder.owned_node("material", &label, 1, BTreeMap::new());
        builder.edge(&model_id, &id, "contains", 1);
        for texture_index in material_texture_indices(material) {
            if let Some(texture) = textures.get(texture_index) {
                builder.edge(&id, texture, "uses_texture", 1);
            }
        }
        materials.push(id);
    }

    let mut meshes = Vec::new();
    for (index, mesh) in array(object, "meshes")
        .iter()
        .take(MAX_GLTF_ITEMS)
        .enumerate()
    {
        let label = name_or(mesh, "mesh", index);
        let id = builder.owned_node("mesh", &label, 1, BTreeMap::new());
        builder.edge(&model_id, &id, "contains", 1);
        for primitive in array_value(mesh, "primitives").iter().take(MAX_GLTF_ITEMS) {
            if let Some(material) =
                index_value(primitive, "material").and_then(|index| materials.get(index))
            {
                builder.edge(&id, material, "uses_material", 1);
            }
        }
        meshes.push(id);
    }

    let mut cameras = Vec::new();
    for (index, camera) in array(object, "cameras")
        .iter()
        .take(MAX_GLTF_ITEMS)
        .enumerate()
    {
        let label = name_or(camera, "camera", index);
        let id = builder.owned_node("camera", &label, 1, BTreeMap::new());
        builder.edge(&model_id, &id, "contains", 1);
        cameras.push(id);
    }

    let mut nodes = Vec::new();
    for (index, node) in array(object, "nodes")
        .iter()
        .take(MAX_GLTF_ITEMS)
        .enumerate()
    {
        let label = name_or(node, "node", index);
        let mut extra = BTreeMap::new();
        if let Some(transform) = transform_summary(node) {
            extra.insert("transform".into(), transform.into());
        }
        let id = builder.owned_node("scene_node", &label, 1, extra);
        builder.edge(&model_id, &id, "contains", 1);
        if let Some(mesh) = index_value(node, "mesh").and_then(|index| meshes.get(index)) {
            builder.edge(&id, mesh, "uses_mesh", 1);
        }
        if let Some(camera) = index_value(node, "camera").and_then(|index| cameras.get(index)) {
            builder.edge(&id, camera, "uses_camera", 1);
        }
        nodes.push(id);
    }
    for (index, node) in array(object, "nodes")
        .iter()
        .take(MAX_GLTF_ITEMS)
        .enumerate()
    {
        let Some(source) = nodes.get(index).cloned() else {
            continue;
        };
        for child in indices(node, "children") {
            if let Some(target) = nodes.get(child) {
                builder.edge(&source, target, "contains", 1);
            }
        }
    }

    for (index, scene) in array(object, "scenes")
        .iter()
        .take(MAX_GLTF_ITEMS)
        .enumerate()
    {
        let id = builder.owned_node("scene", &name_or(scene, "scene", index), 1, BTreeMap::new());
        builder.edge(&model_id, &id, "contains", 1);
        for node in indices(scene, "nodes") {
            if let Some(target) = nodes.get(node) {
                builder.edge(&id, target, "contains", 1);
            }
        }
    }
    for buffer in array(object, "buffers").iter().take(MAX_GLTF_ITEMS) {
        if let Some(uri) = string(buffer, "uri") {
            let reference = builder.reference_node("asset_reference", uri, 1);
            builder.edge(&model_id, &reference, "references", 1);
        }
    }
    if contains_simready(&value) {
        let id = builder.owned_node(
            "simready_metadata",
            "SimReady metadata (unverified)",
            1,
            BTreeMap::from([
                ("certification".into(), false.into()),
                ("interpretation".into(), "metadata_only".into()),
            ]),
        );
        builder.edge(&model_id, &id, "contains", 1);
    }
    Ok(builder.finish())
}

fn ensure_json_depth(bytes: &[u8]) -> anyhow::Result<()> {
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for byte in bytes {
        if quoted {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match byte {
            b'"' => quoted = true,
            b'{' | b'[' => {
                depth += 1;
                anyhow::ensure!(depth <= MAX_JSON_DEPTH, "glTF JSON exceeds nesting limit");
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

fn mark_gltf_truncation(
    builder: &mut Builder<'_>,
    object: &serde_json::Map<String, serde_json::Value>,
) {
    let limited = [
        "images",
        "textures",
        "materials",
        "meshes",
        "cameras",
        "nodes",
        "scenes",
        "buffers",
    ]
    .iter()
    .any(|key| array(object, key).len() > MAX_GLTF_ITEMS);
    if limited && let Some(file) = builder.nodes.first_mut() {
        file.extra.insert("truncated".into(), true.into());
        file.extra.insert(
            "diagnostic".into(),
            format!("glTF collections are limited to {MAX_GLTF_ITEMS} entries per collection")
                .into(),
        );
    }
}

fn array<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> &'a [serde_json::Value] {
    object
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn array_value<'a>(value: &'a serde_json::Value, key: &str) -> &'a [serde_json::Value] {
    value
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn name_or(value: &serde_json::Value, prefix: &str, index: usize) -> String {
    value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{prefix}-{index}"))
}

fn string<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str().filter(|value| !value.is_empty())
}

fn index_value(value: &serde_json::Value, key: &str) -> Option<usize> {
    value.get(key)?.as_u64()?.try_into().ok()
}

fn indices<'a>(value: &'a serde_json::Value, key: &str) -> impl Iterator<Item = usize> + 'a {
    value
        .get(key)
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_u64)
        .filter_map(|index| index.try_into().ok())
}

fn material_texture_indices(material: &serde_json::Value) -> Vec<usize> {
    let mut indices = Vec::new();
    for key in [
        "normalTexture",
        "occlusionTexture",
        "emissiveTexture",
        "baseColorTexture",
        "metallicRoughnessTexture",
    ] {
        let value = if matches!(key, "baseColorTexture" | "metallicRoughnessTexture") {
            material
                .get("pbrMetallicRoughness")
                .and_then(|value| value.get(key))
        } else {
            material.get(key)
        };
        if let Some(index) = value.and_then(|value| index_value(value, "index")) {
            indices.push(index);
        }
    }
    indices.sort_unstable();
    indices.dedup();
    indices
}

fn transform_summary(node: &serde_json::Value) -> Option<String> {
    let object = node.as_object()?;
    let mut fields = Vec::new();
    for key in ["translation", "rotation", "scale", "matrix"] {
        if let Some(value) = object.get(key) {
            fields.push(format!("{key}={value}"));
        }
    }
    (!fields.is_empty()).then(|| fields.join(";"))
}

fn contains_simready(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(object) => object.iter().any(|(key, value)| {
            key.to_ascii_lowercase().contains("simready") || contains_simready(value)
        }),
        serde_json::Value::Array(values) => values.iter().any(contains_simready),
        serde_json::Value::String(value) => value.to_ascii_lowercase().contains("simready"),
        _ => false,
    }
}

#[derive(Clone)]
struct XmlScope {
    tag: String,
    owner: String,
}

fn extract_xml(
    format: SimulationFormat,
    path: &Path,
    source_file: &str,
    bytes: &[u8],
) -> anyhow::Result<Extraction> {
    reject_unsafe_xml(bytes)?;
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut builder = Builder::new(path, source_file, format);
    let mut stack = Vec::<XmlScope>::new();
    let mut event_count = 0usize;
    let mut simready = false;
    loop {
        event_count += 1;
        anyhow::ensure!(
            event_count <= MAX_XML_EVENTS,
            "simulation XML exceeds event limit"
        );
        match reader.read_event().context("parse simulation XML")? {
            Event::Start(event) => {
                anyhow::ensure!(
                    stack.len() < MAX_XML_DEPTH,
                    "simulation XML exceeds nesting limit"
                );
                let tag = local_name(event.name().as_ref());
                let attributes = attributes(&event)?;
                let line = line_of(bytes, reader.buffer_position() as usize);
                let owner = process_xml_element(
                    format,
                    XmlElement {
                        tag: &tag,
                        attributes: &attributes,
                        parent: stack.last().map(|scope| scope.owner.as_str()),
                        line,
                    },
                    &mut builder,
                    &mut simready,
                );
                stack.push(XmlScope { tag, owner });
            }
            Event::Empty(event) => {
                let tag = local_name(event.name().as_ref());
                let attributes = attributes(&event)?;
                let line = line_of(bytes, reader.buffer_position() as usize);
                process_xml_element(
                    format,
                    XmlElement {
                        tag: &tag,
                        attributes: &attributes,
                        parent: stack.last().map(|scope| scope.owner.as_str()),
                        line,
                    },
                    &mut builder,
                    &mut simready,
                );
            }
            Event::End(event) => {
                let name = local_name(event.name().as_ref());
                let Some(scope) = stack.pop() else {
                    anyhow::bail!("simulation XML has an unexpected closing element {name}");
                };
                anyhow::ensure!(
                    scope.tag == name,
                    "simulation XML has mismatched closing element {name}"
                );
            }
            Event::DocType(_) | Event::GeneralRef(_) => {
                anyhow::bail!("simulation XML cannot contain DTD or entity references")
            }
            Event::Eof => break,
            _ => {}
        }
    }
    anyhow::ensure!(
        stack.is_empty(),
        "simulation XML ended with unclosed elements"
    );
    if simready {
        let id = builder.owned_node(
            "simready_metadata",
            "SimReady metadata (unverified)",
            1,
            BTreeMap::from([
                ("certification".into(), false.into()),
                ("interpretation".into(), "metadata_only".into()),
            ]),
        );
        let file_id = builder.file_id.clone();
        builder.edge(&file_id, &id, "contains", 1);
    }
    Ok(builder.finish())
}

fn reject_unsafe_xml(bytes: &[u8]) -> anyhow::Result<()> {
    let lower = bytes.to_ascii_lowercase();
    anyhow::ensure!(
        !lower.windows(9).any(|window| window == b"<!doctype")
            && !lower.windows(8).any(|window| window == b"<!entity"),
        "simulation XML with DOCTYPE/ENTITY declarations is refused"
    );
    Ok(())
}

fn local_name(name: &[u8]) -> String {
    String::from_utf8_lossy(
        name.rsplit(|byte| matches!(byte, b':' | b'}'))
            .next()
            .unwrap_or(name),
    )
    .to_ascii_lowercase()
}

fn attributes(
    event: &quick_xml::events::BytesStart<'_>,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for attribute in event.attributes().with_checks(false).take(128) {
        let attribute = attribute.context("parse simulation XML attribute")?;
        let key = local_name(attribute.key.as_ref());
        let value = crate::decode_xml_attribute(attribute.value.as_ref())
            .context("decode simulation XML attribute")?
            .into_owned();
        values.insert(key, truncate(&value, MAX_ATTRIBUTE_VALUE_BYTES));
    }
    Ok(values)
}

struct XmlElement<'a> {
    tag: &'a str,
    attributes: &'a BTreeMap<String, String>,
    parent: Option<&'a str>,
    line: usize,
}

fn process_xml_element(
    format: SimulationFormat,
    element: XmlElement<'_>,
    builder: &mut Builder<'_>,
    simready: &mut bool,
) -> String {
    if element.tag.contains("simready")
        || element.attributes.iter().any(|(key, value)| {
            key.contains("simready") || value.to_ascii_lowercase().contains("simready")
        })
    {
        *simready = true;
    }
    let parent = element.parent.unwrap_or(&builder.file_id).to_owned();
    let kind = xml_kind(format, element.tag);
    let owner = if let Some(kind) = kind {
        let label = xml_label(element.tag, element.attributes);
        let id = builder.owned_node(
            kind,
            &label,
            element.line,
            xml_extra(format, element.tag, element.attributes),
        );
        builder.edge(&parent, &id, "contains", element.line);
        id
    } else {
        parent
    };
    for (key, value) in element.attributes {
        if (is_reference_attribute(key)
            || matches!((element.tag, key.as_str()), ("parent" | "child", "link")))
            && is_reference_value(value)
        {
            let target = builder.reference_node(reference_kind(key), value, element.line);
            let relation = relation_for_attribute(element.tag, key);
            builder.edge(&owner, &target, relation, element.line);
        }
    }
    owner
}

fn xml_kind(format: SimulationFormat, tag: &str) -> Option<&'static str> {
    match format {
        SimulationFormat::MaterialX => match tag {
            "material" | "surfacematerial" => Some("material"),
            "nodegraph" | "nodedef" | "implementation" => Some("material_graph"),
            "shaderref" | "shader" | "node" => Some("shader"),
            "geominfo" | "geomprop" => Some("geometry"),
            "look" | "lookgroup" => Some("material_look"),
            _ => None,
        },
        SimulationFormat::Urdf => match tag {
            "robot" => Some("robot"),
            "link" => Some("link"),
            "joint" => Some("joint"),
            "material" => Some("material"),
            "visual" => Some("visual"),
            "collision" => Some("collision"),
            "sensor" | "camera" | "imu" | "gazebo" => Some("sensor"),
            "transmission" | "actuator" => Some("actuator"),
            _ => None,
        },
        SimulationFormat::Sdf => match tag {
            "sdf" | "world" | "model" => Some("simulation_model"),
            "link" => Some("link"),
            "joint" => Some("joint"),
            "sensor" | "camera" | "imu" | "lidar" | "ray" => Some("sensor"),
            "visual" => Some("visual"),
            "collision" => Some("collision"),
            "light" => Some("light"),
            "actor" => Some("actor"),
            _ => None,
        },
        SimulationFormat::Mjcf => match tag {
            "mujoco" => Some("simulation_model"),
            "body" => Some("body"),
            "joint" | "freejoint" => Some("joint"),
            "geom" | "site" => Some("geometry"),
            "camera" | "sensor" | "touch" | "accelerometer" | "gyro" => Some("sensor"),
            "actuator" | "motor" | "position" | "velocity" => Some("actuator"),
            "material" | "texture" | "mesh" => Some("asset"),
            _ => None,
        },
        SimulationFormat::FmiModelDescription => match tag {
            "fmimodeldescription" => Some("fmi_model"),
            "modelexchange" | "cosimulation" | "scheduledexecution" => Some("fmi_interface"),
            "scalarvariable" | "float64" | "float32" | "int32" | "int64" | "boolean" | "string"
            | "enumeration" => Some("fmi_variable"),
            "unknown" | "output" | "initialunknown" => Some("fmi_model_structure"),
            _ => None,
        },
        SimulationFormat::OpenDrive => match tag {
            "opendrive" => Some("road_network"),
            "road" => Some("road"),
            "junction" => Some("junction"),
            "controller" => Some("controller"),
            "signal" => Some("signal"),
            "object" => Some("road_object"),
            "lane" => Some("lane"),
            "geometry" => Some("road_geometry"),
            _ => None,
        },
        SimulationFormat::OpenScenario => match tag {
            "openscenario" => Some("scenario"),
            "scenarioobject" => Some("scenario_entity"),
            "vehicle" | "pedestrian" | "miscobject" => Some("scenario_asset"),
            "storyboard" | "story" | "act" | "maneuvergroup" | "maneuver" | "event" | "action" => {
                Some("scenario_flow")
            }
            "catalog" => Some("catalog"),
            "roadnetwork" => Some("road_network"),
            _ => None,
        },
        _ => None,
    }
}

fn xml_label(tag: &str, attributes: &BTreeMap<String, String>) -> String {
    for key in [
        "name",
        "id",
        "modelidentifier",
        "instantiationtoken",
        "type",
        "value",
    ] {
        if let Some(value) = attributes.get(key).filter(|value| !value.is_empty()) {
            return value.clone();
        }
    }
    tag.into()
}

fn xml_extra(
    format: SimulationFormat,
    tag: &str,
    attributes: &BTreeMap<String, String>,
) -> BTreeMap<String, serde_json::Value> {
    let mut extra = BTreeMap::from([
        ("format".into(), format.name().into()),
        ("element".into(), tag.into()),
    ]);
    for key in [
        "type",
        "value_reference",
        "mass",
        "axis",
        "pose",
        "xyz",
        "rpy",
    ] {
        if let Some(value) = attributes.get(key) {
            extra.insert(key.into(), value.clone().into());
        }
    }
    extra
}

fn is_reference_attribute(key: &str) -> bool {
    matches!(
        key,
        "filename"
            | "file"
            | "filepath"
            | "uri"
            | "url"
            | "href"
            | "src"
            | "asset"
            | "texture"
            | "mesh"
            | "material"
            | "parent"
            | "child"
            | "body"
            | "joint"
            | "node"
            | "target"
            | "source"
            | "entityref"
            | "catalogname"
            | "entryname"
    )
}

fn is_reference_value(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_ATTRIBUTE_VALUE_BYTES && !value.starts_with("data:")
}

fn reference_kind(key: &str) -> &'static str {
    match key {
        "filename" | "file" | "filepath" | "uri" | "url" | "href" | "src" => "asset_reference",
        "material" | "texture" | "mesh" => "asset_reference",
        "parent" | "child" | "body" | "joint" | "node" | "link" => "entity_reference",
        _ => "model_reference",
    }
}

fn relation_for_attribute(tag: &str, key: &str) -> &'static str {
    match (tag, key) {
        (_, "material") => "uses_material",
        (_, "texture") => "uses_texture",
        (_, "mesh") => "uses_mesh",
        (_, "parent" | "child" | "body" | "joint" | "node" | "link") => "relates_to",
        (_, "entityref" | "catalogname" | "entryname") => "references",
        _ => "references",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn types(extraction: &Extraction) -> Vec<&str> {
        extraction
            .nodes
            .iter()
            .filter_map(|node| node.extra.get("type")?.as_str())
            .collect()
    }

    #[test]
    fn usda_extracts_prims_references_and_unverified_simready_metadata() {
        let extraction = extract_simulation_bytes(
            Path::new("scene.usda"),
            "scene.usda",
            br#"#usda 1.0
def Xform "World" {
  def Mesh "Crate" {
    rel material:binding = </World/Materials/Steel>
    asset inputs:albedo = @textures/crate.png@
    bool physics:rigidBodyEnabled = true
  }
}
string simready:assetType = "prop"
"#,
        )
        .expect("valid USDA");
        let kinds = types(&extraction);
        assert!(kinds.contains(&"usd_prim"));
        assert!(kinds.contains(&"mesh"));
        assert!(kinds.contains(&"simready_metadata"));
        assert!(extraction
            .edges
            .iter()
            .any(|edge| edge.relation == "uses_material"));
        assert!(extraction
            .edges
            .iter()
            .any(|edge| edge.relation == "references"));
        let simready = extraction
            .nodes
            .iter()
            .find(|node| {
                node.extra.get("type").and_then(serde_json::Value::as_str)
                    == Some("simready_metadata")
            })
            .expect("SimReady node");
        assert_eq!(
            simready
                .extra
                .get("certification")
                .and_then(serde_json::Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn gltf_extracts_scene_mesh_material_texture_and_transform_relationships() {
        let extraction = extract_simulation_bytes(
            Path::new("robot.gltf"),
            "robot.gltf",
            br#"{
              "asset":{"version":"2.0","generator":"test"},
              "images":[{"uri":"robot.png"}],
              "textures":[{"source":0}],
              "materials":[{"name":"paint","pbrMetallicRoughness":{"baseColorTexture":{"index":0}}}],
              "meshes":[{"name":"body","primitives":[{"material":0}]}],
              "nodes":[{"name":"base","mesh":0,"translation":[1,2,3],"children":[1]},{"name":"sensor"}],
              "scenes":[{"nodes":[0]}]
            }"#,
        )
        .expect("valid glTF");
        let kinds = types(&extraction);
        for expected in [
            "gltf_asset",
            "image",
            "texture",
            "material",
            "mesh",
            "scene_node",
            "scene",
        ] {
            assert!(kinds.contains(&expected), "missing {expected}");
        }
        assert!(extraction
            .edges
            .iter()
            .any(|edge| edge.relation == "uses_material"));
        assert!(extraction
            .edges
            .iter()
            .any(|edge| edge.relation == "uses_texture"));
        assert!(extraction
            .edges
            .iter()
            .any(|edge| edge.relation == "contains"));
    }

    #[test]
    fn xml_models_extract_entities_physics_and_external_references() {
        let extraction = extract_simulation_bytes(
            Path::new("robot.urdf"),
            "robot.urdf",
            br#"<robot name="cart"><link name="base"><visual><geometry><mesh filename="meshes/base.dae"/></geometry></visual><collision/></link><joint name="wheel" type="continuous"><parent link="base"/><child link="wheel_link"/></joint><gazebo><sensor name="lidar" type="ray"/></gazebo></robot>"#,
        )
        .expect("valid URDF");
        let kinds = types(&extraction);
        for expected in ["robot", "link", "joint", "sensor", "collision"] {
            assert!(kinds.contains(&expected), "missing {expected}");
        }
        assert!(extraction
            .edges
            .iter()
            .any(|edge| edge.relation == "references"));
        assert!(extraction
            .edges
            .iter()
            .any(|edge| edge.relation == "relates_to"));
    }

    #[test]
    fn inventory_formats_never_claim_semantic_decoding() {
        let extraction = extract_simulation_bytes(
            Path::new("scene.usdz"),
            "scene.usdz",
            b"PK\x03\x04not-an-inspected-archive",
        )
        .expect("inventory result");
        assert_eq!(extraction.nodes.len(), 1);
        let node = &extraction.nodes[0];
        assert_eq!(
            node.extra
                .get("format_capability")
                .and_then(serde_json::Value::as_str),
            Some("inventory_only")
        );
        assert_eq!(
            node.extra
                .get("semantic_extraction")
                .and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert!(node
            .extra
            .get("diagnostic")
            .and_then(serde_json::Value::as_str)
            .is_some());
    }

    #[test]
    fn refuses_unsafe_xml_before_parsing() {
        let error = extract_simulation_bytes(
            Path::new("robot.urdf"),
            "robot.urdf",
            b"<!DOCTYPE robot [<!ENTITY boom 'boom'>]><robot name='x' />",
        )
        .expect_err("DOCTYPE must be rejected");
        assert!(error.to_string().contains("DOCTYPE/ENTITY"));
    }

    #[test]
    fn model_description_name_is_recognized_without_claiming_generic_xml() {
        assert_eq!(
            format_for_path(Path::new("modelDescription.xml")),
            Some(SimulationFormat::FmiModelDescription)
        );
        assert_eq!(format_for_path(Path::new("unrelated.xml")), None);
    }

    #[test]
    fn remaining_xml_simulation_families_emit_domain_nodes_and_references() {
        let cases = [
            (
                "material.mtlx",
                br#"<materialx><nodegraph name="surface"><image name="albedo" file="albedo.png"/><surfacematerial name="paint"/></nodegraph></materialx>"#
                    .as_slice(),
                "material_graph",
            ),
            (
                "model.sdf",
                br#"<sdf version="1.9"><model name="rig"><link name="base"><sensor name="imu" type="imu"/></link><joint name="hinge" type="revolute"/></model></sdf>"#
                    .as_slice(),
                "simulation_model",
            ),
            (
                "model.mjcf",
                br#"<mujoco model="arm"><asset><mesh name="part" file="part.obj"/></asset><worldbody><body name="base"><joint name="axis"/><camera name="eye"/></body></worldbody></mujoco>"#
                    .as_slice(),
                "body",
            ),
            (
                "modelDescription.xml",
                br#"<fmiModelDescription modelName="plant" modelIdentifier="plant"><CoSimulation modelIdentifier="plant"/><ModelVariables><ScalarVariable name="speed" valueReference="1"><Float64/></ScalarVariable></ModelVariables></fmiModelDescription>"#
                    .as_slice(),
                "fmi_model",
            ),
            (
                "roads.xodr",
                br#"<OpenDRIVE><road name="main" id="1"><lanes><laneSection><left><lane id="1" type="driving"/></left></laneSection></lanes></road><junction name="J" id="2"/></OpenDRIVE>"#
                    .as_slice(),
                "road",
            ),
            (
                "scenario.xosc",
                br#"<OpenSCENARIO><RoadNetwork><LogicFile filepath="roads.xodr"/></RoadNetwork><Entities><ScenarioObject name="ego"><Vehicle name="car"/></ScenarioObject></Entities><Storyboard><Story name="drive"><Act name="act"/></Story></Storyboard></OpenSCENARIO>"#
                    .as_slice(),
                "scenario",
            ),
        ];
        for (path, input, expected_kind) in cases {
            let extraction = extract_simulation_bytes(Path::new(path), path, input)
                .unwrap_or_else(|error| panic!("{path}: {error:#}"));
            assert!(
                types(&extraction).contains(&expected_kind),
                "{path} missing {expected_kind}"
            );
        }
    }

    #[test]
    fn gltf_depth_limit_is_checked_before_deserialization() {
        let mut input = Vec::new();
        input.extend(std::iter::repeat_n(b'[', MAX_JSON_DEPTH + 1));
        input.extend(std::iter::repeat_n(b']', MAX_JSON_DEPTH + 1));
        let error = extract_simulation_bytes(Path::new("deep.gltf"), "deep.gltf", &input)
            .expect_err("deep JSON must be rejected");
        assert!(error.to_string().contains("nesting limit"));
    }
}
