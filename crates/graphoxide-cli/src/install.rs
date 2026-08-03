//! Install and uninstall integrations for coding-agent hosts.
//!
//! The upstream project supports both user-wide skill installation and
//! project-local, always-on guidance.  This module keeps those two scopes
//! explicit so project uninstall can never remove a user's global skill.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use std::{
    env, fmt, fs,
    path::{Path, PathBuf},
    str::FromStr,
};

pub const MANAGED_HEADING: &str = "## graphoxide";
const LEGACY_MANAGED_HEADING: &str = "## graphify";

const AGENTS_SECTION: &str = r#"## graphoxide

Use the installed Graphoxide skill or instructions before broad source searches. Start with
`graphoxide query "<question>"`,
`graphoxide explain <node>`, or `graphoxide path <a> <b>` first, and rebuild with
`graphoxide update .` after structural changes.

Dirty graphoxide-out/ files are expected after a rebuild and are not a reason to skip graphoxide.
See graphoxide-out/GRAPH_REPORT.md for the architecture summary.
"#;

const PROJECT_SECTION: &str = r#"## graphoxide

For codebase questions, run `graphoxide query "<question>"` before broad source searches.
If graphoxide-out/wiki/index.md exists, use it for broad navigation instead of scanning raw files.
Use graphoxide-out/GRAPH_REPORT.md as a fallback for broad architecture review. Rebuild with
`graphoxide update .` after structural changes.
"#;

const OPENCODE_PLUGIN: &str = r#"export const GraphoxidePlugin = async () => ({
  "tool.execute.before": async (input, output) => {
    if (input.tool === "bash" && output.args && output.args.command) {
      output.args.command = 'echo "Graphoxide graph available; run graphoxide query before broad searches. GRAPH_REPORT.md is the broad-architecture fallback." ; ' + output.args.command
    }
  },
})
"#;

const CURSOR_RULE: &str = r#"---
description: Use Graphoxide before broad code searches
alwaysApply: true
---

Run `graphoxide query "<question>"` before broad searches. Read
graphoxide-out/GRAPH_REPORT.md only as a fallback for broad architecture review.
"#;

const ANTIGRAVITY_RULE: &str = r#"---
trigger: always_on
---

Run `graphoxide query "<question>"` before broad code searches. Read
graphoxide-out/GRAPH_REPORT.md only as a fallback for broad architecture review.
"#;

const ANTIGRAVITY_WORKFLOW: &str = r#"# Graphoxide

Use the Graphoxide skill by name; its filesystem location depends on install scope.

1. Run `graphoxide query "<question>"`.
2. Inspect the cited source files.
3. Use graphoxide-out/GRAPH_REPORT.md only for broad architecture review.
4. Run `graphoxide update .` after structural changes.
"#;

const KIRO_STEERING: &str = r#"---
inclusion: always
---

Run `graphoxide query "<question>"` before broad source searches. Read
graphoxide-out/GRAPH_REPORT.md only as a fallback for broad architecture review.
"#;

/// Hosts with installable Graphoxide guidance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    Agents,
    Aider,
    Claude,
    CodeBuddy,
    Codex,
    Copilot,
    Devin,
    OpenCode,
    Kilo,
    Kimi,
    Kiro,
    Claw,
    Droid,
    Pi,
    Trae,
    TraeCn,
    Windows,
    Antigravity,
    AntigravityWindows,
    Amp,
    Cursor,
    Gemini,
    Hermes,
}

impl Platform {
    pub const NAMES: &'static [&'static str] = &[
        "agents",
        "aider",
        "claude",
        "codebuddy",
        "codex",
        "copilot",
        "devin",
        "opencode",
        "kilo",
        "kimi",
        "kiro",
        "claw",
        "droid",
        "pi",
        "trae",
        "trae-cn",
        "windows",
        "antigravity",
        "antigravity-windows",
        "amp",
        "cursor",
        "gemini",
        "hermes",
    ];

    /// Platforms represented by the upstream skill configuration table. Cursor
    /// and Gemini use bespoke integration paths and are intentionally excluded.
    pub const CONFIG_PLATFORMS: &'static [Self] = &[
        Self::Agents,
        Self::Aider,
        Self::Amp,
        Self::Antigravity,
        Self::AntigravityWindows,
        Self::Claude,
        Self::Claw,
        Self::CodeBuddy,
        Self::Codex,
        Self::Copilot,
        Self::Devin,
        Self::Droid,
        Self::Hermes,
        Self::Kilo,
        Self::Kimi,
        Self::Kiro,
        Self::OpenCode,
        Self::Pi,
        Self::Trae,
        Self::TraeCn,
        Self::Windows,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agents => "agents",
            Self::Aider => "aider",
            Self::Claude => "claude",
            Self::CodeBuddy => "codebuddy",
            Self::Codex => "codex",
            Self::Copilot => "copilot",
            Self::Devin => "devin",
            Self::OpenCode => "opencode",
            Self::Kilo => "kilo",
            Self::Kimi => "kimi",
            Self::Kiro => "kiro",
            Self::Claw => "claw",
            Self::Droid => "droid",
            Self::Pi => "pi",
            Self::Trae => "trae",
            Self::TraeCn => "trae-cn",
            Self::Windows => "windows",
            Self::Antigravity => "antigravity",
            Self::AntigravityWindows => "antigravity-windows",
            Self::Amp => "amp",
            Self::Cursor => "cursor",
            Self::Gemini => "gemini",
            Self::Hermes => "hermes",
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Platform {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "agents" | "skills" => Ok(Self::Agents),
            "aider" => Ok(Self::Aider),
            "claude" => Ok(Self::Claude),
            "codebuddy" => Ok(Self::CodeBuddy),
            "codex" => Ok(Self::Codex),
            "copilot" | "vscode" => Ok(Self::Copilot),
            "devin" => Ok(Self::Devin),
            "opencode" => Ok(Self::OpenCode),
            "kilo" => Ok(Self::Kilo),
            "kimi" => Ok(Self::Kimi),
            "kiro" => Ok(Self::Kiro),
            "claw" | "openclaw" => Ok(Self::Claw),
            "droid" => Ok(Self::Droid),
            "pi" => Ok(Self::Pi),
            "trae" => Ok(Self::Trae),
            "trae-cn" | "trae_cn" => Ok(Self::TraeCn),
            "windows" => Ok(Self::Windows),
            "antigravity" => Ok(Self::Antigravity),
            "antigravity-windows" | "antigravity_windows" => Ok(Self::AntigravityWindows),
            "amp" => Ok(Self::Amp),
            "cursor" => Ok(Self::Cursor),
            "gemini" => Ok(Self::Gemini),
            "hermes" => Ok(Self::Hermes),
            other => bail!(
                "unknown platform {other:?}; expected one of {}",
                Self::NAMES.join(", ")
            ),
        }
    }
}

