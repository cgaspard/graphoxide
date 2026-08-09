//! Bounded structural extraction for configuration JSON.

use anyhow::Context as _;
use graphoxide_core::{make_id, normalize_id, Confidence, Edge, Extraction, Node};
use std::{collections::BTreeMap, fs, io::Read, path::Path};
use tree_sitter::Parser;

const MAX_BYTES: u64 = 1_048_576;
const MAX_PAIRS: usize = 500;
const MAX_DEPTH: usize = 6;
const DEPENDENCY_KEYS: &[&str] = &[
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
    "bundleDependencies",
    "bundledDependencies",
];
const CONFIG_KEYS: &[&str] = &[
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
    "bundleDependencies",
    "bundledDependencies",
    "extends",
    "$ref",
    "$schema",
    "compilerOptions",
];
const CONFIG_NAMES: &[&str] = &[
    "package.json",
    "tsconfig.json",
    "jsconfig.json",
    "composer.json",
    "deno.json",
    "deno.jsonc",
    "bower.json",
    "manifest.json",
    "app.json",
    "now.json",
    "vercel.json",
    "angular.json",
    "nest-cli.json",
    "biome.json",
    "biome.jsonc",
    "renovate.json",
    ".babelrc",
    ".babelrc.json",
    ".eslintrc.json",
    ".prettierrc.json",
    ".prettierrc",
    "babel.config.json",
];

pub(crate) fn extract_json_config(path: &Path, source_file: &str) -> anyhow::Result<Extraction> {
    let mut file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.by_ref().take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "json file too large to index"
    );
    extract_json_config_bytes(path, source_file, &bytes)
}

/// Extract supported JSON configuration from bytes already read by the I/O
/// plane. This function intentionally performs no filesystem operation.
pub(crate) fn extract_json_config_bytes(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
) -> anyhow::Result<Extraction> {
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "json file too large to index"
    );
    let value = graphoxide_core::parse_jsonc_slice(bytes)
        .with_context(|| format!("parse JSON configuration {source_file}"))?;
    let Some(object) = value.as_object() else {
        return Ok(Extraction::default());
    };
    if !is_config(path, object) {
        return Ok(Extraction::default());
    }

    let stem = Path::new(source_file)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    let file_id = make_id(&[&stem]);
    let mut state = JsonState {
        source_file,
        stem,
        nodes: Vec::new(),
        edges: Vec::new(),
        pair_count: 0,
        key_lines: json_key_lines(bytes),
    };
    state.add_node(
        file_id.clone(),
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(source_file),
        "code",
        1,
        "file",
    );
    state.walk_object(object, &file_id, &[], None, 0, false);
    Ok(Extraction {
        nodes: state.nodes,
        edges: state.edges,
        hyperedges: Vec::new(),
    })
}

/// Return whether a JSON path must retain the compatibility configuration
/// extractor. Generic JSON is owned by the byte-structured adapter; known
/// editor/configuration paths and objects carrying configuration keys retain
/// their existing graph shape and diagnostics.
pub(crate) fn should_use_json_config(path: &Path, bytes: &[u8]) -> bool {
    if is_editor_config_path(path) || is_resolution_config_path(path) {
        return true;
    }
    graphoxide_core::parse_jsonc_slice(bytes)
        .ok()
        .is_some_and(|value| {
            value
                .as_object()
                .is_some_and(|object| object.keys().any(|key| CONFIG_KEYS.contains(&key.as_str())))
        })
}

fn json_key_lines(source: &[u8]) -> BTreeMap<Vec<String>, usize> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_json::LANGUAGE.into())
        .is_err()
    {
        return BTreeMap::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return BTreeMap::new();
    };
    let root = tree.root_node();
    let object = if root.kind() == "object" {
        Some(root)
    } else {
        root.named_child(0).filter(|node| node.kind() == "object")
    };
    let mut lines = BTreeMap::new();
    if let Some(object) = object {
        collect_json_key_lines(object, source, &[], &mut lines);
    }
    lines
}

