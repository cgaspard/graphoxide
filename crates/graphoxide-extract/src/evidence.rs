//! Bounded, byte-only locator candidates for evidence admission.
//!
//! This does not turn extracted facts into claims. It exposes exact locator
//! candidates that an agent may later bind to a captured source.

use anyhow::Result;
use chrono::DateTime;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub const MAX_EVIDENCE_BYTES: usize = 128 * 1024 * 1024;
const PDF_EVIDENCE_PARSER_ALLOWANCE_BYTES: usize = MAX_EVIDENCE_BYTES * 2 + 32 * 1024 * 1024;
const MAX_LOCATORS: usize = 1024;
const TEXT_LOCATOR_TARGET_LINES: u32 = 256;
// Keep exact text ranges below the downstream 64 KiB research-material cap,
// while leaving room for JSON wrapping and provenance metadata.
const MAX_TEXT_LOCATOR_BYTES: usize = 60 * 1024;
const MAX_RENDERED_CAPTURE_PREAMBLE_FIELD_BYTES: usize = 2 * 1024;
const DELIMITED_ROWS_PER_LOCATOR: usize = 128;
const XLSX_ROWS_PER_LOCATOR: u32 = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceExtractionStatus {
    Extracted,
    Partial,
    InventoryOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextLocatorCandidate {
    pub line_start: u32,
    pub line_end: u32,
    /// Deterministic source-role classification, never inferred by authoring.
    pub semantic_role: Option<TextLocatorSemanticRole>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TextLocatorSemanticRole {
    Provenance,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum EvidenceLocatorCandidate {
    Text(TextLocatorCandidate),
    Json {
        pointer: String,
    },
    Pdf {
        page: u32,
    },
    Word {
        heading_path: String,
        paragraph: u32,
    },
    Markdown {
        heading_path: String,
        paragraph: u32,
        byte_start: u64,
        byte_end: u64,
    },
    Yaml {
        path: String,
        document: u32,
    },
    Xml {
        path: String,
    },
    Html {
        heading_path: String,
        paragraph: u32,
    },
    Api {
        pointer: String,
    },
    Archive {
        member: String,
    },
    /// A native partition inside an admitted archive member. The compound
    /// locator retains the complete archive path and leaves inner decoding to
    /// the same bounded adapter used for the standalone format.
    ArchiveChild {
        member: String,
        inner: ArchiveInnerLocatorCandidate,
    },
    Presentation {
        slide: u32,
    },
    Spreadsheet {
        sheet: String,
        cell_range: String,
    },
    Delimited {
        format: String,
        row_start: u32,
        row_end: u32,
    },
    JsonLines {
        record: u32,
    },
    /// The complete intrinsic image frame in source pixels.
    RasterImage {
        /// Always `pixel`; a fixed coordinate system prevents inferred regions.
        coordinate_system: &'static str,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArchiveInnerLocatorCandidate {
    Pdf { page: u32 },
    Spreadsheet { sheet: String, cell_range: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceExtraction {
    pub status: EvidenceExtractionStatus,
    pub locators: Vec<EvidenceLocatorCandidate>,
    /// Bounded, source-free parser diagnostics that explain inventory-only output.
    pub diagnostics: Vec<String>,
    /// Stable blocker retained when this source is incomplete and can be
    /// retried by a declared route.
    pub blocker: Option<String>,
    /// Stable registry route that may reprocess [`Self::blocker`].
    pub retry_route: Option<String>,
}

/// Extract precise locator candidates from ready bytes without filesystem access.
///
/// Registered extraction still runs first, retaining the existing bounded
/// format admission and malformed-input diagnostics. Unsupported representations
/// stay inventory-only until a representation-specific precise locator exists.
pub fn extract_locator_candidates(path: &Path, bytes: &[u8]) -> Result<EvidenceExtraction> {
    if bytes.len() > MAX_EVIDENCE_BYTES {
        return Ok(EvidenceExtraction {
            status: EvidenceExtractionStatus::InventoryOnly,
            locators: Vec::new(),
            diagnostics: vec!["evidence-byte-limit".into()],
            blocker: Some("evidence-byte-limit".into()),
            retry_route: Some("split-source-or-raise-evidence-limit".into()),
        });
    }
    let extraction = if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
    {
        crate::engine::extract_as_bytes_with_parser_allowance(
            path,
            "evidence-source",
            bytes,
            PDF_EVIDENCE_PARSER_ALLOWANCE_BYTES,
        )?
    } else {
        crate::engine::extract_as_bytes(path, "evidence-source", bytes)?
    };
    let diagnostics = extraction_diagnostics(&extraction);
    if let Some(locator) = raster_image_locator_candidate(&extraction) {
        // A parsed raster has one native partition: its complete pixel frame.
        // The registry still blocks semantic image enrichment, but that must
        // not mask exact evidence that the bounded media inspector extracted.
        return Ok(EvidenceExtraction {
            status: EvidenceExtractionStatus::Extracted,
            locators: vec![locator],
            diagnostics,
            blocker: None,
            retry_route: None,
        });
    }
    let reprocessing_route = registered_reprocessing_route(&extraction);
    if let Some((blocker, retry_route)) = crate::format_registry::format_registry()
        .find_by_path(path)
        .and_then(|spec| spec.wiki_processing_contract().blocker())
    {
        // A declared blocker is authoritative for evidence publication. This
        // prevents a named configuration from falling through to generic
        // locators before its redacted material projection exists.
        let mut diagnostics = diagnostics;
        diagnostics.push(blocker.into());
        diagnostics.sort();
        diagnostics.dedup();
        return Ok(EvidenceExtraction {
            status: EvidenceExtractionStatus::InventoryOnly,
            locators: Vec::new(),
            diagnostics,
            blocker: Some(blocker.into()),
            retry_route: Some(retry_route.into()),
        });
    }
    let structured_status = structured_locator_status(&extraction);
    let result = |status, locators: Vec<EvidenceLocatorCandidate>| {
        evidence_extraction_result(
            if status == EvidenceExtractionStatus::Extracted {
                structured_status
            } else {
                status
            },
            locators,
            diagnostics.clone(),
        )
    };
    if let Some((status, locators, archive_diagnostics)) = archive_evidence(&extraction) {
        let mut diagnostics = diagnostics;
        diagnostics.extend(archive_diagnostics);
        diagnostics.sort();
        diagnostics.dedup();
        let locator_limit_reached = locators.len() == MAX_LOCATORS;
        let mut report = evidence_extraction_result_with_locator_limit(
            status,
            locators,
            diagnostics,
            locator_limit_reached,
        );
        if let (Some(blocker), Some(retry_route)) = reprocessing_route {
            report.status = EvidenceExtractionStatus::Partial;
            report.blocker = Some(blocker);
            report.retry_route = Some(retry_route);
        }
        return Ok(report);
    }
    if reprocessing_route.0.is_some() {
        return Ok(EvidenceExtraction {
            status: EvidenceExtractionStatus::InventoryOnly,
            locators: Vec::new(),
            diagnostics,
            blocker: reprocessing_route.0,
            retry_route: reprocessing_route.1,
        });
    }
    if let Some(diagnostic) = partial_pdf_coverage_diagnostic(&extraction) {
        let mut diagnostics = diagnostics;
        diagnostics.push(diagnostic.into());
        diagnostics.sort();
        diagnostics.dedup();
        return Ok(evidence_extraction_result(
            EvidenceExtractionStatus::Partial,
            pdf_locator_candidates(&extraction),
            diagnostics,
        ));
    }
    let locators = pdf_locator_candidates(&extraction);
    if !locators.is_empty() {
        return Ok(result(EvidenceExtractionStatus::Extracted, locators));
    }
    let (locators, locator_limit_reached) = spreadsheet_locator_candidates_with_limit(&extraction);
    if !locators.is_empty() {
        let mut spreadsheet_diagnostics = diagnostics.clone();
        let (mut status, office_diagnostics) = office_locator_status(&extraction, "xlsx_workbook")
            .or_else(|| office_locator_status(&extraction, "ods_workbook"))
            .unwrap_or((EvidenceExtractionStatus::Extracted, Vec::new()));
        spreadsheet_diagnostics.extend(office_diagnostics);
        if locator_limit_reached {
            spreadsheet_diagnostics.push("locator-limit".into());
            if status == EvidenceExtractionStatus::Extracted {
                status = EvidenceExtractionStatus::Partial;
            }
        }
        spreadsheet_diagnostics.sort();
        spreadsheet_diagnostics.dedup();
        return Ok(evidence_extraction_result_with_locator_limit(
            status,
            locators,
            spreadsheet_diagnostics,
            locator_limit_reached,
        ));
    }
    let locators = word_locator_candidates(&extraction);
    if !locators.is_empty() {
        let (status, office_diagnostics) = office_locator_status(&extraction, "docx_document")
            .unwrap_or((EvidenceExtractionStatus::Extracted, Vec::new()));
        let mut word_diagnostics = diagnostics.clone();
        word_diagnostics.extend(office_diagnostics);
        word_diagnostics.sort();
        word_diagnostics.dedup();
        let locator_limit_reached = locators.len() == MAX_LOCATORS;
        return Ok(evidence_extraction_result_with_locator_limit(
            status,
            locators,
            word_diagnostics,
            locator_limit_reached,
        ));
    }
    let locators = delimited_locator_candidates(&extraction);
    if !locators.is_empty() {
        return Ok(result(EvidenceExtractionStatus::Extracted, locators));
    }
    let locators = json_lines_locator_candidates(&extraction);
    if !locators.is_empty() {
        return Ok(result(EvidenceExtractionStatus::Extracted, locators));
    }
    let locators = markdown_locator_candidates(path, bytes);
    if !locators.is_empty() {
        return Ok(result(EvidenceExtractionStatus::Extracted, locators));
    }
    let locators = yaml_locator_candidates(&extraction);
    if !locators.is_empty() {
        return Ok(result(EvidenceExtractionStatus::Extracted, locators));
    }
    let locators = xml_locator_candidates(&extraction);
    if !locators.is_empty() {
        return Ok(result(EvidenceExtractionStatus::Extracted, locators));
    }
    let locators = html_locator_candidates(&extraction);
    if !locators.is_empty() {
        return Ok(result(EvidenceExtractionStatus::Extracted, locators));
    }
    let locators = presentation_locator_candidates(&extraction);
    if !locators.is_empty() {
        return Ok(result(EvidenceExtractionStatus::Extracted, locators));
    }
    if let Ok(value) = serde_json::from_slice(bytes) {
        let locators = api_locator_candidates(&value);
        if !locators.is_empty() {
            return Ok(result(EvidenceExtractionStatus::Extracted, locators));
        }
        let mut locators = Vec::new();
        let overflowed = json_locator_candidates_with_overflow(&value, "", &mut locators);
        if !locators.is_empty() {
            if overflowed && let Some(ranges) = complete_text_line_locators(bytes) {
                return Ok(result(EvidenceExtractionStatus::Extracted, ranges));
            }
            return Ok(result(EvidenceExtractionStatus::Extracted, locators));
        }
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Ok(result(EvidenceExtractionStatus::InventoryOnly, Vec::new()));
    };
    if let Some(locators) = heading_organized_text_line_locators(bytes) {
        return Ok(result(EvidenceExtractionStatus::Extracted, locators));
    }
    let line_count = u32::try_from(text.lines().count()).unwrap_or(u32::MAX);
    if line_count == 0 {
        return Ok(result(EvidenceExtractionStatus::InventoryOnly, Vec::new()));
    }
    let target_parts = line_count.div_ceil(TEXT_LOCATOR_TARGET_LINES) as usize;
    let parts = target_parts.clamp(1, MAX_LOCATORS);
    let lines_per_locator = line_count.div_ceil(u32::try_from(parts).expect("bounded parts"));
    let mut lines = BTreeSet::new();
    for index in 0..parts {
        let offset = u32::try_from(index).expect("bounded locator index") * lines_per_locator;
        let line_start = offset.saturating_add(1);
        if line_start > line_count {
            break;
        }
        lines.insert(TextLocatorCandidate {
            line_start,
            line_end: offset.saturating_add(lines_per_locator).min(line_count),
            semantic_role: None,
        });
    }
    let locator_limit_reached = target_parts > MAX_LOCATORS;
    let status = if locator_limit_reached {
        EvidenceExtractionStatus::Partial
    } else {
        EvidenceExtractionStatus::Extracted
    };
    Ok(evidence_extraction_result_with_locator_limit(
        status,
        lines
            .into_iter()
            .take(MAX_LOCATORS)
            .map(EvidenceLocatorCandidate::Text)
            .collect(),
        diagnostics,
        locator_limit_reached,
    ))
}

fn evidence_extraction_result(
    status: EvidenceExtractionStatus,
    locators: Vec<EvidenceLocatorCandidate>,
    diagnostics: Vec<String>,
) -> EvidenceExtraction {
    // Candidate collectors are bounded at `MAX_LOCATORS`. They deliberately
    // retain no unbounded overflow buffer, so a collector that reaches that
    // ceiling must be treated as incomplete until a later run supplies every
    // remaining native partition. This conservative shared guard applies to
    // every format-specific collector that returns only a capped Vec.
    let locator_limit_reached = locators.len() == MAX_LOCATORS;
    evidence_extraction_result_with_locator_limit(
        status,
        locators,
        diagnostics,
        locator_limit_reached,
    )
}

fn evidence_extraction_result_with_locator_limit(
    status: EvidenceExtractionStatus,
    locators: Vec<EvidenceLocatorCandidate>,
    mut diagnostics: Vec<String>,
    locator_limit_reached: bool,
) -> EvidenceExtraction {
    if locator_limit_reached {
        diagnostics.push("locator-limit".into());
    }
    diagnostics.sort();
    diagnostics.dedup();
    let status = if locator_limit_reached {
        EvidenceExtractionStatus::Partial
    } else {
        status
    };
    let (blocker, retry_route) = match status {
        EvidenceExtractionStatus::Extracted => (None, None),
        EvidenceExtractionStatus::Partial => reprocessing_route(&diagnostics)
            .map(|(blocker, retry_route)| (Some(blocker.into()), Some(retry_route.into())))
            .unwrap_or((None, None)),
        // Inventory-only means no evidence locator was produced. Keep it
        // explicitly reprocessable instead of allowing callers to mistake a
        // parser rejection for a successful complete extraction.
        EvidenceExtractionStatus::InventoryOnly => reprocessing_route(&diagnostics)
            .map(|(blocker, retry_route)| (Some(blocker.into()), Some(retry_route.into())))
            .unwrap_or((
                Some("evidence-locator-extraction-unavailable".into()),
                Some("add-format-adapter-or-enrichment".into()),
            )),
    };
    EvidenceExtraction {
        status,
        locators,
        diagnostics,
        blocker,
        retry_route,
    }
}

fn reprocessing_route(diagnostics: &[String]) -> Option<(&'static str, &'static str)> {
    diagnostics
        .iter()
        .find_map(|diagnostic| match diagnostic.as_str() {
            "archive-omitted-members" => {
                Some(("archive-omitted-members", "raise-archive-member-limit"))
            }
            "archive-recursion-limit" => {
                Some(("archive-recursion-limit", "raise-archive-recursion-limit"))
            }
            "archive-recursive-dispatch-limit" => Some((
                "archive-recursive-dispatch-limit",
                "raise-archive-dispatch-limit",
            )),
            "archive-sensitive-members" => Some((
                "archive-sensitive-members",
                "authorize-sensitive-member-processing",
            )),
            "membersizelimit" => Some((
                "archive-member-size-limit",
                "raise-archive-member-size-limit",
            )),
            "office-fact-limit" => Some(("office-fact-limit", "raise-office-fact-limit")),
            "office-partial" => Some(("office-partial", "retry-office-extraction")),
            "pdf-embedded-attachments" => {
                Some(("pdf-embedded-attachments", "extract-pdf-attachments"))
            }
            "pdf-attachment-unreadable" => {
                Some(("pdf-attachment-unreadable", "repair-pdf-attachment"))
            }
            "pdf-attachment-byte-limit" => Some((
                "pdf-attachment-byte-limit",
                "raise-pdf-attachment-byte-limit",
            )),
            "pdf-attachment-count-limit" => Some((
                "pdf-attachment-count-limit",
                "raise-pdf-attachment-count-limit",
            )),
            "pdf-attachment-depth-limit" => Some((
                "pdf-attachment-depth-limit",
                "raise-pdf-attachment-depth-limit",
            )),
            "pdf-attachment-dispatch-limit" => Some((
                "pdf-attachment-dispatch-limit",
                "raise-container-dispatch-budget",
            )),
            "pdf-attachment-sensitive-path" => Some((
                "pdf-attachment-sensitive-path",
                "authorize-sensitive-member-processing",
            )),
            "pdf-native-partitions-incomplete" => {
                Some(("pdf-native-partitions-incomplete", "retry-pdf-extraction"))
            }
            "csv_parse_error" | "json_lines_parse_error" => {
                Some(("structured-source-parse-error", "repair-structured-source"))
            }
            "depth_limit" | "fact_limit" | "field_limit" | "input_too_large" | "path_limit"
            | "row_limit" | "scalar_limit" => Some((
                "structured-extraction-limit",
                "raise-structured-extraction-limit",
            )),
            "locator-limit" => Some(("locator-limit", "raise-locator-limit")),
            _ => None,
        })
}

fn registered_reprocessing_route(
    extraction: &graphoxide_core::Extraction,
) -> (Option<String>, Option<String>) {
    let routes = extraction
        .nodes
        .iter()
        .filter_map(|node| {
            let blocker = node.extra.get("blocker").and_then(Value::as_str)?;
            let retry_route = node.extra.get("retry_route").and_then(Value::as_str)?;
            (is_safe_reprocessing_token(blocker) && is_safe_reprocessing_token(retry_route))
                .then(|| (blocker.to_owned(), retry_route.to_owned()))
        })
        .collect::<BTreeSet<_>>();
    if routes.len() == 1 {
        let (blocker, retry_route) = routes
            .into_iter()
            .next()
            .expect("one registered reprocessing route");
        (Some(blocker), Some(retry_route))
    } else {
        (None, None)
    }
}

fn extraction_diagnostics(extraction: &graphoxide_core::Extraction) -> Vec<String> {
    extraction
        .nodes
        .iter()
        .flat_map(|node| {
            node.extra
                .get("diagnostic")
                .and_then(Value::as_str)
                .into_iter()
                .chain(
                    node.extra
                        .get("structured_diagnostics")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|diagnostic| diagnostic.get("code")?.as_str()),
                )
        })
        .filter(|diagnostic| is_safe_diagnostic(diagnostic))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(16)
        .map(str::to_owned)
        .collect()
}

fn structured_locator_status(extraction: &graphoxide_core::Extraction) -> EvidenceExtractionStatus {
    let omitted_material = extraction.nodes.iter().any(|node| {
        node.extra.get("type").and_then(Value::as_str) == Some("structured_file")
            && matches!(
                node.extra.get("structured_format").and_then(Value::as_str),
                Some("csv" | "tsv" | "psv" | "json_lines")
            )
            && (node.extra.get("parse_status").and_then(Value::as_str) == Some("partial")
                || node
                    .extra
                    .get("structured_diagnostics")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|diagnostic| diagnostic.get("code")?.as_str())
                    .any(structured_diagnostic_omits_material))
    });
    if omitted_material {
        EvidenceExtractionStatus::Partial
    } else {
        EvidenceExtractionStatus::Extracted
    }
}

fn structured_diagnostic_omits_material(code: &str) -> bool {
    matches!(
        code,
        "csv_parse_error"
            | "depth_limit"
            | "fact_limit"
            | "field_limit"
            | "input_too_large"
            | "json_lines_parse_error"
            | "path_limit"
            | "row_limit"
            | "scalar_limit"
    )
}

fn is_safe_diagnostic(diagnostic: &str) -> bool {
    !diagnostic.is_empty()
        && diagnostic.len() <= 128
        && diagnostic
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn is_safe_reprocessing_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn pdf_locator_candidates(
    extraction: &graphoxide_core::Extraction,
) -> Vec<EvidenceLocatorCandidate> {
    let mut pages = extraction
        .nodes
        .iter()
        .filter(|node| node.extra.get("type").and_then(Value::as_str) == Some("pdf_page"))
        .filter_map(|node| {
            node.extra
                .get("page_number")
                .and_then(Value::as_u64)
                .and_then(|page| u32::try_from(page).ok())
                .filter(|page| *page > 0)
        })
        .collect::<Vec<_>>();
    pages.sort_unstable();
    pages.dedup();
    pages
        .into_iter()
        .take(MAX_LOCATORS)
        .map(|page| EvidenceLocatorCandidate::Pdf { page })
        .collect()
}

fn top_level_pdf_locator_candidates(
    extraction: &graphoxide_core::Extraction,
) -> Vec<EvidenceLocatorCandidate> {
    let mut pages = extraction
        .nodes
        .iter()
        .filter(|node| !node.source_file.contains("!/"))
        .filter(|node| node.extra.get("type").and_then(Value::as_str) == Some("pdf_page"))
        .filter_map(|node| {
            node.extra
                .get("page_number")
                .and_then(Value::as_u64)
                .and_then(|page| u32::try_from(page).ok())
                .filter(|page| *page > 0)
        })
        .collect::<Vec<_>>();
    pages.sort_unstable();
    pages.dedup();
    pages
        .into_iter()
        .take(MAX_LOCATORS)
        .map(|page| EvidenceLocatorCandidate::Pdf { page })
        .collect()
}

fn partial_pdf_coverage_diagnostic(
    extraction: &graphoxide_core::Extraction,
) -> Option<&'static str> {
    extraction
        .nodes
        .iter()
        .find(|node| {
            node.extra.get("type").and_then(Value::as_str) == Some("pdf_document")
                && node.extra.get("parse_status").and_then(Value::as_str) == Some("partial")
        })
        .map(|node| {
            if node
                .extra
                .get("ignored_pdf_features")
                .and_then(Value::as_str)
                .is_some_and(|features| {
                    features
                        .split(',')
                        .any(|feature| feature.trim() == "embedded_files")
                })
            {
                "pdf-embedded-attachments"
            } else {
                "pdf-native-partitions-incomplete"
            }
        })
}

fn raster_image_locator_candidate(
    extraction: &graphoxide_core::Extraction,
) -> Option<EvidenceLocatorCandidate> {
    let mut dimensions = extraction
        .nodes
        .iter()
        .filter(|node| {
            matches!(
                node.extra.get("format").and_then(Value::as_str),
                Some("png" | "jpeg")
            ) && node.extra.get("inspection_status").and_then(Value::as_str) == Some("parsed")
        })
        .filter_map(|node| {
            let width = node
                .extra
                .get("width")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value > 0)?;
            let height = node
                .extra
                .get("height")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value > 0)?;
            Some((width, height))
        })
        .collect::<Vec<_>>();
    dimensions.sort_unstable();
    dimensions.dedup();
    let [(width, height)] = dimensions.as_slice() else {
        return None;
    };
    Some(EvidenceLocatorCandidate::RasterImage {
        coordinate_system: "pixel",
        x: 0,
        y: 0,
        width: *width,
        height: *height,
    })
}

fn bounded_archive_locator_insert(
    locators: &mut BTreeSet<EvidenceLocatorCandidate>,
    locator: EvidenceLocatorCandidate,
    overflowed: &mut bool,
) {
    locators.insert(locator);
    if locators.len() > MAX_LOCATORS {
        *overflowed = true;
        locators.pop_last();
    }
}

fn archive_locator_candidates_with_limit(
    extraction: &graphoxide_core::Extraction,
) -> (Vec<EvidenceLocatorCandidate>, bool) {
    let mut locators = BTreeSet::new();
    let mut overflowed = false;
    for node in extraction.nodes.iter().filter(|node| {
        node.extra.get("type").and_then(Value::as_str) == Some("container_member")
            && node.extra.get("member_kind").and_then(Value::as_str) != Some("directory")
            && !node.label.is_empty()
    }) {
        let member = node.source_file.split_once("!/").map_or_else(
            || node.label.clone(),
            |(_, parent)| format!("{parent}!/{}", node.label),
        );
        bounded_archive_locator_insert(
            &mut locators,
            EvidenceLocatorCandidate::Archive { member },
            &mut overflowed,
        );
    }
    (locators.into_iter().collect(), overflowed)
}

fn archive_child_locator_candidates(
    extraction: &graphoxide_core::Extraction,
) -> (Vec<EvidenceLocatorCandidate>, bool) {
    let mut locators = BTreeSet::new();
    let mut overflowed = false;
    for node in &extraction.nodes {
        let Some((_, member)) = node.source_file.split_once("!/") else {
            continue;
        };
        let member = member.to_owned();
        if node.extra.get("type").and_then(Value::as_str) == Some("pdf_page")
            && let Some(page) = node
                .extra
                .get("page_number")
                .and_then(Value::as_u64)
                .and_then(|page| u32::try_from(page).ok())
                .filter(|page| *page > 0)
        {
            bounded_archive_locator_insert(
                &mut locators,
                EvidenceLocatorCandidate::ArchiveChild {
                    member,
                    inner: ArchiveInnerLocatorCandidate::Pdf { page },
                },
                &mut overflowed,
            );
        }
    }

    let mut ranges = BTreeMap::<(String, String, u32), (u32, u32, u32, u32)>::new();
    for node in &extraction.nodes {
        if node.extra.get("type").and_then(Value::as_str) != Some("workbook_sheet")
            || !matches!(
                node.extra.get("format").and_then(Value::as_str),
                Some("xlsx" | "ods")
            )
        {
            continue;
        }
        let Some((_, member)) = node.source_file.split_once("!/") else {
            continue;
        };
        let Some(text) = node.extra.get("text").and_then(Value::as_str) else {
            continue;
        };
        for line in text.lines() {
            let Some((cell, _)) = line.split_once(':') else {
                continue;
            };
            let Some((column, row)) = xlsx_cell_coordinates(cell) else {
                continue;
            };
            let row_partition = (row - 1) / XLSX_ROWS_PER_LOCATOR;
            let range = ranges
                .entry((member.to_owned(), node.label.clone(), row_partition))
                .or_insert((column, row, column, row));
            range.0 = range.0.min(column);
            range.1 = range.1.min(row);
            range.2 = range.2.max(column);
            range.3 = range.3.max(row);
        }
    }
    for ((member, sheet, _), (column_start, row_start, column_end, row_end)) in ranges {
        bounded_archive_locator_insert(
            &mut locators,
            EvidenceLocatorCandidate::ArchiveChild {
                member,
                inner: ArchiveInnerLocatorCandidate::Spreadsheet {
                    sheet,
                    cell_range: xlsx_cell_range(column_start, row_start, column_end, row_end),
                },
            },
            &mut overflowed,
        );
    }
    (locators.into_iter().collect(), overflowed)
}

fn archive_evidence(
    extraction: &graphoxide_core::Extraction,
) -> Option<(
    EvidenceExtractionStatus,
    Vec<EvidenceLocatorCandidate>,
    Vec<String>,
)> {
    let roots = extraction
        .nodes
        .iter()
        .filter(|node| node.extra.get("type").and_then(Value::as_str) == Some("container"))
        .collect::<Vec<_>>();
    let pdf_attachments = extraction
        .nodes
        .iter()
        .filter(|node| {
            node.extra.get("type").and_then(Value::as_str) == Some("container_member")
                && node.extra.get("member_kind").and_then(Value::as_str) == Some("pdf_attachment")
        })
        .collect::<Vec<_>>();
    if roots.is_empty() && pdf_attachments.is_empty() {
        return None;
    }
    let top_level = top_level_pdf_locator_candidates(extraction);
    let (archive_locators, archive_overflowed) = archive_locator_candidates_with_limit(extraction);
    let (archive_children, child_overflowed) = archive_child_locator_candidates(extraction);
    let mut locators = BTreeSet::new();
    let mut locator_limit_reached =
        top_level.len() == MAX_LOCATORS || archive_overflowed || child_overflowed;
    for locator in top_level
        .into_iter()
        .chain(archive_locators)
        .chain(archive_children)
    {
        bounded_archive_locator_insert(&mut locators, locator, &mut locator_limit_reached);
    }
    let locators = locators.into_iter().collect::<Vec<_>>();

    let mut blockers = BTreeSet::new();
    if locator_limit_reached {
        blockers.insert("locator-limit");
    }
    blockers.extend(pdf_attachments.iter().filter_map(|node| {
        let blocker = node.extra.get("blocker").and_then(Value::as_str)?;
        let retry_route = node.extra.get("retry_route").and_then(Value::as_str)?;
        (is_safe_reprocessing_token(blocker) && is_safe_reprocessing_token(retry_route))
            .then_some(blocker)
    }));
    let mut inventory_only = false;
    for root in roots {
        let diagnostics = root
            .extra
            .get("diagnostics")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<BTreeSet<_>>();
        if root.extra.get("inspection_status").and_then(Value::as_str) == Some("rejected")
            || diagnostics.contains("unsupportedarchiveformat")
        {
            return Some((
                EvidenceExtractionStatus::InventoryOnly,
                locators,
                diagnostics.into_iter().map(str::to_owned).collect(),
            ));
        }
        if root
            .extra
            .get("omitted_member_count")
            .and_then(Value::as_u64)
            > Some(0)
        {
            blockers.insert("archive-omitted-members");
        }
        if root
            .extra
            .get("recursive_dispatch_status")
            .and_then(Value::as_str)
            .is_some_and(|status| status == "recursion_limit")
            || diagnostics.contains("recursionlimit")
        {
            blockers.insert("archive-recursion-limit");
        }
        if root
            .extra
            .get("recursive_dispatch_status")
            .and_then(Value::as_str)
            .is_some_and(|status| status != "recursion_limit")
            || diagnostics.contains("nesteddispatchstopped")
        {
            blockers.insert("archive-recursive-dispatch-limit");
        }
        if root
            .extra
            .get("sensitive_member_count")
            .and_then(Value::as_u64)
            > Some(0)
        {
            blockers.insert("archive-sensitive-members");
        }
        inventory_only |=
            root.extra.get("inspection_status").and_then(Value::as_str) != Some("parsed");
    }

    let status = if !blockers.is_empty() {
        EvidenceExtractionStatus::Partial
    } else if inventory_only {
        EvidenceExtractionStatus::InventoryOnly
    } else {
        EvidenceExtractionStatus::Extracted
    };
    Some((
        status,
        locators,
        blockers.into_iter().map(str::to_owned).collect(),
    ))
}

fn presentation_locator_candidates(
    extraction: &graphoxide_core::Extraction,
) -> Vec<EvidenceLocatorCandidate> {
    let mut slides = extraction
        .nodes
        .iter()
        .filter(|node| node.extra.get("type").and_then(Value::as_str) == Some("presentation_slide"))
        .filter_map(|node| {
            node.extra
                .get("unit_ordinal")
                .and_then(Value::as_u64)
                .and_then(|slide| u32::try_from(slide).ok())
                .filter(|slide| *slide > 0)
        })
        .collect::<Vec<_>>();
    slides.sort_unstable();
    slides.dedup();
    slides
        .into_iter()
        .take(MAX_LOCATORS)
        .map(|slide| EvidenceLocatorCandidate::Presentation { slide })
        .collect()
}

fn spreadsheet_locator_candidates_with_limit(
    extraction: &graphoxide_core::Extraction,
) -> (Vec<EvidenceLocatorCandidate>, bool) {
    let mut ranges = BTreeMap::<(String, u32), (u32, u32, u32, u32)>::new();
    for node in &extraction.nodes {
        if node.extra.get("type").and_then(Value::as_str) != Some("workbook_sheet")
            || !matches!(
                node.extra.get("format").and_then(Value::as_str),
                Some("xlsx" | "ods")
            )
        {
            continue;
        }
        let Some(text) = node.extra.get("text").and_then(Value::as_str) else {
            continue;
        };
        for line in text.lines() {
            let Some((cell, _)) = line.split_once(':') else {
                continue;
            };
            let Some((column, row)) = xlsx_cell_coordinates(cell) else {
                continue;
            };
            let row_partition = (row - 1) / XLSX_ROWS_PER_LOCATOR;
            let range = ranges
                .entry((node.label.clone(), row_partition))
                .or_insert((column, row, column, row));
            range.0 = range.0.min(column);
            range.1 = range.1.min(row);
            range.2 = range.2.max(column);
            range.3 = range.3.max(row);
        }
    }
    let locator_limit_reached = ranges.len() > MAX_LOCATORS;
    let locators = ranges
        .into_iter()
        .take(MAX_LOCATORS)
        .map(
            |((sheet, _), (column_start, row_start, column_end, row_end))| {
                EvidenceLocatorCandidate::Spreadsheet {
                    sheet,
                    cell_range: xlsx_cell_range(column_start, row_start, column_end, row_end),
                }
            },
        )
        .collect();
    (locators, locator_limit_reached)
}

fn delimited_locator_candidates(
    extraction: &graphoxide_core::Extraction,
) -> Vec<EvidenceLocatorCandidate> {
    let mut rows = BTreeMap::<String, BTreeSet<usize>>::new();
    for node in &extraction.nodes {
        let Some(format) = node
            .extra
            .get("structured_format")
            .and_then(Value::as_str)
            .filter(|format| matches!(*format, "csv" | "tsv" | "psv"))
        else {
            continue;
        };
        if node.extra.get("type").and_then(Value::as_str) != Some("table_row") {
            continue;
        }
        let Some(row) = node
            .extra
            .get("structured_path")
            .and_then(Value::as_str)
            .and_then(delimited_row_index)
        else {
            continue;
        };
        rows.entry(format.to_owned()).or_default().insert(row);
    }
    let mut locators = Vec::new();
    for (format, rows) in rows {
        let rows = rows.into_iter().collect::<Vec<_>>();
        for chunk in rows.chunks(DELIMITED_ROWS_PER_LOCATOR) {
            let (Some(first), Some(last)) = (chunk.first(), chunk.last()) else {
                continue;
            };
            let (Ok(row_start), Ok(row_end)) = (u32::try_from(*first), u32::try_from(*last)) else {
                continue;
            };
            locators.push(EvidenceLocatorCandidate::Delimited {
                format: format.clone(),
                row_start,
                row_end,
            });
            if locators.len() == MAX_LOCATORS {
                return locators;
            }
        }
    }
    locators
}

fn json_lines_locator_candidates(
    extraction: &graphoxide_core::Extraction,
) -> Vec<EvidenceLocatorCandidate> {
    extraction
        .nodes
        .iter()
        .filter(|node| {
            node.extra.get("structured_format").and_then(Value::as_str) == Some("json_lines")
                && node.extra.get("type").and_then(Value::as_str) == Some("json_record")
        })
        .filter_map(|node| {
            node.extra
                .get("structured_path")
                .and_then(Value::as_str)
                .and_then(|path| path.strip_prefix("$[")?.strip_suffix(']')?.parse().ok())
                .and_then(|record: usize| u32::try_from(record).ok())
                .and_then(|record| record.checked_add(1))
                .map(|record| EvidenceLocatorCandidate::JsonLines { record })
        })
        .take(MAX_LOCATORS)
        .collect()
}

fn delimited_row_index(path: &str) -> Option<usize> {
    path.strip_prefix("$rows[")?.strip_suffix(']')?.parse().ok()
}

fn word_locator_candidates(
    extraction: &graphoxide_core::Extraction,
) -> Vec<EvidenceLocatorCandidate> {
    let mut paragraphs = BTreeSet::new();
    for node in &extraction.nodes {
        if node.extra.get("type").and_then(Value::as_str) != Some("document_section")
            || node.extra.get("format").and_then(Value::as_str) != Some("docx")
        {
            continue;
        }
        let Some(entries) = node.extra.get("word_paragraphs").and_then(Value::as_array) else {
            continue;
        };
        for entry in entries {
            let (Some(heading_path), Some(paragraph)) = (
                entry.get("heading_path").and_then(Value::as_str),
                entry
                    .get("paragraph")
                    .and_then(Value::as_u64)
                    .and_then(|paragraph| u32::try_from(paragraph).ok())
                    .filter(|paragraph| *paragraph > 0),
            ) else {
                continue;
            };
            if !heading_path.is_empty() {
                paragraphs.insert((paragraph, heading_path.to_owned()));
            }
        }
    }
    paragraphs
        .into_iter()
        .take(MAX_LOCATORS)
        .map(|(paragraph, heading_path)| EvidenceLocatorCandidate::Word {
            heading_path,
            paragraph,
        })
        .collect()
}

fn office_locator_status(
    extraction: &graphoxide_core::Extraction,
    document_type: &str,
) -> Option<(EvidenceExtractionStatus, Vec<String>)> {
    let roots = extraction
        .nodes
        .iter()
        .filter(|node| node.extra.get("type").and_then(Value::as_str) == Some(document_type))
        .collect::<Vec<_>>();
    if roots.is_empty() {
        return None;
    }

    let mut blockers = BTreeSet::new();
    let mut inventory_only = false;
    for root in roots {
        let parse_status = root.extra.get("parse_status").and_then(Value::as_str);
        let coverage_blocker = root
            .extra
            .get("coverage_blocker")
            .and_then(Value::as_str)
            .filter(|blocker| !blocker.is_empty());
        if let Some(coverage_blocker) = coverage_blocker {
            blockers.insert(if coverage_blocker == "office_fact_limit" {
                "office-fact-limit"
            } else {
                "office-partial"
            });
        } else if parse_status == Some("partial") {
            blockers.insert("office-partial");
        } else if parse_status != Some("complete") {
            inventory_only = true;
        }
    }

    let status = if inventory_only {
        EvidenceExtractionStatus::InventoryOnly
    } else if !blockers.is_empty() {
        EvidenceExtractionStatus::Partial
    } else {
        EvidenceExtractionStatus::Extracted
    };
    Some((status, blockers.into_iter().map(str::to_owned).collect()))
}

fn markdown_locator_candidates(path: &Path, bytes: &[u8]) -> Vec<EvidenceLocatorCandidate> {
    let is_markdown = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("md")
                || extension.eq_ignore_ascii_case("markdown")
                || extension.eq_ignore_ascii_case("mdx")
                || extension.eq_ignore_ascii_case("qmd")
        });
    if !is_markdown {
        return Vec::new();
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    let mut locators = Vec::new();
    let mut heading_stack = Vec::<(u8, &str)>::new();
    let mut fence = None::<(u8, usize)>;
    let mut paragraph = 0_u32;
    let mut active = None;
    let mut offset = 0_usize;
    for line_with_ending in text.split_inclusive('\n') {
        let line = line_with_ending.trim_end_matches(['\r', '\n']);
        let line_end = offset + line.len();
        if let Some((marker, width)) = fence {
            if let Some((_, end)) = active.as_mut() {
                *end = line_end;
            }
            if markdown_fence_closes(line, marker, width) {
                finish_markdown_paragraph(
                    &mut active,
                    &heading_stack,
                    &mut paragraph,
                    &mut locators,
                );
                fence = None;
            }
        } else if let Some(fence_marker) = markdown_fence_opener(line) {
            finish_markdown_paragraph(&mut active, &heading_stack, &mut paragraph, &mut locators);
            active = Some((offset, line_end));
            fence = Some(fence_marker);
        } else {
            if let Some((level, title)) = markdown_heading(line) {
                finish_markdown_paragraph(
                    &mut active,
                    &heading_stack,
                    &mut paragraph,
                    &mut locators,
                );
                heading_stack.retain(|(current, _)| *current < level);
                heading_stack.push((level, title));
            } else if line.trim().is_empty() {
                finish_markdown_paragraph(
                    &mut active,
                    &heading_stack,
                    &mut paragraph,
                    &mut locators,
                );
            } else if active.is_none() {
                active = Some((offset, line_end));
            } else if let Some((_, end)) = active.as_mut() {
                *end = line_end;
            }
        }
        offset += line_with_ending.len();
    }
    finish_markdown_paragraph(&mut active, &heading_stack, &mut paragraph, &mut locators);
    locators
}

fn markdown_fence_opener(line: &str) -> Option<(u8, usize)> {
    let indent = line.bytes().take_while(|byte| *byte == b' ').count();
    if indent > 3 {
        return None;
    }
    let line = &line[indent..];
    let marker = *line.as_bytes().first()?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let width = line.bytes().take_while(|byte| *byte == marker).count();
    if width < 3 || (marker == b'`' && line[width..].contains('`')) {
        return None;
    }
    Some((marker, width))
}

fn markdown_fence_closes(line: &str, marker: u8, width: usize) -> bool {
    let indent = line.bytes().take_while(|byte| *byte == b' ').count();
    if indent > 3 {
        return false;
    }
    let line = &line[indent..];
    let fence_width = line.bytes().take_while(|byte| *byte == marker).count();
    fence_width >= width
        && line[fence_width..]
            .bytes()
            .all(|byte| matches!(byte, b' ' | b'\t'))
}

fn yaml_locator_candidates(
    extraction: &graphoxide_core::Extraction,
) -> Vec<EvidenceLocatorCandidate> {
    let mut paths = BTreeSet::new();
    for node in &extraction.nodes {
        if node.extra.get("structured_format").and_then(Value::as_str) != Some("yaml_structural") {
            continue;
        }
        let Some(path) = node.extra.get("structured_path").and_then(Value::as_str) else {
            continue;
        };
        let Some((document, suffix)) = yaml_document_path(path) else {
            continue;
        };
        paths.insert((document, suffix));
    }
    paths
        .into_iter()
        .take(MAX_LOCATORS)
        .map(|(document, path)| EvidenceLocatorCandidate::Yaml { path, document })
        .collect()
}

fn yaml_document_path(path: &str) -> Option<(u32, String)> {
    let suffix = path.strip_prefix("$doc")?;
    let digits = suffix.bytes().take_while(u8::is_ascii_digit).count();
    let document = suffix.get(..digits)?.parse::<u32>().ok()?.checked_add(1)?;
    let suffix = suffix.get(digits..)?;
    (!suffix.is_empty()).then(|| (document, format!("${suffix}")))
}

fn xml_locator_candidates(
    extraction: &graphoxide_core::Extraction,
) -> Vec<EvidenceLocatorCandidate> {
    let mut paths = BTreeSet::new();
    for node in &extraction.nodes {
        if node.extra.get("structured_format").and_then(Value::as_str) != Some("xml") {
            continue;
        }
        if let Some(path) = node.extra.get("structured_path").and_then(Value::as_str) {
            paths.insert(path.to_owned());
        }
    }
    paths
        .into_iter()
        .take(MAX_LOCATORS)
        .map(|path| EvidenceLocatorCandidate::Xml { path })
        .collect()
}

fn html_locator_candidates(
    extraction: &graphoxide_core::Extraction,
) -> Vec<EvidenceLocatorCandidate> {
    let headings = extraction
        .nodes
        .iter()
        .filter(|node| {
            node.extra.get("structured_format").and_then(Value::as_str) == Some("html")
                && node.extra.get("type").and_then(Value::as_str) == Some("document_heading")
        })
        .map(|node| (node.id.as_str(), node.label.as_str()))
        .collect::<BTreeMap<_, _>>();
    let parents = extraction
        .edges
        .iter()
        .filter(|edge| edge.relation == "contains")
        .map(|edge| (edge.target.as_str(), edge.source.as_str()))
        .collect::<BTreeMap<_, _>>();
    let mut blocks = extraction
        .nodes
        .iter()
        .filter(|node| {
            node.extra.get("structured_format").and_then(Value::as_str) == Some("html")
                && matches!(
                    node.extra.get("type").and_then(Value::as_str),
                    Some(
                        "document_paragraph"
                            | "document_list_item"
                            | "document_table_row"
                            | "document_code_block"
                    )
                )
        })
        .filter_map(|node| {
            let path = node.extra.get("structured_path").and_then(Value::as_str)?;
            let index = html_block_index(path)?;
            let heading_path = html_heading_path(&node.id, &parents, &headings)
                .unwrap_or_else(|| "source-root".into());
            Some((index, heading_path))
        })
        .collect::<Vec<_>>();
    blocks.sort_unstable();
    blocks.dedup();
    blocks
        .into_iter()
        .enumerate()
        .filter_map(|(ordinal, (_, heading_path))| {
            u32::try_from(ordinal)
                .ok()
                .and_then(|ordinal| ordinal.checked_add(1))
                .map(|paragraph| EvidenceLocatorCandidate::Html {
                    heading_path,
                    paragraph,
                })
        })
        .take(MAX_LOCATORS)
        .collect()
}

fn html_block_index(path: &str) -> Option<usize> {
    path.strip_prefix("$blocks[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

fn html_heading_path(
    node_id: &str,
    parents: &BTreeMap<&str, &str>,
    headings: &BTreeMap<&str, &str>,
) -> Option<String> {
    let mut titles = Vec::new();
    let mut current = node_id;
    for _ in 0..parents.len() {
        let Some(parent) = parents.get(current) else {
            break;
        };
        current = parent;
        if let Some(title) = headings.get(current) {
            titles.push(*title);
        }
    }
    titles.reverse();
    (!titles.is_empty()).then(|| titles.join(" / "))
}

fn markdown_heading(line: &str) -> Option<(u8, &str)> {
    let level = line.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&level) || !line.as_bytes().get(level)?.is_ascii_whitespace() {
        return None;
    }
    let title = line[level..].trim();
    (!title.is_empty()).then_some((u8::try_from(level).ok()?, title))
}

fn finish_markdown_paragraph(
    active: &mut Option<(usize, usize)>,
    heading_stack: &[(u8, &str)],
    paragraph: &mut u32,
    locators: &mut Vec<EvidenceLocatorCandidate>,
) {
    let Some((byte_start, byte_end)) = active.take() else {
        return;
    };
    *paragraph = paragraph.saturating_add(1);
    if locators.len() >= MAX_LOCATORS {
        return;
    }
    let heading_path = if heading_stack.is_empty() {
        "source-root".into()
    } else {
        heading_stack
            .iter()
            .map(|(_, title)| *title)
            .collect::<Vec<_>>()
            .join(" / ")
    };
    locators.push(EvidenceLocatorCandidate::Markdown {
        heading_path,
        paragraph: *paragraph,
        byte_start: u64::try_from(byte_start).expect("bounded evidence byte offset"),
        byte_end: u64::try_from(byte_end).expect("bounded evidence byte offset"),
    });
}

fn xlsx_cell_coordinates(value: &str) -> Option<(u32, u32)> {
    let letters = value.bytes().take_while(u8::is_ascii_alphabetic).count();
    if letters == 0 || letters > 3 {
        return None;
    }
    let row = value[letters..]
        .parse::<u32>()
        .ok()
        .filter(|row| *row > 0)?;
    let column = value[..letters].bytes().try_fold(0_u32, |column, letter| {
        column
            .checked_mul(26)?
            .checked_add(u32::from(letter.to_ascii_uppercase() - b'A' + 1))
    })?;
    Some((column, row))
}

fn xlsx_cell_range(column_start: u32, row_start: u32, column_end: u32, row_end: u32) -> String {
    let start = format!("{}{row_start}", xlsx_column_name(column_start));
    if column_start == column_end && row_start == row_end {
        start
    } else {
        format!("{start}:{}{row_end}", xlsx_column_name(column_end))
    }
}

fn xlsx_column_name(mut column: u32) -> String {
    let mut letters = Vec::new();
    while column > 0 {
        let remainder = u8::try_from((column - 1) % 26).expect("bounded column remainder");
        letters.push(char::from(b'A' + remainder));
        column = (column - 1) / 26;
    }
    letters.into_iter().rev().collect()
}

fn api_locator_candidates(value: &Value) -> Vec<EvidenceLocatorCandidate> {
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    if !object.contains_key("openapi") && !object.contains_key("asyncapi") {
        return Vec::new();
    }
    let mut locators = Vec::new();
    json_locator_candidates(value, "", &mut locators);
    locators
        .into_iter()
        .filter_map(|locator| match locator {
            EvidenceLocatorCandidate::Json { pointer } if !pointer.is_empty() => {
                Some(EvidenceLocatorCandidate::Api { pointer })
            }
            _ => None,
        })
        .collect()
}

fn json_locator_candidates(
    value: &Value,
    pointer: &str,
    locators: &mut Vec<EvidenceLocatorCandidate>,
) {
    let _ = json_locator_candidates_with_overflow(value, pointer, locators);
}

fn json_locator_candidates_with_overflow(
    value: &Value,
    pointer: &str,
    locators: &mut Vec<EvidenceLocatorCandidate>,
) -> bool {
    if locators.len() >= MAX_LOCATORS {
        return true;
    }
    match value {
        Value::Array(values) if !values.is_empty() => {
            for (index, value) in values.iter().enumerate() {
                if json_locator_candidates_with_overflow(
                    value,
                    &format!("{pointer}/{index}"),
                    locators,
                ) {
                    return true;
                }
            }
            false
        }
        Value::Object(values) if !values.is_empty() => {
            for (key, value) in values {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                if json_locator_candidates_with_overflow(
                    value,
                    &format!("{pointer}/{escaped}"),
                    locators,
                ) {
                    return true;
                }
            }
            false
        }
        _ => {
            locators.push(EvidenceLocatorCandidate::Json {
                pointer: pointer.into(),
            });
            false
        }
    }
}

fn complete_text_line_locators(bytes: &[u8]) -> Option<Vec<EvidenceLocatorCandidate>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut locators = Vec::new();
    let mut line_start = 1_u32;
    let mut line_end = 0_u32;
    let mut range_bytes = 0_usize;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let line_number = u32::try_from(index + 1).ok()?;
        if line.len() > MAX_TEXT_LOCATOR_BYTES {
            return None;
        }
        if range_bytes > 0 && range_bytes.saturating_add(line.len()) > MAX_TEXT_LOCATOR_BYTES {
            locators.push(EvidenceLocatorCandidate::Text(TextLocatorCandidate {
                line_start,
                line_end,
                semantic_role: None,
            }));
            if locators.len() >= MAX_LOCATORS {
                return None;
            }
            line_start = line_number;
            range_bytes = 0;
        }
        range_bytes = range_bytes.checked_add(line.len())?;
        line_end = line_number;
    }
    (range_bytes > 0).then(|| {
        locators.push(EvidenceLocatorCandidate::Text(TextLocatorCandidate {
            line_start,
            line_end,
            semantic_role: None,
        }));
    });
    (locators.len() <= MAX_LOCATORS).then_some(locators)
}

