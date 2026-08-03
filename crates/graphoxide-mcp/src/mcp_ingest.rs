//! Extraction of MCP server configuration files.
//!
//! This is the Rust port of Graphify's `mcp_ingest.py`. It deliberately never
//! persists argument values or environment-variable values: both routinely
//! contain local paths and credentials.

use graphoxide_core::{make_id, sanitize_label, Confidence, Edge, Extraction, Node};
use regex::Regex;
use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io::Read,
    path::Path,
    sync::LazyLock,
};

/// Stable, case-sensitive basenames recognized as MCP configuration files.
pub const MCP_CONFIG_FILENAMES: [&str; 4] = [
    ".mcp.json",
    "claude_desktop_config.json",
    "mcp.json",
    "mcp_servers.json",
];

const MAX_BYTES: u64 = 1_048_576;
const MAX_SERVERS_PER_FILE: usize = 200;

static NPM_PACKAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^@[a-z0-9][a-z0-9._-]*/[a-z0-9][a-z0-9._-]*(?:@[\w.\-+]+)?$")
        .expect("valid npm package regex")
});
static PYTHON_MCP_PACKAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-z0-9][a-z0-9._-]*-mcp(?:-[a-z0-9._-]+)?$|^mcp-[a-z0-9][a-z0-9._-]*$")
        .expect("valid Python MCP package regex")
});
static ARG_FLAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^-{1,2}\w").expect("valid argument flag regex"));

/// The result shape used by the upstream extractor: errors are data and an
/// errored extraction is always empty, so a malformed optional config cannot
/// abort a whole repository scan.
#[derive(Debug, Clone, Default)]
pub struct McpConfigResult {
    pub extraction: Extraction,
    pub error: Option<String>,
}

impl McpConfigResult {
    fn error(message: impl Into<String>) -> Self {
        Self {
            extraction: Extraction::default(),
            error: Some(message.into()),
        }
    }
}

/// The structured-data extractor selected for a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigExtractorKind {
    McpConfig,
    GenericJson,
}

/// Select MCP ingestion before the generic JSON extractor.
pub fn config_extractor_for_path(path: &Path) -> ConfigExtractorKind {
    if is_mcp_config_path(path) {
        ConfigExtractorKind::McpConfig
    } else {
        ConfigExtractorKind::GenericJson
    }
}

pub fn is_mcp_config_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| MCP_CONFIG_FILENAMES.contains(&name))
}

