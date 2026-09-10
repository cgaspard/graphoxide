//! Deterministic, pointer-only source materialization for Hugo.
//!
//! This module never opens a locator or receives a source body. The caller
//! supplies the validated source index and explicit, digest-bound assignments.

use anyhow::{ensure, Result};
use graphoxide_export::direct_taxonomy::{
    assignments_digest, taxonomy_policy_digest, validate_incremental_assignments,
    validate_navigation_assignments, Assignments, TaxonomyPolicy,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

use crate::wiki_source::{SourceIndex, SourceLocation, SourceStatus};

const SOURCE_INDEX_SCHEMA: &str = "graphoxide.source-index";

/// The complete direct-source rendering input for a Hugo adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DirectMaterialization {
    pub policy_sha256: String,
    pub assignments_sha256: String,
    pub pages: Vec<DirectSourcePage>,
    pub domains: Vec<DirectBrowseGroup>,
    pub subjects: Vec<DirectBrowseGroup>,
    pub facets: Vec<DirectFacetAxis>,
    pub applicability: Vec<DirectBrowseGroup>,
}

/// A generated source page. The source body is intentionally not representable here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DirectSourcePage {
    pub page_id: String,
    pub title: String,
    pub route: String,
    pub source_id: String,
    pub location: SourceLocation,
    pub content_sha256: String,
    pub bytes: u64,
    pub status: SourceStatus,
    pub primary_subject: String,
    pub facets: BTreeMap<String, Vec<String>>,
    pub applicability: Vec<String>,
}

impl DirectSourcePage {
    /// Controlled, status-only prose for rendering a provisional source page.
    pub fn status_notice(&self) -> &'static str {
        status_notice(&self.status)
    }
}

/// A labeled, deterministic navigation route and its source page members.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DirectBrowseGroup {
    pub id: String,
    pub label: String,
    pub route: String,
    pub page_ids: Vec<String>,
}

/// One configured facet axis and all of its populated browse terms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DirectFacetAxis {
    pub id: String,
    pub label: String,
    pub terms: Vec<DirectBrowseGroup>,
}