/// Preserve native sections in heading-organized plain-text captures.  Generic
/// `.txt` files often contain Markdown-style headings even when their transport
/// metadata does not identify them as Markdown; the fixed-size fallback would
/// otherwise collapse every short technical brief into one evidence span.
fn heading_organized_text_line_locators(bytes: &[u8]) -> Option<Vec<EvidenceLocatorCandidate>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let lines: Vec<_> = text.split_inclusive('\n').collect();
    let headings: Vec<_> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| atx_heading_line(line).then_some(index))
        .collect();
    if headings.len() < 2 {
        return None;
    }

    let first_heading = *headings
        .first()
        .expect("heading-organized text has a heading");
    let provenance_preamble =
        first_heading > 0 && rendered_capture_envelope_preamble(&lines[..first_heading]);
    let mut boundaries = Vec::with_capacity(headings.len() + 2);
    boundaries.push(0);
    boundaries.extend(headings.into_iter().filter(|index| *index > 0));
    boundaries.push(lines.len());
    boundaries.dedup();

    let mut locators = Vec::new();
    for section in boundaries.windows(2) {
        let mut line_start = section[0];
        let mut range_bytes = 0_usize;
        for (index, line) in lines.iter().enumerate().take(section[1]).skip(section[0]) {
            let line_bytes = line.len();
            if line_bytes > MAX_TEXT_LOCATOR_BYTES {
                return None;
            }
            if range_bytes > 0 && range_bytes.saturating_add(line_bytes) > MAX_TEXT_LOCATOR_BYTES {
                locators.push(EvidenceLocatorCandidate::Text(TextLocatorCandidate {
                    line_start: u32::try_from(line_start + 1).ok()?,
                    line_end: u32::try_from(index).ok()?,
                    semantic_role: (provenance_preamble
                        && section[0] == 0
                        && section[1] == first_heading)
                        .then_some(TextLocatorSemanticRole::Provenance),
                }));
                if locators.len() >= MAX_LOCATORS {
                    return None;
                }
                line_start = index;
                range_bytes = 0;
            }
            range_bytes = range_bytes.checked_add(line_bytes)?;
        }
        if range_bytes > 0 {
            locators.push(EvidenceLocatorCandidate::Text(TextLocatorCandidate {
                line_start: u32::try_from(line_start + 1).ok()?,
                line_end: u32::try_from(section[1]).ok()?,
                semantic_role: (provenance_preamble
                    && section[0] == 0
                    && section[1] == first_heading)
                    .then_some(TextLocatorSemanticRole::Provenance),
            }));
            if locators.len() > MAX_LOCATORS {
                return None;
            }
        }
    }
    (!locators.is_empty()).then_some(locators)
}

