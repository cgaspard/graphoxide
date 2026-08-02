//! Extraction schema validation, derived from upstream Graphify's `validate.py`.

use crate::model::Extraction;
use std::collections::HashSet;

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
        if !matches!(
            node.file_type.as_str(),
            "code" | "document" | "paper" | "image" | "rationale" | "concept"
        ) {
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
