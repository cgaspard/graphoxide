//! Regex and structured-data extraction for languages without a compiled grammar.

use anyhow::Context as _;
use graphoxide_core::{
    make_id,
    mcp_config::{is_mcp_config_path, mcp_server_map},
    sanitize_label, Confidence, Edge, Extraction, Node,
};
use regex::Regex;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Component, Path, PathBuf},
};

pub fn extract_text(path: &Path, source_file: &str) -> anyhow::Result<Extraction> {
    if is_mcp_config_path(path) {
        return extract_mcp_config(path, source_file);
    }
    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("dmi"))
    {
        return crate::compat::extract_dmi(path, source_file);
    }
    if crate::manifest_ingest::is_package_manifest_path(path) {
        return Ok(crate::manifest_ingest::extract_package_manifest_as(
            path,
            source_file,
        ));
    }
    let text = fs::read_to_string(path)?;
    if path
        .extension()
        .and_then(|v| v.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
    {
        return crate::json_config::extract_json_config(path, source_file);
    }
    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            matches!(extension.to_ascii_lowercase().as_str(), "md" | "markdown")
        })
    {
        return extract_markdown(&text, source_file, path);
    }
    if let Some(extraction) = crate::compat::extract_compat(path, &text, source_file)? {
        return Ok(extraction);
    }
    let stem = Path::new(source_file)
        .with_extension("")
        .to_string_lossy()
        .into_owned();
    let file_id = make_id(&[&stem]);
    let mut nodes = vec![node(
        file_id.clone(),
        path.file_name()
            .and_then(|v| v.to_str())
            .unwrap_or(source_file),
        source_file,
        1,
        "file",
    )];
    let mut edges = Vec::new();
    let mut seen = HashSet::from([file_id.clone()]);
    let definitions = Regex::new(
        r"(?m)^\s*(?:(?:pub(?:lic|lic static)?|private|protected|internal|export|abstract|static|async|final|open|partial)\s+)*(class|interface|struct|enum|trait|protocol|module|namespace|type|def|fn|fun|function|func|sub|procedure)\s+([\p{L}_][\p{L}\p{N}_]*)",
    )?;
    let mut labels = HashMap::new();
    for capture in definitions.captures_iter(&text) {
        let kind = &capture[1];
        let name = &capture[2];
        let id = make_id(&[&stem, name]);
        if !seen.insert(id.clone()) {
            continue;
        }
        let line = line_of(&text, capture.get(0).unwrap().start());
        let function = matches!(
            kind,
            "def" | "fn" | "fun" | "function" | "func" | "sub" | "procedure"
        );
        nodes.push(node(
            id.clone(),
            if function {
                format!("{name}()")
            } else {
                name.into()
            },
            source_file,
            line,
            if function { "function" } else { "class" },
        ));
        edges.push(edge(
            file_id.clone(),
            id.clone(),
            "contains",
            source_file,
            line,
            Confidence::Extracted,
        ));
        labels.insert(name.to_lowercase(), id);
    }
    for (kind, name, start, function) in special_definitions(path, &text)? {
        let id = make_id(&[&stem, &name]);
        if !seen.insert(id.clone()) {
            continue;
        }
        let line = line_of(&text, start);
        nodes.push(node(
            id.clone(),
            if function {
                format!("{name}()")
            } else {
                name.clone()
            },
            source_file,
            line,
            &kind,
        ));
        edges.push(edge(
            file_id.clone(),
            id.clone(),
            "contains",
            source_file,
            line,
            Confidence::Extracted,
        ));
        labels.insert(name.to_lowercase(), id);
    }
    let imports = Regex::new(
        r#"(?m)^\s*(?:import|from|use|using|require|include)\b\s*[('\"]*([\p{L}\p{N}_./:@-]+)"#,
    )?;
    for capture in imports.captures_iter(&text) {
        let module = &capture[1];
        let line = line_of(&text, capture.get(0).unwrap().start());
        let id = make_id(&[module]);
        if id.is_empty() {
            continue;
        }
        if seen.insert(id.clone()) {
            nodes.push(node(
                id.clone(),
                module
                    .rsplit(['/', ':', '.'])
                    .find(|v| !v.is_empty())
                    .unwrap_or(module),
                source_file,
                line,
                "module",
            ));
        }
        edges.push(edge(
            file_id.clone(),
            id,
            "imports",
            source_file,
            line,
            Confidence::Extracted,
        ));
    }
    let calls = Regex::new(r"([\p{L}_][\p{L}\p{N}_]*)\s*\(")?;
    let keywords = [
        "if", "for", "while", "switch", "catch", "return", "class", "function", "func", "fn",
        "def", "sizeof", "typeof",
    ];
    for capture in calls.captures_iter(&text) {
        let name = &capture[1];
        if keywords.contains(&name) {
            continue;
        }
        if let Some(target) = labels.get(&name.to_lowercase()) {
            let line = line_of(&text, capture.get(0).unwrap().start());
            if let Some(source) = nearest_definition(&nodes, line) {
                if source != target {
                    edges.push(edge(
                        source.into(),
                        target.clone(),
                        "calls",
                        source_file,
                        line,
                        Confidence::Inferred,
                    ));
                }
            }
        }
    }
    Ok(Extraction {
        nodes,
        edges,
        hyperedges: Vec::new(),
    })
}