fn atx_heading_line(line: &str) -> bool {
    let hash_count = line.bytes().take_while(|byte| *byte == b'#').count();
    (1..=6).contains(&hash_count)
        && line
            .as_bytes()
            .get(hash_count)
            .is_some_and(u8::is_ascii_whitespace)
}

/// Recognize only the fixed, bounded capture envelope emitted for Glean
/// rendered content. Generic preambles remain technical text evidence.
fn rendered_capture_envelope_preamble(lines: &[&str]) -> bool {
    let [capture, url, fetched, blank_before_excerpts, excerpts, blank_before_heading] = lines
    else {
        return false;
    };
    let Some(capture_title) =
        rendered_capture_envelope_line(capture).strip_prefix("Glean rendered capture: ")
    else {
        return false;
    };
    let Some(url) = rendered_capture_envelope_line(url).strip_prefix("URL: ") else {
        return false;
    };
    let Some(fetched) = rendered_capture_envelope_line(fetched).strip_prefix("Fetched: ") else {
        return false;
    };
    let Some(excerpt_label) =
        rendered_capture_envelope_line(excerpts).strip_prefix("Relevant rendered excerpts for ")
    else {
        return false;
    };
    !capture_title.is_empty()
        && capture_title.len() <= MAX_RENDERED_CAPTURE_PREAMBLE_FIELD_BYTES
        && !url.is_empty()
        && url.len() <= MAX_RENDERED_CAPTURE_PREAMBLE_FIELD_BYTES
        && reqwest::Url::parse(url).is_ok_and(|parsed| parsed.has_host())
        && !fetched.is_empty()
        && fetched.len() <= MAX_RENDERED_CAPTURE_PREAMBLE_FIELD_BYTES
        && DateTime::parse_from_rfc3339(fetched).is_ok()
        && rendered_capture_envelope_line(blank_before_excerpts).is_empty()
        && !excerpt_label.is_empty()
        && excerpt_label.len() <= MAX_RENDERED_CAPTURE_PREAMBLE_FIELD_BYTES
        && excerpt_label.ends_with(':')
        && excerpt_label[..excerpt_label.len() - 1].trim().len() == excerpt_label.len() - 1
        && rendered_capture_envelope_line(blank_before_heading).is_empty()
}

