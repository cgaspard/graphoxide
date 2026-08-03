use graphoxide_skillgen::{
    always_on_baseline, always_on_constant, always_on_roundtrip, audit_artifacts, audit_coverage,
    check, check_on_disk, consolidation_allowlist, coverage_baseline_ref, git_dependent_validation,
    headings, legacy_enum_lines, monolith_roundtrip, platforms, render, render_all,
    render_always_on, sanctioned_always_on_edits, schema_singleton, write_artifacts, Bucket,
    GitValidation, HooksVariant, RenderedArtifact, ENUM_PROSE, ENUM_VALUES,
    GRAPHIFY_V8_BASELINE_SHA, NEW_AGENTS_INSTRUCTION, OLD_AGENTS_INSTRUCTION,
    SHARED_INTRO_ALLOWLIST, UNIFIED_DESCRIPTION,
};
use std::collections::{BTreeMap, BTreeSet};

const REFERENCE_NAMES: [&str; 8] = [
    "add-watch.md",
    "exports.md",
    "extraction-spec.md",
    "github-and-merge.md",
    "hooks.md",
    "query.md",
    "transcribe.md",
    "update.md",
];

const PROGRESSIVE_HOSTS: [&str; 10] = [
    "opencode", "kilo", "copilot", "claw", "droid", "amp", "trae", "kiro", "pi", "vscode",
];

fn platform(key: &str) -> graphoxide_skillgen::Platform {
    platforms().get(key).unwrap().clone()
}

fn artifacts(key: &str) -> Vec<RenderedArtifact> {
    render(&platform(key))
}

fn split_artifacts(key: &str) -> (String, BTreeMap<String, String>) {
    let platform = platform(key);
    let rendered = render(&platform);
    let core = rendered
        .iter()
        .find(|artifact| artifact.path == platform.skill_dst)
        .unwrap()
        .content
        .clone();
    let references = rendered
        .into_iter()
        .filter(|artifact| artifact.path != platform.skill_dst)
        .map(|artifact| {
            (
                artifact.path.rsplit('/').next().unwrap().to_owned(),
                artifact.content,
            )
        })
        .collect();
    (core, references)
}

fn frontmatter(body: &str) -> &str {
    body.split("---").nth(1).unwrap()
}

fn dispatch_block(body: &str) -> &str {
    let start = body.find("**Step B2").unwrap();
    let end = body[start..].find("**Step B3").unwrap() + start;
    &body[start..end]
}

fn reference_names(references: &BTreeMap<String, String>) -> Vec<String> {
    references.keys().cloned().collect()
}

#[test]
fn test_audit_coverage_passes() {
    assert!(audit_coverage(&platform("claude")).is_empty());
}

#[test]
fn test_check_passes() {
    let rendered = artifacts("claude");
    assert!(check(&rendered).is_empty());

    let directory = tempfile::tempdir().unwrap();
    write_artifacts(directory.path(), &rendered).unwrap();
    assert!(check_on_disk(directory.path(), &rendered).is_empty());
}

#[test]
fn test_render_is_idempotent() {
    assert_eq!(artifacts("claude"), artifacts("claude"));
}

#[test]
fn test_render_output_is_lf_only() {
    for artifact in artifacts("claude") {
        assert!(!artifact.content.contains('\r'), "{}", artifact.path);
        assert!(artifact.content.ends_with('\n'), "{}", artifact.path);
        assert!(!artifact.content.ends_with("\n\n"), "{}", artifact.path);
    }
}

#[test]
fn test_no_version_or_timestamp_in_output() {
    for artifact in artifacts("claude") {
        assert!(!artifact.content.contains(env!("CARGO_PKG_VERSION")));
        assert!(!artifact.content.contains("Generated at"));
    }
}

#[test]
fn test_lean_core_has_no_reference_only_content() {
    let (core, _) = split_artifacts("claude");
    for marker in [
        ENUM_VALUES,
        "graphoxide cluster-only INPUT_PATH",
        "## Constrained query expansion",
        "graphoxide export wiki",
        "graphoxide export neo4j",
        "graphoxide hook install INPUT_PATH",
        "graphoxide watch INPUT_PATH",
    ] {
        assert!(!core.contains(marker), "lean core leaked {marker:?}");
    }
}