fn collect_json_key_lines(
    object: tree_sitter::Node<'_>,
    source: &[u8],
    prefix: &[String],
    lines: &mut BTreeMap<Vec<String>, usize>,
) {
    let mut cursor = object.walk();
    for pair in object
        .named_children(&mut cursor)
        .filter(|node| node.kind() == "pair")
    {
        let (Some(key_node), Some(value_node)) = (
            pair.child_by_field_name("key"),
            pair.child_by_field_name("value"),
        ) else {
            continue;
        };
        let Some(raw_key) = source.get(key_node.byte_range()) else {
            continue;
        };
        let Ok(key) = serde_json::from_slice::<String>(raw_key) else {
            continue;
        };
        let mut path = prefix.to_vec();
        path.push(key);
        lines
            .entry(path.clone())
            .or_insert(pair.start_position().row + 1);
        if value_node.kind() == "object" {
            collect_json_key_lines(value_node, source, &path, lines);
        }
    }
}

fn is_config(path: &Path, object: &serde_json::Map<String, serde_json::Value>) -> bool {
    is_named_config(path) || object.keys().any(|key| CONFIG_KEYS.contains(&key.as_str()))
}

fn is_named_config(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    CONFIG_NAMES.contains(&name.as_str())
        || [
            ".eslintrc.json",
            ".prettierrc.json",
            ".babelrc.json",
            "tsconfig.json",
            "jsconfig.json",
        ]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

fn is_resolution_config_path(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    ["tsconfig.json", "jsconfig.json"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

fn is_editor_config_path(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|value| value.eq_ignore_ascii_case(".vscode"))
    })
}

struct JsonState<'a> {
    source_file: &'a str,
    stem: String,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    pair_count: usize,
    key_lines: BTreeMap<Vec<String>, usize>,
}

