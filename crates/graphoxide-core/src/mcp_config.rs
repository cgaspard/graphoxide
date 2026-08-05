//! Shared recognition rules for MCP server configuration documents.
//!
//! Both the file extractor and the MCP ingest path must agree on which files
//! are MCP configurations and where a configuration keeps its server map.
//! Keeping one definition here prevents the two copies from drifting apart:
//! a schema recognised by one and not the other silently drops servers from
//! the graph.

use std::path::Path;

/// Basenames treated as MCP server configurations.
///
/// Frozen by the upstream parity contract; extend only alongside it.
pub const MCP_CONFIG_FILENAMES: [&str; 4] = [
    ".mcp.json",
    "claude_desktop_config.json",
    "mcp.json",
    "mcp_servers.json",
];

/// Report whether a path's basename is a recognised MCP configuration name.
pub fn is_mcp_config_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| MCP_CONFIG_FILENAMES.contains(&name))
}

/// Locate the server map inside a parsed MCP configuration root.
///
/// Three layouts are in active use and all are accepted:
///
/// - `mcpServers` at the root (Claude Desktop, `.mcp.json`)
/// - `mcp.servers` nested (VS Code `settings.json`-style embedding)
/// - `servers` at the root (VS Code `.vscode/mcp.json`)
///
/// Lookup order is stable so a document carrying more than one shape keeps
/// producing the same graph across releases. Returns `None` when no layout
/// matches, which callers treat as "not an MCP configuration" rather than as
/// a malformed file.
pub fn mcp_server_map(
    root: &serde_json::Map<String, serde_json::Value>,
) -> Option<&serde_json::Map<String, serde_json::Value>> {
    root.get("mcpServers")
        .and_then(serde_json::Value::as_object)
        .or_else(|| {
            root.get("mcp")
                .and_then(serde_json::Value::as_object)
                .and_then(|mcp| mcp.get("servers"))
                .and_then(serde_json::Value::as_object)
        })
        .or_else(|| root.get("servers").and_then(serde_json::Value::as_object))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        value.as_object().expect("object root").clone()
    }

    #[test]
    fn recognises_known_basenames() {
        assert!(is_mcp_config_path(Path::new("/tmp/.mcp.json")));
        assert!(is_mcp_config_path(Path::new("/tmp/.vscode/mcp.json")));
        assert!(!is_mcp_config_path(Path::new("/tmp/package.json")));
    }

    #[test]
    fn finds_root_mcp_servers() {
        let document = root(serde_json::json!({"mcpServers": {"a": {"command": "node"}}}));
        assert!(mcp_server_map(&document).is_some_and(|servers| servers.contains_key("a")));
    }

    #[test]
    fn finds_nested_mcp_servers() {
        let document = root(serde_json::json!({"mcp": {"servers": {"b": {"command": "node"}}}}));
        assert!(mcp_server_map(&document).is_some_and(|servers| servers.contains_key("b")));
    }

    #[test]
    fn finds_vscode_root_servers() {
        let document = root(serde_json::json!({
            "inputs": [],
            "servers": {"c": {"type": "stdio", "command": "node"}}
        }));
        assert!(mcp_server_map(&document).is_some_and(|servers| servers.contains_key("c")));
    }

    #[test]
    fn prefers_mcp_servers_over_vscode_servers() {
        let document = root(serde_json::json!({
            "mcpServers": {"canonical": {"command": "node"}},
            "servers": {"secondary": {"command": "node"}}
        }));
        let servers = mcp_server_map(&document).expect("server map");
        assert!(servers.contains_key("canonical"));
        assert!(!servers.contains_key("secondary"));
    }

    #[test]
    fn rejects_documents_without_a_server_map() {
        assert!(mcp_server_map(&root(serde_json::json!({}))).is_none());
        assert!(mcp_server_map(&root(serde_json::json!({"unrelated": "shape"}))).is_none());
        assert!(mcp_server_map(&root(serde_json::json!({"servers": []}))).is_none());
    }

    #[test]
    fn an_empty_server_map_is_still_a_server_map() {
        // A declared-but-empty map is a valid configuration, not a malformed
        // one, and all three layouts agree on that.
        assert!(mcp_server_map(&root(serde_json::json!({"mcpServers": {}}))).is_some());
        assert!(mcp_server_map(&root(serde_json::json!({"servers": {}}))).is_some());
    }
}
