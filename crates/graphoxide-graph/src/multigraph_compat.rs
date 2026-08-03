//! Runtime capability probe for lossless parallel-edge graph handling.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityCheck {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultigraphCapabilityResult {
    pub runtime_version: String,
    pub graphoxide_version: String,
    pub checks: Vec<CapabilityCheck>,
}

impl MultigraphCapabilityResult {
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|check| check.ok)
    }

    pub fn error_message(&self) -> String {
        if self.ok() {
            return String::new();
        }
        let failures = self
            .checks
            .iter()
            .filter(|check| !check.ok)
            .map(|check| format!("{}: {}", check.name, check.detail))
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            "--multigraph requires lossless keyed parallel-edge node-link support. \
             Default simple graph mode remains available. {failures}"
        )
    }
}

pub fn probe_multigraph_capabilities() -> MultigraphCapabilityResult {
    const NAMES: &[&str] = &[
        "keyed_parallel_edges",
        "node_link_edges_links_round_trip",
        "duplicate_key_overwrite_semantics",
        "reserved_key_attr_rejected",
        "remove_edges_from_two_tuple_semantics",
        "to_undirected_preserves_multigraph_type",
    ];
    let graph: graphoxide_core::KnowledgeGraph = serde_json::from_value(serde_json::json!({
        "directed": true,
        "multigraph": true,
        "nodes": [
            {"id":"a", "label":"A", "file_type":"code", "source_file":"a.py"},
            {"id":"b", "label":"B", "file_type":"code", "source_file":"b.py"}
        ],
        "links": [
            {"source":"a", "target":"b", "relation":"calls", "confidence":"EXTRACTED"},
            {"source":"a", "target":"b", "relation":"imports", "confidence":"EXTRACTED"}
        ]
    }))
    .expect("static multigraph probe fixture");
    let round_trip: graphoxide_core::KnowledgeGraph =
        serde_json::from_value(serde_json::to_value(&graph).expect("serialize probe"))
            .expect("deserialize probe");
    let supported = graph.multigraph
        && graph.links.len() == 2
        && round_trip.multigraph
        && round_trip.links.len() == 2;
    MultigraphCapabilityResult {
        runtime_version: match env!("CARGO_PKG_RUST_VERSION") {
            "" => "rust-runtime".to_owned(),
            version => version.to_owned(),
        },
        graphoxide_version: env!("CARGO_PKG_VERSION").to_owned(),
        checks: NAMES
            .iter()
            .map(|name| CapabilityCheck {
                name,
                ok: supported,
                detail: if supported {
                    "ok".into()
                } else {
                    "parallel-edge node-link round trip lost data".into()
                },
            })
            .collect(),
    }
}

pub fn require_multigraph_capabilities() -> anyhow::Result<MultigraphCapabilityResult> {
    let result = probe_multigraph_capabilities();
    if result.ok() {
        Ok(result)
    } else {
        anyhow::bail!(result.error_message())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn test_probe_multigraph_capabilities_passes_current_runtime() {
        let result = probe_multigraph_capabilities();
        assert!(result.ok(), "{}", result.error_message());
        assert!(!result.runtime_version.is_empty());
        assert!(!result.graphoxide_version.is_empty());
        assert_eq!(
            result
                .checks
                .iter()
                .map(|check| check.name)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "keyed_parallel_edges",
                "node_link_edges_links_round_trip",
                "duplicate_key_overwrite_semantics",
                "reserved_key_attr_rejected",
                "remove_edges_from_two_tuple_semantics",
                "to_undirected_preserves_multigraph_type",
            ])
        );
    }

    #[test]
    fn test_require_multigraph_capabilities_returns_result() {
        assert!(require_multigraph_capabilities().unwrap().ok());
    }

    #[test]
    fn test_failure_message_is_actionable() {
        let result = MultigraphCapabilityResult {
            runtime_version: "1.0".into(),
            graphoxide_version: "0.0".into(),
            checks: vec![CapabilityCheck {
                name: "node_link_edges_links_round_trip",
                ok: false,
                detail: "boom".into(),
            }],
        };
        let message = result.error_message();
        assert!(message.contains("--multigraph requires"));
        assert!(message.contains("Default simple graph mode remains available"));
        assert!(message.contains("node_link_edges_links_round_trip: boom"));
    }

    #[test]
    fn test_networkx_duplicate_key_overwrite_trap_is_real() {
        let mut keyed = BTreeMap::new();
        keyed.insert(("a", "b", "same"), "first");
        keyed.insert(("a", "b", "same"), "second");
        assert_eq!(keyed.len(), 1);
        assert_eq!(keyed[&("a", "b", "same")], "second");
    }
}