/// Explicit filesystem inputs make installation deterministic and testable.
#[derive(Clone, Debug)]
pub struct InstallContext {
    pub project_root: PathBuf,
    pub home: PathBuf,
    pub project: bool,
    pub executable: PathBuf,
    pub windows: bool,
    pub local_app_data: Option<PathBuf>,
}

impl InstallContext {
    pub fn for_current_process(project_root: impl Into<PathBuf>, project: bool) -> Result<Self> {
        let home = env::var_os("HOME")
            .or_else(|| env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("cannot determine the user home directory"))?;
        Ok(Self {
            project_root: project_root.into(),
            home,
            project,
            executable: env::current_exe().unwrap_or_else(|_| PathBuf::from("graphoxide")),
            windows: cfg!(windows),
            local_app_data: env::var_os("LOCALAPPDATA").map(PathBuf::from),
        })
    }
}

/// One packaged progressive-disclosure reference file.
pub type ReferenceAsset = (&'static str, &'static str);

const REFERENCE_ASSETS: &[ReferenceAsset] = &[
    (
        "add-watch.md",
        include_str!("../assets/references/add-watch.md"),
    ),
    (
        "exports.md",
        include_str!("../assets/references/exports.md"),
    ),
    (
        "extraction-spec.md",
        include_str!("../assets/references/extraction-spec.md"),
    ),
    (
        "github-and-merge.md",
        include_str!("../assets/references/github-and-merge.md"),
    ),
    ("hooks.md", include_str!("../assets/references/hooks.md")),
    ("query.md", include_str!("../assets/references/query.md")),
    (
        "transcribe.md",
        include_str!("../assets/references/transcribe.md"),
    ),
    ("update.md", include_str!("../assets/references/update.md")),
];

/// Return the packaged references bundle for progressive hosts. Aider and
/// Devin intentionally remain monolithic.
pub fn packaged_skill_references(platform: Platform) -> Option<&'static [ReferenceAsset]> {
    match platform {
        Platform::Aider | Platform::Devin | Platform::Cursor => None,
        _ => Some(REFERENCE_ASSETS),
    }
}

/// Copy only a platform's skill artifacts at their real scope destination.
/// Host-specific always-on configuration is handled by [`install`].
pub fn install_skill(platform: Platform, context: &InstallContext) -> Result<Option<PathBuf>> {
    install_skill_with_references(platform, context, packaged_skill_references(platform))
}

/// Install a skill with an explicit bundle selection. This also models a
/// package moving between monolithic and progressive layouts during upgrades.
pub fn install_skill_with_references(
    platform: Platform,
    context: &InstallContext,
    references: Option<&[ReferenceAsset]>,
) -> Result<Option<PathBuf>> {
    let Some(destination) = platform_skill_destination(platform, context)? else {
        return Ok(None);
    };
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("skill destination has no parent: {}", destination.display()))?;
    fs::create_dir_all(parent)?;

    if let Some(references) = references {
        anyhow::ensure!(
            !references.is_empty(),
            "{platform} packaged references bundle is empty"
        );
        install_skill_references_with(&destination, |staging| {
            fs::create_dir(staging)?;
            for (index, (name, content)) in references.iter().enumerate() {
                let relative = Path::new(name);
                anyhow::ensure!(
                    !relative.is_absolute()
                        && relative.components().count() == 1
                        && relative.file_name().is_some(),
                    "invalid packaged reference name {name:?}"
                );
                anyhow::ensure!(
                    !references[..index]
                        .iter()
                        .any(|(previous, _)| previous == name),
                    "duplicate packaged reference name {name:?}"
                );
                fs::write(staging.join(relative), content)?;
            }
            Ok(())
        })?;
    } else {
        remove_managed_directory(&parent.join("references"))?;
        remove_staging_path(&parent.join("references.tmp"))?;
    }

    let temporary = parent.join("SKILL.md.tmp");
    let write_result = fs::write(
        &temporary,
        skill_asset(platform).ok_or_else(|| anyhow!("{platform} has no packaged skill"))?,
    )
    .and_then(|()| replace_file(&temporary, &destination));
    if let Err(error) = write_result {
        let _ = remove_known_file(&temporary);
        return Err(error).with_context(|| format!("could not install {}", destination.display()));
    }
    fs::write(
        parent.join(".graphoxide_version"),
        env!("CARGO_PKG_VERSION"),
    )?;
    remove_known_file(&parent.join(".graphify_version"))?;
    Ok(Some(destination))
}

/// Stage a references directory completely before swapping it into view. If
/// staging fails, the old visible bundle is preserved and partial work is
/// removed.
pub fn install_skill_references_with<F>(skill_destination: &Path, stage: F) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let parent = skill_destination.parent().ok_or_else(|| {
        anyhow!(
            "skill destination has no parent: {}",
            skill_destination.display()
        )
    })?;
    fs::create_dir_all(parent)?;
    let destination = parent.join("references");
    let staging = parent.join("references.tmp");
    remove_staging_path(&staging)?;
    if let Err(error) = stage(&staging) {
        let _ = remove_staging_path(&staging);
        return Err(error);
    }
    let swap = (|| -> Result<()> {
        anyhow::ensure!(
            fs::symlink_metadata(&staging)
                .map(|metadata| metadata.file_type().is_dir())
                .unwrap_or(false),
            "reference staging did not create a directory"
        );
        remove_managed_directory(&destination)?;
        fs::rename(&staging, &destination)?;
        Ok(())
    })();
    if let Err(error) = swap {
        let _ = remove_staging_path(&staging);
        return Err(error);
    }
    Ok(())
}

