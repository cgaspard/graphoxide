//! Fixed, single-threaded end-to-end corpus for Registry v1 live wikis.

use graphoxide_cli::wiki_materialize::reviewable_draft_sha256;
use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Write as _,
    path::Path,
    process::Command,
};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

fn graphoxide() -> Command {
    Command::new(env!("CARGO_BIN_EXE_graphoxide"))
}

fn command(command: &mut Command) {
    let output = command.output().expect("run graphoxide");
    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (path, bytes) in entries {
        writer.start_file(path, options).expect("start ZIP entry");
        writer.write_all(bytes).expect("write ZIP entry");
    }
    writer.finish().expect("finish ZIP").into_inner()
}

fn pdf() -> Vec<u8> {
    b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Count 0 >>\nendobj\ntrailer\n<< /Root 1 0 R >>\n%%EOF\n".to_vec()
}

fn docx() -> Vec<u8> {
    zip(&[
        ("[Content_Types].xml", b"<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/></Types>"),
        ("_rels/.rels", b"<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/></Relationships>"),
        ("word/document.xml", b"<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body><w:p><w:r><w:t>DOCX fixture</w:t></w:r></w:p></w:body></w:document>"),
    ])
}

fn xlsx() -> Vec<u8> {
    zip(&[
        ("[Content_Types].xml", b"<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/></Types>"),
        ("_rels/.rels", b"<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/></Relationships>"),
        ("xl/workbook.xml", b"<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheets><sheet name=\"Fixture\" sheetId=\"1\"/></sheets></workbook>"),
    ])
}