/// Build a deterministic Hugo-ready manifest without reading any source body.
pub fn materialize_direct_sources(
    index: &SourceIndex,
    policy: &TaxonomyPolicy,
    assignments: &Assignments,
) -> Result<DirectMaterialization> {
    ensure!(
        index.schema == SOURCE_INDEX_SCHEMA,
        "source index has an unsupported schema"
    );
    let sources = source_revisions(index)?;
    validate_incremental_assignments(assignments, policy, &sources)?;

    let by_id = index
        .sources
        .iter()
        .map(|source| (source.source_id.as_str(), source))
        .collect::<BTreeMap<_, _>>();
    let mut domain_pages = BTreeMap::<&str, BTreeSet<String>>::new();
    let mut subject_pages = BTreeMap::<&str, BTreeSet<String>>::new();
    let mut facet_pages = BTreeMap::<(&str, &str), BTreeSet<String>>::new();
    let mut applicability_pages = BTreeMap::<&str, BTreeSet<String>>::new();
    let mut pages = Vec::new();

    for assignment in &assignments.assignments {
        let source = by_id[assignment.source_id.as_str()];
        let (domain, _) = assignment
            .primary_subject
            .split_once('/')
            .expect("validated subject contains domain");
        for page_id in &assignment.page_ids {
            pages.push(DirectSourcePage {
                page_id: page_id.clone(),
                title: title_from_id(page_id),
                route: format!("/content/{page_id}/"),
                source_id: source.source_id.clone(),
                location: source.location.clone(),
                content_sha256: source.content_sha256.clone(),
                bytes: source.bytes,
                status: source.status.clone(),
                primary_subject: assignment.primary_subject.clone(),
                facets: assignment.facets.clone(),
                applicability: assignment.applicability.clone(),
            });
            domain_pages
                .entry(domain)
                .or_default()
                .insert(page_id.clone());
            subject_pages
                .entry(&assignment.primary_subject)
                .or_default()
                .insert(page_id.clone());
            for (axis, terms) in &assignment.facets {
                for term in terms {
                    facet_pages
                        .entry((axis, term))
                        .or_default()
                        .insert(page_id.clone());
                }
            }
            for term in &assignment.applicability {
                applicability_pages
                    .entry(term)
                    .or_default()
                    .insert(page_id.clone());
            }
        }
    }
    pages.sort_by(|left, right| left.page_id.cmp(&right.page_id));

    Ok(DirectMaterialization {
        policy_sha256: taxonomy_policy_digest(policy)?,
        assignments_sha256: assignments_digest(assignments, policy, &sources)?,
        pages,
        domains: policy
            .domains
            .iter()
            .map(|domain| {
                browse_group(
                    &domain.id,
                    &domain.label,
                    format!("/domains/{}/", domain.id),
                    domain_pages.remove(domain.id.as_str()),
                )
            })
            .collect(),
        subjects: policy
            .subjects
            .iter()
            .map(|subject| {
                browse_group(
                    &subject.id,
                    &subject.label,
                    format!("/topics/{}/", subject.id),
                    subject_pages.remove(subject.id.as_str()),
                )
            })
            .collect(),
        facets: policy
            .facets
            .iter()
            .filter(|axis| axis.id != "applicability")
            .map(|axis| DirectFacetAxis {
                id: axis.id.clone(),
                label: axis.label.clone(),
                terms: axis
                    .terms
                    .iter()
                    .map(|term| {
                        browse_group(
                            &format!("{}/{}", axis.id, term.id),
                            &term.label,
                            format!("/browse/{}/{}/", axis.id, term.id),
                            facet_pages.remove(&(axis.id.as_str(), term.id.as_str())),
                        )
                    })
                    .collect(),
            })
            .collect(),
        applicability: policy
            .facets
            .iter()
            .find(|axis| axis.id == "applicability")
            .map(|axis| {
                axis.terms
                    .iter()
                    .map(|term| {
                        browse_group(
                            &format!("applicability/{}", term.id),
                            &term.label,
                            format!("/browse/applicability/{}/", term.id),
                            applicability_pages.remove(term.id.as_str()),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// Reject activation until every configured navigation route has a source page.
pub fn validate_direct_navigation(
    index: &SourceIndex,
    policy: &TaxonomyPolicy,
    assignments: &Assignments,
) -> Result<()> {
    ensure!(
        index.schema == SOURCE_INDEX_SCHEMA,
        "source index has an unsupported schema"
    );
    validate_navigation_assignments(assignments, policy, &source_revisions(index)?)
}

fn source_revisions(index: &SourceIndex) -> Result<BTreeMap<String, String>> {
    let mut revisions = BTreeMap::new();
    for source in &index.sources {
        ensure!(
            revisions
                .insert(source.source_id.clone(), source.content_sha256.clone())
                .is_none(),
            "source index has duplicate source ids"
        );
    }
    Ok(revisions)
}

fn browse_group(
    id: &str,
    label: &str,
    route: String,
    page_ids: Option<BTreeSet<String>>,
) -> DirectBrowseGroup {
    let page_ids = page_ids.unwrap_or_default().into_iter().collect::<Vec<_>>();
    DirectBrowseGroup {
        id: id.into(),
        label: label.into(),
        route,
        page_ids,
    }
}

fn title_from_id(page_id: &str) -> String {
    page_id
        .split('-')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn status_notice(status: &SourceStatus) -> &'static str {
    match status {
        SourceStatus::Provisional => {
            "This page is provisional. The original source body is not stored in this knowledgebase."
        }
        SourceStatus::AiReviewed => {
            "This page has an AI review attestation. The original source body is not stored in this knowledgebase."
        }
        SourceStatus::HumanConfirmed => {
            "This page has been human confirmed. The original source body is not stored in this knowledgebase."
        }
        SourceStatus::StaleError => {
            "This source record is stale; refresh it before treating the indexed revision as current. The original source body is not stored in this knowledgebase."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki_source::{SourceEntry, SourceIndex, SourceLocation, SourceStatus};
    use graphoxide_export::direct_taxonomy::{
        produce_assignments, AssignmentInput, Assignments, Domain, FacetAxis, FacetTerm, Subject,
        TaxonomyPolicy,
    };
    use std::collections::BTreeMap;

    fn policy() -> TaxonomyPolicy {
        TaxonomyPolicy {
            schema: "graphoxide.taxonomy-policy".into(),
            domains: vec![Domain {
                id: "interfaces".into(),
                label: "Interfaces".into(),
            }],
            subjects: vec![Subject {
                id: "interfaces/protocols".into(),
                label: "Protocols".into(),
            }],
            facets: vec![FacetAxis {
                id: "layer".into(),
                label: "Layer".into(),
                terms: vec![FacetTerm {
                    id: "application".into(),
                    label: "Application".into(),
                }],
            }],
        }
    }

    fn source(status: SourceStatus) -> SourceEntry {
        SourceEntry {
            source_id: format!("src:{}", "a".repeat(64)),
            location: SourceLocation::Https {
                url: "https://example.test/spec.md".into(),
            },
            content_sha256: "b".repeat(64),
            bytes: 42,
            status,
        }
    }

    fn assignments(policy: &TaxonomyPolicy, source: &SourceEntry) -> Assignments {
        produce_assignments(
            policy,
            &BTreeMap::from([(source.source_id.clone(), source.content_sha256.clone())]),
            [AssignmentInput {
                source_id: source.source_id.clone(),
                content_sha256: source.content_sha256.clone(),
                primary_subject: "interfaces/protocols".into(),
                facets: BTreeMap::from([("layer".into(), vec!["application".into()])]),
                applicability: Vec::new(),
                page_ids: vec!["protocol-overview".into()],
            }],
        )
        .expect("valid explicit assignment")
    }

    #[test]
    fn materializes_a_deterministic_provisional_source_page_and_navigation() {
        let policy = policy();
        let source = source(SourceStatus::Provisional);
        let index = SourceIndex {
            schema: "graphoxide.source-index".into(),
            sources: vec![source.clone()],
        };
        let assignments = assignments(&policy, &source);

        let first = materialize_direct_sources(&index, &policy, &assignments)
            .expect("materialize pointer-only source");
        let second = materialize_direct_sources(&index, &policy, &assignments)
            .expect("materialize pointer-only source again");

        assert_eq!(first, second);
        assert_eq!(first.pages.len(), 1);
        assert_eq!(first.pages[0].page_id, "protocol-overview");
        assert_eq!(first.pages[0].route, "/content/protocol-overview/");
        assert_eq!(first.pages[0].status, SourceStatus::Provisional);
        assert_eq!(
            first.pages[0].status_notice(),
            "This page is provisional. The original source body is not stored in this knowledgebase."
        );
        assert_eq!(first.subjects[0].route, "/topics/interfaces/protocols/");
        assert_eq!(first.facets[0].terms[0].route, "/browse/layer/application/");
    }

    #[test]
    fn keeps_stale_source_visible_with_a_refresh_notice() {
        let policy = policy();
        let source = source(SourceStatus::StaleError);
        let index = SourceIndex {
            schema: "graphoxide.source-index".into(),
            sources: vec![source.clone()],
        };

        let projection =
            materialize_direct_sources(&index, &policy, &assignments(&policy, &source))
                .expect("stale pointer remains navigable");

        assert_eq!(projection.pages[0].status, SourceStatus::StaleError);
        assert!(projection.pages[0].status_notice().contains("stale"));
    }

    #[test]
    fn materializes_a_provisional_subset_before_navigation_coverage_is_complete() {
        let mut policy = policy();
        policy.domains.push(Domain {
            id: "models".into(),
            label: "Models".into(),
        });
        policy.subjects.push(Subject {
            id: "models/physics".into(),
            label: "Physics".into(),
        });
        policy.facets[0].terms.push(FacetTerm {
            id: "transport".into(),
            label: "Transport".into(),
        });
        let source = source(SourceStatus::Provisional);
        let index = SourceIndex {
            schema: "graphoxide.source-index".into(),
            sources: vec![source.clone()],
        };

        let projection =
            materialize_direct_sources(&index, &policy, &assignments(&policy, &source))
                .expect("a provisional source must not wait for corpus-wide coverage");

        assert!(
            validate_direct_navigation(&index, &policy, &assignments(&policy, &source)).is_err(),
            "incomplete coverage must block navigation activation"
        );
        assert_eq!(projection.pages.len(), 1);
        assert!(projection.domains[1].page_ids.is_empty());
        assert!(projection.subjects[1].page_ids.is_empty());
        assert!(projection.facets[0].terms[1].page_ids.is_empty());
    }
}