fn rendered_capture_envelope_line(value: &str) -> &str {
    value.trim_end_matches(['\r', '\n'])
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression};
    use graphoxide_core::Node;
    use std::collections::BTreeMap;
    use std::io::Write;
    use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

    #[test]
    fn evidence_admission_ceiling_supports_a_128_mib_capture() {
        assert_eq!(MAX_EVIDENCE_BYTES, 128 * 1024 * 1024);
    }

    #[test]
    fn oversized_evidence_declares_a_reprocessable_inventory_route() {
        let bytes = vec![0_u8; MAX_EVIDENCE_BYTES + 1];

        let extraction = extract_locator_candidates(Path::new("oversized.bin"), &bytes)
            .expect("bound oversized evidence before parsing");

        assert_eq!(extraction.status, EvidenceExtractionStatus::InventoryOnly);
        assert!(extraction.locators.is_empty());
        assert_eq!(extraction.blocker.as_deref(), Some("evidence-byte-limit"));
        assert_eq!(
            extraction.retry_route.as_deref(),
            Some("split-source-or-raise-evidence-limit")
        );
    }

    #[test]
    fn partial_archives_preserve_their_declared_reprocessing_route() {
        let bytes = zip_bytes(&[(".env", b"VALUE=redacted"), ("guide.md", b"# Guide\n")]);

        let extraction = extract_locator_candidates(Path::new("bundle.zip"), &bytes)
            .expect("extract bounded archive evidence");

        assert_eq!(extraction.status, EvidenceExtractionStatus::Partial);
        assert_eq!(
            extraction.blocker.as_deref(),
            Some("archive-sensitive-members")
        );
        assert_eq!(
            extraction.retry_route.as_deref(),
            Some("authorize-sensitive-member-processing")
        );
    }

    #[test]
    fn rejected_gzip_member_size_retains_its_reprocessing_route() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(b"small member")
            .expect("write gzip member");
        let mut bytes = encoder.finish().expect("finish gzip");
        let isize = u32::try_from(64 * 1024 * 1024 + 1).expect("ISIZE fits u32");
        let trailer = bytes.len().checked_sub(4).expect("gzip has ISIZE trailer");
        bytes[trailer..].copy_from_slice(&isize.to_le_bytes());

        let extraction = extract_locator_candidates(Path::new("oversized-member.gz"), &bytes)
            .expect("extract bounded gzip evidence");

        assert_eq!(extraction.status, EvidenceExtractionStatus::InventoryOnly);
        assert!(extraction.locators.is_empty());
        assert_eq!(extraction.diagnostics, vec!["membersizelimit"]);
        assert_eq!(
            extraction.blocker.as_deref(),
            Some("archive-member-size-limit")
        );
        assert_eq!(
            extraction.retry_route.as_deref(),
            Some("raise-archive-member-size-limit")
        );
    }

    #[test]
    fn rejected_pdf_exposes_a_safe_parser_diagnostic() {
        let extraction = extract_locator_candidates(Path::new("broken.pdf"), b"%PDF-1.7\n\x80")
            .expect("extract bounded PDF evidence");
        assert_eq!(extraction.status, EvidenceExtractionStatus::InventoryOnly);
        assert!(extraction.locators.is_empty());
        assert_eq!(extraction.diagnostics.len(), 1);
        assert!(extraction.diagnostics[0].starts_with("pdf_"));
        assert_eq!(
            extraction.blocker.as_deref(),
            Some("evidence-locator-extraction-unavailable")
        );
        assert_eq!(
            extraction.retry_route.as_deref(),
            Some("add-format-adapter-or-enrichment")
        );
    }

    #[test]
    fn partial_pdf_attachment_is_explicitly_reprocessable() {
        let source = graphoxide_core::Extraction {
            nodes: vec![Node {
                id: "pdf".into(),
                label: "reference.pdf".into(),
                file_type: "paper".into(),
                source_file: "reference.pdf".into(),
                source_location: None,
                community: None,
                extra: BTreeMap::from([
                    ("type".into(), "pdf_document".into()),
                    ("parse_status".into(), "partial".into()),
                    ("ignored_pdf_features".into(), "embedded_files".into()),
                ]),
            }],
            edges: Vec::new(),
            hyperedges: Vec::new(),
        };
        let extraction = evidence_extraction_result(
            EvidenceExtractionStatus::Partial,
            vec![EvidenceLocatorCandidate::Pdf { page: 1 }],
            vec![partial_pdf_coverage_diagnostic(&source)
                .expect("partial PDF attachment must retain coverage evidence")
                .into()],
        );

        assert_eq!(extraction.status, EvidenceExtractionStatus::Partial);
        assert_eq!(
            extraction.blocker.as_deref(),
            Some("pdf-embedded-attachments")
        );
        assert_eq!(
            extraction.retry_route.as_deref(),
            Some("extract-pdf-attachments")
        );
    }

    #[test]
    fn archive_child_pdf_page_preserves_its_member_path() {
        let extraction = graphoxide_core::Extraction {
            nodes: vec![Node {
                id: "embedded-pdf-page".into(),
                label: "Page 3".into(),
                file_type: "document".into(),
                source_file: "bundle.zip!/docs/reference.pdf".into(),
                source_location: None,
                community: None,
                extra: BTreeMap::from([
                    ("type".into(), "pdf_page".into()),
                    ("page_number".into(), 3.into()),
                ]),
            }],
            edges: Vec::new(),
            hyperedges: Vec::new(),
        };

        assert_eq!(
            archive_child_locator_candidates(&extraction),
            (
                vec![EvidenceLocatorCandidate::ArchiveChild {
                    member: "docs/reference.pdf".into(),
                    inner: ArchiveInnerLocatorCandidate::Pdf { page: 3 },
                }],
                false,
            )
        );
    }

    #[test]
    fn locator_limit_is_explicitly_partial() {
        let source = "line\n".repeat((MAX_LOCATORS * 2 + 1) * TEXT_LOCATOR_TARGET_LINES as usize);
        let extraction = extract_locator_candidates(Path::new("large.txt"), source.as_bytes())
            .expect("extract bounded text evidence");
        assert_eq!(extraction.status, EvidenceExtractionStatus::Partial);
        assert!(extraction.locators.len() <= MAX_LOCATORS);
        assert!(extraction.diagnostics.contains(&"locator-limit".to_owned()));
        assert_eq!(extraction.blocker.as_deref(), Some("locator-limit"));
        assert_eq!(
            extraction.retry_route.as_deref(),
            Some("raise-locator-limit")
        );
    }

    #[test]
    fn heading_organized_text_keeps_each_native_section_as_a_locator() {
        let source =
            "Preamble\n\n# System\nSystem details\n# Power\n54V input\n# Interfaces\nNVLink\n";
        let extraction = extract_locator_candidates(Path::new("system.txt"), source.as_bytes())
            .expect("extract heading-organized text");

        assert_eq!(extraction.status, EvidenceExtractionStatus::Extracted);
        assert_eq!(
            extraction.locators,
            vec![
                EvidenceLocatorCandidate::Text(TextLocatorCandidate {
                    line_start: 1,
                    line_end: 2,
                    semantic_role: None,
                }),
                EvidenceLocatorCandidate::Text(TextLocatorCandidate {
                    line_start: 3,
                    line_end: 4,
                    semantic_role: None,
                }),
                EvidenceLocatorCandidate::Text(TextLocatorCandidate {
                    line_start: 5,
                    line_end: 6,
                    semantic_role: None,
                }),
                EvidenceLocatorCandidate::Text(TextLocatorCandidate {
                    line_start: 7,
                    line_end: 8,
                    semantic_role: None,
                }),
            ]
        );
    }

    #[test]
    fn rendered_capture_envelope_preamble_is_tagged_as_provenance() {
        let source = concat!(
            "Glean rendered capture: Example platform\n",
            "URL: https://docs.example.com/pages/123\n",
            "Fetched: 2026-05-10T07:40:00+00:00\n",
            "\n",
            "Relevant rendered excerpts for example platform extraction:\n",
            "\n",
            "# System\n",
            "System details\n",
            "# Power\n",
            "54V input\n",
        );

        let extraction = extract_locator_candidates(Path::new("system.txt"), source.as_bytes())
            .expect("extract rendered capture envelope");
        let EvidenceLocatorCandidate::Text(preamble) = &extraction.locators[0] else {
            panic!("first rendered capture segment must be text");
        };
        assert_eq!((preamble.line_start, preamble.line_end), (1, 6));
        assert_eq!(
            preamble.semantic_role,
            Some(TextLocatorSemanticRole::Provenance)
        );
        assert!(extraction.locators[1..].iter().all(|locator| {
            matches!(
                locator,
                EvidenceLocatorCandidate::Text(TextLocatorCandidate {
                    semantic_role: None,
                    ..
                })
            )
        }));
    }

    #[test]
    fn malformed_rendered_capture_preambles_remain_technical_text() {
        for source in [
            concat!(
                "Glean rendered capture: Example platform\n",
                "URL: not-a-url\n",
                "Fetched: 2026-05-10T07:40:00+00:00\n",
                "\n",
                "Relevant rendered excerpts for example platform extraction:\n",
                "\n",
                "# System\nSystem details\n# Power\n54V input\n",
            ),
            concat!(
                "Glean rendered capture: Example platform\n",
                "URL: https://docs.example.com/pages/123\n",
                "Fetched: 2026-05-10T07:40:00\n",
                "\n",
                "Relevant rendered excerpts for example platform extraction:\n",
                "\n",
                "# System\nSystem details\n# Power\n54V input\n",
            ),
            concat!(
                "Glean rendered capture: Example platform\n",
                "URL: https://docs.example.com/pages/123\n",
                "Fetched: 2026-05-10T07:40:00+00:00\n",
                "\n",
                "Relevant rendered excerpts for example platform extraction\n",
                "\n",
                "# System\nSystem details\n# Power\n54V input\n",
            ),
        ] {
            let extraction = extract_locator_candidates(Path::new("system.txt"), source.as_bytes())
                .expect("extract malformed rendered capture envelope");
            let EvidenceLocatorCandidate::Text(preamble) = &extraction.locators[0] else {
                panic!("malformed preamble must remain a text locator");
            };
            assert_eq!(preamble.semantic_role, None);
        }

        let oversized_label = "x".repeat(MAX_RENDERED_CAPTURE_PREAMBLE_FIELD_BYTES);
        let source = format!(
            "Glean rendered capture: Example platform\n\
             URL: https://docs.example.com/pages/123\n\
             Fetched: 2026-05-10T07:40:00+00:00\n\n\
             Relevant rendered excerpts for {oversized_label}:\n\n\
             # System\nSystem details\n# Power\n54V input\n"
        );
        let extraction = extract_locator_candidates(Path::new("system.txt"), source.as_bytes())
            .expect("extract oversized rendered capture envelope");
        let EvidenceLocatorCandidate::Text(preamble) = &extraction.locators[0] else {
            panic!("oversized preamble must remain a text locator");
        };
        assert_eq!(preamble.semantic_role, None);
    }

    #[test]
    fn locator_limit_normalizes_existing_diagnostics_for_strict_receipts() {
        let extraction = evidence_extraction_result_with_locator_limit(
            EvidenceExtractionStatus::Partial,
            Vec::new(),
            vec![
                "row_limit".into(),
                "locator-limit".into(),
                "row_limit".into(),
            ],
            true,
        );

        assert_eq!(
            extraction.diagnostics,
            vec!["locator-limit".to_owned(), "row_limit".to_owned()]
        );
    }

    #[test]
    fn overflowing_json_scalars_fall_back_to_complete_bounded_text_ranges() {
        let entries = (0..=MAX_LOCATORS)
            .map(|index| {
                format!(
                    "    {{\"id\": {index}, \"payload\": \"{}\"}}",
                    "x".repeat(256)
                )
            })
            .collect::<Vec<_>>()
            .join(",\n");
        let source = format!("{{\n  \"entries\": [\n{entries}\n  ]\n}}\n");

        let extraction = extract_locator_candidates(Path::new("entries.json"), source.as_bytes())
            .expect("extract overflowing JSON evidence");

        assert_eq!(extraction.status, EvidenceExtractionStatus::Extracted);
        assert!(!extraction.diagnostics.contains(&"locator-limit".to_owned()));
        let ranges = extraction
            .locators
            .iter()
            .map(|locator| match locator {
                EvidenceLocatorCandidate::Text(range) => range,
                other => panic!("overflowing JSON must use text ranges, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert!(ranges.len() > 1);
        let mut expected_start = 1_u32;
        for range in ranges {
            assert_eq!(range.line_start, expected_start);
            let content = source
                .split_inclusive('\n')
                .enumerate()
                .filter(|(index, _)| {
                    let line = u32::try_from(index + 1).expect("bounded fixture line number");
                    line >= range.line_start && line <= range.line_end
                })
                .map(|(_, line)| line)
                .collect::<String>();
            assert!(content.len() <= MAX_TEXT_LOCATOR_BYTES);
            expected_start = range.line_end + 1;
        }
        assert_eq!(
            expected_start,
            u32::try_from(source.lines().count() + 1).expect("bounded fixture line count")
        );
    }

    #[test]
    fn evidence_partitions_beyond_the_legacy_256_locator_ceiling_are_complete() {
        let source = "line\n".repeat(257 * TEXT_LOCATOR_TARGET_LINES as usize);
        let extraction = extract_locator_candidates(Path::new("large.txt"), source.as_bytes())
            .expect("extract bounded text evidence");
        assert_eq!(extraction.status, EvidenceExtractionStatus::Extracted);
        assert_eq!(extraction.locators.len(), 257);
    }

    #[test]
    fn inspected_rasters_emit_one_complete_full_frame_pixel_locator() {
        let cases = [
            ("architecture.png", png_bytes(640, 480)),
            ("architecture.jpeg", jpeg_bytes(640, 480)),
        ];

        for (path, bytes) in cases {
            let extraction = extract_locator_candidates(Path::new(path), &bytes)
                .expect("extract bounded raster evidence");
            assert_eq!(extraction.status, EvidenceExtractionStatus::Extracted);
            assert_eq!(
                extraction.locators,
                vec![EvidenceLocatorCandidate::RasterImage {
                    coordinate_system: "pixel",
                    x: 0,
                    y: 0,
                    width: 640,
                    height: 480,
                }]
            );
            assert!(!extraction.diagnostics.contains(&"locator-limit".to_owned()));
        }
    }

    #[test]
    fn rasters_without_positive_dimensions_do_not_admit_pixel_evidence() {
        for (path, bytes) in [
            ("empty.png", png_bytes(0, 480)),
            ("truncated.jpeg", vec![0xff, 0xd8, 0xff]),
        ] {
            let extraction = extract_locator_candidates(Path::new(path), &bytes)
                .expect("inspect bounded raster evidence");

            assert_eq!(extraction.status, EvidenceExtractionStatus::InventoryOnly);
            assert!(extraction.locators.is_empty());
        }
    }

    #[test]
    fn registered_inventory_routes_preserve_reprocessable_blocker_metadata() {
        for (path, bytes, blocker, retry_route) in [
            (
                "recording.flac",
                b"not a flac stream".as_slice(),
                "media-transcription-or-video-analysis-unavailable",
                "media-enrichment-route-available",
            ),
            (
                "archive.7z",
                b"7z\xbc\xaf\x27\x1c".as_slice(),
                "archive-decoder-unavailable",
                "bounded-decoder-available",
            ),
        ] {
            let extraction = extract_locator_candidates(Path::new(path), bytes)
                .expect("extract inventory-only evidence");

            assert_eq!(
                extraction.status,
                EvidenceExtractionStatus::InventoryOnly,
                "{path}"
            );
            assert!(extraction.locators.is_empty(), "{path}");
            assert_eq!(extraction.blocker.as_deref(), Some(blocker), "{path}");
            assert_eq!(
                extraction.retry_route.as_deref(),
                Some(retry_route),
                "{path}"
            );
        }
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend(width.to_be_bytes());
        bytes.extend(height.to_be_bytes());
        bytes.extend([8, 6, 0, 0, 0]);
        bytes
    }

    fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let cursor = std::io::Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, value) in entries {
            writer.start_file(*name, options).expect("start ZIP member");
            writer.write_all(value).expect("write ZIP member");
        }
        writer.finish().expect("finish ZIP").into_inner()
    }

    fn jpeg_bytes(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xc0, 0x00, 0x11, 8];
        bytes.extend(height.to_be_bytes());
        bytes.extend(width.to_be_bytes());
        bytes.extend([0; 10]);
        bytes
    }

    #[test]
    fn delimited_evidence_uses_native_data_row_ranges() {
        let extraction = extract_locator_candidates(
            Path::new("services.csv"),
            b"service,replicas\napi,3\nworker,2\n",
        )
        .expect("extract delimited evidence");
        assert_eq!(
            extraction.locators,
            vec![EvidenceLocatorCandidate::Delimited {
                format: "csv".into(),
                row_start: 1,
                row_end: 2,
            }]
        );
    }

    #[test]
    fn json_lines_evidence_uses_native_record_numbers() {
        let extraction = extract_locator_candidates(
            Path::new("events.jsonl"),
            b"{\"event\":\"start\"}\n{\"event\":\"stop\"}\n",
        )
        .expect("extract JSON Lines evidence");
        assert_eq!(
            extraction.locators,
            vec![
                EvidenceLocatorCandidate::JsonLines { record: 1 },
                EvidenceLocatorCandidate::JsonLines { record: 2 },
            ]
        );
    }

    #[test]
    fn malformed_json_lines_blocks_canonical_coverage_with_retained_records() {
        let extraction = extract_locator_candidates(
            Path::new("events.jsonl"),
            b"{\"event\":\"start\"}\nnot-json\n{\"event\":\"stop\"}\n",
        )
        .expect("extract partially malformed JSON Lines evidence");

        assert_eq!(extraction.status, EvidenceExtractionStatus::Partial);
        assert_eq!(
            extraction.locators,
            vec![
                EvidenceLocatorCandidate::JsonLines { record: 1 },
                EvidenceLocatorCandidate::JsonLines { record: 3 },
            ]
        );
        assert!(extraction
            .diagnostics
            .contains(&"json_lines_parse_error".to_owned()));
        assert_eq!(
            extraction.blocker.as_deref(),
            Some("structured-source-parse-error")
        );
        assert_eq!(
            extraction.retry_route.as_deref(),
            Some("repair-structured-source")
        );
    }

    #[test]
    fn malformed_csv_tail_blocks_canonical_coverage_with_retained_rows() {
        let extraction = extract_locator_candidates(
            Path::new("services.csv"),
            b"service,replicas\napi,3\nworker,2\nbroken,\"unterminated",
        )
        .expect("extract CSV evidence before malformed tail");

        assert_eq!(extraction.status, EvidenceExtractionStatus::Partial);
        assert_eq!(
            extraction.locators,
            vec![EvidenceLocatorCandidate::Delimited {
                format: "csv".into(),
                row_start: 1,
                row_end: 2,
            }]
        );
        assert!(extraction
            .diagnostics
            .contains(&"csv_parse_error".to_owned()));
        assert_eq!(
            extraction.blocker.as_deref(),
            Some("structured-source-parse-error")
        );
        assert_eq!(
            extraction.retry_route.as_deref(),
            Some("repair-structured-source")
        );
    }

    #[test]
    fn json_lines_locator_cap_blocks_canonical_coverage() {
        let source = (0..=MAX_LOCATORS)
            .map(|record| format!("{{\"record\":{record}}}\n"))
            .collect::<String>();
        let extraction = extract_locator_candidates(Path::new("events.jsonl"), source.as_bytes())
            .expect("extract bounded JSON Lines evidence");

        assert_eq!(extraction.status, EvidenceExtractionStatus::Partial);
        assert_eq!(extraction.locators.len(), MAX_LOCATORS);
        assert_eq!(
            extraction.locators.last(),
            Some(&EvidenceLocatorCandidate::JsonLines {
                record: u32::try_from(MAX_LOCATORS).expect("locator cap fits JSON Lines record"),
            })
        );
        assert!(extraction.diagnostics.contains(&"locator-limit".to_owned()));
        assert!(extraction.diagnostics.contains(&"fact_limit".to_owned()));
        assert_eq!(
            extraction.blocker.as_deref(),
            Some("structured-extraction-limit")
        );
        assert_eq!(
            extraction.retry_route.as_deref(),
            Some("raise-structured-extraction-limit")
        );
    }

    #[test]
    fn markdown_locator_cap_blocks_canonical_coverage() {
        let source = (0..=MAX_LOCATORS)
            .map(|paragraph| format!("Paragraph {paragraph}.\n\n"))
            .collect::<String>();
        let extraction = extract_locator_candidates(Path::new("guide.md"), source.as_bytes())
            .expect("extract bounded Markdown evidence");

        assert_eq!(extraction.status, EvidenceExtractionStatus::Partial);
        assert_eq!(extraction.locators.len(), MAX_LOCATORS);
        assert_eq!(extraction.diagnostics, vec!["locator-limit".to_owned()]);
        assert_eq!(extraction.blocker.as_deref(), Some("locator-limit"));
        assert_eq!(
            extraction.retry_route.as_deref(),
            Some("raise-locator-limit")
        );
    }

    #[test]
    fn structured_document_locator_caps_are_reprocessable() {
        let yaml = (0..=MAX_LOCATORS)
            .map(|index| format!("field_{index}: value_{index}\n"))
            .collect::<String>();
        let xml = format!(
            "<root>{}</root>",
            (0..=MAX_LOCATORS)
                .map(|index| format!("<field_{index}>value_{index}</field_{index}>"))
                .collect::<String>()
        );
        let html = (0..=MAX_LOCATORS)
            .map(|index| format!("<p>Paragraph {index}.</p>\n"))
            .collect::<String>();

        for (path, source) in [
            ("settings.yaml", yaml),
            ("settings.xml", xml),
            ("guide.html", html),
        ] {
            let extraction = extract_locator_candidates(Path::new(path), source.as_bytes())
                .expect("extract bounded structured document evidence");
            assert_eq!(
                extraction.status,
                EvidenceExtractionStatus::Partial,
                "{path}"
            );
            assert_eq!(extraction.locators.len(), MAX_LOCATORS, "{path}");
            assert!(
                extraction.diagnostics.contains(&"locator-limit".to_owned()),
                "{path} must retain its capped native partitions as a reprocessable state"
            );
            assert!(extraction.blocker.is_some(), "{path}");
            assert!(extraction.retry_route.is_some(), "{path}");
        }
    }

    #[test]
    fn delimited_fact_limit_blocks_canonical_coverage_without_false_locator_limit() {
        let source = std::iter::once("record\n".to_owned())
            .chain(
                (0..=crate::format_registry::STRUCTURED_TEXT_LIMITS.max_records)
                    .map(|record| format!("{record}\n")),
            )
            .collect::<String>();
        let extraction = extract_locator_candidates(Path::new("records.csv"), source.as_bytes())
            .expect("extract bounded delimited evidence");

        assert_eq!(extraction.status, EvidenceExtractionStatus::Partial);
        assert!(extraction.locators.len() < MAX_LOCATORS);
        assert!(
            extraction.diagnostics.contains(&"fact_limit".to_owned()),
            "delimited fact omission must remain observable"
        );
        assert!(
            !extraction.diagnostics.contains(&"locator-limit".to_owned()),
            "a structured parser limit must not be misreported as a locator limit"
        );
        assert_eq!(
            extraction.blocker.as_deref(),
            Some("structured-extraction-limit")
        );
        assert_eq!(
            extraction.retry_route.as_deref(),
            Some("raise-structured-extraction-limit")
        );
    }

    #[test]
    fn truncated_delimited_data_is_partial_with_retained_row_ranges() {
        let columns = (0..257)
            .map(|column| format!("column_{column}"))
            .collect::<Vec<_>>();
        for (path, delimiter, format) in [("wide.csv", ",", "csv"), ("wide.tsv", "\t", "tsv")] {
            let source = format!("{}\n{}\n", columns.join(delimiter), columns.join(delimiter));
            let extraction = extract_locator_candidates(Path::new(path), source.as_bytes())
                .expect("extract bounded delimited evidence");

            assert_eq!(
                extraction.status,
                EvidenceExtractionStatus::Partial,
                "{path}"
            );
            assert_eq!(
                extraction.locators,
                vec![EvidenceLocatorCandidate::Delimited {
                    format: format.into(),
                    row_start: 1,
                    row_end: 1,
                }],
                "{path}"
            );
            assert!(
                extraction.diagnostics.contains(&"field_limit".to_owned()),
                "{path}: field omission must remain observable"
            );
        }
    }

    #[test]
    fn truncated_json_lines_data_is_partial_with_retained_record_evidence() {
        let record = (0..3_000)
            .map(|index| format!("\"field_{index}\":{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let source = format!("{{{record}}}\n");
        let extraction = extract_locator_candidates(Path::new("wide.jsonl"), source.as_bytes())
            .expect("extract bounded JSON Lines evidence");

        assert_eq!(extraction.status, EvidenceExtractionStatus::Partial);
        assert_eq!(
            extraction.locators,
            vec![EvidenceLocatorCandidate::JsonLines { record: 1 }]
        );
        assert!(
            extraction.diagnostics.contains(&"fact_limit".to_owned()),
            "fact omission must remain observable"
        );
    }

    #[test]
    fn spreadsheet_candidates_preserve_bounded_xlsx_sheet_ranges() {
        let extraction = graphoxide_core::Extraction {
            nodes: vec![Node {
                id: "sheet".into(),
                label: "Measurements".into(),
                file_type: "document".into(),
                source_file: "measurements.xlsx".into(),
                source_location: None,
                community: None,
                extra: BTreeMap::from([
                    ("type".into(), "workbook_sheet".into()),
                    ("format".into(), "xlsx".into()),
                    (
                        "text".into(),
                        "A1: Voltage\nB2: 400\nnot-a-cell: ignored".into(),
                    ),
                ]),
            }],
            edges: Vec::new(),
            hyperedges: Vec::new(),
        };

        assert_eq!(
            spreadsheet_locator_candidates_with_limit(&extraction).0,
            vec![EvidenceLocatorCandidate::Spreadsheet {
                sheet: "Measurements".into(),
                cell_range: "A1:B2".into(),
            }]
        );
    }

    #[test]
    fn spreadsheet_candidates_preserve_bounded_ods_sheet_ranges() {
        let extraction = graphoxide_core::Extraction {
            nodes: vec![Node {
                id: "sheet".into(),
                label: "Measurements".into(),
                file_type: "document".into(),
                source_file: "measurements.ods".into(),
                source_location: None,
                community: None,
                extra: BTreeMap::from([
                    ("type".into(), "workbook_sheet".into()),
                    ("format".into(), "ods".into()),
                    (
                        "text".into(),
                        "A1: Voltage\nB2: 400\nnot-a-cell: ignored".into(),
                    ),
                ]),
            }],
            edges: Vec::new(),
            hyperedges: Vec::new(),
        };

        assert_eq!(
            spreadsheet_locator_candidates_with_limit(&extraction).0,
            vec![EvidenceLocatorCandidate::Spreadsheet {
                sheet: "Measurements".into(),
                cell_range: "A1:B2".into(),
            }]
        );
    }

    #[test]
    fn spreadsheet_candidates_do_not_exhaust_the_locator_cap_per_cell() {
        let text = (1..=MAX_LOCATORS as u32 + 1)
            .map(|row| format!("A{row}: {row}"))
            .collect::<Vec<_>>()
            .join("\n");
        let extraction = graphoxide_core::Extraction {
            nodes: vec![Node {
                id: "sheet".into(),
                label: "Measurements".into(),
                file_type: "document".into(),
                source_file: "measurements.xlsx".into(),
                source_location: None,
                community: None,
                extra: BTreeMap::from([
                    ("type".into(), "workbook_sheet".into()),
                    ("format".into(), "xlsx".into()),
                    ("text".into(), text.into()),
                ]),
            }],
            edges: Vec::new(),
            hyperedges: Vec::new(),
        };

        assert_eq!(
            spreadsheet_locator_candidates_with_limit(&extraction).0,
            vec![
                EvidenceLocatorCandidate::Spreadsheet {
                    sheet: "Measurements".into(),
                    cell_range: "A1:A128".into(),
                },
                EvidenceLocatorCandidate::Spreadsheet {
                    sheet: "Measurements".into(),
                    cell_range: "A129:A256".into(),
                },
                EvidenceLocatorCandidate::Spreadsheet {
                    sheet: "Measurements".into(),
                    cell_range: "A257:A384".into(),
                },
                EvidenceLocatorCandidate::Spreadsheet {
                    sheet: "Measurements".into(),
                    cell_range: "A385:A512".into(),
                },
                EvidenceLocatorCandidate::Spreadsheet {
                    sheet: "Measurements".into(),
                    cell_range: "A513:A640".into(),
                },
                EvidenceLocatorCandidate::Spreadsheet {
                    sheet: "Measurements".into(),
                    cell_range: "A641:A768".into(),
                },
                EvidenceLocatorCandidate::Spreadsheet {
                    sheet: "Measurements".into(),
                    cell_range: "A769:A896".into(),
                },
                EvidenceLocatorCandidate::Spreadsheet {
                    sheet: "Measurements".into(),
                    cell_range: "A897:A1024".into(),
                },
                EvidenceLocatorCandidate::Spreadsheet {
                    sheet: "Measurements".into(),
                    cell_range: "A1025".into(),
                },
            ]
        );
    }

    #[test]
    fn spreadsheet_candidates_report_only_a_real_range_cap() {
        let text = (0..MAX_LOCATORS as u32)
            .map(|partition| {
                let row = partition * XLSX_ROWS_PER_LOCATOR + 1;
                format!("A{row}: {row}")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let spreadsheet = |text: String| graphoxide_core::Extraction {
            nodes: vec![Node {
                id: "sheet".into(),
                label: "Measurements".into(),
                file_type: "document".into(),
                source_file: "measurements.xlsx".into(),
                source_location: None,
                community: None,
                extra: BTreeMap::from([
                    ("type".into(), "workbook_sheet".into()),
                    ("format".into(), "xlsx".into()),
                    ("text".into(), text.into()),
                ]),
            }],
            edges: Vec::new(),
            hyperedges: Vec::new(),
        };
        let (locators, limit_reached) =
            spreadsheet_locator_candidates_with_limit(&spreadsheet(text.clone()));
        assert!(!limit_reached);
        assert_eq!(locators.len(), MAX_LOCATORS);
        assert_eq!(
            locators.last(),
            Some(&EvidenceLocatorCandidate::Spreadsheet {
                sheet: "Measurements".into(),
                cell_range: "A130945".into(),
            })
        );

        let overflow_row = MAX_LOCATORS as u32 * XLSX_ROWS_PER_LOCATOR + 1;
        let (locators, limit_reached) = spreadsheet_locator_candidates_with_limit(&spreadsheet(
            format!("{text}\nA{overflow_row}: {overflow_row}"),
        ));
        assert!(limit_reached);
        assert_eq!(locators.len(), MAX_LOCATORS);
        assert_eq!(
            locators.last(),
            Some(&EvidenceLocatorCandidate::Spreadsheet {
                sheet: "Measurements".into(),
                cell_range: "A130945".into(),
            })
        );
    }

    #[test]
    fn word_candidates_preserve_heading_path_and_document_paragraph() {
        let extraction = graphoxide_core::Extraction {
            nodes: vec![Node {
                id: "section".into(),
                label: "Section 1".into(),
                file_type: "document".into(),
                source_file: "reference.docx".into(),
                source_location: None,
                community: None,
                extra: BTreeMap::from([
                    ("type".into(), "document_section".into()),
                    ("format".into(), "docx".into()),
                    (
                        "word_paragraphs".into(),
                        serde_json::json!([
                            {"heading_path": "Architecture / Interface", "paragraph": 7},
                            {"heading_path": "Architecture", "paragraph": 2}
                        ]),
                    ),
                ]),
            }],
            edges: Vec::new(),
            hyperedges: Vec::new(),
        };

        assert_eq!(
            word_locator_candidates(&extraction),
            vec![
                EvidenceLocatorCandidate::Word {
                    heading_path: "Architecture".into(),
                    paragraph: 2,
                },
                EvidenceLocatorCandidate::Word {
                    heading_path: "Architecture / Interface".into(),
                    paragraph: 7,
                },
            ]
        );
    }

    #[test]
    fn office_locator_status_keeps_complete_roots_and_blocks_fact_limited_roots() {
        let root = |kind: &str, parse_status: &str, coverage_blocker: Option<&str>| {
            let mut extra = BTreeMap::from([
                ("type".into(), kind.into()),
                ("parse_status".into(), parse_status.into()),
            ]);
            if let Some(coverage_blocker) = coverage_blocker {
                extra.insert("coverage_blocker".into(), coverage_blocker.into());
            }
            Node {
                id: format!("root:{kind}:{parse_status}"),
                label: "reference".into(),
                file_type: "document".into(),
                source_file: "reference".into(),
                source_location: None,
                community: None,
                extra,
            }
        };
        let fixture = |root: Node| graphoxide_core::Extraction {
            nodes: vec![root],
            edges: Vec::new(),
            hyperedges: Vec::new(),
        };

        assert_eq!(
            office_locator_status(
                &fixture(root("docx_document", "complete", None)),
                "docx_document"
            ),
            Some((EvidenceExtractionStatus::Extracted, Vec::new()))
        );
        assert_eq!(
            office_locator_status(
                &fixture(root("docx_document", "partial", Some("office_fact_limit"),)),
                "docx_document",
            ),
            Some((
                EvidenceExtractionStatus::Partial,
                vec!["office-fact-limit".into()],
            ))
        );
        assert_eq!(
            office_locator_status(
                &fixture(root("xlsx_workbook", "complete", None)),
                "xlsx_workbook"
            ),
            Some((EvidenceExtractionStatus::Extracted, Vec::new()))
        );
        assert_eq!(
            office_locator_status(
                &fixture(root("xlsx_workbook", "partial", Some("office_fact_limit"),)),
                "xlsx_workbook",
            ),
            Some((
                EvidenceExtractionStatus::Partial,
                vec!["office-fact-limit".into()],
            ))
        );
    }

    #[test]
    fn markdown_candidates_preserve_heading_path_paragraph_and_byte_range() {
        let source = "# Design\nintro one\ncontinues\n\n## Interface\npayload\n";
        let first_end = source.find("\n\n").expect("paragraph separator");
        let second_start = source.find("payload").expect("second paragraph");
        let extraction = extract_locator_candidates(Path::new("guide.md"), source.as_bytes())
            .expect("extract Markdown locators");

        assert_eq!(
            extraction.locators,
            vec![
                EvidenceLocatorCandidate::Markdown {
                    heading_path: "Design".into(),
                    paragraph: 1,
                    byte_start: 9,
                    byte_end: u64::try_from(first_end).expect("byte offset"),
                },
                EvidenceLocatorCandidate::Markdown {
                    heading_path: "Design / Interface".into(),
                    paragraph: 2,
                    byte_start: u64::try_from(second_start).expect("byte offset"),
                    byte_end: u64::try_from(second_start + "payload".len()).expect("byte offset"),
                },
            ]
        );
    }

    #[test]
    fn markdown_candidates_include_fenced_code_as_citable_evidence() {
        let source = "# Design\n\n```rust\nlet answer = 42;\n```\n";
        let start = source.find("```rust").expect("fence start");
        let fence = "```rust\nlet answer = 42;\n```";
        let extraction = extract_locator_candidates(Path::new("guide.md"), source.as_bytes())
            .expect("extract Markdown locators");

        assert_eq!(
            extraction.locators,
            vec![EvidenceLocatorCandidate::Markdown {
                heading_path: "Design".into(),
                paragraph: 1,
                byte_start: u64::try_from(start).expect("byte offset"),
                byte_end: u64::try_from(start + fence.len()).expect("byte offset"),
            }]
        );
    }

    #[test]
    fn mdx_and_qmd_candidates_preserve_heading_and_fenced_code_locators() {
        let source = "# Notebook\n\n```python\nprint(42)\n```";
        let expected = vec![EvidenceLocatorCandidate::Markdown {
            heading_path: "Notebook".into(),
            paragraph: 1,
            byte_start: 12,
            byte_end: 35,
        }];

        for path in ["notebook.mdx", "notebook.qmd"] {
            let extraction = extract_locator_candidates(Path::new(path), source.as_bytes())
                .expect("extract native Markdown locators");

            assert_eq!(extraction.locators, expected, "{path}");
        }
    }

    #[test]
    fn markdown_candidates_preserve_source_root_and_nested_fence_exactly() {
        let source =
            "Lead-in.\n\n# Example\n\n````markdown\n~~~rust\nlet value = `nested`;\n~~~\n````\n";
        let fence = "````markdown\n~~~rust\nlet value = `nested`;\n~~~\n````";
        let fence_start = source.find(fence).expect("fence start");
        let extraction = extract_locator_candidates(Path::new("guide.md"), source.as_bytes())
            .expect("extract Markdown locators");

        assert_eq!(
            extraction.locators,
            vec![
                EvidenceLocatorCandidate::Markdown {
                    heading_path: "source-root".into(),
                    paragraph: 1,
                    byte_start: 0,
                    byte_end: 8,
                },
                EvidenceLocatorCandidate::Markdown {
                    heading_path: "Example".into(),
                    paragraph: 2,
                    byte_start: u64::try_from(fence_start).expect("byte offset"),
                    byte_end: u64::try_from(fence_start + fence.len()).expect("byte offset"),
                },
            ]
        );
        let EvidenceLocatorCandidate::Markdown {
            byte_start,
            byte_end,
            ..
        } = &extraction.locators[1]
        else {
            panic!("nested fence must have a Markdown locator");
        };
        assert_eq!(
            &source[usize::try_from(*byte_start).expect("byte offset")
                ..usize::try_from(*byte_end).expect("byte offset")],
            fence
        );
    }

    #[test]
    fn yaml_candidates_preserve_structural_path_and_document() {
        let extraction =
            extract_locator_candidates(Path::new("reference.yaml"), b"service:\n  replicas: 3\n")
                .expect("extract YAML locators");

        assert_eq!(
            extraction.locators,
            vec![
                EvidenceLocatorCandidate::Yaml {
                    path: "$.service".into(),
                    document: 1,
                },
                EvidenceLocatorCandidate::Yaml {
                    path: "$.service.replicas".into(),
                    document: 1,
                },
            ]
        );
    }

    #[test]
    fn xml_candidates_preserve_structural_element_and_attribute_paths() {
        let extraction = extract_locator_candidates(
            Path::new("reference.xml"),
            br#"<service version="1"><replicas>3</replicas></service>"#,
        )
        .expect("extract XML locators");

        assert_eq!(
            extraction.locators,
            vec![
                EvidenceLocatorCandidate::Xml {
                    path: "$/service[1]".into(),
                },
                EvidenceLocatorCandidate::Xml {
                    path: "$/service[1]/@version".into(),
                },
                EvidenceLocatorCandidate::Xml {
                    path: "$/service[1]/replicas[1]".into(),
                },
            ]
        );
    }

    #[test]
    fn html_candidates_preserve_extracted_heading_hierarchy_and_paragraph_order() {
        let extraction = extract_locator_candidates(
            Path::new("reference.html"),
            b"<h1>Design</h1><p>Overview</p><h2>Interface</h2><p>Details</p>",
        )
        .expect("extract HTML locators");

        assert_eq!(
            extraction.locators,
            vec![
                EvidenceLocatorCandidate::Html {
                    heading_path: "Design".into(),
                    paragraph: 1,
                },
                EvidenceLocatorCandidate::Html {
                    heading_path: "Design / Interface".into(),
                    paragraph: 2,
                },
            ]
        );
    }

    #[test]
    fn html_candidates_address_visible_rich_blocks_without_hidden_content() {
        let extraction = extract_locator_candidates(
            Path::new("reference.html"),
            b"<p>Lead-in</p><h1>Guide</h1><p>Overview</p><ul><li>first item</li></ul><table><tr><td>api</td><td>fast</td></tr></table><pre><code>let ready = true;</code></pre><script><p>hidden script</p></script><style>.hidden { display: none; }</style>",
        )
        .expect("extract HTML locators");

        assert_eq!(
            extraction.locators,
            vec![
                EvidenceLocatorCandidate::Html {
                    heading_path: "source-root".into(),
                    paragraph: 1,
                },
                EvidenceLocatorCandidate::Html {
                    heading_path: "Guide".into(),
                    paragraph: 2,
                },
                EvidenceLocatorCandidate::Html {
                    heading_path: "Guide".into(),
                    paragraph: 3,
                },
                EvidenceLocatorCandidate::Html {
                    heading_path: "Guide".into(),
                    paragraph: 4,
                },
                EvidenceLocatorCandidate::Html {
                    heading_path: "Guide".into(),
                    paragraph: 5,
                },
            ]
        );
    }

    #[test]
    fn api_candidates_preserve_rfc_6901_operation_and_component_pointers() {
        let extraction = extract_locator_candidates(
            Path::new("openapi.json"),
            br#"{"openapi":"3.1.0","paths":{"/units":{"get":{"operationId":"listUnits"}}},"components":{"schemas":{"Unit":{"type":"object"}}}}"#,
        )
        .expect("extract API locators");

        assert!(extraction
            .locators
            .contains(&EvidenceLocatorCandidate::Api {
                pointer: "/paths/~1units/get/operationId".into(),
            }));
        assert!(extraction
            .locators
            .contains(&EvidenceLocatorCandidate::Api {
                pointer: "/components/schemas/Unit/type".into(),
            }));
    }

    #[test]
    fn archive_candidates_preserve_admitted_member_paths() {
        let extraction = graphoxide_core::Extraction {
            nodes: vec![
                Node {
                    id: "member".into(),
                    label: "specs/interface.json".into(),
                    file_type: "document".into(),
                    source_file: "bundle.zip".into(),
                    source_location: None,
                    community: None,
                    extra: BTreeMap::from([("type".into(), "container_member".into())]),
                },
                Node {
                    id: "nested-member".into(),
                    label: "docs/readme.txt".into(),
                    file_type: "document".into(),
                    source_file: "bundle.zip!/archives/reference.tar".into(),
                    source_location: None,
                    community: None,
                    extra: BTreeMap::from([("type".into(), "container_member".into())]),
                },
                Node {
                    id: "directory".into(),
                    label: "docs".into(),
                    file_type: "document".into(),
                    source_file: "bundle.zip".into(),
                    source_location: None,
                    community: None,
                    extra: BTreeMap::from([
                        ("type".into(), "container_member".into()),
                        ("member_kind".into(), "directory".into()),
                    ]),
                },
            ],
            edges: Vec::new(),
            hyperedges: Vec::new(),
        };

        assert_eq!(
            archive_locator_candidates_with_limit(&extraction).0,
            vec![
                EvidenceLocatorCandidate::Archive {
                    member: "archives/reference.tar!/docs/readme.txt".into(),
                },
                EvidenceLocatorCandidate::Archive {
                    member: "specs/interface.json".into(),
                },
            ]
        );
    }

    #[test]
    fn archive_evidence_status_reflects_owning_container_completeness() {
        let member = |source_file: &str| Node {
            id: format!("member:{source_file}"),
            label: "docs/guide.md".into(),
            file_type: "document".into(),
            source_file: source_file.into(),
            source_location: None,
            community: None,
            extra: BTreeMap::from([("type".into(), "container_member".into())]),
        };
        let root = |status: &str, extra: BTreeMap<String, Value>| Node {
            id: format!("root:{status}"),
            label: "bundle.zip".into(),
            file_type: "document".into(),
            source_file: "bundle.zip".into(),
            source_location: None,
            community: None,
            extra: BTreeMap::from([
                ("type".into(), "container".into()),
                ("inspection_status".into(), status.into()),
            ])
            .into_iter()
            .chain(extra)
            .collect(),
        };
        let fixture = |root: Node| graphoxide_core::Extraction {
            nodes: vec![root, member("bundle.zip")],
            edges: Vec::new(),
            hyperedges: Vec::new(),
        };

        assert_eq!(
            archive_evidence(&fixture(root("parsed", BTreeMap::new()))),
            Some((
                EvidenceExtractionStatus::Extracted,
                vec![EvidenceLocatorCandidate::Archive {
                    member: "docs/guide.md".into(),
                }],
                Vec::new(),
            ))
        );
        assert_eq!(
            archive_evidence(&fixture(root(
                "inventoryonly",
                BTreeMap::from([("omitted_member_count".into(), 1.into())]),
            ))),
            Some((
                EvidenceExtractionStatus::Partial,
                vec![EvidenceLocatorCandidate::Archive {
                    member: "docs/guide.md".into(),
                }],
                vec!["archive-omitted-members".into()],
            ))
        );
        assert_eq!(
            archive_evidence(&fixture(root(
                "inventoryonly",
                BTreeMap::from([("recursive_dispatch_status".into(), "recursion_limit".into(),)]),
            ))),
            Some((
                EvidenceExtractionStatus::Partial,
                vec![EvidenceLocatorCandidate::Archive {
                    member: "docs/guide.md".into(),
                }],
                vec!["archive-recursion-limit".into()],
            ))
        );
        assert_eq!(
            archive_evidence(&fixture(root(
                "parsed",
                BTreeMap::from([("sensitive_member_count".into(), 1.into())]),
            ))),
            Some((
                EvidenceExtractionStatus::Partial,
                vec![EvidenceLocatorCandidate::Archive {
                    member: "docs/guide.md".into(),
                }],
                vec!["archive-sensitive-members".into()],
            ))
        );
        assert_eq!(
            archive_evidence(&fixture(root("rejected", BTreeMap::new()))),
            Some((
                EvidenceExtractionStatus::InventoryOnly,
                vec![EvidenceLocatorCandidate::Archive {
                    member: "docs/guide.md".into(),
                }],
                Vec::new(),
            ))
        );
        assert_eq!(
            archive_evidence(&fixture(root(
                "inventoryonly",
                BTreeMap::from([(
                    "diagnostics".into(),
                    serde_json::json!(["unsupportedarchiveformat"]),
                )]),
            ))),
            Some((
                EvidenceExtractionStatus::InventoryOnly,
                vec![EvidenceLocatorCandidate::Archive {
                    member: "docs/guide.md".into(),
                }],
                vec!["unsupportedarchiveformat".into()],
            ))
        );
    }

    #[test]
    fn archive_locator_overflow_is_explicitly_partial_with_a_retry_route() {
        let root = Node {
            id: "archive".into(),
            label: "bundle.zip".into(),
            file_type: "document".into(),
            source_file: "bundle.zip".into(),
            source_location: None,
            community: None,
            extra: BTreeMap::from([
                ("type".into(), "container".into()),
                ("inspection_status".into(), "parsed".into()),
            ]),
        };
        let members = (0..=MAX_LOCATORS)
            .map(|index| Node {
                id: format!("member-{index}"),
                label: format!("docs/{index:04}.txt"),
                file_type: "document".into(),
                source_file: "bundle.zip".into(),
                source_location: None,
                community: None,
                extra: BTreeMap::from([("type".into(), "container_member".into())]),
            })
            .collect::<Vec<_>>();
        let extraction = graphoxide_core::Extraction {
            nodes: std::iter::once(root).chain(members).collect(),
            edges: Vec::new(),
            hyperedges: Vec::new(),
        };

        let (status, locators, diagnostics) =
            archive_evidence(&extraction).expect("archive evidence");
        assert_eq!(status, EvidenceExtractionStatus::Partial);
        assert_eq!(locators.len(), MAX_LOCATORS);
        assert!(diagnostics.contains(&"locator-limit".to_owned()));
        let report = evidence_extraction_result(status, locators, diagnostics);
        assert_eq!(report.blocker.as_deref(), Some("locator-limit"));
        assert_eq!(report.retry_route.as_deref(), Some("raise-locator-limit"));
    }

    #[test]
    fn pdf_attachment_evidence_keeps_pages_and_every_archive_partition() {
        let node = |id: &str,
                    label: &str,
                    source_file: &str,
                    kind: &str,
                    extra: BTreeMap<String, Value>| Node {
            id: id.into(),
            label: label.into(),
            file_type: "document".into(),
            source_file: source_file.into(),
            source_location: None,
            community: None,
            extra: BTreeMap::from([("type".into(), kind.into())])
                .into_iter()
                .chain(extra)
                .collect(),
        };
        let extraction = graphoxide_core::Extraction {
            nodes: vec![
                node(
                    "pdf",
                    "reference.pdf",
                    "reference.pdf",
                    "pdf_document",
                    BTreeMap::new(),
                ),
                node(
                    "page",
                    "Page 1",
                    "reference.pdf",
                    "pdf_page",
                    BTreeMap::from([("page_number".into(), 1.into())]),
                ),
                node(
                    "attachment",
                    "attachments.zip",
                    "reference.pdf",
                    "container_member",
                    BTreeMap::new(),
                ),
                node(
                    "archive",
                    "attachments.zip",
                    "reference.pdf!/attachments.zip",
                    "container",
                    BTreeMap::from([("inspection_status".into(), "parsed".into())]),
                ),
                node(
                    "member",
                    "specs/interface.json",
                    "reference.pdf!/attachments.zip",
                    "container_member",
                    BTreeMap::new(),
                ),
            ],
            edges: Vec::new(),
            hyperedges: Vec::new(),
        };

        assert_eq!(
            archive_evidence(&extraction),
            Some((
                EvidenceExtractionStatus::Extracted,
                vec![
                    EvidenceLocatorCandidate::Pdf { page: 1 },
                    EvidenceLocatorCandidate::Archive {
                        member: "attachments.zip".into(),
                    },
                    EvidenceLocatorCandidate::Archive {
                        member: "attachments.zip!/specs/interface.json".into(),
                    },
                ],
                Vec::new(),
            ))
        );
    }

    #[test]
    fn blocked_pdf_attachment_keeps_its_partition_and_declared_retry_route() {
        let node = |id: &str, label: &str, kind: &str, extra: BTreeMap<String, Value>| Node {
            id: id.into(),
            label: label.into(),
            file_type: "document".into(),
            source_file: "reference.pdf".into(),
            source_location: None,
            community: None,
            extra: BTreeMap::from([("type".into(), kind.into())])
                .into_iter()
                .chain(extra)
                .collect(),
        };
        let extraction = graphoxide_core::Extraction {
            nodes: vec![
                node(
                    "pdf",
                    "reference.pdf",
                    "pdf_document",
                    BTreeMap::from([("parse_status".into(), "partial".into())]),
                ),
                node(
                    "page",
                    "Page 1",
                    "pdf_page",
                    BTreeMap::from([("page_number".into(), 1.into())]),
                ),
                node(
                    "attachment",
                    "locked.zip",
                    "container_member",
                    BTreeMap::from([
                        ("member_kind".into(), "pdf_attachment".into()),
                        ("dispatch_status".into(), "pdf_attachment_unreadable".into()),
                        ("blocker".into(), "pdf-attachment-unreadable".into()),
                        ("retry_route".into(), "repair-pdf-attachment".into()),
                    ]),
                ),
            ],
            edges: Vec::new(),
            hyperedges: Vec::new(),
        };

        let (status, locators, diagnostics) =
            archive_evidence(&extraction).expect("PDF attachment evidence");
        assert_eq!(
            (status, &locators, &diagnostics),
            (
                EvidenceExtractionStatus::Partial,
                &vec![
                    EvidenceLocatorCandidate::Pdf { page: 1 },
                    EvidenceLocatorCandidate::Archive {
                        member: "locked.zip".into(),
                    },
                ],
                &vec!["pdf-attachment-unreadable".into()],
            )
        );
        assert_eq!(
            registered_reprocessing_route(&extraction),
            (
                Some("pdf-attachment-unreadable".into()),
                Some("repair-pdf-attachment".into()),
            )
        );
        let report = evidence_extraction_result(status, locators, diagnostics);
        assert_eq!(report.blocker.as_deref(), Some("pdf-attachment-unreadable"));
        assert_eq!(report.retry_route.as_deref(), Some("repair-pdf-attachment"));
    }

    #[test]
    fn oversized_pdf_attachment_has_a_declared_retry_route() {
        assert_eq!(
            reprocessing_route(&["pdf-attachment-byte-limit".into()]),
            Some((
                "pdf-attachment-byte-limit",
                "raise-pdf-attachment-byte-limit",
            ))
        );
    }

    #[test]
    fn sensitive_pdf_attachment_has_an_authorization_route() {
        assert_eq!(
            reprocessing_route(&["pdf-attachment-sensitive-path".into()]),
            Some((
                "pdf-attachment-sensitive-path",
                "authorize-sensitive-member-processing",
            ))
        );
    }
}