/// Remove a platform's skill, stamp, sidecar, and now-empty directory chain.
pub fn remove_skill(platform: Platform, context: &InstallContext) -> Result<bool> {
    let Some(destination) = platform_skill_destination(platform, context)? else {
        return Ok(false);
    };
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("skill destination has no parent: {}", destination.display()))?;
    let mut removed = path_exists(&destination);
    remove_known_file(&destination)?;
    for stamp in [".graphoxide_version", ".graphify_version"] {
        let path = parent.join(stamp);
        removed |= path_exists(&path);
        remove_known_file(&path)?;
    }
    let references = parent.join("references");
    removed |= path_exists(&references);
    remove_managed_directory(&references)?;
    let staging = parent.join("references.tmp");
    removed |= path_exists(&staging);
    remove_staging_path(&staging)?;
    remove_empty_parents(parent, &skill_cleanup_boundary(platform, context))?;
    Ok(removed)
}

/// Install one host integration and return every primary artifact written.
pub fn install(platform: Platform, context: &InstallContext) -> Result<Vec<PathBuf>> {
    install_with_strict(platform, context, false)
}

/// Install one host integration, optionally baking strict Claude read behavior
/// into the project hook command.
pub fn install_with_strict(
    platform: Platform,
    context: &InstallContext,
    strict: bool,
) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();

    if let Some(destination) = install_skill(platform, context)? {
        written.push(destination);
    }

    match platform {
        Platform::Claude | Platform::Windows => {
            let scoped = context.project_root.join(".claude/CLAUDE.md");
            install_marked_file(&scoped, PROJECT_SECTION)?;
            install_claude_hook_with_strict(&context.project_root, &context.executable, strict)?;
            written.push(scoped);
            if context.project {
                let root = context.project_root.join("CLAUDE.md");
                install_marked_file(&root, PROJECT_SECTION)?;
                written.push(root);
            }
        }
        Platform::CodeBuddy => {
            codebuddy_install(&context.project_root, &context.executable)?;
            written.push(context.project_root.join("CODEBUDDY.md"));
        }
        Platform::Codex => {
            agents_install(&context.project_root, platform)?;
            install_codex_hook(&context.project_root, &context.executable)?;
            written.push(context.project_root.join("AGENTS.md"));
        }
        Platform::OpenCode | Platform::Claw => {
            agents_install(&context.project_root, platform)?;
            written.push(context.project_root.join("AGENTS.md"));
        }
        Platform::Kilo => {
            let command = context.home.join(".config/kilo/command/graphoxide.md");
            write_text(&command, KILO_COMMAND)?;
            agents_install(&context.project_root, platform)?;
            written.extend([command, context.project_root.join("AGENTS.md")]);
        }
        Platform::Antigravity | Platform::AntigravityWindows => {
            let rule = context.project_root.join(".agents/rules/graphoxide.md");
            let workflow = context.project_root.join(".agents/workflows/graphoxide.md");
            write_text(&rule, ANTIGRAVITY_RULE)?;
            write_text(&workflow, ANTIGRAVITY_WORKFLOW)?;
            written.extend([rule, workflow]);
        }
        Platform::Amp => {
            cleanup_legacy_amp_skill(&context.home)?;
            agents_install(&context.project_root, platform)?;
            written.push(context.project_root.join("AGENTS.md"));
        }
        Platform::Cursor => {
            cursor_install(&context.project_root)?;
            written.push(context.project_root.join(".cursor/rules/graphoxide.mdc"));
        }
        Platform::Gemini => {
            gemini_install(&context.project_root, &context.executable)?;
            written.push(context.project_root.join("GEMINI.md"));
        }
        Platform::Kiro => {
            let steering = context.project_root.join(".kiro/steering/graphoxide.md");
            write_text(&steering, KIRO_STEERING)?;
            written.push(steering);
        }
        Platform::Devin => {
            if context.project {
                devin_rules_install(&context.project_root)?;
                written.push(devin_rules_path(&context.project_root));
            }
        }
        Platform::Agents
        | Platform::Aider
        | Platform::Copilot
        | Platform::Droid
        | Platform::Kimi
        | Platform::Pi
        | Platform::Trae
        | Platform::TraeCn
        | Platform::Hermes => {}
    }

    Ok(written)
}

/// Remove one host integration without crossing the selected scope.
pub fn uninstall(platform: Platform, context: &InstallContext) -> Result<()> {
    remove_skill(platform, context)?;

    match platform {
        Platform::Claude | Platform::Windows => {
            for path in [
                context.project_root.join("CLAUDE.md"),
                context.project_root.join("CLAUDE.local.md"),
                context.project_root.join(".claude/CLAUDE.md"),
                context.project_root.join(".claude/CLAUDE.local.md"),
            ] {
                uninstall_marked_file_tolerant(&path)?;
            }
            uninstall_claude_hook(&context.project_root)?;
        }
        Platform::CodeBuddy => codebuddy_uninstall(&context.project_root)?,
        Platform::Codex => {
            agents_uninstall(&context.project_root, Some(platform))?;
            uninstall_codex_hook(&context.project_root)?;
        }
        Platform::OpenCode | Platform::Claw | Platform::Amp => {
            agents_uninstall(&context.project_root, Some(platform))?;
        }
        Platform::Kilo => {
            agents_uninstall(&context.project_root, Some(platform))?;
            let command = context.home.join(".config/kilo/command/graphoxide.md");
            remove_file_and_empty_parents(&command, context.home.join(".config/kilo"))?;
        }
        Platform::Antigravity | Platform::AntigravityWindows => {
            remove_known_file(&context.project_root.join(".agents/rules/graphoxide.md"))?;
            remove_known_file(&context.project_root.join(".agents/workflows/graphoxide.md"))?;
        }
        Platform::Cursor => cursor_uninstall(&context.project_root)?,
        Platform::Gemini => gemini_uninstall(&context.project_root)?,
        Platform::Kiro => {
            remove_file_and_empty_parents(
                &context.project_root.join(".kiro/steering/graphoxide.md"),
                context.project_root.join(".kiro"),
            )?;
        }
        Platform::Devin => {
            if context.project {
                devin_rules_uninstall(&context.project_root)?;
            }
        }
        Platform::Agents
        | Platform::Aider
        | Platform::Copilot
        | Platform::Droid
        | Platform::Kimi
        | Platform::Pi
        | Platform::Trae
        | Platform::TraeCn
        | Platform::Hermes => {}
    }
    Ok(())
}