/// Parse one MCP configuration without retaining secret values.
pub fn extract_mcp_config(path: &Path) -> McpConfigResult {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            return McpConfigResult::error(format!("mcp_ingest read error: {error}"));
        }
    };
    let mut raw = Vec::new();
    if let Err(error) = file.take(MAX_BYTES + 1).read_to_end(&mut raw) {
        return McpConfigResult::error(format!("mcp_ingest read error: {error}"));
    }
    if raw.len() as u64 > MAX_BYTES {
        return McpConfigResult::error("mcp config too large to index");
    }
    let text = match std::str::from_utf8(&raw) {
        Ok(text) => text,
        Err(error) => {
            return McpConfigResult::error(format!("mcp_ingest decode error: {error}"));
        }
    };
    let document: serde_json::Value = match serde_json::from_str(text) {
        Ok(document) => document,
        Err(error) => {
            return McpConfigResult::error(format!("mcp_ingest json error: {error}"));
        }
    };
    let Some(root) = document.as_object() else {
        return McpConfigResult::error("mcp_ingest: root is not an object");
    };
    let servers = root
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
        .or_else(|| {
            root.get("mcp")
                .and_then(serde_json::Value::as_object)
                .and_then(|mcp| mcp.get("servers"))
                .and_then(serde_json::Value::as_object)
        });
    let Some(servers) = servers else {
        return McpConfigResult::error("mcp_ingest: no mcpServers map");
    };

    let source_file = path.to_string_lossy().into_owned();
    let file_id = make_id(&[&source_file]);
    let file_stem = path.with_extension("").to_string_lossy().replace('\\', "/");
    let mut extraction = Extraction::default();
    let mut seen_nodes = HashSet::new();
    let mut seen_edges = HashSet::new();
    add_node(
        &mut extraction.nodes,
        &mut seen_nodes,
        file_id.clone(),
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&source_file),
        "mcp_config_file",
        &source_file,
    );

    for (server_name, value) in servers.iter().take(MAX_SERVERS_PER_FILE) {
        if server_name.is_empty() {
            continue;
        }
        let Some(spec) = value.as_object() else {
            continue;
        };
        emit_server(
            server_name,
            spec,
            &file_id,
            &file_stem,
            &source_file,
            &mut extraction,
            &mut seen_nodes,
            &mut seen_edges,
        );
    }

    McpConfigResult {
        extraction,
        error: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_server(
    server_name: &str,
    spec: &serde_json::Map<String, serde_json::Value>,
    file_id: &str,
    file_stem: &str,
    source_file: &str,
    extraction: &mut Extraction,
    seen_nodes: &mut HashSet<String>,
    seen_edges: &mut HashSet<(String, String, String)>,
) {
    let server_id = make_id(&[file_stem, "mcp_server", server_name]);
    add_node(
        &mut extraction.nodes,
        seen_nodes,
        server_id.clone(),
        server_name,
        "mcp_server",
        source_file,
    );
    add_edge(
        &mut extraction.edges,
        seen_edges,
        file_id,
        &server_id,
        "contains",
        source_file,
        None,
    );

    if let Some(command) = spec
        .get("command")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|command| !command.is_empty())
    {
        let command_id = make_id(&["mcp_command", command]);
        add_node(
            &mut extraction.nodes,
            seen_nodes,
            command_id.clone(),
            command,
            "mcp_command",
            source_file,
        );
        add_edge(
            &mut extraction.edges,
            seen_edges,
            &server_id,
            &command_id,
            "references",
            source_file,
            Some("command"),
        );
    }

    if let Some(package) = spec
        .get("args")
        .and_then(serde_json::Value::as_array)
        .and_then(|args| detect_package_from_args(args))
    {
        let package_id = make_id(&["mcp_package", &package]);
        add_node(
            &mut extraction.nodes,
            seen_nodes,
            package_id.clone(),
            &package,
            "mcp_package",
            source_file,
        );
        add_edge(
            &mut extraction.edges,
            seen_edges,
            &server_id,
            &package_id,
            "references",
            source_file,
            Some("package"),
        );
    }

    if let Some(environment) = spec.get("env").and_then(serde_json::Value::as_object) {
        // Keys only. Do not even bind values: they frequently contain secrets.
        for environment_name in environment.keys().filter(|name| !name.is_empty()) {
            let environment_id = make_id(&["env_var", environment_name]);
            add_node(
                &mut extraction.nodes,
                seen_nodes,
                environment_id.clone(),
                environment_name,
                "env_var",
                source_file,
            );
            add_edge(
                &mut extraction.edges,
                seen_edges,
                &server_id,
                &environment_id,
                "requires_env",
                source_file,
                None,
            );
        }
    }
}

fn detect_package_from_args(args: &[serde_json::Value]) -> Option<String> {
    args.iter()
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|argument| !argument.is_empty() && !ARG_FLAG.is_match(argument))
        .find_map(|argument| {
            if NPM_PACKAGE.is_match(argument) {
                Some(strip_npm_version(argument))
            } else if PYTHON_MCP_PACKAGE.is_match(argument) {
                Some(argument.to_owned())
            } else {
                None
            }
        })
}

fn strip_npm_version(package: &str) -> String {
    let version_at = if let Some(scoped) = package.strip_prefix('@') {
        scoped.find('@').map(|index| index + 1)
    } else {
        package.find('@')
    };
    version_at.map_or_else(|| package.to_owned(), |index| package[..index].to_owned())
}

fn add_node(
    nodes: &mut Vec<Node>,
    seen: &mut HashSet<String>,
    id: String,
    label: &str,
    kind: &str,
    source_file: &str,
) {
    if id.is_empty() || !seen.insert(id.clone()) {
        return;
    }
    nodes.push(Node {
        id,
        label: sanitize_label(label),
        file_type: "code".into(),
        source_file: source_file.into(),
        source_location: Some("L1".into()),
        community: None,
        extra: BTreeMap::from([("metadata".into(), serde_json::json!({"mcp_kind": kind}))]),
    });
}

