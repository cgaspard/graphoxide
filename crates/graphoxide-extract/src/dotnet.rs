//! Structural extractors for .NET solution, project, XAML, and Razor files.
//!
//! These formats do not have tree-sitter grammars in the compiled registry.
//! Keep their extraction in one place so XML validation, portable identities,
//! and the XAML-to-C# bridge share the same corpus boundary.

use crate::project_path::{normalize_project_path, source_relative_project_path, ProjectPath};
use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use quick_xml::{events::Event, Reader};
use regex::Regex;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

const PROJECT_XML_MAX_BYTES: usize = 2 * 1024 * 1024;
const XAML_DIRECTORY_CAP: usize = 20_000;
const DOTNET_EXTENSIONS: &[&str] = &[
    "sln", "slnx", "csproj", "fsproj", "vbproj", "xaml", "razor", "cshtml",
];

pub(crate) fn supports_extension(extension: &str) -> bool {
    DOTNET_EXTENSIONS.contains(&extension)
}

pub(crate) fn extract_dotnet(
    path: &Path,
    source_file: &str,
    extension: &str,
) -> anyhow::Result<Extraction> {
    let bytes = fs::read(path)?;
    extract_dotnet_with_path_probes(path, source_file, extension, &bytes, true)
}

/// Extract a .NET-adjacent document from already-read source bytes.
///
/// The path is retained solely for stable identities and existing project
/// resolution. The primary document is never read by this entry point.
pub(crate) fn extract_dotnet_bytes(
    path: &Path,
    source_file: &str,
    extension: &str,
    bytes: &[u8],
) -> anyhow::Result<Extraction> {
    extract_dotnet_with_path_probes(path, source_file, extension, bytes, false)
}

/// Extract a .NET document with explicit control over cross-file probes.
/// Legacy path entrypoints retain the existing project-aware enrichment while
/// I/O-isolated CPU work is restricted to the admitted document bytes.
pub(crate) fn extract_dotnet_with_path_probes(
    path: &Path,
    source_file: &str,
    extension: &str,
    bytes: &[u8],
    allow_path_probes: bool,
) -> anyhow::Result<Extraction> {
    match extension {
        "sln" => extract_sln(path, source_file, bytes, allow_path_probes),
        "slnx" => extract_slnx(path, source_file, bytes, allow_path_probes),
        "csproj" | "fsproj" | "vbproj" => {
            extract_project(path, source_file, bytes, allow_path_probes)
        }
        "xaml" => extract_xaml(path, source_file, bytes, allow_path_probes),
        "razor" | "cshtml" => extract_razor(path, source_file, bytes),
        _ => anyhow::bail!("unsupported .NET extension: {extension}"),
    }
}

struct Builder<'a> {
    source_file: &'a str,
    stem: String,
    file_id: String,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    seen_nodes: HashSet<String>,
    seen_edges: HashSet<(String, String, String, String)>,
}

impl<'a> Builder<'a> {
    fn new(path: &Path, source_file: &'a str) -> Self {
        let stem = Path::new(source_file)
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let file_id = make_id(&[&stem]);
        let mut value = Self {
            source_file,
            stem,
            file_id: file_id.clone(),
            nodes: Vec::new(),
            edges: Vec::new(),
            seen_nodes: HashSet::new(),
            seen_edges: HashSet::new(),
        };
        value.node(
            file_id,
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(source_file),
            "file",
            "code",
            source_file,
            1,
        );
        value
    }

    fn node(
        &mut self,
        id: String,
        label: &str,
        kind: &str,
        file_type: &str,
        source_file: &str,
        line: usize,
    ) -> String {
        if !id.is_empty() && self.seen_nodes.insert(id.clone()) {
            self.nodes.push(Node {
                id: id.clone(),
                label: label.to_owned(),
                file_type: file_type.to_owned(),
                source_file: source_file.to_owned(),
                source_location: Some(format!("L{line}")),
                community: None,
                extra: BTreeMap::from([
                    ("_origin".into(), "dotnet".into()),
                    ("type".into(), kind.into()),
                ]),
            });
        }
        id
    }

    fn existing_node(&mut self, node: &Node) {
        if self.seen_nodes.insert(node.id.clone()) {
            self.nodes.push(node.clone());
        }
    }

    fn edge(
        &mut self,
        source: &str,
        target: &str,
        relation: &str,
        context: Option<&str>,
        confidence: Confidence,
        line: usize,
    ) {
        self.edge_from(
            source,
            target,
            relation,
            context,
            confidence,
            self.source_file,
            line,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn edge_from(
        &mut self,
        source: &str,
        target: &str,
        relation: &str,
        context: Option<&str>,
        confidence: Confidence,
        source_file: &str,
        line: usize,
    ) {
        let context_key = context.unwrap_or_default().to_owned();
        if !self.seen_edges.insert((
            source.to_owned(),
            target.to_owned(),
            relation.to_owned(),
            context_key,
        )) {
            return;
        }
        let mut extra = BTreeMap::from([
            ("_src".into(), source.into()),
            ("_tgt".into(), target.into()),
            ("source_location".into(), format!("L{line}").into()),
            ("weight".into(), 1.0.into()),
        ]);
        if let Some(context) = context {
            extra.insert("context".into(), context.into());
        }
        self.edges.push(Edge {
            source: source.to_owned(),
            target: target.to_owned(),
            relation: relation.to_owned(),
            confidence,
            source_file: source_file.to_owned(),
            extra,
        });
    }

    fn existing_edge(&mut self, edge: &Edge) {
        let context = edge
            .extra
            .get("context")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if self.seen_edges.insert((
            edge.true_source().to_owned(),
            edge.true_target().to_owned(),
            edge.relation.clone(),
            context,
        )) {
            self.edges.push(edge.clone());
        }
    }

    fn finish(self) -> Extraction {
        Extraction {
            nodes: self.nodes,
            edges: self.edges,
            hyperedges: Vec::new(),
        }
    }
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].bytes().filter(|byte| *byte == b'\n').count() + 1
}

fn lossy_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn validate_xml(bytes: &[u8]) -> anyhow::Result<()> {
    anyhow::ensure!(
        bytes.len() <= PROJECT_XML_MAX_BYTES,
        "project XML is larger than {PROJECT_XML_MAX_BYTES} bytes"
    );
    let lowercase = bytes.to_ascii_lowercase();
    anyhow::ensure!(
        !lowercase.windows(9).any(|window| window == b"<!doctype")
            && !lowercase.windows(8).any(|window| window == b"<!entity"),
        "refusing XML with DOCTYPE/ENTITY declaration"
    );
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(false);
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(anyhow::anyhow!("XML parse error: {error}")),
        }
    }
    Ok(())
}