/// Extract an MCP configuration, degrading to generic JSON when the document
/// turns out not to be one.
///
/// The recognised basenames — `mcp.json` above all — are common enough that a
/// project may use them for something else entirely. A document that does not
/// carry a server map is therefore ordinary JSON, not a malformed MCP config:
/// treating it as an error would abort the whole repository scan over one
/// unrelated file (#4). Genuine faults (oversized or unparsable input) are
/// still reported.
fn extract_mcp_config(path: &Path, source_file: &str) -> anyhow::Result<Extraction> {
    const MAX_BYTES: u64 = 1_048_576;
    const MAX_SERVERS: usize = 200;
    let metadata = fs::metadata(path)?;
    anyhow::ensure!(metadata.len() <= MAX_BYTES, "mcp config too large to index");
    let text = fs::read_to_string(path)?;
    let document = graphoxide_core::parse_jsonc(&text)
        .with_context(|| format!("parse MCP configuration {source_file}"))?;
    let Some(root) = document.as_object() else {
        tracing::warn!(
            "{source_file}: mcp config root is not an object; indexing as generic JSON instead"
        );
        return crate::json_config::extract_json_config(path, source_file);
    };
    let Some(servers) = mcp_server_map(root) else {
        tracing::warn!(
            "{source_file}: mcp config has no server map; indexing as generic JSON instead"
        );
        return crate::json_config::extract_json_config(path, source_file);
    };

    let stem = Path::new(source_file)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    let file_id = make_id(&[source_file]);
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(source_file);
    let mut nodes = vec![mcp_node(
        file_id.clone(),
        filename,
        "mcp_config_file",
        source_file,
    )];
    let mut edges = Vec::new();
    let mut seen_nodes = HashSet::from([file_id.clone()]);
    let mut seen_edges = HashSet::new();
    let npm_package = Regex::new(r"^@[a-z0-9][a-z0-9._-]*/[a-z0-9][a-z0-9._-]*(?:@[\w.\-+]+)?$")?;
    let python_package =
        Regex::new(r"^[a-z0-9][a-z0-9._-]*-mcp(?:-[a-z0-9._-]+)?$|^mcp-[a-z0-9][a-z0-9._-]*$")?;

    for (server_name, value) in servers.iter().take(MAX_SERVERS) {
        if server_name.is_empty() {
            continue;
        }
        let Some(spec) = value.as_object() else {
            continue;
        };
        let server_id = make_id(&[&stem, "mcp_server", server_name]);
        insert_mcp_node(
            &mut nodes,
            &mut seen_nodes,
            server_id.clone(),
            server_name,
            "mcp_server",
            source_file,
        );
        insert_mcp_edge(
            &mut edges,
            &mut seen_edges,
            &file_id,
            &server_id,
            "contains",
            None,
            source_file,
        );

        if let Some(command) = spec
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|command| !command.is_empty())
        {
            let command_id = make_id(&["mcp_command", command]);
            insert_mcp_node(
                &mut nodes,
                &mut seen_nodes,
                command_id.clone(),
                command,
                "mcp_command",
                source_file,
            );
            insert_mcp_edge(
                &mut edges,
                &mut seen_edges,
                &server_id,
                &command_id,
                "references",
                Some("command"),
                source_file,
            );
        }

        if let Some(package) = spec
            .get("args")
            .and_then(serde_json::Value::as_array)
            .and_then(|arguments| {
                arguments
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|argument| !argument.starts_with('-'))
                    .find_map(|argument| {
                        if npm_package.is_match(argument) {
                            let version = argument[1..]
                                .find('@')
                                .map(|index| index + 1)
                                .unwrap_or(argument.len());
                            Some(argument[..version].to_owned())
                        } else if python_package.is_match(argument) {
                            Some(argument.to_owned())
                        } else {
                            None
                        }
                    })
            })
        {
            let package_id = make_id(&["mcp_package", &package]);
            insert_mcp_node(
                &mut nodes,
                &mut seen_nodes,
                package_id.clone(),
                &package,
                "mcp_package",
                source_file,
            );
            insert_mcp_edge(
                &mut edges,
                &mut seen_edges,
                &server_id,
                &package_id,
                "references",
                Some("package"),
                source_file,
            );
        }

        if let Some(environment) = spec.get("env").and_then(serde_json::Value::as_object) {
            for name in environment.keys().filter(|name| !name.is_empty()) {
                let environment_id = make_id(&["env_var", name]);
                insert_mcp_node(
                    &mut nodes,
                    &mut seen_nodes,
                    environment_id.clone(),
                    name,
                    "env_var",
                    source_file,
                );
                insert_mcp_edge(
                    &mut edges,
                    &mut seen_edges,
                    &server_id,
                    &environment_id,
                    "requires_env",
                    None,
                    source_file,
                );
            }
        }
    }
    Ok(Extraction {
        nodes,
        edges,
        hyperedges: Vec::new(),
    })
}

