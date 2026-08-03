//! Self-contained HTML graph and architecture viewers.

use graphoxide_core::KnowledgeGraph;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const COMMUNITY_COLORS: [&str; 12] = [
    "#60a5fa", "#f472b6", "#34d399", "#fbbf24", "#a78bfa", "#fb7185", "#22d3ee", "#a3e635",
    "#f97316", "#818cf8", "#2dd4bf", "#e879f9",
];

const VIS_NETWORK_URL: &str =
    "https://unpkg.com/vis-network@9.1.6/standalone/umd/vis-network.min.js";
const VIS_NETWORK_SRI: &str =
    "sha384-Ux6phic9PEHJ38YtrijhkzyJ8yQlH8i/+buBR8s3mAZOJrP1gwyvAcIYl3GWtpX1";

#[derive(Debug, Clone, Default)]
pub struct HtmlOptions {
    pub community_labels: BTreeMap<i64, String>,
    pub member_counts: BTreeMap<i64, usize>,
    /// Node id -> overlay record (`status`, `stale`, `uses`, `score`, ...).
    pub learning_overlay: BTreeMap<String, Value>,
}

pub fn render_html(graph: &KnowledgeGraph) -> anyhow::Result<String> {
    render_html_with_options(graph, &HtmlOptions::default())
}