fn normalize_logical_path(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let absolute = text.starts_with('/');
    let mut parts = Vec::new();
    for component in text.split('/') {
        match component {
            "" | "." => {}
            ".." if parts.last().is_some_and(|part| *part != "..") => {
                parts.pop();
            }
            ".." if !absolute => parts.push(component),
            ".." => {}
            _ => parts.push(component),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

fn looks_absolute_path(path: &str) -> bool {
    let path = path.replace('\\', "/");
    path.starts_with('/')
        || path
            .as_bytes()
            .get(1..3)
            .is_some_and(|bytes| bytes[0] == b':' && bytes[1] == b'/')
}

fn portable_project_reference_with_probes(
    physical_source: &Path,
    logical_source: &str,
    reference: &str,
    allow_path_probes: bool,
) -> Option<(String, bool)> {
    if !has_project_extension(reference) {
        return None;
    }
    let logical_source = normalize_project_path(logical_source)
        .filter(|source| !source.is_empty())
        .or_else(|| {
            allow_path_probes
                .then(|| {
                    physical_source
                        .file_name()
                        .and_then(|name| name.to_str())
                        .and_then(normalize_project_path)
                        .filter(|source| !source.is_empty())
                })
                .flatten()
        })?;
    match source_relative_project_path(&logical_source, reference)? {
        ProjectPath::Contained(logical) => Some((logical, false)),
        ProjectPath::EscapesRoot(logical)
            if allow_path_probes
                && physical_project_reference_is_file(physical_source, reference) =>
        {
            Some((logical, true))
        }
        ProjectPath::EscapesRoot(_) => None,
    }
}

fn has_project_extension(path: &str) -> bool {
    let filename = path.rsplit(['/', '\\']).next().unwrap_or(path);
    filename
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .is_some_and(|extension| {
            !extension.is_empty() && extension.to_ascii_lowercase().ends_with("proj")
        })
}

fn physical_project_reference_is_file(physical_source: &Path, reference: &str) -> bool {
    let Some(parent) = physical_source.parent() else {
        return false;
    };
    let mut candidate = parent.to_path_buf();
    for component in reference.split(['/', '\\']) {
        match component {
            "." => {}
            ".." => {
                if !candidate.pop() {
                    return false;
                }
            }
            component => candidate.push(component),
        }
    }
    fs::canonicalize(candidate).is_ok_and(|candidate| candidate.is_file())
}

fn portable_project_id(logical_path: &str, external: bool) -> String {
    if external {
        make_id(&["ext", logical_path])
    } else {
        let stem = Path::new(logical_path)
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        make_id(&[&stem])
    }
}

fn extract_sln(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
    allow_path_probes: bool,
) -> anyhow::Result<Extraction> {
    let text = lossy_text(bytes);
    let mut builder = Builder::new(path, source_file);
    let project = Regex::new(
        r#"(?m)^Project\("[^"]*"\)\s*=\s*"([^"]+)"\s*,\s*"([^"]+)"\s*,\s*"\{?([^"}]+)\}?""#,
    )?;
    let mut guid_to_id = HashMap::new();
    for capture in project.captures_iter(&text) {
        let name = &capture[1];
        let Some(relative) = static_msbuild_value(&capture[2]) else {
            continue;
        };
        let relative = relative.replace('\\', "/");
        let solution_folder = relative == name;
        let portable = if solution_folder {
            normalize_project_path(&relative)
                .filter(|logical| !logical.is_empty())
                .map(|logical| (logical, false))
        } else {
            portable_project_reference_with_probes(path, source_file, &relative, allow_path_probes)
        };
        let Some((logical, external)) = portable else {
            continue;
        };
        let id = if solution_folder {
            make_id(&[&logical])
        } else {
            portable_project_id(&logical, external)
        };
        let line = line_of(&text, capture.get(0).expect("solution project").start());
        builder.node(id.clone(), name, "project", "code", &logical, line);
        builder.edge(
            &builder.file_id.clone(),
            &id,
            "contains",
            None,
            Confidence::Extracted,
            line,
        );
        guid_to_id.insert(capture[3].to_ascii_lowercase(), id);
    }

    let project_line =
        Regex::new(r#"Project\("[^"]*"\)\s*=\s*"[^"]+"\s*,\s*"[^"]+"\s*,\s*"\{([^}]+)\}""#)?;
    let dependency = Regex::new(r"\{([0-9A-Fa-f-]+)\}\s*=\s*\{([0-9A-Fa-f-]+)\}")?;
    let mut current_project = None;
    let mut in_dependencies = false;
    for (line_index, line) in text.lines().enumerate() {
        if let Some(capture) = project_line.captures(line) {
            current_project = Some(capture[1].to_ascii_lowercase());
            continue;
        }
        if line.trim() == "EndProject" {
            current_project = None;
            continue;
        }
        if line.contains("ProjectSection(ProjectDependencies)") {
            in_dependencies = true;
            continue;
        }
        if line.contains("EndProjectSection") {
            in_dependencies = false;
            continue;
        }
        if !in_dependencies {
            continue;
        }
        let (Some(current), Some(capture)) = (current_project.as_ref(), dependency.captures(line))
        else {
            continue;
        };
        if let (Some(source), Some(target)) = (
            guid_to_id.get(current),
            guid_to_id.get(&capture[1].to_ascii_lowercase()),
        ) {
            builder.edge(
                source,
                target,
                "imports",
                None,
                Confidence::Extracted,
                line_index + 1,
            );
        }
    }
    Ok(builder.finish())
}

fn xml_local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|byte| matches!(byte, b':' | b'}'))
        .next()
        .unwrap_or(name)
}

fn event_attributes(
    event: &quick_xml::events::BytesStart<'_>,
) -> anyhow::Result<HashMap<String, String>> {
    let mut values = HashMap::new();
    for attribute in event.attributes() {
        let attribute = attribute?;
        let key = String::from_utf8_lossy(attribute.key.as_ref()).into_owned();
        let value = attribute.unescape_value()?.into_owned();
        values.insert(key, value);
    }
    Ok(values)
}

fn static_build_value(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty()
        || ["$(", "@(", "%("]
            .iter()
            .any(|marker| value.contains(marker))
        || value.contains('*')
        || value.contains('?')
    {
        return None;
    }
    Some(value)
}

fn static_msbuild_value(value: &str) -> Option<String> {
    let decoded = quick_xml::escape::unescape(value.trim()).ok()?;
    static_build_value(decoded.as_ref()).map(str::to_owned)
}

#[derive(Default)]
struct SolutionProject {
    path: String,
    dependencies: Vec<String>,
}

fn extract_slnx(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
    allow_path_probes: bool,
) -> anyhow::Result<Extraction> {
    validate_xml(bytes)?;
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut projects = Vec::new();
    let mut current: Option<SolutionProject> = None;
    loop {
        match reader.read_event()? {
            Event::Start(event) if xml_local_name(event.name().as_ref()) == b"Project" => {
                let attributes = event_attributes(&event)?;
                current = attributes
                    .get("Path")
                    .and_then(|path| static_msbuild_value(path))
                    .map(|path| SolutionProject {
                        path,
                        dependencies: Vec::new(),
                    });
            }
            Event::Empty(event) if xml_local_name(event.name().as_ref()) == b"Project" => {
                let attributes = event_attributes(&event)?;
                if let Some(path) = attributes
                    .get("Path")
                    .and_then(|path| static_msbuild_value(path))
                {
                    projects.push(SolutionProject {
                        path,
                        dependencies: Vec::new(),
                    });
                }
            }
            Event::Start(event) | Event::Empty(event)
                if xml_local_name(event.name().as_ref()) == b"BuildDependency" =>
            {
                if let (Some(project), Some(dependency)) =
                    (current.as_mut(), event_attributes(&event)?.get("Project"))
                    && let Some(dependency) = static_msbuild_value(dependency)
                {
                    project.dependencies.push(dependency);
                }
            }
            Event::End(event) if xml_local_name(event.name().as_ref()) == b"Project" => {
                if let Some(project) = current.take() {
                    projects.push(project);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    let mut builder = Builder::new(path, source_file);
    let mut by_path = HashMap::new();
    for project in &projects {
        let Some((logical, external)) = portable_project_reference_with_probes(
            path,
            source_file,
            &project.path,
            allow_path_probes,
        ) else {
            continue;
        };
        let id = portable_project_id(&logical, external);
        let normalized_path = project.path.replace('\\', "/");
        let label = Path::new(&normalized_path)
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or(&project.path);
        builder.node(id.clone(), label, "project", "code", &logical, 1);
        builder.edge(
            &builder.file_id.clone(),
            &id,
            "contains",
            None,
            Confidence::Extracted,
            1,
        );
        by_path.insert(logical.to_ascii_lowercase(), id);
    }
    for project in &projects {
        let Some((source_path, _)) = portable_project_reference_with_probes(
            path,
            source_file,
            &project.path,
            allow_path_probes,
        ) else {
            continue;
        };
        let Some(source) = by_path.get(&source_path.to_ascii_lowercase()) else {
            continue;
        };
        for dependency in &project.dependencies {
            let Some((dependency, _)) = portable_project_reference_with_probes(
                path,
                source_file,
                dependency,
                allow_path_probes,
            ) else {
                continue;
            };
            if let Some(target) = by_path.get(&dependency.to_ascii_lowercase()) {
                builder.edge(source, target, "imports", None, Confidence::Extracted, 1);
            }
        }
    }
    Ok(builder.finish())
}

fn xml_attribute(attributes: &str, name: &str) -> Option<String> {
    let pattern = format!(r#"(?i)\b{}\s*=\s*"([^"]*)""#, regex::escape(name));
    Regex::new(&pattern)
        .ok()?
        .captures(attributes)
        .map(|capture| capture[1].to_owned())
}

fn extract_project(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
    allow_path_probes: bool,
) -> anyhow::Result<Extraction> {
    validate_xml(bytes)?;
    let text = lossy_text(bytes);
    let mut builder = Builder::new(path, source_file);
    let framework = Regex::new(r"(?is)<TargetFrameworks?>\s*([^<]+?)\s*</TargetFrameworks?>")?;
    for capture in framework.captures_iter(&text) {
        for value in capture[1]
            .split(';')
            .map(str::trim)
            .filter_map(static_msbuild_value)
        {
            let id = make_id(&["framework", &value]);
            let line = line_of(&text, capture.get(0).expect("target framework").start());
            builder.node(
                id.clone(),
                &value,
                "framework",
                "concept",
                source_file,
                line,
            );
            builder.edge(
                &builder.file_id.clone(),
                &id,
                "references",
                None,
                Confidence::Extracted,
                line,
            );
        }
    }
    let element = Regex::new(r"(?is)<(PackageReference|ProjectReference)\b([^>]*)>")?;
    for capture in element.captures_iter(&text) {
        let attributes = &capture[2];
        let Some(include) = xml_attribute(attributes, "Include") else {
            continue;
        };
        let Some(include) = static_msbuild_value(&include) else {
            continue;
        };
        let line = line_of(&text, capture.get(0).expect("project reference").start());
        if capture[1].eq_ignore_ascii_case("PackageReference") {
            let label = xml_attribute(attributes, "Version")
                .and_then(|version| static_msbuild_value(&version))
                .map(|version| format!("{include} ({version})"))
                .unwrap_or_else(|| include.to_owned());
            let id = make_id(&["nuget", &include]);
            builder.node(id.clone(), &label, "package", "code", source_file, line);
            builder.edge(
                &builder.file_id.clone(),
                &id,
                "imports",
                None,
                Confidence::Extracted,
                line,
            );
        } else {
            let normalized = include.replace('\\', "/");
            let Some((logical, external)) = portable_project_reference_with_probes(
                path,
                source_file,
                &normalized,
                allow_path_probes,
            ) else {
                continue;
            };
            let id = portable_project_id(&logical, external);
            let label = normalized.rsplit('/').next().unwrap_or(&normalized);
            builder.node(id.clone(), label, "project", "code", &logical, line);
            builder.edge(
                &builder.file_id.clone(),
                &id,
                "imports",
                None,
                Confidence::Extracted,
                line,
            );
        }
    }
    let sdk = Regex::new(r#"(?is)<Project\b[^>]*\bSdk\s*=\s*"([^"]+)""#)?;
    if let Some(capture) = sdk.captures(&text) {
        let Some(value) = static_msbuild_value(&capture[1]) else {
            return Ok(builder.finish());
        };
        let id = make_id(&["sdk", &value]);
        let line = line_of(&text, capture.get(0).expect("project SDK").start());
        builder.node(id.clone(), &value, "sdk", "concept", source_file, line);
        builder.edge(
            &builder.file_id.clone(),
            &id,
            "references",
            None,
            Confidence::Extracted,
            line,
        );
    }
    Ok(builder.finish())
}

fn logical_sibling(source_file: &str, filename: &str) -> String {
    if looks_absolute_path(source_file) {
        normalize_logical_path(&Path::new(source_file).with_file_name(filename))
    } else {
        normalize_logical_path(
            &Path::new(source_file)
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join(filename),
        )
    }
}

fn codebehind_path(path: &Path) -> Option<PathBuf> {
    let expected_name = format!(
        "{}.cs",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
    );
    let expected = path.with_file_name(&expected_name);
    if expected.is_file() {
        return Some(expected);
    }
    fs::read_dir(path.parent()?)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|candidate| {
            candidate
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case(&expected_name))
        })
}

struct CodebehindSymbols {
    class: Option<Node>,
    handlers: HashMap<String, Node>,
    method_edges: HashMap<String, Edge>,
}

fn codebehind_symbols(
    path: &Path,
    source_file: &str,
    class_name: Option<&str>,
) -> anyhow::Result<CodebehindSymbols> {
    let Some(codebehind) = codebehind_path(path) else {
        return Ok(CodebehindSymbols {
            class: None,
            handlers: HashMap::new(),
            method_edges: HashMap::new(),
        });
    };
    let filename = codebehind
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let logical = logical_sibling(source_file, filename);
    let extraction = crate::engine::extract_as(&codebehind, &logical)?;
    let simple_class = class_name.and_then(|name| name.rsplit('.').next());
    let class = simple_class
        .and_then(|name| extraction.nodes.iter().find(|node| node.label == name))
        .cloned();
    let Some(class) = class else {
        return Ok(CodebehindSymbols {
            class: None,
            handlers: HashMap::new(),
            method_edges: HashMap::new(),
        });
    };
    let method_edges = extraction
        .edges
        .iter()
        .filter(|edge| edge.relation == "method" && edge.true_source() == class.id)
        .map(|edge| (edge.true_target().to_owned(), edge.clone()))
        .collect::<HashMap<_, _>>();
    let code = String::from_utf8_lossy(&fs::read(&codebehind)?).into_owned();
    let signature = Regex::new(
        r"(?s)([A-Za-z_][A-Za-z0-9_]*)\s*\(\s*object\??\s+[A-Za-z_]\w*\s*,\s*[A-Za-z0-9_.]*EventArgs(?:<[^>]*>)?\s+[A-Za-z_]\w*\s*\)",
    )?;
    let eligible = signature
        .captures_iter(&code)
        .map(|capture| capture[1].to_owned())
        .collect::<HashSet<_>>();
    let handlers = extraction
        .nodes
        .iter()
        .filter(|node| method_edges.contains_key(&node.id))
        .filter_map(|node| {
            let name = node.label.trim().trim_matches(['.', '(', ')']);
            eligible
                .contains(name)
                .then(|| (name.to_owned(), node.clone()))
        })
        .collect();
    Ok(CodebehindSymbols {
        class: Some(class),
        handlers,
        method_edges,
    })
}

fn corpus_boundary(path: &Path, source_file: &str) -> PathBuf {
    if !looks_absolute_path(source_file) {
        let mut boundary = path.to_path_buf();
        for _ in Path::new(source_file).components() {
            boundary.pop();
        }
        return boundary;
    }
    let markers = ["csproj", "fsproj", "vbproj", "sln", "slnx"];
    for directory in path
        .parent()
        .into_iter()
        .chain(path.parent().into_iter().flat_map(Path::ancestors).skip(1))
    {
        if fs::read_dir(directory).ok().is_some_and(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                entry
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| markers.contains(&extension))
            })
        }) {
            return directory.to_path_buf();
        }
    }
    path.parent().unwrap_or_else(|| Path::new("")).to_path_buf()
}

#[derive(Clone)]
struct ViewModelCandidate {
    node: Node,
    physical_path: PathBuf,
}

fn logical_from_boundary(path: &Path, boundary: &Path, absolute: bool) -> String {
    if absolute {
        normalize_logical_path(path)
    } else {
        normalize_logical_path(path.strip_prefix(boundary).unwrap_or(path))
    }
}

fn view_model_classes(
    path: &Path,
    source_file: &str,
) -> anyhow::Result<HashMap<String, Vec<ViewModelCandidate>>> {
    let boundary = corpus_boundary(path, source_file);
    let patterns = crate::detect::load_ignore_patterns(&boundary, false);
    let mut pending = vec![boundary.clone()];
    let mut classes: HashMap<String, Vec<ViewModelCandidate>> = HashMap::new();
    let mut visited = 0usize;
    while let Some(directory) = pending.pop() {
        if visited >= XAML_DIRECTORY_CAP {
            break;
        }
        visited += 1;
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let candidate = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.')
                    || crate::detect::is_noise_dir(&name, candidate.parent())
                    || crate::detect::is_ignored(&candidate, &boundary, &patterns)
                {
                    continue;
                }
                pending.push(candidate);
                continue;
            }
            if candidate.extension().and_then(|value| value.to_str()) != Some("cs")
                || crate::detect::is_ignored(&candidate, &boundary, &patterns)
            {
                continue;
            }
            let logical =
                logical_from_boundary(&candidate, &boundary, looks_absolute_path(source_file));
            let extraction = match crate::engine::extract_as(&candidate, &logical) {
                Ok(extraction) => extraction,
                Err(_) => continue,
            };
            for node in extraction.nodes.into_iter().filter(|node| {
                node.label.ends_with("ViewModel")
                    && node.extra.get("type").and_then(serde_json::Value::as_str) == Some("class")
            }) {
                classes
                    .entry(node.label.clone())
                    .or_default()
                    .push(ViewModelCandidate {
                        node,
                        physical_path: candidate.clone(),
                    });
            }
        }
    }
    Ok(classes)
}

