use graphoxide_export::{
    load_wiki_plan, parse_wiki_plan, WikiPlan, WikiPlanPathKind, MAX_WIKI_PLAN_ARTICLES,
    MAX_WIKI_PLAN_DOMAINS, MAX_WIKI_PLAN_SOURCES,
};
use serde_json::{json, Value};
use std::collections::BTreeSet;

fn valid_plan() -> Value {
    json!({
        "version": 1,
        "domains": [
            {"id": "architecture", "title": "Architecture", "slug": "architecture"},
            {"id": "operations", "title": "Operations", "slug": "operations"}
        ],
        "sources": [
            {
                "id": "source-one#capture-one",
                "title": "Primary design",
                "slug": "primary-design",
                "domain": "architecture",
                "coverage": "complete"
            },
            {
                "id": "source-two#capture-two",
                "title": "Operations notes",
                "slug": "operations-notes",
                "domain": "operations",
                "coverage": "partial"
            }
        ],
        "articles": [
            {
                "id": "system-overview",
                "title": "System overview",
                "slug": "system-overview",
                "domain": "architecture",
                "article_type": "overview",
                "sources": ["source-one#capture-one"],
                "aliases": ["Architecture overview"],
                "related": ["deployment"]
            },
            {
                "id": "deployment",
                "title": "Deployment",
                "slug": "deployment",
                "domain": "operations",
                "article_type": "procedure",
                "sources": ["source-two#capture-two"],
                "aliases": [],
                "related": ["system-overview"]
            }
        ]
    })
}

fn catalog() -> BTreeSet<String> {
    BTreeSet::from([
        "source-one#capture-one".into(),
        "source-two#capture-two".into(),
    ])
}

fn load(value: &Value) -> anyhow::Result<WikiPlan> {
    load_wiki_plan(&serde_json::to_vec(value).unwrap(), &catalog())
}

