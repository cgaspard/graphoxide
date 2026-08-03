//! Deterministic package-manifest ingestion.
//!
//! A package is keyed only by its declared name.  Dependency edges therefore
//! converge on the same node when the dependency's own manifest is present,
//! while the graph builder can prune edges to packages outside the corpus.

use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use quick_xml::{events::Event, Reader};
use std::{collections::BTreeMap, fs, path::Path};

/// Manifests are configuration, not an unbounded document-ingestion surface.
pub const MAX_MANIFEST_BYTES: u64 = 2_000_000;
const MAX_DEPENDENCIES: usize = 10_000;
const MAX_PACKAGE_NAME_BYTES: usize = 4_096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ecosystem {
    Apm,
    Python,
    Go,
    Maven,
}

impl Ecosystem {
    fn label(self) -> &'static str {
        match self {
            Self::Apm => "apm",
            Self::Python => "python",
            Self::Go => "go",
            Self::Maven => "maven",
        }
    }
}

#[derive(Default)]
struct PackageInfo {
    name: String,
    version: Option<String>,
    dependencies: Vec<String>,
}

/// Return true only for the deterministic package formats handled here.
pub fn is_package_manifest_path(path: &Path) -> bool {
    ecosystem(path).is_some()
}

/// Parse a recognized manifest. Read and parse failures deliberately produce
/// an empty extraction so one malformed configuration file cannot abort a
/// repository scan.
pub fn extract_package_manifest(path: &Path) -> Extraction {
    let source_file = path.to_string_lossy().replace('\\', "/");
    extract_package_manifest_as(path, &source_file)
}

/// Parse a manifest while using a caller-provided portable source identity.
pub(crate) fn extract_package_manifest_as(path: &Path, source_file: &str) -> Extraction {
    let Some(ecosystem) = ecosystem(path) else {
        return Extraction::default();
    };
    let Ok(metadata) = fs::metadata(path) else {
        return Extraction::default();
    };
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Extraction::default();
    }
    let Ok(text) = fs::read_to_string(path) else {
        return Extraction::default();
    };
    let parsed = match ecosystem {
        Ecosystem::Apm => parse_apm(&text),
        Ecosystem::Python => parse_pyproject(&text),
        Ecosystem::Go => parse_go_mod(&text),
        Ecosystem::Maven => parse_pom(&text),
    };
    let Some(mut info) = parsed else {
        return Extraction::default();
    };
    info.name = info.name.trim().to_owned();
    if !valid_package_name(&info.name) {
        return Extraction::default();
    }

    let package_id = make_id(&["pkg", &info.name]);
    if package_id.is_empty() {
        return Extraction::default();
    }
    let mut extra = BTreeMap::from([
        ("type".into(), "package".into()),
        ("ecosystem".into(), ecosystem.label().into()),
    ]);
    if let Some(version) = info
        .version
        .as_deref()
        .map(str::trim)
        .filter(|version| !version.is_empty() && version.len() <= MAX_PACKAGE_NAME_BYTES)
    {
        extra.insert("version".into(), version.into());
    }
    let node = Node {
        id: package_id.clone(),
        label: info.name,
        file_type: "code".into(),
        source_file: source_file.replace('\\', "/"),
        source_location: Some("L1".into()),
        community: None,
        extra,
    };

    let mut seen = std::collections::BTreeSet::new();
    let mut edges = Vec::new();
    for dependency in info.dependencies.into_iter().take(MAX_DEPENDENCIES) {
        let dependency = dependency.trim();
        if !valid_package_name(dependency) {
            continue;
        }
        let target = make_id(&["pkg", dependency]);
        if target.is_empty() || target == package_id || !seen.insert(target.clone()) {
            continue;
        }
        edges.push(dependency_edge(&package_id, &target, &node.source_file));
    }

    Extraction {
        nodes: vec![node],
        edges,
        hyperedges: Vec::new(),
    }
}

fn ecosystem(path: &Path) -> Option<Ecosystem> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    match name.as_str() {
        "apm.yml" | "apm.yaml" => Some(Ecosystem::Apm),
        "pyproject.toml" => Some(Ecosystem::Python),
        "go.mod" => Some(Ecosystem::Go),
        "pom.xml" => Some(Ecosystem::Maven),
        _ => None,
    }
}