/// Remove every known installation for the selected scope.
pub fn uninstall_all(context: &InstallContext) -> Result<()> {
    for name in Platform::NAMES {
        uninstall(name.parse()?, context)?;
    }
    Ok(())
}

/// Resolve where a host discovers its skill file.
pub fn platform_skill_destination(
    platform: Platform,
    context: &InstallContext,
) -> Result<Option<PathBuf>> {
    if platform == Platform::Cursor {
        return Ok(None);
    }
    if context.project {
        let relative = match platform {
            Platform::Agents | Platform::Amp => ".agents/skills/graphoxide/SKILL.md",
            Platform::Aider => ".aider/graphoxide/SKILL.md",
            Platform::Claude | Platform::Windows => ".claude/skills/graphoxide/SKILL.md",
            Platform::CodeBuddy => ".codebuddy/skills/graphoxide/SKILL.md",
            Platform::Codex => ".codex/skills/graphoxide/SKILL.md",
            Platform::Copilot => ".copilot/skills/graphoxide/SKILL.md",
            Platform::Devin => ".devin/skills/graphoxide/SKILL.md",
            Platform::OpenCode => ".opencode/skills/graphoxide/SKILL.md",
            Platform::Kilo => ".kilo/skills/graphoxide/SKILL.md",
            Platform::Kimi => ".kimi/skills/graphoxide/SKILL.md",
            Platform::Kiro => ".kiro/skills/graphoxide/SKILL.md",
            Platform::Claw => ".openclaw/skills/graphoxide/SKILL.md",
            Platform::Droid => ".factory/skills/graphoxide/SKILL.md",
            Platform::Pi => ".pi/agent/skills/graphoxide/SKILL.md",
            Platform::Trae => ".trae/skills/graphoxide/SKILL.md",
            Platform::TraeCn => ".trae-cn/skills/graphoxide/SKILL.md",
            Platform::Antigravity | Platform::AntigravityWindows => {
                ".agents/skills/graphoxide/SKILL.md"
            }
            Platform::Hermes => ".hermes/skills/graphoxide/SKILL.md",
            Platform::Gemini => ".gemini/skills/graphoxide/SKILL.md",
            Platform::Cursor => unreachable!(),
        };
        return Ok(Some(context.project_root.join(relative)));
    }

    let destination = match platform {
        Platform::Agents => context.home.join(".agents/skills/graphoxide/SKILL.md"),
        Platform::Aider => context.home.join(".aider/graphoxide/SKILL.md"),
        Platform::Claude | Platform::Windows => {
            context.home.join(".claude/skills/graphoxide/SKILL.md")
        }
        Platform::CodeBuddy => context.home.join(".codebuddy/skills/graphoxide/SKILL.md"),
        Platform::Codex => context.home.join(".codex/skills/graphoxide/SKILL.md"),
        Platform::Copilot => context.home.join(".copilot/skills/graphoxide/SKILL.md"),
        Platform::Devin => context
            .home
            .join(".config/devin/skills/graphoxide/SKILL.md"),
        Platform::OpenCode => context
            .home
            .join(".config/opencode/skills/graphoxide/SKILL.md"),
        Platform::Kilo => context.home.join(".config/kilo/skills/graphoxide/SKILL.md"),
        Platform::Kimi => context.home.join(".kimi/skills/graphoxide/SKILL.md"),
        Platform::Kiro => context.home.join(".kiro/skills/graphoxide/SKILL.md"),
        Platform::Claw => context.home.join(".openclaw/skills/graphoxide/SKILL.md"),
        Platform::Droid => context.home.join(".factory/skills/graphoxide/SKILL.md"),
        Platform::Pi => context.home.join(".pi/agent/skills/graphoxide/SKILL.md"),
        Platform::Trae => context.home.join(".trae/skills/graphoxide/SKILL.md"),
        Platform::TraeCn => context.home.join(".trae-cn/skills/graphoxide/SKILL.md"),
        Platform::Antigravity | Platform::AntigravityWindows => context
            .home
            .join(".gemini/config/skills/graphoxide/SKILL.md"),
        Platform::Amp => context
            .home
            .join(".config/agents/skills/graphoxide/SKILL.md"),
        Platform::Hermes if context.windows => context
            .local_app_data
            .clone()
            .unwrap_or_else(|| context.home.join("AppData/Local"))
            .join("hermes/skills/graphoxide/SKILL.md"),
        Platform::Hermes => context.home.join(".hermes/skills/graphoxide/SKILL.md"),
        Platform::Gemini => context.home.join(".gemini/skills/graphoxide/SKILL.md"),
        Platform::Cursor => unreachable!(),
    };
    Ok(Some(destination))
}

fn skill_cleanup_boundary(platform: Platform, context: &InstallContext) -> PathBuf {
    if context.project {
        return context.project_root.clone();
    }
    match platform {
        Platform::Amp => context.home.join(".config/agents"),
        Platform::Antigravity | Platform::AntigravityWindows => context.home.join(".gemini/config"),
        Platform::Hermes if context.windows => context
            .local_app_data
            .clone()
            .unwrap_or_else(|| context.home.clone()),
        _ => context.home.clone(),
    }
}

pub fn skill_asset(platform: Platform) -> Option<&'static str> {
    Some(match platform {
        Platform::Agents => AGENTS_SKILL,
        Platform::Aider => AIDER_SKILL,
        Platform::Codex => CODEX_SKILL,
        Platform::Copilot => COPILOT_SKILL,
        Platform::Devin => DEVIN_SKILL,
        Platform::OpenCode => OPENCODE_SKILL,
        Platform::Kilo => KILO_SKILL,
        Platform::Kimi => BASE_SKILL,
        Platform::Kiro => KIRO_SKILL,
        Platform::Claw => CLAW_SKILL,
        Platform::Windows | Platform::AntigravityWindows => WINDOWS_SKILL,
        Platform::Droid => DROID_SKILL,
        Platform::Pi => PI_SKILL,
        Platform::Trae | Platform::TraeCn => TRAE_SKILL,
        Platform::Amp => AMP_SKILL,
        Platform::Claude | Platform::CodeBuddy | Platform::Antigravity | Platform::Hermes => {
            BASE_SKILL
        }
        Platform::Gemini => BASE_SKILL,
        Platform::Cursor => return None,
    })
}