#[test]
fn test_lean_core_runs_default_pipeline_with_zero_references() {
    let (core, _) = split_artifacts("claude");
    for needed in [
        "### Step 1 - Ensure graphoxide is installed",
        "### Step 2 - Detect files",
        "### Step 3 - Extract entities and relationships",
        "#### Part A - Structural extraction for code files",
        "#### Part C - Merge AST + semantic into final extraction",
        "### Step 4 - Build graph, cluster, analyze, generate outputs",
        "### Step 5 - Label communities",
        "### Step 6 - Generate Obsidian vault (opt-in) + HTML",
        "### Step 9 - Save manifest, update cost tracker, clean up, and report",
        "## Honesty Rules",
        "graphoxide export html",
    ] {
        assert!(core.contains(needed), "missing {needed:?}");
    }
}

#[test]
fn test_extraction_states_no_api_key_required_for_every_host() {
    let rendered = render_all(&platforms(), None).unwrap();
    let bodies = rendered
        .iter()
        .filter(|artifact| {
            artifact
                .content
                .contains("### Step 3 - Extract entities and relationships")
        })
        .collect::<Vec<_>>();
    assert!(!bodies.is_empty());
    for artifact in bodies {
        let body = &artifact.content;
        assert!(
            body.contains("graphoxide needs no API key"),
            "{}",
            artifact.path
        );
        assert!(
            body.contains("Never ask the user for one, and never block on one."),
            "{}",
            artifact.path
        );
        assert!(
            body.contains("cannot dispatch subagents"),
            "{}",
            artifact.path
        );
        if let Some(tip) = body.find("Tip: set `GEMINI_API_KEY`") {
            assert!(body.find("graphoxide needs no API key").unwrap() < tip);
        }
    }
}

#[test]
fn test_references_contain_no_core_pipeline_content() {
    let (_, references) = split_artifacts("claude");
    for (name, body) in references {
        for marker in [
            "### Step 4 - Build graph, cluster, analyze, generate outputs",
            "### Step 5 - Label communities",
            "## Honesty Rules",
        ] {
            assert!(!body.contains(marker), "{name} leaked {marker:?}");
        }
    }
}

#[test]
fn test_reference_pointers_in_core_resolve_to_real_fragments() {
    let (core, references) = split_artifacts("claude");
    for name in REFERENCE_NAMES {
        assert!(
            core.contains(&format!("references/{name}")),
            "missing pointer {name}"
        );
        assert!(references.contains_key(name), "missing artifact {name}");
    }
}

#[test]
fn test_query_heading_is_homed_in_core_stub_only() {
    let (core, references) = split_artifacts("claude");
    let core_headings = headings(&core).into_iter().collect::<BTreeSet<_>>();
    let query_headings = headings(&references["query.md"])
        .into_iter()
        .collect::<BTreeSet<_>>();
    assert!(core_headings.contains("## For /graphoxide query"));
    assert!(!query_headings.contains("## For /graphoxide query"));
    assert!(query_headings.contains("## For /graphoxide path"));
    assert!(query_headings.contains("## For /graphoxide explain"));
    assert!(!core_headings.contains("## For /graphoxide path"));
}

#[test]
fn test_eight_references_render_for_claude() {
    let (_, references) = split_artifacts("claude");
    assert_eq!(reference_names(&references), REFERENCE_NAMES);
}

#[test]
fn test_headings_helper_ignores_code_fence_comments() {
    let markdown = "# Real Heading\n\n```bash\n# not a heading, a shell comment\necho hi\n```\n\n## Another Real One\n";
    assert_eq!(
        headings(markdown),
        ["# Real Heading", "## Another Real One"]
    );
}

#[test]
fn test_enum_is_full_six_value_superset_in_extraction_spec() {
    let (_, references) = split_artifacts("claude");
    let spec = &references["extraction-spec.md"];
    assert!(spec.contains(ENUM_PROSE));
    assert!(spec.contains(&format!("\"file_type\":\"{ENUM_VALUES}\"")));
}

#[test]
fn test_check_passes_for_codex_and_windows() {
    for key in ["codex", "windows"] {
        assert!(check(&artifacts(key)).is_empty(), "{key}");
    }
}

#[test]
fn test_audit_coverage_passes_for_codex_and_windows() {
    for key in ["codex", "windows"] {
        assert!(audit_coverage(&platform(key)).is_empty(), "{key}");
    }
}

