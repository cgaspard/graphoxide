//! Strict reviewer-owned plan for deterministic wiki generation.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_WIKI_PLAN_DOMAINS: usize = 256;
/// Bounds the reviewed manifest, not the size of a knowledgebase or its graph.
pub const MAX_WIKI_PLAN_SOURCES: usize = 65_536;
/// Bounds the reviewed manifest, not the size of a knowledgebase or its graph.
pub const MAX_WIKI_PLAN_ARTICLES: usize = 65_536;
const MAX_PATH_COMPONENT_BYTES: usize = 200;
const MAX_TEXT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiPlan {
    pub version: u32,
    pub domains: Vec<WikiPlanDomain>,
    pub sources: Vec<WikiPlanSource>,
    pub articles: Vec<WikiPlanArticle>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiPlanDomain {
    pub id: String,
    pub title: String,
    pub slug: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiPlanSource {
    /// Catalog citation key in `source-id#capture-id` form.
    pub id: String,
    pub title: String,
    pub slug: String,
    pub domain: String,
    pub coverage: WikiPlanCoverage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WikiPlanCoverage {
    Complete,
    Partial,
    InventoryOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiPlanArticle {
    pub id: String,
    pub title: String,
    pub slug: String,
    pub domain: String,
    pub article_type: WikiArticleType,
    pub sources: Vec<String>,
    pub aliases: Vec<String>,
    pub related: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WikiArticleType {
    Overview,
    Concept,
    Component,
    Interface,
    Behavior,
    Procedure,
    Reference,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WikiPlanPathKind {
    Source,
    Reference,
    Inventory,
}

/// Deserialize the strict versioned model without resolving its references.
pub fn parse_wiki_plan(bytes: &[u8]) -> anyhow::Result<WikiPlan> {
    let plan: WikiPlan = serde_json::from_slice(bytes)?;
    anyhow::ensure!(plan.version == 1, "wiki plan version must be 1");
    Ok(plan)
}

/// Deserialize and validate a reviewed plan against the active catalog.
pub fn load_wiki_plan(
    bytes: &[u8],
    catalog_citations: &BTreeSet<String>,
) -> anyhow::Result<WikiPlan> {
    let plan = parse_wiki_plan(bytes)?;
    plan.validate(catalog_citations)?;
    Ok(plan)
}

impl WikiPlan {
    pub fn validate(&self, catalog_citations: &BTreeSet<String>) -> anyhow::Result<()> {
        anyhow::ensure!(self.version == 1, "wiki plan version must be 1");
        anyhow::ensure!(
            !self.domains.is_empty(),
            "wiki plan domains must not be empty"
        );
        anyhow::ensure!(
            self.domains.len() <= MAX_WIKI_PLAN_DOMAINS,
            "wiki plan has more than {MAX_WIKI_PLAN_DOMAINS} domains"
        );
        anyhow::ensure!(
            self.sources.len() <= MAX_WIKI_PLAN_SOURCES,
            "wiki plan has more than {MAX_WIKI_PLAN_SOURCES} sources"
        );
        anyhow::ensure!(
            self.articles.len() <= MAX_WIKI_PLAN_ARTICLES,
            "wiki plan has more than {MAX_WIKI_PLAN_ARTICLES} articles"
        );

        let mut domain_ids = BTreeSet::new();
        let mut domain_slugs = BTreeSet::new();
        let mut domain_slug_by_id = BTreeMap::new();
        for domain in &self.domains {
            validate_id(&domain.id, "domain id")?;
            validate_title(&domain.title, "domain title")?;
            validate_slug(&domain.slug, "domain slug")?;
            anyhow::ensure!(
                domain_ids.insert(domain.id.as_str()),
                "duplicate domain id {:?}",
                domain.id
            );
            anyhow::ensure!(
                domain_slugs.insert(domain.slug.as_str()),
                "duplicate domain slug {:?}",
                domain.slug
            );
            domain_slug_by_id.insert(domain.id.as_str(), domain.slug.as_str());
        }

        let mut source_ids = BTreeSet::new();
        let mut source_paths = BTreeSet::new();
        for source in &self.sources {
            validate_citation(&source.id)?;
            validate_title(&source.title, "source title")?;
            validate_slug(&source.slug, "source slug")?;
            anyhow::ensure!(
                domain_ids.contains(source.domain.as_str()),
                "source {:?} has unknown domain {:?}",
                source.id,
                source.domain
            );
            anyhow::ensure!(
                catalog_citations.contains(&source.id),
                "source {:?} is absent from the catalog",
                source.id
            );
            anyhow::ensure!(
                source_ids.insert(source.id.as_str()),
                "duplicate source id {:?}",
                source.id
            );
            let path = Self::canonical_support_path(WikiPlanPathKind::Source, &source.slug)?;
            anyhow::ensure!(source_paths.insert(path), "duplicate emitted source path");
        }

        let mut article_ids = BTreeSet::new();
        let mut article_paths = BTreeSet::new();
        for article in &self.articles {
            validate_id(&article.id, "article id")?;
            validate_title(&article.title, "article title")?;
            validate_slug(&article.slug, "article slug")?;
            let domain_slug = domain_slug_by_id
                .get(article.domain.as_str())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "article {:?} has unknown domain {:?}",
                        article.id,
                        article.domain
                    )
                })?;
            let path = canonical_article_path(domain_slug, &article.slug, &article.id)?;
            anyhow::ensure!(
                article_paths.insert(path.clone()),
                "duplicate emitted article path {path:?}"
            );
            anyhow::ensure!(
                article_ids.insert(article.id.as_str()),
                "duplicate article id {:?}",
                article.id
            );
        }

        for article in &self.articles {
            anyhow::ensure!(
                !article.sources.is_empty(),
                "article {:?} must have at least one source citation",
                article.id
            );
            let mut citations = BTreeSet::new();
            for citation in &article.sources {
                validate_citation(citation)?;
                anyhow::ensure!(
                    citations.insert(citation),
                    "article {:?} has duplicate source citation {:?}",
                    article.id,
                    citation
                );
                anyhow::ensure!(
                    source_ids.contains(citation.as_str()) && catalog_citations.contains(citation),
                    "article {:?} has unknown source citation {:?}",
                    article.id,
                    citation
                );
            }
            validate_unique_text(&article.aliases, "alias", &article.id)?;
            let mut related = BTreeSet::new();
            for related_id in &article.related {
                validate_id(related_id, "related article id")?;
                anyhow::ensure!(
                    related.insert(related_id),
                    "article {:?} has duplicate related id {:?}",
                    article.id,
                    related_id
                );
                anyhow::ensure!(
                    related_id != &article.id && article_ids.contains(related_id.as_str()),
                    "article {:?} has unknown related id {:?}",
                    article.id,
                    related_id
                );
            }
        }
        Ok(())
    }

    pub fn article_path(&self, article_id: &str) -> anyhow::Result<String> {
        let article = self
            .articles
            .iter()
            .find(|article| article.id == article_id)
            .ok_or_else(|| anyhow::anyhow!("unknown article id {article_id:?}"))?;
        let domain = self
            .domains
            .iter()
            .find(|domain| domain.id == article.domain)
            .ok_or_else(|| anyhow::anyhow!("unknown domain id {:?}", article.domain))?;
        canonical_article_path(&domain.slug, &article.slug, &article.id)
    }

    pub fn source_path(&self, citation: &str) -> anyhow::Result<String> {
        let source = self
            .sources
            .iter()
            .find(|source| source.id == citation)
            .ok_or_else(|| anyhow::anyhow!("unknown source citation {citation:?}"))?;
        Self::canonical_support_path(WikiPlanPathKind::Source, &source.slug)
    }

    pub fn canonical_support_path(kind: WikiPlanPathKind, name: &str) -> anyhow::Result<String> {
        validate_slug(name, "wiki output name")?;
        let directory = match kind {
            WikiPlanPathKind::Source => "sources",
            WikiPlanPathKind::Reference => "references",
            WikiPlanPathKind::Inventory => "inventory",
        };
        let file = format!("{name}.md");
        anyhow::ensure!(
            file.len() <= MAX_PATH_COMPONENT_BYTES,
            "wiki output filename is too long"
        );
        Ok(format!("{directory}/{file}"))
    }
}

fn canonical_article_path(domain: &str, slug: &str, id: &str) -> anyhow::Result<String> {
    validate_slug(domain, "article domain slug")?;
    validate_slug(slug, "article slug")?;
    validate_id(id, "article id")?;
    let file = format!("{slug}--{id}.md");
    anyhow::ensure!(
        file.len() <= MAX_PATH_COMPONENT_BYTES,
        "article output filename is too long"
    );
    Ok(format!("{domain}/{file}"))
}

fn validate_id(value: &str, kind: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        super::wiki::valid_catalog_identifier(value),
        "unsafe {kind} {value:?}"
    );
    Ok(())
}

fn validate_citation(value: &str) -> anyhow::Result<()> {
    let Some((source_id, capture_id)) = value.split_once('#') else {
        anyhow::bail!("invalid source citation {value:?}");
    };
    anyhow::ensure!(
        !capture_id.contains('#')
            && super::wiki::valid_catalog_identifier(source_id)
            && super::wiki::valid_catalog_identifier(capture_id),
        "invalid source citation {value:?}"
    );
    Ok(())
}

fn validate_slug(value: &str, kind: &str) -> anyhow::Result<()> {
    let bytes = value.as_bytes();
    anyhow::ensure!(
        bytes.len() <= MAX_PATH_COMPONENT_BYTES
            && bytes
                .first()
                .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            && bytes
                .last()
                .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-'),
        "unsafe {kind} {value:?}"
    );
    Ok(())
}

fn validate_title(value: &str, kind: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.trim().is_empty()
            && value.len() <= MAX_TEXT_BYTES
            && !value.chars().any(char::is_control),
        "invalid {kind}"
    );
    Ok(())
}

fn validate_unique_text(values: &[String], kind: &str, article_id: &str) -> anyhow::Result<()> {
    let mut seen = BTreeSet::new();
    for value in values {
        validate_title(value, kind)?;
        anyhow::ensure!(
            seen.insert(value),
            "article {article_id:?} has duplicate {kind} {value:?}"
        );
    }
    Ok(())
}
