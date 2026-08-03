use graphoxide_core::validate::validate_extraction;
use graphoxide_extract::pg_introspect::{
    introspect_postgres_with_source, PgCatalog, PgCatalogSourceError, PgForeignKey,
    PgIntrospectionError, PgRoutine, PgTable, PgView, PostgresCatalogSource, FOREIGN_KEY_QUERY,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone)]
struct StaticSource(PgCatalog);

impl PostgresCatalogSource for StaticSource {
    fn load_catalog(&self, _dsn: Option<&str>) -> Result<PgCatalog, PgCatalogSourceError> {
        Ok(self.0.clone())
    }
}

#[derive(Clone)]
struct FailingSource(PgCatalogSourceError);

impl PostgresCatalogSource for FailingSource {
    fn load_catalog(&self, _dsn: Option<&str>) -> Result<PgCatalog, PgCatalogSourceError> {
        Err(self.0.clone())
    }
}

fn table(name: &str) -> PgTable {
    PgTable {
        schema: "public".into(),
        name: name.into(),
        table_type: "BASE TABLE".into(),
    }
}

fn foreign_key(name: &str, source: &str, target: &str) -> PgForeignKey {
    PgForeignKey {
        constraint_name: name.into(),
        table_schema: "public".into(),
        table_name: source.into(),
        columns: vec!["target_id".into()],
        foreign_schema: "public".into(),
        foreign_table: target.into(),
        foreign_columns: vec!["id".into()],
    }
}

fn quoted(schema: &str, name: &str) -> String {
    format!(
        r#""{}"."{}""#,
        schema.replace('"', "\"\""),
        name.replace('"', "\"\"")
    )
}

fn base_catalog() -> PgCatalog {
    PgCatalog {
        host: "myhost".into(),
        dbname: "mydb".into(),
        ..PgCatalog::default()
    }
}

#[test]
fn test_pg_introspect_success() {
    let mut catalog = base_catalog();
    catalog.tables = vec![table("users"), table("orders")];
    catalog.views = vec![PgView {
        schema: "public".into(),
        name: "active_users".into(),
        definition: Some("SELECT * FROM public.users WHERE active = true".into()),
    }];
    catalog.routines = vec![
        PgRoutine {
            schema: "public".into(),
            name: "calculate_total".into(),
            routine_type: "FUNCTION".into(),
            definition: Some("SELECT 42;".into()),
            external_language: Some("SQL".into()),
        },
        PgRoutine {
            schema: "public".into(),
            name: "do_nothing".into(),
            routine_type: "PROCEDURE".into(),
            definition: None,
            external_language: Some("PLPGSQL".into()),
        },
    ];
    catalog.foreign_keys = vec![foreign_key("fk_orders_user_id", "orders", "users")];

    let result = introspect_postgres_with_source(
        Some("postgresql://myuser:mypassword@myhost/mydb"),
        &StaticSource(catalog),
    )
    .unwrap();
    validate_extraction(&result).unwrap();
    assert!(result
        .nodes
        .iter()
        .all(|node| node.source_file == "postgresql:/myhost/mydb"));
    assert!(result
        .edges
        .iter()
        .all(|edge| edge.source_file == "postgresql:/myhost/mydb"));

    let labels = result
        .nodes
        .iter()
        .map(|node| node.label.as_str())
        .collect::<BTreeSet<_>>();
    assert!(labels.contains(quoted("public", "users").as_str()));
    assert!(labels.contains(quoted("public", "orders").as_str()));
    assert!(labels.contains(quoted("public", "active_users").as_str()));
    assert!(labels.contains(format!("{}()", quoted("public", "calculate_total")).as_str()));
    assert!(labels.contains(format!("{}()", quoted("public", "do_nothing")).as_str()));
    assert_eq!(
        result
            .nodes
            .iter()
            .filter(|node| node.label == "mydb")
            .count(),
        1
    );

    let ids = result
        .nodes
        .iter()
        .map(|node| (node.label.as_str(), node.id.as_str()))
        .collect::<BTreeMap<_, _>>();
    let users = ids[quoted("public", "users").as_str()];
    let orders = ids[quoted("public", "orders").as_str()];
    assert_eq!(
        result
            .edges
            .iter()
            .filter(|edge| {
                edge.source == orders && edge.target == users && edge.relation == "references"
            })
            .count(),
        1
    );
}