#[test]
fn test_descriptions_are_unified() {
    let expected = format!("description: \"{UNIFIED_DESCRIPTION}\"");
    for (key, platform) in platforms() {
        let body = &render(&platform)[0].content;
        assert!(body.contains(&expected), "{key}");
        assert!(!body.contains("Provides persistent graph with god nodes"));
        assert!(!body.contains("treat the question as a /graphify query."));
        assert!(!body.contains("clustered communities"));
    }
}

#[test]
fn test_windows_frontmatter_name_and_shell_and_extra() {
    let (core, _) = split_artifacts("windows");
    assert!(core.starts_with("---\nname: graphoxide\n"));
    assert!(core.contains("```powershell"));
    assert!(core.contains("function Find-GraphoxideBinary"));
    assert!(core.contains("## Troubleshooting"));
    assert!(core.contains("### PowerShell 5.1: Vertical scrolling stops working"));
    assert!(core.contains("\n4. **Skip graspologic**"));
    assert!(core.find("## Troubleshooting") < core.find("## Honesty Rules"));
}

#[test]
fn test_codex_dispatch_is_agenttask_and_collects_in_memory() {
    let (core, _) = split_artifacts("codex");
    for marker in [
        "spawn_agent",
        "wait_agent",
        "close_agent",
        "multi_agent = true",
        "Codex collects in memory",
    ] {
        assert!(core.contains(marker));
    }
    let b2 = dispatch_block(&core);
    assert!(!b2.contains("Concrete example for 3 chunks"));
    assert!(!b2.contains("Agent tool call 1"));
}

#[test]
fn test_codex_and_windows_unify_enum_to_six_values() {
    for key in ["codex", "windows"] {
        let (_, references) = split_artifacts(key);
        let spec = &references["extraction-spec.md"];
        assert!(spec.contains(ENUM_PROSE));
        assert!(spec.contains(ENUM_VALUES));
        assert!(references
            .values()
            .all(|body| legacy_enum_lines(body).is_empty()));
    }
}

#[test]
fn test_codex_uses_compact_extraction_windows_uses_verbose() {
    let (_, codex) = split_artifacts("codex");
    let (_, windows) = split_artifacts("windows");
    assert!(codex["extraction-spec.md"].contains("(compact)"));
    assert!(!windows["extraction-spec.md"].contains("(compact)"));
}

#[test]
fn test_every_platform_query_has_expansion_and_fallback() {
    for key in ["claude", "codex", "windows", "opencode"] {
        let (core, references) = split_artifacts(key);
        assert!(core.contains("Expand the question against the graph's own vocabulary"));
        assert!(core.contains("read-only JSON graph traversal fallback"));
        let query = &references["query.md"];
        assert!(query.contains("Constrained query expansion"));
        assert!(query.contains("If the CLI is unavailable"));
        assert!(query.contains("## For /graphoxide path"));
        assert!(query.contains("## For /graphoxide explain"));
    }
}

#[test]
fn test_schema_singleton_passes_across_all_platforms() {
    assert!(schema_singleton(&platforms()).is_empty());
}

#[test]
fn test_schema_singleton_catches_legacy_enums() {
    let four = "file_type\":\"code|document|paper|image\"";
    let five = "file_type\":\"code|document|paper|image|rationale\"";
    let superset = format!("\"file_type\":\"{ENUM_VALUES}\"");
    assert_eq!(legacy_enum_lines(four), [four]);
    assert_eq!(legacy_enum_lines(five), [five]);
    assert!(legacy_enum_lines(&superset).is_empty());
    assert!(legacy_enum_lines("no enum here").is_empty());
}

#[test]
fn test_all_progressive_hosts_check_and_audit_clean() {
    for key in PROGRESSIVE_HOSTS {
        assert!(check(&artifacts(key)).is_empty(), "check {key}");
        assert!(audit_coverage(&platform(key)).is_empty(), "audit {key}");
    }
}

#[test]
fn test_no_host_has_trigger_in_frontmatter() {
    for key in [
        "claude", "codex", "opencode", "kilo", "copilot", "claw", "droid", "amp", "trae", "vscode",
        "kiro", "pi",
    ] {
        let (core, _) = split_artifacts(key);
        assert!(!frontmatter(&core).contains("trigger:"), "{key}");
    }
}

#[test]
fn test_kilo_renders_its_rules_tail_section() {
    let (core, _) = split_artifacts("kilo");
    assert!(core.contains("## Kilo-specific rules"));
    assert!(core.find("## Kilo-specific rules") < core.find("## Honesty Rules"));
}

