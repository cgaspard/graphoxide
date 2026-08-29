//! Reviewed-plan, evidence-grounded Markdown wiki rendering.

use crate::{
    wiki::StructuredWikiPage,
    wiki::{navigation_collection_title, readable_navigation_component, readable_navigation_path},
    wiki_evidence::{project_wiki_evidence, WikiEvidenceBlock, WikiEvidenceSource},
    wiki_plan::{
        WikiArticleType, WikiPlan, WikiPlanArticle, WikiPlanCoverage, WikiPlanDomain,
        WikiPlanSource,
    },
    StructuredWikiPlan,
};
use graphoxide_core::KnowledgeGraph;
use graphoxide_graph::{communities, label_communities_by_hub};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone)]
struct ReferencePage {
    citation: String,
    path: String,
    title: String,
    heading: Vec<String>,
    blocks: Vec<WikiEvidenceBlock>,
}

struct GraphTopicNavigation {
    id: String,
    label: String,
    path: String,
    communities: Vec<i64>,
}

struct CanonicalGraphNavigation {
    topics: Vec<GraphTopicNavigation>,
    pages: Vec<StructuredWikiPage>,
}

/// Render the reviewer-owned canonical wiki from graph-only evidence.
///
/// This never reads source files. A plan must contain every active graph source
/// and its declared coverage must agree with the retained evidence. Historical
/// captures can be represented as inventory-only without local graph nodes.
pub fn render_canonical_wiki(
    graph: &KnowledgeGraph,
    plan: &WikiPlan,
    active_annotations: &BTreeMap<String, Value>,
) -> anyhow::Result<StructuredWikiPlan> {
    let planned_citations = plan
        .sources
        .iter()
        .map(|source| source.id.clone())
        .collect::<BTreeSet<_>>();
    plan.validate(&planned_citations)?;

    let evidence = project_wiki_evidence(graph, Some(active_annotations))?;
    let evidence_by_citation = evidence
        .sources
        .iter()
        .map(|source| (source.citation.as_str(), source))
        .collect::<BTreeMap<_, _>>();
    for citation in evidence_by_citation.keys() {
        anyhow::ensure!(
            planned_citations.contains(*citation),
            "active graph source {citation:?} is absent from the reviewed wiki plan"
        );
    }

    let domains = plan
        .domains
        .iter()
        .map(|domain| (domain.id.as_str(), domain))
        .collect::<BTreeMap<_, _>>();
    let source_paths = plan
        .sources
        .iter()
        .map(|source| Ok((source.id.as_str(), plan.source_path(&source.id)?)))
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
    let article_paths = plan
        .articles
        .iter()
        .map(|article| Ok((article.id.as_str(), plan.article_path(&article.id)?)))
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
    let inventory_paths = plan
        .sources
        .iter()
        .filter(|source| source.coverage == WikiPlanCoverage::InventoryOnly)
        .map(|source| {
            Ok((
                source.id.as_str(),
                WikiPlan::canonical_support_path(crate::WikiPlanPathKind::Inventory, &source.slug)?,
            ))
        })
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
    let source_titles = plan
        .sources
        .iter()
        .map(|source| (source.id.as_str(), source.title.as_str()))
        .collect::<BTreeMap<_, _>>();
    let references = reference_pages(plan, &evidence_by_citation)?;
    let navigation = graph_navigation(graph, &source_paths, &source_titles)?;
    let references_by_citation = references.iter().fold(
        BTreeMap::<&str, Vec<&ReferencePage>>::new(),
        |mut grouped, reference| {
            grouped
                .entry(&reference.citation)
                .or_default()
                .push(reference);
            grouped
        },
    );

    let mut pages = Vec::new();
    pages.push(root_page(
        plan,
        &source_paths,
        &article_paths,
        &navigation.topics,
    ));
    pages.push(agents_page());
    for domain in &plan.domains {
        pages.push(domain_page(domain, plan, &source_paths, &article_paths));
    }
    for article in &plan.articles {
        pages.push(article_page(
            article,
            &domains,
            &source_paths,
            &source_titles,
            &article_paths,
        )?);
    }
    for source in &plan.sources {
        let source_evidence = evidence_by_citation.get(source.id.as_str()).copied();
        validate_source_coverage(source, source_evidence)?;
        pages.push(source_page(
            source,
            source_evidence,
            source_paths[&source.id.as_str()].as_str(),
            &references_by_citation,
            inventory_paths.get(source.id.as_str()).map(String::as_str),
        ));
        if let Some(inventory_path) = inventory_paths.get(source.id.as_str()) {
            pages.push(inventory_page(source, inventory_path, &source_paths));
        }
    }
    for reference in &references {
        pages.push(reference_page(reference, &source_paths));
    }
    pages.extend(navigation.pages);
    pages.sort_by(|left, right| left.path.cmp(&right.path));
    let mut emitted = BTreeSet::new();
    for page in &pages {
        anyhow::ensure!(
            emitted.insert(page.path.as_str()),
            "canonical wiki emits duplicate page {:?}",
            page.path
        );
        anyhow::ensure!(
            page.markdown
                .split("---\n\n")
                .nth(1)
                .is_some_and(|body| body.starts_with("# ")),
            "canonical wiki page {:?} does not begin with an H1 after frontmatter",
            page.path
        );
    }
    Ok(StructuredWikiPlan { pages })
}

