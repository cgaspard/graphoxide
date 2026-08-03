//! Executable port of upstream `tests/test_replace_or_append_section.py`.

use graphoxide_cli::install::replace_or_append_section;

const MARKER: &str = "## graphoxide";
const NEW: &str = "## graphoxide\n\nThis project has a knowledge graph at graphoxide-out/.\n";

#[test]
fn test_inline_reference_to_marker_is_not_treated_as_the_section() {
    let before = concat!(
        "# My Project\n\n",
        "## Setup\n",
        "- See the `## graphoxide` section for graph usage.\n\n",
        "## Release Process\n",
        "Critical steps that must not be lost.\n",
    );
    let after = replace_or_append_section(before, MARKER, NEW);
    assert!(after.contains("See the `## graphoxide` section"));
    assert!(after.contains("Critical steps that must not be lost"));
    assert!(after.contains("knowledge graph at graphoxide-out/"));
}

#[test]
fn test_real_section_is_replaced_in_place() {
    let before = concat!(
        "# P\n\n## Setup\n- do things\n\n",
        "## graphoxide\n\nOLD text.\n\n",
        "## Release\nkeep me\n",
    );
    let after = replace_or_append_section(before, MARKER, NEW);
    assert!(!after.contains("OLD text."));
    assert!(after.contains("knowledge graph at graphoxide-out/"));
    assert!(after.contains("do things"));
    assert!(after.contains("keep me"));
    assert!(after.find("knowledge graph").unwrap() < after.find("## Release").unwrap());
}

#[test]
fn test_reinstall_is_idempotent() {
    let once = replace_or_append_section("# P\n\n## Setup\n- x\n", MARKER, NEW);
    let twice = replace_or_append_section(&once, MARKER, NEW);
    assert_eq!(once.lines().filter(|line| *line == MARKER).count(), 1);
    assert_eq!(twice.lines().filter(|line| *line == MARKER).count(), 1);
    assert_eq!(once, twice);
}

#[test]
fn test_append_when_no_real_heading() {
    let before = "# P\n\n## Setup\n- x\n";
    let after = replace_or_append_section(before, MARKER, NEW);
    assert!(after.contains("- x"));
    assert_eq!(after.lines().filter(|line| *line == MARKER).count(), 1);
}

#[test]
fn test_prefers_last_heading_when_duplicated() {
    let before = concat!(
        "## graphoxide\nstale early copy\n\n",
        "## Other\nmid\n\n",
        "## graphoxide\nreal trailing copy\n",
    );
    let after = replace_or_append_section(before, MARKER, NEW);
    assert!(after.contains("stale early copy"));
    assert!(after.contains("mid"));
    assert!(!after.contains("real trailing copy"));
    assert!(after.contains("knowledge graph at graphoxide-out/"));
}