fn valid_package_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PACKAGE_NAME_BYTES
        && !value.chars().any(char::is_control)
}

fn dependency_edge(source: &str, target: &str, source_file: &str) -> Edge {
    Edge {
        source: source.into(),
        target: target.into(),
        relation: "depends_on".into(),
        confidence: Confidence::Extracted,
        source_file: source_file.into(),
        extra: BTreeMap::from([
            ("context".into(), "dependency".into()),
            ("confidence_score".into(), 1.0.into()),
            ("source_location".into(), "L1".into()),
            ("weight".into(), 1.0.into()),
        ]),
    }
}

fn parse_apm(text: &str) -> Option<PackageInfo> {
    let mut result = PackageInfo::default();
    let mut in_dependencies = false;
    for raw_line in text.lines() {
        if raw_line.contains('\t') {
            // YAML tabs are invalid indentation; fail closed instead of
            // guessing which level an ambiguous key belongs to.
            return None;
        }
        let indent = raw_line.len() - raw_line.trim_start_matches(' ').len();
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line == "---" || line == "..." {
            continue;
        }
        if indent == 0 {
            in_dependencies = false;
            let (key, value) = split_yaml_pair(line)?;
            match key {
                "name" => result.name = yaml_scalar(value)?,
                "version" => result.version = yaml_scalar(value),
                "dependencies" => {
                    in_dependencies = value.trim().is_empty();
                    if !in_dependencies {
                        // The upstream contract accepts list/map dependency
                        // blocks. Inline YAML collections are intentionally not
                        // guessed by this bounded fallback parser.
                    }
                }
                _ => {}
            }
        } else if in_dependencies {
            let dependency = if let Some(item) = line.strip_prefix('-') {
                let item = item.trim();
                if let Some((key, _)) = split_yaml_pair_optional(item) {
                    yaml_scalar(key)
                } else {
                    yaml_scalar(item)
                }
            } else {
                split_yaml_pair_optional(line).and_then(|(key, _)| yaml_scalar(key))
            };
            if let Some(dependency) = dependency {
                result.dependencies.push(dependency);
            }
        }
    }
    (!result.name.is_empty()).then_some(result)
}

fn split_yaml_pair(line: &str) -> Option<(&str, &str)> {
    split_yaml_pair_optional(line)
}

fn split_yaml_pair_optional(line: &str) -> Option<(&str, &str)> {
    let index = line.find(':')?;
    let key = line[..index].trim();
    (!key.is_empty()).then_some((key, line[index + 1..].trim()))
}

fn yaml_scalar(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || matches!(value, "null" | "Null" | "NULL" | "~") {
        return None;
    }
    if let Some(quoted) = value.strip_prefix('"') {
        let end = quoted.find('"')?;
        return Some(quoted[..end].to_owned());
    }
    if let Some(quoted) = value.strip_prefix('\'') {
        let end = quoted.find('\'')?;
        return Some(quoted[..end].replace("''", "'"));
    }
    Some(value.split(" #").next().unwrap_or(value).trim().to_owned())
}

fn parse_pyproject(text: &str) -> Option<PackageInfo> {
    let document = toml::from_str::<toml::Value>(text).ok()?;
    let project = document.get("project").and_then(toml::Value::as_table);
    let poetry = document
        .get("tool")
        .and_then(toml::Value::as_table)
        .and_then(|tool| tool.get("poetry"))
        .and_then(toml::Value::as_table);
    let name = project
        .and_then(|table| table.get("name"))
        .and_then(toml::Value::as_str)
        .or_else(|| {
            poetry
                .and_then(|table| table.get("name"))
                .and_then(toml::Value::as_str)
        })?
        .to_owned();
    let version = project
        .and_then(|table| table.get("version"))
        .and_then(toml::Value::as_str)
        .or_else(|| {
            poetry
                .and_then(|table| table.get("version"))
                .and_then(toml::Value::as_str)
        })
        .map(str::to_owned);
    let mut dependencies = project
        .and_then(|table| table.get("dependencies"))
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .filter_map(pep508_name)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if let Some(poetry_dependencies) = poetry
        .and_then(|table| table.get("dependencies"))
        .and_then(toml::Value::as_table)
    {
        dependencies.extend(
            poetry_dependencies
                .keys()
                .filter(|name| !name.eq_ignore_ascii_case("python"))
                .cloned(),
        );
    }
    Some(PackageInfo {
        name,
        version,
        dependencies,
    })
}