#[allow(clippy::too_many_arguments)]
fn add_edge(
    edges: &mut Vec<Edge>,
    seen: &mut HashSet<(String, String, String)>,
    source: &str,
    target: &str,
    relation: &str,
    source_file: &str,
    context: Option<&str>,
) {
    if source.is_empty() || target.is_empty() || source == target {
        return;
    }
    let key = (source.to_owned(), target.to_owned(), relation.to_owned());
    if !seen.insert(key) {
        return;
    }
    let mut extra = BTreeMap::from([
        ("confidence_score".into(), 1.0.into()),
        ("source_location".into(), "L1".into()),
        ("weight".into(), 1.0.into()),
        ("_src".into(), source.into()),
        ("_tgt".into(), target.into()),
    ]);
    if let Some(context) = context {
        extra.insert("context".into(), context.into());
    }
    edges.push(Edge {
        source: source.into(),
        target: target.into(),
        relation: relation.into(),
        confidence: Confidence::Extracted,
        source_file: source_file.into(),
        extra,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeSet, fs};

    fn fixture() -> McpConfigResult {
        extract_mcp_config(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/upstream/sample.mcp.json"
        )))
    }

    fn labels_by_kind(result: &McpConfigResult, kind: &str) -> BTreeSet<String> {
        result
            .extraction
            .nodes
            .iter()
            .filter(|node| {
                node.extra
                    .get("metadata")
                    .and_then(|metadata| metadata.get("mcp_kind"))
                    .and_then(serde_json::Value::as_str)
                    == Some(kind)
            })
            .map(|node| node.label.clone())
            .collect()
    }

    fn write_json(directory: &Path, name: &str, value: serde_json::Value) -> std::path::PathBuf {
        let path = directory.join(name);
        fs::write(
            &path,
            serde_json::to_vec(&value).expect("serialize fixture"),
        )
        .expect("write fixture");
        path
    }

    #[test]
    fn test_is_mcp_config_path_recognises_known_filenames() {
        for name in MCP_CONFIG_FILENAMES {
            assert!(is_mcp_config_path(
                Path::new("/some/dir").join(name).as_path()
            ));
        }
    }

    #[test]
    fn test_is_mcp_config_path_rejects_generic_json() {
        for name in ["package.json", "config.json", "tsconfig.json"] {
            assert!(!is_mcp_config_path(Path::new(name)));
        }
    }

    #[test]
    fn test_recognised_filenames_set_is_frozen() {
        assert_eq!(MCP_CONFIG_FILENAMES.len(), 4);
        assert!(MCP_CONFIG_FILENAMES.contains(&".mcp.json"));
    }

    #[test]
    fn test_fixture_parses_without_error() {
        let result = fixture();
        assert!(result.error.is_none(), "{:?}", result.error);
        assert!(!result.extraction.nodes.is_empty());
        assert!(!result.extraction.edges.is_empty());
    }

    #[test]
    fn test_fixture_emits_every_server() {
        assert_eq!(
            labels_by_kind(&fixture(), "mcp_server"),
            ["fetch", "filesystem", "github", "time"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
    }

    #[test]
    fn test_fixture_emits_commands_as_global_nodes() {
        assert_eq!(
            labels_by_kind(&fixture(), "mcp_command"),
            ["npx", "uvx"].into_iter().map(str::to_owned).collect()
        );
    }

    #[test]
    fn test_fixture_emits_npm_packages() {
        let packages = labels_by_kind(&fixture(), "mcp_package");
        assert!(packages.contains("@modelcontextprotocol/server-filesystem"));
        assert!(packages.contains("@modelcontextprotocol/server-github"));
    }

    #[test]
    fn test_fixture_emits_python_packages() {
        let packages = labels_by_kind(&fixture(), "mcp_package");
        assert!(packages.contains("mcp-server-fetch"));
        assert!(packages.contains("mcp-server-time"));
    }

    #[test]
    fn test_fixture_strips_version_from_npm_package() {
        let packages = labels_by_kind(&fixture(), "mcp_package");
        assert!(packages.contains("@modelcontextprotocol/server-github"));
        assert!(!packages.contains("@modelcontextprotocol/server-github@0.6.2"));
    }

    #[test]
    fn test_fixture_emits_env_var_names() {
        let names = labels_by_kind(&fixture(), "env_var");
        assert!(names.contains("FILESYSTEM_ROOT"));
        assert!(names.contains("GITHUB_PERSONAL_ACCESS_TOKEN"));
    }

    #[test]
    fn test_env_var_values_never_appear_anywhere() {
        let secret = "ghp_PLACEHOLDER_NOT_A_REAL_TOKEN";
        let result = fixture();
        for node in &result.extraction.nodes {
            assert!(!node.label.contains(secret));
            assert!(!serde_json::to_string(&node.extra)
                .expect("serialize node metadata")
                .contains(secret));
        }
        for edge in &result.extraction.edges {
            assert!(!serde_json::to_string(&edge.extra)
                .expect("serialize edge metadata")
                .contains(secret));
        }
    }

    #[test]
    fn test_filesystem_path_not_persisted_as_node() {
        assert!(fixture()
            .extraction
            .nodes
            .iter()
            .all(|node| !node.label.contains("/tmp/workspace")));
    }

    #[test]
    fn test_fixture_relations_include_contains_references_requires_env() {
        let relations: BTreeSet<_> = fixture()
            .extraction
            .edges
            .into_iter()
            .map(|edge| edge.relation)
            .collect();
        for relation in ["contains", "references", "requires_env"] {
            assert!(relations.contains(relation));
        }
    }

    #[test]
    fn test_no_dangling_edges() {
        let result = fixture();
        let ids: HashSet<_> = result
            .extraction
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect();
        for edge in &result.extraction.edges {
            assert!(ids.contains(edge.true_source()));
            assert!(ids.contains(edge.true_target()));
        }
    }

    #[test]
    fn test_every_edge_has_confidence_score() {
        for edge in fixture().extraction.edges {
            assert_eq!(edge.confidence, Confidence::Extracted);
            assert_eq!(
                edge.extra
                    .get("confidence_score")
                    .and_then(serde_json::Value::as_f64),
                Some(1.0)
            );
            assert_eq!(
                edge.extra.get("weight").and_then(serde_json::Value::as_f64),
                Some(1.0)
            );
        }
    }

    #[test]
    fn test_same_command_collapses_to_one_node_across_configs() {
        let directory = tempfile::tempdir().expect("temp directory");
        let first = write_json(
            directory.path(),
            ".mcp.json",
            serde_json::json!({"mcpServers":{"a":{"command":"npx","args":["@scope/server-a"]}}}),
        );
        fs::create_dir(directory.path().join("subdir")).expect("subdir");
        let second = write_json(
            &directory.path().join("subdir"),
            "claude_desktop_config.json",
            serde_json::json!({"mcpServers":{"b":{"command":"npx","args":["@scope/server-b"]}}}),
        );
        let first = labels_and_ids(&extract_mcp_config(&first), "mcp_command");
        let second = labels_and_ids(&extract_mcp_config(&second), "mcp_command");
        assert_eq!(first.get("npx"), second.get("npx"));
    }

    #[test]
    fn test_same_env_var_collapses_to_one_node_across_configs() {
        let directory = tempfile::tempdir().expect("temp directory");
        let first = write_json(
            directory.path(),
            ".mcp.json",
            serde_json::json!({"mcpServers":{"x":{"command":"npx","env":{"OPENAI_API_KEY":"v1"}}}}),
        );
        fs::create_dir(directory.path().join("sub")).expect("subdir");
        let second = write_json(
            &directory.path().join("sub"),
            "claude_desktop_config.json",
            serde_json::json!({"mcpServers":{"y":{"command":"uvx","env":{"OPENAI_API_KEY":"v2"}}}}),
        );
        let first = labels_and_ids(&extract_mcp_config(&first), "env_var");
        let second = labels_and_ids(&extract_mcp_config(&second), "env_var");
        assert_eq!(first.get("OPENAI_API_KEY"), second.get("OPENAI_API_KEY"));
    }

    #[test]
    fn test_same_server_name_in_different_dirs_does_not_collide() {
        let directory = tempfile::tempdir().expect("temp directory");
        for name in ["proj_a", "proj_b"] {
            fs::create_dir(directory.path().join(name)).expect("project directory");
        }
        let first = write_json(
            &directory.path().join("proj_a"),
            ".mcp.json",
            serde_json::json!({"mcpServers":{"filesystem":{"command":"npx"}}}),
        );
        let second = write_json(
            &directory.path().join("proj_b"),
            ".mcp.json",
            serde_json::json!({"mcpServers":{"filesystem":{"command":"npx"}}}),
        );
        let first = labels_and_ids(&extract_mcp_config(&first), "mcp_server");
        let second = labels_and_ids(&extract_mcp_config(&second), "mcp_server");
        assert_ne!(first.get("filesystem"), second.get("filesystem"));
    }

    fn labels_and_ids(result: &McpConfigResult, kind: &str) -> BTreeMap<String, String> {
        result
            .extraction
            .nodes
            .iter()
            .filter(|node| {
                node.extra
                    .get("metadata")
                    .and_then(|metadata| metadata.get("mcp_kind"))
                    .and_then(serde_json::Value::as_str)
                    == Some(kind)
            })
            .map(|node| (node.label.clone(), node.id.clone()))
            .collect()
    }

    #[test]
    fn test_missing_mcp_servers_key() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = write_json(
            directory.path(),
            ".mcp.json",
            serde_json::json!({"unrelated":"shape"}),
        );
        let result = extract_mcp_config(&path);
        assert!(result.extraction.nodes.is_empty());
        assert!(result.extraction.edges.is_empty());
        assert!(result
            .error
            .is_some_and(|error| error.contains("no mcpServers map")));
    }

    #[test]
    fn test_nested_mcp_servers_shape() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = write_json(
            directory.path(),
            ".mcp.json",
            serde_json::json!({"mcp":{"servers":{"x":{"command":"node","args":["dist/index.js"]}}}}),
        );
        let result = extract_mcp_config(&path);
        assert!(result.error.is_none());
        assert!(labels_by_kind(&result, "mcp_server").contains("x"));
        assert!(labels_by_kind(&result, "mcp_command").contains("node"));
    }

    #[test]
    fn test_malformed_json_returns_error() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = directory.path().join(".mcp.json");
        fs::write(&path, "{not valid json").expect("write fixture");
        let result = extract_mcp_config(&path);
        assert!(result.extraction.nodes.is_empty());
        assert!(result.extraction.edges.is_empty());
        assert!(result
            .error
            .is_some_and(|error| error.contains("json error")));
    }

    #[test]
    fn test_oversize_file_skipped() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = directory.path().join(".mcp.json");
        let payload = format!(
            "{{\"mcpServers\":{{\"x\":{{\"command\":\"npx\",\"args\":[\"{}\"]}}}}}}",
            "a".repeat(2_000_000)
        );
        fs::write(&path, payload).expect("write fixture");
        assert!(extract_mcp_config(&path)
            .error
            .is_some_and(|error| error.contains("too large")));
    }

    #[test]
    fn test_root_not_an_object() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = write_json(directory.path(), ".mcp.json", serde_json::json!([1, 2, 3]));
        assert!(extract_mcp_config(&path)
            .error
            .is_some_and(|error| error.contains("root is not an object")));
    }

    #[test]
    fn test_non_dict_server_entry_skipped() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = write_json(
            directory.path(),
            ".mcp.json",
            serde_json::json!({"mcpServers":{"valid":{"command":"npx"},"broken":["not","object"]}}),
        );
        let labels = labels_by_kind(&extract_mcp_config(&path), "mcp_server");
        assert!(labels.contains("valid"));
        assert!(!labels.contains("broken"));
    }

    #[test]
    fn test_package_detection_skips_flags() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = write_json(
            directory.path(),
            ".mcp.json",
            serde_json::json!({"mcpServers":{"x":{"command":"npx","args":["-y","@scope/server-x"]}}}),
        );
        assert!(
            labels_by_kind(&extract_mcp_config(&path), "mcp_package").contains("@scope/server-x")
        );
    }

    #[test]
    fn test_no_package_detected_for_unknown_arg_shape() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = write_json(
            directory.path(),
            ".mcp.json",
            serde_json::json!({"mcpServers":{"x":{"command":"node","args":["./local-script.js","--verbose"]}}}),
        );
        assert!(labels_by_kind(&extract_mcp_config(&path), "mcp_package").is_empty());
    }

    #[test]
    fn test_server_without_command_still_emits_server_node() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = write_json(
            directory.path(),
            ".mcp.json",
            serde_json::json!({"mcpServers":{"x":{"args":["@scope/server-x"]}}}),
        );
        let result = extract_mcp_config(&path);
        assert!(labels_by_kind(&result, "mcp_server").contains("x"));
        assert!(labels_by_kind(&result, "mcp_command").is_empty());
    }

    #[test]
    fn test_dispatch_routes_mcp_filename_to_mcp_extractor() {
        assert_eq!(
            config_extractor_for_path(Path::new(".mcp.json")),
            ConfigExtractorKind::McpConfig
        );
    }

    #[test]
    fn test_dispatch_does_not_reroute_generic_json() {
        assert_eq!(
            config_extractor_for_path(Path::new("package.json")),
            ConfigExtractorKind::GenericJson
        );
    }
}
