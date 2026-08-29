//! Portable Wikipedia-style Markdown export.

use crate::{
    obsidian::{communities_from_graph, community_labels_from_graph, Communities},
    taxonomy::cross_community_relationships,
};
use graphoxide_core::{Confidence, KnowledgeGraph, Node, CONTAINER_SOURCE_ATTRIBUTE};
use graphoxide_graph::{communities, label_communities_by_hub};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::Path,
};

const MAX_CATALOG_ID_BYTES: usize = 4_096;
const MAX_FRONTMATTER_BYTES: usize = 64 * 1024;
const MAX_FILENAME_COMPONENT_BYTES: usize = 200;
const MAX_NAVIGATION_LABEL_BYTES: usize = 160;

/// Build a stable, human-readable graph-navigation filename from a title and
/// retain the opaque graph identity only as a collision suffix.
pub(crate) fn readable_navigation_path(directory: &str, title: &str, identity: &str) -> String {
    let component = readable_navigation_component(title);
    let component = if component.len() <= MAX_NAVIGATION_LABEL_BYTES {
        component
    } else {
        let digest = short_navigation_digest(component.as_bytes());
        let keep = MAX_NAVIGATION_LABEL_BYTES.saturating_sub(digest.len() + 1);
        format!("{}-{digest}", truncate_utf8(&component, keep))
    };
    format!(
        "{directory}/{component}-{}.md",
        short_navigation_digest(identity.as_bytes())
    )
}

pub(crate) fn readable_navigation_component(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars() {
        if character.is_alphanumeric() {
            output.extend(character.to_lowercase());
        } else if !output.ends_with('-') {
            output.push('-');
        }
    }
    let output = output.trim_matches('-');
    if output.is_empty() {
        "item".into()
    } else {
        output.into()
    }
}

fn short_navigation_digest(value: &[u8]) -> String {
    hex::encode(Sha256::digest(value))[..16].into()
}

fn truncate_utf8(value: &str, length: usize) -> &str {
    let mut length = length.min(value.len());
    while length > 0 && !value.is_char_boundary(length) {
        length -= 1;
    }
    &value[..length]
}

/// One Markdown file in a structured wiki render plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredWikiPage {
    /// A normalized path relative to the wiki root.
    pub path: String,
    /// Complete Markdown, including frontmatter.
    pub markdown: String,
}

/// Deterministic, pure Markdown pages for the graph-derived wiki hierarchy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StructuredWikiPlan {
    pub pages: Vec<StructuredWikiPage>,
}

impl StructuredWikiPlan {
    /// Find a page by its normalized relative path, or by a legacy logical
    /// topic/community graph reference during a route-name migration.
    pub fn page(&self, path: &str) -> Option<&StructuredWikiPage> {
        self.pages
            .iter()
            .find(|page| page.path == path)
            .or_else(|| {
                let (directory, graph_ref) = path.rsplit_once('/')?;
                let graph_ref = graph_ref.strip_suffix(".md")?;
                let kind = match directory {
                    "topics" => "topic",
                    "communities" => "community",
                    _ => return None,
                };
                self.page_by_graph_ref(kind, graph_ref)
            })
    }

