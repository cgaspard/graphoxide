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
    if report.files_truncated > 0
        || report.boundaries_truncated > 0
        || report.directory_walks_truncated > 0
        || report.ignore_sources_truncated > 0
    {
        writeln!(output, "Omissions:")?;
        if report.files_truncated > 0 {
            writeln!(
                output,
                "- {} file outcome(s) omitted",
                report.files_truncated
            )?;
        }
        if report.boundaries_truncated > 0 {
            writeln!(
                output,
                "- {} boundary outcome(s) omitted",
                report.boundaries_truncated
            )?;
        }
        if report.directory_walks_truncated > 0 {
            writeln!(
                output,
                "- {} directory walk(s) omitted after exceeding the traversal budget",
                report.directory_walks_truncated
            )?;
        }
        if report.ignore_sources_truncated > 0 {
            writeln!(
                output,
                "- {} oversized ignore-policy source(s) rejected",
                report.ignore_sources_truncated
            )?;
        }
    }

    Ok(output.trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphoxide_extract::coverage::{audit_coverage, CoverageOptions};

    #[test]
    fn incomplete_human_report_names_every_truncation_category() {
        let root = tempfile::tempdir().expect("coverage root");
        let mut report = audit_coverage(root.path(), &CoverageOptions::default())
            .expect("baseline coverage report");
        report.complete = false;
        report.files_truncated = 2;
        report.boundaries_truncated = 3;
        report.directory_walks_truncated = 4;
        report.walk_errors_truncated = 5;
        report.ignore_sources_truncated = 6;

        let rendered = render_coverage_report(&report, false).expect("human report");
        assert!(rendered.contains("Status: incomplete"), "{rendered}");
        assert!(rendered.contains("2 file outcome(s) omitted"), "{rendered}");
        assert!(
            rendered.contains("3 boundary outcome(s) omitted"),
            "{rendered}"
        );
        assert!(
            rendered.contains("4 directory walk(s) omitted"),
            "{rendered}"
        );
        assert!(
            rendered.contains("5 additional walk error(s) omitted"),
            "{rendered}"
        );
        assert!(
            rendered.contains("6 oversized ignore-policy source(s) rejected"),
            "{rendered}"
        );
    }

    #[test]
    fn complete_human_report_does_not_claim_any_omissions() {
        let root = tempfile::tempdir().expect("coverage root");
        let report =
            audit_coverage(root.path(), &CoverageOptions::default()).expect("coverage report");
        let rendered = render_coverage_report(&report, false).expect("human report");
        assert!(!rendered.contains("Omissions:"), "{rendered}");
        assert!(!rendered.contains("omitted"), "{rendered}");
    }
}