pub const BASE_SKILL: &str = include_str!("../assets/skill.md");
pub const AGENTS_SKILL: &str = include_str!("../assets/skill-agents.md");
pub const AIDER_SKILL: &str = include_str!("../assets/skill-aider.md");
pub const CODEX_SKILL: &str = include_str!("../assets/skill-codex.md");
pub const COPILOT_SKILL: &str = include_str!("../assets/skill-copilot.md");
pub const DEVIN_SKILL: &str = include_str!("../assets/skill-devin.md");
pub const DEVIN_RULES: &str = include_str!("../assets/devin-rules.md");
pub const OPENCODE_SKILL: &str = include_str!("../assets/skill-opencode.md");
pub const KILO_SKILL: &str = include_str!("../assets/skill-kilo.md");
pub const CLAW_SKILL: &str = include_str!("../assets/skill-claw.md");
pub const WINDOWS_SKILL: &str = include_str!("../assets/skill-windows.md");
pub const DROID_SKILL: &str = include_str!("../assets/skill-droid.md");
pub const TRAE_SKILL: &str = include_str!("../assets/skill-trae.md");
pub const KIRO_SKILL: &str = include_str!("../assets/skill-kiro.md");
pub const AMP_SKILL: &str = include_str!("../assets/skill-amp.md");
pub const PI_SKILL: &str = include_str!("../assets/skill-pi.md");
pub const KILO_COMMAND: &str = include_str!("../assets/command-kilo.md");

/// All user-visible instruction surfaces installed by the CLI. Keeping this
/// inventory centralized makes the query-first token policy executable.
pub fn install_instruction_surfaces() -> Vec<(&'static str, &'static str)> {
    vec![
        ("agents-section", AGENTS_SECTION),
        ("project-section", PROJECT_SECTION),
        ("opencode-plugin", OPENCODE_PLUGIN),
        ("cursor-rule", CURSOR_RULE),
        ("antigravity-rule", ANTIGRAVITY_RULE),
        ("antigravity-workflow", ANTIGRAVITY_WORKFLOW),
        ("kiro-steering", KIRO_STEERING),
        ("devin-rules", DEVIN_RULES),
        ("skill", BASE_SKILL),
        ("skill-agents", AGENTS_SKILL),
        ("skill-aider", AIDER_SKILL),
        ("skill-codex", CODEX_SKILL),
        ("skill-copilot", COPILOT_SKILL),
        ("skill-devin", DEVIN_SKILL),
        ("skill-opencode", OPENCODE_SKILL),
        ("skill-kilo", KILO_SKILL),
        ("skill-claw", CLAW_SKILL),
        ("skill-windows", WINDOWS_SKILL),
        ("skill-droid", DROID_SKILL),
        ("skill-trae", TRAE_SKILL),
        ("skill-kiro", KIRO_SKILL),
        ("skill-amp", AMP_SKILL),
        ("skill-pi", PI_SKILL),
        ("command-kilo", KILO_COMMAND),
    ]
}

/// Host-neutral registration copy for integrations without a dedicated skill
/// invocation API.
pub fn skill_registration() -> &'static str {
    "Use the installed Graphoxide skill or instructions. Start with `graphoxide query \"<question>\"`; use graphoxide-out/GRAPH_REPORT.md only for broad architecture review."
}

pub fn packaged_asset_names() -> &'static [&'static str] {
    &[
        "skill.md",
        "skill-agents.md",
        "skill-aider.md",
        "skill-codex.md",
        "skill-copilot.md",
        "skill-devin.md",
        "devin-rules.md",
        "skill-opencode.md",
        "skill-kilo.md",
        "skill-claw.md",
        "skill-windows.md",
        "skill-droid.md",
        "skill-trae.md",
        "skill-kiro.md",
        "skill-amp.md",
        "skill-pi.md",
        "command-kilo.md",
    ]
}

pub fn agents_install(root: &Path, platform: Platform) -> Result<()> {
    install_marked_file(&root.join("AGENTS.md"), AGENTS_SECTION)?;
    match platform {
        Platform::OpenCode => install_agent_plugin(root, ".opencode", "opencode.json", false),
        Platform::Kilo => install_agent_plugin(root, ".kilo", "kilo.json", true),
        _ => Ok(()),
    }
}

pub fn agents_uninstall(root: &Path, platform: Option<Platform>) -> Result<bool> {
    let changed = uninstall_marked_file(&root.join("AGENTS.md"))?;
    match platform {
        Some(Platform::OpenCode) => uninstall_agent_plugin(root, ".opencode", "opencode.json")?,
        Some(Platform::Kilo) => uninstall_agent_plugin(root, ".kilo", "kilo.json")?,
        _ => {}
    }
    Ok(changed)
}

/// Install the generic Agent-Skills target and wire its project AGENTS.md.
/// This is deliberately separate from `install --platform agents`, whose
/// contract is skill-only.
pub fn agents_platform_install(context: &InstallContext) -> Result<()> {
    install_skill(Platform::Agents, context)?;
    agents_install(&context.project_root, Platform::Agents)
}

/// Remove the generic Agent-Skills target and its managed AGENTS.md section.
pub fn agents_platform_uninstall(context: &InstallContext) -> Result<()> {
    remove_skill(Platform::Agents, context)?;
    agents_uninstall(&context.project_root, Some(Platform::Agents))?;
    Ok(())
}

/// Return Devin's project-only always-on rules destination.
pub fn devin_rules_path(project_root: &Path) -> PathBuf {
    project_root.join(".windsurf/rules/graphoxide.md")
}

