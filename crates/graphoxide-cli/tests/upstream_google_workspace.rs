//! Executable port of upstream `tests/test_google_workspace.py`.

use anyhow::Result;
use graphoxide_cli::google_workspace::{
    convert_google_workspace_file_with, google_workspace_enabled_value, gws_export_command,
    read_google_shortcut,
};
use serde_json::Value;
use std::{ffi::OsStr, fs, path::Path};
use tempfile::TempDir;

#[test]
fn test_read_google_shortcut_doc_id() {
    let temp = TempDir::new().unwrap();
    let shortcut = temp.path().join("Planning.gdoc");
    fs::write(&shortcut, r#"{"url":"https://docs.google.com/document/d/doc-123/edit","doc_id":"doc-123","email":"me@example.com"}"#).unwrap();
    let metadata = read_google_shortcut(&shortcut).unwrap();
    assert_eq!(metadata.file_id, "doc-123");
    assert_eq!(metadata.account.as_deref(), Some("me@example.com"));
}

#[test]
fn test_read_google_shortcut_extracts_id_from_url() {
    let temp = TempDir::new().unwrap();
    let shortcut = temp.path().join("Budget.gsheet");
    fs::write(
        &shortcut,
        r#"{"url":"https://docs.google.com/spreadsheets/d/sheet-456/edit?resourcekey=key-1"}"#,
    )
    .unwrap();
    let metadata = read_google_shortcut(&shortcut).unwrap();
    assert_eq!(metadata.file_id, "sheet-456");
    assert_eq!(metadata.resource_key.as_deref(), Some("key-1"));
}

#[test]
fn test_convert_gdoc_to_markdown_sidecar() {
    let temp = TempDir::new().unwrap();
    let shortcut = temp.path().join("Planning.gdoc");
    fs::write(
        &shortcut,
        r#"{"url":"https://docs.google.com/document/d/doc-123/edit","doc_id":"doc-123"}"#,
    )
    .unwrap();
    let output = convert_google_workspace_file_with(
        &shortcut,
        &temp.path().join("converted"),
        |file_id, mime_type, output, _| {
            assert_eq!(file_id, "doc-123");
            assert_eq!(mime_type, "text/markdown");
            fs::write(output, "# Planning\n\nExported doc text.")?;
            Ok(())
        },
        |_| anyhow::bail!("spreadsheet converter must not run"),
    )
    .unwrap()
    .unwrap();
    assert_eq!(output.extension(), Some(OsStr::new("md")));
    let content = fs::read_to_string(output).unwrap();
    assert!(content.contains("source_type: \"google_workspace\""));
    assert!(content.contains("# Planning"));
}

#[test]
fn test_convert_gsheet_uses_xlsx_markdown_callback() {
    let temp = TempDir::new().unwrap();
    let shortcut = temp.path().join("Budget.gsheet");
    fs::write(&shortcut, r#"{"doc_id":"sheet-456"}"#).unwrap();
    let output = convert_google_workspace_file_with(
        &shortcut,
        &temp.path().join("converted"),
        |file_id, mime_type, output, _| {
            assert_eq!(file_id, "sheet-456");
            assert_eq!(
                mime_type,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            );
            fs::write(output, b"xlsx")?;
            Ok(())
        },
        |path| {
            assert_eq!(fs::read(path)?, b"xlsx");
            Ok("## Sheet: Main\n\n| A |\n| --- |\n| 1 |".to_owned())
        },
    )
    .unwrap()
    .unwrap();
    assert!(fs::read_to_string(output)
        .unwrap()
        .contains("## Sheet: Main"));
}

fn command_arguments(command: &graphoxide_cli::google_workspace::GwsExportCommand) -> Vec<String> {
    command
        .arguments
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn test_run_gws_export_uses_output_directory_as_cwd() {
    let temp = TempDir::new().unwrap();
    let output = temp.path().join("converted/doc.md");
    let command = gws_export_command(
        Path::new("/usr/local/bin/gws"),
        "doc-123",
        "text/markdown",
        &output,
    )
    .unwrap();
    assert_eq!(
        command.cwd,
        output.parent().unwrap().canonicalize().unwrap()
    );
    assert_eq!(command.program, Path::new("/usr/local/bin/gws"));
    let arguments = command_arguments(&command);
    assert_eq!(&arguments[..3], ["drive", "files", "export"]);
    assert_eq!(&arguments[arguments.len() - 2..], ["-o", "doc.md"]);
}

#[test]
fn test_run_gws_export_does_not_send_resource_key_as_query_param() {
    let temp = TempDir::new().unwrap();
    let output = temp.path().join("converted/doc.md");
    let command =
        gws_export_command(Path::new("gws"), "doc-123", "text/markdown", &output).unwrap();
    let arguments = command_arguments(&command);
    let position = arguments
        .iter()
        .position(|value| value == "--params")
        .unwrap();
    let parameters: Value = serde_json::from_str(&arguments[position + 1]).unwrap();
    assert_eq!(
        parameters,
        serde_json::json!({"fileId":"doc-123","mimeType":"text/markdown"})
    );
    assert!(!arguments.iter().any(|value| value.contains("resource")));
}

#[test]
fn test_google_workspace_enabled_env() -> Result<()> {
    assert!(google_workspace_enabled_value(Some("yes")));
    assert!(!google_workspace_enabled_value(Some("0")));
    assert!(!google_workspace_enabled_value(None));
    Ok(())
}