fn pep508_name(specification: &str) -> Option<&str> {
    let specification = specification.trim();
    let end = specification
        .char_indices()
        .find_map(|(index, character)| {
            (character.is_whitespace() || "<>=!~;[(".contains(character)).then_some(index)
        })
        .unwrap_or(specification.len());
    let name = &specification[..end];
    (!name.is_empty()).then_some(name)
}

fn parse_go_mod(text: &str) -> Option<PackageInfo> {
    let mut result = PackageInfo::default();
    let mut require_block = false;
    for raw_line in text.lines() {
        let line = raw_line.split("//").next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let tokens = line.split_whitespace().collect::<Vec<_>>();
        if result.name.is_empty() && tokens.first() == Some(&"module") {
            if let Some(name) = tokens.get(1) {
                result.name = (*name).to_owned();
            }
            continue;
        }
        if tokens.first() == Some(&"require") && tokens.get(1) == Some(&"(") {
            require_block = true;
            continue;
        }
        if require_block && line.starts_with(')') {
            require_block = false;
            continue;
        }
        let dependency = if require_block {
            (tokens.len() >= 2 && tokens[1].starts_with('v')).then_some(tokens[0])
        } else if tokens.first() == Some(&"require")
            && tokens.len() >= 3
            && tokens[2].starts_with('v')
        {
            Some(tokens[1])
        } else {
            None
        };
        if let Some(dependency) = dependency {
            result.dependencies.push(dependency.to_owned());
        }
    }
    (!result.name.is_empty()).then_some(result)
}

fn parse_pom(text: &str) -> Option<PackageInfo> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut stack = Vec::<String>::new();
    let mut project_group = None;
    let mut project_artifact = None;
    let mut project_version = None;
    let mut dependency_group = None;
    let mut dependency_artifact = None;
    let mut dependencies = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                let name = String::from_utf8_lossy(element.local_name().as_ref()).into_owned();
                stack.push(name);
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::Text(value)) => {
                let value = value.decode().ok()?.trim().to_owned();
                if value.is_empty() {
                    continue;
                }
                let current = stack.last().map(String::as_str);
                let in_dependency = stack.iter().any(|name| name == "dependency");
                let direct_dependency_child = stack
                    .len()
                    .checked_sub(2)
                    .is_some_and(|index| stack[index] == "dependency");
                match (in_dependency, current) {
                    (false, Some("groupId")) if stack.len() == 2 => project_group = Some(value),
                    (false, Some("artifactId")) if stack.len() == 2 => {
                        project_artifact = Some(value)
                    }
                    (false, Some("version")) if stack.len() == 2 => project_version = Some(value),
                    (true, Some("groupId")) if direct_dependency_child => {
                        dependency_group = Some(value)
                    }
                    (true, Some("artifactId")) if direct_dependency_child => {
                        dependency_artifact = Some(value)
                    }
                    _ => {}
                }
            }
            Ok(Event::End(element)) => {
                let name = String::from_utf8_lossy(element.local_name().as_ref()).into_owned();
                if name == "dependency" {
                    if let Some(artifact) = dependency_artifact.take() {
                        dependencies.push(match dependency_group.take() {
                            Some(group) => format!("{group}:{artifact}"),
                            None => artifact,
                        });
                    } else {
                        dependency_group = None;
                    }
                }
                if stack.pop().as_deref() != Some(name.as_str()) {
                    return None;
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    if !stack.is_empty() {
        return None;
    }
    let artifact = project_artifact?;
    Some(PackageInfo {
        name: project_group
            .map(|group| format!("{group}:{artifact}"))
            .unwrap_or(artifact),
        version: project_version,
        dependencies,
    })
}