fn reference_pages(
    plan: &WikiPlan,
    evidence: &BTreeMap<&str, &WikiEvidenceSource>,
) -> anyhow::Result<Vec<ReferencePage>> {
    let planned = plan
        .sources
        .iter()
        .map(|source| (source.id.as_str(), source))
        .collect::<BTreeMap<_, _>>();
    let mut references = Vec::new();
    for (citation, source) in evidence {
        let planned_source = planned[citation];
        let mut sections = BTreeMap::<Vec<String>, Vec<WikiEvidenceBlock>>::new();
        for block in &source.blocks {
            if reference_block(block) {
                let heading = if block.heading_ancestry.is_empty() {
                    vec!["Document content".into()]
                } else {
                    block.heading_ancestry.clone()
                };
                sections.entry(heading).or_default().push(block.clone());
            }
        }
        for (heading, blocks) in sections {
            let digest = short_digest([citation.as_bytes(), heading.join("\u{1f}").as_bytes()]);
            let name = reference_filename(&planned_source.slug, citation, &digest);
            let path = WikiPlan::canonical_support_path(crate::WikiPlanPathKind::Reference, &name)?;
            let title = format!("{} — {}", planned_source.title, heading.join(" / "));
            references.push(ReferencePage {
                citation: (*citation).to_owned(),
                path,
                title,
                heading,
                blocks,
            });
        }
    }
    references.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(references)
}

fn reference_block(block: &WikiEvidenceBlock) -> bool {
    block.value.is_some()
        || block.node_type.as_deref().is_some_and(|kind| {
            !matches!(
                kind,
                "format_inventory" | "protocol_inventory" | "document_package" | "pdf_document"
            )
        })
}

fn reference_filename(source_slug: &str, citation: &str, section_digest: &str) -> String {
    let citation_digest = short_digest([citation.as_bytes()]);
    let suffix = format!("--{citation_digest}--section-{section_digest}");
    let maximum_slug_bytes = 200 - ".md".len() - suffix.len();
    let source_slug = readable_navigation_component(source_slug);
    let source_slug = if source_slug.len() <= maximum_slug_bytes {
        source_slug
    } else {
        format!("reference-{}", short_digest([source_slug.as_bytes()]))
    };
    format!("{source_slug}{suffix}")
}