fn simple_type_name(value: &str) -> Option<String> {
    let mut value = value.trim().trim_matches(['{', '}']).to_owned();
    if let Some((head, _)) = value.split_once(',') {
        value = head.trim().to_owned();
    }
    if let Some(rest) = value.strip_prefix("x:Type ") {
        value = rest.trim().to_owned();
    }
    let value = value
        .rsplit([':', '.', '+'])
        .next()
        .unwrap_or(&value)
        .to_owned();
    Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$")
        .expect("identifier regex")
        .is_match(&value)
        .then_some(value)
}

fn inferred_view_model_names(view_name: Option<&str>) -> Vec<String> {
    let Some(view_name) = view_name else {
        return Vec::new();
    };
    let mut result = Vec::new();
    if view_name == "MainWindow" {
        result.push("MainWindowViewModel".into());
        result.push("MainViewModel".into());
    }
    for suffix in ["UserControl", "View", "Page", "Control"] {
        if view_name.ends_with(suffix) && view_name.len() > suffix.len() {
            let candidate = format!("{}ViewModel", &view_name[..view_name.len() - suffix.len()]);
            if !result.contains(&candidate) {
                result.push(candidate);
            }
            break;
        }
    }
    result
}

fn split_markup_args(arguments: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    for (index, character) in arguments.char_indices() {
        match character {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                result.push(arguments[start..index].trim());
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    let tail = arguments[start..].trim();
    if !tail.is_empty() {
        result.push(tail);
    }
    result
}

fn markup(value: &str) -> Option<(&str, &str)> {
    let value = value.trim();
    let inner = value.strip_prefix('{')?.strip_suffix('}')?.trim();
    let (name, arguments) = inner.split_once(' ').unwrap_or((inner, ""));
    (!name.is_empty()).then_some((name, arguments.trim()))
}

fn static_resource(value: &str) -> Option<String> {
    let (name, arguments) = markup(value)?;
    if name != "StaticResource" {
        return None;
    }
    for argument in split_markup_args(arguments) {
        if let Some((key, value)) = argument.split_once('=') {
            if key.trim() == "ResourceKey" && !value.trim().is_empty() {
                return Some(value.trim().to_owned());
            }
        } else if !argument.is_empty() {
            return Some(argument.to_owned());
        }
    }
    None
}

fn binding_references(value: &str) -> (Option<String>, Option<String>) {
    let Some((name, arguments)) = markup(value) else {
        return (None, None);
    };
    if name != "Binding" {
        return (None, None);
    }
    let mut path = None;
    let mut converter = None;
    for argument in split_markup_args(arguments) {
        if let Some((key, value)) = argument.split_once('=') {
            match key.trim() {
                "Path" => path = Some(value.trim().to_owned()),
                "Converter" => converter = static_resource(value.trim()),
                _ => {}
            }
        } else if path.is_none() && !argument.is_empty() {
            path = Some(argument.to_owned());
        }
    }
    if path
        .as_ref()
        .is_some_and(|value| value.contains(['{', '}']))
    {
        path = None;
    }
    (path.filter(|value| !value.is_empty()), converter)
}

fn pascal_name(name: &str) -> Option<String> {
    let mut name = name.trim().trim_start_matches('_');
    if let Some(rest) = name.strip_prefix("m_") {
        name = rest;
    }
    let mut characters = name.chars();
    let first = characters.next()?;
    if !first.is_ascii_alphabetic() && first != '_' {
        return None;
    }
    if !characters
        .clone()
        .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return None;
    }
    Some(format!(
        "{}{}",
        first.to_ascii_uppercase(),
        characters.as_str()
    ))
}

fn toolkit_members(
    candidate: &ViewModelCandidate,
) -> anyhow::Result<(HashMap<String, Node>, Vec<Edge>)> {
    let text = String::from_utf8_lossy(&fs::read(&candidate.physical_path)?).into_owned();
    let field = Regex::new(r"\b(_?m?_?[A-Za-z_][A-Za-z0-9_]*)\s*(?:=.*)?;")?;
    let method = Regex::new(r"\b([A-Za-z_][A-Za-z0-9_]*)\s*\(")?;
    let mut pending: Option<(&str, usize)> = None;
    let mut members = HashMap::new();
    let mut edges = Vec::new();
    for (index, original) in text.lines().enumerate() {
        let line_number = index + 1;
        let mut line = original;
        if let Some((_, remainder)) = line.split_once(']') {
            if line.contains("ObservableProperty") {
                pending = Some(("property", line_number));
                line = remainder.trim();
                if line.is_empty() {
                    continue;
                }
            } else if line.contains("RelayCommand") {
                pending = Some(("command", line_number));
                line = remainder.trim();
                if line.is_empty() {
                    continue;
                }
            }
        } else if line.contains("[ObservableProperty") {
            pending = Some(("property", line_number));
            continue;
        } else if line.contains("[RelayCommand") {
            pending = Some(("command", line_number));
            continue;
        }
        if line.trim().is_empty() || line.trim_start().starts_with('[') {
            continue;
        }
        let Some((kind, attribute_line)) = pending.take() else {
            continue;
        };
        let (label, context) = if kind == "property" {
            let Some(capture) = field.captures(line) else {
                continue;
            };
            let Some(label) = pascal_name(&capture[1]) else {
                continue;
            };
            (label, "communitytoolkit_observable_property")
        } else {
            let Some(capture) = method.captures(line) else {
                continue;
            };
            (
                format!(
                    "{}Command",
                    capture[1].strip_suffix("Async").unwrap_or(&capture[1])
                ),
                "communitytoolkit_relay_command",
            )
        };
        let id = make_id(&[&candidate.node.id, &label]);
        let node = Node {
            id: id.clone(),
            label: label.clone(),
            file_type: "code".into(),
            source_file: candidate.node.source_file.clone(),
            source_location: Some(format!("L{attribute_line}")),
            community: None,
            extra: BTreeMap::from([
                ("_origin".into(), "dotnet".into()),
                ("type".into(), "generated_member".into()),
            ]),
        };
        let mut extra = BTreeMap::from([
            ("_src".into(), candidate.node.id.clone().into()),
            ("_tgt".into(), id.clone().into()),
            ("context".into(), context.into()),
            (
                "source_location".into(),
                format!("L{attribute_line}").into(),
            ),
            ("weight".into(), 1.0.into()),
        ]);
        extra.insert("confidence_score".into(), 0.5.into());
        edges.push(Edge {
            source: candidate.node.id.clone(),
            target: id,
            relation: "defines".into(),
            confidence: Confidence::Inferred,
            source_file: candidate.node.source_file.clone(),
            extra,
        });
        members.insert(label, node);
    }
    Ok((members, edges))
}

fn add_binding(
    builder: &mut Builder<'_>,
    owner: &str,
    attribute: &str,
    value: &str,
    line: usize,
    generated: &HashMap<String, Node>,
) {
    let (path, converter) = binding_references(value);
    if let Some(path) = path {
        let id = make_id(&["binding", &path]);
        builder.node(
            id.clone(),
            &path,
            "binding",
            "concept",
            builder.source_file,
            line,
        );
        let context = if attribute == "Command" || attribute.ends_with(".Command") {
            "binding_command"
        } else {
            "binding_path"
        };
        builder.edge(
            owner,
            &id,
            "references",
            Some(context),
            Confidence::Extracted,
            line,
        );
        if let Some(member) = generated.get(&path) {
            builder.existing_node(member);
            builder.edge_from(
                owner,
                &member.id,
                "references",
                Some(context),
                Confidence::Inferred,
                &member.source_file,
                line,
            );
        }
    }
    if let Some(converter) = converter {
        let id = make_id(&["binding_converter", &converter]);
        builder.node(
            id.clone(),
            &converter,
            "binding_converter",
            "concept",
            builder.source_file,
            line,
        );
        builder.edge(
            owner,
            &id,
            "references",
            Some("binding_converter"),
            Confidence::Extracted,
            line,
        );
    }
}

fn extract_xaml(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
    allow_path_probes: bool,
) -> anyhow::Result<Extraction> {
    validate_xml(bytes)?;
    let text = lossy_text(bytes);
    let mut builder = Builder::new(path, source_file);
    let root_tag = Regex::new(r"(?s)<\s*([A-Za-z_][A-Za-z0-9_.:-]*)\b")?
        .captures(&text)
        .map(|capture| {
            capture[1]
                .rsplit(':')
                .next()
                .unwrap_or(&capture[1])
                .to_owned()
        })
        .unwrap_or_else(|| "Xaml".into());
    let root_id = make_id(&[&builder.stem, &root_tag]);
    builder.node(
        root_id.clone(),
        &root_tag,
        "element",
        "code",
        source_file,
        1,
    );
    builder.edge(
        &builder.file_id.clone(),
        &root_id,
        "contains",
        None,
        Confidence::Extracted,
        1,
    );

    let class_regex = Regex::new(r#"\bx:Class\s*=\s*"([^"]+)""#)?;
    let class_name = class_regex
        .captures(&text)
        .map(|capture| capture[1].to_owned());
    let codebehind = if allow_path_probes {
        codebehind_symbols(path, source_file, class_name.as_deref())?
    } else {
        CodebehindSymbols {
            class: None,
            handlers: HashMap::new(),
            method_edges: HashMap::new(),
        }
    };
    if let Some(class_name) = class_name.as_deref() {
        let line = text
            .find(class_name)
            .map(|offset| line_of(&text, offset))
            .unwrap_or(1);
        let class = if let Some(class) = codebehind.class.as_ref() {
            builder.existing_node(class);
            class.clone()
        } else {
            let label = class_name.rsplit('.').next().unwrap_or(class_name);
            let id = make_id(&[&builder.stem, label]);
            builder.node(id.clone(), label, "class", "code", source_file, line);
            builder
                .nodes
                .iter()
                .find(|node| node.id == id)
                .expect("inserted XAML class")
                .clone()
        };
        builder.edge(
            &root_id,
            &class.id,
            "references",
            Some("x_class"),
            Confidence::Extracted,
            line,
        );
    }

    let data_context_block = Regex::new(
        r"(?is)<[A-Za-z_][A-Za-z0-9_:.-]*\.DataContext\b[^>]*>(.*?)</[A-Za-z_][A-Za-z0-9_:.-]*\.DataContext\s*>",
    )?;
    let child_tag = Regex::new(r"<\s*([A-Za-z_][A-Za-z0-9_:.-]*)\b")?;
    let mut explicit_names = Vec::new();
    let mut has_data_context = false;
    for capture in data_context_block.captures_iter(&text) {
        has_data_context = true;
        if let Some(child) = child_tag.captures(&capture[1])
            && let Some(name) = simple_type_name(&child[1])
            && !explicit_names.contains(&name)
        {
            explicit_names.push(name);
        }
    }
    let attribute_regex = Regex::new(r#"([A-Za-z_][A-Za-z0-9_.:-]*)\s*=\s*"([^"]*)""#)?;
    let design_type = Regex::new(r"\bType\s*=\s*(?:\{x:Type\s+)?([A-Za-z0-9_.:+]+)")?;
    let mut prism_autowire = false;
    for capture in attribute_regex.captures_iter(&text) {
        let local = capture[1].rsplit([':', '}']).next().unwrap_or(&capture[1]);
        if local == "DataContext" {
            has_data_context = true;
            if let Some(value) = design_type.captures(&capture[2])
                && let Some(name) = simple_type_name(&value[1])
                && !explicit_names.contains(&name)
            {
                explicit_names.push(name);
            }
        }
        if local.ends_with("ViewModelLocator.AutoWireViewModel")
            && capture[2].eq_ignore_ascii_case("true")
        {
            prism_autowire = true;
        }
    }
    let class_simple = class_name
        .as_deref()
        .and_then(|name| name.rsplit('.').next());
    let vm_names = if has_data_context {
        explicit_names
    } else {
        let view_name = class_simple.or_else(|| {
            prism_autowire
                .then(|| path.file_stem().and_then(|name| name.to_str()))
                .flatten()
        });
        inferred_view_model_names(view_name)
    };
    let mut generated_members = HashMap::new();
    if allow_path_probes && !vm_names.is_empty() {
        let classes = view_model_classes(path, source_file)?;
        let mut candidates = vm_names
            .iter()
            .flat_map(|name| classes.get(name).into_iter().flatten())
            .cloned()
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| left.node.id.cmp(&right.node.id));
        candidates.dedup_by(|left, right| left.node.id == right.node.id);
        if candidates.len() == 1 {
            let candidate = candidates.pop().expect("one ViewModel candidate");
            builder.existing_node(&candidate.node);
            builder.edge(
                &root_id,
                &candidate.node.id,
                "references",
                Some("view_model"),
                if has_data_context {
                    Confidence::Extracted
                } else {
                    Confidence::Inferred
                },
                1,
            );
            let (members, edges) = toolkit_members(&candidate)?;
            for member in members.values() {
                builder.existing_node(member);
            }
            for edge in &edges {
                builder.existing_edge(edge);
            }
            generated_members = members;
        }
    }

    let tag = Regex::new(r"(?s)<\s*([A-Za-z_][A-Za-z0-9_.:-]*)\b([^<>]*)/?>")?;
    let identifier = Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$")?;
    const NON_EVENT_ATTRIBUTES: &[&str] = &[
        "Name",
        "Content",
        "Text",
        "Title",
        "Tag",
        "ToolTip",
        "Header",
        "Class",
        "Key",
        "Uid",
        "DataContext",
        "Style",
        "Source",
    ];
    for capture in tag.captures_iter(&text) {
        let element_type = capture[1].rsplit(':').next().unwrap_or(&capture[1]);
        let attributes = &capture[2];
        let line = line_of(&text, capture.get(0).expect("XAML element").start());
        let parsed = attribute_regex
            .captures_iter(attributes)
            .map(|capture| (capture[1].to_owned(), capture[2].to_owned()))
            .collect::<Vec<_>>();
        let name = parsed.iter().find_map(|(key, value)| {
            (key.rsplit([':', '}']).next().unwrap_or(key) == "Name").then(|| value.clone())
        });
        let owner = if let Some(name) = name {
            let id = make_id(&[&builder.stem, &name]);
            builder.node(id.clone(), &name, "element", "code", source_file, line);
            builder.edge(&root_id, &id, "contains", None, Confidence::Extracted, line);
            let type_id = make_id(&["xaml", element_type]);
            builder.node(
                type_id.clone(),
                element_type,
                "element_type",
                "concept",
                source_file,
                line,
            );
            builder.edge(
                &id,
                &type_id,
                "references",
                Some("type"),
                Confidence::Extracted,
                line,
            );
            id
        } else {
            root_id.clone()
        };
        for (key, value) in parsed {
            let local = key.rsplit([':', '}']).next().unwrap_or(&key);
            if !NON_EVENT_ATTRIBUTES.contains(&local)
                && identifier.is_match(&value)
                && let Some(handler) = codebehind.handlers.get(&value)
            {
                builder.existing_node(handler);
                if let Some(method_edge) = codebehind.method_edges.get(&handler.id) {
                    if let Some(class) = codebehind.class.as_ref() {
                        builder.existing_node(class);
                    }
                    builder.existing_edge(method_edge);
                }
                builder.edge(
                    &owner,
                    &handler.id,
                    "references",
                    Some("event"),
                    Confidence::Extracted,
                    line,
                );
            }
            add_binding(
                &mut builder,
                &owner,
                local,
                &value,
                line,
                &generated_members,
            );
            if element_type == "Binding" && local == "Path" {
                let value = value.trim();
                if !value.is_empty() && !value.contains(['{', '}']) {
                    let id = make_id(&["binding", value]);
                    builder.node(id.clone(), value, "binding", "concept", source_file, line);
                    builder.edge(
                        &owner,
                        &id,
                        "references",
                        Some("binding_path"),
                        Confidence::Extracted,
                        line,
                    );
                }
            }
            if element_type == "Binding"
                && local == "Converter"
                && let Some(converter) = static_resource(&value)
            {
                let id = make_id(&["binding_converter", &converter]);
                builder.node(
                    id.clone(),
                    &converter,
                    "binding_converter",
                    "concept",
                    source_file,
                    line,
                );
                builder.edge(
                    &owner,
                    &id,
                    "references",
                    Some("binding_converter"),
                    Confidence::Extracted,
                    line,
                );
            }
        }
    }
    Ok(builder.finish())
}

fn extract_razor(path: &Path, source_file: &str, bytes: &[u8]) -> anyhow::Result<Extraction> {
    let text = lossy_text(bytes);
    let mut builder = Builder::new(path, source_file);
    for (pattern, relation, context) in [
        (
            r"(?m)^@using\s+([A-Za-z_][A-Za-z0-9_.]*)",
            "imports",
            Some("import"),
        ),
        (
            r"(?m)^@inject\s+([A-Za-z_][A-Za-z0-9_.<>\[\]]*)\s+\w+",
            "imports",
            Some("import"),
        ),
        (
            r"(?m)^@inherits\s+([A-Za-z_][A-Za-z0-9_.<>\[\]]*)",
            "inherits",
            None,
        ),
        (
            r"(?m)^@model\s+([A-Za-z_][A-Za-z0-9_.<>\[\]]*)",
            "references",
            None,
        ),
    ] {
        let regex = Regex::new(pattern)?;
        for capture in regex.captures_iter(&text) {
            let name = &capture[1];
            let line = line_of(&text, capture.get(0).expect("Razor directive").start());
            let id = make_id(&[name]);
            builder.node(id.clone(), name, "reference", "code", "", line);
            builder.edge(
                &builder.file_id.clone(),
                &id,
                relation,
                context,
                Confidence::Extracted,
                line,
            );
        }
    }
    let page = Regex::new(r#"(?m)^@page\s+"([^"]+)""#)?;
    for capture in page.captures_iter(&text) {
        let route = &capture[1];
        let line = line_of(&text, capture.get(0).expect("Razor route").start());
        let id = make_id(&["route", route]);
        builder.node(
            id.clone(),
            &format!("route:{route}"),
            "route",
            "concept",
            source_file,
            line,
        );
        builder.edge(
            &builder.file_id.clone(),
            &id,
            "references",
            Some("route"),
            Confidence::Extracted,
            line,
        );
    }
    let components = Regex::new(r"<([A-Z][A-Za-z0-9]+)(?:\s|/|>)")?;
    const HTML_TAGS: &[&str] = &[
        "DOCTYPE", "Html", "Head", "Body", "Div", "Span", "Table", "Form", "Input", "Button",
        "Select", "Option", "Label", "Textarea", "Script", "Style", "Link", "Meta", "Title",
        "Header", "Footer", "Nav", "Main", "Section", "Article", "Aside",
    ];
    for capture in components.captures_iter(&text) {
        let name = &capture[1];
        if HTML_TAGS.contains(&name) {
            continue;
        }
        let line = line_of(&text, capture.get(0).expect("Razor component").start());
        let id = make_id(&[name]);
        builder.node(id.clone(), name, "component", "code", "", line);
        builder.edge(
            &builder.file_id.clone(),
            &id,
            "calls",
            Some("call"),
            Confidence::Extracted,
            line,
        );
    }
    let methods = Regex::new(
        r"(?m)^\s*(?:public|private|protected|internal|static|async|override|virtual|abstract)(?:\s+(?:public|private|protected|internal|static|async|override|virtual|abstract))*\s+[A-Za-z_][A-Za-z0-9_.<>\[\], ]*\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(",
    )?;
    for capture in methods.captures_iter(&text) {
        let name = &capture[1];
        let line = line_of(&text, capture.get(0).expect("Razor method").start());
        let id = make_id(&[&builder.stem, name]);
        builder.node(id.clone(), name, "function", "code", source_file, line);
        builder.edge(
            &builder.file_id.clone(),
            &id,
            "contains",
            None,
            Confidence::Extracted,
            line,
        );
    }
    Ok(builder.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_path_and_bytes_match(extension: &str, contents: &str) {
        let directory = tempfile::tempdir().expect("fixture directory");
        let path = directory.path().join(format!("fixture.{extension}"));
        fs::write(&path, contents).expect("write fixture");

        let path_extraction = extract_dotnet(&path, &format!("fixture.{extension}"), extension)
            .expect("path extraction");
        let bytes_extraction = extract_dotnet_bytes(
            &path,
            &format!("fixture.{extension}"),
            extension,
            contents.as_bytes(),
        )
        .expect("byte extraction");

        assert_eq!(
            serde_json::to_value(path_extraction).expect("serialize path extraction"),
            serde_json::to_value(bytes_extraction).expect("serialize byte extraction"),
        );
    }

    #[test]
    fn byte_entrypoint_matches_path_entrypoint_for_all_dotnet_primary_formats() {
        assert_path_and_bytes_match(
            "sln",
            r#"Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "App", "App/App.csproj", "{AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA}"
EndProject
"#,
        );
        assert_path_and_bytes_match(
            "slnx",
            r#"<Solution><Project Path="App/App.csproj" /></Solution>"#,
        );
        assert_path_and_bytes_match(
            "csproj",
            r#"<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><TargetFramework>net8.0</TargetFramework></PropertyGroup><ItemGroup><PackageReference Include="Example" Version="1.0" /></ItemGroup></Project>"#,
        );
        assert_path_and_bytes_match(
            "xaml",
            r#"<Window x:Class="Sample.MainWindow" xmlns:x="http://schemas.microsoft.com/winfx/2006/xaml"><Button Name="SaveButton" Click="Save" /></Window>"#,
        );
        assert_path_and_bytes_match(
            "razor",
            "@using Sample.Services\n@page \"/sample\"\n<Widget />\n",
        );
    }

    #[test]
    fn byte_xml_entrypoint_rejects_unsafe_declarations() {
        let error = extract_dotnet_bytes(
            Path::new("fixture.csproj"),
            "fixture.csproj",
            "csproj",
            b"<!DOCTYPE project><Project />",
        )
        .expect_err("DOCTYPE must be rejected");
        assert!(error.to_string().contains("DOCTYPE/ENTITY"));
    }

    #[test]
    fn byte_msbuild_extraction_publishes_only_static_coordinates() {
        let project = br#"<Project Sdk="$&#40;ProjectSdk)">
  <PropertyGroup>
    <TargetFrameworks>net8.0;$(ExtraFramework);@(Frameworks);%(Identity)</TargetFrameworks>
  </PropertyGroup>
  <ItemGroup>
    <ProjectReference Include="$(Folder)/Worker.csproj" />
    <ProjectReference Include="@(GeneratedProjects)" />
    <ProjectReference Include="%(RelativeDir)Metadata.csproj" />
    <ProjectReference Include="Wild*/Target.csproj" />
    <ProjectReference Include="Static/Static.csproj" />
    <PackageReference Include="$(PackageName)" Version="1.0.0" />
    <PackageReference Include="Static.DynamicVersion" Version="$(PackageVersion)" />
    <PackageReference Include="Static.Literal" Version="1.2.3" />
  </ItemGroup>
</Project>"#;
        let extraction = extract_dotnet_bytes(
            Path::new("App/App.csproj"),
            "App/App.csproj",
            "csproj",
            project,
        )
        .expect("extract admitted MSBuild bytes");
        let labels = extraction
            .nodes
            .iter()
            .map(|node| node.label.as_str())
            .collect::<HashSet<_>>();

        for expected in [
            "net8.0",
            "Static.csproj",
            "Static.DynamicVersion",
            "Static.Literal (1.2.3)",
        ] {
            assert!(labels.contains(expected), "missing static fact {expected}");
        }
        assert!(labels.iter().all(|label| {
            !["$(", "@(", "%(", "&#40;", "Wild*"]
                .iter()
                .any(|marker| label.contains(marker))
        }));
        assert!(!labels.iter().any(|label| label.contains("ProjectSdk")));
    }

    #[test]
    fn byte_slnx_extraction_rejects_dynamic_project_paths() {
        let extraction = extract_dotnet_bytes(
            Path::new("Workspace.slnx"),
            "Workspace.slnx",
            "slnx",
            br#"<Solution>
  <Project Path="$(Folder)/Worker.csproj" />
  <Project Path="@(GeneratedProjects)" />
  <Project Path="%(RelativeDir)Metadata.csproj" />
  <Project Path="Wild*/Target.csproj" />
  <Project Path="$&#40;Encoded)/Encoded.csproj" />
  <Project Path="Static/Static.csproj" />
</Solution>"#,
        )
        .expect("extract admitted SLNX bytes");
        let project_labels = extraction
            .nodes
            .iter()
            .filter(|node| {
                node.extra.get("type").and_then(serde_json::Value::as_str) == Some("project")
            })
            .map(|node| node.label.as_str())
            .collect::<Vec<_>>();

        assert_eq!(project_labels, ["Static"]);
    }

    fn project_labels(extraction: &Extraction) -> Vec<&str> {
        extraction
            .nodes
            .iter()
            .filter(|node| {
                node.extra.get("type").and_then(serde_json::Value::as_str) == Some("project")
            })
            .map(|node| node.label.as_str())
            .collect()
    }

    #[test]
    fn byte_project_paths_are_static_contained_and_portable_in_all_three_formats() {
        let sln = br#"Project("{TYPE}") = "Dynamic", "$(Folder)/Worker.csproj", "{00000000-0000-0000-0000-000000000001}"
EndProject
Project("{TYPE}") = "Absolute", "/Absolute/Absolute.csproj", "{00000000-0000-0000-0000-000000000002}"
EndProject
Project("{TYPE}") = "DriveAbsolute", "C:\Absolute\Absolute.csproj", "{00000000-0000-0000-0000-000000000003}"
EndProject
Project("{TYPE}") = "DriveRelative", "C:Relative\Relative.csproj", "{00000000-0000-0000-0000-000000000004}"
EndProject
Project("{TYPE}") = "Unc", "\\server\share\Unc.csproj", "{00000000-0000-0000-0000-000000000005}"
EndProject
Project("{TYPE}") = "Escaping", "..\..\Outside\Outside.csproj", "{00000000-0000-0000-0000-000000000006}"
EndProject
Project("{TYPE}") = "Device", "Devices\NUL.csproj", "{00000000-0000-0000-0000-000000000007}"
EndProject
Project("{TYPE}") = "EncodedDrive", "C&#58;Encoded\Encoded.csproj", "{00000000-0000-0000-0000-000000000008}"
EndProject
Project("{TYPE}") = "Safe", "..\Safe\Safe.csproj", "{00000000-0000-0000-0000-000000000009}"
EndProject
"#;
        let sln = extract_dotnet_bytes(
            Path::new("Solutions/Workspace.sln"),
            "Solutions/Workspace.sln",
            "sln",
            sln,
        )
        .expect("extract bounded SLN bytes");
        assert_eq!(project_labels(&sln), ["Safe"]);

        let slnx = extract_dotnet_bytes(
            Path::new("Solutions/Workspace.slnx"),
            "Solutions/Workspace.slnx",
            "slnx",
            br#"<Solution>
  <Project Path="$(Folder)/Worker.csproj" />
  <Project Path="/Absolute/Absolute.csproj" />
  <Project Path="C:\Absolute\Absolute.csproj" />
  <Project Path="C:Relative\Relative.csproj" />
  <Project Path="\\server\share\Unc.csproj" />
  <Project Path="../../Outside/Outside.csproj" />
  <Project Path="Devices/NUL.csproj" />
  <Project Path="C&#58;Encoded/Encoded.csproj" />
  <Project Path="../Safe/Safe.csproj" />
</Solution>"#,
        )
        .expect("extract bounded SLNX bytes");
        assert_eq!(project_labels(&slnx), ["Safe"]);

        let project = extract_dotnet_bytes(
            Path::new("Solutions/App.csproj"),
            "Solutions/App.csproj",
            "csproj",
            br#"<Project><ItemGroup>
  <ProjectReference Include="$(Folder)/Worker.csproj" />
  <ProjectReference Include="/Absolute/Absolute.csproj" />
  <ProjectReference Include="C:\Absolute\Absolute.csproj" />
  <ProjectReference Include="C:Relative\Relative.csproj" />
  <ProjectReference Include="\\server\share\Unc.csproj" />
  <ProjectReference Include="../../Outside/Outside.csproj" />
  <ProjectReference Include="Devices/NUL.csproj" />
  <ProjectReference Include="C&#58;Encoded/Encoded.csproj" />
  <ProjectReference Include="&#47;EncodedRoot/EncodedRoot.csproj" />
  <ProjectReference Include="../Safe/Safe.csproj" />
</ItemGroup></Project>"#,
        )
        .expect("extract bounded project bytes");
        assert_eq!(project_labels(&project), ["Safe.csproj"]);
    }

    #[test]
    fn byte_project_paths_reject_invalid_source_identities_without_fallback() {
        let contents = br#"<Project><ItemGroup><ProjectReference Include="Safe/Safe.csproj" /></ItemGroup></Project>"#;
        for source_file in [
            "/absolute/App.csproj",
            r"C:\absolute\App.csproj",
            r"\\server\share\App.csproj",
            "App/NUL.csproj",
        ] {
            let extraction =
                extract_dotnet_bytes(Path::new("App/App.csproj"), source_file, "csproj", contents)
                    .expect("extract bytes with invalid logical source");
            assert!(
                project_labels(&extraction).is_empty(),
                "invalid source identity admitted a reference: {source_file:?}",
            );
        }
    }

    #[test]
    fn path_probes_preserve_only_existing_relative_external_projects() {
        for extension in ["sln", "slnx", "csproj"] {
            let directory = tempfile::tempdir().expect("external project fixture");
            let source_dir = directory.path().join("App");
            let core = directory.path().join("Core/Core.csproj");
            fs::create_dir_all(&source_dir).expect("create source directory");
            fs::create_dir_all(core.parent().expect("Core parent")).expect("create Core directory");
            fs::write(&core, "<Project />").expect("write existing external project");

            let contents = match extension {
                "sln" => concat!(
                    "Project(\"{TYPE}\") = \"Core\", \"..\\Core\\Core.csproj\", \"{00000000-0000-0000-0000-000000000001}\"\nEndProject\n",
                    "Project(\"{TYPE}\") = \"Missing\", \"..\\Missing\\Missing.csproj\", \"{00000000-0000-0000-0000-000000000002}\"\nEndProject\n",
                ),
                "slnx" => concat!(
                    "<Solution><Project Path=\"../Core/Core.csproj\" />",
                    "<Project Path=\"../Missing/Missing.csproj\" /></Solution>",
                ),
                "csproj" => concat!(
                    "<Project><ItemGroup><ProjectReference Include=\"../Core/Core.csproj\" />",
                    "<ProjectReference Include=\"../Missing/Missing.csproj\" /></ItemGroup></Project>",
                ),
                _ => unreachable!(),
            };
            let source = source_dir.join(format!("App.{extension}"));
            fs::write(&source, contents).expect("write referencing file");

            let path_extraction = extract_dotnet(
                &source,
                source.file_name().and_then(|name| name.to_str()).unwrap(),
                extension,
            )
            .expect("extract path-mode external reference");
            let byte_extraction = extract_dotnet_bytes(
                &source,
                source.file_name().and_then(|name| name.to_str()).unwrap(),
                extension,
                contents.as_bytes(),
            )
            .expect("extract byte-mode external reference");
            let external_core = make_id(&["ext", "../Core/Core.csproj"]);

            assert!(path_extraction
                .nodes
                .iter()
                .any(|node| node.id == external_core));
            assert!(!project_labels(&path_extraction).contains(&"Missing"));
            assert!(!project_labels(&path_extraction).contains(&"Missing.csproj"));
            assert!(byte_extraction
                .nodes
                .iter()
                .all(|node| node.id != external_core));
            assert!(project_labels(&byte_extraction).is_empty());
        }
    }
}