fn mcp_node(id: String, label: &str, kind: &str, source_file: &str) -> Node {
    let mut result = node(id, sanitize_label(label), source_file, 1, kind);
    result
        .extra
        .insert("metadata".into(), serde_json::json!({"mcp_kind": kind}));
    result
}

fn insert_mcp_node(
    nodes: &mut Vec<Node>,
    seen: &mut HashSet<String>,
    id: String,
    label: &str,
    kind: &str,
    source_file: &str,
) {
    if !id.is_empty() && seen.insert(id.clone()) {
        nodes.push(mcp_node(id, label, kind, source_file));
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_mcp_edge(
    edges: &mut Vec<Edge>,
    seen: &mut HashSet<(String, String, String)>,
    source: &str,
    target: &str,
    relation: &str,
    context: Option<&str>,
    source_file: &str,
) {
    if source.is_empty()
        || target.is_empty()
        || source == target
        || !seen.insert((source.to_owned(), target.to_owned(), relation.to_owned()))
    {
        return;
    }
    let mut result = edge(
        source.to_owned(),
        target.to_owned(),
        relation,
        source_file,
        1,
        Confidence::Extracted,
    );
    result.extra.insert("confidence_score".into(), 1.0.into());
    if let Some(context) = context {
        result.extra.insert("context".into(), context.into());
    }
    edges.push(result);
}

#[cfg(test)]
fn extract_json(text: &str, source_file: &str, path: &Path) -> anyhow::Result<Extraction> {
    let stem = Path::new(source_file)
        .with_extension("")
        .to_string_lossy()
        .into_owned();
    let file_id = make_id(&[&stem]);
    let mut nodes = vec![node(
        file_id.clone(),
        path.file_name()
            .and_then(|v| v.to_str())
            .unwrap_or(source_file),
        source_file,
        1,
        "file",
    )];
    let mut edges = Vec::new();
    let value = serde_json::from_str(text)?;
    let mut seen = HashSet::from([file_id.clone()]);
    walk_json(
        &value,
        "",
        &file_id,
        source_file,
        &mut nodes,
        &mut edges,
        &mut seen,
    );
    Ok(Extraction {
        nodes,
        edges,
        hyperedges: Vec::new(),
    })
}
#[cfg(test)]
fn walk_json(
    value: &serde_json::Value,
    prefix: &str,
    parent: &str,
    source: &str,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    seen: &mut HashSet<String>,
) {
    if let Some(object) = value.as_object() {
        for (key, value) in object {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            let id = make_id(&[source, &path]);
            if seen.insert(id.clone()) {
                nodes.push(node(id.clone(), key, source, 1, "json_key"));
                edges.push(edge(
                    parent.into(),
                    id.clone(),
                    "contains",
                    source,
                    1,
                    Confidence::Extracted,
                ));
            }
            walk_json(value, &path, &id, source, nodes, edges, seen);
        }
    } else if let Some(array) = value.as_array() {
        for value in array {
            walk_json(value, prefix, parent, source, nodes, edges, seen);
        }
    }
}

/// Extract Markdown document structure and local document links.
///
/// This is a direct behavioral port of upstream
/// `graphify/extractors/markdown.py::extract_markdown`. Link targets are IDs
/// only: the extractor never fabricates a node for a file it has not parsed.
/// When the linked file is part of the project scan, the target ID is the same
/// ID as that file's real document node.
fn extract_markdown(text: &str, source_file: &str, path: &Path) -> anyhow::Result<Extraction> {
    let stem = Path::new(source_file)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    let file_id = make_id(&[&stem]);
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(source_file);
    let mut nodes = vec![node(file_id.clone(), filename, source_file, 1, "document")];
    let mut edges = Vec::new();
    let mut seen_ids = HashSet::from([file_id.clone()]);
    let mut linked_targets = HashSet::new();
    let mut heading_stack: Vec<(usize, String)> = Vec::new();
    let mut in_code_block = false;

    let inline_link = Regex::new(r#"\[[^\]]*\]\(\s*<?([^\)\s>]+)>?(?:\s+[^\)]*)?\)"#)?;
    let reference_definition = Regex::new(r#"^\s{0,3}\[[^\]]+\]:\s*<?([^\s>]+)>?"#)?;
    let wiki_link = Regex::new(r#"\[\[([^\]|#]+)(?:[#|][^\]]*)?\]\]"#)?;
    let heading = Regex::new(r"^(#{1,6})\s+(.+)")?;

    for (line_index, line_text) in text.lines().enumerate() {
        let line = line_index + 1;
        if line_text.trim().starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            continue;
        }

        {
            let mut add_link = |raw: &str, start: usize| {
                let Some(target) = resolve_markdown_link(raw, source_file, path) else {
                    return;
                };
                if target.id == file_id || !linked_targets.insert(target.id.clone()) {
                    return;
                }
                let mut reference = edge(
                    file_id.clone(),
                    target.id,
                    "references",
                    source_file,
                    line,
                    Confidence::Extracted,
                );
                if let Some(target_file) = target.existing_source_file {
                    reference
                        .extra
                        .insert("target_file".into(), target_file.into());
                }
                // Keep the exact start available for parity diagnostics without
                // changing the upstream line-oriented source location contract.
                reference
                    .extra
                    .insert("source_column".into(), (start + 1).into());
                edges.push(reference);
            };

            for capture in inline_link.captures_iter(line_text) {
                let whole = capture.get(0).expect("inline link has a whole match");
                if whole.start() > 0 && line_text.as_bytes()[whole.start() - 1] == b'!' {
                    continue;
                }
                add_link(&capture[1], whole.start());
            }
            for capture in wiki_link.captures_iter(line_text) {
                let whole = capture.get(0).expect("wiki link has a whole match");
                if whole.start() > 0 && line_text.as_bytes()[whole.start() - 1] == b'!' {
                    continue;
                }
                add_link(&capture[1], whole.start());
            }
            if let Some(capture) = reference_definition.captures(line_text) {
                let whole = capture
                    .get(0)
                    .expect("reference definition has a whole match");
                add_link(&capture[1], whole.start());
            }
        }

        let Some(capture) = heading.captures(line_text) else {
            continue;
        };
        let level = capture[1].len();
        let title = capture[2].trim();
        let mut heading_id = make_id(&[&stem, title]);
        if seen_ids.contains(&heading_id) {
            heading_id = make_id(&[&stem, title, &line.to_string()]);
        }
        if !seen_ids.insert(heading_id.clone()) {
            continue;
        }
        nodes.push(node(
            heading_id.clone(),
            title,
            source_file,
            line,
            "document",
        ));
        while heading_stack
            .last()
            .is_some_and(|(parent_level, _)| *parent_level >= level)
        {
            heading_stack.pop();
        }
        let parent = heading_stack
            .last()
            .map(|(_, id)| id.clone())
            .unwrap_or_else(|| file_id.clone());
        edges.push(edge(
            parent,
            heading_id.clone(),
            "contains",
            source_file,
            line,
            Confidence::Extracted,
        ));
        heading_stack.push((level, heading_id));
    }

    Ok(Extraction {
        nodes,
        edges,
        hyperedges: Vec::new(),
    })
}

struct MarkdownTarget {
    id: String,
    existing_source_file: Option<String>,
}

fn resolve_markdown_link(
    raw: &str,
    source_file: &str,
    physical_source: &Path,
) -> Option<MarkdownTarget> {
    let mut target = raw.trim();
    if target.is_empty() {
        return None;
    }
    target = target.split('#').next()?.split('?').next()?.trim();
    if target.is_empty() {
        return None;
    }
    let lower = target.to_ascii_lowercase();
    if target.contains("://")
        || lower.starts_with("mailto:")
        || lower.starts_with("tel:")
        || lower.starts_with("//")
        || lower.starts_with("data:")
    {
        return None;
    }

    let mut raw_path = PathBuf::from(target);
    let suffix = raw_path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if suffix.is_empty() {
        raw_path.set_extension("md");
    } else if !matches!(
        suffix.as_str(),
        "md" | "mdx" | "qmd" | "markdown" | "rst" | "txt"
    ) {
        return None;
    }

    let logical = if raw_path.is_absolute() {
        raw_path
            .strip_prefix(Path::new("/"))
            .unwrap_or(&raw_path)
            .to_path_buf()
    } else {
        Path::new(source_file)
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(&raw_path)
    };
    let logical = normalize_lexical_path(&logical);
    let logical_without_extension = logical.with_extension("");
    let id = make_id(&[&logical_without_extension
        .to_string_lossy()
        .replace('\\', "/")]);
    if id.is_empty() {
        return None;
    }

    let physical = if raw_path.is_absolute() {
        raw_path
    } else {
        physical_source
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(raw_path)
    };
    let existing_source_file = physical
        .is_file()
        .then(|| logical.to_string_lossy().replace('\\', "/"));
    Some(MarkdownTarget {
        id,
        existing_source_file,
    })
}

fn normalize_lexical_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            }
            Component::Normal(part) => normalized.push(part),
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    normalized
}

fn node(id: String, label: impl Into<String>, source: &str, line: usize, kind: &str) -> Node {
    Node {
        id,
        label: label.into(),
        file_type: if kind == "document" {
            "document"
        } else {
            "code"
        }
        .into(),
        source_file: source.into(),
        source_location: Some(format!("L{line}")),
        community: None,
        extra: BTreeMap::from([
            ("_origin".into(), "fallback".into()),
            ("type".into(), kind.into()),
        ]),
    }
}
fn edge(
    source: String,
    target: String,
    relation: &str,
    file: &str,
    line: usize,
    confidence: Confidence,
) -> Edge {
    Edge {
        source: source.clone(),
        target: target.clone(),
        relation: relation.into(),
        confidence,
        source_file: file.into(),
        extra: BTreeMap::from([
            ("source_location".into(), format!("L{line}").into()),
            ("weight".into(), 1.0.into()),
            ("_src".into(), source.into()),
            ("_tgt".into(), target.into()),
        ]),
    }
}
fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].bytes().filter(|b| *b == b'\n').count() + 1
}
fn nearest_definition(nodes: &[Node], line: usize) -> Option<&str> {
    nodes
        .iter()
        .filter_map(|n| {
            Some((
                n.source_location
                    .as_deref()?
                    .trim_start_matches('L')
                    .parse::<usize>()
                    .ok()?,
                n.id.as_str(),
            ))
        })
        .filter(|(at, _)| *at <= line)
        .max_by_key(|(at, _)| *at)
        .map(|(_, id)| id)
}