fn graph_navigation(
    graph: &KnowledgeGraph,
    source_paths: &BTreeMap<&str, String>,
    source_titles: &BTreeMap<&str, &str>,
) -> anyhow::Result<CanonicalGraphNavigation> {
    let mut community_sources = BTreeMap::<i64, BTreeSet<String>>::new();
    for node in &graph.nodes {
        let (Some(community), Some(catalog)) = (node.community, node.extra.get("catalog")) else {
            continue;
        };
        let (Some(source_id), Some(capture_id)) = (
            catalog.get("source_id").and_then(Value::as_str),
            catalog.get("capture_id").and_then(Value::as_str),
        ) else {
            continue;
        };
        let citation = format!("{source_id}#{capture_id}");
        if source_paths.contains_key(citation.as_str()) {
            community_sources
                .entry(community)
                .or_default()
                .insert(citation);
        }
    }
    if community_sources.is_empty() {
        return Ok(CanonicalGraphNavigation {
            topics: Vec::new(),
            pages: Vec::new(),
        });
    }

    let mut topics = crate::derive_topic_tree(graph)?.topics;
    for topic in &mut topics {
        topic
            .communities
            .retain(|community| community_sources.contains_key(community));
        topic.communities.sort();
        topic.label = topic_title(topic, &community_sources, source_titles);
    }
    topics.retain(|topic| !topic.communities.is_empty());
    let community_source_titles = community_sources
        .keys()
        .map(|community| {
            (
                *community,
                community_source_title(*community, &community_sources, source_titles),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let duplicate_source_titles = community_source_titles.values().fold(
        BTreeMap::<String, usize>::new(),
        |mut counts, title| {
            *counts.entry(title.clone()).or_default() += 1;
            counts
        },
    );
    let hub_labels = label_communities_by_hub(graph, &communities(graph));
    let community_titles = community_sources
        .keys()
        .map(|community| {
            (
                *community,
                community_title(
                    *community,
                    &community_source_titles[community],
                    community_sources[community].len(),
                    duplicate_source_titles[&community_source_titles[community]] > 1,
                    &hub_labels,
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let community_paths = community_titles
        .iter()
        .map(|(community, title)| {
            (
                *community,
                readable_navigation_path("communities", title, &community.to_string()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    topics.sort_by(|left, right| {
        left.label
            .cmp(&right.label)
            .then_with(|| left.id.cmp(&right.id))
    });

    let topic_by_community = topics
        .iter()
        .flat_map(|topic| {
            topic
                .communities
                .iter()
                .map(move |community| (*community, topic))
        })
        .collect::<BTreeMap<_, _>>();
    let mut related = BTreeMap::<i64, BTreeSet<i64>>::new();
    for (left, right, _) in crate::taxonomy::cross_community_relationships(graph) {
        if community_sources.contains_key(&left) && community_sources.contains_key(&right) {
            related.entry(left).or_default().insert(right);
            related.entry(right).or_default().insert(left);
        }
    }

    let navigation_topics = topics
        .iter()
        .map(|topic| GraphTopicNavigation {
            id: topic.id.clone(),
            label: topic.label.clone(),
            path: readable_navigation_path("topics", &topic.label, &topic.id),
            communities: topic.communities.clone(),
        })
        .collect::<Vec<_>>();
    let topic_paths = navigation_topics
        .iter()
        .map(|topic| (topic.id.as_str(), topic.path.as_str()))
        .collect::<BTreeMap<_, _>>();
    let mut pages = Vec::new();
    for topic in &navigation_topics {
        let citations = topic
            .communities
            .iter()
            .flat_map(|community| community_sources[community].iter().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut body = format!("# {}\n\n## Communities\n\n", topic.label);
        for community in &topic.communities {
            body.push_str(&format!(
                "- {}\n",
                link(
                    &community_titles[community],
                    &relative_path(&topic.path, &community_paths[community]),
                )
            ));
        }
        body.push_str("\n[← Knowledge base](../index.md)\n");
        let mut input = vec![topic.id.clone(), topic.label.clone(), topic.path.clone()];
        input.extend(topic.communities.iter().map(ToString::to_string));
        input.extend(citations.iter().cloned());
        pages.push(page(
            &topic.path,
            &topic.label,
            "topic",
            "overview",
            "index.md",
            &topic.id,
            "graph",
            "partial",
            &citations,
            &[],
            &[],
            &input,
            body,
        ));
    }
    for (community, citations) in &community_sources {
        let topic = topic_by_community[community];
        let topic_path = topic_paths[topic.id.as_str()];
        let path = community_paths[community].clone();
        let citations = citations.iter().cloned().collect::<Vec<_>>();
        let title = community_titles[community].clone();
        let related_paths = related
            .get(community)
            .into_iter()
            .flatten()
            .map(|related| community_paths[related].clone())
            .collect::<Vec<_>>();
        let mut body = format!("# {title}\n\n## Sources\n\n");
        for citation in &citations {
            body.push_str(&format!(
                "- {}\n",
                link(
                    source_titles[citation.as_str()],
                    &relative_path(&path, &source_paths[citation.as_str()]),
                )
            ));
        }
        if !related_paths.is_empty() {
            body.push_str("\n## Related communities\n\n");
            for related_path in &related_paths {
                let related = community_paths
                    .iter()
                    .find_map(|(community, path)| (path == related_path).then_some(*community))
                    .expect("generated community path has a community");
                body.push_str(&format!(
                    "- {}\n",
                    link(
                        &community_titles[&related],
                        &relative_path(&path, related_path),
                    )
                ));
            }
        }
        body.push_str(&format!(
            "\n[← {}]({})\n",
            escape(&topic.label),
            relative_path(&path, topic_path)
        ));
        let mut input = vec![
            community.to_string(),
            title.clone(),
            path.clone(),
            topic.id.clone(),
        ];
        input.extend(citations.iter().cloned());
        input.extend(related_paths.iter().cloned());
        pages.push(page(
            &path,
            &title,
            "community",
            "reference",
            topic_path,
            &community.to_string(),
            "graph",
            "partial",
            &citations,
            &related_paths,
            &[],
            &input,
            body,
        ));
    }
    Ok(CanonicalGraphNavigation {
        topics: navigation_topics,
        pages,
    })
}

fn topic_title(
    topic: &crate::Topic,
    community_sources: &BTreeMap<i64, BTreeSet<String>>,
    source_titles: &BTreeMap<&str, &str>,
) -> String {
    if !is_topic_placeholder(&topic.label) {
        return topic.label.clone();
    }
    let titles = topic
        .communities
        .iter()
        .flat_map(|community| community_sources[community].iter())
        .filter_map(|citation| source_titles.get(citation.as_str()).copied())
        .collect::<BTreeSet<_>>();
    navigation_collection_title(titles)
}

fn is_topic_placeholder(label: &str) -> bool {
    label
        .strip_prefix("Topic ")
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
}

fn community_title(
    community: i64,
    source_title: &str,
    source_count: usize,
    duplicate_source_title: bool,
    hub_labels: &BTreeMap<i64, String>,
) -> String {
    if (source_count > 1 || duplicate_source_title)
        && let Some(label) = hub_labels.get(&community)
        && label != &format!("Community {community}")
        && label != source_title
    {
        return format!("{label} — {source_title}");
    }
    source_title.to_owned()
}

fn community_source_title(
    community: i64,
    community_sources: &BTreeMap<i64, BTreeSet<String>>,
    source_titles: &BTreeMap<&str, &str>,
) -> String {
    navigation_collection_title(
        community_sources[&community]
            .iter()
            .filter_map(|citation| source_titles.get(citation.as_str()).copied())
            .collect(),
    )
}

/// Determine the only coverage declaration supported by retained graph evidence.
pub fn canonical_source_coverage(source: &WikiEvidenceSource) -> WikiPlanCoverage {
    let incomplete = !source.diagnostics.is_empty()
        || source.statuses.iter().any(|status| {
            let status = status.to_ascii_lowercase();
            status.contains("partial")
                || status.contains("reject")
                || status.contains("unsupported")
        });
    if source.blocks.iter().any(|block| block.value.is_some()) && !incomplete {
        WikiPlanCoverage::Complete
    } else {
        WikiPlanCoverage::Partial
    }
}

fn validate_source_coverage(
    source: &WikiPlanSource,
    evidence: Option<&WikiEvidenceSource>,
) -> anyhow::Result<()> {
    match source.coverage {
        WikiPlanCoverage::Complete => {
            anyhow::ensure!(
                evidence
                    .is_some_and(|evidence| canonical_source_coverage(evidence)
                        == WikiPlanCoverage::Complete),
                "source {:?} declares complete coverage without complete graph evidence",
                source.id
            );
        }
        WikiPlanCoverage::InventoryOnly => anyhow::ensure!(
            evidence.is_none_or(|evidence| evidence.blocks.is_empty()),
            "source {:?} declares inventory-only coverage but has graph evidence",
            source.id
        ),
        WikiPlanCoverage::Partial => {}
    }
    Ok(())
}

fn root_page(
    plan: &WikiPlan,
    source_paths: &BTreeMap<&str, String>,
    article_paths: &BTreeMap<&str, String>,
    graph_topics: &[GraphTopicNavigation],
) -> StructuredWikiPage {
    let mut body = String::from("# Knowledge base\n\n## Domains\n\n");
    for domain in &plan.domains {
        let articles = plan
            .articles
            .iter()
            .filter(|article| article.domain == domain.id)
            .count();
        let sources = plan
            .sources
            .iter()
            .filter(|source| source.domain == domain.id)
            .count();
        body.push_str(&format!(
            "- {} — {articles} articles, {sources} sources\n",
            link(&domain.title, &format!("{}/index.md", domain.slug))
        ));
    }
    body.push_str("\n## Use this wiki\n\nStart with a domain, then read its articles and source references. Every factual page carries catalog capture citations; use the graph for deeper relationship traversal.\n");
    if !graph_topics.is_empty() {
        body.push_str("\n## Graph topics\n\n");
        for topic in graph_topics {
            body.push_str(&format!("- {}\n", link(&topic.label, &topic.path)));
        }
    }
    let mut input = vec!["root".into()];
    input.extend(
        plan.domains
            .iter()
            .flat_map(|domain| [domain.id.clone(), domain.title.clone(), domain.slug.clone()]),
    );
    input.extend(source_paths.values().cloned());
    input.extend(article_paths.values().cloned());
    input.extend(graph_topics.iter().flat_map(|topic| {
        [
            topic.id.clone(),
            topic.label.clone(),
            topic.path.clone(),
            topic
                .communities
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(","),
        ]
    }));
    page(
        "index.md",
        "Knowledge base",
        "root",
        "overview",
        "root",
        "root",
        "root",
        "complete",
        &[],
        &[],
        &[],
        &input,
        body,
    )
}

fn agents_page() -> StructuredWikiPage {
    let body = "# Agent guide\n\n1. Begin at [Knowledge base](index.md).\n2. Choose a domain and its reviewed articles.\n3. Follow graph-topic and community pages to traverse extracted relationships.\n4. Follow source and reference pages for technical evidence and provenance.\n5. Use catalog citations to identify captures; use graph queries for deeper relationships.\n";
    page(
        "AGENTS.md",
        "Agent guide",
        "reference",
        "reference",
        "index.md",
        "agents",
        "root",
        "complete",
        &[],
        &[],
        &[],
        &["agents".into()],
        body.into(),
    )
}

fn domain_page(
    domain: &WikiPlanDomain,
    plan: &WikiPlan,
    source_paths: &BTreeMap<&str, String>,
    article_paths: &BTreeMap<&str, String>,
) -> StructuredWikiPage {
    let path = format!("{}/index.md", domain.slug);
    let mut body = format!("# {}\n\n## Articles\n\n", domain.title);
    for article in plan
        .articles
        .iter()
        .filter(|article| article.domain == domain.id)
    {
        body.push_str(&format!(
            "- {} — `{}`\n",
            link(
                &article.title,
                &relative_path(&path, &article_paths[article.id.as_str()])
            ),
            article_type(article.article_type)
        ));
    }
    body.push_str("\n## Sources\n\n");
    for source in plan
        .sources
        .iter()
        .filter(|source| source.domain == domain.id)
    {
        body.push_str(&format!(
            "- {} — `{}`\n",
            link(
                &source.title,
                &relative_path(&path, &source_paths[source.id.as_str()])
            ),
            coverage(source.coverage)
        ));
    }
    body.push_str("\n[← Knowledge base](../index.md)\n");
    let input = vec![
        domain.id.clone(),
        domain.title.clone(),
        domain.slug.clone(),
        body.clone(),
    ];
    page(
        &path,
        &domain.title,
        "domain",
        "overview",
        "index.md",
        &domain.id,
        &domain.id,
        "complete",
        &[],
        &[],
        &[],
        &input,
        body,
    )
}

fn article_page(
    article: &WikiPlanArticle,
    domains: &BTreeMap<&str, &WikiPlanDomain>,
    source_paths: &BTreeMap<&str, String>,
    source_titles: &BTreeMap<&str, &str>,
    article_paths: &BTreeMap<&str, String>,
) -> anyhow::Result<StructuredWikiPage> {
    let path = article_paths[article.id.as_str()].as_str();
    let domain = domains[article.domain.as_str()];
    let parent = format!("{}/index.md", domain.slug);
    let mut body = format!("# {}\n\n## Evidence\n\n", article.title);
    for citation in &article.sources {
        let source = source_paths
            .get(citation.as_str())
            .ok_or_else(|| anyhow::anyhow!("article {:?} source is not rendered", article.id))?;
        let title = source_titles
            .get(citation.as_str())
            .ok_or_else(|| anyhow::anyhow!("article {:?} source title is missing", article.id))?;
        body.push_str(&format!(
            "- {} (`{citation}`)\n",
            link(title, &relative_path(path, source))
        ));
    }
    if !article.related.is_empty() {
        body.push_str("\n## Related articles\n\n");
        for related in &article.related {
            let target = article_paths.get(related.as_str()).ok_or_else(|| {
                anyhow::anyhow!("article {:?} related path is missing", article.id)
            })?;
            body.push_str(&format!(
                "- {}\n",
                link(related, &relative_path(path, target))
            ));
        }
    }
    body.push_str(&format!(
        "\n[← {}]({})\n",
        escape(&domain.title),
        relative_path(path, &parent)
    ));
    let related = article
        .related
        .iter()
        .map(|id| relative_path(path, &article_paths[id.as_str()]))
        .collect::<Vec<_>>();
    let input = [
        article.id.clone(),
        article.title.clone(),
        article.slug.clone(),
        body.clone(),
    ];
    Ok(page(
        path,
        &article.title,
        "article",
        article_type(article.article_type),
        &parent,
        &article.id,
        &article.domain,
        "partial",
        &article.sources,
        &related,
        &article.aliases,
        &input,
        body,
    ))
}

fn source_page(
    source: &WikiPlanSource,
    evidence: Option<&WikiEvidenceSource>,
    path: &str,
    references: &BTreeMap<&str, Vec<&ReferencePage>>,
    inventory_path: Option<&str>,
) -> StructuredWikiPage {
    let mut body = format!("# {}\n\n## Extraction coverage\n\n", source.title);
    body.push_str(&format!(
        "- Declared coverage: `{}`\n",
        coverage(source.coverage)
    ));
    match evidence {
        Some(evidence) => {
            body.push_str(&format!(
                "- Retained graph blocks: {}\n",
                evidence.blocks.len()
            ));
            optional_bullet(
                &mut body,
                "Representation",
                evidence.representation.as_deref(),
            );
            let capabilities = evidence.capabilities.join(", ");
            optional_bullet(&mut body, "Parser capabilities", Some(&capabilities));
            let statuses = evidence.statuses.join(", ");
            optional_bullet(&mut body, "Parser status", Some(&statuses));
            if !evidence.diagnostics.is_empty() {
                body.push_str(&format!(
                    "- Extraction diagnostics: {}\n",
                    evidence.diagnostics.len()
                ));
            }
        }
        None => body.push_str("- No extracted graph evidence is available for this capture.\n"),
    }
    body.push_str("\n## Technical reference\n\n");
    if let Some(references) = references.get(source.id.as_str()) {
        for reference in references {
            body.push_str(&format!(
                "- {}\n",
                link(
                    &reference.heading.join(" / "),
                    &relative_path(path, &reference.path)
                )
            ));
        }
    } else {
        body.push_str("No extractable technical blocks were retained.\n");
    }
    if let Some(inventory_path) = inventory_path {
        body.push_str(&format!(
            "\n## Inventory\n\n{}\n",
            link("Inventory", &relative_path(path, inventory_path))
        ));
    }
    body.push_str("\n## Sources and provenance\n\n");
    body.push_str(&format!("- Catalog capture: `{}`\n", source.id));
    if let Some(evidence) = evidence {
        optional_bullet(
            &mut body,
            "Source system",
            evidence.source_system.as_deref(),
        );
        optional_bullet(&mut body, "Location", evidence.location.as_deref());
        optional_bullet(&mut body, "URL", evidence.url.as_deref());
        optional_bullet(&mut body, "SHA-256", Some(&evidence.sha256));
        optional_bullet(&mut body, "Captured at", evidence.captured_at.as_deref());
        optional_bullet(&mut body, "Accessed at", evidence.accessed_at.as_deref());
        optional_bullet(&mut body, "Updated at", evidence.updated_at.as_deref());
    }
    body.push_str("\n[← Knowledge base](../index.md)\n");
    let input = vec![source.id.clone(), source.title.clone(), body.clone()];
    page(
        path,
        &source.title,
        "source",
        "reference",
        "index.md",
        &source.id,
        &source.domain,
        coverage(source.coverage),
        std::slice::from_ref(&source.id),
        &[],
        &[],
        &input,
        body,
    )
}

fn inventory_page(
    source: &WikiPlanSource,
    path: &str,
    source_paths: &BTreeMap<&str, String>,
) -> StructuredWikiPage {
    let source_path = source_paths[source.id.as_str()].as_str();
    let body = format!(
        "# {} inventory\n\nCapture `{}` is retained for citation history but has no active extracted graph evidence.\n\n[← Source]({})\n",
        source.title,
        source.id,
        relative_path(path, source_path),
    );
    let input = [source.id.clone(), source.title.clone(), body.clone()];
    page(
        path,
        &format!("{} inventory", source.title),
        "inventory",
        "reference",
        source_path,
        &source.id,
        &source.domain,
        "inventory-only",
        std::slice::from_ref(&source.id),
        &[],
        &[],
        &input,
        body,
    )
}

fn reference_page(
    reference: &ReferencePage,
    source_paths: &BTreeMap<&str, String>,
) -> StructuredWikiPage {
    let source_path = source_paths[reference.citation.as_str()].as_str();
    let mut body = format!("# {}\n\n## Technical reference\n", reference.title);
    for block in &reference.blocks {
        body.push_str(&format!("\n### {}\n\n", escape(&block.label)));
        body.push_str(&format!("- Evidence block: `{}`\n", block.id));
        optional_bullet(&mut body, "Kind", Some(&block.kind));
        optional_bullet(&mut body, "Node type", block.node_type.as_deref());
        optional_bullet(
            &mut body,
            "Source location",
            block.source_location.as_deref(),
        );
        optional_bullet(
            &mut body,
            "Structured path",
            block.structured_path.as_deref(),
        );
        if !block.redacted_indicators.is_empty() {
            body.push_str("- Redaction: applied\n");
        }
        if !block.truncated_indicators.is_empty() {
            body.push_str("- Truncation: present\n");
        }
        if let Some(value) = &block.value {
            let value = value_text(value);
            let fence = fence(&value);
            body.push_str(&format!("\n{fence}text\n{value}\n{fence}\n"));
        }
    }
    body.push_str(&format!(
        "\n## Sources\n\n- Catalog capture: `{}`\n",
        reference.citation
    ));
    body.push_str(&format!(
        "\n[← Source]({})\n",
        relative_path(&reference.path, source_path)
    ));
    let input = reference
        .blocks
        .iter()
        .flat_map(|block| {
            let mut fields = vec![
                block.id.clone(),
                block.label.clone(),
                block.kind.clone(),
                block.node_type.clone().unwrap_or_default(),
                block.structured_path.clone().unwrap_or_default(),
            ];
            if let Some(value) = &block.value {
                fields.push(value_text(value));
            }
            fields
        })
        .chain([reference.citation.clone(), reference.path.clone()])
        .collect::<Vec<_>>();
    page(
        &reference.path,
        &reference.title,
        "reference",
        "reference",
        source_path,
        &reference.blocks[0].id,
        "references",
        "complete",
        std::slice::from_ref(&reference.citation),
        &[],
        &[],
        &input,
        body,
    )
}

#[allow(clippy::too_many_arguments)]
fn page(
    path: &str,
    title: &str,
    kind: &str,
    article_type: &str,
    parent: &str,
    graph_ref: &str,
    domain: &str,
    coverage: &str,
    sources: &[String],
    related: &[String],
    aliases: &[String],
    input: &[String],
    body: String,
) -> StructuredWikiPage {
    let mut digest_input = input.to_vec();
    digest_input.extend([path.into(), title.into(), kind.into(), body.clone()]);
    let input_sha256 = digest(digest_input.iter().map(String::as_str));
    let markdown = format!(
        "---\ntitle: {}\nkind: {}\narticle_type: {}\ngraph_ref: {}\nparent: {}\ndomain: {}\nsummary: {}\ncoverage: {}\nreview_status: \"generated\"\ninput_sha256: {}\nsources:{}\nrelated:{}\naliases:{}\n---\n\n{}",
        quoted(title),
        quoted(kind),
        quoted(article_type),
        quoted(graph_ref),
        quoted(parent),
        quoted(domain),
        quoted(&format!("{kind} page: {title}")),
        quoted(coverage),
        quoted(&input_sha256),
        frontmatter_list(sources),
        frontmatter_list(related),
        frontmatter_list(aliases),
        body,
    );
    StructuredWikiPage {
        path: path.into(),
        markdown,
    }
}

fn frontmatter_list(values: &[String]) -> String {
    if values.is_empty() {
        " []".into()
    } else {
        values
            .iter()
            .map(|value| format!("\n  - {}", quoted(value)))
            .collect()
    }
}

fn optional_bullet(output: &mut String, label: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        output.push_str(&format!("- {label}: {}\n", escape(value)));
    }
}

fn article_type(value: WikiArticleType) -> &'static str {
    match value {
        WikiArticleType::Overview => "overview",
        WikiArticleType::Concept => "concept",
        WikiArticleType::Component => "component",
        WikiArticleType::Interface => "interface",
        WikiArticleType::Behavior => "behavior",
        WikiArticleType::Procedure => "procedure",
        WikiArticleType::Reference => "reference",
    }
}

fn coverage(value: WikiPlanCoverage) -> &'static str {
    match value {
        WikiPlanCoverage::Complete => "complete",
        WikiPlanCoverage::Partial => "partial",
        WikiPlanCoverage::InventoryOnly => "inventory-only",
    }
}

fn relative_path(from: &str, to: &str) -> String {
    let from = from.split('/').collect::<Vec<_>>();
    let to = to.split('/').collect::<Vec<_>>();
    let from_directories = &from[..from.len().saturating_sub(1)];
    let common = from_directories
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    let mut components = vec![".."; from_directories.len().saturating_sub(common)];
    components.extend_from_slice(&to[common..]);
    components.join("/")
}

fn short_digest(parts: impl IntoIterator<Item = impl AsRef<[u8]>>) -> String {
    hex::encode(digest_bytes(parts))[..16].into()
}

fn digest<'a>(values: impl IntoIterator<Item = &'a str>) -> String {
    hex::encode(digest_bytes(values.into_iter().map(str::as_bytes)))
}

fn digest_bytes(parts: impl IntoIterator<Item = impl AsRef<[u8]>>) -> [u8; 32] {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(part.as_ref());
        digest.update([0]);
    }
    digest.finalize().into()
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        value => serde_json::to_string_pretty(value).expect("JSON values serialize"),
    }
}

fn fence(value: &str) -> String {
    let width = value
        .lines()
        .filter_map(|line| line.strip_prefix('`'))
        .map(str::len)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(3);
    "`".repeat(width)
}

fn quoted(value: &str) -> String {
    serde_json::to_string(value).expect("strings serialize")
}

fn link(label: &str, target: &str) -> String {
    format!("[{}]({target})", escape(label))
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('<', "\\<")
        .replace('>', "\\>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_only_navigation_uses_source_names_not_graph_numbers() {
        let community_sources = BTreeMap::from([(
            3,
            BTreeSet::from(["alpha#capture".to_owned(), "beta#capture".to_owned()]),
        )]);
        let source_titles = BTreeMap::from([
            ("alpha#capture", "Alpha guide"),
            ("beta#capture", "Beta guide"),
        ]);
        let topic = crate::Topic {
            id: "topic-0".into(),
            label: "Topic 0".into(),
            communities: vec![3],
        };

        let topic_title = topic_title(&topic, &community_sources, &source_titles);
        let community_source_title = community_source_title(3, &community_sources, &source_titles);
        assert_eq!(topic_title, "Alpha guide + Beta guide");
        assert_eq!(community_source_title, "Alpha guide + Beta guide");
        assert_eq!(
            community_title(
                3,
                &community_source_title,
                2,
                false,
                &BTreeMap::from([(3, "Community 3".into())]),
            ),
            "Alpha guide + Beta guide"
        );
        assert!(readable_navigation_path("topics", &topic_title, &topic.id)
            .starts_with("topics/alpha-guide-beta-guide-"));
        assert!(
            readable_navigation_path("communities", &community_source_title, "3")
                .starts_with("communities/alpha-guide-beta-guide-")
        );
    }

    #[test]
    fn semantic_hubs_lead_multi_source_community_names() {
        assert_eq!(
            community_title(
                3,
                "Alpha guide + Beta guide",
                2,
                false,
                &BTreeMap::from([(3, "Deployment".into())]),
            ),
            "Deployment — Alpha guide + Beta guide"
        );
    }
}