#[test]
fn rejects_malformed_and_unknown_fields() {
    let mut malformed = valid_plan();
    malformed["domains"] = json!("not-an-array");
    assert!(parse_wiki_plan(&serde_json::to_vec(&malformed).unwrap()).is_err());

    for (section, index) in [("domains", 0), ("sources", 0), ("articles", 0)] {
        let mut value = valid_plan();
        value[section][index]["surprise"] = json!(true);
        assert!(parse_wiki_plan(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    let mut root = valid_plan();
    root["surprise"] = json!(true);
    assert!(parse_wiki_plan(&serde_json::to_vec(&root).unwrap()).is_err());
}

#[test]
fn rejects_noncanonical_version() {
    let mut value = valid_plan();
    value["version"] = json!(2);
    assert!(parse_wiki_plan(&serde_json::to_vec(&value).unwrap()).is_err());
}

#[test]
fn rejects_unsafe_identifiers_slugs_and_paths() {
    for (pointer, unsafe_value) in [
        ("/domains/0/id", "../architecture"),
        ("/domains/0/slug", "../architecture"),
        ("/sources/0/id", "source-one/capture-one"),
        ("/sources/0/slug", "primary/design"),
        ("/articles/0/id", "system/overview"),
        ("/articles/0/slug", "system/overview"),
    ] {
        let mut value = valid_plan();
        *value.pointer_mut(pointer).unwrap() = json!(unsafe_value);
        assert!(load(&value).is_err(), "accepted {pointer}={unsafe_value:?}");
    }

    assert!(WikiPlan::canonical_support_path(WikiPlanPathKind::Source, "../escape").is_err());
    assert!(WikiPlan::canonical_support_path(WikiPlanPathKind::Reference, "a/b").is_err());
    assert!(WikiPlan::canonical_support_path(WikiPlanPathKind::Inventory, "").is_err());
}

#[test]
fn rejects_more_than_the_domain_bound() {
    let mut value = valid_plan();
    value["domains"] = Value::Array(
        (0..=MAX_WIKI_PLAN_DOMAINS)
            .map(|index| {
                json!({
                    "id": format!("domain-{index}"),
                    "title": format!("Domain {index}"),
                    "slug": format!("domain-{index}")
                })
            })
            .collect(),
    );
    assert!(load(&value).is_err());
}

#[test]
fn rejects_more_than_the_source_or_article_bound() {
    let mut sources = valid_plan();
    let source = sources["sources"][0].clone();
    sources["sources"] = Value::Array(
        std::iter::repeat_with(|| source.clone())
            .take(MAX_WIKI_PLAN_SOURCES + 1)
            .collect(),
    );
    assert!(load(&sources).is_err());

    let mut articles = valid_plan();
    let article = articles["articles"][0].clone();
    articles["articles"] = Value::Array(
        std::iter::repeat_with(|| article.clone())
            .take(MAX_WIKI_PLAN_ARTICLES + 1)
            .collect(),
    );
    assert!(load(&articles).is_err());
}

#[test]
fn rejects_duplicate_domains_sources_and_articles() {
    for section in ["domains", "sources", "articles"] {
        let mut value = valid_plan();
        let duplicate = value[section][0].clone();
        value[section].as_array_mut().unwrap().push(duplicate);
        assert!(load(&value).is_err(), "accepted duplicate {section}");
    }
}

#[test]
fn rejects_dangling_article_domain_source_and_related_ids() {
    for (pointer, dangling) in [
        ("/articles/0/domain", "missing-domain"),
        ("/articles/0/sources/0", "missing#capture"),
        ("/articles/0/related/0", "missing-article"),
    ] {
        let mut value = valid_plan();
        *value.pointer_mut(pointer).unwrap() = json!(dangling);
        assert!(load(&value).is_err(), "accepted {pointer}={dangling:?}");
    }
}

#[test]
fn rejects_article_citation_missing_from_catalog() {
    let mut catalog = catalog();
    catalog.remove("source-one#capture-one");
    assert!(load_wiki_plan(&serde_json::to_vec(&valid_plan()).unwrap(), &catalog).is_err());
}

#[test]
fn rejects_empty_article_citations_and_invalid_article_type() {
    let mut empty = valid_plan();
    empty["articles"][0]["sources"] = json!([]);
    assert!(load(&empty).is_err());

    let mut invalid_type = valid_plan();
    invalid_type["articles"][0]["article_type"] = json!("tutorial");
    assert!(parse_wiki_plan(&serde_json::to_vec(&invalid_type).unwrap()).is_err());
}

#[test]
fn rejects_duplicate_emitted_article_path() {
    let mut value = valid_plan();
    value["articles"].as_array_mut().unwrap().push(json!({
        "id": "system-overview",
        "title": "Colliding overview",
        "slug": "system-overview",
        "domain": "architecture",
        "article_type": "concept",
        "sources": ["source-one#capture-one"],
        "aliases": [],
        "related": []
    }));
    value["articles"][2]["id"] = json!("system-overview");
    assert!(load(&value)
        .unwrap_err()
        .to_string()
        .contains("duplicate emitted article path"));
}

#[test]
fn valid_plan_has_deterministic_round_trip_and_canonical_paths() {
    let bytes = serde_json::to_vec(&valid_plan()).unwrap();
    let plan = load_wiki_plan(&bytes, &catalog()).unwrap();
    let encoded = serde_json::to_vec(&plan).unwrap();
    assert_eq!(
        encoded,
        serde_json::to_vec(&parse_wiki_plan(&encoded).unwrap()).unwrap()
    );
    assert_eq!(
        plan.article_path("system-overview").unwrap(),
        "architecture/system-overview--system-overview.md"
    );
    assert_eq!(
        plan.source_path("source-one#capture-one").unwrap(),
        "sources/primary-design.md"
    );
    assert_eq!(
        WikiPlan::canonical_support_path(WikiPlanPathKind::Reference, "api").unwrap(),
        "references/api.md"
    );
    assert_eq!(
        WikiPlan::canonical_support_path(WikiPlanPathKind::Inventory, "uncovered").unwrap(),
        "inventory/uncovered.md"
    );
}
