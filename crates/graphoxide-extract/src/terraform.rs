//! Deterministic Terraform/HCL structural extraction.
//!
//! The extractor models addressable Terraform objects rather than generic HCL
//! block headers. Resource IDs are directory-scoped so references survive a
//! multi-file merge, matching Terraform module semantics.

use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use regex::Regex;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

#[derive(Debug)]
struct Block {
    kind: String,
    first: Option<String>,
    second: Option<String>,
    start: usize,
    body_start: usize,
    body_end: usize,
}

#[derive(Debug)]
struct OwnerSpan {
    id: String,
    start: usize,
    end: usize,
}

fn line_of(source: &str, offset: usize) -> usize {
    source[..offset.min(source.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

fn code_mask(source: &str) -> Vec<bool> {
    let bytes = source.as_bytes();
    let mut mask = vec![true; bytes.len()];
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                mask[index] = false;
                index += 1;
                while index < bytes.len() {
                    mask[index] = false;
                    if bytes[index] == b'\\' {
                        index += 1;
                        if index < bytes.len() {
                            mask[index] = false;
                        }
                    } else if bytes[index] == b'"' {
                        index += 1;
                        break;
                    }
                    index += 1;
                }
            }
            b'#' => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    mask[index] = false;
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    mask[index] = false;
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                mask[index] = false;
                mask[index + 1] = false;
                index += 2;
                while index < bytes.len() {
                    mask[index] = false;
                    if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                        mask[index + 1] = false;
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            _ => index += 1,
        }
    }
    mask
}

fn brace_depth(source: &str, mask: &[bool]) -> Vec<usize> {
    let mut depths = vec![0; source.len()];
    let mut depth = 0usize;
    for (index, byte) in source.bytes().enumerate() {
        depths[index] = depth;
        if !mask[index] {
            continue;
        }
        if byte == b'{' {
            depth += 1;
        } else if byte == b'}' {
            depth = depth.saturating_sub(1);
        }
    }
    depths
}

fn closing_brace(source: &str, mask: &[bool], open: usize) -> Option<usize> {
    let mut depth = 1usize;
    for (index, byte) in source.bytes().enumerate().skip(open + 1) {
        if !mask[index] {
            continue;
        }
        if byte == b'{' {
            depth += 1;
        } else if byte == b'}' {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

fn blocks(source: &str, mask: &[bool]) -> anyhow::Result<Vec<Block>> {
    let header =
        Regex::new(r#"(?m)\b([A-Za-z_][A-Za-z0-9_-]*)\s*(?:"([^"]+)")?\s*(?:"([^"]+)")?\s*\{"#)?;
    let depths = brace_depth(source, mask);
    let mut found = Vec::new();
    for captures in header.captures_iter(source) {
        let whole = captures.get(0).expect("Terraform block match");
        let open = whole.end() - 1;
        if !mask.get(whole.start()).copied().unwrap_or(false)
            || !mask.get(open).copied().unwrap_or(false)
            || depths.get(open).copied().unwrap_or(1) != 0
        {
            continue;
        }
        let Some(close) = closing_brace(source, mask, open) else {
            continue;
        };
        found.push(Block {
            kind: captures[1].to_ascii_lowercase(),
            first: captures.get(2).map(|value| value.as_str().to_owned()),
            second: captures.get(3).map(|value| value.as_str().to_owned()),
            start: whole.start(),
            body_start: open + 1,
            body_end: close,
        });
    }
    Ok(found)
}

fn address(block: &Block) -> Option<(String, &'static str)> {
    match block.kind.as_str() {
        "resource" => Some((
            format!("{}.{}", block.first.as_deref()?, block.second.as_deref()?),
            "resource",
        )),
        "data" => Some((
            format!(
                "data.{}.{}",
                block.first.as_deref()?,
                block.second.as_deref()?
            ),
            "data",
        )),
        "variable" => Some((format!("var.{}", block.first.as_deref()?), "variable")),
        "provider" => Some((format!("provider.{}", block.first.as_deref()?), "provider")),
        "module" => Some((format!("module.{}", block.first.as_deref()?), "module")),
        "output" => Some((format!("output.{}", block.first.as_deref()?), "output")),
        // `terraform {}` is settings, not an addressable graph entity.
        _ => None,
    }
}

fn directory_scope(source_file: &str) -> String {
    Path::new(source_file)
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_string_lossy()
        .replace('\\', "/")
}

fn address_id(source_file: &str, address: &str) -> String {
    let scope = directory_scope(source_file);
    if scope.is_empty() {
        make_id(&[address])
    } else {
        make_id(&[&scope, address])
    }
}

fn terraform_node(
    id: String,
    label: impl Into<String>,
    source_file: &str,
    line: usize,
    kind: &str,
) -> Node {
    Node {
        id,
        label: label.into(),
        file_type: "code".into(),
        source_file: source_file.into(),
        source_location: Some(format!("L{line}")),
        community: None,
        extra: BTreeMap::from([
            ("_origin".into(), "terraform".into()),
            ("type".into(), kind.into()),
        ]),
    }
}

fn terraform_edge(
    source: String,
    target: String,
    relation: &str,
    source_file: &str,
    line: usize,
) -> Edge {
    Edge {
        source: source.clone(),
        target: target.clone(),
        relation: relation.into(),
        confidence: Confidence::Extracted,
        source_file: source_file.into(),
        extra: BTreeMap::from([
            ("_src".into(), source.into()),
            ("_tgt".into(), target.into()),
            ("source_location".into(), format!("L{line}").into()),
            ("weight".into(), 1.0.into()),
        ]),
    }
}

fn interpolation_at(source: &str, offset: usize) -> bool {
    let prefix = &source[..offset.min(source.len())];
    let Some(open) = prefix.rfind("${") else {
        return false;
    };
    prefix[open + 2..].rfind('}').is_none()
}

fn traversal_target(traversal: &str) -> Option<String> {
    let parts = traversal.split('.').collect::<Vec<_>>();
    let head = *parts.first()?;
    if ["count", "each", "self", "path", "terraform"].contains(&head) {
        return None;
    }
    let count = if head == "data" { 3 } else { 2 };
    (parts.len() >= count).then(|| parts[..count].join("."))
}

fn emit_references(
    source: &str,
    mask: &[bool],
    source_file: &str,
    owner: &OwnerSpan,
    edges: &mut Vec<Edge>,
    seen: &mut BTreeSet<(String, String, String)>,
) -> anyhow::Result<()> {
    let body = &source[owner.start..owner.end];
    let depends = Regex::new(r"(?s)\bdepends_on\s*=\s*\[([^\]]*)\]")?;
    let traversal = Regex::new(r"\b[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)+\b")?;
    let mut dependency_ranges = Vec::new();
    for assignment in depends.captures_iter(body) {
        let whole = assignment.get(0).expect("depends_on assignment");
        let absolute = owner.start + whole.start();
        if !mask.get(absolute).copied().unwrap_or(false) {
            continue;
        }
        dependency_ranges.push(whole.start()..whole.end());
        let values = assignment.get(1).expect("depends_on values");
        for matched in traversal.find_iter(values.as_str()) {
            let Some(target) = traversal_target(matched.as_str()) else {
                continue;
            };
            let target = address_id(source_file, &target);
            let key = (owner.id.clone(), target.clone(), "depends_on".to_owned());
            if seen.insert(key) {
                edges.push(terraform_edge(
                    owner.id.clone(),
                    target,
                    "depends_on",
                    source_file,
                    line_of(source, owner.start + values.start() + matched.start()),
                ));
            }
        }
    }

    for matched in traversal.find_iter(body) {
        if dependency_ranges
            .iter()
            .any(|range| range.contains(&matched.start()))
        {
            continue;
        }
        let absolute = owner.start + matched.start();
        if !mask.get(absolute).copied().unwrap_or(false) && !interpolation_at(source, absolute) {
            continue;
        }
        let Some(target) = traversal_target(matched.as_str()) else {
            continue;
        };
        let target = address_id(source_file, &target);
        if target == owner.id {
            continue;
        }
        let key = (owner.id.clone(), target.clone(), "references".to_owned());
        if seen.insert(key) {
            edges.push(terraform_edge(
                owner.id.clone(),
                target,
                "references",
                source_file,
                line_of(source, absolute),
            ));
        }
    }
    Ok(())
}

/// Extract addressable Terraform/HCL entities and their traversal edges.
pub fn extract_terraform(path: &Path, source_file: &str) -> anyhow::Result<Extraction> {
    let source = fs::read_to_string(path)?;
    extract_terraform_bytes(path, source_file, source.as_bytes())
}

/// Extract addressable Terraform/HCL entities from already-read source bytes.
///
/// This entry point deliberately has no filesystem operations. Callers that
/// schedule I/O separately can hand the extractor their owned source buffer
/// without making a second read on a compute worker.
pub fn extract_terraform_bytes(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
) -> anyhow::Result<Extraction> {
    let source = std::str::from_utf8(bytes)?;
    let file_stem = Path::new(source_file)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    let file_id = make_id(&[&file_stem]);
    let mut nodes = vec![terraform_node(
        file_id.clone(),
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(source_file),
        source_file,
        1,
        "file",
    )];
    let mut edges = Vec::new();
    let mask = code_mask(source);
    let depths = brace_depth(source, &mask);
    let parsed = blocks(source, &mask)?;
    let mut owners = Vec::new();
    let mut seen_nodes = BTreeSet::from([file_id.clone()]);
    let local_assignment = Regex::new(r"(?m)^\s*([A-Za-z_][A-Za-z0-9_-]*)\s*=")?;

    for block in &parsed {
        if block.kind == "locals" {
            let body = &source[block.body_start..block.body_end];
            let matches = local_assignment
                .captures_iter(body)
                .filter(|captures| {
                    let absolute =
                        block.body_start + captures.get(0).expect("local assignment").start();
                    mask.get(absolute).copied().unwrap_or(false)
                        && depths.get(absolute).copied() == Some(1)
                })
                .collect::<Vec<_>>();
            for (index, captures) in matches.iter().enumerate() {
                let whole = captures.get(0).expect("local assignment");
                let absolute = block.body_start + whole.start();
                let label = format!("local.{}", &captures[1]);
                let id = address_id(source_file, &label);
                let line = line_of(source, absolute);
                if seen_nodes.insert(id.clone()) {
                    nodes.push(terraform_node(
                        id.clone(),
                        label,
                        source_file,
                        line,
                        "local",
                    ));
                    edges.push(terraform_edge(
                        file_id.clone(),
                        id.clone(),
                        "contains",
                        source_file,
                        line,
                    ));
                }
                let end = matches
                    .get(index + 1)
                    .and_then(|next| next.get(0))
                    .map(|next| block.body_start + next.start())
                    .unwrap_or(block.body_end);
                owners.push(OwnerSpan {
                    id,
                    start: block.body_start + whole.end(),
                    end,
                });
            }
            continue;
        }
        let Some((label, kind)) = address(block) else {
            continue;
        };
        let id = address_id(source_file, &label);
        let line = line_of(source, block.start);
        if seen_nodes.insert(id.clone()) {
            nodes.push(terraform_node(id.clone(), label, source_file, line, kind));
            edges.push(terraform_edge(
                file_id.clone(),
                id.clone(),
                "contains",
                source_file,
                line,
            ));
        }
        owners.push(OwnerSpan {
            id,
            start: block.body_start,
            end: block.body_end,
        });
    }

    let mut seen_edges = edges
        .iter()
        .map(|edge| {
            (
                edge.true_source().to_owned(),
                edge.true_target().to_owned(),
                edge.relation.clone(),
            )
        })
        .collect::<BTreeSet<_>>();
    for owner in &owners {
        emit_references(
            source,
            &mask,
            source_file,
            owner,
            &mut edges,
            &mut seen_edges,
        )?;
    }
    Ok(Extraction {
        nodes,
        edges,
        hyperedges: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_entrypoint_does_not_require_a_source_file() {
        let extraction = extract_terraform_bytes(
            Path::new("missing.tf"),
            "infra/missing.tf",
            b"resource \"aws_instance\" \"web\" {}\n",
        )
        .expect("extract in-memory Terraform source");
        assert!(extraction
            .nodes
            .iter()
            .any(|node| node.label == "aws_instance.web"));
    }
}