impl JsonState<'_> {
    fn add_node(&mut self, id: String, label: &str, file_type: &str, line: usize, kind: &str) {
        self.add_node_with_redaction(id, label, file_type, line, kind, false);
    }

    fn add_node_with_redaction(
        &mut self,
        id: String,
        label: &str,
        file_type: &str,
        line: usize,
        kind: &str,
        redacted: bool,
    ) {
        if id.is_empty() || self.nodes.iter().any(|node| node.id == id) {
            return;
        }
        let mut node = Node {
            id,
            label: if redacted {
                crate::structured::REDACTED_STRUCTURED_VALUE.into()
            } else {
                label.into()
            },
            file_type: file_type.into(),
            // Upstream keeps config-owned reference/dependency concepts tied to
            // the manifest that declared them. Besides preserving provenance,
            // this prevents corpus resolution from mistaking them for sourceless
            // symbol stubs and collapsing them into same-labelled JSON-key nodes.
            source_file: self.source_file.into(),
            source_location: Some(format!("L{line}")),
            community: None,
            extra: BTreeMap::from([
                ("_origin".into(), "ast".into()),
                ("type".into(), kind.into()),
            ]),
        };
        if redacted {
            node.extra
                .insert("structured_value_redacted".into(), true.into());
            node.extra
                .insert("structured_label_redacted".into(), true.into());
            node.extra
                .insert("structured_value_type".into(), "string".into());
            node.extra
                .insert("structured_redaction_policy".into(), 1_u64.into());
        }
        self.nodes.push(node);
    }

    fn add_edge(
        &mut self,
        source: String,
        target: String,
        relation: &str,
        context: Option<&str>,
        line: usize,
    ) {
        if source.is_empty() || target.is_empty() || source == target {
            return;
        }
        if self.edges.iter().any(|edge| {
            edge.true_source() == source
                && edge.true_target() == target
                && edge.relation == relation
        }) {
            return;
        }
        let mut extra = BTreeMap::from([
            ("source_location".into(), format!("L{line}").into()),
            ("weight".into(), 1.0.into()),
        ]);
        if let Some(context) = context {
            extra.insert("context".into(), context.into());
        }
        self.edges.push(Edge {
            source,
            target,
            relation: relation.into(),
            confidence: Confidence::Extracted,
            source_file: self.source_file.into(),
            extra,
        });
    }

    fn walk_object(
        &mut self,
        object: &serde_json::Map<String, serde_json::Value>,
        parent_id: &str,
        prefix: &[String],
        parent_key: Option<&str>,
        depth: usize,
        ancestor_identity_redacted: bool,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        for (key, value) in object {
            if self.pair_count >= MAX_PAIRS {
                return;
            }
            let pair_ordinal = self.pair_count;
            self.pair_count += 1;
            if normalize_id(key).is_empty() {
                continue;
            }
            let mut path = prefix.to_vec();
            path.push(key.clone());
            let line = self.key_lines.get(&path).copied().unwrap_or(1);
            let key_identity_redacted = crate::structured::structured_string_is_sensitive(key);
            let identity_redacted = ancestor_identity_redacted || key_identity_redacted;
            let key_id = if identity_redacted {
                make_id(&[parent_id, "redacted_json_key", &pair_ordinal.to_string()])
            } else {
                let mut parts = vec![self.stem.as_str()];
                parts.extend(path.iter().map(String::as_str));
                make_id(&parts)
            };
            self.add_node_with_redaction(
                key_id.clone(),
                key,
                "code",
                line,
                "json_key",
                key_identity_redacted,
            );
            self.add_edge(parent_id.into(), key_id.clone(), "contains", None, line);

            match value {
                serde_json::Value::Object(child) => {
                    self.walk_object(
                        child,
                        &key_id,
                        &path,
                        Some(key),
                        depth + 1,
                        identity_redacted,
                    );
                }
                serde_json::Value::Array(items) if key == "extends" => {
                    for (index, item) in items
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .enumerate()
                    {
                        let redacted = crate::structured::structured_string_is_sensitive(item);
                        let index = index.to_string();
                        let reference = if redacted {
                            make_id(&[&key_id, "redacted_reference", &index])
                        } else {
                            make_id(&["ref", item])
                        };
                        self.add_node_with_redaction(
                            reference.clone(),
                            if redacted {
                                crate::structured::REDACTED_STRUCTURED_VALUE
                            } else {
                                item
                            },
                            "concept",
                            line,
                            "reference",
                            redacted,
                        );
                        self.add_edge(key_id.clone(), reference, "extends", Some("import"), line);
                    }
                }
                serde_json::Value::String(text) if key == "extends" => {
                    let redacted = crate::structured::structured_string_is_sensitive(text);
                    let reference = if redacted {
                        make_id(&[&key_id, "redacted_reference"])
                    } else {
                        make_id(&["ref", text])
                    };
                    self.add_node_with_redaction(
                        reference.clone(),
                        if redacted {
                            crate::structured::REDACTED_STRUCTURED_VALUE
                        } else {
                            text
                        },
                        "concept",
                        line,
                        "reference",
                        redacted,
                    );
                    let file_id = make_id(&[&self.stem]);
                    self.add_edge(file_id, reference, "extends", Some("import"), line);
                }
                serde_json::Value::String(text) if key == "$ref" => {
                    let redacted = crate::structured::structured_string_is_sensitive(text);
                    let reference = if redacted {
                        make_id(&[&key_id, "redacted_reference"])
                    } else {
                        make_id(&["ref", text])
                    };
                    self.add_node_with_redaction(
                        reference.clone(),
                        if redacted {
                            crate::structured::REDACTED_STRUCTURED_VALUE
                        } else {
                            text
                        },
                        "concept",
                        line,
                        "reference",
                        redacted,
                    );
                    self.add_edge(parent_id.into(), reference, "references", None, line);
                }
                serde_json::Value::String(_)
                    if parent_key.is_some_and(|key| DEPENDENCY_KEYS.contains(&key)) =>
                {
                    let dependency = if key_identity_redacted {
                        make_id(&[&key_id, "redacted_dependency"])
                    } else {
                        make_id(&[key])
                    };
                    self.add_node_with_redaction(
                        dependency.clone(),
                        key,
                        "concept",
                        line,
                        "dependency",
                        key_identity_redacted,
                    );
                    self.add_edge(key_id, dependency, "imports", Some("import"), line);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_entrypoint_does_not_require_a_source_file() {
        let extraction = extract_json_config_bytes(
            Path::new("package.json"),
            "package.json",
            br#"{"name":"demo","dependencies":{"serde":"1"}}"#,
        )
        .expect("extract in-memory JSON configuration");
        assert!(extraction
            .nodes
            .iter()
            .any(|node| node.label == "dependencies"));
        assert!(extraction.nodes.iter().any(|node| node.label == "serde"));
    }

    #[test]
    fn compatibility_references_apply_the_structured_redaction_policy() {
        let secret_ref = "Bearer abcdefghijklmnop";
        let secret_extend = "sk_live_1234567890abcdef";
        let extraction = extract_json_config_bytes(
            Path::new(".vscode/settings.json"),
            ".vscode/settings.json",
            format!(r#"{{"$ref":"{secret_ref}","extends":["safe-base.json","{secret_extend}"]}}"#)
                .as_bytes(),
        )
        .expect("extract named JSON configuration");
        let rendered = serde_json::to_string(&extraction).expect("serialize extraction");
        assert!(!rendered.contains(secret_ref));
        assert!(!rendered.contains(secret_extend));
        assert!(rendered.contains("safe-base.json"));
        assert!(extraction.nodes.iter().any(|node| {
            node.label == crate::structured::REDACTED_STRUCTURED_VALUE
                && node.extra.get("structured_value_redacted")
                    == Some(&serde_json::Value::Bool(true))
                && node.extra.get("structured_value_type")
                    == Some(&serde_json::Value::String("string".into()))
        }));
    }

    #[test]
    fn compatibility_references_do_not_redact_authentication_prose() {
        let extraction = extract_json_config_bytes(
            Path::new(".vscode/settings.json"),
            ".vscode/settings.json",
            br#"{"extends":"Basic authentication settings"}"#,
        )
        .expect("extract named JSON configuration");
        assert!(extraction
            .nodes
            .iter()
            .any(|node| node.label == "Basic authentication settings"));
    }

    #[test]
    fn compatibility_keys_use_safe_labels_and_secret_independent_ids() {
        fn extract_with_key(secret_key: &str) -> Extraction {
            extract_json_config_bytes(
                Path::new(".vscode/settings.json"),
                ".vscode/settings.json",
                format!(
                    r#"{{"{secret_key}":{{"visibleChild":true}},"compilerOptions":{{"strict":true}}}}"#
                )
                .as_bytes(),
            )
            .expect("extract named JSON configuration")
        }

        let first_secret = "ghp_aaaaaaaaaaaaaaaa";
        let second_secret = "ghp_bbbbbbbbbbbbbbbb";
        let first = extract_with_key(first_secret);
        let second = extract_with_key(second_secret);
        let first_rendered = serde_json::to_string(&first).expect("serialize first extraction");
        let second_rendered = serde_json::to_string(&second).expect("serialize second extraction");
        assert!(!first_rendered.contains(first_secret));
        assert!(!second_rendered.contains(second_secret));
        assert_eq!(
            first
                .nodes
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            second
                .nodes
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>()
        );
        assert!(first.nodes.iter().any(|node| {
            node.label == crate::structured::REDACTED_STRUCTURED_VALUE
                && node.extra.get("structured_label_redacted")
                    == Some(&serde_json::Value::Bool(true))
        }));
        assert!(first_rendered.contains("visibleChild"));
    }

    #[test]
    fn malformed_named_config_keeps_the_compatibility_error_route() {
        assert!(should_use_json_config(
            Path::new("tsconfig.json"),
            b"{not valid json"
        ));
        assert!(should_use_json_config(
            Path::new(".vscode/tasks.json"),
            b"{not valid json"
        ));
        assert!(!should_use_json_config(
            Path::new("package.json"),
            b"{not valid json"
        ));
        assert!(!should_use_json_config(
            Path::new("unrelated.json"),
            b"{not valid json"
        ));
    }
}