pub fn render_html_with_options(
    graph: &KnowledgeGraph,
    options: &HtmlOptions,
) -> anyhow::Result<String> {
    let mut raw_nodes = Vec::with_capacity(graph.nodes.len());
    for node in &graph.nodes {
        let color = COMMUNITY_COLORS
            [node.community.unwrap_or_default().unsigned_abs() as usize % COMMUNITY_COLORS.len()];
        let label = if node.label.is_empty() {
            node.id.as_str()
        } else {
            node.label.as_str()
        };
        let mut rendered = Map::from_iter([
            ("id".into(), json!(node.id)),
            ("label".into(), json!(label)),
            (
                "title".into(),
                json!(format!(
                    "{}<br><small>{}</small>",
                    html_escape(label),
                    html_escape(&node.source_file)
                )),
            ),
            ("community".into(), json!(node.community)),
            (
                "color".into(),
                json!({"background": color, "border": color}),
            ),
        ]);
        if let Some(overlay) = options
            .learning_overlay
            .get(&node.id)
            .and_then(Value::as_object)
        {
            apply_learning_overlay(&mut rendered, overlay);
        }
        raw_nodes.push(Value::Object(rendered));
    }
    let raw_edges: Vec<_> = graph
        .links
        .iter()
        .map(|edge| {
            json!({
                "from": edge.true_source(),
                "to": edge.true_target(),
                "label": edge.relation,
                "arrows": "to",
                "confidence": edge.confidence,
            })
        })
        .collect();
    let nodes_json = script_json(&raw_nodes)?;
    let edges_json = script_json(&raw_edges)?;
    let legend = legend_html(graph, options);

    Ok(format!(
        r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Graphoxide graph</title>
<script src="{VIS_NETWORK_URL}" integrity="{VIS_NETWORK_SRI}" crossorigin="anonymous"></script>
<style>
html,body,#graph{{height:100%;margin:0}}body{{font:14px system-ui;background:#0f172a;color:#e2e8f0;overflow:hidden}}
#toolbar{{position:absolute;z-index:3;left:12px;top:12px;background:#111827e8;padding:10px;border-radius:9px;box-shadow:0 8px 30px #0008}}
#search{{width:260px;padding:8px;border-radius:6px;border:1px solid #475569;background:#0f172a;color:#fff}}
#legend{{margin-top:8px;max-height:180px;overflow:auto}}.legend-row{{display:flex;gap:7px;align-items:center;margin:3px 0}}.dot{{width:9px;height:9px;border-radius:50%}}
.neighbor-link{{cursor:pointer;color:#7dd3fc;text-decoration:underline}}#details{{max-width:360px;margin-top:8px}}
</style></head><body><div id="toolbar"><input id="search" aria-label="Search graph" placeholder="Search nodes"><div id="legend">{legend}</div><div id="details"></div></div><div id="graph"></div>
<script>
const RAW_NODES = {nodes_json};
const RAW_EDGES = {edges_json};
const nodes = new vis.DataSet(RAW_NODES), edges = new vis.DataSet(RAW_EDGES);
const network = new vis.Network(document.getElementById('graph'), {{nodes,edges}}, {{interaction:{{hover:true}},physics:{{stabilization:true}}}});
const esc = value => String(value).replace(/[&<>"']/g, c => ({{'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}}[c]));
function focusNode(nid) {{ network.selectNodes([nid]); network.focus(nid, {{scale:1.35,animation:true}}); }}
network.on('selectNode', event => {{
  const nid=event.nodes[0], n=nodes.get(nid);
  const nearby=network.getConnectedNodes(nid).map(id => nodes.get(id)).filter(Boolean);
  document.getElementById('details').innerHTML=`<b>${{esc(n.label)}}</b><ul>${{nearby.map(x=>{{const nid=x.id;return `<li><a class="neighbor-link" data-nid="${{esc(nid)}}">${{esc(x.label)}}</a></li>`;}}).join('')}}</ul>`;
}});
document.addEventListener('click', event => {{ const link=event.target.closest('.neighbor-link'); if(link) focusNode(link.dataset.nid); }});
document.getElementById('search').addEventListener('input', event => {{ const q=event.target.value.toLowerCase(); const hit=RAW_NODES.find(n=>String(n.label).toLowerCase().includes(q)||String(n.id).toLowerCase().includes(q)); if(hit) focusNode(hit.id); }});
</script></body></html>"##
    ))
}

fn apply_learning_overlay(node: &mut Map<String, Value>, overlay: &Map<String, Value>) {
    let status = overlay
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let stale = overlay
        .get("stale")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    node.insert("learning_status".into(), json!(status));
    node.insert("learning_stale".into(), json!(stale));
    let border = if stale {
        "#9ca3af"
    } else {
        match status {
            "preferred" => "#22c55e",
            "contested" => "#f59e0b",
            "avoid" => "#ef4444",
            _ => "#60a5fa",
        }
    };
    let color = node
        .entry("color")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("color is created as an object");
    color.insert("border".into(), json!(border));
    node.insert("borderWidth".into(), json!(3));
    if stale {
        node.insert("shapeProperties".into(), json!({"borderDashes": [4, 4]}));
    }
    let lesson = match status {
        "preferred" => "preferred source",
        "contested" => "contested guidance",
        "avoid" => "avoid this path",
        _ => "learning annotation",
    };
    let stale_note = if stale { " (code changed)" } else { "" };
    if let Some(title) = node.get("title").and_then(Value::as_str).map(str::to_owned) {
        let next = format!("{title}<br>Lesson: {lesson}{stale_note}");
        node.insert("title".into(), json!(next));
    }
}

fn script_json(value: &impl serde::Serialize) -> anyhow::Result<String> {
    // Prevent `</script>` in a hostile label from terminating the data script.
    Ok(serde_json::to_string(value)?.replace("</", "<\\/"))
}

fn legend_html(graph: &KnowledgeGraph, options: &HtmlOptions) -> String {
    let communities: BTreeSet<_> = graph
        .nodes
        .iter()
        .filter_map(|node| node.community)
        .collect();
    communities
        .into_iter()
        .map(|community| {
            let label = options
                .community_labels
                .get(&community)
                .cloned()
                .unwrap_or_else(|| format!("Community {community}"));
            let count = options.member_counts.get(&community).copied().unwrap_or_else(|| {
                graph.nodes.iter().filter(|node| node.community == Some(community)).count()
            });
            let color = COMMUNITY_COLORS
                [community.unsigned_abs() as usize % COMMUNITY_COLORS.len()];
            format!("<div class=\"legend-row\"><span class=\"dot\" style=\"background:{color}\"></span>{} ({count})</div>", html_escape(&label))
        })
        .collect()
}

/// A compact architecture/call-flow document. All user-controlled fields are
/// escaped before entering HTML.
pub fn render_callflow_html(graph: &KnowledgeGraph) -> anyhow::Result<String> {
    render_callflow_html_with_options(graph, &BTreeMap::new(), "", 15)
}

pub fn render_callflow_html_with_options(
    graph: &KnowledgeGraph,
    labels: &BTreeMap<i64, String>,
    report: &str,
    max_sections: usize,
) -> anyhow::Result<String> {
    let nodes: Vec<Value> = graph.nodes.iter().map(|node| {
        json!({"id": node.id, "label": node.label, "source_file": node.source_file, "community": node.community})
    }).collect();
    let sections = derive_sections_from_communities(&nodes, labels, max_sections);
    let mut sections_html = String::new();
    for section in sections.iter().filter(|section| section.id != "overview") {
        sections_html.push_str(&format!(
            "<section><h2>{}</h2><ul>",
            html_escape(&section.name)
        ));
        for node in graph
            .nodes
            .iter()
            .filter(|node| {
                node.community
                    .is_some_and(|community| section.communities.contains(&community))
            })
            .take(80)
        {
            sections_html.push_str(&format!(
                "<li><code>{}</code> {} <small>{}</small></li>",
                html_escape(&node.id),
                html_escape(&node.label),
                html_escape(&node.source_file)
            ));
        }
        sections_html.push_str("</ul></section>");
    }
    let mut flow = String::from("flowchart LR\n");
    for (index, edge) in graph.links.iter().take(250).enumerate() {
        flow.push_str(&format!(
            "n{index}[\"{}\"] -->|{}| m{index}[\"{}\"]\n",
            mermaid_escape(label_for(graph, edge.true_source())),
            mermaid_escape(&edge.relation),
            mermaid_escape(label_for(graph, edge.true_target()))
        ));
    }
    let highlights = report_highlights(report);
    Ok(format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>Graphoxide Complete Call Flow &amp; Architecture Documentation</title><script src="https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.min.js"></script><style>body{{font:14px system-ui;max-width:1200px;margin:auto;padding:2rem;background:#0f172a;color:#e2e8f0}}h1,h2{{color:#7dd3fc}}section,.card{{background:#1e293b;border-radius:10px;padding:1rem;margin:1rem 0}}code{{color:#f0abfc}}small{{color:#94a3b8}}</style></head><body><h1>Graphoxide architecture and call flow</h1><p>{} nodes · {} edges</p><div class="mermaid">{}</div>{}<div class="card"><h2>Graph Report Highlights</h2>{}</div><script>mermaid.initialize({{startOnLoad:true}});</script></body></html>"#,
        graph.nodes.len(),
        graph.links.len(),
        flow,
        sections_html,
        highlights
    ))
}

pub fn write_callflow_html(
    graph: &KnowledgeGraph,
    output: &Path,
    labels: &BTreeMap<i64, String>,
    report: &str,
    max_sections: usize,
) -> anyhow::Result<PathBuf> {
    let html = render_callflow_html_with_options(graph, labels, report, max_sections)?;
    graphoxide_core::write_text_atomic(output, &html)?;
    Ok(output.to_path_buf())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchitectureSection {
    pub id: String,
    pub name: String,
    pub communities: Vec<i64>,
}

/// Group communities into the same architecture archetypes used by upstream.
pub fn derive_sections_from_communities(
    nodes: &[Value],
    labels: &BTreeMap<i64, String>,
    max_sections: usize,
) -> Vec<ArchitectureSection> {
    let archetypes: [(&str, &str, &[&str]); 8] = [
        (
            "extract-pipeline",
            "Extraction Pipeline",
            &["extract", "parser", "ast", "language"],
        ),
        (
            "outputs-docs",
            "Outputs & Documentation",
            &["export", "html", "report", "obsidian", "canvas"],
        ),
        (
            "query-analysis",
            "Query & Analysis",
            &["query", "search", "path", "cluster", "analysis"],
        ),
        (
            "cli-orchestration",
            "CLI & Orchestration",
            &["cli", "command", "main", "pipeline"],
        ),
        (
            "ingest-cache-update",
            "Ingestion & Updates",
            &["ingest", "cache", "watch", "update"],
        ),
        (
            "serve-api",
            "Serving API",
            &["serve", "api", "request", "router"],
        ),
        (
            "security-global",
            "Security & Global Graph",
            &["security", "safe", "global", "prune"],
        ),
        (
            "tests-fixtures",
            "Tests & Fixtures",
            &["test", "tests", "fixture", "pytest", "mock"],
        ),
    ];
    let mut by_community: BTreeMap<i64, Vec<&Value>> = BTreeMap::new();
    for node in nodes {
        if let Some(community) = node.get("community").and_then(Value::as_i64) {
            by_community.entry(community).or_default().push(node);
        }
    }
    let mut grouped: BTreeMap<&str, ArchitectureSection> = BTreeMap::new();
    let mut other = Vec::new();
    for (community, members) in by_community {
        let text = members
            .iter()
            .flat_map(|node| {
                ["label", "source_file", "node_type", "file_type"]
                    .into_iter()
                    .filter_map(|field| node.get(field).and_then(Value::as_str))
            })
            .chain(labels.get(&community).map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        let best = archetypes
            .iter()
            .map(|item| {
                let score = item
                    .2
                    .iter()
                    .map(|keyword| keyword_occurrences(&text, keyword))
                    .sum::<usize>();
                (score, item)
            })
            .max_by_key(|(score, _)| *score);
        if let Some((score, (id, name, _))) = best.filter(|(score, _)| *score >= 2) {
            let _ = score;
            grouped
                .entry(id)
                .or_insert_with(|| ArchitectureSection {
                    id: (*id).into(),
                    name: (*name).into(),
                    communities: Vec::new(),
                })
                .communities
                .push(community);
        } else {
            other.push(community);
        }
    }
    let mut result = vec![ArchitectureSection {
        id: "overview".into(),
        name: "Architecture Overview".into(),
        communities: Vec::new(),
    }];
    let cap = max_sections.max(1);
    result.extend(grouped.into_values().take(cap.saturating_sub(1)));
    if !other.is_empty() && result.len() < cap {
        result.push(ArchitectureSection {
            id: "other".into(),
            name: "Other".into(),
            communities: other,
        });
    }
    result
}

fn keyword_occurrences(text: &str, keyword: &str) -> usize {
    text.split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| *token == keyword)
        .count()
}

fn label_for<'a>(graph: &'a KnowledgeGraph, id: &'a str) -> &'a str {
    graph
        .nodes
        .iter()
        .find(|node| node.id == id)
        .map(|node| {
            if node.label.is_empty() {
                node.id.as_str()
            } else {
                node.label.as_str()
            }
        })
        .unwrap_or(id)
}

fn report_highlights(report: &str) -> String {
    report
        .lines()
        .filter(|line| {
            line.trim_start().starts_with("- ")
                || line
                    .trim_start()
                    .chars()
                    .next()
                    .is_some_and(|character| character.is_ascii_digit())
        })
        .take(6)
        .map(|line| format!("<p>{}</p>", html_escape(line.trim())))
        .collect()
}

fn mermaid_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
