//! Regex and structured-data extraction for languages without a compiled grammar.

use crate::project_path::{
    normalize_project_path, source_relative_project_path, ProjectPath,
    EXACT_PROJECT_RELATIVE_PLACEHOLDER,
};
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
    path::{Path, PathBuf},
    sync::LazyLock,
};

static GENERIC_DEFINITIONS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^\s*(?:(?:pub(?:lic|lic static)?|private|protected|internal|export|abstract|static|async|final|open|partial)\s+)*(class|interface|struct|enum|trait|protocol|module|namespace|type|def|fn|fun|function|func|sub|procedure)\s+([\p{L}_][\p{L}\p{N}_]*)",
    )
    .expect("valid generic definition regex")
});

static GENERIC_STATEMENT_IMPORTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*(?:import|from|use|using)\s+([\p{L}\p{N}_./:@-]+)(?:\s|;|$)")
        .expect("valid generic statement import regex")
});

static GENERIC_LITERAL_CALL_IMPORTS: LazyLock<[Regex; 4]> = LazyLock::new(|| {
    [
        Regex::new(
            r#"(?m)^\s*(?:require|include)\s*\(\s*'([\p{L}\p{N}_./:@-]+)'\s*\)\s*;?\s*(?:(?://|--|#)[^\r\n]*)?$"#,
        ),
        Regex::new(
            r#"(?m)^\s*(?:require|include)\s*\(\s*\"([\p{L}\p{N}_./:@-]+)\"\s*\)\s*;?\s*(?:(?://|--|#)[^\r\n]*)?$"#,
        ),
        Regex::new(
            r#"(?m)^\s*(?:require|include)\s+'([\p{L}\p{N}_./:@-]+)'\s*;?\s*(?:(?://|--|#)[^\r\n]*)?$"#,
        ),
        Regex::new(
            r#"(?m)^\s*(?:require|include)\s+\"([\p{L}\p{N}_./:@-]+)\"\s*;?\s*(?:(?://|--|#)[^\r\n]*)?$"#,
        ),
    ]
    .map(|regex| regex.expect("valid generic literal call import regex"))
});

static GENERIC_CALLS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([\p{L}_][\p{L}\p{N}_]*)\s*\(").expect("valid generic call regex")
});

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
    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
    {
        return crate::json_config::extract_json_config(path, source_file);
    }
    let bytes = fs::read(path)?;
    extract_text_from_bytes(path, source_file, &bytes, true)
}

/// Extract fallback text facts from an already-read source buffer.
///
/// The byte-oriented path never performs filesystem I/O. Format-specific
/// extractors that still require a path-owned dependency are deliberately
/// deferred to their dedicated byte entry points instead of silently reading
/// from a compute worker.
pub fn extract_text_bytes(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
) -> anyhow::Result<Extraction> {
    if is_mcp_config_path(path) {
        return extract_mcp_config_bytes(path, source_file, bytes);
    }
    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("dmi"))
    {
        return crate::compat::extract_dmi_bytes(path, source_file, bytes);
    }
    if crate::manifest_ingest::is_package_manifest_path(path) {
        return Ok(crate::manifest_ingest::extract_package_manifest_bytes(
            path,
            source_file,
            bytes,
        ));
    }
    extract_text_from_bytes(path, source_file, bytes, false)
}

