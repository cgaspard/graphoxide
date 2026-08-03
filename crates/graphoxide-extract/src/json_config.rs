//! Bounded structural extraction for configuration JSON.

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
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
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
        key_lines: json_key_lines(&bytes),
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
    state.walk_object(object, &file_id, &[], None, 0);
    Ok(Extraction {
        nodes: state.nodes,
        edges: state.edges,
        hyperedges: Vec::new(),
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
        || object.keys().any(|key| CONFIG_KEYS.contains(&key.as_str()))
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
        if id.is_empty() || self.nodes.iter().any(|node| node.id == id) {
            return;
        }
        self.nodes.push(Node {
            id,
            label: label.into(),
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
        });
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
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        for (key, value) in object {
            if self.pair_count >= MAX_PAIRS {
                return;
            }
            self.pair_count += 1;
            if normalize_id(key).is_empty() {
                continue;
            }
            let mut path = prefix.to_vec();
            path.push(key.clone());
            let line = self.key_lines.get(&path).copied().unwrap_or(1);
            let mut parts = vec![self.stem.as_str()];
            parts.extend(path.iter().map(String::as_str));
            let key_id = make_id(&parts);
            self.add_node(key_id.clone(), key, "code", line, "json_key");
            self.add_edge(parent_id.into(), key_id.clone(), "contains", None, line);

            match value {
                serde_json::Value::Object(child) => {
                    self.walk_object(child, &key_id, &path, Some(key), depth + 1);
                }
                serde_json::Value::Array(items) if key == "extends" => {
                    for item in items.iter().filter_map(serde_json::Value::as_str) {
                        let reference = make_id(&["ref", item]);
                        self.add_node(reference.clone(), item, "concept", line, "reference");
                        self.add_edge(key_id.clone(), reference, "extends", Some("import"), line);
                    }
                }
                serde_json::Value::String(text) if key == "extends" => {
                    let reference = make_id(&["ref", text]);
                    self.add_node(reference.clone(), text, "concept", line, "reference");
                    let file_id = make_id(&[&self.stem]);
                    self.add_edge(file_id, reference, "extends", Some("import"), line);
                }
                serde_json::Value::String(text) if key == "$ref" => {
                    let reference = make_id(&["ref", text]);
                    self.add_node(reference.clone(), text, "concept", line, "reference");
                    self.add_edge(parent_id.into(), reference, "references", None, line);
                }
                serde_json::Value::String(_)
                    if parent_key.is_some_and(|key| DEPENDENCY_KEYS.contains(&key)) =>
                {
                    let dependency = make_id(&[key]);
                    self.add_node(dependency.clone(), key, "concept", line, "dependency");
                    self.add_edge(key_id, dependency, "imports", Some("import"), line);
                }
                _ => {}
            }
        }
    }
}
