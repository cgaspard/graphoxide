use graphoxide_core::make_id;
use graphoxide_extract::extract_project_with_options;
use std::{fs, path::Path};
use tempfile::TempDir;

fn write(root: &Path, relative: &str, source: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create static-module fixture parent");
    }
    fs::write(path, source).expect("write static-module fixture");
}

fn assert_dynamic_import_cannot_bind(
    consumer: &str,
    consumer_source: &str,
    collision: &str,
    collision_source: &str,
    dynamic_line: &str,
    static_line: &str,
) {
    let fixture = TempDir::new().expect("static-module collision fixture");
    write(fixture.path(), consumer, consumer_source);
    write(fixture.path(), collision, collision_source);
    let extractions = extract_project_with_options(fixture.path(), true)
        .expect("extract admitted static-module collision corpus");

    let collision_id = make_id(&[&Path::new(collision)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/")]);
    assert!(extractions
        .iter()
        .flat_map(|extraction| &extraction.nodes)
        .any(|node| {
            node.id == collision_id
                && node.source_file == collision
                && node.extra.get("type").and_then(|value| value.as_str()) == Some("file")
        }));

    let imports = extractions
        .iter()
        .flat_map(|extraction| &extraction.edges)
        .filter(|edge| edge.source_file == consumer && edge.relation == "imports")
        .collect::<Vec<_>>();
    assert!(imports.iter().all(|edge| {
        edge.extra
            .get("source_location")
            .and_then(|value| value.as_str())
            != Some(dynamic_line)
    }));
    assert!(imports
        .iter()
        .all(|edge| edge.true_target() != collision_id));
    assert!(imports.iter().any(|edge| {
        edge.extra
            .get("source_location")
            .and_then(|value| value.as_str())
            == Some(static_line)
    }));
}

#[test]
fn scala_expression_import_cannot_bind_an_admitted_file() {
    assert_dynamic_import_cannot_bind(
        "consumer.scala",
        "import adapter()\nimport Static.Adapter\nclass Consumer\n",
        "adapter.scala",
        "class Collision\n",
        "L1",
        "L2",
    );
}

#[test]
fn elixir_expression_use_cannot_bind_an_admitted_file() {
    assert_dynamic_import_cannot_bind(
        "consumer.ex",
        "defmodule Consumer do\n  use @adapter\n  use Static.Adapter\nend\n",
        "adapter.ex",
        "defmodule Collision do\nend\n",
        "L2",
        "L3",
    );
}

#[test]
fn julia_interpolated_using_cannot_bind_an_admitted_file() {
    assert_dynamic_import_cannot_bind(
        "consumer.jl",
        "using $mod\nusing Static.Module\n",
        "mod.jl",
        "module Collision\nend\n",
        "L1",
        "L2",
    );
}