#[test]
fn test_dispatch_variants_are_host_specific() {
    for (key, marker) in [
        ("opencode", "@mention"),
        ("droid", "Task(description="),
        ("amp", "Task(description="),
        ("trae", "Task(description="),
        ("vscode", "paste each response back"),
    ] {
        let (core, _) = split_artifacts(key);
        assert!(
            dispatch_block(&core)
                .to_lowercase()
                .contains(&marker.to_lowercase()),
            "{key}"
        );
    }
}

#[test]
fn test_compact_extraction_hosts_use_the_compact_spec() {
    for key in ["kiro", "pi", "claw"] {
        assert!(split_artifacts(key).1["extraction-spec.md"].contains("(compact)"));
    }
    for key in [
        "opencode", "kilo", "copilot", "droid", "amp", "trae", "vscode",
    ] {
        assert!(!split_artifacts(key).1["extraction-spec.md"].contains("(compact)"));
    }
}

#[test]
fn test_every_split_host_renders_eight_references() {
    for (key, platform) in platforms() {
        if platform.bucket == Bucket::Split {
            assert_eq!(
                reference_names(&split_artifacts(key).1),
                REFERENCE_NAMES,
                "{key}"
            );
        }
    }
}

#[test]
fn test_monoliths_render_inline_single_file_no_references() {
    for key in ["aider", "devin"] {
        let platform = platform(key);
        assert_eq!(platform.bucket, Bucket::Monolith);
        let rendered = render(&platform);
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0].path, format!("graphoxide/skill-{key}.md"));
        assert!(!rendered[0].content.contains("references/"));
    }
}

#[test]
fn test_monolith_roundtrip_passes_for_aider_and_devin() {
    for key in ["aider", "devin"] {
        assert!(monolith_roundtrip(&platform(key)).is_empty(), "{key}");
    }
}

#[test]
fn test_monoliths_change_only_sanctioned_lines() {
    for key in ["aider", "devin"] {
        let platform = platform(key);
        assert!(monolith_roundtrip(&platform).is_empty());
        let body = &render(&platform)[0].content;
        assert!(body.contains(ENUM_VALUES));
        assert!(body.contains(UNIFIED_DESCRIPTION));
    }
}

#[test]
fn test_monoliths_carry_the_1392_runbook_fixes() {
    for key in ["aider", "devin"] {
        let body = render(&platform(key))[0].content.clone();
        assert!(body.contains("directed: IS_DIRECTED"));
        assert!(!body.contains("build_with_options(extraction, Default::default())"));
        assert!(body.contains("substitute `IS_DIRECTED` everywhere"));
        assert!(body.contains("[\"document\", \"paper\", \"image\"]"));
        assert!(!body.contains("all detected categories"));
        assert!(body.contains("remove the stale cache entry"));

        let build = body.find("let graph = build_with_options").unwrap();
        let guard = body[build..].find("if graph.nodes.is_empty()").unwrap() + build;
        let wrote = body[build..].find("let wrote =").unwrap() + build;
        let report = body[build..].find("write_report(").unwrap() + build;
        assert!(build < guard && guard < wrote && wrote < report, "{key}");
        assert!(body.contains("if !wrote"));
    }
}

#[test]
fn test_monoliths_scope_semantic_cache_writes_to_uncached_files() {
    for key in ["aider", "devin"] {
        let body = &render(&platform(key))[0].content;
        assert!(body.contains(".graphoxide_uncached.txt"));
        assert!(body.contains("allowed_source_files = uncached"));
    }
}

#[test]
fn test_generated_runbooks_pass_root_to_save_manifest() {
    let all = platforms();
    let mut checked = 0;
    for platform in all.values() {
        for artifact in render(platform) {
            if artifact.path.ends_with("update.md") {
                checked += 1;
                assert!(artifact.content.contains("graphoxide update INPUT_PATH"));
            }
        }
        let core = &render(platform)[0].content;
        if platform.bucket == Bucket::Monolith {
            checked += 1;
            assert!(core.contains("root: Some(INPUT_PATH.into())"));
        } else {
            checked += 1;
            assert!(core.contains("graphoxide update INPUT_PATH"));
            assert!(core.contains("anchors manifest keys to the scan root"));
        }
    }
    assert!(checked >= 4);
}