fn special_definitions(
    path: &Path,
    text: &str,
) -> anyhow::Result<Vec<(String, String, usize, bool)>> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut found = Vec::new();
    let mut capture =
        |pattern: &str, kind_group: Option<usize>, name_group: usize, function: bool| {
            let regex = Regex::new(pattern)?;
            for row in regex.captures_iter(text) {
                let Some(name) = row.get(name_group) else {
                    continue;
                };
                let kind = kind_group
                    .and_then(|index| row.get(index))
                    .map(|value| value.as_str().to_ascii_lowercase())
                    .unwrap_or_else(|| {
                        if function {
                            "function".into()
                        } else {
                            "class".into()
                        }
                    });
                found.push((
                    kind,
                    name.as_str().trim_matches(['"', '\'']).to_owned(),
                    row.get(0).unwrap().start(),
                    function,
                ));
            }
            anyhow::Ok(())
        };
    match extension.as_str() {
        "sql" => capture(
            r"(?im)^\s*create\s+(?:or\s+replace\s+)?(table|view|function|procedure|trigger|type)\s+(?:if\s+not\s+exists\s+)?([\p{L}_][\p{L}\p{N}_.$]*)",
            Some(1),
            2,
            false,
        )?,
        "tf" | "tfvars" | "hcl" => capture(
            r#"(?m)^\s*(resource|data|module|variable|output|provider|terraform)\s*(?:\"([^\"]+)\")?"#,
            Some(1),
            2,
            false,
        )?,
        "ps1" | "psm1" | "psd1" => {
            capture(
                r"(?im)^\s*(function|filter|class|enum)\s+([\p{L}_][\p{L}\p{N}_-]*)",
                Some(1),
                2,
                false,
            )?;
        }
        "v" | "sv" | "svh" => capture(
            r"(?im)^\s*(module|interface|package|program|function|task|class)\s+(?:automatic\s+)?([\p{L}_][\p{L}\p{N}_]*)",
            Some(1),
            2,
            false,
        )?,
        "f" | "f90" | "f95" | "f03" | "f08" => capture(
            r"(?im)^\s*(module|program|subroutine|function|type)\s+([\p{L}_][\p{L}\p{N}_]*)",
            Some(1),
            2,
            false,
        )?,
        "pas" | "pp" | "dpr" | "dpk" | "lpr" | "inc" => {
            capture(
                r"(?im)^\s*(unit|program|library|package)\s+([\p{L}_][\p{L}\p{N}_]*)",
                Some(1),
                2,
                false,
            )?;
            capture(
                r"(?im)^\s*(?:class\s+)?(function|procedure|constructor|destructor)\s+(?:[\p{L}_][\p{L}\p{N}_]*\.)?([\p{L}_][\p{L}\p{N}_]*)",
                Some(1),
                2,
                true,
            )?;
        }
        "dart" => capture(
            r"(?m)^\s*(class|mixin|enum|extension|typedef)\s+([\p{L}_][\p{L}\p{N}_]*)",
            Some(1),
            2,
            false,
        )?,
        "cls" | "trigger" => capture(
            r"(?im)^\s*(?:public|private|global|protected|virtual|abstract|with\s+sharing|without\s+sharing|inherited\s+sharing|\s)*(class|interface|enum|trigger)\s+([\p{L}_][\p{L}\p{N}_]*)",
            Some(1),
            2,
            false,
        )?,
        "h" | "m" | "mm" => capture(
            r"(?im)^\s*@(interface|implementation|protocol)\s+([\p{L}_][\p{L}\p{N}_]*)",
            Some(1),
            2,
            false,
        )?,
        "dfm" | "lfm" => capture(
            r"(?im)^\s*(?:object|inherited|inline)\s+([\p{L}_][\p{L}\p{N}_]*)\s*:",
            None,
            1,
            false,
        )?,
        "sln" => capture(
            r#"(?m)^Project\([^\r\n]+\)\s*=\s*\"([^\"]+)\""#,
            None,
            1,
            false,
        )?,
        "slnx" | "csproj" | "fsproj" | "vbproj" | "xaml" | "lpk" | "xml" => {
            capture(
                r#"(?i)<(?:Project|Package|Compile|Page|ApplicationDefinition|ProjectReference)[^>]*(?:Name|Include|Source)=\"([^\"]+)\""#,
                None,
                1,
                false,
            )?;
            capture(r#"(?i)x:Class=\"([^\"]+)\""#, None, 1, false)?;
        }
        _ => {}
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const DEPLOY_GUIDE: &str = include_str!("../../../tests/fixtures/upstream/deploy_guide.md");
    static NEXT_MARKDOWN_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct MarkdownFixture {
        root: PathBuf,
    }

    impl MarkdownFixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "graphoxide-markdown-parity-{}-{}",
                std::process::id(),
                NEXT_MARKDOWN_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).expect("create Markdown parity fixture");
            Self { root }
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create Markdown fixture parent");
            }
            fs::write(&path, contents).expect("write Markdown fixture");
            path
        }
    }

    impl Drop for MarkdownFixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).expect("remove Markdown parity fixture");
        }
    }

    fn deploy_guide() -> Extraction {
        extract_markdown(
            DEPLOY_GUIDE,
            "deploy_guide.md",
            Path::new("deploy_guide.md"),
        )
        .expect("extract vendored upstream Markdown fixture")
    }

    fn markdown_labels(extraction: &Extraction) -> Vec<&str> {
        extraction
            .nodes
            .iter()
            .map(|node| node.label.as_str())
            .collect()
    }

    fn markdown_link_fixture() -> (MarkdownFixture, PathBuf) {
        let fixture = MarkdownFixture::new();
        let base = "packages/coding-standards-csharp";
        let index = fixture.write(
            &format!("{base}/index.md"),
            "# C# Coding Standards\n\n\
             | Topic | Doc |\n| --- | --- |\n\
             | Repository | [C# Repository Standards](./repository.md) |\n\
             | HTTP Client | [C# HTTP Client Standards](http-client.md) |\n\
             | Unit Tests | [C# Unit Test Standards](unit-tests.md) |\n\n\
             See also [external](https://example.com/x) and ![logo](./logo.png).\n\
             Anchor: [section](./repository.md#setup).\n\
             Wikilink: [[http-client]].\n",
        );
        fixture.write(
            &format!("{base}/repository.md"),
            "# C# Repository Standards\nContent.\n",
        );
        fixture.write(
            &format!("{base}/http-client.md"),
            "# C# HTTP Client Standards\nContent.\n",
        );
        fixture.write(
            &format!("{base}/unit-tests.md"),
            "# C# Unit Test Standards\nContent.\n",
        );
        (fixture, index)
    }

    #[test]
    fn upstream_test_markdown_no_error() {
        let _ = deploy_guide();
    }

    #[test]
    fn upstream_test_markdown_finds_headings() {
        let result = deploy_guide();
        let labels = markdown_labels(&result);
        for expected in ["Deploy Guide", "Prerequisites", "Full Deploy", "Rollback"] {
            assert!(
                labels.iter().any(|label| label.contains(expected)),
                "missing heading {expected:?}: {labels:?}"
            );
        }
    }

    #[test]
    fn upstream_test_markdown_finds_nested_heading() {
        let result = deploy_guide();
        let labels = markdown_labels(&result);
        assert!(labels
            .iter()
            .any(|label| label.contains("Database Migration")));
        let full_deploy = result
            .nodes
            .iter()
            .find(|node| node.label == "Full Deploy")
            .expect("Full Deploy heading");
        let migration = result
            .nodes
            .iter()
            .find(|node| node.label == "Database Migration")
            .expect("Database Migration heading");
        assert!(result.edges.iter().any(|edge| {
            edge.relation == "contains"
                && edge.true_source() == full_deploy.id
                && edge.true_target() == migration.id
        }));
    }

    #[test]
    fn upstream_test_markdown_skips_fenced_code_blocks() {
        let result = deploy_guide();
        let labels = markdown_labels(&result);
        assert!(labels.iter().all(|label| !label.starts_with("code:")));
    }

    #[test]
    fn upstream_test_markdown_contains_edges() {
        let result = deploy_guide();
        let contains = result
            .edges
            .iter()
            .filter(|edge| edge.relation == "contains")
            .count();
        assert!(
            contains >= 5,
            "expected at least five contains edges, got {contains}"
        );
    }

    #[test]
    fn upstream_test_markdown_fenced_heading_not_parsed() {
        let source = "# Real Heading\n\n```bash\n## Not A Heading\necho hello\n```\n\n## Another Real Heading\n";
        let result = extract_markdown(source, "fenced.md", Path::new("fenced.md"))
            .expect("extract fenced Markdown");
        let labels = markdown_labels(&result);
        assert!(labels.iter().any(|label| label.contains("Real Heading")));
        assert!(labels
            .iter()
            .any(|label| label.contains("Another Real Heading")));
        assert!(labels.iter().all(|label| !label.contains("Not A Heading")));
    }

    #[test]
    fn upstream_test_markdown_no_dangling_edges() {
        let result = deploy_guide();
        let node_ids: HashSet<_> = result.nodes.iter().map(|node| node.id.as_str()).collect();
        for edge in &result.edges {
            assert!(
                node_ids.contains(edge.true_source()),
                "dangling Markdown edge source: {edge:?}"
            );
        }
    }

    #[test]
    fn upstream_test_markdown_link_edges_emitted() {
        let (_fixture, index) = markdown_link_fixture();
        let source_file = "packages/coding-standards-csharp/index.md";
        let text = fs::read_to_string(&index).expect("read link fixture");
        let result = extract_markdown(&text, source_file, &index).expect("extract link fixture");
        let references: Vec<_> = result
            .edges
            .iter()
            .filter(|edge| edge.relation == "references")
            .collect();
        let targets: HashSet<_> = references.iter().map(|edge| edge.true_target()).collect();
        assert_eq!(references.len(), 3, "duplicate local links were not folded");
        for expected in ["repository", "http_client", "unit_tests"] {
            assert!(
                targets.iter().any(|target| target.contains(expected)),
                "missing local document target {expected}: {targets:?}"
            );
        }
        assert_eq!(
            result.nodes.len(),
            2,
            "local links must not fabricate target document nodes"
        );
    }

    #[test]
    fn upstream_test_markdown_link_skips_external_and_images() {
        let (_fixture, index) = markdown_link_fixture();
        let source_file = "packages/coding-standards-csharp/index.md";
        let text = fs::read_to_string(&index).expect("read link fixture");
        let result = extract_markdown(&text, source_file, &index).expect("extract link fixture");
        for edge in result
            .edges
            .iter()
            .filter(|edge| edge.relation == "references")
        {
            assert!(!edge.true_target().contains("example_com"));
            assert!(!edge.true_target().contains("logo"));
        }
    }

    #[test]
    fn upstream_test_markdown_link_edges_resolve_to_real_nodes() {
        let (fixture, _index) = markdown_link_fixture();
        let extractions = crate::extract_project_with_options(&fixture.root, true)
            .expect("extract Markdown link project");
        let node_ids: HashSet<_> = extractions
            .iter()
            .flat_map(|extraction| extraction.nodes.iter().map(|node| node.id.as_str()))
            .collect();
        let references: Vec<_> = extractions
            .iter()
            .flat_map(|extraction| extraction.edges.iter())
            .filter(|edge| edge.relation == "references")
            .collect();
        assert!(
            !references.is_empty(),
            "expected project-level Markdown links"
        );
        for edge in &references {
            assert!(
                node_ids.contains(edge.true_target()),
                "Markdown reference target is not a real node: {edge:?}"
            );
        }
        let index_id = make_id(&["packages/coding-standards-csharp/index"]);
        let index_targets: HashSet<_> = references
            .iter()
            .filter(|edge| edge.true_source() == index_id)
            .map(|edge| edge.true_target())
            .collect();
        assert_eq!(index_targets.len(), 3, "Markdown hub is under-connected");
    }

    #[test]
    fn unreadable_utf8_is_an_extraction_error() {
        let path = std::env::temp_dir().join(format!(
            "graphoxide-fallback-invalid-utf8-{}.json",
            std::process::id()
        ));
        fs::write(&path, [0xff, 0xfe]).expect("write invalid UTF-8 fixture");
        let result = extract_text(&path, "invalid.json");
        fs::remove_file(&path).expect("remove invalid UTF-8 fixture");

        assert!(
            result.is_err(),
            "invalid UTF-8 must not become an empty graph"
        );
    }

    #[test]
    fn malformed_json_is_an_extraction_error() {
        let result = extract_json("{not valid json", "broken.json", Path::new("broken.json"));
        assert!(
            result.is_err(),
            "malformed JSON must not become a file-only graph"
        );
    }

    #[test]
    fn fallback_nodes_do_not_claim_ast_origin() {
        let regex_node = node("demo_symbol".into(), "symbol", "demo.sql", 1, "table");
        assert_eq!(
            regex_node
                .extra
                .get("_origin")
                .and_then(|value| value.as_str()),
            Some("fallback")
        );

        let structured = extract_json(
            r#"{"name":"demo"}"#,
            "config.json",
            Path::new("config.json"),
        )
        .expect("extract valid JSON");
        assert!(structured.nodes.iter().all(|node| {
            node.extra.get("_origin").and_then(|value| value.as_str()) == Some("fallback")
        }));
    }

    #[test]
    fn json_array_shapes_do_not_emit_duplicate_node_ids() {
        let result = extract_json(
            r#"[{"meta":{"first":1}},{"meta":{"second":2}}]"#,
            "records.json",
            Path::new("records.json"),
        )
        .expect("extract valid JSON array");
        let unique: HashSet<_> = result.nodes.iter().map(|node| node.id.as_str()).collect();

        assert_eq!(unique.len(), result.nodes.len());
        assert_eq!(
            result
                .nodes
                .iter()
                .filter(|node| node.label == "meta")
                .count(),
            1
        );
        for label in ["first", "second"] {
            assert!(
                result.nodes.iter().any(|node| node.label == label),
                "deduplication discarded the distinct {label} child"
            );
        }
    }

    #[test]
    fn yaml_keys_do_not_masquerade_as_imports_or_empty_nodes() {
        let path = std::env::temp_dir().join(format!(
            "graphoxide-fallback-workflow-{}.yml",
            std::process::id()
        ));
        fs::write(
            &path,
            "from: base-image\nuses: actions/checkout@v4\ninclude: value\n",
        )
        .expect("write YAML fixture");
        let result = extract_text(&path, ".github/workflows/demo.yml")
            .expect("extract YAML fixture without inventing imports");
        fs::remove_file(&path).expect("remove YAML fixture");

        assert!(result.nodes.iter().all(|node| !node.id.is_empty()));
        assert!(result.nodes.iter().all(|node| node
            .extra
            .get("type")
            .and_then(|value| value.as_str())
            != Some("module")));
    }

    #[test]
    fn structured_fallbacks_find_tier_two_and_three_symbols() {
        let cases = [
            ("schema.sql", "CREATE TABLE users (id int);", "users"),
            (
                "main.tf",
                "resource \"aws_s3_bucket\" \"assets\" {}",
                "aws_s3_bucket",
            ),
            ("build.ps1", "function Invoke-Build { }", "Invoke-Build"),
            ("chip.sv", "module counter(input clk); endmodule", "counter"),
            ("main.pas", "procedure RunApp; begin end;", "RunApp"),
            (
                "Demo.trigger",
                "trigger Demo on Account (before insert) {}",
                "Demo",
            ),
            ("App.xaml", "<Application x:Class=\"Demo.App\">", "Demo.App"),
        ];
        for (path, source, expected) in cases {
            let symbols = special_definitions(Path::new(path), source).unwrap();
            assert!(
                symbols.iter().any(|(_, name, _, _)| name == expected),
                "{path}: {symbols:?}"
            );
        }
    }

    #[test]
    fn manifests_emit_only_the_local_package_node() {
        let fixture = MarkdownFixture::new();
        let path = fixture.write(
            "pyproject.toml",
            "[project]\nname = \"demo\"\ndependencies = [\"serde>=1\"]",
        );
        let result = extract_text(&path, "pyproject.toml").unwrap();
        assert_eq!(result.nodes.len(), 1);
        assert!(result
            .edges
            .iter()
            .all(|edge| edge.relation == "depends_on"));
    }

    #[test]
    fn known_mcp_basename_routes_before_generic_json() {
        let fixture = MarkdownFixture::new();
        let path = fixture.write(
            ".mcp.json",
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/fixtures/upstream/sample.mcp.json"
            )),
        );
        let result =
            extract_text(&path, ".mcp.json").expect("extract MCP config through dispatcher");
        let kinds: HashSet<_> = result
            .nodes
            .iter()
            .filter_map(|node| {
                node.extra
                    .get("metadata")
                    .and_then(|metadata| metadata.get("mcp_kind"))
                    .and_then(serde_json::Value::as_str)
            })
            .collect();
        for expected in [
            "mcp_config_file",
            "mcp_server",
            "mcp_command",
            "mcp_package",
            "env_var",
        ] {
            assert!(
                kinds.contains(expected),
                "missing MCP node kind {expected}; actual={kinds:?}; nodes={:?}",
                result.nodes
            );
        }
        assert!(result.nodes.iter().all(|node| node
            .extra
            .get("type")
            .and_then(|value| value.as_str())
            != Some("json_key")));
    }

    #[test]
    fn mcp_dispatch_accepts_jsonc_without_persisting_values() {
        let fixture = MarkdownFixture::new();
        let path = fixture.write(
            ".mcp.json",
            r#"{
                // Project MCP files are commonly edited as JSONC.
                "mcpServers": {
                    "local": {
                        "command": "npx",
                        "args": ["-y", "@scope/local-mcp", "secret-argument",],
                        "env": { "LOCAL_TOKEN": "secret-value", },
                    },
                },
            }"#,
        );

        let result = extract_text(&path, ".mcp.json").expect("extract JSONC MCP config");
        let serialized = serde_json::to_string(&result).expect("serialize extraction");
        assert!(serialized.contains("local"));
        assert!(!serialized.contains("secret-argument"));
        assert!(!serialized.contains("secret-value"));
    }

    #[test]
    fn mcp_dispatch_never_persists_argument_or_environment_values() {
        let fixture = MarkdownFixture::new();
        let path = fixture.write(
            ".mcp.json",
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/fixtures/upstream/sample.mcp.json"
            )),
        );
        let result =
            extract_text(&path, ".mcp.json").expect("extract MCP config through dispatcher");
        let serialized = serde_json::to_string(&result).expect("serialize extraction");
        for secret_or_path in ["ghp_PLACEHOLDER_NOT_A_REAL_TOKEN", "/tmp/workspace"] {
            assert!(!serialized.contains(secret_or_path));
        }
    }
}