fn extract_text_from_bytes(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
    allow_path_dependent_compat: bool,
) -> anyhow::Result<Extraction> {
    let text = crate::bytes::validate_utf8(bytes)?;
    let line_index = crate::bytes::LineIndex::new(bytes);
    if path
        .extension()
        .and_then(|v| v.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
    {
        return crate::json_config::extract_json_config_bytes(path, source_file, bytes);
    }
    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            matches!(extension.to_ascii_lowercase().as_str(), "md" | "markdown")
        })
    {
        return if allow_path_dependent_compat {
            extract_markdown(text, source_file, path)
        } else {
            extract_markdown_with_path_probe(text, source_file, path, false)
        };
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !allow_path_dependent_compat && matches!(extension.as_str(), "m" | "mm" | "h") {
        return crate::compat::extract_objc_bytes(path, text, source_file);
    }
    if (allow_path_dependent_compat || extension != "xaml")
        && let Some(extraction) =
            crate::compat::extract_compat(path, text, source_file, allow_path_dependent_compat)?
    {
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
    let mut labels = HashMap::new();
    for capture in GENERIC_DEFINITIONS.captures_iter(text) {
        let kind = &capture[1];
        let name = &capture[2];
        let id = make_id(&[&stem, name]);
        if !seen.insert(id.clone()) {
            continue;
        }
        let line = line_index.line_of(capture.get(0).unwrap().start());
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
    for (kind, name, start, function) in special_definitions(path, text)? {
        let id = make_id(&[&stem, &name]);
        if !seen.insert(id.clone()) {
            continue;
        }
        let line = line_index.line_of(start);
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
    for (start, module) in generic_import_facts(text) {
        let Some(target) =
            classify_generic_import(source_file, module, allow_path_dependent_compat)
        else {
            continue;
        };
        let line = line_index.line_of(start);
        let (identity, label, target_file) = match target {
            GenericImportTarget::Bare(module) => (
                module.to_owned(),
                module
                    .rsplit(['/', ':', '.'])
                    .find(|value| !value.is_empty())
                    .unwrap_or(module)
                    .to_owned(),
                None,
            ),
            GenericImportTarget::ProjectRelative(logical) => {
                let identity = Path::new(&logical)
                    .with_extension("")
                    .to_string_lossy()
                    .into_owned();
                let label = Path::new(&logical)
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .filter(|value| !value.is_empty())
                    .unwrap_or(&logical)
                    .to_owned();
                (identity, label, Some(logical))
            }
        };
        let id = make_id(&[&identity]);
        if id.is_empty() {
            continue;
        }
        if seen.insert(id.clone()) {
            let mut module_node = node(id.clone(), label, source_file, line, "module");
            if let Some(target_file) = target_file.as_deref() {
                module_node
                    .extra
                    .insert(EXACT_PROJECT_RELATIVE_PLACEHOLDER.into(), true.into());
                module_node
                    .extra
                    .insert("target_file".into(), target_file.into());
            }
            nodes.push(module_node);
        }
        let mut import = edge(
            file_id.clone(),
            id,
            "imports",
            source_file,
            line,
            Confidence::Extracted,
        );
        if let Some(target_file) = target_file {
            import
                .extra
                .insert("target_file".into(), target_file.into());
            import
                .extra
                .insert(EXACT_PROJECT_RELATIVE_PLACEHOLDER.into(), true.into());
        }
        edges.push(import);
    }
    let keywords = [
        "if", "for", "while", "switch", "catch", "return", "class", "function", "func", "fn",
        "def", "sizeof", "typeof",
    ];
    for capture in GENERIC_CALLS.captures_iter(text) {
        let name = &capture[1];
        if keywords.contains(&name) {
            continue;
        }
        if let Some(target) = labels.get(&name.to_lowercase()) {
            let line = line_index.line_of(capture.get(0).unwrap().start());
            if let Some(source) = nearest_definition(&nodes, line)
                && source != target
            {
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
    Ok(Extraction {
        nodes,
        edges,
        hyperedges: Vec::new(),
    })
}

fn generic_import_facts(text: &str) -> Vec<(usize, &str)> {
    let mut facts = GENERIC_STATEMENT_IMPORTS
        .captures_iter(text)
        .map(|capture| {
            (
                capture.get(0).expect("generic statement import").start(),
                capture.get(1).expect("generic statement module").as_str(),
            )
        })
        .collect::<Vec<_>>();
    for regex in GENERIC_LITERAL_CALL_IMPORTS.iter() {
        facts.extend(regex.captures_iter(text).map(|capture| {
            (
                capture.get(0).expect("generic literal import").start(),
                capture.get(1).expect("generic literal module").as_str(),
            )
        }));
    }
    facts.sort_unstable_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(right.1)));
    facts.dedup();
    facts
}

enum GenericImportTarget<'a> {
    Bare(&'a str),
    ProjectRelative(String),
}

fn classify_generic_import<'a>(
    source_file: &str,
    module: &'a str,
    allow_path_entrypoint_compat: bool,
) -> Option<GenericImportTarget<'a>> {
    if module.is_empty()
        || module.trim() != module
        || module.starts_with(['/', '\\'])
        || module.contains('\\')
        || module.contains("//")
        || module.chars().any(char::is_control)
    {
        return None;
    }

    let bytes = module.as_bytes();
    if bytes.first().is_some_and(u8::is_ascii_alphabetic) && bytes.get(1) == Some(&b':') {
        return None;
    }

    if module.starts_with('.') {
        if !(module.starts_with("./") || module.starts_with("../")) {
            return None;
        }
        let logical_source = normalize_project_path(source_file)
            .filter(|source| !source.is_empty())
            .or_else(|| {
                if !allow_path_entrypoint_compat {
                    return None;
                }
                let basename = source_file.rsplit(['/', '\\']).next()?;
                normalize_project_path(basename).filter(|source| !source.is_empty())
            })?;
        return match source_relative_project_path(&logical_source, module)? {
            ProjectPath::Contained(logical) => Some(GenericImportTarget::ProjectRelative(logical)),
            ProjectPath::EscapesRoot(_) => None,
        };
    }

    if (module.contains('/') && module.contains(':'))
        || module
            .split('/')
            .any(|component| matches!(component, "" | "." | ".."))
    {
        return None;
    }

    Some(GenericImportTarget::Bare(module))
}

/// Extract an MCP configuration, degrading to generic JSON when a valid
/// document with a recognised basename turns out not to contain a server map.
///
/// The path entrypoint retains upstream's compatibility behavior and reports
/// genuine file, size, and parse faults. The byte-only entrypoint below cannot
/// reopen or reclassify input, so malformed MCP-like bytes fail closed to
/// redacted metadata instead.
fn extract_mcp_config(path: &Path, source_file: &str) -> anyhow::Result<Extraction> {
    const MAX_BYTES: u64 = 1_048_576;
    let metadata = fs::metadata(path)?;
    anyhow::ensure!(metadata.len() <= MAX_BYTES, "mcp config too large to index");
    let bytes = fs::read(path)?;
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("read MCP configuration {source_file} as UTF-8"))?;
    let document = graphoxide_core::parse_jsonc(text)
        .with_context(|| format!("parse MCP configuration {source_file}"))?;
    let Some(root) = document.as_object() else {
        tracing::warn!(
            "{source_file}: mcp config root is not an object; indexing as generic JSON instead"
        );
        return crate::json_config::extract_json_config(path, source_file);
    };
    if mcp_server_map(root).is_none() {
        tracing::warn!(
            "{source_file}: mcp config has no server map; indexing as generic JSON instead"
        );
        return crate::json_config::extract_json_config(path, source_file);
    }
    extract_mcp_config_bytes(path, source_file, &bytes)
}

fn extract_mcp_config_bytes(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
) -> anyhow::Result<Extraction> {
    const MAX_BYTES: usize = 1_048_576;
    const MAX_SERVERS: usize = 200;
    if bytes.len() > MAX_BYTES {
        return Ok(non_mcp_config_fallback(
            path,
            source_file,
            "exceeds safe byte limit",
        ));
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Ok(non_mcp_config_fallback(
            path,
            source_file,
            "content is not UTF-8",
        ));
    };
    let Ok(document) = graphoxide_core::parse_jsonc(text) else {
        return Ok(non_mcp_config_fallback(
            path,
            source_file,
            "content is not valid JSON or JSONC",
        ));
    };
    let Some(root) = document.as_object() else {
        return Ok(non_mcp_config_fallback(
            path,
            source_file,
            "root is not an object",
        ));
    };
    let Some(servers) = mcp_server_map(root) else {
        return Ok(non_mcp_config_fallback(
            path,
            source_file,
            "no MCP server map",
        ));
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

    for (server_ordinal, (server_name, value)) in servers.iter().take(MAX_SERVERS).enumerate() {
        if server_name.is_empty() {
            continue;
        }
        let Some(spec) = value.as_object() else {
            continue;
        };
        let server_name_redacted = crate::structured::structured_string_is_sensitive(server_name);
        let server_id = if server_name_redacted {
            make_id(&[&stem, "redacted_mcp_server", &server_ordinal.to_string()])
        } else {
            make_id(&[&stem, "mcp_server", server_name])
        };
        if server_name_redacted {
            insert_mcp_redacted_node(
                &mut nodes,
                &mut seen_nodes,
                server_id.clone(),
                "mcp_server",
                source_file,
            );
        } else {
            insert_mcp_node(
                &mut nodes,
                &mut seen_nodes,
                server_id.clone(),
                server_name,
                "mcp_server",
                source_file,
            );
        }
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
            let command_redacted = crate::structured::structured_string_is_sensitive(command);
            let command_id = if command_redacted {
                make_id(&[&server_id, "redacted_mcp_command"])
            } else {
                make_id(&["mcp_command", command])
            };
            if command_redacted {
                insert_mcp_redacted_node(
                    &mut nodes,
                    &mut seen_nodes,
                    command_id.clone(),
                    "mcp_command",
                    source_file,
                );
            } else {
                insert_mcp_node(
                    &mut nodes,
                    &mut seen_nodes,
                    command_id.clone(),
                    command,
                    "mcp_command",
                    source_file,
                );
            }
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
            let package_redacted = crate::structured::structured_string_is_sensitive(&package);
            let package_id = if package_redacted {
                make_id(&[&server_id, "redacted_mcp_package"])
            } else {
                make_id(&["mcp_package", &package])
            };
            if package_redacted {
                insert_mcp_redacted_node(
                    &mut nodes,
                    &mut seen_nodes,
                    package_id.clone(),
                    "mcp_package",
                    source_file,
                );
            } else {
                insert_mcp_node(
                    &mut nodes,
                    &mut seen_nodes,
                    package_id.clone(),
                    &package,
                    "mcp_package",
                    source_file,
                );
            }
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
            for (environment_ordinal, name) in environment
                .keys()
                .filter(|name| !name.is_empty())
                .enumerate()
            {
                let name_redacted = crate::structured::structured_string_is_sensitive(name);
                let environment_id = if name_redacted {
                    make_id(&[
                        &server_id,
                        "redacted_env_var",
                        &environment_ordinal.to_string(),
                    ])
                } else {
                    make_id(&["env_var", name])
                };
                if name_redacted {
                    insert_mcp_redacted_node(
                        &mut nodes,
                        &mut seen_nodes,
                        environment_id.clone(),
                        "env_var",
                        source_file,
                    );
                } else {
                    insert_mcp_node(
                        &mut nodes,
                        &mut seen_nodes,
                        environment_id.clone(),
                        name,
                        "env_var",
                        source_file,
                    );
                }
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

fn non_mcp_config_fallback(path: &Path, source_file: &str, reason: &'static str) -> Extraction {
    tracing::warn!(
        source_file,
        reason,
        "recognized MCP filename does not contain an MCP configuration; retaining metadata only"
    );
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(source_file);
    let mut file = node(
        make_id(&[source_file]),
        sanitize_label(filename),
        source_file,
        1,
        "json_config_file",
    );
    file.extra.insert(
        "metadata".into(),
        serde_json::json!({
            "parse_status": "not_mcp_configuration",
            "reason": reason,
        }),
    );
    Extraction {
        nodes: vec![file],
        edges: Vec::new(),
        hyperedges: Vec::new(),
    }
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

fn insert_mcp_redacted_node(
    nodes: &mut Vec<Node>,
    seen: &mut HashSet<String>,
    id: String,
    kind: &str,
    source_file: &str,
) {
    if id.is_empty() || !seen.insert(id.clone()) {
        return;
    }
    let mut node = mcp_node(
        id,
        crate::structured::REDACTED_STRUCTURED_VALUE,
        kind,
        source_file,
    );
    node.extra
        .insert("structured_value_redacted".into(), true.into());
    node.extra
        .insert("structured_label_redacted".into(), true.into());
    node.extra
        .insert("structured_value_type".into(), "string".into());
    node.extra
        .insert("structured_redaction_policy".into(), 1_u64.into());
    nodes.push(node);
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
    extract_markdown_with_path_probe(text, source_file, path, true)
}

fn extract_markdown_with_path_probe(
    text: &str,
    source_file: &str,
    path: &Path,
    probe_target_paths: bool,
) -> anyhow::Result<Extraction> {
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
                let Some(target) =
                    resolve_markdown_link(raw, source_file, path, probe_target_paths)
                else {
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

        if let Some(capture) = heading.captures(line_text) {
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
            continue;
        }

        let text = line_text.trim();
        if text.is_empty() {
            continue;
        }
        let paragraph_id = make_id(&[&stem, "document_paragraph", &line.to_string()]);
        if !seen_ids.insert(paragraph_id.clone()) {
            continue;
        }
        let (text, truncated) = bounded_markdown_evidence(text);
        let mut paragraph = node(
            paragraph_id.clone(),
            "paragraph",
            source_file,
            line,
            "document",
        );
        paragraph
            .extra
            .insert("type".into(), "document_paragraph".into());
        paragraph
            .extra
            .insert("structured_text".into(), text.into());
        paragraph
            .extra
            .insert("structured_text_type".into(), "string".into());
        if truncated {
            paragraph
                .extra
                .insert("structured_text_truncated".into(), true.into());
        }
        nodes.push(paragraph);
        let parent = heading_stack
            .last()
            .map(|(_, id)| id.clone())
            .unwrap_or_else(|| file_id.clone());
        edges.push(edge(
            parent,
            paragraph_id,
            "contains",
            source_file,
            line,
            Confidence::Extracted,
        ));
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

const MAX_MARKDOWN_EVIDENCE_BYTES: usize = 4 * 1024;

fn bounded_markdown_evidence(text: &str) -> (String, bool) {
    if text.len() <= MAX_MARKDOWN_EVIDENCE_BYTES {
        return (text.to_owned(), false);
    }
    let mut end = MAX_MARKDOWN_EVIDENCE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

fn resolve_markdown_link(
    raw: &str,
    source_file: &str,
    physical_source: &Path,
    probe_target_paths: bool,
) -> Option<MarkdownTarget> {
    let raw = raw.trim();
    if raw.is_empty()
        || raw.chars().any(|character| {
            character.is_control() || matches!(character, '$' | '`' | '{' | '}' | '<' | '>' | '\\')
        })
    {
        return None;
    }
    let target = raw.split('#').next()?.split('?').next()?.trim();
    if target.is_empty() {
        return None;
    }
    let bytes = target.as_bytes();
    if target.starts_with('/') || bytes.get(1) == Some(&b':') || target.contains(':') {
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

    let logical = normalize_portable_markdown_path(source_file, target)?;
    let logical_without_extension = logical.with_extension("");
    let id = make_id(&[&logical_without_extension
        .to_string_lossy()
        .replace('\\', "/")]);
    if id.is_empty() {
        return None;
    }

    let existing_source_file = if probe_target_paths {
        let physical = physical_source
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(raw_path);
        physical
            .is_file()
            .then(|| logical.to_string_lossy().replace('\\', "/"))
    } else {
        None
    };
    Some(MarkdownTarget {
        id,
        existing_source_file,
    })
}

fn normalize_portable_markdown_path(source_file: &str, target: &str) -> Option<PathBuf> {
    let source_file = source_file.replace('\\', "/");
    let mut parts = Vec::<String>::new();
    for part in source_file
        .rsplit_once('/')
        .map_or("", |(parent, _)| parent)
        .split('/')
    {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part.to_owned()),
        }
    }
    for part in target.split('/') {
        match part {
            "" => return None,
            "." => {}
            ".." => {
                parts.pop()?;
            }
            _ if portable_markdown_path_segment(part) => parts.push(part.to_owned()),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.iter().collect())
}

fn portable_markdown_path_segment(segment: &str) -> bool {
    let device_stem = segment
        .split('.')
        .next()
        .unwrap_or(segment)
        .trim_end()
        .to_ascii_lowercase();
    !segment.is_empty()
        && !segment.ends_with(['.', ' '])
        && !matches!(
            device_stem.as_str(),
            "con"
                | "prn"
                | "aux"
                | "nul"
                | "com1"
                | "com2"
                | "com3"
                | "com4"
                | "com5"
                | "com6"
                | "com7"
                | "com8"
                | "com9"
                | "lpt1"
                | "lpt2"
                | "lpt3"
                | "lpt4"
                | "lpt5"
                | "lpt6"
                | "lpt7"
                | "lpt8"
                | "lpt9"
        )
        && !segment.chars().any(|character| {
            character.is_control()
                || matches!(character, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*')
        })
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
    fn markdown_retains_document_text_for_wiki_evidence() {
        let result = extract_markdown(
            "# Guide\n\nUse the approved installation command.\n",
            "guide.md",
            Path::new("guide.md"),
        )
        .expect("extract Markdown");
        assert!(result.nodes.iter().any(|node| {
            node.extra.get("type").and_then(serde_json::Value::as_str) == Some("document_paragraph")
                && node.extra.get("structured_text")
                    == Some(&"Use the approved installation command.".into())
        }));
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
        let node_ids = result
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<HashSet<_>>();
        assert!(
            targets.iter().all(|target| !node_ids.contains(target)),
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
    fn generic_call_imports_require_quoted_literal_evidence() {
        let extraction = extract_text_bytes(
            Path::new("consumer.lua"),
            "consumer.lua",
            br#"import static.module
from package.core import value
use namespace:item
require('quoted.module')
include "./literal.lua"
require('commented.module'); -- static trailing comment
require(module_name)
include(foo)
require(get_module())
require("${module_name}")
require("prefix" .. suffix)
include "extra.module", bar
require('unexpected.module') unexpected
"#,
        )
        .expect("extract generic imports");
        let targets = extraction
            .edges
            .iter()
            .filter(|edge| edge.relation == "imports")
            .map(|edge| edge.true_target())
            .collect::<HashSet<_>>();
        assert_eq!(
            targets,
            HashSet::from([
                "static_module",
                "package_core",
                "namespace_item",
                "quoted_module",
                "literal",
                "commented_module",
            ])
        );
        assert!(targets.is_disjoint(&HashSet::from([
            "module_name",
            "foo",
            "get_module",
            "prefix",
            "extra_module",
            "unexpected_module",
        ])));
        let literal = extraction
            .nodes
            .iter()
            .find(|node| node.id == "literal")
            .expect("project-relative literal module");
        assert_eq!(literal.label, "literal");
        assert_eq!(
            literal
                .extra
                .get(EXACT_PROJECT_RELATIVE_PLACEHOLDER)
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn generic_project_relative_imports_use_safe_logical_file_identity() {
        let extraction = extract_text_bytes(
            Path::new("/graphoxide-missing-project/src/deep/consumer.lua"),
            "src/deep/consumer.lua",
            br#"include "../lib/worker.lua"
include "../../../victim.lua"
include "/victim.lua"
include "//server/share/victim.lua"
include "C:/victim.lua"
include "./aux:worker.lua"
include "./CON.lua"
include "./bad//worker.lua"
import static.module
use namespace:item
require('@scope/package')
"#,
        )
        .expect("extract safe generic imports");
        let imports = extraction
            .edges
            .iter()
            .filter(|edge| edge.relation == "imports")
            .collect::<Vec<_>>();
        assert_eq!(
            imports
                .iter()
                .map(|edge| edge.true_target())
                .collect::<HashSet<_>>(),
            HashSet::from([
                "src_lib_worker",
                "static_module",
                "namespace_item",
                "scope_package",
            ])
        );

        let worker = extraction
            .nodes
            .iter()
            .find(|node| node.id == "src_lib_worker")
            .expect("source-relative worker module");
        assert_eq!(worker.label, "worker", "file extension became the label");
        assert_eq!(
            worker
                .extra
                .get(EXACT_PROJECT_RELATIVE_PLACEHOLDER)
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        let worker_import = imports
            .iter()
            .find(|edge| edge.true_target() == "src_lib_worker")
            .expect("worker import edge");
        assert_eq!(
            worker_import
                .extra
                .get("target_file")
                .and_then(serde_json::Value::as_str),
            Some("src/lib/worker.lua")
        );
    }

    #[test]
    fn generic_path_entrypoint_scopes_relative_compatibility_to_a_basename() {
        let directory = std::env::temp_dir().join(format!(
            "graphoxide-generic-path-{}-{}",
            std::process::id(),
            NEXT_MARKDOWN_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).expect("create generic path fixture");
        let path = directory.join("consumer.lua");
        fs::write(
            &path,
            "include \"./worker.lua\"\ninclude \"../outside.lua\"\n",
        )
        .expect("write generic path fixture");
        let source_file = path.to_string_lossy().into_owned();
        let extraction = extract_text(&path, &source_file).expect("extract path-owned source");
        fs::remove_file(&path).expect("remove generic path fixture");
        fs::remove_dir(&directory).expect("remove generic path fixture directory");

        let imports = extraction
            .edges
            .iter()
            .filter(|edge| edge.relation == "imports")
            .map(|edge| edge.true_target())
            .collect::<Vec<_>>();
        assert_eq!(imports, ["worker"]);
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

    #[test]
    fn every_registered_mcp_filename_uses_the_redacting_fallback() {
        let command_secret = "ghp_1234567890abcdef";
        let payload = r#"{
            "mcpServers": {
                "private": {
                    "command": "ghp_1234567890abcdef",
                    "args": ["--token", "secret-argument"],
                    "env": {"TOKEN": "secret-environment"}
                }
            }
        }"#;
        for filename in [
            ".mcp.json",
            "mcp.json",
            "mcp_servers.json",
            "claude_desktop_config.json",
        ] {
            let extraction = extract_text_bytes(Path::new(filename), filename, payload.as_bytes())
                .unwrap_or_else(|error| panic!("{filename} did not use MCP extraction: {error:#}"));
            let rendered = serde_json::to_string(&extraction).expect("serialize extraction");
            assert!(!rendered.contains(command_secret), "{filename}");
            assert!(!rendered.contains("secret-argument"), "{filename}");
            assert!(!rendered.contains("secret-environment"), "{filename}");
            assert!(extraction.nodes.iter().any(|node| {
                node.label == crate::structured::REDACTED_STRUCTURED_VALUE
                    && node.extra.get("structured_value_redacted")
                        == Some(&serde_json::Value::Bool(true))
            }));
        }
    }

    #[test]
    fn mcp_value_and_secret_like_name_ids_are_independent_of_raw_credentials() {
        fn extract_private(
            server: &str,
            command: &str,
            package: &str,
            environment_name: &str,
        ) -> Extraction {
            let environment = serde_json::Map::from_iter([(
                environment_name.to_owned(),
                serde_json::Value::String("ordinary-secret-value".into()),
            )]);
            let servers = serde_json::Map::from_iter([(
                server.to_owned(),
                serde_json::json!({
                    "command": command,
                    "args": ["-y", package],
                    "env": environment,
                }),
            )]);
            let payload = serde_json::json!({
                "mcpServers": servers,
            });
            extract_text_bytes(
                Path::new(".mcp.json"),
                ".mcp.json",
                serde_json::to_vec(&payload)
                    .expect("serialize MCP fixture")
                    .as_slice(),
            )
            .expect("extract MCP fixture")
        }

        let first_values = [
            "ghp_aaaaaaaaaaaaaaaa",
            "sk_live_aaaaaaaaaaaaaaaa",
            "rk_live_aaaaaaaaaaaaaaaa",
        ];
        let second_values = [
            "ghp_bbbbbbbbbbbbbbbb",
            "sk_live_bbbbbbbbbbbbbbbb",
            "rk_live_bbbbbbbbbbbbbbbb",
        ];
        let safe_package = "@scope/private-mcp";
        let first = extract_private(
            first_values[0],
            first_values[1],
            safe_package,
            first_values[2],
        );
        let second = extract_private(
            second_values[0],
            second_values[1],
            safe_package,
            second_values[2],
        );
        let first_rendered = serde_json::to_string(&first).expect("serialize first extraction");
        let second_rendered = serde_json::to_string(&second).expect("serialize second extraction");
        for secret in first_values {
            assert!(!first_rendered.contains(secret), "leaked {secret}");
        }
        for secret in second_values {
            assert!(!second_rendered.contains(secret), "leaked {secret}");
        }
        assert_eq!(first_rendered, second_rendered);
        assert!(first_rendered.contains(safe_package));
        assert!(
            first
                .nodes
                .iter()
                .filter(|node| {
                    node.extra.get("structured_label_redacted")
                        == Some(&serde_json::Value::Bool(true))
                })
                .count()
                >= 3
        );
    }

    #[test]
    fn mcp_safe_command_and_package_remain_useful_structure() {
        let extraction = extract_text_bytes(
            Path::new(".mcp.json"),
            ".mcp.json",
            br#"{"mcpServers":{"graphoxide":{"command":"graphoxide","args":["-y","@graphoxide/mcp@1.2.3"],"env":{"SAFE_MODE":"ordinary-omitted-value"}}}}"#,
        )
        .expect("extract safe MCP structure");
        let labels = extraction
            .nodes
            .iter()
            .map(|node| node.label.as_str())
            .collect::<HashSet<_>>();
        assert!(labels.contains("graphoxide"));
        assert!(labels.contains("@graphoxide/mcp"));
        let rendered = serde_json::to_string(&extraction).expect("serialize extraction");
        assert!(!rendered.contains("ordinary-omitted-value"));
    }

    #[test]
    fn mcp_filename_without_server_map_is_safe_metadata_instead_of_an_error() {
        let payload = br#"{"someOtherKey":{"token":"sentinel-secret"}}"#;
        let extraction = extract_text_bytes(Path::new("mcp.json"), "mcp.json", payload)
            .expect("an unrelated JSON document named mcp.json must not abort extraction");
        assert_eq!(extraction.nodes.len(), 1);
        assert_eq!(
            extraction.nodes[0].extra["metadata"]["parse_status"],
            "not_mcp_configuration"
        );
        let rendered = serde_json::to_string(&extraction).expect("serialize extraction");
        assert!(!rendered.contains("sentinel-secret"));
    }

    #[test]
    fn mcp_filename_with_non_object_root_is_safe_metadata_instead_of_an_error() {
        let payload = br#"[{"token":"sentinel-secret"}]"#;
        let extraction =
            extract_text_bytes(Path::new(".vscode/mcp.json"), ".vscode/mcp.json", payload)
                .expect("a non-object MCP filename must not abort extraction");
        assert_eq!(extraction.nodes.len(), 1);
        assert_eq!(
            extraction.nodes[0].extra["metadata"]["parse_status"],
            "not_mcp_configuration"
        );
        let rendered = serde_json::to_string(&extraction).expect("serialize extraction");
        assert!(!rendered.contains("sentinel-secret"));
    }

    #[test]
    fn malformed_or_non_utf8_mcp_filename_is_safe_metadata_instead_of_an_error() {
        for payload in [
            b"{not-json sentinel-secret".as_slice(),
            b"\xffsentinel-secret",
        ] {
            let extraction = extract_text_bytes(Path::new("mcp.json"), "mcp.json", payload)
                .expect("malformed MCP-like input must not abort extraction");
            assert_eq!(extraction.nodes.len(), 1);
            assert_eq!(
                extraction.nodes[0].extra["metadata"]["parse_status"],
                "not_mcp_configuration"
            );
            let rendered = serde_json::to_string(&extraction).expect("serialize extraction");
            assert!(!rendered.contains("sentinel-secret"));
        }
    }

    #[test]
    fn oversized_mcp_filename_is_safe_metadata_instead_of_an_error() {
        let mut payload = vec![b' '; 1_048_577];
        payload[..15].copy_from_slice(b"sentinel-secret");
        let extraction = extract_text_bytes(Path::new("mcp.json"), "mcp.json", &payload)
            .expect("oversized MCP-like input must not abort extraction");
        assert_eq!(extraction.nodes.len(), 1);
        assert_eq!(
            extraction.nodes[0].extra["metadata"]["parse_status"],
            "not_mcp_configuration"
        );
        let rendered = serde_json::to_string(&extraction).expect("serialize extraction");
        assert!(!rendered.contains("sentinel-secret"));
    }

    #[test]
    fn byte_entrypoint_extracts_markdown_without_target_path_probes() {
        let extraction = extract_text_bytes(
            Path::new("missing.md"),
            "docs/missing.md",
            b"# Missing\n[Guide](guide.md)\n",
        )
        .expect("extract in-memory Markdown source");
        let reference = extraction
            .edges
            .iter()
            .find(|edge| edge.relation == "references")
            .expect("document link");
        assert!(!reference.extra.contains_key("target_file"));
        assert!(extraction.nodes.iter().any(|node| node.label == "Missing"));
    }

    #[test]
    fn byte_markdown_links_require_static_contained_portable_paths() {
        let extraction = extract_text_bytes(
            Path::new("missing.md"),
            "docs/guide/index.md",
            r#"# Links
[literal](./page.md#section)
[[nested/../données]]
[parent]: ../overview.markdown#summary
[dynamic](${page}.md)
[[{{page}}]]
[dynamic-ref]: $page.md
[escape](../../../page.md)
[absolute](/page.md)
[drive](C:page.md)
[unc](//server/share/page.md)
[backslash](..\page.md)
"#
            .as_bytes(),
        )
        .expect("extract guarded Markdown links");
        let targets = extraction
            .edges
            .iter()
            .filter(|edge| edge.relation == "references")
            .map(|edge| edge.true_target().to_owned())
            .collect::<HashSet<_>>();
        assert_eq!(
            targets,
            HashSet::from([
                make_id(&["docs/guide/page"]),
                make_id(&["docs/guide/données"]),
                make_id(&["docs/overview"]),
            ])
        );
    }

    #[test]
    fn simd_utf8_decoder_matches_scalar_validation_and_text() {
        for bytes in [
            b"plain ASCII\n".as_slice(),
            "valid UTF-8: λ and 文\n".as_bytes(),
            b"invalid \xF0\x28\x8C\x28".as_slice(),
            b"truncated \xE2\x82".as_slice(),
        ] {
            assert_eq!(
                crate::bytes::validate_utf8(bytes).map(str::to_owned),
                std::str::from_utf8(bytes).map(str::to_owned),
                "SIMD validation must preserve scalar UTF-8 behavior for {bytes:?}",
            );
        }
    }
}