#[test]
fn test_devin_keeps_its_multi_field_frontmatter() {
    let body = &render(&platform("devin"))[0].content;
    let head = frontmatter(body);
    assert!(head.contains("argument-hint:"));
    assert!(head.contains("model:"));
    assert!(head.contains("allowed-tools:"));
}

#[test]
fn test_always_on_renders_six_blocks() {
    let paths = render_always_on()
        .into_iter()
        .map(|artifact| artifact.path)
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            "graphoxide/always_on/agents-md.md",
            "graphoxide/always_on/antigravity-rules.md",
            "graphoxide/always_on/claude-md.md",
            "graphoxide/always_on/gemini-md.md",
            "graphoxide/always_on/kiro-steering.md",
            "graphoxide/always_on/vscode-instructions.md",
        ]
    );
}

#[test]
fn test_always_on_included_in_full_render_not_per_platform() {
    let all = platforms();
    let full = render_all(&all, None)
        .unwrap()
        .into_iter()
        .map(|artifact| artifact.path)
        .collect::<BTreeSet<_>>();
    let claude = render_all(&all, Some("claude"))
        .unwrap()
        .into_iter()
        .map(|artifact| artifact.path)
        .collect::<BTreeSet<_>>();
    assert!(full.contains("graphoxide/always_on/claude-md.md"));
    assert!(!claude.contains("graphoxide/always_on/claude-md.md"));
}

#[test]
fn test_always_on_roundtrip_is_byte_faithful() {
    assert!(always_on_roundtrip().is_empty());
    assert_eq!(
        sanctioned_always_on_edits()["_AGENTS_MD_SECTION"],
        ((OLD_AGENTS_INSTRUCTION, NEW_AGENTS_INSTRUCTION),)
    );
    let baseline = always_on_baseline("agents-md").unwrap();
    let rendered = always_on_constant("agents-md").unwrap();
    assert!(baseline.contains(OLD_AGENTS_INSTRUCTION));
    assert_eq!(
        baseline.replace(OLD_AGENTS_INSTRUCTION, NEW_AGENTS_INSTRUCTION),
        rendered
    );
    assert!(!rendered.contains("`skill` tool"));
    assert!(!rendered.contains("skill: \"graphify\""));
}

#[test]
fn test_extracted_constants_equal_the_packaged_always_on_files() {
    for artifact in render_always_on() {
        let name = artifact
            .path
            .rsplit('/')
            .next()
            .unwrap()
            .trim_end_matches(".md");
        assert_eq!(always_on_constant(name).unwrap(), artifact.content);
    }
}

#[test]
fn test_always_on_files_are_guarded_by_check() {
    let all = render_all(&platforms(), None).unwrap();
    assert!(check(&all).is_empty());
    let mutated = all
        .into_iter()
        .map(|mut artifact| {
            if artifact.path == "graphoxide/always_on/claude-md.md" {
                artifact.content.push_str("drift\n");
            }
            artifact
        })
        .collect::<Vec<_>>();
    let problems = check(&mutated);
    assert!(problems
        .iter()
        .any(|problem| problem.contains("always_on/claude-md.md")));
}

#[test]
fn test_audit_coverage_passes_for_every_split_host() {
    for (key, platform) in platforms() {
        if platform.bucket == Bucket::Split {
            assert!(audit_coverage(&platform).is_empty(), "{key}");
        }
    }
}

#[test]
fn test_audit_reads_each_host_against_its_own_v8_body() {
    assert_eq!(
        coverage_baseline_ref("claude"),
        format!("{GRAPHIFY_V8_BASELINE_SHA}:graphify/skill.md")
    );
    assert_eq!(
        coverage_baseline_ref("trae"),
        format!("{GRAPHIFY_V8_BASELINE_SHA}:graphify/skill-trae.md")
    );
    assert_eq!(
        coverage_baseline_ref("vscode"),
        format!("{GRAPHIFY_V8_BASELINE_SHA}:graphify/skill-vscode.md")
    );
}

#[test]
fn test_audit_catches_an_induced_per_host_drop() {
    let mut trae = platform("trae");
    trae.hooks_variant = HooksVariant::ClaudeMd;
    let problems = audit_coverage(&trae);
    assert!(problems
        .iter()
        .any(|problem| problem.contains("native AGENTS.md integration (Trae)")));
}