/// Install Devin's always-on project guidance, reporting whether bytes changed.
pub fn devin_rules_install(project_root: &Path) -> Result<bool> {
    let destination = devin_rules_path(project_root);
    if fs::read_to_string(&destination).ok().as_deref() == Some(DEVIN_RULES) {
        return Ok(false);
    }
    write_text(&destination, DEVIN_RULES)?;
    Ok(true)
}

/// Remove Devin's project-only rule without crossing the project boundary.
pub fn devin_rules_uninstall(project_root: &Path) -> Result<bool> {
    let destination = devin_rules_path(project_root);
    let removed = path_exists(&destination);
    remove_file_and_empty_parents(&destination, project_root.to_path_buf())?;
    Ok(removed)
}

/// Install Devin's scoped skill and, for project scope, its always-on rule.
/// The return value is false only when every managed artifact already matched.
pub fn devin_platform_install(context: &InstallContext) -> Result<bool> {
    let skill = platform_skill_destination(Platform::Devin, context)?
        .ok_or_else(|| anyhow!("devin has no skill destination"))?;
    let skill_current = fs::read_to_string(&skill).ok().as_deref() == Some(DEVIN_SKILL);
    let rules_current = !context.project
        || fs::read_to_string(devin_rules_path(&context.project_root))
            .ok()
            .as_deref()
            == Some(DEVIN_RULES);
    install(Platform::Devin, context)?;
    Ok(!(skill_current && rules_current))
}

/// Remove only the Devin artifacts in the selected scope.
pub fn devin_platform_uninstall(context: &InstallContext) -> Result<bool> {
    let skill = platform_skill_destination(Platform::Devin, context)?
        .ok_or_else(|| anyhow!("devin has no skill destination"))?;
    let mut removed = path_exists(&skill);
    if context.project {
        removed |= path_exists(&devin_rules_path(&context.project_root));
    }
    uninstall(Platform::Devin, context)?;
    Ok(removed)
}

pub fn codebuddy_install(root: &Path, executable: &Path) -> Result<bool> {
    let markdown = root.join("CODEBUDDY.md");
    let before = fs::read(&markdown).ok();
    install_marked_file(&markdown, PROJECT_SECTION)?;
    install_json_hook(
        &root.join(".codebuddy/settings.json"),
        "PreToolUse",
        &claude_pretooluse_hooks(executable, false),
    )?;
    Ok(before.as_deref() != fs::read(&markdown).ok().as_deref())
}

pub fn codebuddy_uninstall(root: &Path) -> Result<()> {
    uninstall_marked_file(&root.join("CODEBUDDY.md"))?;
    uninstall_json_hook(&root.join(".codebuddy/settings.json"), "PreToolUse")
}

/// Install CodeBuddy's scoped skill plus its project guidance and hooks.
pub fn codebuddy_platform_install(context: &InstallContext) -> Result<bool> {
    install_skill(Platform::CodeBuddy, context)?;
    codebuddy_install(&context.project_root, &context.executable)
}

/// Remove CodeBuddy's scoped skill plus its project guidance and hooks.
pub fn codebuddy_platform_uninstall(context: &InstallContext) -> Result<()> {
    remove_skill(Platform::CodeBuddy, context)?;
    codebuddy_uninstall(&context.project_root)
}

pub fn gemini_install(root: &Path, executable: &Path) -> Result<()> {
    install_marked_file(&root.join("GEMINI.md"), PROJECT_SECTION)?;
    install_json_hook(
        &root.join(".gemini/settings.json"),
        "BeforeTool",
        &[gemini_hook(executable)],
    )
}

pub fn gemini_uninstall(root: &Path) -> Result<()> {
    uninstall_marked_file(&root.join("GEMINI.md"))?;
    uninstall_json_hook(&root.join(".gemini/settings.json"), "BeforeTool")
}

/// Install the VS Code Copilot Chat skill globally and its always-on project
/// instructions locally.
pub fn vscode_install(context: &InstallContext) -> Result<()> {
    let mut skill_context = context.clone();
    skill_context.project = false;
    install_skill(Platform::Copilot, &skill_context)?;
    install_marked_file(
        &context.project_root.join(".github/copilot-instructions.md"),
        PROJECT_SECTION,
    )
}

/// Remove only Graphoxide's VS Code skill and managed instructions section.
pub fn vscode_uninstall(context: &InstallContext) -> Result<()> {
    let mut skill_context = context.clone();
    skill_context.project = false;
    remove_skill(Platform::Copilot, &skill_context)?;
    uninstall_marked_file(&context.project_root.join(".github/copilot-instructions.md"))?;
    Ok(())
}

pub fn install_claude_hook(root: &Path, executable: &Path) -> Result<()> {
    install_claude_hook_with_strict(root, executable, false)
}

pub fn install_claude_hook_with_strict(root: &Path, executable: &Path, strict: bool) -> Result<()> {
    install_json_hook(
        &root.join(".claude/settings.json"),
        "PreToolUse",
        &claude_pretooluse_hooks(executable, strict),
    )
}

/// Return the exact shell-agnostic Claude/CodeBuddy hook commands emitted by
/// installation. Keeping this builder public makes command-shape parity
/// independently testable without writing settings files.
pub fn claude_pretooluse_hooks(executable: &Path, strict: bool) -> Vec<(&'static str, String)> {
    let read_argument = if strict {
        "hook-guard read --strict"
    } else {
        "hook-guard read"
    };
    vec![
        ("Bash|Grep", plain_command(executable, "hook-guard search")),
        ("Read|Glob", plain_command(executable, read_argument)),
    ]
}

/// Return the exact Gemini BeforeTool hook command emitted by installation.
pub fn gemini_hook(executable: &Path) -> (&'static str, String) {
    (
        "read_file|list_directory",
        plain_command(executable, "hook-guard gemini"),
    )
}

pub fn uninstall_claude_hook(root: &Path) -> Result<()> {
    for path in [
        root.join(".claude/settings.json"),
        root.join(".claude/settings.local.json"),
    ] {
        uninstall_json_hook(&path, "PreToolUse")?;
    }
    Ok(())
}

pub fn install_codex_hook(root: &Path, executable: &Path) -> Result<()> {
    install_json_hook(
        &root.join(".codex/hooks.json"),
        "PreToolUse",
        &[("Bash", plain_command(executable, "hook-check"))],
    )
}