#[test]
fn test_pg_introspect_quoted_identifiers() {
    let mut catalog = base_catalog();
    catalog.tables = vec![table("order"), table("user-data")];
    catalog.foreign_keys = vec![foreign_key("fk_userdata_order", "user-data", "order")];
    let result = introspect_postgres_with_source(
        Some("postgresql://myuser:secret@myhost/mydb"),
        &StaticSource(catalog),
    )
    .unwrap();
    validate_extraction(&result).unwrap();
    let labels = result
        .nodes
        .iter()
        .map(|node| node.label.as_str())
        .collect::<BTreeSet<_>>();
    assert!(labels.contains(quoted("public", "order").as_str()));
    assert!(labels.contains(quoted("public", "user-data").as_str()));
    assert_eq!(
        result
            .edges
            .iter()
            .filter(|edge| edge.relation == "references")
            .count(),
        1
    );
}

#[test]
fn test_pg_introspect_composite_fk() {
    let mut catalog = base_catalog();
    catalog.tables = vec![table("products"), table("order_items")];
    catalog.foreign_keys = vec![PgForeignKey {
        constraint_name: "fk_order_items_composite".into(),
        table_schema: "public".into(),
        table_name: "order_items".into(),
        columns: vec!["order_id".into(), "product_id".into()],
        foreign_schema: "public".into(),
        foreign_table: "products".into(),
        foreign_columns: vec!["order_id".into(), "product_id".into()],
    }];
    let result = introspect_postgres_with_source(None, &StaticSource(catalog)).unwrap();
    validate_extraction(&result).unwrap();
    assert_eq!(
        result
            .edges
            .iter()
            .filter(|edge| edge.relation == "references")
            .count(),
        1
    );
}

#[test]
fn test_pg_introspect_fk_query_avoids_privilege_filtered_view() {
    let query = FOREIGN_KEY_QUERY.to_ascii_lowercase();
    assert!(query.contains("pg_catalog.pg_constraint"));
    assert!(!query.contains("information_schema.referential_constraints"));

    let mut catalog = base_catalog();
    catalog.tables = vec![table("users"), table("orders")];
    catalog.foreign_keys = vec![foreign_key("fk_orders_user_id", "orders", "users")];
    let result = introspect_postgres_with_source(
        Some("postgresql://readonly:secret@myhost/mydb"),
        &StaticSource(catalog),
    )
    .unwrap();
    assert_eq!(
        result
            .edges
            .iter()
            .filter(|edge| edge.relation == "references")
            .count(),
        1
    );
}

#[test]
fn test_pg_introspect_fk_edges_survive_unparseable_function_stubs() {
    let mut catalog = base_catalog();
    catalog.tables = (0..=6).map(|index| table(&format!("t{index}"))).collect();
    catalog.routines = vec![
        PgRoutine {
            schema: "public".into(),
            name: "levenshtein".into(),
            routine_type: "FUNCTION".into(),
            definition: Some("levenshtein".into()),
            external_language: Some("C".into()),
        },
        PgRoutine {
            schema: "public".into(),
            name: "trigfunc".into(),
            routine_type: "FUNCTION".into(),
            definition: Some("BEGIN SELECT 1; END;".into()),
            external_language: Some("PLPGSQL".into()),
        },
    ];
    catalog.foreign_keys = (0..6)
        .map(|index| {
            foreign_key(
                &format!("fk{index}"),
                &format!("t{index}"),
                &format!("t{}", index + 1),
            )
        })
        .collect();
    let result = introspect_postgres_with_source(None, &StaticSource(catalog)).unwrap();
    validate_extraction(&result).unwrap();
    let labels = result
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node.label.as_str()))
        .collect::<BTreeMap<_, _>>();
    let actual = result
        .edges
        .iter()
        .filter(|edge| edge.relation == "references")
        .map(|edge| (labels[edge.true_source()], labels[edge.true_target()]))
        .collect::<BTreeSet<_>>();
    let expected = (0..6)
        .map(|index| {
            (
                quoted("public", &format!("t{index}")),
                quoted("public", &format!("t{}", index + 1)),
            )
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        actual,
        expected
            .iter()
            .map(|(source, target)| (source.as_str(), target.as_str()))
            .collect()
    );
}