#[test]
fn test_audit_catches_a_dropped_non_allowlisted_heading() {
    let trae = platform("trae");
    let mut rendered = render(&trae);
    rendered[0].content = rendered[0]
        .content
        .replace("## Honesty Rules", "## Closing notes");
    assert!(!consolidation_allowlist("trae").contains(&"## Honesty Rules"));
    let problems = audit_artifacts(&trae, &rendered);
    assert!(problems
        .iter()
        .any(|problem| problem.contains("## Honesty Rules")));
}

#[test]
fn test_git_show_validators_skip_cleanly_without_origin_v8() {
    let directory = tempfile::tempdir().unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(directory.path())
        .status()
        .unwrap();
    assert!(status.success());
    let GitValidation::Skipped(message) = git_dependent_validation(directory.path()) else {
        panic!("shallow repository unexpectedly contained the v8 baseline")
    };
    assert!(message.contains("SKIPPED"));
    assert!(message.contains("fetch-depth: 0"));
}

#[test]
fn test_audit_allowlist_documents_only_consolidations() {
    let all_allowlisted = SHARED_INTRO_ALLOWLIST
        .iter()
        .chain(consolidation_allowlist("kilo"))
        .chain(consolidation_allowlist("vscode"))
        .copied()
        .collect::<BTreeSet<_>>();
    assert!(!all_allowlisted.contains("## For native AGENTS.md integration (Trae)"));
    let nonempty = platforms()
        .keys()
        .filter(|key| !consolidation_allowlist(key).is_empty())
        .copied()
        .collect::<BTreeSet<_>>();
    assert_eq!(nonempty, BTreeSet::from(["kilo", "vscode"]));
}

#[test]
fn test_trae_renders_native_agents_md_integration_not_claude() {
    let (core, references) = split_artifacts("trae");
    let hooks = &references["hooks.md"];
    assert!(hooks.contains("## For native AGENTS.md integration (Trae)"));
    assert!(hooks.contains("graphoxide trae install"));
    assert!(hooks.contains("graphoxide trae-cn install"));
    assert!(hooks.contains("writes a `## graphoxide` section to the local `AGENTS.md`"));
    assert!(!hooks.contains("graphoxide claude install"));
    assert!(!hooks.contains("native CLAUDE.md integration"));
    assert!(core.contains("## For the commit hook and native AGENTS.md integration"));
    assert!(core.contains("wire graphoxide into a project's AGENTS.md"));
    assert!(!core.contains("native CLAUDE.md integration"));
}

#[test]
fn test_trae_dispatch_carries_the_no_pretooluse_caveat() {
    let (core, _) = split_artifacts("trae");
    let b2 = dispatch_block(&core);
    assert!(b2.contains("Trae does NOT support PreToolUse hooks"));
    assert!(b2.contains("AGENTS.md rules are the always-on mechanism instead"));
}

#[test]
fn test_trae_hooks_reference_includes_the_pretooluse_note() {
    let hooks = split_artifacts("trae").1.remove("hooks.md").unwrap();
    assert!(hooks.contains("Unlike Claude Code, Trae does NOT support PreToolUse hooks"));
    assert!(hooks.contains("Run `/graphoxide --update` manually after code changes"));
}

#[test]
fn test_claude_flavored_hosts_keep_their_hooks_text_unchanged() {
    for key in ["claude", "droid", "codex", "windows", "kilo", "vscode"] {
        let (core, references) = split_artifacts(key);
        let hooks = &references["hooks.md"];
        assert!(hooks.contains("graphoxide claude install"), "{key}");
        assert!(hooks.contains("native CLAUDE.md integration"), "{key}");
        assert!(
            !core.contains("Trae does NOT support PreToolUse hooks"),
            "{key}"
        );
        assert!(
            !hooks.contains("Trae does NOT support PreToolUse hooks"),
            "{key}"
        );
        assert!(core.contains("## For the commit hook and native CLAUDE.md integration"));
    }
}

