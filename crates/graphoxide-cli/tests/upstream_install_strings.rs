//! Executable port of upstream `tests/test_install_strings.py`.

use graphoxide_cli::install::{install_instruction_surfaces, skill_registration};
use std::{fs, path::PathBuf};

#[test]
fn test_every_install_surface_recommends_graphify_query() {
    let missing: Vec<_> = install_instruction_surfaces()
        .into_iter()
        .filter_map(|(name, text)| (!text.contains("graphoxide query")).then_some(name))
        .collect();
    assert!(
        missing.is_empty(),
        "surfaces missing graphoxide query: {missing:?}"
    );
}

#[test]
fn test_no_install_surface_demands_reading_the_full_report_first() {
    let mut hits = Vec::new();
    for (name, text) in install_instruction_surfaces() {
        for line in text.lines() {
            let lower = line.to_ascii_lowercase();
            let report = lower.find("graph_report.md");
            let read = lower.find("read");
            let before = lower.find("before");
            let bad_read_before =
                matches!((read, report, before), (Some(r), Some(g), Some(b)) if r < g && g < b);
            if lower.contains("always read") && report.is_some()
                || lower.contains("first tool call") && report.is_some()
                || bad_read_before
            {
                hits.push((name, line.to_owned()));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "report-first phrasing reappeared: {hits:?}"
    );
}

#[test]
fn test_report_is_still_referenced_as_fallback() {
    for (name, text) in install_instruction_surfaces()
        .into_iter()
        .filter(|(name, _)| {
            matches!(
                *name,
                "agents-section"
                    | "project-section"
                    | "cursor-rule"
                    | "antigravity-rule"
                    | "antigravity-workflow"
            )
        })
    {
        assert!(
            text.contains("GRAPH_REPORT.md"),
            "{name} lost report fallback"
        );
    }
}

#[test]
fn test_agents_section_does_not_skip_dirty_graph_output() {
    let agents = install_instruction_surfaces()
        .into_iter()
        .find(|(name, _)| *name == "agents-section")
        .unwrap()
        .1;
    assert!(agents.contains("Dirty graphoxide-out/ files are expected"));
    assert!(agents.contains("not a reason to skip graphoxide"));
}

#[test]
fn test_agents_section_uses_generic_graphify_instruction() {
    let agents = install_instruction_surfaces()
        .into_iter()
        .find(|(name, _)| *name == "agents-section")
        .unwrap()
        .1;
    assert!(!agents.contains("`skill` tool"));
    assert!(!agents.contains("skill: \"graphoxide\""));
    assert!(agents.contains("installed Graphoxide skill or instructions"));
}

#[test]
fn test_skill_registration_uses_host_generic_instruction() {
    let registration = skill_registration();
    assert!(!registration.contains("skill: \"graphoxide\""));
    assert!(!registration.contains("Skill tool"));
    assert!(registration.contains("installed Graphoxide skill or instructions"));
}

#[test]
fn test_how_it_works_clarifies_code_only_semantic_extraction() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/how-it-works.md");
    let document = fs::read_to_string(path).unwrap();
    assert!(document.contains("Code files are not sent to the LLM semantic extractor"));
    assert!(document.contains("code files, Pass 3 is skipped entirely"));
    assert!(document.contains("docs, papers, images, and transcripts"));
}