    /// Find a structured page from its stable graph identity rather than its
    /// human-facing route name.
    pub fn page_by_graph_ref(&self, kind: &str, graph_ref: &str) -> Option<&StructuredWikiPage> {
        let kind_line = format!("kind: {}", quoted(kind));
        let graph_ref_line = format!("graph_ref: {}", quoted(graph_ref));
        self.pages.iter().find(|page| {
            let Some(frontmatter) = page
                .markdown
                .strip_prefix("---\n")
                .and_then(|markdown| markdown.split_once("\n---\n"))
                .map(|(frontmatter, _)| frontmatter)
            else {
                return false;
            };
            frontmatter.lines().any(|line| line == kind_line)
                && frontmatter.lines().any(|line| line == graph_ref_line)
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CatalogSource {
    pub(crate) id: String,
    pub(crate) capture: String,
    pub(crate) graph_ref: String,
    pub(crate) sha256: String,
    pub(crate) label: String,
    pub(crate) communities: BTreeSet<i64>,
    pub(crate) represented: bool,
    pub(crate) provenance: Option<CatalogProvenance>,
}

#[derive(Debug, Clone)]
pub(crate) struct CatalogProvenance {
    pub(crate) source_system: Option<String>,
    pub(crate) url: Option<String>,
    pub(crate) location: Option<String>,
    pub(crate) source_path: Option<String>,
    pub(crate) representation: Option<String>,
    pub(crate) captured_at: Option<String>,
    pub(crate) accessed_at: Option<String>,
    pub(crate) updated_at: Option<String>,
}

/// Render the pure structural wiki pages for graph annotations only.
pub fn render_structured_wiki(
    graph: &KnowledgeGraph,
    topics: &crate::taxonomy::TopicTree,
) -> anyhow::Result<StructuredWikiPlan> {
    render_structured_wiki_with_catalog(graph, topics, &BTreeMap::new())
}

/// Render structural wiki pages, including every active Catalog annotation.
///
/// The map is keyed by project-relative source path and must contain the same
/// active source/capture identity as every graph annotation it represents.
pub fn render_structured_wiki_with_catalog(
    graph: &KnowledgeGraph,
    topics: &crate::taxonomy::TopicTree,
    active_annotations: &BTreeMap<String, serde_json::Value>,
) -> anyhow::Result<StructuredWikiPlan> {
    let sources = catalog_sources(graph, active_annotations)?;
    let mut topic_communities = BTreeMap::new();
    let communities = sources
        .values()
        .flat_map(|source| source.communities.iter().copied())
        .collect::<BTreeSet<_>>();
    let community_labels = readable_community_labels(graph, &sources, &communities);
    let mut sorted_topics = topics.topics.clone();
    sorted_topics.sort_by(|left, right| left.id.cmp(&right.id));
    for topic in &mut sorted_topics {
        topic.communities.sort();
        topic.communities.dedup();
        topic.label = readable_topic_label(topic, &community_labels, &sources);
    }
    for topic in &sorted_topics {
        for community in &topic.communities {
            anyhow::ensure!(
                topic_communities.insert(*community, topic).is_none(),
                "community {community} occurs in more than one topic"
            );
        }
    }
    for community in &communities {
        anyhow::ensure!(
            topic_communities.contains_key(community),
            "catalog-backed community {community} has no topic placement"
        );
    }

    let source_paths = source_paths(&sources);
    let inventory_paths = sources
        .values()
        .filter(|source| !source.represented)
        .map(|source| {
            let source_path = &source_paths[&source.graph_ref];
            (
                source.graph_ref.clone(),
                source_path.replacen("sources/", "inventory/", 1),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let community_paths = communities
        .iter()
        .map(|community| {
            (
                *community,
                readable_navigation_path(
                    "communities",
                    &community_title(*community, &community_labels),
                    &community.to_string(),
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let related = related_communities(graph);
    let topic_paths = sorted_topics
        .iter()
        .filter(|topic| topic.communities.iter().any(|id| communities.contains(id)))
        .map(|topic| {
            (
                topic.id.clone(),
                readable_navigation_path(
                    "topics",
                    &readable_topic_route_label(topic, &sources),
                    &topic.id,
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let community_context = CommunityPageContext {
        source_paths: &source_paths,
        labels: &community_labels,
        topic_paths: &topic_paths,
        community_paths: &community_paths,
        related: &related,
    };

    let mut pages = Vec::new();
    pages.push(StructuredWikiPage {
        path: "index.md".into(),
        markdown: root_page(
            &sorted_topics,
            &topic_paths,
            &sources,
            &source_paths,
            &inventory_paths,
        ),
    });
    for topic in &sorted_topics {
        let Some(path) = topic_paths.get(&topic.id) else {
            continue;
        };
        pages.push(StructuredWikiPage {
            path: path.clone(),
            markdown: topic_page(
                topic,
                &communities,
                &community_labels,
                &community_paths,
                &sources,
                &source_paths,
                path,
            ),
        });
    }
    for community in &communities {
        let topic = topic_communities[community];
        pages.push(StructuredWikiPage {
            path: community_paths[community].clone(),
            markdown: community_page(*community, topic, &sources, &community_context),
        });
    }
    for source in sources.values() {
        pages.push(StructuredWikiPage {
            path: source_paths[&source.graph_ref].clone(),
            markdown: source_page(
                source,
                &source_paths[&source.graph_ref],
                &community_paths,
                &community_labels,
                &topic_communities,
                &topic_paths,
                inventory_paths.get(&source.graph_ref).map(String::as_str),
            ),
        });
    }
    for source in sources.values().filter(|source| !source.represented) {
        pages.push(StructuredWikiPage {
            path: inventory_paths[&source.graph_ref].clone(),
            markdown: inventory_page(
                source,
                &source_paths[&source.graph_ref],
                &inventory_paths[&source.graph_ref],
            ),
        });
    }
    Ok(StructuredWikiPlan { pages })
}

pub(crate) fn catalog_sources(
    graph: &KnowledgeGraph,
    active_annotations: &BTreeMap<String, serde_json::Value>,
) -> anyhow::Result<BTreeMap<String, CatalogSource>> {
    let mut sources = BTreeMap::new();
    let mut active_source_paths = BTreeMap::new();
    for (source_path, catalog) in active_annotations {
        anyhow::ensure!(
            catalog
                .get("source_path")
                .and_then(serde_json::Value::as_str)
                == Some(source_path),
            "active catalog annotation for {source_path} has an unsafe source_path"
        );
        let source = parse_catalog_source(
            catalog,
            &format!("active catalog annotation {source_path}"),
            true,
        )?;
        anyhow::ensure!(
            source
                .provenance
                .as_ref()
                .and_then(|provenance| provenance.source_path.as_deref())
                == Some(source_path),
            "active catalog annotation for {source_path} has an unsafe source_path"
        );
        let identity = (source.id.clone(), source.capture.clone());
        anyhow::ensure!(
            active_source_paths
                .insert(identity.clone(), source_path)
                .is_none(),
            "active catalog annotations duplicate a source/capture identity"
        );
        anyhow::ensure!(
            sources.insert(identity, source).is_none(),
            "active catalog annotations duplicate a source/capture identity"
        );
    }
    for node in &graph.nodes {
        let node_source_path = node
            .extra
            .get(CONTAINER_SOURCE_ATTRIBUTE)
            .and_then(serde_json::Value::as_str)
            .filter(|source| !source.is_empty())
            .unwrap_or(&node.source_file);
        let Some(catalog) = node.extra.get("catalog") else {
            anyhow::ensure!(
                active_annotations.is_empty() || !active_annotations.contains_key(node_source_path),
                "node {} from active catalog source {node_source_path} is missing its catalog annotation",
                node.id
            );
            continue;
        };
        let mut annotated = parse_catalog_source(
            catalog,
            &format!("node {} catalog annotation", node.id),
            false,
        )?;
        let identity = (annotated.id.clone(), annotated.capture.clone());
        if !active_annotations.is_empty() {
            let expected_source_path = active_source_paths.get(&identity).ok_or_else(|| {
                anyhow::anyhow!(
                    "node {} catalog annotation has no active catalog record",
                    node.id
                )
            })?;
            anyhow::ensure!(
                node_source_path == *expected_source_path,
                "node {} catalog annotation does not match active source path",
                node.id
            );
        }
        if active_annotations.is_empty() {
            annotated.label = source_display_label(&annotated.id, node_source_path);
        }
        annotated.communities = node.community.into_iter().collect();
        annotated.represented = true;
        match sources.entry(identity) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                anyhow::ensure!(
                    active_annotations.is_empty(),
                    "node {} catalog annotation has no active catalog record",
                    node.id
                );
                entry.insert(annotated);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let source = entry.get_mut();
                anyhow::ensure!(
                    source.capture == annotated.capture && source.sha256 == annotated.sha256,
                    "source {} has conflicting active catalog annotations",
                    source.id
                );
                if active_annotations.is_empty() && annotated.label < source.label {
                    source.label = annotated.label;
                }
                if active_annotations.is_empty() {
                    merge_graph_only_provenance(&mut source.provenance, annotated.provenance);
                }
                source.represented = true;
                if let Some(community) = node.community {
                    source.communities.insert(community);
                }
            }
        }
    }
    let repeated_ids = sources
        .keys()
        .map(|(source_id, _)| source_id.as_str())
        .fold(BTreeMap::<String, usize>::new(), |mut counts, source_id| {
            *counts.entry(source_id.to_owned()).or_default() += 1;
            counts
        });
    let mut normalized = BTreeMap::new();
    for ((source_id, capture_id), mut source) in sources {
        source.graph_ref = if repeated_ids[&source_id] > 1 {
            format!("{source_id}#{capture_id}")
        } else {
            source_id
        };
        anyhow::ensure!(
            normalized
                .insert(source.graph_ref.clone(), source)
                .is_none(),
            "active catalog annotations duplicate a generated source identity"
        );
    }
    Ok(normalized)
}

fn source_display_label(source_id: &str, source_path: &str) -> String {
    Path::new(source_path)
        .file_stem()
        .and_then(|name| name.to_str())
        .map(graphoxide_core::sanitize_label)
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| source_id.into())
}

fn merge_graph_only_provenance(
    current: &mut Option<CatalogProvenance>,
    candidate: Option<CatalogProvenance>,
) {
    let Some(candidate) = candidate else {
        return;
    };
    let Some(current) = current else {
        *current = Some(candidate);
        return;
    };
    merge_provenance_value(&mut current.source_system, candidate.source_system);
    merge_provenance_value(&mut current.url, candidate.url);
    merge_provenance_value(&mut current.location, candidate.location);
    merge_provenance_value(&mut current.source_path, candidate.source_path);
    merge_provenance_value(&mut current.representation, candidate.representation);
    merge_provenance_value(&mut current.captured_at, candidate.captured_at);
    merge_provenance_value(&mut current.accessed_at, candidate.accessed_at);
    merge_provenance_value(&mut current.updated_at, candidate.updated_at);
}

fn merge_provenance_value(current: &mut Option<String>, candidate: Option<String>) {
    let Some(candidate) = candidate else {
        return;
    };
    if current.as_ref().is_none_or(|value| candidate < *value) {
        *current = Some(candidate);
    }
}

fn parse_catalog_source(
    catalog: &serde_json::Value,
    origin: &str,
    require_provenance: bool,
) -> anyhow::Result<CatalogSource> {
    let catalog = catalog
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("{origin} is malformed"))?;
    let field = |name: &str| {
        catalog
            .get(name)
            .and_then(serde_json::Value::as_str)
            .filter(|value| valid_catalog_identifier(value))
            .ok_or_else(|| anyhow::anyhow!("{origin} requires safe {name}"))
    };
    let id = field("source_id")?.to_owned();
    let capture = field("capture_id")?.to_owned();
    let sha256 = catalog
        .get("sha256")
        .and_then(serde_json::Value::as_str)
        .filter(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
        .ok_or_else(|| anyhow::anyhow!("{origin} requires lowercase SHA-256"))?
        .to_owned();
    let provenance = parse_catalog_provenance(catalog, origin, require_provenance)?;
    let label = provenance
        .as_ref()
        .and_then(|provenance| provenance.source_path.as_deref())
        .map(|source_path| source_display_label(&id, source_path))
        .unwrap_or_else(|| id.clone());
    Ok(CatalogSource {
        label,
        id,
        capture,
        graph_ref: String::new(),
        sha256,
        communities: BTreeSet::new(),
        represented: false,
        provenance,
    })
}

fn parse_catalog_provenance(
    catalog: &serde_json::Map<String, serde_json::Value>,
    origin: &str,
    require_provenance: bool,
) -> anyhow::Result<Option<CatalogProvenance>> {
    const FIELDS: [&str; 8] = [
        "source_system",
        "url",
        "location",
        "source_path",
        "representation",
        "captured_at",
        "accessed_at",
        "updated_at",
    ];
    let present = FIELDS.iter().any(|field| catalog.contains_key(*field));
    if !require_provenance && !present {
        return Ok(None);
    }
    let field = |name: &str| -> anyhow::Result<Option<String>> {
        match catalog.get(name) {
            None => Ok(None),
            Some(value) => value
                .as_str()
                .filter(|value| valid_catalog_provenance(value))
                .map(|value| Some(value.to_owned()))
                .ok_or_else(|| anyhow::anyhow!("{origin} requires safe {name}")),
        }
    };
    let provenance = CatalogProvenance {
        source_system: field("source_system")?,
        url: field("url")?,
        location: field("location")?,
        source_path: field("source_path")?,
        representation: field("representation")?,
        captured_at: field("captured_at")?,
        accessed_at: field("accessed_at")?,
        updated_at: field("updated_at")?,
    };
    if require_provenance {
        for (name, value) in [
            ("source_system", &provenance.source_system),
            ("url", &provenance.url),
            ("location", &provenance.location),
            ("source_path", &provenance.source_path),
            ("representation", &provenance.representation),
            ("captured_at", &provenance.captured_at),
            ("accessed_at", &provenance.accessed_at),
            ("updated_at", &provenance.updated_at),
        ] {
            anyhow::ensure!(value.is_some(), "{origin} requires safe {name}");
        }
    }
    if let Some(source_path) = &provenance.source_path {
        anyhow::ensure!(
            valid_catalog_source_path(source_path),
            "{origin} requires safe source_path"
        );
    }
    if [
        &provenance.source_system,
        &provenance.url,
        &provenance.location,
        &provenance.source_path,
        &provenance.representation,
        &provenance.captured_at,
        &provenance.accessed_at,
        &provenance.updated_at,
    ]
    .iter()
    .all(|value| value.is_none())
    {
        return Ok(None);
    }
    Ok(Some(provenance))
}

fn structured_community_labels(graph: &KnowledgeGraph) -> BTreeMap<i64, String> {
    label_communities_by_hub(graph, &communities(graph))
        .into_iter()
        .filter_map(|(community, label)| {
            let label = graphoxide_core::sanitize_label(&label);
            (!label.is_empty()).then_some((community, label))
        })
        .collect()
}

fn readable_community_labels(
    graph: &KnowledgeGraph,
    sources: &BTreeMap<String, CatalogSource>,
    communities: &BTreeSet<i64>,
) -> BTreeMap<i64, String> {
    let mut labels = structured_community_labels(graph);
    for community in communities {
        let source_labels = source_labels_for_communities(sources, std::slice::from_ref(community));
        if source_labels.len() == 1 {
            labels.insert(
                *community,
                source_labels
                    .into_iter()
                    .next()
                    .expect("one source label")
                    .into(),
            );
            continue;
        }
        if is_community_placeholder(*community, labels.get(community)) {
            labels.insert(*community, navigation_collection_title(source_labels));
        }
    }
    labels
}

fn readable_topic_label(
    topic: &crate::taxonomy::Topic,
    community_labels: &BTreeMap<i64, String>,
    sources: &BTreeMap<String, CatalogSource>,
) -> String {
    if !is_topic_placeholder(topic) {
        return display_label(&topic.label, &topic.id);
    }
    let source_labels = source_labels_for_communities(sources, &topic.communities);
    if source_labels.len() == 1 {
        return source_labels
            .into_iter()
            .next()
            .expect("one source label")
            .into();
    }
    let labels = topic
        .communities
        .iter()
        .filter_map(|community| community_labels.get(community).map(String::as_str))
        .collect::<BTreeSet<_>>();
    if labels.len() == 1 {
        return labels.into_iter().next().expect("one topic label").into();
    }
    let source_title = navigation_collection_title(source_labels);
    if !source_title.starts_with("Source collection (") {
        return source_title;
    }
    format!(
        "Related documentation ({} communities)",
        topic.communities.len()
    )
}

fn readable_topic_route_label(
    topic: &crate::taxonomy::Topic,
    sources: &BTreeMap<String, CatalogSource>,
) -> String {
    if topic.id.strip_prefix("topic-").is_some() {
        let title =
            navigation_collection_title(source_labels_for_communities(sources, &topic.communities));
        if title != "Source collection" {
            return title;
        }
    }
    display_label(&topic.label, &topic.id)
}

fn source_labels_for_communities<'a>(
    sources: &'a BTreeMap<String, CatalogSource>,
    communities: &[i64],
) -> BTreeSet<&'a str> {
    sources
        .values()
        .filter(|source| {
            source
                .communities
                .iter()
                .any(|community| communities.contains(community))
        })
        .map(|source| source.label.as_str())
        .collect()
}

fn is_community_placeholder(community: i64, label: Option<&String>) -> bool {
    label.is_none_or(|label| label.trim().is_empty() || label == &format!("Community {community}"))
}

fn is_topic_placeholder(topic: &crate::taxonomy::Topic) -> bool {
    topic.id.strip_prefix("topic-").is_some_and(|index| {
        topic.label.trim().is_empty() || topic.label == format!("Topic {index}")
    })
}

pub(crate) fn navigation_collection_title(labels: BTreeSet<&str>) -> String {
    match labels.len() {
        0 => "Source collection".into(),
        1 => labels.into_iter().next().expect("one source label").into(),
        count => common_source_label_prefix(&labels)
            .map(|prefix| format!("{prefix} collection"))
            .unwrap_or_else(|| {
                let mut labels = labels.into_iter();
                let first = labels.next().expect("multiple source labels");
                let second = labels.next().expect("multiple source labels");
                if count == 2 {
                    format!("{first} + {second}")
                } else {
                    format!("{first} + {second} + {} more", count - 2)
                }
            }),
    }
}

fn common_source_label_prefix(labels: &BTreeSet<&str>) -> Option<String> {
    let first = labels
        .first()?
        .split(|character: char| !character.is_alphanumeric());
    let mut prefix = first.filter(|word| !word.is_empty()).collect::<Vec<_>>();
    for label in labels.iter().skip(1) {
        let words = label
            .split(|character: char| !character.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>();
        let shared = prefix
            .iter()
            .zip(words)
            .take_while(|(left, right)| left.eq_ignore_ascii_case(right))
            .count();
        prefix.truncate(shared);
    }
    let prefix = prefix.join(" ");
    (prefix.chars().count() >= 3).then_some(prefix)
}

fn source_paths(sources: &BTreeMap<String, CatalogSource>) -> BTreeMap<String, String> {
    let mut used = BTreeSet::new();
    let mut paths = BTreeMap::new();
    for source in sources.values() {
        let component = if source.graph_ref == source.id {
            source.id.clone()
        } else {
            format!("{}--{}", source.id, source.capture)
        };
        let base = bounded_path_component(&component);
        let mut candidate = base.clone();
        let mut suffix = 2_u64;
        while !used.insert(candidate.to_ascii_lowercase()) {
            let suffix_text = format!("-{suffix}");
            let keep = MAX_FILENAME_COMPONENT_BYTES.saturating_sub(suffix_text.len());
            candidate = format!("{}{}", truncate_ascii(&base, keep), suffix_text);
            suffix += 1;
        }
        paths.insert(source.graph_ref.clone(), format!("sources/{candidate}.md"));
    }
    paths
}

fn root_page(
    topics: &[crate::taxonomy::Topic],
    topic_paths: &BTreeMap<String, String>,
    sources: &BTreeMap<String, CatalogSource>,
    source_paths: &BTreeMap<String, String>,
    inventory_paths: &BTreeMap<String, String>,
) -> String {
    let mut body = String::from("# Knowledge graph\n");
    if !topic_paths.is_empty() {
        body.push_str("\n## Topics\n\n");
        let mut root_topics = topics
            .iter()
            .filter(|topic| topic_paths.contains_key(&topic.id))
            .collect::<Vec<_>>();
        root_topics.sort_by(|left, right| {
            left.label
                .cmp(&right.label)
                .then_with(|| left.id.cmp(&right.id))
        });
        for (index, chunk) in root_topics.chunks(100).enumerate() {
            let start = index * 100 + 1;
            let end = start + chunk.len() - 1;
            body.push_str(&format!("### Topics {start}–{end}\n\n"));
            for topic in chunk {
                body.push_str(&format!(
                    "- {}\n",
                    markdown_path_link(&topic.label, &topic_paths[&topic.id])
                ));
            }
            body.push('\n');
        }
    }
    let unassigned = sources
        .values()
        .filter(|source| source.communities.is_empty())
        .collect::<Vec<_>>();
    if !unassigned.is_empty() {
        body.push_str("\n## Sources\n\n");
        for source in unassigned {
            body.push_str(&format!(
                "- {}\n",
                markdown_path_link(&source.label, &source_paths[&source.graph_ref])
            ));
        }
    }
    if !inventory_paths.is_empty() {
        body.push_str("\n## Inventory\n\n");
        for (id, path) in inventory_paths {
            body.push_str(&format!(
                "- {}\n",
                markdown_path_link(&sources[id].label, path)
            ));
        }
    }
    let mut input = vec!["root".into()];
    for topic in topics {
        if let Some(path) = topic_paths.get(&topic.id) {
            input.extend([topic.id.clone(), topic.label.clone(), path.clone()]);
            input.extend(topic.communities.iter().map(ToString::to_string));
        }
    }
    for source in sources.values() {
        append_source_input(&mut input, source, &source_paths[&source.graph_ref]);
        if let Some(path) = inventory_paths.get(&source.graph_ref) {
            input.push(path.clone());
        }
    }
    frontmatter(
        "Knowledge graph",
        "root",
        "root",
        "root",
        &[],
        &digest(input.iter().map(String::as_str)),
        &body,
    )
}

fn topic_page(
    topic: &crate::taxonomy::Topic,
    active_communities: &BTreeSet<i64>,
    labels: &BTreeMap<i64, String>,
    community_paths: &BTreeMap<i64, String>,
    sources: &BTreeMap<String, CatalogSource>,
    source_paths: &BTreeMap<String, String>,
    path: &str,
) -> String {
    let mut body = format!("# {}\n\n## Communities\n\n", topic.label);
    for community in topic
        .communities
        .iter()
        .filter(|community| active_communities.contains(community))
    {
        body.push_str(&format!(
            "- {}\n",
            markdown_path_link(
                &community_title(*community, labels),
                &format!("../{}", community_paths[community])
            )
        ));
    }
    body.push_str("\n[← Knowledge graph](../index.md)\n");
    let mut input = vec![
        topic.id.clone(),
        topic.label.clone(),
        path.into(),
        "index.md".into(),
    ];
    input.extend(topic.communities.iter().map(ToString::to_string));
    for community in topic
        .communities
        .iter()
        .filter(|community| active_communities.contains(community))
    {
        input.extend([
            community.to_string(),
            community_title(*community, labels),
            community_paths[community].clone(),
        ]);
        for source in sources
            .values()
            .filter(|source| source.communities.contains(community))
        {
            append_source_input(&mut input, source, &source_paths[&source.graph_ref]);
        }
    }
    frontmatter(
        &topic.label,
        "topic",
        &topic.id,
        "index.md",
        &[],
        &digest(input.iter().map(String::as_str)),
        &body,
    )
}

struct CommunityPageContext<'a> {
    source_paths: &'a BTreeMap<String, String>,
    labels: &'a BTreeMap<i64, String>,
    topic_paths: &'a BTreeMap<String, String>,
    community_paths: &'a BTreeMap<i64, String>,
    related: &'a BTreeMap<i64, Vec<(i64, f64)>>,
}

fn community_page(
    community: i64,
    topic: &crate::taxonomy::Topic,
    sources: &BTreeMap<String, CatalogSource>,
    context: &CommunityPageContext<'_>,
) -> String {
    let title = community_title(community, context.labels);
    let source_ids = sources
        .values()
        .filter(|source| source.communities.contains(&community))
        .map(|source| source.graph_ref.as_str())
        .collect::<Vec<_>>();
    let mut input = vec![
        community.to_string(),
        title.clone(),
        topic.id.clone(),
        topic.label.clone(),
        context.topic_paths[&topic.id].clone(),
    ];
    for id in &source_ids {
        append_source_input(&mut input, &sources[*id], &context.source_paths[*id]);
    }
    let related = context
        .related
        .get(&community)
        .into_iter()
        .flatten()
        .filter(|(related, _)| {
            sources
                .values()
                .any(|source| source.communities.contains(related))
        })
        .copied()
        .collect::<Vec<_>>();
    for (related_community, weight) in &related {
        input.extend([
            related_community.to_string(),
            community_title(*related_community, context.labels),
            context.community_paths[related_community].clone(),
            weight.to_bits().to_string(),
        ]);
    }
    let input_sha256 = digest(input.iter().map(String::as_str));
    let citations = select_citations(
        &title,
        "community",
        &community.to_string(),
        &context.topic_paths[&topic.id],
        &input_sha256,
        source_ids.iter().map(|id| citation(&sources[*id])),
    );
    let mut body = format!("# {title}\n\n## Sources\n\n");
    for id in &source_ids {
        let source = &sources[*id];
        body.push_str(&format!(
            "- {}\n",
            markdown_path_link(&source.label, &format!("../{}", context.source_paths[*id]))
        ));
    }
    if !related.is_empty() {
        body.push_str("\n## Related communities\n\n");
        for (related_community, _) in related {
            body.push_str(&format!(
                "- {}\n",
                markdown_path_link(
                    &community_title(related_community, context.labels),
                    &format!("../{}", context.community_paths[&related_community])
                )
            ));
        }
    }
    body.push_str(&format!(
        "\n[← {}](../{})\n",
        escape_markdown_label(&topic.label),
        context.topic_paths[&topic.id]
    ));
    frontmatter(
        &title,
        "community",
        &community.to_string(),
        &context.topic_paths[&topic.id],
        &citations,
        &input_sha256,
        &body,
    )
}

fn related_communities(graph: &KnowledgeGraph) -> BTreeMap<i64, Vec<(i64, f64)>> {
    let mut related = BTreeMap::<i64, Vec<(i64, f64)>>::new();
    for (source, target, weight) in cross_community_relationships(graph) {
        related.entry(source).or_default().push((target, weight));
        related.entry(target).or_default().push((source, weight));
    }
    for neighbors in related.values_mut() {
        neighbors.sort_by(|(left_id, left_weight), (right_id, right_weight)| {
            right_weight
                .total_cmp(left_weight)
                .then_with(|| left_id.cmp(right_id))
        });
    }
    related
}

fn source_page(
    source: &CatalogSource,
    source_path: &str,
    community_paths: &BTreeMap<i64, String>,
    labels: &BTreeMap<i64, String>,
    topic_communities: &BTreeMap<i64, &crate::taxonomy::Topic>,
    topic_paths: &BTreeMap<String, String>,
    inventory_path: Option<&str>,
) -> String {
    let primary = source.communities.iter().next().copied();
    let parent = primary
        .map(|community| community_paths[&community].as_str())
        .unwrap_or("index.md");
    let mut body = format!(
        "# {}\n\nCatalog source `{}`.\n",
        source.label,
        citation(source)
    );
    body.push_str(&provenance_section(source));
    if let Some(community) = primary {
        body.push_str(&format!(
            "\n[← {}](../{})\n",
            escape_markdown_label(&community_title(community, labels)),
            community_paths[&community]
        ));
    } else {
        body.push_str("\n[← Knowledge graph](../index.md)\n");
    }
    if let Some(inventory_path) = inventory_path {
        body.push_str(&format!(
            "\n## Inventory\n\n{}\n",
            markdown_path_link("Inventory", &format!("../{inventory_path}"))
        ));
    }
    let mut input = Vec::new();
    append_source_input(&mut input, source, source_path);
    input.push(parent.into());
    if let Some(inventory_path) = inventory_path {
        input.push(inventory_path.into());
    }
    for community in &source.communities {
        input.extend([
            community.to_string(),
            community_title(*community, labels),
            community_paths[community].clone(),
        ]);
        if let Some(topic) = topic_communities.get(community) {
            input.extend([
                topic.id.clone(),
                topic.label.clone(),
                topic_paths[&topic.id].clone(),
            ]);
        }
    }
    frontmatter(
        &source.label,
        "source",
        &source.graph_ref,
        parent,
        &[citation(source)],
        &digest(input.iter().map(String::as_str)),
        &body,
    )
}

fn inventory_page(source: &CatalogSource, source_path: &str, inventory_path: &str) -> String {
    let mut body = format!(
        "# {} inventory\n\nCatalog capture `{}` has no extracted graph content.\n\n[← {}](../{})\n",
        source.label,
        citation(source),
        escape_markdown_label(&source.label),
        source_path,
    );
    body.push_str(&provenance_section(source));
    let mut input = Vec::new();
    append_source_input(&mut input, source, source_path);
    input.push(inventory_path.into());
    frontmatter(
        &format!("{} inventory", source.label),
        "inventory",
        &source.graph_ref,
        source_path,
        &[],
        &digest(input.iter().map(String::as_str)),
        &body,
    )
}

fn provenance_section(source: &CatalogSource) -> String {
    let Some(provenance) = &source.provenance else {
        return String::new();
    };
    let mut output = String::from("\n## Provenance\n\n");
    for (label, value) in [
        ("Source system", provenance.source_system.as_deref()),
        ("URL", provenance.url.as_deref()),
        ("Location", provenance.location.as_deref()),
        ("Source path", provenance.source_path.as_deref()),
        ("Representation", provenance.representation.as_deref()),
        ("Captured at", provenance.captured_at.as_deref()),
        ("Accessed at", provenance.accessed_at.as_deref()),
        ("Updated at", provenance.updated_at.as_deref()),
    ] {
        if let Some(value) = value {
            output.push_str(&format!("- {label}: {}\n", escape_markdown_text(value)));
        }
    }
    output.push_str(&format!(
        "- SHA-256: {}\n",
        escape_markdown_text(&source.sha256)
    ));
    output
}

fn frontmatter(
    title: &str,
    kind: &str,
    graph_ref: &str,
    parent: &str,
    sources: &[String],
    input_sha256: &str,
    body: &str,
) -> String {
    let mut output = format!(
        "---\ntitle: {}\nkind: {}\ngraph_ref: {}\nparent: {}\ninput_sha256: {}\nsources:\n",
        quoted(title),
        quoted(kind),
        quoted(graph_ref),
        quoted(parent),
        quoted(input_sha256),
    );
    for source in sources {
        output.push_str(&format!("  - {source}\n"));
    }
    output.push_str("---\n\n");
    output.push_str(body);
    output
}

fn quoted(value: &str) -> String {
    serde_json::to_string(value).expect("strings serialize")
}

fn community_title(community: i64, labels: &BTreeMap<i64, String>) -> String {
    labels
        .get(&community)
        .cloned()
        .unwrap_or_else(|| format!("Community {community}"))
}

pub(crate) fn citation(source: &CatalogSource) -> String {
    format!("{}#{}", source.id, source.capture)
}

fn append_source_input(input: &mut Vec<String>, source: &CatalogSource, path: &str) {
    input.extend([
        source.id.clone(),
        source.capture.clone(),
        source.graph_ref.clone(),
        source.sha256.clone(),
        source.label.clone(),
        path.into(),
    ]);
    input.extend(source.communities.iter().map(ToString::to_string));
    if let Some(provenance) = &source.provenance {
        input.push("provenance".into());
        for (name, value) in [
            ("source_system", provenance.source_system.as_deref()),
            ("url", provenance.url.as_deref()),
            ("location", provenance.location.as_deref()),
            ("source_path", provenance.source_path.as_deref()),
            ("representation", provenance.representation.as_deref()),
            ("captured_at", provenance.captured_at.as_deref()),
            ("accessed_at", provenance.accessed_at.as_deref()),
            ("updated_at", provenance.updated_at.as_deref()),
        ] {
            input.push(name.into());
            input.push(value.unwrap_or_default().into());
        }
    }
}

fn select_citations(
    title: &str,
    kind: &str,
    graph_ref: &str,
    parent: &str,
    input_sha256: &str,
    candidates: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let mut citations = Vec::new();
    let mut bytes = frontmatter(title, kind, graph_ref, parent, &citations, input_sha256, "").len();
    for citation in candidates {
        let next = bytes.saturating_add(citation.len() + "  - \n".len());
        if next <= MAX_FRONTMATTER_BYTES {
            bytes = next;
            citations.push(citation);
        } else {
            break;
        }
    }
    citations
}

fn digest<'a>(values: impl IntoIterator<Item = &'a str>) -> String {
    let mut digest = Sha256::new();
    for value in values {
        digest.update(value.as_bytes());
        digest.update([0]);
    }
    hex::encode(digest.finalize())
}

pub(crate) fn valid_catalog_identifier(value: &str) -> bool {
    value.len() <= MAX_CATALOG_ID_BYTES
        && value.as_bytes().split_first().is_some_and(|(first, rest)| {
            first.is_ascii_alphanumeric()
                && rest
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
}

fn valid_catalog_provenance(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_FRONTMATTER_BYTES
        && !value.chars().any(char::is_control)
}

fn valid_catalog_source_path(value: &str) -> bool {
    !value.starts_with('/')
        && value.split('/').all(|component| {
            !component.is_empty()
                && component != "."
                && component != ".."
                && !component.contains('\\')
        })
}

fn bounded_path_component(value: &str) -> String {
    if value.len() <= MAX_FILENAME_COMPONENT_BYTES {
        value.into()
    } else {
        let digest = hex::encode(Sha256::digest(value.as_bytes()));
        let keep = MAX_FILENAME_COMPONENT_BYTES - digest.len() - 1;
        format!("{}-{digest}", truncate_ascii(value, keep))
    }
}

fn truncate_ascii(value: &str, length: usize) -> &str {
    &value[..value.len().min(length)]
}

fn markdown_path_link(label: &str, path: &str) -> String {
    format!("[{}]({path})", escape_markdown_label(label))
}

fn escape_markdown_label(label: &str) -> String {
    label
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

fn escape_markdown_text(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('<', "\\<")
        .replace('>', "\\>")
}

fn display_label(value: &str, fallback: &str) -> String {
    let value = graphoxide_core::sanitize_label(value);
    if value.is_empty() {
        fallback.into()
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str, label: &str) -> CatalogSource {
        CatalogSource {
            id: id.into(),
            capture: "capture".into(),
            graph_ref: id.into(),
            sha256: "a".repeat(64),
            label: label.into(),
            communities: BTreeSet::from([1]),
            represented: true,
            provenance: None,
        }
    }

    #[test]
    fn fallback_navigation_titles_describe_a_multi_source_collection() {
        let sources = BTreeMap::from([
            (
                "access".into(),
                source("access", "Equipment documentation — Access"),
            ),
            (
                "network".into(),
                source("network", "Equipment documentation — Network"),
            ),
        ]);
        let labels =
            readable_community_labels(&KnowledgeGraph::default(), &sources, &BTreeSet::from([1]));
        let topic = crate::taxonomy::Topic {
            id: "topic-0".into(),
            label: "Topic 0".into(),
            communities: vec![1],
        };

        assert_eq!(labels[&1], "Equipment documentation collection");
        assert_eq!(
            readable_topic_label(&topic, &labels, &sources),
            "Equipment documentation collection"
        );
    }

    #[test]
    fn navigation_paths_keep_unicode_titles_readable() {
        let path = readable_navigation_path("topics", "设备 概览", "topic-7");
        assert!(path.starts_with("topics/设备-概览-"));
        assert!(!path.starts_with("topics/item-"));
    }

    #[test]
    fn catalog_source_names_derive_from_the_tracked_source_path() {
        assert_eq!(
            source_display_label("firmware-guide", "docs/Firmware update.pdf"),
            "Firmware update"
        );
        assert_eq!(source_display_label("firmware-guide", ""), "firmware-guide");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GodNodeArticle {
    pub id: String,
    pub label: String,
    pub degree: usize,
}

#[derive(Debug, Clone, Default)]
pub struct WikiOptions {
    pub community_labels: BTreeMap<i64, String>,
    pub cohesion: BTreeMap<i64, f64>,
    pub god_nodes: Vec<GodNodeArticle>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WikiReport {
    pub article_count: usize,
    pub stale_nodes_dropped: usize,
    pub warnings: Vec<String>,
}

/// Backward-compatible CLI entry point. Graph community metadata is used when
/// present; an unclustered graph becomes one community so the export remains
/// useful before clustering.
pub fn export_wiki(graph: &KnowledgeGraph, directory: &Path) -> anyhow::Result<()> {
    let mut communities = communities_from_graph(graph);
    if communities.is_empty() && !graph.nodes.is_empty() {
        communities.insert(0, graph.nodes.iter().map(|node| node.id.clone()).collect());
    }
    let options = WikiOptions {
        community_labels: community_labels_from_graph(graph),
        ..WikiOptions::default()
    };
    export_wiki_with_options(graph, &communities, directory, &options).map(|_| ())
}

pub fn export_wiki_with_options(
    graph: &KnowledgeGraph,
    communities: &Communities,
    directory: &Path,
    options: &WikiOptions,
) -> anyhow::Result<WikiReport> {
    fs::create_dir_all(directory)?;
    let by_id = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<HashMap<_, _>>();
    let mut report = WikiReport::default();
    let mut valid = Communities::new();
    for (community, members) in communities {
        let retained = members
            .iter()
            .filter(|id| by_id.contains_key(id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        report.stale_nodes_dropped += members.len().saturating_sub(retained.len());
        if !retained.is_empty() {
            valid.insert(*community, retained);
        }
    }
    if !communities.is_empty() && valid.is_empty() {
        anyhow::bail!("all community node IDs are stale; rebuild communities before wiki export");
    }
    if report.stale_nodes_dropped > 0 {
        let warning = format!(
            "dropped {} stale community node ID(s) during wiki export",
            report.stale_nodes_dropped
        );
        eprintln!("warning: {warning}");
        report.warnings.push(warning);
    }

    let node_community = reverse_communities(&valid);
    let mut used_slugs = BTreeSet::new();
    let mut community_articles = BTreeMap::new();
    for community in valid.keys() {
        let label = community_label(*community, options);
        community_articles.insert(*community, allocate_slug(&label, &mut used_slugs));
    }
    let valid_gods = options
        .god_nodes
        .iter()
        .filter(|god| by_id.contains_key(god.id.as_str()))
        .collect::<Vec<_>>();
    let mut god_articles = BTreeMap::new();
    for god in &valid_gods {
        god_articles.insert(god.id.clone(), allocate_slug(&god.label, &mut used_slugs));
    }

    for (community, members) in &valid {
        let label = community_label(*community, options);
        let body = community_article(
            graph,
            *community,
            members,
            &label,
            options.cohesion.get(community).copied(),
            &node_community,
            &community_articles,
            options,
        );
        write_markdown(directory, &community_articles[community], &body)?;
        report.article_count += 1;
    }
    for god in valid_gods {
        let body = god_node_article(
            graph,
            god,
            &node_community,
            &community_articles,
            &god_articles,
            options,
        );
        write_markdown(directory, &god_articles[&god.id], &body)?;
        report.article_count += 1;
    }
    let index = index_markdown(
        &valid,
        &community_articles,
        &valid_god_data(options, &by_id),
        &god_articles,
        options,
    );
    graphoxide_core::write_text_atomic(directory.join("index.md"), &index)?;
    Ok(report)
}

fn valid_god_data<'a>(
    options: &'a WikiOptions,
    by_id: &HashMap<&str, &Node>,
) -> Vec<&'a GodNodeArticle> {
    options
        .god_nodes
        .iter()
        .filter(|god| by_id.contains_key(god.id.as_str()))
        .collect()
}

fn index_markdown(
    communities: &Communities,
    community_articles: &BTreeMap<i64, String>,
    gods: &[&GodNodeArticle],
    god_articles: &BTreeMap<String, String>,
    options: &WikiOptions,
) -> String {
    let mut output = String::from("# Knowledge graph\n\n## Communities\n\n");
    for community in communities.keys() {
        let label = community_label(*community, options);
        output.push_str(&format!(
            "- {}\n",
            markdown_link(&label, &community_articles[community])
        ));
    }
    if !gods.is_empty() {
        output.push_str("\n## Highly connected nodes\n\n");
        for god in gods {
            output.push_str(&format!(
                "- {} — {} connections\n",
                markdown_link(&god.label, &god_articles[&god.id]),
                god.degree
            ));
        }
    }
    output
}

#[allow(clippy::too_many_arguments)]
fn community_article(
    graph: &KnowledgeGraph,
    community: i64,
    members: &[String],
    label: &str,
    cohesion: Option<f64>,
    node_community: &HashMap<&str, i64>,
    articles: &BTreeMap<i64, String>,
    options: &WikiOptions,
) -> String {
    let by_id = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<HashMap<_, _>>();
    let mut output = format!("# {label}\n\n");
    if let Some(cohesion) = cohesion {
        output.push_str(&format!("_Community cohesion {cohesion:.2}_\n\n"));
    }
    output.push_str("## Nodes\n\n");
    let mut nodes = members
        .iter()
        .filter_map(|id| by_id.get(id.as_str()).copied())
        .collect::<Vec<_>>();
    nodes.sort_by(|left, right| {
        left.label
            .to_ascii_lowercase()
            .cmp(&right.label.to_ascii_lowercase())
            .then_with(|| left.source_file.cmp(&right.source_file))
            .then_with(|| left.id.cmp(&right.id))
    });
    for node in nodes.iter().take(25) {
        if node.source_file.is_empty() {
            output.push_str(&format!("- {}\n", node.label));
        } else {
            output.push_str(&format!("- {} — `{}`\n", node.label, node.source_file));
        }
    }
    if nodes.len() > 25 {
        output.push_str(&format!("- _and {} more nodes_\n", nodes.len() - 25));
    }

    let member_ids = members.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let mut audit = graph
        .links
        .iter()
        .filter(|edge| {
            member_ids.contains(edge.true_source()) || member_ids.contains(edge.true_target())
        })
        .collect::<Vec<_>>();
    audit.sort_by(|left, right| {
        left.true_source()
            .cmp(right.true_source())
            .then_with(|| left.true_target().cmp(right.true_target()))
            .then_with(|| left.relation.cmp(&right.relation))
    });
    if !audit.is_empty() {
        output.push_str("\n## Relationship audit\n\n");
        for edge in audit.iter().take(50) {
            let source = by_id
                .get(edge.true_source())
                .map_or(edge.true_source(), |node| node.label.as_str());
            let target = by_id
                .get(edge.true_target())
                .map_or(edge.true_target(), |node| node.label.as_str());
            output.push_str(&format!(
                "- {source} —{}→ {target} [{}]\n",
                edge.relation,
                confidence_name(edge.confidence)
            ));
        }
    }

    let mut cross = BTreeSet::new();
    for edge in &graph.links {
        for (inside, outside) in [
            (edge.true_source(), edge.true_target()),
            (edge.true_target(), edge.true_source()),
        ] {
            if member_ids.contains(inside)
                && let Some(other) = node_community.get(outside).copied()
                && other != community
            {
                cross.insert(other);
            }
        }
    }
    if !cross.is_empty() {
        output.push_str("\n## Related communities\n\n");
        for other in cross {
            output.push_str(&format!(
                "- {}\n",
                markdown_link(&community_label(other, options), &articles[&other])
            ));
        }
    }
    output.push_str("\n---\n[← index](index.md)\n");
    output
}

fn god_node_article(
    graph: &KnowledgeGraph,
    god: &GodNodeArticle,
    node_community: &HashMap<&str, i64>,
    community_articles: &BTreeMap<i64, String>,
    god_articles: &BTreeMap<String, String>,
    options: &WikiOptions,
) -> String {
    let by_id = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<HashMap<_, _>>();
    let mut output = format!("# {}\n\n_{} connections_\n", god.label, god.degree);
    if let Some(community) = node_community.get(god.id.as_str()) {
        output.push_str(&format!(
            "\nCommunity: {}\n",
            markdown_link(
                &community_label(*community, options),
                &community_articles[community]
            )
        ));
    }
    let mut neighbors = BTreeSet::new();
    for edge in &graph.links {
        if edge.true_source() == god.id {
            neighbors.insert(edge.true_target());
        } else if edge.true_target() == god.id {
            neighbors.insert(edge.true_source());
        }
    }
    if !neighbors.is_empty() {
        output.push_str("\n## Connections\n\n");
        for id in neighbors {
            let label = by_id.get(id).map_or(id, |node| node.label.as_str());
            if let Some(article) = god_articles.get(id) {
                output.push_str(&format!("- {}\n", markdown_link(label, article)));
            } else {
                output.push_str(&format!("- {label}\n"));
            }
        }
    }
    output.push_str("\n---\n[← index](index.md)\n");
    output
}

fn reverse_communities(communities: &Communities) -> HashMap<&str, i64> {
    communities
        .iter()
        .flat_map(|(community, members)| {
            members
                .iter()
                .map(move |member| (member.as_str(), *community))
        })
        .collect()
}

fn community_label(community: i64, options: &WikiOptions) -> String {
    options
        .community_labels
        .get(&community)
        .cloned()
        .unwrap_or_else(|| format!("Community {community}"))
}

fn allocate_slug(label: &str, used: &mut BTreeSet<String>) -> String {
    let base = safe_filename(label);
    let mut candidate = base.clone();
    let mut suffix = 2_u64;
    while used.contains(&candidate.to_ascii_lowercase()) {
        candidate = format!("{base}_{suffix}");
        suffix += 1;
    }
    used.insert(candidate.to_ascii_lowercase());
    candidate
}

fn safe_filename(label: &str) -> String {
    let mut result = String::new();
    let mut separator = false;
    for character in label.trim().chars() {
        if character.is_whitespace() {
            separator = true;
            continue;
        }
        if separator && !result.is_empty() {
            result.push('_');
        }
        separator = false;
        if matches!(
            character,
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
        ) {
            result.push('_');
        } else {
            result.push(character);
        }
    }
    let result = result.trim_matches(['.', '_']).to_owned();
    if result.is_empty() {
        "Community".into()
    } else {
        result
    }
}

fn markdown_link(label: &str, slug: &str) -> String {
    let display = label.replace('[', "\\[").replace(']', "\\]");
    format!("[{display}]({}.md)", percent_encode(slug))
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'-' | b'.' | b'~') {
            encoded.push(char::from(*byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn confidence_name(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::Extracted => "EXTRACTED",
        Confidence::Inferred => "INFERRED",
        Confidence::Ambiguous => "AMBIGUOUS",
    }
}

fn write_markdown(directory: &Path, slug: &str, body: &str) -> anyhow::Result<()> {
    graphoxide_core::write_text_atomic(directory.join(format!("{slug}.md")), body)
}