pub fn uninstall_codex_hook(root: &Path) -> Result<()> {
    uninstall_json_hook(&root.join(".codex/hooks.json"), "PreToolUse")
}

pub fn cursor_install(root: &Path) -> Result<()> {
    let path = root.join(".cursor/rules/graphoxide.mdc");
    write_text(&path, CURSOR_RULE)
}

pub fn cursor_uninstall(root: &Path) -> Result<()> {
    remove_known_file(&root.join(".cursor/rules/graphoxide.mdc"))
}

/// Strip only an exact managed H2 section and leave similarly named H3 or
/// inline user text untouched. The section ends at the next H2 or EOF.
pub fn remove_marker_section(content: &str, marker: &str) -> Option<String> {
    let mut start = None;
    let mut end = content.len();
    let mut offset = 0;
    for segment in content.split_inclusive('\n') {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if start.is_none() {
            if line == marker {
                start = Some(offset);
            }
        } else if line.starts_with("## ") && !line.starts_with("### ") {
            end = offset;
            break;
        }
        offset += segment.len();
    }
    let start = start?;
    let mut output = String::with_capacity(content.len().saturating_sub(end - start));
    output.push_str(&content[..start]);
    output.push_str(&content[end..]);
    while output.contains("\n\n\n") {
        output = output.replacen("\n\n\n", "\n\n", 1);
    }
    Some(output)
}

/// Replace the last exact managed H2 section in place, or append it when no
/// exact heading exists. Inline mentions and similarly named headings remain
/// user-owned content.
pub fn replace_or_append_section(content: &str, marker: &str, section: &str) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    let Some(start) = lines.iter().rposition(|line| line.trim() == marker) else {
        let mut output = if content.trim().is_empty() {
            section.trim().to_owned()
        } else {
            format!("{}\n\n{}", content.trim_end(), section.trim_start())
        };
        if !output.ends_with('\n') {
            output.push('\n');
        }
        return output;
    };

    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find_map(|(index, line)| line.starts_with("## ").then_some(index))
        .unwrap_or(lines.len());
    let head = lines[..start].join("\n");
    let tail = lines[end..].join("\n");
    let mut parts = Vec::with_capacity(3);
    if !head.trim().is_empty() {
        parts.push(head.trim_end());
    }
    parts.push(section.trim());
    if !tail.trim().is_empty() {
        parts.push(tail.trim_start());
    }
    let mut output = parts.join("\n\n");
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output
}

/// Parse the leading numeric core of each dotted version segment. Malformed
/// segments degrade to zero so a stale stamp can never crash the CLI.
pub fn version_tuple(version: &str) -> Vec<u64> {
    version
        .split('.')
        .map(|segment| {
            let digits: String = segment
                .chars()
                .take_while(|character| character.is_ascii_digit())
                .collect();
            digits.parse().unwrap_or(0)
        })
        .collect()
}

/// Return a direction-aware warning for a stale installed skill.
///
/// A newer on-disk skill must never be "repaired" with the older running
/// package because that would silently downgrade its instructions.
pub fn skill_version_warning(skill_destination: &Path, package_version: &str) -> Option<String> {
    let parent = skill_destination.parent()?;
    let version_file = [".graphoxide_version", ".graphify_version"]
        .into_iter()
        .map(|name| parent.join(name))
        .find(|path| path.is_file())?;
    if !skill_destination.is_file() {
        return Some(
            "warning: skill directory exists but SKILL.md is missing. Run 'graphoxide install' to repair."
                .to_owned(),
        );
    }
    let installed = fs::read_to_string(version_file).ok()?.trim().to_owned();
    let skill = fs::read_to_string(skill_destination).ok()?;
    if skill.contains("references/") && !parent.join("references").is_dir() {
        return Some(
            "warning: the installed skill's references/ sidecar is missing. Run 'graphoxide install' to repair."
                .to_owned(),
        );
    }
    if installed == package_version {
        return None;
    }
    if version_tuple(&installed) > version_tuple(package_version) {
        Some(format!(
            "warning: skill is from graphoxide {installed}, but the package is {package_version} (older). Upgrade the package; running 'graphoxide install' would downgrade the skill."
        ))
    } else {
        Some(format!(
            "warning: skill is from graphoxide {installed}, package is {package_version}. Run 'graphoxide install' to update."
        ))
    }
}

fn install_marked_file(path: &Path, section: &str) -> Result<()> {
    let original = fs::read_to_string(path).unwrap_or_default();
    let original = remove_marker_section(&original, LEGACY_MANAGED_HEADING).unwrap_or(original);
    let content = replace_or_append_section(&original, MANAGED_HEADING, section);
    write_text(path, &content)
}

fn uninstall_marked_file(path: &Path) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let original = fs::read_to_string(path)?;
    let content = remove_marker_section(&original, MANAGED_HEADING)
        .or_else(|| remove_marker_section(&original, LEGACY_MANAGED_HEADING));
    let Some(content) = content else {
        return Ok(false);
    };
    if content.trim().is_empty() {
        fs::remove_file(path)?;
    } else {
        write_text(path, &content)?;
    }
    Ok(true)
}

/// Local-only Claude instruction files can be user-managed, unreadable, or
/// encoded as something other than UTF-8. In those cases there is no safely
/// decodable managed heading to remove, so leave the file untouched.
fn uninstall_marked_file_tolerant(path: &Path) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let original = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::InvalidData | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error.into()),
    };
    let content = remove_marker_section(&original, MANAGED_HEADING)
        .or_else(|| remove_marker_section(&original, LEGACY_MANAGED_HEADING));
    let Some(content) = content else {
        return Ok(false);
    };
    if content.trim().is_empty() {
        fs::remove_file(path)?;
    } else {
        write_text(path, &content)?;
    }
    Ok(true)
}

