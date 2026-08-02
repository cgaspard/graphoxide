//! Obsidian-compatible Markdown vault export.

use graphoxide_core::{sanitize_label, KnowledgeGraph};
use std::{collections::HashMap, fs, path::Path};
pub fn export_vault(graph: &KnowledgeGraph, directory: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(directory)?;
    let labels: HashMap<_, _> = graph
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.label.as_str()))
        .collect();
    for node in &graph.nodes {
        let safe = node
            .id
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>();
        let mut text = format!(
            "---\nid: {}\nsource_file: {}\nfile_type: {}\ncommunity: {}\n---\n\n# {}\n\n",
            node.id,
            node.source_file,
            node.file_type,
            node.community.map(|v| v.to_string()).unwrap_or_default(),
            sanitize_label(&node.label)
        );
        for edge in &graph.links {
            if edge.true_source() == node.id {
                text.push_str(&format!(
                    "- {} → [[{}|{}]]\n",
                    edge.relation,
                    edge.true_target(),
                    labels
                        .get(edge.true_target())
                        .copied()
                        .unwrap_or(edge.true_target())
                ))
            } else if edge.true_target() == node.id {
                text.push_str(&format!(
                    "- [[{}|{}]] → {}\n",
                    edge.true_source(),
                    labels
                        .get(edge.true_source())
                        .copied()
                        .unwrap_or(edge.true_source()),
                    edge.relation
                ))
            }
        }
        fs::write(directory.join(format!("{safe}.md")), text)?;
    }
    Ok(())
}
