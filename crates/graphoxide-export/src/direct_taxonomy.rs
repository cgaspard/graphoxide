//! Direct-source taxonomy configuration and digest-bound assignments.
//!
//! This contract deliberately knows nothing about stored source bodies. Callers
//! provide the current `source_id -> content_sha256` revision map from the
//! pointer-only source index.

use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const TAXONOMY_POLICY_SCHEMA: &str = "graphoxide.taxonomy-policy";
pub const TAXONOMY_ASSIGNMENTS_SCHEMA: &str = "graphoxide.taxonomy-assignments";
pub const REVIEW_ATTESTATION_SCHEMA: &str = "graphoxide.review-attestation";
const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaxonomyPolicy {
    pub schema: String,
    pub domains: Vec<Domain>,
    pub subjects: Vec<Subject>,
    pub facets: Vec<FacetAxis>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Domain {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subject {
    /// A stable `domain/subject` identifier.
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FacetAxis {
    pub id: String,
    pub label: String,
    pub terms: Vec<FacetTerm>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FacetTerm {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assignments {
    pub schema: String,
    pub policy_sha256: String,
    pub assignments: Vec<Assignment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    pub source_id: String,
    pub content_sha256: String,
    pub primary_subject: String,
    pub facets: BTreeMap<String, Vec<String>>,
    pub applicability: Vec<String>,
    pub page_ids: Vec<String>,
}

/// Explicit classification input for one source revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssignmentInput {
    pub source_id: String,
    pub content_sha256: String,
    pub primary_subject: String,
    pub facets: BTreeMap<String, Vec<String>>,
    pub applicability: Vec<String>,
    pub page_ids: Vec<String>,
}

/// A privacy-safe record of an explicit review of one source revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectReviewAttestation {
    pub schema: String,
    pub source_id: String,
    pub content_sha256: String,
    pub derived_page_sha256: String,
    pub policy_sha256: String,
    /// Digest of this source's canonical assignment, excluding other sources.
    pub assignments_sha256: String,
    pub reviewer: Reviewer,
    pub decision: ReviewDecision,
    pub reviewed_at: String,
}

/// The review actor is deliberately non-identifying.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Reviewer {
    Ai { model_sha256: String },
    Human,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewDecision {
    Approve,
    Reject,
}

/// The reviewed starting vocabulary for a technical knowledgebase.
///
/// Callers may persist and edit the returned value; classification never adds
/// an implicit subject or facet outside this policy.
pub fn default_taxonomy_policy() -> TaxonomyPolicy {
    TaxonomyPolicy {
        schema: TAXONOMY_POLICY_SCHEMA.into(),
        domains: vec![
            domain("engineering-assurance", "Engineering Assurance"),
            domain("experience-and-interaction", "Experience and Interaction"),
            domain("interfaces-and-protocols", "Interfaces and Protocols"),
            domain("models-and-simulation", "Models and Simulation"),
            domain("physical-infrastructure", "Physical Infrastructure"),
            domain("systems-and-runtime", "Systems and Runtime"),
        ],
        subjects: vec![
            subject(
                "engineering-assurance/integration-and-gaps",
                "Integration and Gaps",
            ),
            subject(
                "engineering-assurance/source-governance",
                "Source Governance",
            ),
            subject(
                "engineering-assurance/verification-and-calibration",
                "Verification and Calibration",
            ),
            subject("experience-and-interaction/accessibility", "Accessibility"),
            subject(
                "experience-and-interaction/application-architecture",
                "Application Architecture",
            ),
            subject(
                "experience-and-interaction/interaction-and-design-systems",
                "Interaction and Design Systems",
            ),
            subject("interfaces-and-protocols/grpc", "gRPC"),
            subject("interfaces-and-protocols/nmx", "NMX"),
            subject(
                "interfaces-and-protocols/protocol-standards",
                "Protocol Standards",
            ),
            subject("interfaces-and-protocols/redfish", "Redfish"),
            subject(
                "interfaces-and-protocols/telemetry-and-network-protocols",
                "Telemetry and Network Protocols",
            ),
            subject(
                "models-and-simulation/algorithms-and-optimization",
                "Algorithms and Optimization",
            ),
            subject(
                "models-and-simulation/battery-and-electrochemistry",
                "Battery and Electrochemistry",
            ),
            subject("models-and-simulation/physical-models", "Physical Models"),
            subject(
                "models-and-simulation/simulation-and-fidelity",
                "Simulation and Fidelity",
            ),
            subject(
                "physical-infrastructure/equipment-and-platforms",
                "Equipment and Platforms",
            ),
            subject(
                "physical-infrastructure/mechanical-systems",
                "Mechanical Systems",
            ),
            subject("physical-infrastructure/power-systems", "Power Systems"),
            subject(
                "physical-infrastructure/thermal-and-fluid-systems",
                "Thermal and Fluid Systems",
            ),
            subject(
                "systems-and-runtime/control-and-automation",
                "Control and Automation",
            ),
            subject("systems-and-runtime/runtime-behavior", "Runtime Behavior"),
            subject(
                "systems-and-runtime/system-architecture",
                "System Architecture",
            ),
        ],
        facets: vec![
            axis(
                "applicability",
                "Applicability",
                &[
                    ("component", "Component"),
                    ("facility", "Facility"),
                    ("rack", "Rack"),
                    ("simulation", "Simulation"),
                    ("software", "Software"),
                ],
            ),
            axis(
                "artifact-kind",
                "Artifact Kind",
                &[
                    ("algorithm", "Algorithm"),
                    ("architecture", "Architecture"),
                    ("catalog", "Catalog"),
                    ("model", "Model"),
                    ("protocol-contract", "Protocol Contract"),
                    ("specification", "Specification"),
                    ("user-experience", "User Experience"),
                    ("validation", "Validation"),
                ],
            ),
            axis(
                "concern",
                "Engineering Concern",
                &[
                    ("interoperability", "Interoperability"),
                    ("observability", "Observability"),
                    ("performance", "Performance"),
                    ("reliability", "Reliability"),
                    ("safety", "Safety"),
                    ("security", "Security"),
                ],
            ),
            axis(
                "lifecycle-stage",
                "Lifecycle Stage",
                &[
                    ("design", "Design"),
                    ("integration", "Integration"),
                    ("operation", "Operation"),
                    ("validation", "Validation"),
                ],
            ),
            axis(
                "system-layer",
                "System Layer",
                &[
                    ("application", "Application"),
                    ("control", "Control"),
                    ("equipment", "Equipment"),
                    ("facility", "Facility"),
                    ("management", "Management"),
                    ("platform", "Platform"),
                ],
            ),
        ],
    }
}

fn domain(id: &str, label: &str) -> Domain {
    Domain {
        id: id.into(),
        label: label.into(),
    }
}

fn subject(id: &str, label: &str) -> Subject {
    Subject {
        id: id.into(),
        label: label.into(),
    }
}

fn axis(id: &str, label: &str, terms: &[(&str, &str)]) -> FacetAxis {
    FacetAxis {
        id: id.into(),
        label: label.into(),
        terms: terms
            .iter()
            .map(|(id, label)| FacetTerm {
                id: (*id).into(),
                label: (*label).into(),
            })
            .collect(),
    }
}

/// Build canonical assignments from explicit classifications only.
pub fn produce_assignments(
    policy: &TaxonomyPolicy,
    source_revisions: &BTreeMap<String, String>,
    inputs: impl IntoIterator<Item = AssignmentInput>,
) -> Result<Assignments> {
    let mut assignments = inputs
        .into_iter()
        .map(|input| Assignment {
            source_id: input.source_id,
            content_sha256: input.content_sha256,
            primary_subject: input.primary_subject,
            facets: input
                .facets
                .into_iter()
                .map(|(axis, mut terms)| {
                    terms.sort();
                    (axis, terms)
                })
                .collect(),
            applicability: {
                let mut applicability = input.applicability;
                applicability.sort();
                applicability
            },
            page_ids: {
                let mut page_ids = input.page_ids;
                page_ids.sort();
                page_ids
            },
        })
        .collect::<Vec<_>>();
    assignments.sort_by(|left, right| left.source_id.cmp(&right.source_id));
    let assignments = Assignments {
        schema: TAXONOMY_ASSIGNMENTS_SCHEMA.into(),
        policy_sha256: taxonomy_policy_digest(policy)?,
        assignments,
    };
    validate_incremental_assignments(&assignments, policy, source_revisions)?;
    Ok(assignments)
}

pub fn parse_taxonomy_policy(bytes: &[u8]) -> Result<TaxonomyPolicy> {
    ensure!(
        bytes.len() <= MAX_DOCUMENT_BYTES,
        "taxonomy policy exceeds the {MAX_DOCUMENT_BYTES}-byte size limit"
    );
    let policy = serde_json::from_slice(bytes)?;
    validate_taxonomy_policy(&policy)?;
    Ok(policy)
}

pub fn canonical_taxonomy_policy(policy: &TaxonomyPolicy) -> Result<Vec<u8>> {
    validate_taxonomy_policy(policy)?;
    Ok(serde_json::to_vec(policy)?)
}

pub fn taxonomy_policy_digest(policy: &TaxonomyPolicy) -> Result<String> {
    let canonical = canonical_taxonomy_policy(policy)?;
    let mut digest = Sha256::new();
    digest.update(b"graphoxide-taxonomy-policy\0");
    digest.update(canonical);
    Ok(hex::encode(digest.finalize()))
}

pub fn parse_assignments(
    bytes: &[u8],
    policy: &TaxonomyPolicy,
    source_revisions: &BTreeMap<String, String>,
) -> Result<Assignments> {
    ensure!(
        bytes.len() <= MAX_DOCUMENT_BYTES,
        "taxonomy assignments exceed the {MAX_DOCUMENT_BYTES}-byte size limit"
    );
    let assignments = serde_json::from_slice(bytes)?;
    validate_incremental_assignments(&assignments, policy, source_revisions)?;
    ensure!(
        bytes == canonical_assignments(&assignments, policy, source_revisions)?,
        "taxonomy assignments are not exact canonical bytes"
    );
    Ok(assignments)
}

pub fn canonical_assignments(
    assignments: &Assignments,
    policy: &TaxonomyPolicy,
    source_revisions: &BTreeMap<String, String>,
) -> Result<Vec<u8>> {
    validate_incremental_assignments(assignments, policy, source_revisions)?;
    Ok(serde_json::to_vec(assignments)?)
}

pub fn assignments_digest(
    assignments: &Assignments,
    policy: &TaxonomyPolicy,
    source_revisions: &BTreeMap<String, String>,
) -> Result<String> {
    let canonical = canonical_assignments(assignments, policy, source_revisions)?;
    let mut digest = Sha256::new();
    digest.update(b"graphoxide-taxonomy-assignments\0");
    digest.update(canonical);
    Ok(hex::encode(digest.finalize()))
}

/// Bind a source review to that source's assignment. Other sources can be added,
/// refreshed, or retired without changing what this reviewer approved.
/// The complete assignment document still has to pass the integrity checks.
pub fn source_assignments_digest(
    assignments: &Assignments,
    policy: &TaxonomyPolicy,
    source_revisions: &BTreeMap<String, String>,
    source_id: &str,
) -> Result<String> {
    validate_incremental_assignments(assignments, policy, source_revisions)?;
    let assignment = assignments
        .assignments
        .iter()
        .find(|assignment| assignment.source_id == source_id)
        .ok_or_else(|| anyhow::anyhow!("review source has no taxonomy assignment"))?;
    let scoped = Assignments {
        schema: assignments.schema.clone(),
        policy_sha256: assignments.policy_sha256.clone(),
        assignments: vec![assignment.clone()],
    };
    assignments_digest(&scoped, policy, source_revisions)
}

pub fn parse_review_attestation(
    bytes: &[u8],
    policy: &TaxonomyPolicy,
    assignments: &Assignments,
    source_revisions: &BTreeMap<String, String>,
    derived_page_sha256: &str,
) -> Result<DirectReviewAttestation> {
    ensure!(
        bytes.len() <= MAX_DOCUMENT_BYTES,
        "review attestation exceeds the {MAX_DOCUMENT_BYTES}-byte size limit"
    );
    let attestation = serde_json::from_slice(bytes)?;
    validate_review_attestation(
        &attestation,
        policy,
        assignments,
        source_revisions,
        derived_page_sha256,
    )?;
    ensure!(
        bytes
            == canonical_review_attestation(
                &attestation,
                policy,
                assignments,
                source_revisions,
                derived_page_sha256,
            )?,
        "review attestation is not exact canonical bytes"
    );
    Ok(attestation)
}

pub fn canonical_review_attestation(
    attestation: &DirectReviewAttestation,
    policy: &TaxonomyPolicy,
    assignments: &Assignments,
    source_revisions: &BTreeMap<String, String>,
    derived_page_sha256: &str,
) -> Result<Vec<u8>> {
    validate_review_attestation(
        attestation,
        policy,
        assignments,
        source_revisions,
        derived_page_sha256,
    )?;
    Ok(serde_json::to_vec(attestation)?)
}

pub fn validate_taxonomy_policy(policy: &TaxonomyPolicy) -> Result<()> {
    ensure!(
        policy.schema == TAXONOMY_POLICY_SCHEMA,
        "taxonomy policy has an unsupported schema"
    );
    let domain_ids = unique_ids(
        policy.domains.iter().map(|domain| domain.id.as_str()),
        "domain",
    )?;
    ensure!(
        strictly_sorted_by(&policy.domains, |domain| domain.id.as_str()),
        "taxonomy policy domains must be sorted by id"
    );
    for domain in &policy.domains {
        validate_id(&domain.id, "domain")?;
        validate_label(&domain.label, "domain")?;
    }
    let mut subject_ids = BTreeSet::new();
    ensure!(
        strictly_sorted_by(&policy.subjects, |subject| subject.id.as_str()),
        "taxonomy policy subjects must be sorted by id"
    );
    for subject in &policy.subjects {
        let Some((domain, leaf)) = subject.id.split_once('/') else {
            anyhow::bail!("taxonomy subject id must be domain/subject");
        };
        ensure!(
            domain_ids.contains(domain) && !leaf.contains('/'),
            "taxonomy subject must belong to a configured domain"
        );
        validate_id(leaf, "subject")?;
        validate_label(&subject.label, "subject")?;
        ensure!(
            subject_ids.insert(&subject.id),
            "taxonomy subject ids must be unique"
        );
    }
    ensure!(
        !subject_ids.is_empty(),
        "taxonomy policy requires at least one subject"
    );
    let mut axes = BTreeSet::new();
    ensure!(
        strictly_sorted_by(&policy.facets, |axis| axis.id.as_str()),
        "taxonomy policy facet axes must be sorted by id"
    );
    for axis in &policy.facets {
        validate_id(&axis.id, "facet axis")?;
        validate_label(&axis.label, "facet axis")?;
        ensure!(
            axes.insert(&axis.id),
            "taxonomy facet axis ids must be unique"
        );
        let terms = unique_ids(axis.terms.iter().map(|term| term.id.as_str()), "facet term")?;
        ensure!(
            strictly_sorted_by(&axis.terms, |term| term.id.as_str()),
            "taxonomy policy facet terms must be sorted by id"
        );
        ensure!(
            !terms.is_empty(),
            "taxonomy facet axes require at least one term"
        );
        for term in &axis.terms {
            validate_id(&term.id, "facet term")?;
            validate_label(&term.label, "facet term")?;
        }
    }
    Ok(())
}

/// Validate assignments which may describe only a provisional subset of the
/// configured taxonomy. This is the admission gate used when sources are added.
pub fn validate_incremental_assignments(
    assignments: &Assignments,
    policy: &TaxonomyPolicy,
    source_revisions: &BTreeMap<String, String>,
) -> Result<()> {
    validate_taxonomy_policy(policy)?;
    ensure!(
        assignments.schema == TAXONOMY_ASSIGNMENTS_SCHEMA,
        "taxonomy assignments have an unsupported schema"
    );
    ensure!(
        assignments.policy_sha256 == taxonomy_policy_digest(policy)?
            && is_sha256(&assignments.policy_sha256),
        "taxonomy assignments do not match the configured policy"
    );
    ensure!(
        assignments
            .assignments
            .windows(2)
            .all(|pair| pair[0].source_id < pair[1].source_id),
        "taxonomy assignments must be sorted and unique by source id"
    );
    let subjects = policy
        .subjects
        .iter()
        .map(|subject| subject.id.as_str())
        .collect::<BTreeSet<_>>();
    let facet_terms = policy
        .facets
        .iter()
        .map(|axis| {
            (
                axis.id.as_str(),
                axis.terms
                    .iter()
                    .map(|term| term.id.as_str())
                    .collect::<BTreeSet<_>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let applicability_terms = facet_terms.get("applicability");
    let mut page_ids = BTreeSet::new();
    for assignment in &assignments.assignments {
        validate_source_id(&assignment.source_id)?;
        ensure!(
            source_revisions.get(&assignment.source_id) == Some(&assignment.content_sha256)
                && is_sha256(&assignment.content_sha256),
            "taxonomy assignment does not match its source revision"
        );
        ensure!(
            subjects.contains(assignment.primary_subject.as_str()),
            "taxonomy assignment primary subject is not configured"
        );
        for (axis, terms) in &assignment.facets {
            ensure!(
                axis != "applicability",
                "taxonomy assignment applicability must use its explicit field"
            );
            let configured = facet_terms
                .get(axis.as_str())
                .ok_or_else(|| anyhow::anyhow!("taxonomy assignment uses an unknown facet axis"))?;
            ensure!(
                !terms.is_empty()
                    && terms.windows(2).all(|pair| pair[0] < pair[1])
                    && terms.iter().all(|term| configured.contains(term.as_str())),
                "taxonomy assignment facet terms must be configured, sorted, and unique"
            );
        }
        ensure!(
            assignment
                .applicability
                .windows(2)
                .all(|pair| pair[0] < pair[1])
                && applicability_terms.is_none_or(|configured| {
                    assignment
                        .applicability
                        .iter()
                        .all(|term| configured.contains(term.as_str()))
                })
                && (applicability_terms.is_some() || assignment.applicability.is_empty()),
            "taxonomy assignment applicability must be configured, sorted, and unique"
        );
        ensure!(
            !assignment.page_ids.is_empty()
                && assignment.page_ids.windows(2).all(|pair| pair[0] < pair[1]),
            "taxonomy assignment page ids must be sorted and unique"
        );
        for page_id in &assignment.page_ids {
            validate_id(page_id, "page")?;
            ensure!(
                page_ids.insert(page_id),
                "taxonomy assignment page ids must be globally unique"
            );
        }
    }
    Ok(())
}

/// Validate that assignments are ready to render every configured navigation
/// route. Unlike incremental admission, this requires the full source corpus
/// and every configured browse term to be populated.
pub fn validate_navigation_assignments(
    assignments: &Assignments,
    policy: &TaxonomyPolicy,
    source_revisions: &BTreeMap<String, String>,
) -> Result<()> {
    validate_incremental_assignments(assignments, policy, source_revisions)?;

    let assigned = assignments
        .assignments
        .iter()
        .map(|assignment| assignment.source_id.as_str())
        .collect::<BTreeSet<_>>();
    ensure!(
        assigned == source_revisions.keys().map(String::as_str).collect(),
        "taxonomy assignments must exactly cover the current source revisions"
    );

    let mut domains = BTreeSet::new();
    let mut subjects = BTreeSet::new();
    let mut terms = BTreeSet::new();
    for assignment in &assignments.assignments {
        let (domain, _) = assignment
            .primary_subject
            .split_once('/')
            .expect("validated subject contains a domain");
        domains.insert(domain);
        subjects.insert(assignment.primary_subject.as_str());
        for (axis, values) in &assignment.facets {
            for value in values {
                terms.insert((axis.as_str(), value.as_str()));
            }
        }
        for value in &assignment.applicability {
            terms.insert(("applicability", value.as_str()));
        }
    }
    ensure!(
        domains
            == policy
                .domains
                .iter()
                .map(|domain| domain.id.as_str())
                .collect(),
        "every configured domain must have a non-empty page set"
    );
    ensure!(
        subjects
            == policy
                .subjects
                .iter()
                .map(|subject| subject.id.as_str())
                .collect(),
        "every configured subject must have a non-empty page set"
    );
    ensure!(
        policy.facets.iter().all(|axis| axis
            .terms
            .iter()
            .all(|term| terms.contains(&(axis.id.as_str(), term.id.as_str())))),
        "every configured facet term must have a non-empty page set"
    );
    Ok(())
}

fn validate_review_attestation(
    attestation: &DirectReviewAttestation,
    policy: &TaxonomyPolicy,
    assignments: &Assignments,
    source_revisions: &BTreeMap<String, String>,
    derived_page_sha256: &str,
) -> Result<()> {
    validate_incremental_assignments(assignments, policy, source_revisions)?;
    ensure!(
        attestation.schema == REVIEW_ATTESTATION_SCHEMA,
        "review attestation has an unsupported schema"
    );
    validate_source_id(&attestation.source_id)?;
    ensure!(
        source_revisions.get(&attestation.source_id) == Some(&attestation.content_sha256)
            && is_sha256(&attestation.content_sha256),
        "review attestation source revision is stale or unknown"
    );
    ensure!(
        is_sha256(derived_page_sha256)
            && attestation.derived_page_sha256 == derived_page_sha256
            && is_sha256(&attestation.derived_page_sha256),
        "review attestation derived page digest is stale or invalid"
    );
    ensure!(
        attestation.policy_sha256 == taxonomy_policy_digest(policy)?
            && is_sha256(&attestation.policy_sha256),
        "review attestation policy digest does not match"
    );
    ensure!(
        attestation.assignments_sha256
            == source_assignments_digest(
                assignments,
                policy,
                source_revisions,
                &attestation.source_id,
            )?
            && is_sha256(&attestation.assignments_sha256),
        "review attestation assignments digest does not match"
    );
    ensure!(
        assignments.assignments.iter().any(|assignment| {
            assignment.source_id == attestation.source_id
                && assignment.content_sha256 == attestation.content_sha256
        }),
        "review attestation source revision does not match assignments"
    );
    match &attestation.reviewer {
        Reviewer::Ai { model_sha256 } => ensure!(
            is_sha256(model_sha256),
            "AI reviewer model digest must be SHA-256"
        ),
        Reviewer::Human => {}
    }
    let reviewed_at =
        chrono::DateTime::parse_from_rfc3339(&attestation.reviewed_at).map_err(|_| {
            anyhow::anyhow!("review attestation timestamp must be canonical RFC 3339 UTC")
        })?;
    ensure!(
        reviewed_at.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true) == attestation.reviewed_at,
        "review attestation timestamp must be canonical RFC 3339 UTC"
    );
    Ok(())
}

fn strictly_sorted_by<T>(values: &[T], key: impl Fn(&T) -> &str) -> bool {
    values.windows(2).all(|pair| key(&pair[0]) < key(&pair[1]))
}

fn unique_ids<'a>(values: impl Iterator<Item = &'a str>, label: &str) -> Result<BTreeSet<&'a str>> {
    let mut unique = BTreeSet::new();
    for value in values {
        ensure!(unique.insert(value), "taxonomy {label} ids must be unique");
    }
    ensure!(
        !unique.is_empty(),
        "taxonomy policy requires at least one {label}"
    );
    Ok(unique)
}

fn validate_id(value: &str, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            && !value.starts_with('-')
            && !value.ends_with('-')
            && !matches!(
                value,
                "other" | "default" | "raw" | "capture" | "source-store"
            ),
        "taxonomy {label} id must be a lowercase slug"
    );
    Ok(())
}

fn validate_label(value: &str, label: &str) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= 160,
        "taxonomy {label} label must be nonempty and at most 160 bytes"
    );
    Ok(())
}

fn validate_source_id(value: &str) -> Result<()> {
    ensure!(
        value.len() == 68
            && value.starts_with("src:")
            && value[4..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "taxonomy assignment source id must be a direct source id"
    );
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
