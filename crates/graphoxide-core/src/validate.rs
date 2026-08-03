//! Extraction schema validation, derived from upstream Graphify's `validate.py`.

use crate::model::Extraction;
use serde_json::Value;
use std::collections::HashSet;

const VALID_FILE_TYPES: &[&str] = &["code", "document", "paper", "image", "rationale", "concept"];
const VALID_CONFIDENCES: &[&str] = &["EXTRACTED", "INFERRED", "AMBIGUOUS"];
const REQUIRED_NODE_FIELDS: &[&str] = &["id", "label", "file_type", "source_file"];
const REQUIRED_EDGE_FIELDS: &[&str] =
    &["source", "target", "relation", "confidence", "source_file"];

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("edge references unknown node id: {0}")]
    DanglingEdge(String),
    #[error("duplicate node id: {0}")]
    DuplicateNode(String),
    #[error("invalid node file_type: {0}")]
    InvalidFileType(String),
    #[error("required field is empty: {0}")]
    EmptyField(String),
}

/// All schema violations found while validating raw extraction JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationReport {
    pub errors: Vec<String>,
}

impl std::fmt::Display for ValidationReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            formatter,
            "Extraction JSON has {} error(s):",
            self.errors.len()
        )?;
        for error in &self.errors {
            writeln!(formatter, "  • {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationReport {}

/// Validate untyped extraction JSON without panicking on malformed IDs.
///
/// This is the lossless counterpart to [`validate_extraction`].  Keeping it
/// value-based matters for semantic/LLM input: arrays or objects can otherwise
/// fail typed deserialization before the caller receives a useful diagnostic.
pub fn validate_extraction_json(data: &Value) -> Vec<String> {
    let Some(data) = data.as_object() else {
        return vec!["Extraction must be a JSON object".into()];
    };

    let mut errors = Vec::new();
    let mut node_ids: Vec<&Value> = Vec::new();
    match data.get("nodes") {
        None => errors.push("Missing required key 'nodes'".into()),
        Some(Value::Array(nodes)) => {
            for (index, node) in nodes.iter().enumerate() {
                let Some(node) = node.as_object() else {
                    errors.push(format!("Node {index} must be an object"));
                    continue;
                };
                for field in REQUIRED_NODE_FIELDS {
                    if !node.contains_key(*field) {
                        errors.push(format!(
                            "Node {index} (id={}) missing required field '{field}'",
                            python_repr(node.get("id"))
                        ));
                    }
                }
                if let Some(id) = node.get("id") {
                    if is_non_hashable_json(id) {
                        errors.push(format!(
                            "Node {index} has non-hashable id {} - id must be a string",
                            python_repr(Some(id))
                        ));
                    } else {
                        node_ids.push(id);
                    }
                }
                if let Some(file_type) = node.get("file_type") {
                    let valid = file_type
                        .as_str()
                        .is_some_and(|file_type| VALID_FILE_TYPES.contains(&file_type));
                    if !valid {
                        errors.push(format!(
                            "Node {index} (id={}) has invalid file_type {} - must be one of {:?}",
                            python_repr(node.get("id")),
                            python_repr(Some(file_type)),
                            VALID_FILE_TYPES
                        ));
                    }
                }
            }
        }
        Some(_) => errors.push("'nodes' must be a list".into()),
    }

    let edge_list = data.get("edges").or_else(|| data.get("links"));
    match edge_list {
        None => errors.push("Missing required key 'edges'".into()),
        Some(Value::Array(edges)) => {
            for (index, edge) in edges.iter().enumerate() {
                let Some(edge) = edge.as_object() else {
                    errors.push(format!("Edge {index} must be an object"));
                    continue;
                };
                for field in REQUIRED_EDGE_FIELDS {
                    if !edge.contains_key(*field) {
                        errors.push(format!("Edge {index} missing required field '{field}'"));
                    }
                }
                if let Some(confidence) = edge.get("confidence") {
                    let valid = confidence
                        .as_str()
                        .is_some_and(|confidence| VALID_CONFIDENCES.contains(&confidence));
                    if !valid {
                        errors.push(format!(
                            "Edge {index} has invalid confidence {} - must be one of {:?}",
                            python_repr(Some(confidence)),
                            VALID_CONFIDENCES
                        ));
                    }
                }
                for endpoint in ["source", "target"] {
                    let Some(value) = edge.get(endpoint) else {
                        continue;
                    };
                    if is_non_hashable_json(value) {
                        errors.push(format!(
                            "Edge {index} {endpoint} {} is non-hashable - must be a string",
                            python_repr(Some(value))
                        ));
                    } else if !node_ids.is_empty() && !node_ids.contains(&value) {
                        errors.push(format!(
                            "Edge {index} {endpoint} {} does not match any node id",
                            python_repr(Some(value))
                        ));
                    }
                }
            }
        }
        Some(_) => errors.push("'edges' must be a list".into()),
    }

    errors
}

/// Return every raw-JSON validation error in one failure.
pub fn assert_valid_json(data: &Value) -> Result<(), ValidationReport> {
    let errors = validate_extraction_json(data);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ValidationReport { errors })
    }
}

fn is_non_hashable_json(value: &Value) -> bool {
    matches!(value, Value::Array(_) | Value::Object(_))
}

fn python_repr(value: Option<&Value>) -> String {
    match value {
        None => "'?'".into(),
        Some(Value::String(value)) => format!("{value:?}"),
        Some(Value::Null) => "None".into(),
        Some(Value::Bool(value)) => if *value { "True" } else { "False" }.into(),
        Some(value) => value.to_string(),
    }
}

/// Validate an extraction before it is consumed by the graph builder.
pub fn validate_extraction(data: &Extraction) -> Result<(), ValidationError> {
    let mut ids = HashSet::with_capacity(data.nodes.len());
    for node in &data.nodes {
        if node.id.is_empty() {
            return Err(ValidationError::EmptyField("node.id".into()));
        }
        if node.label.is_empty() {
            return Err(ValidationError::EmptyField(format!(
                "node[{}].label",
                node.id
            )));
        }
        if !VALID_FILE_TYPES.contains(&node.file_type.as_str()) {
            return Err(ValidationError::InvalidFileType(node.file_type.clone()));
        }
        if !ids.insert(node.id.as_str()) {
            return Err(ValidationError::DuplicateNode(node.id.clone()));
        }
    }
    for edge in &data.edges {
        if edge.relation.is_empty() {
            return Err(ValidationError::EmptyField("edge.relation".into()));
        }
        for endpoint in [&edge.source, &edge.target] {
            if !ids.contains(endpoint.as_str()) {
                return Err(ValidationError::DanglingEdge(endpoint.clone()));
            }
        }
    }
    Ok(())
}
