//! Self-contained interactive HTML graph viewer.
use graphoxide_core::KnowledgeGraph;
pub fn render_html(graph: &KnowledgeGraph) -> anyhow::Result<String> {
    let data = serde_json::to_string(graph)?;
    Ok(format!(
        r#"<!doctype html><meta charset="utf-8"><title>Graphoxide</title><style>body{{font:14px system-ui;margin:0;background:#111;color:#eee}}header{{padding:16px}}svg{{width:100vw;height:calc(100vh - 60px)}}line{{stroke:#596275;stroke-opacity:.55}}circle{{fill:#54a0ff}}text{{fill:#eee;font-size:11px}}</style><header><b>Graphoxide</b> — {} nodes, {} edges</header><svg></svg><script>const g={data};const s=document.querySelector('svg'),W=innerWidth,H=innerHeight-60,N=g.nodes.length;let p=new Map(g.nodes.map((n,i)=>[n.id,[W/2+Math.cos(i*2.399)*Math.sqrt(i)*18,H/2+Math.sin(i*2.399)*Math.sqrt(i)*18]]));for(const e of g.links){{let a=p.get(e.source),b=p.get(e.target);if(!a||!b)continue;let l=document.createElementNS('http://www.w3.org/2000/svg','line');l.setAttribute('x1',a[0]);l.setAttribute('y1',a[1]);l.setAttribute('x2',b[0]);l.setAttribute('y2',b[1]);s.append(l)}}for(const n of g.nodes){{let a=p.get(n.id),c=document.createElementNS('http://www.w3.org/2000/svg','circle');c.setAttribute('cx',a[0]);c.setAttribute('cy',a[1]);c.setAttribute('r',4);let t=document.createElementNS('http://www.w3.org/2000/svg','title');t.textContent=n.label+'\n'+n.source_file;c.append(t);s.append(c)}}</script>"#,
        graph.nodes.len(),
        graph.links.len()
    ))
}

/// Render a deterministic architecture and call-flow document that remains
/// useful offline (all graph data and styles are embedded in the file).
pub fn render_callflow_html(graph: &KnowledgeGraph) -> anyhow::Result<String> {
    use std::collections::BTreeMap;
    let labels: BTreeMap<_, _> = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node.label.as_str()))
        .collect();
    let mut communities: BTreeMap<Option<i64>, Vec<_>> = BTreeMap::new();
    for node in &graph.nodes {
        communities.entry(node.community).or_default().push(node);
    }
    for nodes in communities.values_mut() {
        nodes.sort_by(|a, b| a.id.cmp(&b.id));
    }
    let mut flows: Vec<_> = graph
        .links
        .iter()
        .filter(|edge| {
            matches!(
                edge.relation.as_str(),
                "calls" | "indirect_call" | "imports" | "imports_from" | "uses"
            )
        })
        .collect();
    flows.sort_by(|a, b| {
        (a.true_source(), a.true_target(), a.relation.as_str()).cmp(&(
            b.true_source(),
            b.true_target(),
            b.relation.as_str(),
        ))
    });
    let mut sections = String::new();
    for (community, nodes) in communities {
        let title = community
            .map(|id| format!("Community {id}"))
            .unwrap_or_else(|| "Unclustered".into());
        sections.push_str(&format!("<section><h2>{}</h2><ul>", escape(&title)));
        for node in nodes.iter().take(80) {
            sections.push_str(&format!(
                "<li><code>{}</code> {} <small>{}</small></li>",
                escape(&node.id),
                escape(&node.label),
                escape(&node.source_file)
            ));
        }
        sections.push_str("</ul></section>");
    }
    let flow_rows = flows
        .into_iter()
        .take(1500)
        .map(|edge| {
            format!(
                "<tr><td>{}</td><td><code>{}</code></td><td>{}</td></tr>",
                escape(
                    labels
                        .get(edge.true_source())
                        .copied()
                        .unwrap_or(edge.true_source())
                ),
                escape(&edge.relation),
                escape(
                    labels
                        .get(edge.true_target())
                        .copied()
                        .unwrap_or(edge.true_target())
                )
            )
        })
        .collect::<String>();
    Ok(format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Graphoxide call flow</title><style>body{{font:14px system-ui;max-width:1200px;margin:auto;padding:2rem;background:#0f172a;color:#e2e8f0}}h1,h2{{color:#7dd3fc}}.grid{{display:grid;grid-template-columns:repeat(auto-fit,minmax(300px,1fr));gap:1rem}}section,table{{background:#1e293b;border-radius:10px;padding:1rem}}table{{width:100%;border-collapse:collapse}}td{{border-bottom:1px solid #334155;padding:.45rem}}code{{color:#f0abfc}}small{{color:#94a3b8}}</style><h1>Graphoxide architecture and call flow</h1><p>{} nodes · {} edges</p><div class=\"grid\">{sections}</div><h2>Dependency flow</h2><table><tbody>{flow_rows}</tbody></table>",
        graph.nodes.len(), graph.links.len()
    ))
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
