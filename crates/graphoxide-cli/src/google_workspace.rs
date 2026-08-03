//! Google Workspace shortcut parsing and deterministic `gws` export helpers.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoogleShortcut {
    pub file_id: String,
    pub account: Option<String>,
    pub resource_key: Option<String>,
    pub url: Option<String>,
}

#[derive(Deserialize)]
struct ShortcutDocument {
    #[serde(default)]
    file_id: Option<String>,
    #[serde(default)]
    doc_id: Option<String>,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    resource_key: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

pub fn read_google_shortcut(path: &Path) -> Result<GoogleShortcut> {
    let document: ShortcutDocument = serde_json::from_str(
        &fs::read_to_string(path)
            .with_context(|| format!("could not read Google shortcut {}", path.display()))?,
    )
    .with_context(|| format!("could not parse Google shortcut {}", path.display()))?;
    let url_id = document.url.as_deref().and_then(file_id_from_url);
    let file_id = document
        .file_id
        .or(document.doc_id)
        .or(url_id)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("Google shortcut {} has no file ID", path.display()))?;
    let url_resource_key = document
        .url
        .as_deref()
        .and_then(|url| query_value(url, "resourcekey"));
    Ok(GoogleShortcut {
        file_id,
        account: document.account.or(document.email),
        resource_key: document.resource_key.or(url_resource_key),
        url: document.url,
    })
}

fn file_id_from_url(url: &str) -> Option<String> {
    let (_, after) = url.split_once("/d/")?;
    let id = after.split(['/', '?', '#']).next()?.trim();
    (!id.is_empty()).then(|| id.to_owned())
}

fn query_value(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1.split('#').next().unwrap_or_default();
    query.split('&').find_map(|part| {
        let (name, value) = part.split_once('=')?;
        name.eq_ignore_ascii_case(key).then(|| value.to_owned())
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GwsExportCommand {
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
    pub cwd: PathBuf,
}

pub fn gws_export_command(
    program: &Path,
    file_id: &str,
    mime_type: &str,
    output: &Path,
) -> Result<GwsExportCommand> {
    let parent = output
        .parent()
        .ok_or_else(|| anyhow!("Google Workspace export has no output directory"))?;
    fs::create_dir_all(parent)?;
    let cwd = parent.canonicalize()?;
    let filename = output
        .file_name()
        .ok_or_else(|| anyhow!("Google Workspace export has no output filename"))?;
    let parameters = serde_json::to_string(&json!({
        "fileId": file_id,
        "mimeType": mime_type,
    }))?;
    Ok(GwsExportCommand {
        program: program.to_path_buf(),
        arguments: vec![
            "drive".into(),
            "files".into(),
            "export".into(),
            "--params".into(),
            parameters.into(),
            "-o".into(),
            filename.to_os_string(),
        ],
        cwd,
    })
}

pub fn run_gws_export(
    file_id: &str,
    mime_type: &str,
    output: &Path,
    _resource_key: Option<&str>,
) -> Result<()> {
    let program = find_on_path("gws").ok_or_else(|| anyhow!("gws executable was not found"))?;
    let specification = gws_export_command(&program, file_id, mime_type, output)?;
    let result = Command::new(&specification.program)
        .args(&specification.arguments)
        .current_dir(&specification.cwd)
        .output()?;
    if !result.status.success() {
        bail!(
            "gws export failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        );
    }
    Ok(())
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?).find_map(|directory| {
        let candidate = directory.join(name);
        candidate.is_file().then_some(candidate)
    })
}

/// Convert a shortcut using injectable export and spreadsheet conversion
/// functions. The injected boundary keeps subprocess and document conversion
/// behavior independently testable.
pub fn convert_google_workspace_file_with<E, X>(
    shortcut_path: &Path,
    output_directory: &Path,
    mut export: E,
    mut xlsx_to_markdown: X,
) -> Result<Option<PathBuf>>
where
    E: FnMut(&str, &str, &Path, Option<&str>) -> Result<()>,
    X: FnMut(&Path) -> Result<String>,
{
    let shortcut = read_google_shortcut(shortcut_path)?;
    let stem = shortcut_path
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("Google shortcut has no UTF-8 filename"))?;
    fs::create_dir_all(output_directory)?;
    let markdown_path = output_directory.join(format!("{stem}.md"));
    match shortcut_path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("gdoc") => {
            export(
                &shortcut.file_id,
                "text/markdown",
                &markdown_path,
                shortcut.resource_key.as_deref(),
            )?;
            let body = fs::read_to_string(&markdown_path)?;
            fs::write(
                &markdown_path,
                format!(
                    "---\nsource_type: \"google_workspace\"\nsource_file: \"{}\"\n---\n\n{}",
                    shortcut_path.display(),
                    body
                ),
            )?;
        }
        Some("gsheet") => {
            let workbook = output_directory.join(format!("{stem}.xlsx"));
            export(
                &shortcut.file_id,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                &workbook,
                shortcut.resource_key.as_deref(),
            )?;
            let body = xlsx_to_markdown(&workbook)?;
            fs::write(
                &markdown_path,
                format!(
                    "---\nsource_type: \"google_workspace\"\nsource_file: \"{}\"\n---\n\n{}",
                    shortcut_path.display(),
                    body
                ),
            )?;
        }
        _ => return Ok(None),
    }
    Ok(Some(markdown_path))
}

pub fn google_workspace_enabled_value(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

pub fn google_workspace_enabled() -> bool {
    let value = env::var("GRAPHOXIDE_GOOGLE_WORKSPACE")
        .ok()
        .or_else(|| env::var("GRAPHIFY_GOOGLE_WORKSPACE").ok());
    google_workspace_enabled_value(value.as_deref())
}
