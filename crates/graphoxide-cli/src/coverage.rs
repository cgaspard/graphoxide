//! Rendering for the deterministic file-coverage audit.

use graphoxide_extract::coverage::CoverageReport;
use std::fmt::Write as _;

/// Render a coverage report without changing its stable, path-sorted order.
pub fn render_coverage_report(report: &CoverageReport, json: bool) -> anyhow::Result<String> {
    if json {
        return serde_json::to_string_pretty(report).map_err(Into::into);
    }

    let mut output = String::new();
    writeln!(
        output,
        "Coverage audit: {}",
        serde_json::to_string(&report.root)?
    )?;
    writeln!(
        output,
        "Status: {}; schema: coverage/v{}",
        if report.complete {
            "complete"
        } else {
            "incomplete"
        },
        report.schema_version,
    )?;
    writeln!(
        output,
        "Files: {} (covered: {}, inventory-only: {}, unsupported: {}, sensitive: {}, policy-excluded: {}, unreadable: {})",
        report.summary.total_files,
        report.summary.covered,
        report.summary.inventory_only,
        report.summary.unsupported,
        report.summary.excluded_sensitive,
        report.summary.excluded_policy,
        report.summary.unreadable,
    )?;
    writeln!(
        output,
        "Boundaries: {} (ignored: {}, pruned-noise: {}); walk errors: {}",
        report.boundaries.len(),
        report.summary.ignored_boundaries,
        report.summary.pruned_boundaries,
        report.summary.walk_errors,
    )?;
    writeln!(output, "Outcomes:")?;
    for file in &report.files {
        write!(
            output,
            "- {}\tpath={}",
            file.status.as_str(),
            serde_json::to_string(&file.path)?
        )?;
        if let Some(format_id) = &file.format_id {
            write!(output, "\tformat={}", serde_json::to_string(format_id)?)?;
        }
        if let Some(capability) = file.declared_capability {
            write!(output, "\tcapability={}", capability.as_str())?;
        }
        if let Some(reason) = &file.reason {
            write!(output, "\treason={}", serde_json::to_string(reason)?)?;
        }
        writeln!(output)?;
    }
    if !report.boundaries.is_empty() {
        writeln!(output, "Boundaries:")?;
        for boundary in &report.boundaries {
            writeln!(
                output,
                "- {}\tpath={}\treason={}",
                boundary.kind.as_str(),
                serde_json::to_string(&boundary.path)?,
                serde_json::to_string(&boundary.reason)?,
            )?;
        }
    }
    if !report.walk_errors.is_empty() || report.walk_errors_truncated > 0 {
        writeln!(output, "Walk errors:")?;
        for diagnostic in &report.walk_errors {
            writeln!(
                output,
                "- operation={}\tpath={}",
                serde_json::to_string(&diagnostic.operation)?,
                serde_json::to_string(&diagnostic.path)?,
            )?;
        }
        if report.walk_errors_truncated > 0 {
            writeln!(
                output,
                "- {} additional walk error(s) omitted",
                report.walk_errors_truncated
            )?;
        }
    }

    Ok(output.trim_end().to_owned())
}