fn commit_registry(tree: &Path) -> String {
    for arguments in [
        vec!["init"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Graphoxide Test",
            "-c",
            "user.email=graphoxide-test@example.invalid",
            "commit",
            "-m",
            "fixture",
        ],
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(tree)
            .args(arguments)
            .output()
            .expect("git fixture");
        assert!(
            output.status.success(),
            "git stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(tree)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("revision")
            .stdout,
    )
    .expect("UTF-8 revision")
    .trim()
    .into()
}

fn commit_registry_changes(tree: &Path, message: &str) -> String {
    for arguments in [
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Graphoxide Test",
            "-c",
            "user.email=graphoxide-test@example.invalid",
            "commit",
            "-m",
            message,
        ],
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(tree)
            .args(arguments)
            .output()
            .expect("git fixture update");
        assert!(
            output.status.success(),
            "git stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(tree)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("revision")
            .stdout,
    )
    .expect("UTF-8 revision")
    .trim()
    .into()
}

fn evidence_by_citation(output: &Path) -> BTreeMap<String, String> {
    fs::read_dir(output.join("references"))
        .expect("read canonical references")
        .map(|entry| fs::read_to_string(entry.expect("reference entry").path()).expect("reference"))
        .filter_map(|reference| {
            let citation = reference
                .lines()
                .find_map(|line| line.strip_prefix("- Catalog capture: `"))
                .and_then(|line| line.strip_suffix('`'))?;
            let evidence = reference
                .lines()
                .find_map(|line| line.strip_prefix("- Evidence block: `"))
                .and_then(|line| line.strip_suffix('`'))?;
            Some((citation.to_owned(), evidence.to_owned()))
        })
        .collect()
}

fn article_citations(markdown: &str) -> Vec<String> {
    let mut citations = Vec::new();
    let mut in_sources = false;
    for line in markdown.lines() {
        if line == "---" && in_sources {
            break;
        }
        if line == "sources:" {
            in_sources = true;
            continue;
        }
        if in_sources {
            let Some(value) = line.strip_prefix("  - ") else {
                break;
            };
            citations.push(serde_json::from_str(value).expect("source citation"));
        }
    }
    citations
}

fn capture_set_sha256(
    snapshot: &graphoxide_extract::registry::RegistrySnapshot,
    citations: &[String],
) -> String {
    let lines = citations
        .iter()
        .map(|citation| {
            let (source_id, capture_id) = citation.split_once('#').expect("citation");
            let capture = &snapshot.captures()[capture_id];
            format!("{source_id}\t{capture_id}\t{}\n", capture.sha256)
        })
        .collect::<std::collections::BTreeSet<_>>();
    hex::encode(Sha256::digest(
        lines.into_iter().collect::<String>().as_bytes(),
    ))
}

#[test]
fn twelve_input_registry_builds_incremental_source_ready_wiki() {
    let fixture = tempfile::tempdir().expect("fixture");
    let raw = fixture.path().join("raw");
    let tree = fixture.path().join("registry");
    let cache = fixture.path().join("cache");
    fs::create_dir_all(&raw).expect("raw root");
    let corpus: Vec<(&str, Vec<u8>)> = vec![
        ("docs/guide.md", b"# Guide\n\nDeterministic wiki fixture.\n".to_vec()),
        ("config/defaults.yaml", b"default_username: admin\ndefault_password: fake-only-password\n".to_vec()),
        ("api/openapi.json", b"{\"openapi\":\"3.1.0\",\"paths\":{\"/health\":{\"get\":{\"operationId\":\"health\"}}}}".to_vec()),
        ("schema/service.proto", b"syntax = \"proto3\"; message Fixture { string name = 1; }\n".to_vec()),
        ("diagrams/topology.dot", b"digraph fixture { source -> target; }\n".to_vec()),
        ("archives/bundle.zip", zip(&[("readme.md", b"archive fixture\n"), ("diagram.dot", b"digraph a { a -> b; }\n")])),
        ("docs/manual.pdf", pdf()),
        ("docs/manual.docx", docx()),
        ("sheets/fixture.xlsx", xlsx()),
        ("images/fixture.png", vec![137, 80, 78, 71, 13, 10, 26, 10]),
        ("opaque/fixture.unknown", vec![0, 1, 2, 3, 4]),
    ];
    for (path, bytes) in &corpus {
        let path = raw.join(path);
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(path, bytes).expect("write fixture");
    }
    let sqlite = raw.join("data/fixture.sqlite");
    fs::create_dir_all(sqlite.parent().expect("sqlite parent")).expect("create sqlite parent");
    let connection = Connection::open(&sqlite).expect("SQLite fixture");
    connection
        .execute_batch("CREATE TABLE equipment (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
        .expect("SQLite schema");
    drop(connection);

    command(
        graphoxide()
            .args(["registry", "init", "--tree"])
            .arg(&tree)
            .args(["--catalog-id", "wiki-e2e"]),
    );
    command(
        graphoxide()
            .args(["registry", "origin", "add", "--tree"])
            .arg(&tree)
            .args([
                "--origin-id",
                "fixtures",
                "--kind",
                "filesystem",
                "--logical-name",
                "synthetic-fixtures",
            ]),
    );
    command(
        graphoxide()
            .args(["registry", "origin", "bind", "--tree"])
            .arg(&tree)
            .args(["--origin-id", "fixtures", "--local-root"])
            .arg(&raw)
            .env("XDG_CACHE_HOME", &cache),
    );
    command(
        graphoxide()
            .args(["registry", "discover", "--tree"])
            .arg(&tree)
            .args(["--origin-id", "fixtures", "--accept-discovered"])
            .env("XDG_CACHE_HOME", &cache),
    );
    command(
        graphoxide()
            .args(["registry", "scan", "--tree"])
            .arg(&tree)
            .args(["--origin-id", "fixtures", "--mode", "changed"])
            .env("XDG_CACHE_HOME", &cache),
    );
    command(
        graphoxide()
            .args(["registry", "publish", "--tree"])
            .arg(&tree)
            .args([
                "--origin-id",
                "fixtures",
                "--from-local-state",
                "--observed-at",
                "2026-08-27T12:00:00Z",
            ])
            .env("XDG_CACHE_HOME", &cache),
    );
    let revision = commit_registry(&tree);
    let snapshot =
        graphoxide_extract::registry::RegistrySnapshot::load(&tree).expect("published registry");
    assert_eq!(snapshot.sources().len(), 12);
    assert_eq!(snapshot.active_captures().len(), 12);

    command(
        graphoxide()
            .arg("index")
            .arg(&raw)
            .args(["--registry"])
            .arg(&tree)
            .args([
                "--registry-origin",
                "fixtures",
                "--io-workers",
                "1",
                "--compute-workers",
                "1",
                "--progress",
                "never",
            ])
            .env("XDG_CACHE_HOME", &cache)
            .env("RAYON_NUM_THREADS", "1")
            .env("TOKIO_WORKER_THREADS", "1"),
    );
    let graph = raw.join("graphoxide-out/graph.json");
    let citation_by_path = snapshot
        .active_captures()
        .into_iter()
        .map(|capture| {
            (
                capture.source().relative_path.clone(),
                format!(
                    "{}#{}",
                    capture.source().source_id,
                    capture.capture().capture_id
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let citation = |path: &str| citation_by_path[path].clone();
    let sources = [
        ("docs/guide.md", "Operator guide", "operator-guide", "operations"),
        ("config/defaults.yaml", "Equipment default access", "equipment-default-access", "operations"),
        ("data/fixture.sqlite", "Equipment database schema", "equipment-database-schema", "operations"),
        ("api/openapi.json", "Service health API", "service-health-api", "interfaces"),
        ("schema/service.proto", "Service protocol schema", "service-protocol-schema", "interfaces"),
        ("diagrams/topology.dot", "System topology", "system-topology", "architecture"),
        ("archives/bundle.zip", "Documentation bundle", "documentation-bundle", "architecture"),
        ("docs/manual.pdf", "PDF installation manual", "pdf-installation-manual", "reference"),
        ("docs/manual.docx", "DOCX installation manual", "docx-installation-manual", "reference"),
        ("sheets/fixture.xlsx", "Equipment workbook", "equipment-workbook", "reference"),
        ("images/fixture.png", "Topology image", "topology-image", "reference"),
        ("opaque/fixture.unknown", "Opaque device artifact", "opaque-device-artifact", "reference"),
    ]
    .into_iter()
    .map(|(path, title, slug, domain)| {
        json!({"id": citation(path), "title": title, "slug": slug, "domain": domain, "coverage": "partial"})
    })
    .collect::<Vec<_>>();
    let articles = vec![
        json!({
            "id": "operations-overview", "title": "Equipment operations overview", "slug": "operations-overview", "domain": "operations", "article_type": "overview",
            "sources": [citation("docs/guide.md"), citation("config/defaults.yaml"), citation("data/fixture.sqlite")],
            "aliases": ["Operator workflow"], "related": ["equipment-defaults", "system-topology", "service-contracts"]
        }),
        json!({
            "id": "equipment-defaults", "title": "Equipment defaults and access", "slug": "equipment-defaults", "domain": "operations", "article_type": "procedure",
            "sources": [citation("config/defaults.yaml"), citation("data/fixture.sqlite")],
            "aliases": ["Default credentials"], "related": ["operations-overview"]
        }),
        json!({
            "id": "service-contracts", "title": "Service interfaces and contracts", "slug": "service-contracts", "domain": "interfaces", "article_type": "interface",
            "sources": [citation("api/openapi.json"), citation("schema/service.proto")],
            "aliases": ["API contract"], "related": ["operations-overview", "system-topology"]
        }),
        json!({
            "id": "system-topology", "title": "System topology", "slug": "system-topology", "domain": "architecture", "article_type": "component",
            "sources": [citation("diagrams/topology.dot"), citation("archives/bundle.zip")],
            "aliases": ["Topology"], "related": ["operations-overview", "service-contracts"]
        }),
        json!({
            "id": "artifact-inventory", "title": "Artifact inventory and format coverage", "slug": "artifact-inventory", "domain": "reference", "article_type": "reference",
            "sources": [citation("docs/guide.md"), citation("docs/manual.pdf"), citation("docs/manual.docx"), citation("sheets/fixture.xlsx"), citation("images/fixture.png"), citation("opaque/fixture.unknown")],
            "aliases": ["Format inventory"], "related": ["operations-overview", "system-topology"]
        }),
    ];
    let plan = fixture.path().join("plan.json");
    fs::write(
        &plan,
        serde_json::to_vec(&json!({
            "version": 1,
            "domains": [
                {"id":"operations","title":"Equipment operations","slug":"operations"},
                {"id":"interfaces","title":"Service interfaces","slug":"interfaces"},
                {"id":"architecture","title":"Architecture","slug":"architecture"},
                {"id":"reference","title":"Reference artifacts","slug":"reference"}
            ],
            "sources": sources,
            "articles": articles
        }))
        .expect("plan JSON"),
    )
    .expect("write plan");
    let output = fixture.path().join("wiki");
    let materialize = graphoxide()
        .args(["wiki", "materialize", "--registry-repo"])
        .arg(&tree)
        .args([
            "--registry-rev",
            &revision,
            "--origin",
            "fixtures",
            "--graph",
        ])
        .arg(&graph)
        .args(["--plan"])
        .arg(&plan)
        .args(["--output"])
        .arg(&output)
        .args(["--agent-jobs", "1", "--progress", "jsonl"])
        .env("XDG_CACHE_HOME", &cache)
        .output()
        .expect("materialize");
    assert!(
        materialize.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&materialize.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&materialize.stderr)
            .matches("\"event\":\"source-ready\"")
            .count(),
        12
    );
    let manifest: Value =
        serde_json::from_slice(&fs::read(output.join("wiki-manifest.json")).expect("manifest"))
            .expect("manifest JSON");
    assert_eq!(manifest["sources"].as_array().map(Vec::len), Some(12));
    assert!(manifest["sources"].as_array().is_some_and(|sources| sources
        .iter()
        .all(|source| source["state"] == "source-ready")));
    assert!(
        fs::read_to_string(output.join("sources/equipment-default-access.md"))
            .expect("source-ready page")
            .contains("publication_state: \"source-ready\"")
    );
    let index = fs::read_to_string(output.join("index.md")).expect("human navigation");
    assert!(index.contains("[Equipment operations](operations/index.md)"));
    assert!(index.contains("[Service interfaces](interfaces/index.md)"));
    assert!(index.contains("## Graph topics"));
    assert!(fs::read_to_string(output.join("llms.txt"))
        .expect("llms navigation")
        .contains("operations/operations-overview--operations-overview.md"));
    assert!(fs::read_to_string(output.join("AGENTS.md"))
        .expect("agent navigation")
        .contains("graph-topic and community pages"));
    let topics = fs::read_dir(output.join("topics"))
        .expect("graph topic pages")
        .map(|entry| {
            entry
                .expect("topic entry")
                .file_name()
                .into_string()
                .expect("UTF-8 topic name")
        })
        .collect::<Vec<_>>();
    assert!(!topics.is_empty());
    assert!(topics.iter().all(|path| !path
        .trim_end_matches(".md")
        .bytes()
        .all(|byte| byte.is_ascii_digit())));
    let communities = fs::read_dir(output.join("communities"))
        .expect("community pages")
        .map(|entry| entry.expect("community entry").path())
        .collect::<Vec<_>>();
    assert!(!communities.is_empty());
    let community_names = communities
        .iter()
        .map(|path| {
            path.file_name()
                .expect("community filename")
                .to_str()
                .expect("UTF-8 community name")
        })
        .collect::<Vec<_>>();
    assert!(community_names.iter().all(|path| !path
        .trim_end_matches(".md")
        .bytes()
        .all(|byte| byte.is_ascii_digit())));
    assert!(community_names
        .iter()
        .all(|path| !path.starts_with("community-")));
    let community_pages = communities
        .iter()
        .map(|path| fs::read_to_string(path).expect("community page text"))
        .collect::<Vec<_>>();
    assert!(community_pages
        .iter()
        .all(|page| !page.contains("# Community ")));
    assert!(fs::read_dir(output.join("references"))
        .expect("references")
        .any(|entry| {
            fs::read_to_string(entry.expect("reference entry").path())
                .expect("reference text")
                .contains("fake-only-password")
        }));

    let evidence = evidence_by_citation(&output);
    let drafts = fixture.path().join("drafts");
    let draft_specs = vec![
        (
            "operations/operations-overview--operations-overview.md",
            "operations-overview",
            vec![citation("docs/guide.md")],
            "This article groups the operator guide, equipment defaults, and database schema into one operations entry point.",
        ),
        (
            "operations/equipment-defaults--equipment-defaults.md",
            "equipment-defaults",
            vec![citation("config/defaults.yaml")],
            "The configuration evidence documents the equipment default access values and the associated data model source.",
        ),
        (
            "interfaces/service-contracts--service-contracts.md",
            "service-contracts",
            vec![citation("api/openapi.json")],
            "The API and protocol sources are organized together as the service interface contract.",
        ),
        (
            "architecture/system-topology--system-topology.md",
            "system-topology",
            vec![citation("diagrams/topology.dot")],
            "The topology evidence records the directed relationship used by the architecture navigation.",
        ),
        (
            "reference/artifact-inventory--artifact-inventory.md",
            "artifact-inventory",
            vec![citation("docs/guide.md")],
            "The inventory links document, spreadsheet, image, and opaque artifacts while preserving their extraction status.",
        ),
    ];
    for (path, _, citations, summary) in &draft_specs {
        let evidence_id = citations
            .iter()
            .find_map(|citation| evidence.get(citation))
            .unwrap_or_else(|| {
                panic!(
                    "article {path} has no retained evidence among {citations:?}; available paths: {:?}",
                    evidence
                        .keys()
                        .map(|citation| citation_by_path
                            .iter()
                            .find_map(|(path, known)| (known == citation).then_some(path)))
                        .collect::<Vec<_>>()
                )
            });
        let mut article = fs::read_to_string(output.join(path)).expect("canonical article");
        let heading = article.find("# ").expect("canonical article heading");
        let heading_end = heading + article[heading..].find('\n').expect("heading newline") + 1;
        let synthesis = format!("\n## Summary\n\n{summary}\n\nEvidence blocks: `{evidence_id}`\n");
        let marker = format!(
            "\n<!-- graphoxide-draft sha256={} -->\n{synthesis}",
            hex::encode(Sha256::digest(synthesis.as_bytes()))
        );
        article.insert_str(heading_end, &marker);
        let draft = drafts.join(path);
        fs::create_dir_all(draft.parent().expect("draft parent")).expect("draft parent");
        fs::write(draft, article).expect("write evidence-bound draft");
    }
    command(
        graphoxide()
            .args(["wiki", "materialize", "--registry-repo"])
            .arg(&tree)
            .args([
                "--registry-rev",
                &revision,
                "--origin",
                "fixtures",
                "--graph",
            ])
            .arg(&graph)
            .args(["--plan"])
            .arg(&plan)
            .args(["--output"])
            .arg(&output)
            .args(["--drafts"])
            .arg(&drafts)
            .args(["--agent-jobs", "1", "--progress", "never"])
            .env("XDG_CACHE_HOME", &cache),
    );
    let drafted_manifest: Value = serde_json::from_slice(
        &fs::read(output.join("wiki-manifest.json")).expect("drafted manifest"),
    )
    .expect("drafted manifest JSON");
    for (path, _, _, _) in &draft_specs {
        assert!(drafted_manifest["pages"]
            .as_array()
            .is_some_and(|pages| pages
                .iter()
                .any(|page| page["path"] == *path && page["state"] == "draft-ready")));
    }
    assert!(fs::read_to_string(output.join(draft_specs[0].0))
        .expect("draft-ready page")
        .contains("publication_state: \"draft-ready\""));

    let plan_sha256 = hex::encode(Sha256::digest(fs::read(&plan).expect("reviewed plan")));
    for (path, id, _citations, _) in &draft_specs {
        let input = fixture.path().join(format!("review-{id}.json"));
        let draft = fs::read(drafts.join(path)).expect("reviewed draft artifact");
        let draft = String::from_utf8(draft).expect("UTF-8 reviewed draft");
        fs::write(
            &input,
            serde_json::to_vec(&json!({
                "version": 1,
                "review_id": format!("review-{id}"),
                "decision": "approved",
                "reviewer": "fixture-reviewer",
                "reviewed_at": "2026-08-27T12:01:00Z",
                "plan_sha256": &plan_sha256,
                "capture_set_sha256": capture_set_sha256(&snapshot, &article_citations(&draft)),
                "draft_sha256": reviewable_draft_sha256(&draft)
            }))
            .expect("review JSON"),
        )
        .expect("write review input");
        command(
            graphoxide()
                .args(["registry", "review", "record", "--tree"])
                .arg(&tree)
                .args(["--input"])
                .arg(&input),
        );
    }
    let reviewed_revision = commit_registry_changes(&tree, "approve semantic fixture wiki");
    command(
        graphoxide()
            .args(["wiki", "materialize", "--registry-repo"])
            .arg(&tree)
            .args([
                "--registry-rev",
                &reviewed_revision,
                "--origin",
                "fixtures",
                "--graph",
            ])
            .arg(&graph)
            .args(["--plan"])
            .arg(&plan)
            .args(["--output"])
            .arg(&output)
            .args(["--drafts"])
            .arg(&drafts)
            .args(["--agent-jobs", "1", "--progress", "never"])
            .env("XDG_CACHE_HOME", &cache),
    );
    let reviewed_manifest: Value = serde_json::from_slice(
        &fs::read(output.join("wiki-manifest.json")).expect("reviewed manifest"),
    )
    .expect("reviewed manifest JSON");
    for (path, id, _, _) in &draft_specs {
        assert!(reviewed_manifest["pages"]
            .as_array()
            .is_some_and(|pages| pages.iter().any(|page| {
                page["path"] == *path
                    && page["state"] == "reviewed-ready"
                    && page["review_id"] == format!("review-{id}")
            })));
    }
    assert!(fs::read_to_string(output.join(draft_specs[0].0))
        .expect("reviewed-ready page")
        .contains("publication_state: \"reviewed-ready\""));

    fs::write(
        raw.join("config/defaults.yaml"),
        "default_username: admin\ndefault_password: next-fake-password\n",
    )
    .expect("change exactly one raw source");
    let changed = graphoxide()
        .args(["registry", "scan", "--tree"])
        .arg(&tree)
        .args(["--origin-id", "fixtures", "--mode", "changed", "--json"])
        .env("XDG_CACHE_HOME", &cache)
        .output()
        .expect("incremental scan");
    assert!(
        changed.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    let changed: Value = serde_json::from_slice(&changed.stdout).expect("incremental scan JSON");
    assert_eq!(changed["hashed"], 1);
    assert_eq!(changed["unchanged"], 11);
    assert_eq!(changed["queued"], 1);

}