#[test]
fn test_amp_renders_native_agents_md_integration_v8_faithfully() {
    let (core, references) = split_artifacts("amp");
    let hooks = &references["hooks.md"];
    assert!(hooks.contains("## For native AGENTS.md integration"));
    assert!(!hooks.contains("## For native AGENTS.md integration (Trae)"));
    assert!(hooks.contains("make graphoxide always-on in Amp sessions"));
    assert!(hooks.contains("instructs Amp to check the graph"));
    assert!(hooks.contains("graphoxide amp install"));
    assert!(hooks.contains("graphoxide amp uninstall  # remove the section"));
    assert!(!hooks.contains("graphoxide trae install"));
    assert!(!hooks.contains("graphoxide trae-cn"));
    assert!(!hooks.contains("or: graphoxide"));
    assert!(!hooks.contains("graphoxide claude install"));
    assert!(!hooks.contains("native CLAUDE.md integration"));
    assert!(core.contains("## For the commit hook and native AGENTS.md integration"));
    assert!(core.contains("wire graphoxide into a project's AGENTS.md"));
    assert!(!core.contains("native CLAUDE.md integration"));
}

#[test]
fn test_amp_has_no_pretooluse_caveat_anywhere() {
    let (core, references) = split_artifacts("amp");
    let hooks = &references["hooks.md"];
    assert!(!core.contains("PreToolUse"));
    assert!(!hooks.contains("PreToolUse"));
    assert!(!core.contains("Trae does NOT support"));
    assert!(!hooks.contains("Trae does NOT support"));
    assert!(!dispatch_block(&core).contains("Trae"));
}

#[test]
fn test_amp_audit_coverage_passes_against_its_own_v8() {
    assert_eq!(
        coverage_baseline_ref("amp"),
        format!("{GRAPHIFY_V8_BASELINE_SHA}:graphify/skill-amp.md")
    );
    assert!(audit_coverage(&platform("amp")).is_empty());
}

#[test]
fn test_agents_renders_its_own_agents_md_hooks_wording() {
    let (core, references) = split_artifacts("agents");
    let hooks = &references["hooks.md"];
    assert!(hooks.contains("## For native AGENTS.md integration"));
    assert!(!hooks.contains("## For native AGENTS.md integration (Trae)"));
    assert!(hooks.contains("make graphoxide always-on in your agent sessions"));
    assert!(hooks.contains("graphoxide agents install"));
    assert!(hooks.contains("graphoxide agents uninstall  # remove the section"));
    assert!(!hooks.contains("graphoxide amp install"));
    assert!(!hooks.contains("graphoxide trae"));
    assert!(!hooks.contains("graphoxide claude install"));
    assert!(!hooks.contains("PreToolUse"));
    assert!(!core.contains("PreToolUse"));
    assert!(core.contains("## For the commit hook and native AGENTS.md integration"));
    assert!(!core.contains("native CLAUDE.md integration"));
}

#[test]
fn test_agents_body_matches_amp_modulo_hooks_wording() {
    let amp = artifacts("amp")
        .into_iter()
        .map(|artifact| {
            (
                artifact.path.rsplit('/').next().unwrap().to_owned(),
                artifact.content,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let agents = artifacts("agents")
        .into_iter()
        .map(|artifact| {
            (
                artifact.path.rsplit('/').next().unwrap().to_owned(),
                artifact.content,
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(amp["skill-amp.md"], agents["skill-agents.md"]);
    for (name, body) in &amp {
        if name != "skill-amp.md" && name != "hooks.md" {
            assert_eq!(body, &agents[name], "{name}");
        }
    }
    assert_ne!(amp["hooks.md"], agents["hooks.md"]);
}

#[test]
fn test_agents_audit_baseline_is_amps_v8_body() {
    assert_eq!(
        coverage_baseline_ref("agents"),
        format!("{GRAPHIFY_V8_BASELINE_SHA}:graphify/skill-amp.md")
    );
    assert!(audit_coverage(&platform("agents")).is_empty());
}

#[test]
fn test_semantic_cache_calls_pass_prompt_file_for_every_split_host() {
    let rendered = render_all(&platforms(), None).unwrap();
    let bodies = rendered
        .iter()
        .filter(|artifact| {
            artifact.content.contains("check_semantic_cache(")
                && artifact.content.contains("references/extraction-spec.md")
        })
        .collect::<Vec<_>>();
    assert!(!bodies.is_empty());
    for artifact in bodies {
        for call in ["check_semantic_cache(", "save_semantic_cache("] {
            let line = artifact
                .content
                .lines()
                .find(|line| line.contains(call))
                .unwrap();
            assert!(
                line.contains("prompt_file='SPEC_PATH'"),
                "{}: {line}",
                artifact.path
            );
        }
        assert!(artifact
            .content
            .contains("SPEC_PATH` below is the **absolute** path"));
    }
}