fn install_agent_plugin(root: &Path, directory: &str, config_name: &str, uri: bool) -> Result<()> {
    let base = root.join(directory);
    let plugin = base.join("plugins/graphoxide.js");
    write_text(&plugin, OPENCODE_PLUGIN)?;
    let registration = if uri {
        path_uri(&plugin)?
    } else {
        "./plugins/graphoxide.js".to_owned()
    };
    let (config_path, mut config) = read_agent_config(&base, config_name)?;
    let object = config
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} must contain a JSON object", config_path.display()))?;
    let plugins = object.entry("plugin").or_insert_with(|| json!([]));
    let plugins = plugins
        .as_array_mut()
        .ok_or_else(|| anyhow!("{}.plugin must be an array", config_path.display()))?;
    if !plugins
        .iter()
        .any(|value| value.as_str() == Some(&registration))
    {
        plugins.push(Value::String(registration));
    }
    write_json(&config_path, &config)
}

fn uninstall_agent_plugin(root: &Path, directory: &str, config_name: &str) -> Result<()> {
    let base = root.join(directory);
    let plugin = base.join("plugins/graphoxide.js");
    remove_known_file(&plugin)?;
    let config_path = base.join(config_name);
    if !config_path.is_file() {
        return Ok(());
    }
    let mut config: Value = serde_json::from_str(&fs::read_to_string(&config_path)?)?;
    if let Some(plugins) = config.get_mut("plugin").and_then(Value::as_array_mut) {
        plugins.retain(|value| {
            !value
                .as_str()
                .is_some_and(|item| item.contains("graphoxide.js"))
        });
    }
    write_json(&config_path, &config)
}

fn read_agent_config(base: &Path, config_name: &str) -> Result<(PathBuf, Value)> {
    let json_path = base.join(config_name);
    if json_path.is_file() {
        let value = serde_json::from_str(&fs::read_to_string(&json_path)?)?;
        return Ok((json_path, value));
    }
    let jsonc_path = json_path.with_extension("jsonc");
    if jsonc_path.is_file() {
        let source = fs::read_to_string(&jsonc_path)?;
        let without_comments = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let value = serde_json::from_str(&without_comments)
            .with_context(|| format!("could not parse JSONC source {}", jsonc_path.display()))?;
        return Ok((json_path, value));
    }
    Ok((json_path, Value::Object(Map::new())))
}

fn install_json_hook(path: &Path, event: &str, hooks_to_add: &[(&str, String)]) -> Result<()> {
    let original_bytes = if path.is_file() {
        Some(fs::read(path)?)
    } else {
        None
    };
    let mut settings = if let Some(bytes) = original_bytes.as_deref() {
        let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
        serde_json::from_slice(bytes)
            .with_context(|| format!("refusing to modify invalid {}", path.display()))?
    } else {
        Value::Object(Map::new())
    };
    let original_settings = settings.clone();
    let root = settings
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} must contain a JSON object", path.display()))?;
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("{}.hooks must be an object", path.display()))?;
    let entries = hooks.entry(event).or_insert_with(|| json!([]));
    let entries = entries
        .as_array_mut()
        .ok_or_else(|| anyhow!("{}.hooks.{event} must be an array", path.display()))?;
    entries.retain(|entry| {
        let entry = entry.to_string().to_ascii_lowercase();
        !entry.contains("graphoxide") && !entry.contains("graphify")
    });
    for (matcher, command) in hooks_to_add {
        entries.push(json!({
            "matcher": matcher,
            "hooks": [{"type": "command", "command": command}],
        }));
    }
    if settings == original_settings {
        return Ok(());
    }
    if original_bytes.is_some() {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("settings path has no file name: {}", path.display()))?;
        fs::copy(
            path,
            path.with_file_name(format!("{file_name}.graphify-bak")),
        )?;
    }
    write_json(path, &settings)
}

fn uninstall_json_hook(path: &Path, event: &str) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let mut settings: Value = match serde_json::from_str(&fs::read_to_string(path)?) {
        Ok(value) => value,
        Err(_) => return Ok(()),
    };
    if let Some(entries) = settings
        .get_mut("hooks")
        .and_then(|hooks| hooks.get_mut(event))
        .and_then(Value::as_array_mut)
    {
        entries.retain(|entry| {
            let entry = entry.to_string().to_ascii_lowercase();
            !entry.contains("graphoxide") && !entry.contains("graphify")
        });
        write_json(path, &settings)?;
    }
    Ok(())
}

fn plain_command(executable: &Path, argument: &str) -> String {
    let raw = executable.to_string_lossy().replace('\\', "/");
    if raw.contains(' ') {
        format!("\"{}\" {argument}", raw.replace('"', "\\\""))
    } else {
        format!("{raw} {argument}")
    }
}

fn cleanup_legacy_amp_skill(home: &Path) -> Result<()> {
    let skill = home.join(".amp/skills/graphoxide/SKILL.md");
    remove_file_and_empty_parents(&skill, home.join(".amp"))
}

fn path_uri(path: &Path) -> Result<String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    Ok(format!(
        "file://{}",
        absolute.to_string_lossy().replace(' ', "%20")
    ))
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    write_text(path, &(serde_json::to_string_pretty(value)? + "\n"))
}

fn write_text(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content).with_context(|| format!("could not write {}", path.display()))
}

fn remove_known_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
            fs::remove_file(path)?;
        }
        Ok(_) => bail!("refusing to remove non-file {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn remove_managed_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(path)?,
        Ok(metadata) if metadata.file_type().is_symlink() => fs::remove_file(path)?,
        Ok(_) => bail!("refusing to remove non-directory {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn remove_staging_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(first_error) if path_exists(destination) => {
            let metadata = fs::symlink_metadata(destination)?;
            if !(metadata.file_type().is_file() || metadata.file_type().is_symlink()) {
                return Err(first_error);
            }
            fs::remove_file(destination)?;
            fs::rename(source, destination)
        }
        Err(error) => Err(error),
    }
}

fn remove_empty_parents(start: &Path, boundary: &Path) -> Result<()> {
    let mut directory = Some(start);
    while let Some(current) = directory {
        if current == boundary || !current.starts_with(boundary) {
            break;
        }
        match fs::remove_dir(current) {
            Ok(()) => directory = current.parent(),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound
                ) =>
            {
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn remove_file_and_empty_parents(path: &Path, boundary: PathBuf) -> Result<()> {
    remove_known_file(path)?;
    if let Some(parent) = path.parent() {
        remove_empty_parents(parent, &boundary)?;
    }
    Ok(())
}
