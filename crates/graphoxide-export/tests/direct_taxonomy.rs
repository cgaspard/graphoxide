use graphoxide_export::direct_taxonomy::{
    assignments_digest, canonical_assignments, canonical_review_attestation,
    canonical_taxonomy_policy, default_taxonomy_policy, parse_assignments,
    parse_review_attestation, parse_taxonomy_policy, produce_assignments,
    source_assignments_digest, taxonomy_policy_digest, validate_navigation_assignments, Assignment,
    AssignmentInput, Assignments, DirectReviewAttestation, Domain, FacetAxis, FacetTerm,
    ReviewDecision, Reviewer, Subject, TaxonomyPolicy,
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

#[test]
fn assignment_is_bound_to_the_source_revision_and_configured_taxonomy() {
    let policy = policy();
    let policy_sha256 = taxonomy_policy_digest(&policy).unwrap();
    let source_id = format!("src:{}", "b".repeat(64));
    let sources = BTreeMap::from([(source_id.clone(), "a".repeat(64))]);
    let assignments = Assignments {
        schema: "graphoxide.taxonomy-assignments".into(),
        policy_sha256,
        assignments: vec![Assignment {
            source_id,
            content_sha256: "a".repeat(64),
            primary_subject: "interfaces/protocols".into(),
            facets: BTreeMap::from([("layer".into(), vec!["application".into()])]),
            applicability: Vec::new(),
            page_ids: vec!["protocol-overview".into()],
        }],
    };

    let bytes = canonical_assignments(&assignments, &policy, &sources).unwrap();
    assert_eq!(
        parse_assignments(&bytes, &policy, &sources).unwrap(),
        assignments
    );
}

#[test]
fn policy_rejects_duplicate_configured_domain_ids() {
    let mut policy = policy();
    policy.domains.push(policy.domains[0].clone());

    let error = canonical_taxonomy_policy(&policy).expect_err("duplicate domain must fail");
    assert!(error.to_string().contains("domain ids must be unique"));
}

#[test]
fn policy_rejects_noncanonical_term_ordering() {
    let mut policy = policy();
    policy.domains.push(Domain {
        id: "models".into(),
        label: "Models".into(),
    });
    policy.domains.reverse();

    let error = parse_taxonomy_policy(&serde_json::to_vec(&policy).unwrap())
        .expect_err("unordered policy must fail");
    assert!(error.to_string().contains("domains must be sorted"));
}

#[test]
fn assignments_reject_a_page_id_reused_by_two_sources() {
    let policy = policy();
    let first = format!("src:{}", "a".repeat(64));
    let second = format!("src:{}", "b".repeat(64));
    let sources = BTreeMap::from([
        (first.clone(), "a".repeat(64)),
        (second.clone(), "b".repeat(64)),
    ]);
    let assignments = Assignments {
        schema: "graphoxide.taxonomy-assignments".into(),
        policy_sha256: taxonomy_policy_digest(&policy).unwrap(),
        assignments: vec![
            Assignment {
                source_id: first,
                content_sha256: "a".repeat(64),
                primary_subject: "interfaces/protocols".into(),
                facets: BTreeMap::from([("layer".into(), vec!["application".into()])]),
                applicability: Vec::new(),
                page_ids: vec!["shared-page".into()],
            },
            Assignment {
                source_id: second,
                content_sha256: "b".repeat(64),
                primary_subject: "interfaces/protocols".into(),
                facets: BTreeMap::from([("layer".into(), vec!["application".into()])]),
                applicability: Vec::new(),
                page_ids: vec!["shared-page".into()],
            },
        ],
    };

    let error = canonical_assignments(&assignments, &policy, &sources)
        .expect_err("page ids must identify one source assignment");
    assert!(error
        .to_string()
        .contains("page ids must be globally unique"));
}

#[test]
fn assignments_reject_unknown_stored_body_fields() {
    let policy = policy();
    let source_id = format!("src:{}", "b".repeat(64));
    let sources = BTreeMap::from([(source_id.clone(), "a".repeat(64))]);
    let assignments = Assignments {
        schema: "graphoxide.taxonomy-assignments".into(),
        policy_sha256: taxonomy_policy_digest(&policy).unwrap(),
        assignments: vec![Assignment {
            source_id,
            content_sha256: "a".repeat(64),
            primary_subject: "interfaces/protocols".into(),
            facets: BTreeMap::from([("layer".into(), vec!["application".into()])]),
            applicability: Vec::new(),
            page_ids: vec!["protocol-overview".into()],
        }],
    };
    let mut value: serde_json::Value =
        serde_json::from_slice(&canonical_assignments(&assignments, &policy, &sources).unwrap())
            .unwrap();
    value["assignments"][0]["body"] = serde_json::json!("forbidden");

    let error = parse_assignments(&serde_json::to_vec(&value).unwrap(), &policy, &sources)
        .expect_err("stored-body-era fields must be rejected");
    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn producer_requires_explicit_terms_and_emits_sorted_assignments() {
    let policy = TaxonomyPolicy {
        schema: "graphoxide.taxonomy-policy".into(),
        domains: vec![Domain {
            id: "interfaces".into(),
            label: "Interfaces".into(),
        }],
        subjects: vec![Subject {
            id: "interfaces/protocols".into(),
            label: "Protocols".into(),
        }],
        facets: vec![
            FacetAxis {
                id: "applicability".into(),
                label: "Applicability".into(),
                terms: vec![
                    FacetTerm {
                        id: "component".into(),
                        label: "Component".into(),
                    },
                    FacetTerm {
                        id: "facility".into(),
                        label: "Facility".into(),
                    },
                ],
            },
            FacetAxis {
                id: "system-layer".into(),
                label: "System Layer".into(),
                terms: vec![
                    FacetTerm {
                        id: "application".into(),
                        label: "Application".into(),
                    },
                    FacetTerm {
                        id: "data".into(),
                        label: "Data".into(),
                    },
                ],
            },
        ],
    };
    let first = format!("src:{}", "a".repeat(64));
    let second = format!("src:{}", "b".repeat(64));
    let source_revisions = BTreeMap::from([
        (first.clone(), "a".repeat(64)),
        (second.clone(), "b".repeat(64)),
    ]);

    let assignments = produce_assignments(
        &policy,
        &source_revisions,
        vec![
            AssignmentInput {
                source_id: second.clone(),
                content_sha256: "b".repeat(64),
                primary_subject: "interfaces/protocols".into(),
                facets: BTreeMap::from([(
                    "system-layer".into(),
                    vec!["data".into(), "application".into()],
                )]),
                applicability: vec!["facility".into(), "component".into()],
                page_ids: vec!["z-page".into(), "a-page".into()],
            },
            AssignmentInput {
                source_id: first.clone(),
                content_sha256: "a".repeat(64),
                primary_subject: "interfaces/protocols".into(),
                facets: BTreeMap::new(),
                applicability: Vec::new(),
                page_ids: vec!["first-page".into()],
            },
        ],
    )
    .unwrap();

    assert_eq!(
        assignments
            .assignments
            .iter()
            .map(|assignment| assignment.source_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.as_str(), second.as_str()]
    );
    assert_eq!(
        assignments.assignments[1].applicability,
        ["component", "facility"]
    );
    assert_eq!(
        assignments.assignments[1].facets["system-layer"],
        ["application", "data"]
    );
    assert_eq!(assignments.assignments[1].page_ids, ["a-page", "z-page"]);
}

#[test]
fn one_source_provisional_assignment_is_admissible_before_full_navigation() {
    let policy = default_taxonomy_policy();
    let source_id = format!("src:{}", "a".repeat(64));
    let source_revisions = BTreeMap::from([
        (source_id.clone(), "b".repeat(64)),
        (format!("src:{}", "c".repeat(64)), "d".repeat(64)),
    ]);

    let assignments = produce_assignments(
        &policy,
        &source_revisions,
        [AssignmentInput {
            source_id,
            content_sha256: "b".repeat(64),
            primary_subject: "interfaces-and-protocols/grpc".into(),
            facets: BTreeMap::new(),
            applicability: Vec::new(),
            page_ids: vec!["grpc-provisional".into()],
        }],
    );

    assert!(
        assignments.is_ok(),
        "a source can be indexed provisionally before the entire taxonomy is populated"
    );
}

#[test]
fn navigation_requires_every_configured_browse_term_to_have_a_page() {
    let policy = TaxonomyPolicy {
        schema: "graphoxide.taxonomy-policy".into(),
        domains: vec![
            Domain {
                id: "interfaces".into(),
                label: "Interfaces".into(),
            },
            Domain {
                id: "models".into(),
                label: "Models".into(),
            },
        ],
        subjects: vec![
            Subject {
                id: "interfaces/protocols".into(),
                label: "Protocols".into(),
            },
            Subject {
                id: "models/simulation".into(),
                label: "Simulation".into(),
            },
        ],
        facets: vec![
            FacetAxis {
                id: "applicability".into(),
                label: "Applicability".into(),
                terms: vec![
                    FacetTerm {
                        id: "component".into(),
                        label: "Component".into(),
                    },
                    FacetTerm {
                        id: "facility".into(),
                        label: "Facility".into(),
                    },
                ],
            },
            FacetAxis {
                id: "layer".into(),
                label: "Layer".into(),
                terms: vec![
                    FacetTerm {
                        id: "application".into(),
                        label: "Application".into(),
                    },
                    FacetTerm {
                        id: "data".into(),
                        label: "Data".into(),
                    },
                ],
            },
        ],
    };
    let first = format!("src:{}", "a".repeat(64));
    let second = format!("src:{}", "b".repeat(64));
    let sources = BTreeMap::from([
        (first.clone(), "a".repeat(64)),
        (second.clone(), "b".repeat(64)),
    ]);
    let assignments = produce_assignments(
        &policy,
        &sources,
        [
            AssignmentInput {
                source_id: first,
                content_sha256: "a".repeat(64),
                primary_subject: "interfaces/protocols".into(),
                facets: BTreeMap::from([("layer".into(), vec!["application".into()])]),
                applicability: vec!["component".into()],
                page_ids: vec!["protocol-overview".into()],
            },
            AssignmentInput {
                source_id: second,
                content_sha256: "b".repeat(64),
                primary_subject: "models/simulation".into(),
                facets: BTreeMap::from([("layer".into(), vec!["data".into()])]),
                applicability: vec!["facility".into()],
                page_ids: vec!["simulation-overview".into()],
            },
        ],
    )
    .unwrap();

    validate_navigation_assignments(&assignments, &policy, &sources)
        .expect("all configured browse routes have a page");

    let mut missing_term = assignments;
    missing_term.assignments[1].facets.clear();
    let error = validate_navigation_assignments(&missing_term, &policy, &sources)
        .expect_err("an empty configured browse route must not be published");
    assert!(error.to_string().contains("facet term"));
}

#[test]
fn review_attestation_binds_an_ai_decision_to_the_current_assignment() {
    let policy = policy();
    let source_id = format!("src:{}", "b".repeat(64));
    let sources = BTreeMap::from([(source_id.clone(), "a".repeat(64))]);
    let assignments = Assignments {
        schema: "graphoxide.taxonomy-assignments".into(),
        policy_sha256: taxonomy_policy_digest(&policy).unwrap(),
        assignments: vec![Assignment {
            source_id: source_id.clone(),
            content_sha256: "a".repeat(64),
            primary_subject: "interfaces/protocols".into(),
            facets: BTreeMap::from([("layer".into(), vec!["application".into()])]),
            applicability: Vec::new(),
            page_ids: vec!["protocol-overview".into()],
        }],
    };
    let attestation = DirectReviewAttestation {
        schema: "graphoxide.review-attestation".into(),
        source_id,
        content_sha256: "a".repeat(64),
        derived_page_sha256: "d".repeat(64),
        policy_sha256: taxonomy_policy_digest(&policy).unwrap(),
        assignments_sha256: assignments_digest(&assignments, &policy, &sources).unwrap(),
        reviewer: Reviewer::Ai {
            model_sha256: "c".repeat(64),
        },
        decision: ReviewDecision::Approve,
        reviewed_at: "2026-09-08T12:00:00Z".into(),
    };

    let bytes = canonical_review_attestation(
        &attestation,
        &policy,
        &assignments,
        &sources,
        &attestation.derived_page_sha256,
    )
    .unwrap();
    assert_eq!(
        parse_review_attestation(
            &bytes,
            &policy,
            &assignments,
            &sources,
            &attestation.derived_page_sha256,
        )
        .unwrap(),
        attestation
    );
}

#[test]
fn review_survives_other_source_changes_but_rejects_its_own_assignment_changes() {
    let policy = policy();
    let first = format!("src:{}", "a".repeat(64));
    let second = format!("src:{}", "b".repeat(64));
    let mut sources = BTreeMap::from([(first.clone(), "c".repeat(64))]);
    let mut assignments = Assignments {
        schema: "graphoxide.taxonomy-assignments".into(),
        policy_sha256: taxonomy_policy_digest(&policy).unwrap(),
        assignments: vec![Assignment {
            source_id: first.clone(),
            content_sha256: "c".repeat(64),
            primary_subject: "interfaces/protocols".into(),
            facets: BTreeMap::new(),
            applicability: Vec::new(),
            page_ids: vec!["first-protocol".into()],
        }],
    };
    let receipt = DirectReviewAttestation {
        schema: "graphoxide.review-attestation".into(),
        source_id: first.clone(),
        content_sha256: "c".repeat(64),
        derived_page_sha256: "d".repeat(64),
        policy_sha256: taxonomy_policy_digest(&policy).unwrap(),
        assignments_sha256: source_assignments_digest(&assignments, &policy, &sources, &first)
            .unwrap(),
        reviewer: Reviewer::Human,
        decision: ReviewDecision::Approve,
        reviewed_at: "2026-09-08T12:00:00Z".into(),
    };
    let bytes = canonical_review_attestation(
        &receipt,
        &policy,
        &assignments,
        &sources,
        &receipt.derived_page_sha256,
    )
    .unwrap();

    sources.insert(second.clone(), "e".repeat(64));
    assignments.assignments.push(Assignment {
        source_id: second.clone(),
        content_sha256: "e".repeat(64),
        primary_subject: "interfaces/protocols".into(),
        facets: BTreeMap::new(),
        applicability: Vec::new(),
        page_ids: vec!["second-protocol".into()],
    });
    parse_review_attestation(
        &bytes,
        &policy,
        &assignments,
        &sources,
        &receipt.derived_page_sha256,
    )
    .expect("adding another assignment must preserve the source review");
    sources.insert(second.clone(), "f".repeat(64));
    assignments.assignments[1].content_sha256 = "f".repeat(64);
    parse_review_attestation(
        &bytes,
        &policy,
        &assignments,
        &sources,
        &receipt.derived_page_sha256,
    )
    .expect("refreshing another assignment must preserve the source review");
    assignments.assignments.pop();
    sources.remove(&second);
    parse_review_attestation(
        &bytes,
        &policy,
        &assignments,
        &sources,
        &receipt.derived_page_sha256,
    )
    .expect("retiring another assignment must preserve the source review");

    assignments.assignments[0]
        .facets
        .insert("layer".into(), vec!["application".into()]);
    let error = parse_review_attestation(
        &bytes,
        &policy,
        &assignments,
        &sources,
        &receipt.derived_page_sha256,
    )
    .expect_err("the same source's changed assignment requires review again");
    assert!(error.to_string().contains("assignments digest"));
}

#[test]
fn source_review_digest_requires_a_valid_complete_document_and_an_assigned_source() {
    let policy = policy();
    let source = format!("src:{}", "a".repeat(64));
    let unknown = format!("src:{}", "b".repeat(64));
    let sources = BTreeMap::from([(source.clone(), "c".repeat(64))]);
    let mut assignments = Assignments {
        schema: "graphoxide.taxonomy-assignments".into(),
        policy_sha256: taxonomy_policy_digest(&policy).unwrap(),
        assignments: vec![Assignment {
            source_id: source.clone(),
            content_sha256: "c".repeat(64),
            primary_subject: "interfaces/protocols".into(),
            facets: BTreeMap::new(),
            applicability: Vec::new(),
            page_ids: vec!["protocol".into()],
        }],
    };
    assert!(source_assignments_digest(&assignments, &policy, &sources, &unknown).is_err());
    let mut invalid = assignments.assignments[0].clone();
    invalid.source_id = unknown;
    invalid.page_ids = vec!["unrelated-page".into()];
    assignments.assignments.push(invalid);
    assert!(source_assignments_digest(&assignments, &policy, &sources, &source).is_err());
}

#[test]
fn review_attestation_rejects_a_missing_derived_page_digest() {
    #[derive(serde::Serialize)]
    struct AttestationWithoutPageDigest {
        schema: String,
        source_id: String,
        content_sha256: String,
        policy_sha256: String,
        assignments_sha256: String,
        reviewer: Reviewer,
        decision: ReviewDecision,
        reviewed_at: String,
    }

    let policy = policy();
    let source_id = format!("src:{}", "b".repeat(64));
    let sources = BTreeMap::from([(source_id.clone(), "a".repeat(64))]);
    let assignments = Assignments {
        schema: "graphoxide.taxonomy-assignments".into(),
        policy_sha256: taxonomy_policy_digest(&policy).unwrap(),
        assignments: vec![Assignment {
            source_id: source_id.clone(),
            content_sha256: "a".repeat(64),
            primary_subject: "interfaces/protocols".into(),
            facets: BTreeMap::from([("layer".into(), vec!["application".into()])]),
            applicability: Vec::new(),
            page_ids: vec!["protocol-overview".into()],
        }],
    };
    let bytes = serde_json::to_vec(&AttestationWithoutPageDigest {
        schema: "graphoxide.review-attestation".into(),
        source_id,
        content_sha256: "a".repeat(64),
        policy_sha256: taxonomy_policy_digest(&policy).unwrap(),
        assignments_sha256: assignments_digest(&assignments, &policy, &sources).unwrap(),
        reviewer: Reviewer::Human,
        decision: ReviewDecision::Approve,
        reviewed_at: "2026-09-08T12:00:00Z".into(),
    })
    .unwrap();

    let error = parse_review_attestation(&bytes, &policy, &assignments, &sources, &"d".repeat(64))
        .expect_err("a review must bind the exact derived page bytes");
    assert!(error.to_string().contains("derived_page_sha256"));
}

#[test]
fn review_attestation_rejects_a_stale_derived_page_digest() {
    let policy = policy();
    let source_id = format!("src:{}", "b".repeat(64));
    let sources = BTreeMap::from([(source_id.clone(), "a".repeat(64))]);
    let assignments = Assignments {
        schema: "graphoxide.taxonomy-assignments".into(),
        policy_sha256: taxonomy_policy_digest(&policy).unwrap(),
        assignments: vec![Assignment {
            source_id: source_id.clone(),
            content_sha256: "a".repeat(64),
            primary_subject: "interfaces/protocols".into(),
            facets: BTreeMap::from([("layer".into(), vec!["application".into()])]),
            applicability: Vec::new(),
            page_ids: vec!["protocol-overview".into()],
        }],
    };
    let attestation = DirectReviewAttestation {
        schema: "graphoxide.review-attestation".into(),
        source_id,
        content_sha256: "a".repeat(64),
        derived_page_sha256: "d".repeat(64),
        policy_sha256: taxonomy_policy_digest(&policy).unwrap(),
        assignments_sha256: assignments_digest(&assignments, &policy, &sources).unwrap(),
        reviewer: Reviewer::Human,
        decision: ReviewDecision::Approve,
        reviewed_at: "2026-09-08T12:00:00Z".into(),
    };
    let bytes = canonical_review_attestation(
        &attestation,
        &policy,
        &assignments,
        &sources,
        &attestation.derived_page_sha256,
    )
    .unwrap();

    let error = parse_review_attestation(&bytes, &policy, &assignments, &sources, &"e".repeat(64))
        .expect_err("a derived page rewrite makes its prior review stale");
    assert!(error.to_string().contains("derived page digest"));
}

#[test]
fn review_attestation_rejects_a_malformed_derived_page_digest() {
    let policy = policy();
    let source_id = format!("src:{}", "b".repeat(64));
    let sources = BTreeMap::from([(source_id.clone(), "a".repeat(64))]);
    let assignments = Assignments {
        schema: "graphoxide.taxonomy-assignments".into(),
        policy_sha256: taxonomy_policy_digest(&policy).unwrap(),
        assignments: vec![Assignment {
            source_id,
            content_sha256: "a".repeat(64),
            primary_subject: "interfaces/protocols".into(),
            facets: BTreeMap::from([("layer".into(), vec!["application".into()])]),
            applicability: Vec::new(),
            page_ids: vec!["protocol-overview".into()],
        }],
    };
    let attestation = DirectReviewAttestation {
        schema: "graphoxide.review-attestation".into(),
        source_id: assignments.assignments[0].source_id.clone(),
        content_sha256: "a".repeat(64),
        derived_page_sha256: "D".repeat(64),
        policy_sha256: taxonomy_policy_digest(&policy).unwrap(),
        assignments_sha256: assignments_digest(&assignments, &policy, &sources).unwrap(),
        reviewer: Reviewer::Human,
        decision: ReviewDecision::Approve,
        reviewed_at: "2026-09-08T12:00:00Z".into(),
    };

    let error = canonical_review_attestation(
        &attestation,
        &policy,
        &assignments,
        &sources,
        &"d".repeat(64),
    )
    .expect_err("page digests are strictly lowercase SHA-256");
    assert!(error.to_string().contains("derived page digest"));
}

#[test]
fn review_attestation_rejects_a_stale_assignment_revision() {
    let policy = policy();
    let source_id = format!("src:{}", "b".repeat(64));
    let original_sources = BTreeMap::from([(source_id.clone(), "a".repeat(64))]);
    let assignments = Assignments {
        schema: "graphoxide.taxonomy-assignments".into(),
        policy_sha256: taxonomy_policy_digest(&policy).unwrap(),
        assignments: vec![Assignment {
            source_id: source_id.clone(),
            content_sha256: "a".repeat(64),
            primary_subject: "interfaces/protocols".into(),
            facets: BTreeMap::from([("layer".into(), vec!["application".into()])]),
            applicability: Vec::new(),
            page_ids: vec!["protocol-overview".into()],
        }],
    };
    let attestation = DirectReviewAttestation {
        schema: "graphoxide.review-attestation".into(),
        source_id,
        content_sha256: "a".repeat(64),
        derived_page_sha256: "d".repeat(64),
        policy_sha256: taxonomy_policy_digest(&policy).unwrap(),
        assignments_sha256: assignments_digest(&assignments, &policy, &original_sources).unwrap(),
        reviewer: Reviewer::Human,
        decision: ReviewDecision::Approve,
        reviewed_at: "2026-09-08T12:00:00Z".into(),
    };
    let bytes = canonical_review_attestation(
        &attestation,
        &policy,
        &assignments,
        &original_sources,
        &attestation.derived_page_sha256,
    )
    .unwrap();
    let stale_sources = BTreeMap::from([(attestation.source_id.clone(), "d".repeat(64))]);

    let error = parse_review_attestation(
        &bytes,
        &policy,
        &assignments,
        &stale_sources,
        &attestation.derived_page_sha256,
    )
    .expect_err("review must not promote stale assignments");
    assert!(error.to_string().contains("source revision"));
}

#[test]
fn review_attestation_rejects_an_equivalent_noncanonical_utc_timestamp() {
    let policy = policy();
    let source_id = format!("src:{}", "b".repeat(64));
    let sources = BTreeMap::from([(source_id.clone(), "a".repeat(64))]);
    let assignments = Assignments {
        schema: "graphoxide.taxonomy-assignments".into(),
        policy_sha256: taxonomy_policy_digest(&policy).unwrap(),
        assignments: vec![Assignment {
            source_id: source_id.clone(),
            content_sha256: "a".repeat(64),
            primary_subject: "interfaces/protocols".into(),
            facets: BTreeMap::from([("layer".into(), vec!["application".into()])]),
            applicability: Vec::new(),
            page_ids: vec!["protocol-overview".into()],
        }],
    };
    let attestation = DirectReviewAttestation {
        schema: "graphoxide.review-attestation".into(),
        source_id,
        content_sha256: "a".repeat(64),
        derived_page_sha256: "d".repeat(64),
        policy_sha256: taxonomy_policy_digest(&policy).unwrap(),
        assignments_sha256: assignments_digest(&assignments, &policy, &sources).unwrap(),
        reviewer: Reviewer::Human,
        decision: ReviewDecision::Approve,
        reviewed_at: "2026-09-08T12:00:00.000Z".into(),
    };

    let error = canonical_review_attestation(
        &attestation,
        &policy,
        &assignments,
        &sources,
        &attestation.derived_page_sha256,
    )
    .expect_err("equivalent timestamp spellings must not change canonical attestation bytes");
    assert!(error.to_string().contains("canonical RFC 3339 UTC"));
}

#[test]
fn default_policy_is_the_reviewed_six_domain_taxonomy() {
    let policy = default_taxonomy_policy();
    assert_eq!(
        policy
            .domains
            .iter()
            .map(|domain| (domain.id.as_str(), domain.label.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("engineering-assurance", "Engineering Assurance"),
            ("experience-and-interaction", "Experience and Interaction"),
            ("interfaces-and-protocols", "Interfaces and Protocols"),
            ("models-and-simulation", "Models and Simulation"),
            ("physical-infrastructure", "Physical Infrastructure"),
            ("systems-and-runtime", "Systems and Runtime"),
        ]
    );
    assert_eq!(
        policy
            .subjects
            .iter()
            .map(|subject| (subject.id.as_str(), subject.label.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (
                "engineering-assurance/integration-and-gaps",
                "Integration and Gaps"
            ),
            (
                "engineering-assurance/source-governance",
                "Source Governance"
            ),
            (
                "engineering-assurance/verification-and-calibration",
                "Verification and Calibration"
            ),
            ("experience-and-interaction/accessibility", "Accessibility"),
            (
                "experience-and-interaction/application-architecture",
                "Application Architecture"
            ),
            (
                "experience-and-interaction/interaction-and-design-systems",
                "Interaction and Design Systems"
            ),
            ("interfaces-and-protocols/grpc", "gRPC"),
            ("interfaces-and-protocols/nmx", "NMX"),
            (
                "interfaces-and-protocols/protocol-standards",
                "Protocol Standards"
            ),
            ("interfaces-and-protocols/redfish", "Redfish"),
            (
                "interfaces-and-protocols/telemetry-and-network-protocols",
                "Telemetry and Network Protocols"
            ),
            (
                "models-and-simulation/algorithms-and-optimization",
                "Algorithms and Optimization"
            ),
            (
                "models-and-simulation/battery-and-electrochemistry",
                "Battery and Electrochemistry"
            ),
            ("models-and-simulation/physical-models", "Physical Models"),
            (
                "models-and-simulation/simulation-and-fidelity",
                "Simulation and Fidelity"
            ),
            (
                "physical-infrastructure/equipment-and-platforms",
                "Equipment and Platforms"
            ),
            (
                "physical-infrastructure/mechanical-systems",
                "Mechanical Systems"
            ),
            ("physical-infrastructure/power-systems", "Power Systems"),
            (
                "physical-infrastructure/thermal-and-fluid-systems",
                "Thermal and Fluid Systems"
            ),
            (
                "systems-and-runtime/control-and-automation",
                "Control and Automation"
            ),
            ("systems-and-runtime/runtime-behavior", "Runtime Behavior"),
            (
                "systems-and-runtime/system-architecture",
                "System Architecture"
            ),
        ]
    );
    assert_eq!(
        policy
            .facets
            .iter()
            .map(|axis| (axis.id.as_str(), axis.label.as_str(), axis.terms.len()))
            .collect::<Vec<_>>(),
        vec![
            ("applicability", "Applicability", 5),
            ("artifact-kind", "Artifact Kind", 8),
            ("concern", "Engineering Concern", 6),
            ("lifecycle-stage", "Lifecycle Stage", 4),
            ("system-layer", "System Layer", 6),
        ]
    );

    let canonical = canonical_taxonomy_policy(&policy).unwrap();
    assert_eq!(parse_taxonomy_policy(&canonical).unwrap(), policy);
    assert_eq!(canonical, canonical_taxonomy_policy(&policy).unwrap());
    assert_eq!(
        taxonomy_policy_digest(&policy).unwrap(),
        "ad51da81398c053ae413c241d1cc36296f4e491a3a4c4974ca13a5b2072b646a"
    );
}