#[test]
fn test_pg_introspect_connection_error() {
    let raw = "connection using postgresql://myuser:secret@myhost/mydb failed: authentication error\nDETAIL: private diagnostics";
    let error = introspect_postgres_with_source(
        Some("postgresql://myuser:secret@myhost/mydb"),
        &FailingSource(PgCatalogSourceError::Connection(raw.into())),
    )
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("could not connect to PostgreSQL"));
    assert!(!message.contains("secret"));
    assert!(!message.contains("DETAIL"));
}

#[test]
fn test_pg_introspect_import_error() {
    let error = introspect_postgres_with_source(
        Some("postgresql://localhost/db"),
        &FailingSource(PgCatalogSourceError::DriverUnavailable(
            "install a Graphoxide build with PostgreSQL support".into(),
        )),
    )
    .unwrap_err();
    assert!(matches!(error, PgIntrospectionError::DriverUnavailable(_)));
    assert!(error.to_string().contains("PostgreSQL support"));
}

#[test]
fn test_pg_introspect_uri_forward_slashes() {
    let catalog = PgCatalog {
        host: "some-host".into(),
        dbname: "some-db".into(),
        ..PgCatalog::default()
    };
    let result = introspect_postgres_with_source(
        Some("postgresql://some-host/some-db"),
        &StaticSource(catalog),
    )
    .unwrap();
    assert!(!result.nodes.is_empty());
    assert!(result.nodes.iter().all(|node| {
        !node.source_file.contains('\\')
            && node.source_file.contains("postgresql:/some-host/some-db")
    }));
}

#[test]
fn pg_introspect_deduplicates_rows_and_rejects_ambiguous_case_folded_endpoints() {
    let mut catalog = base_catalog();
    catalog.tables = vec![table("target"), table("target"), table("Foo"), table("foo")];
    catalog.foreign_keys = vec![
        foreign_key("duplicate", "target", "target"),
        foreign_key("duplicate", "target", "target"),
        foreign_key("ambiguous", "target", "Foo"),
    ];
    let result = introspect_postgres_with_source(None, &StaticSource(catalog)).unwrap();
    validate_extraction(&result).unwrap();
    assert_eq!(
        result
            .nodes
            .iter()
            .filter(|node| node.label == quoted("public", "target"))
            .count(),
        1
    );
    assert_eq!(
        result
            .edges
            .iter()
            .filter(|edge| edge.relation == "references")
            .count(),
        1,
        "duplicate constraints collapse and an ambiguous case-folded target is skipped"
    );
}

#[test]
fn pg_introspect_escapes_embedded_quotes_and_skips_malformed_foreign_keys() {
    let mut catalog = base_catalog();
    catalog.tables = vec![table("odd\"name"), table("target")];
    catalog.foreign_keys = vec![
        foreign_key("missing", "not-present", "target"),
        PgForeignKey {
            constraint_name: "mismatched".into(),
            table_schema: "public".into(),
            table_name: "odd\"name".into(),
            columns: vec!["a".into(), "b".into()],
            foreign_schema: "public".into(),
            foreign_table: "target".into(),
            foreign_columns: vec!["id".into()],
        },
    ];
    let result = introspect_postgres_with_source(None, &StaticSource(catalog)).unwrap();
    validate_extraction(&result).unwrap();
    assert!(result
        .nodes
        .iter()
        .any(|node| node.label == r#""public"."odd""name""#));
    assert!(result
        .edges
        .iter()
        .all(|edge| edge.relation != "references"));
}
