//! Bounded, byte-only PDF text and page extraction.
//!
//! This module deliberately supports a conservative PDF subset: classic
//! cross-reference tables whose page/content/font objects are direct objects.
//! It never renders pages, follows actions, opens attachments, performs I/O,
//! or delegates to an external program. Unsupported representations fail
//! closed before semantic facts are published.

use flate2::bufread::ZlibDecoder;
use graphoxide_core::{make_id, sanitize_metadata_string, Confidence, Edge, Extraction, Node};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    ops::Range,
    path::Path,
};

const MIB: usize = 1024 * 1024;
const MAX_PARSER_ALLOWANCE_INPUT_BYTES: usize = 128 * MIB;
const FIXED_ALLOWANCE_BYTES: usize = 64 * 1024;
// Source-proportional scratch for the PDF text parser: the source bytes
// themselves (retained once) plus the worst-case object-table/token scratch
// (bounded by one more source copy). Decoded streams and extracted text are
// separate capped classes reserved out of the same allowance below, so the
// source term is not inflated to cover them. Matches the container-backed
// Office parser, whose compressed sources likewise do not describe peak
// decoded scratch (the generic source x16 admission estimate does not apply
// to this format).
const SOURCE_SCRATCH_MULTIPLIER: usize = 2;
const RETAINED_BYTES_PER_FACT: usize = 2 * 1024;
const DECODE_CHUNK_BYTES: usize = 16 * 1024;
// Content operations are PDF operator keywords. Scale aggregate work with the
// validated page count, while keeping an independent document-wide bomb cap.
const CONTENT_OPERATIONS_PER_PAGE: usize = 2_048;
const MAX_CONTENT_OPERATIONS_HARD_CAP: usize = 1_000_000;
// Visual resource dictionaries are source-controlled. Inspect a bounded
// prefix and mark the inventory partial rather than publishing a subset count.
const MAX_VISUAL_XOBJECTS_PER_PAGE: usize = 256;
const MAX_CAPTION_CANDIDATES_PER_KIND: usize = 16;
const MAX_CAPTION_CANDIDATE_BYTES: usize = 512;
const MAX_OUTLINE_DEPTH: usize = 32;
const MAX_OUTLINE_TITLE_BYTES: usize = 512;
const MAX_OUTLINE_PATH_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ObjectId {
    number: u32,
    generation: u16,
}

#[derive(Debug, Clone, PartialEq)]
enum PdfValue {
    Null,
    Boolean,
    Integer(i64),
    Real,
    Name(Vec<u8>),
    String(Vec<u8>),
    Array(Vec<PdfValue>),
    Dictionary(BTreeMap<Vec<u8>, PdfValue>),
    Reference(ObjectId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamFilter {
    Raw,
    Flate,
    /// A filter this extractor does not decode (e.g. `DCTDecode` image data).
    /// Recording the stream is harmless — it is only rejected if a code path
    /// actually tries to decode it (`decode_stream_bytes`), so documents that
    /// merely *contain* undecoded streams (image XObjects, font file streams,
    /// embedded files) still yield their text.
    Unsupported,
}

#[derive(Debug, Clone)]
struct StreamSpec {
    encoded: Range<usize>,
    filter: StreamFilter,
}

#[derive(Debug, Clone)]
struct PdfObject {
    value: PdfValue,
    stream: Option<StreamSpec>,
}

#[derive(Debug, Default)]
struct PdfPageVisualInventory {
    media_box: Option<(i64, i64)>,
    xobject_resource_count: Option<usize>,
    image_xobject_count: Option<usize>,
    form_xobject_count: Option<usize>,
    image_xobject_dimensions: Vec<(i64, i64)>,
    xobject_resources_limited: bool,
}

#[derive(Debug)]
struct PdfPageMaterial {
    number: usize,
    text: String,
    outline: Option<PdfPageOutline>,
    visual: PdfPageVisualInventory,
}

#[derive(Debug, Clone)]
struct PdfPageOutline {
    heading: String,
    path: String,
}

/// A member of an object stream, decoded in place from the packed
/// `id offset` header list. The raw `id gen obj` header is stripped and the
/// value is re-parsed from the member's byte range.

#[derive(Debug)]
struct ParsedPdf {
    objects: BTreeMap<ObjectId, PdfObject>,
    trailer: BTreeMap<Vec<u8>, PdfValue>,
}

#[derive(Debug, Clone, Copy)]
struct XrefEntry {
    id: ObjectId,
    offset: usize,
    /// The xref section that admitted this object. Its object span must not
    /// cross into a later incremental revision.
    section_end: usize,
    /// Whether this is the effective revision for its object number. Older
    /// entries remain as physical span boundaries but are never parsed.
    active: bool,
}

#[derive(Debug)]
struct XrefTable {
    entries: Vec<XrefEntry>,
    trailer: BTreeMap<Vec<u8>, PdfValue>,
    counters: ParseCounters,
    /// Xref-stream object ids. Their dictionaries were consumed as trailers,
    /// so object parsing must not try to parse their trailing revision spans.
    xref_object_ids: BTreeSet<ObjectId>,
    /// Non-head free entries. Incremental free-object updates need tombstone
    /// semantics, which this bounded merger deliberately rejects rather than
    /// resurrecting a predecessor's object.
    free_object_numbers: BTreeSet<u32>,
    /// Type-2 cross-reference members (member id -> owning object-stream id
    /// and index within it), populated only by cross-reference streams.
    objstm_members: BTreeMap<ObjectId, (u32, usize)>,
}

/// Explicit ceilings for one PDF parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PdfLimits {
    pub(crate) max_input_bytes: usize,
    pub(crate) max_objects: usize,
    pub(crate) max_pages: usize,
    pub(crate) max_page_tree_depth: usize,
    pub(crate) max_reference_depth: usize,
    pub(crate) max_object_nesting: usize,
    pub(crate) max_tokens: usize,
    pub(crate) max_tokens_per_object: usize,
    pub(crate) max_container_entries: usize,
    pub(crate) max_container_entries_per_object: usize,
    pub(crate) max_streams: usize,
    pub(crate) max_stream_input_bytes: usize,
    pub(crate) max_stream_decoded_bytes: usize,
    pub(crate) max_total_decoded_bytes: usize,
    pub(crate) max_expansion_ratio: usize,
    pub(crate) max_content_operations: usize,
    pub(crate) max_content_nesting: usize,
    pub(crate) max_text_bytes_per_page: usize,
    pub(crate) max_total_text_bytes: usize,
    pub(crate) max_metadata_bytes: usize,
    pub(crate) max_attachments: usize,
    pub(crate) max_attachment_name_bytes: usize,
    pub(crate) max_attachment_tree_depth: usize,
    pub(crate) max_facts: usize,
}

impl Default for PdfLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 16 * MIB,
            max_objects: 16 * 1024,
            max_pages: 1_024,
            max_page_tree_depth: 32,
            max_reference_depth: 32,
            max_object_nesting: 32,
            max_tokens: 262_144,
            max_tokens_per_object: 65_536,
            max_container_entries: 131_072,
            max_container_entries_per_object: 32_768,
            max_streams: 2_048,
            max_stream_input_bytes: 4 * MIB,
            max_stream_decoded_bytes: 4 * MIB,
            max_total_decoded_bytes: 16 * MIB,
            max_expansion_ratio: 64,
            max_content_operations: 1_000_000,
            max_content_nesting: 32,
            // A page fact remains comfortably below the graph's one-MiB
            // serialized-fact boundary even after JSON escaping/attributes.
            max_text_bytes_per_page: 256 * 1024,
            max_total_text_bytes: 4 * MIB,
            max_metadata_bytes: 64 * 1024,
            max_attachments: 64,
            max_attachment_name_bytes: 4 * 1024,
            max_attachment_tree_depth: 8,
            // One document node plus a node and containment edge per page.
            max_facts: 2_049,
        }
    }
}

#[derive(Debug)]
pub(crate) struct PdfExtraction {
    pub(crate) extraction: Extraction,
    pub(crate) attachments: Vec<PdfAttachment>,
}

/// One directly reusable raster embedded in a specific PDF page.
///
/// The artifact is deliberately limited to a complete JPEG XObject. It is not
/// a rendered page: Graphoxide never executes PDF content or delegates page
/// rendering to an external program merely to prepare vision enrichment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfPageVisualArtifact {
    /// One-indexed PDF page number.
    pub page: u32,
    /// Zero-indexed ordinal among image XObjects on this page, in stable PDF
    /// resource-name order.
    pub asset_index: u32,
    /// Exact immutable PDF-object locator where the source uses an indirect
    /// XObject; otherwise a stable encoded resource-name locator.
    pub asset_locator: String,
    /// The vision transport media type for [`Self::bytes`].
    pub media_type: &'static str,
    /// Complete encoded JPEG bytes copied from the admitted immutable source.
    pub bytes: Vec<u8>,
}

/// A source-free explanation for why one PDF page cannot yield a safe visual
/// enrichment artifact under the requested bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfPageVisualArtifactBlocker {
    /// The PDF could not be safely parsed through the bounded local route.
    SourceRejected,
    /// The requested page is outside the admitted document page tree.
    PageUnavailable,
    /// The XObject resource inventory exceeded the bounded complete route.
    VisualInventoryPartial,
    /// Resource resolution failed before a complete visual inventory existed.
    VisualInventoryUnavailable,
    /// The page has no image XObject.
    NoDirectImage,
    /// The page has images but none is a direct JPEG XObject.
    UnsupportedImageEncoding,
    /// A direct JPEG exists but exceeds the caller's explicit artifact cap.
    ByteLimit,
    /// A claimed direct JPEG XObject does not contain a complete JPEG image.
    InvalidImage,
}

/// Source-free reason an exact embedded PDF attachment cannot be returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfEmbeddedAttachmentBlocker {
    /// The PDF cannot be admitted through the bounded empty-password route.
    SourceRejected,
    /// The requested path is not canonical.
    InvalidPath,
    /// Policy forbids exposing this attachment path.
    SensitivePath,
    /// The PDF does not contain the requested admitted attachment.
    AttachmentUnavailable,
    /// The selected embedded-file declaration or stream is malformed.
    Unreadable,
    /// The exact attachment exceeds a parser or caller byte ceiling.
    ByteLimit,
    /// The PDF attachment name tree exceeds the admitted member count.
    CountLimit,
    /// The PDF attachment name tree exceeds the admitted nesting depth.
    DepthLimit,
    /// Attachment resolution was cancelled before completion.
    Cancelled,
}

impl PdfEmbeddedAttachmentBlocker {
    /// Stable, source-free blocker code for material-resolution diagnostics.
    pub const fn code(self) -> &'static str {
        match self {
            Self::SourceRejected => "pdf-attachment-source-rejected",
            Self::InvalidPath => "pdf-attachment-path-invalid",
            Self::SensitivePath => "pdf-attachment-sensitive-path",
            Self::AttachmentUnavailable => "pdf-attachment-unavailable",
            Self::Unreadable => "pdf-attachment-unreadable",
            Self::ByteLimit => "pdf-attachment-byte-limit",
            Self::CountLimit => "pdf-attachment-count-limit",
            Self::DepthLimit => "pdf-attachment-depth-limit",
            Self::Cancelled => "cancelled",
        }
    }
}

impl PdfPageVisualArtifactBlocker {
    /// Stable, source-free blocker code for coverage and retry reporting.
    pub const fn code(self) -> &'static str {
        match self {
            Self::SourceRejected => "pdf-visual-source-rejected",
            Self::PageUnavailable => "pdf-visual-page-unavailable",
            Self::VisualInventoryPartial => "pdf-visual-inventory-partial",
            Self::VisualInventoryUnavailable => "pdf-visual-inventory-unavailable",
            Self::NoDirectImage => "pdf-visual-no-direct-image",
            Self::UnsupportedImageEncoding => "pdf-visual-image-encoding-unsupported",
            Self::ByteLimit => "pdf-visual-artifact-byte-limit",
            Self::InvalidImage => "pdf-visual-image-invalid",
        }
    }
}

#[derive(Debug)]
pub(crate) struct PdfAttachment {
    pub(crate) path: String,
    pub(crate) bytes: Option<Vec<u8>>,
    pub(crate) encoded_bytes: u64,
    pub(crate) blocker: Option<PdfAttachmentBlocker>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PdfAttachmentBlocker {
    Unreadable,
    ByteLimit,
    CountLimit,
    DepthLimit,
    Cancelled,
}

impl PdfAttachmentBlocker {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::Unreadable => "pdf-attachment-unreadable",
            Self::ByteLimit => "pdf-attachment-byte-limit",
            Self::CountLimit => "pdf-attachment-count-limit",
            Self::DepthLimit => "pdf-attachment-depth-limit",
            Self::Cancelled => "cancelled",
        }
    }

    pub(crate) const fn retry_route(self) -> &'static str {
        match self {
            Self::Unreadable => "repair-pdf-attachment",
            Self::ByteLimit => "raise-pdf-attachment-byte-limit",
            Self::CountLimit => "raise-pdf-attachment-count-limit",
            Self::DepthLimit => "raise-pdf-attachment-depth-limit",
            Self::Cancelled => "retry-extraction",
        }
    }
}

impl PdfLimits {
    fn effective_content_operation_limit(self, page_count: usize) -> Result<usize, PdfError> {
        let page_limit = page_count
            .checked_mul(CONTENT_OPERATIONS_PER_PAGE)
            .ok_or(PdfError::ContentLimit)?;
        Ok(self
            .max_content_operations
            .min(page_limit)
            .min(MAX_CONTENT_OPERATIONS_HARD_CAP))
    }

    /// Tighten PDF-specific retained/decode ceilings to one isolated parser
    /// allowance. The PDF adapter owns this scratch proof like the
    /// container-backed formats: a PDF's compressed stream sources do not
    /// describe peak decoded scratch, so the generic source x16 admission
    /// estimate does not apply and the adapter installs its own fact plan
    /// (`ParserPlan::for_fact_limit`) from the ceiling derived here.
    pub(crate) fn for_parser_allowance(allowance_bytes: usize, source_len: usize) -> Option<Self> {
        let mut limits = Self::default();
        if source_len > MAX_PARSER_ALLOWANCE_INPUT_BYTES {
            return None;
        }
        // The caller has already admitted the source and supplied a complete
        // parser-scratch allowance. Keep the normal default conservative, but
        // let this explicitly budgeted path process the full admitted source.
        limits.max_input_bytes = source_len;
        let source_scratch = source_len
            .checked_mul(SOURCE_SCRATCH_MULTIPLIER)?
            .checked_add(FIXED_ALLOWANCE_BYTES)?;
        let available = allowance_bytes.checked_sub(source_scratch)?;
        let full_page_fact_bytes = limits.max_facts.checked_mul(RETAINED_BYTES_PER_FACT)?;
        let full_page_plan = full_page_fact_bytes
            .checked_add(limits.max_total_text_bytes)?
            .checked_add(64 * 1024)?;
        let (decoded, text, retained) = if available >= full_page_plan {
            // Keep the advertised page coverage when the allowance can also
            // retain the full text ceiling and a minimally useful decode
            // budget. This avoids making compact, page-dense PDFs depend on
            // an incidental proportional split of the parser arena.
            let text = limits.max_total_text_bytes;
            let decoded = limits
                .max_total_decoded_bytes
                .min(available - full_page_fact_bytes - text);
            let retained = available - decoded - text;
            (decoded, text, retained)
        } else {
            let decoded = limits.max_total_decoded_bytes.min(available / 2);
            let text = limits.max_total_text_bytes.min(available / 4);
            let retained = available.checked_sub(decoded)?.checked_sub(text)?;
            (decoded, text, retained)
        };
        let facts = limits.max_facts.min(retained / RETAINED_BYTES_PER_FACT);
        if decoded < 64 * 1024 || text < 4 * 1024 || facts < 3 {
            return None;
        }
        limits.max_total_decoded_bytes = decoded;
        limits.max_stream_decoded_bytes = limits.max_stream_decoded_bytes.min(decoded);
        limits.max_total_text_bytes = text;
        limits.max_text_bytes_per_page = limits.max_text_bytes_per_page.min(text);
        limits.max_metadata_bytes = limits.max_metadata_bytes.min(text / 4);
        limits.max_facts = facts;
        limits.max_pages = limits.max_pages.min(facts.saturating_sub(1) / 2);
        (limits.max_pages > 0).then_some(limits)
    }
}

/// Stable, non-source-bearing rejection classes for adapter diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PdfError {
    #[error("PDF input exceeds its byte ceiling")]
    InputLimit,
    #[error("PDF parsing was cancelled")]
    Cancelled,
    #[error("PDF header is malformed")]
    InvalidHeader,
    #[error("PDF requires an unsupported cross-reference representation")]
    UnsupportedXref,
    #[error("incrementally updated PDFs are unsupported")]
    UnsupportedIncremental,
    #[error("PDF uses a hybrid cross-reference table and stream")]
    HybridXref,
    #[error("PDF object streams are unsupported")]
    UnsupportedObjectStream,
    #[error("encrypted PDFs are unsupported")]
    Encrypted,
    #[error("active or externally resolved PDF content is unsupported")]
    ActiveContent,
    #[error("PDF syntax is malformed")]
    Malformed,
    #[error("PDF object ceiling was exceeded")]
    ObjectLimit,
    #[error("PDF token or container ceiling was exceeded")]
    TokenLimit,
    #[error("PDF nesting ceiling was exceeded")]
    NestingLimit,
    #[error("PDF page ceiling was exceeded")]
    PageLimit,
    #[error("PDF reference ceiling or cycle was encountered")]
    ReferenceLimit,
    #[error("PDF stream boundary is invalid")]
    InvalidStream,
    #[error("PDF stream representation is unsupported")]
    UnsupportedFilter,
    #[error("PDF decoded-stream ceiling was exceeded")]
    DecompressionLimit,
    #[error("PDF stream expansion ratio was exceeded")]
    ExpansionRatioLimit,
    #[error("PDF content operation ceiling was exceeded")]
    ContentLimit,
    #[error("PDF inline images are unsupported")]
    InlineImage,
    #[error("PDF font representation is unsupported")]
    UnsupportedFont,
    #[error("PDF text ceiling was exceeded")]
    TextLimit,
    #[error("PDF metadata ceiling was exceeded")]
    MetadataLimit,
    #[error("PDF fact ceiling was exceeded")]
    FactLimit,
}

impl PdfError {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::InputLimit => "pdf_input_limit",
            Self::Cancelled => "cancelled",
            Self::InvalidHeader => "pdf_invalid_header",
            Self::UnsupportedXref => "pdf_unsupported_xref",
            Self::UnsupportedIncremental => "pdf_incremental_unsupported",
            Self::HybridXref => "pdf_hybrid_xref_unsupported",
            Self::UnsupportedObjectStream => "pdf_object_stream_unsupported",
            Self::Encrypted => "pdf_encrypted",
            Self::ActiveContent => "pdf_active_content_unsupported",
            Self::Malformed => "pdf_malformed",
            Self::ObjectLimit => "pdf_object_limit",
            Self::TokenLimit => "pdf_token_limit",
            Self::NestingLimit => "pdf_nesting_limit",
            Self::PageLimit => "pdf_page_limit",
            Self::ReferenceLimit => "pdf_reference_limit",
            Self::InvalidStream => "pdf_stream_invalid",
            Self::UnsupportedFilter => "pdf_filter_unsupported",
            Self::DecompressionLimit => "pdf_decompression_limit",
            Self::ExpansionRatioLimit => "pdf_expansion_ratio_limit",
            Self::ContentLimit => "pdf_content_limit",
            Self::InlineImage => "pdf_inline_image_unsupported",
            Self::UnsupportedFont => "pdf_font_unsupported",
            Self::TextLimit => "pdf_text_limit",
            Self::MetadataLimit => "pdf_metadata_limit",
            Self::FactLimit => "pdf_fact_limit",
        }
    }
}

/// Extract a bounded semantic page graph from ready PDF bytes.
pub(crate) fn extract_pdf_bytes(
    path: &Path,
    source_file: &str,
    source: &[u8],
    limits: PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<Extraction, PdfError> {
    extract_pdf(path, source_file, source, limits, cancelled).map(|result| result.extraction)
}

pub(crate) fn extract_pdf(
    path: &Path,
    source_file: &str,
    source: &[u8],
    limits: PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<PdfExtraction, PdfError> {
    match extract_pdf_bytes_once(path, source_file, source, limits, cancelled) {
        Err(PdfError::HybridXref) => {
            extract_lopdf_pdf_fallback(path, source_file, source, limits, cancelled)
        }
        Err(PdfError::Malformed | PdfError::ObjectLimit) if source_has_hybrid_xref(source) => {
            extract_lopdf_pdf_fallback(path, source_file, source, limits, cancelled)
        }
        Err(PdfError::Encrypted) => {
            let (document, scan) = decrypt_empty_password_pdf(source, limits, cancelled)?;
            let mut extraction =
                extract_decrypted_pdf(path, source_file, &document, scan, limits, cancelled)?;
            let attachments = if scan.embedded_files {
                extract_embedded_attachments(&document, &mut extraction, limits, cancelled)?
            } else {
                Vec::new()
            };
            Ok(PdfExtraction {
                extraction,
                attachments,
            })
        }
        Ok(extraction) => Ok(PdfExtraction {
            extraction,
            attachments: Vec::new(),
        }),
        Err(error) => Err(error),
    }
}

fn source_has_hybrid_xref(source: &[u8]) -> bool {
    source
        .windows(b"/XRefStm".len())
        .any(|window| window == b"/XRefStm")
}

/// Safely route the PDF hybrid-reference form through the already bounded
/// `lopdf` path. The custom parser remains the admission authority for all
/// other PDF representations.
fn extract_lopdf_pdf_fallback(
    path: &Path,
    source_file: &str,
    source: &[u8],
    limits: PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<PdfExtraction, PdfError> {
    check_cancelled(cancelled)?;
    let document = lopdf::Document::load_mem_with_options(
        source,
        lopdf::LoadOptions {
            max_decompressed_size: Some(limits.max_stream_decoded_bytes),
            ..Default::default()
        },
    )
    .map_err(|_| PdfError::UnsupportedXref)?;
    if document.is_encrypted() {
        return Err(PdfError::Encrypted);
    }
    if document.objects.len() > limits.max_objects {
        return Err(PdfError::ObjectLimit);
    }
    let scan = validate_decrypted_document(&document, &limits, cancelled)?;
    let mut extraction =
        extract_decrypted_pdf(path, source_file, &document, scan, limits, cancelled)?;
    let attachments = if scan.embedded_files {
        extract_embedded_attachments(&document, &mut extraction, limits, cancelled)?
    } else {
        Vec::new()
    };
    Ok(PdfExtraction {
        extraction,
        attachments,
    })
}

/// Return one exact, bounded attachment from an admitted empty-password PDF.
///
/// The attachment name is resolved with the same canonicalization and
/// duplicate-name rules as semantic PDF extraction. Only the selected stream
/// is decoded; unrelated attachment payloads remain unopened.
pub fn pdf_embedded_attachment(
    source: &[u8],
    attachment_path: &str,
    max_bytes: usize,
) -> Result<Vec<u8>, PdfEmbeddedAttachmentBlocker> {
    if source.is_empty()
        || source.len() > MAX_PARSER_ALLOWANCE_INPUT_BYTES
        || max_bytes == 0
        || crate::containers::normalized_member_path(
            attachment_path,
            PdfLimits::default().max_attachment_name_bytes,
        )
        .as_deref()
            != Some(attachment_path)
    {
        return Err(PdfEmbeddedAttachmentBlocker::InvalidPath);
    }
    if crate::containers::is_sensitive_archive_member_path(attachment_path) {
        return Err(PdfEmbeddedAttachmentBlocker::SensitivePath);
    }
    let mut limits = PdfLimits {
        max_input_bytes: source.len(),
        ..PdfLimits::default()
    };
    limits.max_stream_decoded_bytes = limits.max_stream_decoded_bytes.min(max_bytes);
    limits.max_total_decoded_bytes = limits.max_total_decoded_bytes.min(max_bytes);
    let (document, scan) = decrypt_empty_password_pdf(source, limits, None)
        .map_err(|_| PdfEmbeddedAttachmentBlocker::SourceRejected)?;
    if !scan.embedded_files {
        return Err(PdfEmbeddedAttachmentBlocker::AttachmentUnavailable);
    }
    let tree = embedded_file_name_tree(&document).map_err(pdf_embedded_attachment_blocker)?;
    let mut entries = Vec::new();
    let mut visited = BTreeSet::new();
    let tree_blocker =
        collect_embedded_file_entries(&document, tree, 0, &mut visited, &mut entries, limits, None)
            .err()
            .map(pdf_embedded_attachment_blocker);

    let mut used_paths = BTreeSet::new();
    let attachment_count = entries.len();
    for (index, (name, filespec)) in entries.into_iter().enumerate() {
        let mut path = decode_info_text_string(&name, limits.max_attachment_name_bytes)
            .ok()
            .map(sanitize_metadata_string)
            .and_then(|name| {
                crate::containers::normalized_member_path(&name, limits.max_attachment_name_bytes)
            })
            .unwrap_or_else(|| format!("attachment-{:06}", index + 1));
        let duplicate = !used_paths.insert(path.clone());
        if duplicate {
            let mut candidate_index = index;
            loop {
                let candidate = format!("attachment-{:06}", candidate_index + 1);
                if used_paths.insert(candidate.clone()) {
                    path = candidate;
                    break;
                }
                candidate_index = candidate_index
                    .checked_add(attachment_count)
                    .ok_or(PdfEmbeddedAttachmentBlocker::AttachmentUnavailable)?;
            }
        }
        if path != attachment_path {
            continue;
        }
        if duplicate {
            return Err(PdfEmbeddedAttachmentBlocker::Unreadable);
        }
        let mut total_decoded = 0;
        let mut decode_budget = AttachmentDecodeBudget {
            total_decoded: &mut total_decoded,
            total_limit: limits.max_total_decoded_bytes,
        };
        let attachment = extract_embedded_file(
            &document,
            index,
            &name,
            &filespec,
            &mut decode_budget,
            limits,
            None,
        )
        .map_err(|_| PdfEmbeddedAttachmentBlocker::SourceRejected)?;
        return match (attachment.bytes, attachment.blocker) {
            (Some(bytes), None) if bytes.len() <= max_bytes => Ok(bytes),
            (_, Some(blocker)) => Err(pdf_embedded_attachment_blocker(blocker)),
            _ => Err(PdfEmbeddedAttachmentBlocker::ByteLimit),
        };
    }
    Err(tree_blocker.unwrap_or(PdfEmbeddedAttachmentBlocker::AttachmentUnavailable))
}

fn pdf_embedded_attachment_blocker(blocker: PdfAttachmentBlocker) -> PdfEmbeddedAttachmentBlocker {
    match blocker {
        PdfAttachmentBlocker::ByteLimit => PdfEmbeddedAttachmentBlocker::ByteLimit,
        PdfAttachmentBlocker::Unreadable => PdfEmbeddedAttachmentBlocker::Unreadable,
        PdfAttachmentBlocker::CountLimit => PdfEmbeddedAttachmentBlocker::CountLimit,
        PdfAttachmentBlocker::DepthLimit => PdfEmbeddedAttachmentBlocker::DepthLimit,
        PdfAttachmentBlocker::Cancelled => PdfEmbeddedAttachmentBlocker::Cancelled,
    }
}

/// Return one bounded, directly embedded JPEG visual for a PDF page.
///
/// This only accepts a complete JPEG image XObject already present in the
/// immutable source. Vector-only pages, non-JPEG pixel encodings, and partial
/// XObject inventories remain explicit blockers instead of being represented
/// as successful text-only extraction.
pub fn pdf_page_visual_artifact(
    source: &[u8],
    page: u32,
    max_bytes: usize,
) -> Result<PdfPageVisualArtifact, PdfPageVisualArtifactBlocker> {
    if max_bytes == 0 || source.is_empty() || source.len() > MAX_PARSER_ALLOWANCE_INPUT_BYTES {
        return Err(PdfPageVisualArtifactBlocker::ByteLimit);
    }
    let limits = PdfLimits {
        max_input_bytes: source.len(),
        ..PdfLimits::default()
    };
    match pdf_page_visual_artifact_once(source, page, max_bytes, &limits) {
        Err(PdfError::Encrypted) => {
            pdf_page_visual_artifact_decrypted(source, page, max_bytes, limits)
        }
        Ok(artifact) => Ok(artifact),
        Err(error) => Err(pdf_visual_artifact_blocker(error)),
    }
}

fn pdf_page_visual_artifact_once(
    source: &[u8],
    page: u32,
    max_bytes: usize,
    limits: &PdfLimits,
) -> Result<PdfPageVisualArtifact, PdfError> {
    validate_pdf_header(source)?;
    let xref = parse_xref(source, limits, None)?;
    let parsed = parse_indirect_objects(source, xref, limits, None)?;
    let page_ids = collect_page_ids(&parsed, limits, None)?;
    let page_index = usize::try_from(page.saturating_sub(1)).map_err(|_| PdfError::PageLimit)?;
    let page_id = *page_ids.get(page_index).ok_or(PdfError::PageLimit)?;
    pdf_page_visual_artifact_from_parsed(source, page, max_bytes, &parsed, page_id, limits)
}

fn pdf_page_visual_artifact_from_parsed(
    source: &[u8],
    page: u32,
    max_bytes: usize,
    parsed: &ParsedPdf,
    page_id: ObjectId,
    limits: &PdfLimits,
) -> Result<PdfPageVisualArtifact, PdfError> {
    let Some(PdfValue::Dictionary(resources)) =
        inherited_page_value(parsed, page_id, b"Resources", limits)
    else {
        return Err(PdfError::InlineImage);
    };
    let Some(xobjects) = resources.get(b"XObject".as_slice()) else {
        return Err(PdfError::InlineImage);
    };
    let Some(PdfValue::Dictionary(xobjects)) = resolve_value(parsed, xobjects, limits)
        .ok()
        .map(|(_, value)| value)
    else {
        return Err(PdfError::UnsupportedFont);
    };
    if xobjects.len() > MAX_VISUAL_XOBJECTS_PER_PAGE {
        return Err(PdfError::FactLimit);
    }

    let mut saw_image = false;
    let mut blocker = PdfPageVisualArtifactBlocker::UnsupportedImageEncoding;
    let mut asset_index = 0_u32;
    for (resource_name, xobject) in xobjects {
        let (object_id, value) = resolve_value(parsed, xobject, limits)?;
        let PdfValue::Dictionary(dictionary) = value else {
            continue;
        };
        if !dictionary_name_is(dictionary, b"Type", b"XObject")
            || !matches!(dictionary.get(b"Subtype".as_slice()), Some(PdfValue::Name(subtype)) if subtype == b"Image")
        {
            continue;
        }
        saw_image = true;
        let this_index = asset_index;
        asset_index = asset_index.checked_add(1).ok_or(PdfError::FactLimit)?;
        let Some(object_id) = object_id else {
            blocker = PdfPageVisualArtifactBlocker::VisualInventoryUnavailable;
            continue;
        };
        let Some(stream) = parsed
            .objects
            .get(&object_id)
            .and_then(|object| object.stream.as_ref())
        else {
            blocker = PdfPageVisualArtifactBlocker::VisualInventoryUnavailable;
            continue;
        };
        if !pdf_image_filter_is_direct_jpeg(dictionary) {
            continue;
        }
        let bytes = source
            .get(stream.encoded.clone())
            .ok_or(PdfError::InvalidStream)?;
        if bytes.len() > max_bytes {
            blocker = PdfPageVisualArtifactBlocker::ByteLimit;
            continue;
        }
        if !is_complete_jpeg(bytes) {
            blocker = PdfPageVisualArtifactBlocker::InvalidImage;
            continue;
        }
        return Ok(PdfPageVisualArtifact {
            page,
            asset_index: this_index,
            asset_locator: pdf_visual_asset_locator(object_id, resource_name),
            media_type: "image/jpeg",
            bytes: bytes.to_vec(),
        });
    }
    if !saw_image {
        Err(PdfError::InlineImage)
    } else {
        Err(pdf_visual_artifact_error(blocker))
    }
}

fn pdf_page_visual_artifact_decrypted(
    source: &[u8],
    page: u32,
    max_bytes: usize,
    limits: PdfLimits,
) -> Result<PdfPageVisualArtifact, PdfPageVisualArtifactBlocker> {
    let (document, _) =
        decrypt_empty_password_pdf(source, limits, None).map_err(pdf_visual_artifact_blocker)?;
    let page_id = document
        .get_pages()
        .get(&page)
        .copied()
        .ok_or(PdfPageVisualArtifactBlocker::PageUnavailable)?;
    let Some(resources) = decrypted_inherited_page_value(&document, page_id, b"Resources", &limits)
        .and_then(decrypted_dictionary)
    else {
        return Err(PdfPageVisualArtifactBlocker::NoDirectImage);
    };
    let Some(xobjects) = resources
        .get(b"XObject")
        .ok()
        .and_then(|value| decrypted_resolve_value(&document, value))
        .and_then(decrypted_dictionary)
    else {
        return Err(PdfPageVisualArtifactBlocker::VisualInventoryUnavailable);
    };
    if xobjects.len() > MAX_VISUAL_XOBJECTS_PER_PAGE {
        return Err(PdfPageVisualArtifactBlocker::VisualInventoryPartial);
    }

    let mut saw_image = false;
    let mut blocker = PdfPageVisualArtifactBlocker::UnsupportedImageEncoding;
    let mut asset_index = 0_u32;
    for (resource_name, xobject) in xobjects.iter() {
        let object_id = xobject.as_reference().ok();
        let Some(lopdf::Object::Stream(stream)) = decrypted_resolve_value(&document, xobject)
        else {
            continue;
        };
        if stream
            .dict
            .get(b"Type")
            .and_then(lopdf::Object::as_name)
            .ok()
            != Some(b"XObject")
            || stream
                .dict
                .get(b"Subtype")
                .and_then(lopdf::Object::as_name)
                .ok()
                != Some(b"Image")
        {
            continue;
        }
        saw_image = true;
        let this_index = asset_index;
        asset_index = asset_index
            .checked_add(1)
            .ok_or(PdfPageVisualArtifactBlocker::VisualInventoryPartial)?;
        if !decrypted_pdf_image_filter_is_direct_jpeg(&stream.dict) {
            continue;
        }
        if stream.content.len() > max_bytes {
            blocker = PdfPageVisualArtifactBlocker::ByteLimit;
            continue;
        }
        if !is_complete_jpeg(&stream.content) {
            blocker = PdfPageVisualArtifactBlocker::InvalidImage;
            continue;
        }
        let asset_locator = object_id.map_or_else(
            || format!("pdf-resource:{}", hex::encode(resource_name)),
            |id| format!("pdf-object:{}:{}", id.0, id.1),
        );
        return Ok(PdfPageVisualArtifact {
            page,
            asset_index: this_index,
            asset_locator,
            media_type: "image/jpeg",
            bytes: stream.content.clone(),
        });
    }
    if saw_image {
        Err(blocker)
    } else {
        Err(PdfPageVisualArtifactBlocker::NoDirectImage)
    }
}

fn pdf_image_filter_is_direct_jpeg(dictionary: &BTreeMap<Vec<u8>, PdfValue>) -> bool {
    matches!(
        dictionary.get(b"Filter".as_slice()),
        Some(PdfValue::Name(filter)) if matches!(filter.as_slice(), b"DCTDecode" | b"DCT")
    )
}

fn decrypted_pdf_image_filter_is_direct_jpeg(dictionary: &lopdf::Dictionary) -> bool {
    matches!(
        dictionary
            .get(b"Filter")
            .and_then(lopdf::Object::as_name)
            .ok(),
        Some(b"DCTDecode" | b"DCT")
    )
}

fn pdf_visual_asset_locator(object_id: ObjectId, resource_name: &[u8]) -> String {
    format!(
        "pdf-object:{}:{}:resource:{}",
        object_id.number,
        object_id.generation,
        hex::encode(resource_name)
    )
}

fn pdf_visual_artifact_error(blocker: PdfPageVisualArtifactBlocker) -> PdfError {
    match blocker {
        PdfPageVisualArtifactBlocker::ByteLimit => PdfError::DecompressionLimit,
        PdfPageVisualArtifactBlocker::InvalidImage => PdfError::InvalidStream,
        PdfPageVisualArtifactBlocker::VisualInventoryUnavailable => PdfError::UnsupportedFont,
        PdfPageVisualArtifactBlocker::UnsupportedImageEncoding => PdfError::UnsupportedFilter,
        PdfPageVisualArtifactBlocker::NoDirectImage => PdfError::InlineImage,
        PdfPageVisualArtifactBlocker::VisualInventoryPartial => PdfError::FactLimit,
        PdfPageVisualArtifactBlocker::PageUnavailable => PdfError::PageLimit,
        PdfPageVisualArtifactBlocker::SourceRejected => PdfError::Malformed,
    }
}

fn pdf_visual_artifact_blocker(error: PdfError) -> PdfPageVisualArtifactBlocker {
    match error {
        PdfError::PageLimit => PdfPageVisualArtifactBlocker::PageUnavailable,
        PdfError::FactLimit => PdfPageVisualArtifactBlocker::VisualInventoryPartial,
        PdfError::InlineImage => PdfPageVisualArtifactBlocker::NoDirectImage,
        PdfError::UnsupportedFilter => PdfPageVisualArtifactBlocker::UnsupportedImageEncoding,
        PdfError::UnsupportedFont => PdfPageVisualArtifactBlocker::VisualInventoryUnavailable,
        PdfError::DecompressionLimit => PdfPageVisualArtifactBlocker::ByteLimit,
        PdfError::InvalidStream => PdfPageVisualArtifactBlocker::InvalidImage,
        _ => PdfPageVisualArtifactBlocker::SourceRejected,
    }
}

fn is_complete_jpeg(bytes: &[u8]) -> bool {
    if bytes.len() < 4 || !bytes.starts_with(&[0xff, 0xd8]) || !bytes.ends_with(&[0xff, 0xd9]) {
        return false;
    }
    let mut cursor = 2;
    let mut dimensions = false;
    while cursor + 1 < bytes.len() {
        if bytes[cursor] != 0xff {
            return false;
        }
        while bytes.get(cursor) == Some(&0xff) {
            cursor += 1;
        }
        let Some(&marker) = bytes.get(cursor) else {
            return false;
        };
        cursor += 1;
        if marker == 0xda {
            return dimensions;
        }
        if marker == 0xd9 {
            return false;
        }
        if matches!(marker, 0x01 | 0xd0..=0xd7) {
            continue;
        }
        let Some(length) = bytes
            .get(cursor..cursor + 2)
            .map(|value| usize::from(u16::from_be_bytes([value[0], value[1]])))
        else {
            return false;
        };
        if length < 2
            || cursor
                .checked_add(length)
                .is_none_or(|end| end > bytes.len())
        {
            return false;
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            if length < 8 {
                return false;
            }
            let height = u16::from_be_bytes([bytes[cursor + 3], bytes[cursor + 4]]);
            let width = u16::from_be_bytes([bytes[cursor + 5], bytes[cursor + 6]]);
            if width == 0 || height == 0 {
                return false;
            }
            dimensions = true;
        }
        cursor += length;
    }
    false
}

fn extract_pdf_bytes_once(
    path: &Path,
    source_file: &str,
    source: &[u8],
    limits: PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<Extraction, PdfError> {
    if source.len() > limits.max_input_bytes {
        return Err(PdfError::InputLimit);
    }
    check_cancelled(cancelled)?;
    validate_pdf_header(source)?;
    let xref = parse_xref(source, &limits, cancelled)?;
    let parsed = parse_indirect_objects(source, xref, &limits, cancelled)?;
    let page_ids = collect_page_ids(&parsed, &limits, cancelled)?;
    let page_outlines = collect_page_outlines(&parsed, &page_ids, &limits);
    let required_facts = page_ids
        .len()
        .checked_mul(2)
        .and_then(|facts| facts.checked_add(1))
        .ok_or(PdfError::FactLimit)?;
    if required_facts > limits.max_facts {
        return Err(PdfError::FactLimit);
    }

    let mut decode = DecodeBudget::new(limits);
    let cmaps = collect_tounicode_cmaps(source, &parsed, &limits, cancelled, &mut decode)?;
    let mut content_limits = limits;
    content_limits.max_content_operations =
        limits.effective_content_operation_limit(page_ids.len())?;
    let mut content_budget = ContentBudget::default();
    let mut page_text = Vec::new();
    page_text
        .try_reserve_exact(page_ids.len())
        .map_err(|_| PdfError::PageLimit)?;
    for (page_index, page_id) in page_ids.iter().copied().enumerate() {
        check_cancelled(cancelled)?;
        let content_ids = page_content_ids(&parsed, page_id, &limits)?;
        let page_resources = page_resources(&parsed, page_id, &limits)?;
        let fonts = page_resources
            .map(|resources| parse_font_resources(&parsed, resources, &cmaps, &limits))
            .transpose()?
            .unwrap_or_default();
        let empty_resources = BTreeMap::new();
        let resources = page_resources.unwrap_or(&empty_resources);
        let text = extract_page_text(
            PageTextRequest {
                source,
                parsed: &parsed,
                content_ids: &content_ids,
                fonts: &fonts,
                resources,
                cmaps: &cmaps,
                limits: &content_limits,
                cancelled,
            },
            &mut decode,
            &mut content_budget,
        )?;
        page_text.push(PdfPageMaterial {
            number: page_index + 1,
            text,
            outline: page_outlines.get(&page_id).cloned(),
            visual: page_visual_inventory(&parsed, page_id, &limits),
        });
    }

    let metadata = extract_metadata(&parsed, &limits, decode.total_text_bytes)?;
    let final_text_bytes = decode
        .total_text_bytes
        .checked_add(metadata.total_bytes)
        .ok_or(PdfError::TextLimit)?;
    if final_text_bytes > limits.max_total_text_bytes {
        return Err(PdfError::TextLimit);
    }
    check_cancelled(cancelled)?;

    // This is intentionally the last fallible admission check. Once credits
    // are consumed, materialization below contains no parser/cancellation path
    // that can fail and strand the adapter's rejection-root credit.
    if !crate::parser_budget::try_reserve_facts(required_facts) {
        return Err(PdfError::FactLimit);
    }
    Ok(materialize_extraction(
        path,
        source_file,
        page_text,
        metadata,
        decode.decoded_streams,
        decode.total_decoded_bytes,
        final_text_bytes,
    ))
}

fn decrypt_empty_password_pdf(
    source: &[u8],
    limits: PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<(lopdf::Document, DecryptedPdfScan), PdfError> {
    check_cancelled(cancelled)?;
    let document = lopdf::Document::load_mem_with_options(
        source,
        lopdf::LoadOptions {
            password: Some(String::new()),
            max_decompressed_size: Some(limits.max_stream_decoded_bytes),
            ..Default::default()
        },
    )
    .map_err(|_| PdfError::Encrypted)?;
    if !document.was_encrypted() || document.is_encrypted() {
        return Err(PdfError::Encrypted);
    }
    authenticate_empty_user_password(&document)?;
    if document.objects.len() > limits.max_objects {
        return Err(PdfError::ObjectLimit);
    }
    let page_count = document.get_pages().len();
    if page_count == 0 {
        return Err(PdfError::Malformed);
    }
    if page_count > limits.max_pages {
        return Err(PdfError::PageLimit);
    }
    let scan = validate_decrypted_document(&document, &limits, cancelled)?;
    check_cancelled(cancelled)?;
    Ok((document, scan))
}

fn authenticate_empty_user_password(document: &lopdf::Document) -> Result<(), PdfError> {
    let state = document
        .encryption_state
        .as_ref()
        .ok_or(PdfError::Encrypted)?;
    // lopdf accepts either the owner or user password while loading. Rebuild
    // only the security dictionary and trailer ID so the user-password-only
    // authenticator can distinguish those two cases without retaining a
    // second copy of the decrypted object graph.
    let mut authentication_document = lopdf::Document::new();
    if let Ok(id) = document.trailer.get(b"ID") {
        authentication_document.trailer.set("ID", id.clone());
    }
    let encryption_id =
        authentication_document.add_object(state.encode().map_err(|_| PdfError::Encrypted)?);
    authentication_document
        .trailer
        .set("Encrypt", lopdf::Object::Reference(encryption_id));
    authentication_document
        .authenticate_user_password("")
        .map_err(|_| PdfError::Encrypted)
}

#[derive(Clone, Copy, Default)]
struct DecryptedPdfScan {
    open_actions: bool,
    embedded_files: bool,
    stream_count: usize,
}

fn validate_decrypted_document(
    document: &lopdf::Document,
    limits: &PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<DecryptedPdfScan, PdfError> {
    let mut entries = 0usize;
    let mut streams = 0usize;
    let mut scan = DecryptedPdfScan::default();
    let catalog_id = document
        .trailer
        .get(b"Root")
        .and_then(lopdf::Object::as_reference)
        .map_err(|_| PdfError::Malformed)?;
    let page_ids = document.get_pages().into_values().collect::<BTreeSet<_>>();
    validate_lopdf_object(
        &lopdf::Object::Dictionary(document.trailer.clone()),
        0,
        &mut entries,
        &mut streams,
        &mut scan,
        limits,
    )?;
    for (id, object) in &document.objects {
        check_cancelled(cancelled)?;
        if *id == catalog_id {
            let catalog = object.as_dict().map_err(|_| PdfError::Malformed)?;
            if catalog.get(b"Type").and_then(lopdf::Object::as_name).ok() != Some(b"Catalog") {
                return Err(PdfError::ActiveContent);
            }
            validate_lopdf_catalog(
                catalog,
                document,
                &page_ids,
                &mut entries,
                &mut streams,
                &mut scan,
                limits,
            )?;
        } else {
            validate_lopdf_object(object, 0, &mut entries, &mut streams, &mut scan, limits)?;
        }
    }
    scan.stream_count = streams;
    Ok(scan)
}

fn validate_lopdf_catalog(
    catalog: &lopdf::Dictionary,
    document: &lopdf::Document,
    page_ids: &BTreeSet<lopdf::ObjectId>,
    entries: &mut usize,
    streams: &mut usize,
    scan: &mut DecryptedPdfScan,
    limits: &PdfLimits,
) -> Result<(), PdfError> {
    *entries = entries
        .checked_add(catalog.len())
        .ok_or(PdfError::TokenLimit)?;
    if *entries > limits.max_container_entries {
        return Err(PdfError::TokenLimit);
    }
    for (name, value) in catalog.iter() {
        if name == b"OpenAction" {
            validate_catalog_open_action(value, document, page_ids)?;
            scan.open_actions = true;
        } else {
            validate_decrypted_name(name, scan)?;
            validate_lopdf_object(value, 1, entries, streams, scan, limits)?;
        }
    }
    Ok(())
}

fn validate_catalog_open_action(
    value: &lopdf::Object,
    document: &lopdf::Document,
    page_ids: &BTreeSet<lopdf::ObjectId>,
) -> Result<(), PdfError> {
    let value = match value {
        lopdf::Object::Reference(id) => document
            .get_object(*id)
            .map_err(|_| PdfError::ActiveContent)?,
        value => value,
    };
    let destination = match value {
        lopdf::Object::Array(destination) => destination,
        lopdf::Object::Dictionary(action) => {
            if action
                .iter()
                .any(|(key, _)| !matches!(key.as_slice(), b"Type" | b"S" | b"D"))
            {
                return Err(PdfError::ActiveContent);
            }
            if let Ok(action_type) = action.get(b"Type")
                && action_type.as_name().ok() != Some(b"Action")
            {
                return Err(PdfError::ActiveContent);
            }
            if action.get(b"S").and_then(lopdf::Object::as_name).ok() != Some(b"GoTo") {
                return Err(PdfError::ActiveContent);
            }
            action
                .get(b"D")
                .and_then(lopdf::Object::as_array)
                .map_err(|_| PdfError::ActiveContent)?
        }
        _ => return Err(PdfError::ActiveContent),
    };
    validate_explicit_destination(destination, page_ids)
}

fn validate_explicit_destination(
    destination: &[lopdf::Object],
    page_ids: &BTreeSet<lopdf::ObjectId>,
) -> Result<(), PdfError> {
    destination
        .first()
        .and_then(|target| target.as_reference().ok())
        .filter(|id| page_ids.contains(id))
        .ok_or(PdfError::ActiveContent)?;
    let mode = destination
        .get(1)
        .and_then(|mode| mode.as_name().ok())
        .ok_or(PdfError::ActiveContent)?;
    let valid = match mode {
        b"XYZ" => {
            destination.len() == 5
                && destination[2..4].iter().all(is_number_or_null)
                && is_nonnegative_number_or_null(&destination[4])
        }
        b"Fit" | b"FitB" => destination.len() == 2,
        b"FitH" | b"FitV" | b"FitBH" | b"FitBV" => {
            destination.len() == 3 && is_number_or_null(&destination[2])
        }
        b"FitR" => destination.len() == 6 && destination[2..].iter().all(is_finite_number),
        _ => false,
    };
    valid.then_some(()).ok_or(PdfError::ActiveContent)
}

fn is_number_or_null(value: &lopdf::Object) -> bool {
    matches!(value, lopdf::Object::Null) || is_finite_number(value)
}

fn is_nonnegative_number_or_null(value: &lopdf::Object) -> bool {
    match value {
        lopdf::Object::Null | lopdf::Object::Integer(0..) => true,
        lopdf::Object::Real(number) => number.is_finite() && *number >= 0.0,
        _ => false,
    }
}

fn is_finite_number(value: &lopdf::Object) -> bool {
    match value {
        lopdf::Object::Integer(_) => true,
        lopdf::Object::Real(number) => number.is_finite(),
        _ => false,
    }
}

fn validate_lopdf_object(
    object: &lopdf::Object,
    depth: usize,
    entries: &mut usize,
    streams: &mut usize,
    scan: &mut DecryptedPdfScan,
    limits: &PdfLimits,
) -> Result<(), PdfError> {
    if depth > limits.max_object_nesting {
        return Err(PdfError::NestingLimit);
    }
    match object {
        lopdf::Object::Name(name) => validate_decrypted_name(name, scan),
        lopdf::Object::Array(values) => {
            *entries = entries
                .checked_add(values.len())
                .ok_or(PdfError::TokenLimit)?;
            if *entries > limits.max_container_entries {
                return Err(PdfError::TokenLimit);
            }
            for value in values {
                validate_lopdf_object(value, depth + 1, entries, streams, scan, limits)?;
            }
            Ok(())
        }
        lopdf::Object::Dictionary(dictionary) => {
            let typed_filespec = dictionary
                .get(b"Type")
                .and_then(lopdf::Object::as_name)
                .ok()
                == Some(b"Filespec");
            let external_filespec = dictionary.has(b"FS")
                && (typed_filespec || dictionary.has(b"F") || dictionary.has(b"UF"));
            if (typed_filespec
                && (!dictionary.has(b"EF") || dictionary.has(b"FS") || dictionary.has(b"RF")))
                || external_filespec
            {
                return Err(PdfError::ActiveContent);
            }
            *entries = entries
                .checked_add(dictionary.len())
                .ok_or(PdfError::TokenLimit)?;
            if *entries > limits.max_container_entries {
                return Err(PdfError::TokenLimit);
            }
            for (name, value) in dictionary.iter() {
                validate_decrypted_name(name, scan)?;
                validate_lopdf_object(value, depth + 1, entries, streams, scan, limits)?;
            }
            Ok(())
        }
        lopdf::Object::Stream(stream) => {
            *streams = streams.checked_add(1).ok_or(PdfError::ObjectLimit)?;
            if *streams > limits.max_streams {
                return Err(PdfError::ObjectLimit);
            }
            if stream.dict.has(b"F")
                || stream.dict.has(b"FFilter")
                || stream.dict.has(b"FDecodeParms")
            {
                return Err(PdfError::ActiveContent);
            }
            validate_lopdf_object(
                &lopdf::Object::Dictionary(stream.dict.clone()),
                depth + 1,
                entries,
                streams,
                scan,
                limits,
            )
        }
        _ => Ok(()),
    }
}

fn validate_decrypted_name(name: &[u8], scan: &mut DecryptedPdfScan) -> Result<(), PdfError> {
    match name {
        // Empty-password authentication above already decrypted this document;
        // lopdf may retain the inert trailer key/object for inspection.
        b"Encrypt" => {}
        b"EmbeddedFile" | b"EmbeddedFiles" | b"Filespec" => scan.embedded_files = true,
        _ => reject_unsafe_name(name)?,
    }
    Ok(())
}

fn extract_decrypted_pdf(
    path: &Path,
    source_file: &str,
    document: &lopdf::Document,
    scan: DecryptedPdfScan,
    limits: PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<Extraction, PdfError> {
    let pages = document.get_pages();
    let required_facts = pages
        .len()
        .checked_mul(2)
        .and_then(|facts| facts.checked_add(1))
        .ok_or(PdfError::FactLimit)?;
    if required_facts > limits.max_facts {
        return Err(PdfError::FactLimit);
    }
    let mut total_text_bytes = 0usize;
    let mut total_decoded_bytes = 0usize;
    let mut total_operations = 0usize;
    let max_content_operations = limits.effective_content_operation_limit(pages.len())?;
    let mut page_text = Vec::new();
    page_text
        .try_reserve_exact(pages.len())
        .map_err(|_| PdfError::PageLimit)?;
    for (page_number, page_id) in pages.iter().map(|(number, id)| (*number, *id)) {
        check_cancelled(cancelled)?;
        let remaining_decoded = limits
            .max_total_decoded_bytes
            .checked_sub(total_decoded_bytes)
            .ok_or(PdfError::DecompressionLimit)?;
        let page_content = document
            .get_page_content_with_limit(
                page_id,
                remaining_decoded.min(limits.max_stream_decoded_bytes),
            )
            .map_err(|_| PdfError::DecompressionLimit)?;
        reserve_decoded_bytes(&mut total_decoded_bytes, page_content.len(), &limits)?;

        // lopdf decodes each page font's ToUnicode CMap independently of its
        // bounded page-content read. Preflight the same immutable streams for
        // every font resource on every page, including repeated references, so
        // the subsequent extraction cannot reset a per-page limit around a
        // document-wide aggregate overrun.
        for font in document
            .get_page_fonts(page_id)
            .map_err(|_| PdfError::UnsupportedFont)?
            .values()
        {
            let Ok(stream) = font
                .get_deref(b"ToUnicode", document)
                .and_then(lopdf::Object::as_stream)
            else {
                continue;
            };
            let remaining_decoded = limits
                .max_total_decoded_bytes
                .checked_sub(total_decoded_bytes)
                .ok_or(PdfError::DecompressionLimit)?;
            let cmap = stream
                .get_plain_content_with_limit(
                    remaining_decoded.min(limits.max_stream_decoded_bytes),
                )
                .map_err(|_| PdfError::DecompressionLimit)?;
            reserve_decoded_bytes(&mut total_decoded_bytes, cmap.len(), &limits)?;
        }

        let content =
            lopdf::content::Content::decode(&page_content).map_err(|_| PdfError::ContentLimit)?;
        total_operations = total_operations
            .checked_add(content.operations.len())
            .ok_or(PdfError::ContentLimit)?;
        if total_operations > max_content_operations {
            return Err(PdfError::ContentLimit);
        }
        let text = document
            .extract_text_with_limit(&[page_number], limits.max_stream_decoded_bytes)
            .map_err(|_| PdfError::DecompressionLimit)?;
        let text = sanitize_decrypted_page_text(text);
        if text.len() > limits.max_text_bytes_per_page {
            return Err(PdfError::TextLimit);
        }
        total_text_bytes = total_text_bytes
            .checked_add(text.len())
            .ok_or(PdfError::TextLimit)?;
        if total_text_bytes > limits.max_total_text_bytes {
            return Err(PdfError::TextLimit);
        }
        page_text.push(PdfPageMaterial {
            number: page_number as usize,
            text,
            outline: None,
            visual: decrypted_page_visual_inventory(document, page_id, &limits),
        });
    }
    check_cancelled(cancelled)?;
    if !crate::parser_budget::try_reserve_facts(required_facts) {
        return Err(PdfError::FactLimit);
    }
    let mut extraction = materialize_extraction(
        path,
        source_file,
        page_text,
        PdfMetadata::default(),
        scan.stream_count,
        total_decoded_bytes,
        total_text_bytes,
    );
    if scan.open_actions || scan.embedded_files {
        let root = &mut extraction.nodes[0];
        root.extra.insert("parse_status".into(), "partial".into());
        root.extra.insert(
            "ignored_pdf_features".into(),
            match (scan.open_actions, scan.embedded_files) {
                (true, true) => "open_actions,embedded_files",
                (true, false) => "open_actions",
                (false, true) => "embedded_files",
                (false, false) => unreachable!(),
            }
            .into(),
        );
    }
    Ok(extraction)
}

fn reserve_decoded_bytes(
    total: &mut usize,
    decoded: usize,
    limits: &PdfLimits,
) -> Result<(), PdfError> {
    *total = total
        .checked_add(decoded)
        .ok_or(PdfError::DecompressionLimit)?;
    if *total > limits.max_total_decoded_bytes {
        return Err(PdfError::DecompressionLimit);
    }
    Ok(())
}

fn extract_embedded_attachments(
    document: &lopdf::Document,
    extraction: &mut Extraction,
    limits: PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<Vec<PdfAttachment>, PdfError> {
    let mut entries = Vec::new();
    let mut visited = BTreeSet::new();
    let tree_result = embedded_file_name_tree(document).and_then(|tree| {
        collect_embedded_file_entries(
            document,
            tree,
            0,
            &mut visited,
            &mut entries,
            limits,
            cancelled,
        )
    });
    let mut total_decoded = extraction
        .nodes
        .first()
        .and_then(|root| root.extra.get("decompressed_bytes"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0);
    // The parser plan reserves decoded bytes and retained text as disjoint
    // classes. Once page extraction has materialized its actual text, an
    // attachment may safely borrow only the unused text reservation; their
    // combined retained bytes still cannot exceed the proven allowance.
    let retained_text = extraction
        .nodes
        .first()
        .and_then(|root| root.extra.get("text_bytes"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(limits.max_total_text_bytes);
    let attachment_decode_limit = limits
        .max_total_decoded_bytes
        .saturating_add(limits.max_total_text_bytes.saturating_sub(retained_text));
    if tree_result == Err(PdfAttachmentBlocker::Cancelled) {
        return Err(PdfError::Cancelled);
    }
    let mut decode_budget = AttachmentDecodeBudget {
        total_decoded: &mut total_decoded,
        total_limit: attachment_decode_limit,
    };
    let mut attachments = Vec::with_capacity(entries.len().saturating_add(1));
    for (index, (name, filespec)) in entries.into_iter().enumerate() {
        attachments.push(extract_embedded_file(
            document,
            index,
            &name,
            &filespec,
            &mut decode_budget,
            limits,
            cancelled,
        )?);
    }
    if let Err(blocker) = tree_result {
        attachments.push(blocked_attachment(attachments.len(), blocker));
    } else if attachments.is_empty() {
        attachments.push(blocked_attachment(0, PdfAttachmentBlocker::Unreadable));
    }
    make_attachment_paths_unique(&mut attachments);
    attachments.sort_by(|left, right| left.path.cmp(&right.path));
    if let Some(root) = extraction.nodes.first_mut() {
        root.extra
            .insert("decompressed_bytes".into(), total_decoded.into());
    }
    Ok(attachments)
}

fn make_attachment_paths_unique(attachments: &mut [PdfAttachment]) {
    let mut used = BTreeSet::new();
    let attachment_count = attachments.len();
    for (index, attachment) in attachments.iter_mut().enumerate() {
        if used.insert(attachment.path.clone()) {
            continue;
        }
        attachment.bytes = None;
        attachment.blocker = Some(PdfAttachmentBlocker::Unreadable);
        let mut candidate_index = index;
        loop {
            let candidate = format!("attachment-{:06}", candidate_index + 1);
            if used.insert(candidate.clone()) {
                attachment.path = candidate;
                break;
            }
            candidate_index = candidate_index
                .checked_add(attachment_count)
                .expect("bounded attachment path attempts must not overflow");
        }
    }
}

fn embedded_file_name_tree(
    document: &lopdf::Document,
) -> Result<&lopdf::Object, PdfAttachmentBlocker> {
    let catalog_id = document
        .trailer
        .get(b"Root")
        .and_then(lopdf::Object::as_reference)
        .map_err(|_| PdfAttachmentBlocker::Unreadable)?;
    let catalog = document
        .get_dictionary(catalog_id)
        .map_err(|_| PdfAttachmentBlocker::Unreadable)?;
    let names = catalog
        .get(b"Names")
        .map_err(|_| PdfAttachmentBlocker::Unreadable)?;
    let names = resolve_lopdf_object(document, names)?
        .as_dict()
        .map_err(|_| PdfAttachmentBlocker::Unreadable)?;
    names
        .get(b"EmbeddedFiles")
        .map_err(|_| PdfAttachmentBlocker::Unreadable)
}

fn resolve_lopdf_object<'a>(
    document: &'a lopdf::Document,
    object: &'a lopdf::Object,
) -> Result<&'a lopdf::Object, PdfAttachmentBlocker> {
    match object {
        lopdf::Object::Reference(id) => document
            .get_object(*id)
            .map_err(|_| PdfAttachmentBlocker::Unreadable),
        object => Ok(object),
    }
}

fn collect_embedded_file_entries(
    document: &lopdf::Document,
    tree: &lopdf::Object,
    depth: usize,
    visited: &mut BTreeSet<lopdf::ObjectId>,
    entries: &mut Vec<(Vec<u8>, lopdf::Object)>,
    limits: PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<(), PdfAttachmentBlocker> {
    if cancelled.is_some_and(|check| check()) {
        return Err(PdfAttachmentBlocker::Cancelled);
    }
    if depth > limits.max_attachment_tree_depth {
        return Err(PdfAttachmentBlocker::DepthLimit);
    }
    let tree = match tree {
        lopdf::Object::Reference(id) => {
            if !visited.insert(*id) {
                return Err(PdfAttachmentBlocker::Unreadable);
            }
            document
                .get_object(*id)
                .map_err(|_| PdfAttachmentBlocker::Unreadable)?
        }
        tree => tree,
    };
    let dictionary = tree
        .as_dict()
        .map_err(|_| PdfAttachmentBlocker::Unreadable)?;
    let mut found = false;
    if let Ok(names) = dictionary.get(b"Names") {
        found = true;
        let names = resolve_lopdf_object(document, names)?
            .as_array()
            .map_err(|_| PdfAttachmentBlocker::Unreadable)?;
        if names.len() % 2 != 0 {
            return Err(PdfAttachmentBlocker::Unreadable);
        }
        for pair in names.chunks_exact(2) {
            if entries.len() >= limits.max_attachments {
                return Err(PdfAttachmentBlocker::CountLimit);
            }
            let lopdf::Object::String(name, _) = &pair[0] else {
                return Err(PdfAttachmentBlocker::Unreadable);
            };
            entries.push((name.clone(), pair[1].clone()));
        }
    }
    if let Ok(kids) = dictionary.get(b"Kids") {
        found = true;
        let kids = resolve_lopdf_object(document, kids)?
            .as_array()
            .map_err(|_| PdfAttachmentBlocker::Unreadable)?;
        for kid in kids {
            collect_embedded_file_entries(
                document,
                kid,
                depth + 1,
                visited,
                entries,
                limits,
                cancelled,
            )?;
        }
    }
    found.then_some(()).ok_or(PdfAttachmentBlocker::Unreadable)
}

struct AttachmentDecodeBudget<'a> {
    total_decoded: &'a mut usize,
    total_limit: usize,
}

fn extract_embedded_file(
    document: &lopdf::Document,
    index: usize,
    name: &[u8],
    filespec: &lopdf::Object,
    decode_budget: &mut AttachmentDecodeBudget<'_>,
    limits: PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<PdfAttachment, PdfError> {
    if cancelled.is_some_and(|check| check()) {
        return Err(PdfError::Cancelled);
    }
    let path = decode_info_text_string(name, limits.max_attachment_name_bytes)
        .ok()
        .map(sanitize_metadata_string)
        .and_then(|name| {
            crate::containers::normalized_member_path(&name, limits.max_attachment_name_bytes)
        });
    let Some(path) = path else {
        return Ok(blocked_attachment(index, PdfAttachmentBlocker::Unreadable));
    };
    let mut encoded_bytes = 0_u64;
    let content = (|| {
        let filespec = resolve_lopdf_object(document, filespec)?
            .as_dict()
            .map_err(|_| PdfAttachmentBlocker::Unreadable)?;
        if filespec.get(b"Type").and_then(lopdf::Object::as_name).ok() != Some(b"Filespec") {
            return Err(PdfAttachmentBlocker::Unreadable);
        }
        let embedded = resolve_lopdf_object(
            document,
            filespec
                .get(b"EF")
                .map_err(|_| PdfAttachmentBlocker::Unreadable)?,
        )?
        .as_dict()
        .map_err(|_| PdfAttachmentBlocker::Unreadable)?;
        let stream = embedded
            .get(b"UF")
            .or_else(|_| embedded.get(b"F"))
            .map_err(|_| PdfAttachmentBlocker::Unreadable)?;
        let stream = resolve_lopdf_object(document, stream)?
            .as_stream()
            .map_err(|_| PdfAttachmentBlocker::Unreadable)?;
        encoded_bytes = u64::try_from(stream.content.len()).unwrap_or(u64::MAX);
        if stream.content.len() > limits.max_stream_input_bytes {
            return Err(PdfAttachmentBlocker::ByteLimit);
        }
        let declared = declared_embedded_file_size(document, stream)?;
        if declared.is_some_and(|size| size > limits.max_stream_decoded_bytes) {
            return Err(PdfAttachmentBlocker::ByteLimit);
        }
        let remaining = decode_budget
            .total_limit
            .checked_sub(*decode_budget.total_decoded)
            .ok_or(PdfAttachmentBlocker::ByteLimit)?;
        let ceiling = remaining.min(limits.max_stream_decoded_bytes);
        let bytes = stream
            .get_plain_content_with_limit(ceiling)
            .map_err(|error| {
                if matches!(
                    error,
                    lopdf::Error::Decompress(lopdf::DecompressError::MemoryLimitExceeded { .. })
                ) {
                    PdfAttachmentBlocker::ByteLimit
                } else {
                    PdfAttachmentBlocker::Unreadable
                }
            })?;
        if bytes.len()
            > stream
                .content
                .len()
                .max(1)
                .saturating_mul(limits.max_expansion_ratio)
        {
            return Err(PdfAttachmentBlocker::ByteLimit);
        }
        *decode_budget.total_decoded = decode_budget
            .total_decoded
            .checked_add(bytes.len())
            .ok_or(PdfAttachmentBlocker::ByteLimit)?;
        Ok(bytes)
    })();
    if cancelled.is_some_and(|check| check()) {
        return Err(PdfError::Cancelled);
    }
    Ok(match content {
        Ok(bytes) => PdfAttachment {
            path,
            bytes: Some(bytes),
            encoded_bytes,
            blocker: None,
        },
        Err(blocker) => PdfAttachment {
            path,
            bytes: None,
            encoded_bytes,
            blocker: Some(blocker),
        },
    })
}

fn declared_embedded_file_size(
    document: &lopdf::Document,
    stream: &lopdf::Stream,
) -> Result<Option<usize>, PdfAttachmentBlocker> {
    let Ok(params) = stream.dict.get(b"Params") else {
        return Ok(None);
    };
    let params = resolve_lopdf_object(document, params)?
        .as_dict()
        .map_err(|_| PdfAttachmentBlocker::Unreadable)?;
    let Ok(size) = params.get(b"Size") else {
        return Ok(None);
    };
    let size = size
        .as_i64()
        .map_err(|_| PdfAttachmentBlocker::Unreadable)?;
    usize::try_from(size)
        .map(Some)
        .map_err(|_| PdfAttachmentBlocker::Unreadable)
}

fn blocked_attachment(index: usize, blocker: PdfAttachmentBlocker) -> PdfAttachment {
    PdfAttachment {
        path: format!("attachment-{:06}", index + 1),
        bytes: None,
        encoded_bytes: 0,
        blocker: Some(blocker),
    }
}

/// Deterministically join bounded page text for compatibility callers.
pub(crate) fn extraction_text(extraction: &Extraction) -> String {
    let mut pages = extraction
        .nodes
        .iter()
        .filter_map(|node| {
            let page = node.extra.get("page_number")?.as_u64()?;
            let text = node.extra.get("text")?.as_str()?;
            Some((page, node.id.as_str(), text))
        })
        .collect::<Vec<_>>();
    pages.sort_unstable_by(|left, right| (left.0, left.1).cmp(&(right.0, right.1)));
    pages
        .into_iter()
        .map(|(_, _, text)| text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn check_cancelled(cancelled: Option<&dyn Fn() -> bool>) -> Result<(), PdfError> {
    if cancelled.is_some_and(|check| check()) {
        Err(PdfError::Cancelled)
    } else {
        Ok(())
    }
}

#[derive(Debug, Default)]
struct ParseCounters {
    tokens: usize,
    container_entries: usize,
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Integer(i64),
    Real,
    Name(Vec<u8>),
    String(Vec<u8>),
    ArrayStart,
    ArrayEnd,
    DictionaryStart,
    DictionaryEnd,
    Keyword(Vec<u8>),
}

struct ValueParser<'a, 'b> {
    source: &'a [u8],
    position: usize,
    end: usize,
    limits: &'b PdfLimits,
    global: &'b mut ParseCounters,
    cancelled: Option<&'b dyn Fn() -> bool>,
    local_tokens: usize,
    local_entries: usize,
    allow_encrypt_name: bool,
}

impl<'a, 'b> ValueParser<'a, 'b> {
    fn new(
        source: &'a [u8],
        position: usize,
        end: usize,
        limits: &'b PdfLimits,
        global: &'b mut ParseCounters,
        cancelled: Option<&'b dyn Fn() -> bool>,
    ) -> Self {
        Self {
            source,
            position,
            end,
            limits,
            global,
            cancelled,
            local_tokens: 0,
            local_entries: 0,
            allow_encrypt_name: false,
        }
    }

    fn new_trailer(
        source: &'a [u8],
        position: usize,
        end: usize,
        limits: &'b PdfLimits,
        global: &'b mut ParseCounters,
        cancelled: Option<&'b dyn Fn() -> bool>,
    ) -> Self {
        Self {
            allow_encrypt_name: true,
            ..Self::new(source, position, end, limits, global, cancelled)
        }
    }

    fn next(&mut self) -> Result<Token, PdfError> {
        if self.global.tokens.is_multiple_of(1_024) {
            check_cancelled(self.cancelled)?;
        }
        let (token, end) = lex_token_at(self.source, self.position, self.end, self.limits)?;
        if let Token::Name(name) = &token
            && !(self.allow_encrypt_name && name == b"Encrypt")
        {
            reject_unsafe_name(name)?;
        }
        self.position = end;
        self.local_tokens = self
            .local_tokens
            .checked_add(1)
            .ok_or(PdfError::TokenLimit)?;
        self.global.tokens = self
            .global
            .tokens
            .checked_add(1)
            .ok_or(PdfError::TokenLimit)?;
        if self.local_tokens > self.limits.max_tokens_per_object
            || self.global.tokens > self.limits.max_tokens
        {
            return Err(PdfError::TokenLimit);
        }
        Ok(token)
    }

    fn peek(&self) -> Result<Token, PdfError> {
        lex_token_at(self.source, self.position, self.end, self.limits).map(|value| value.0)
    }

    fn add_entry(&mut self) -> Result<(), PdfError> {
        self.local_entries = self
            .local_entries
            .checked_add(1)
            .ok_or(PdfError::TokenLimit)?;
        self.global.container_entries = self
            .global
            .container_entries
            .checked_add(1)
            .ok_or(PdfError::TokenLimit)?;
        if self.local_entries > self.limits.max_container_entries_per_object
            || self.global.container_entries > self.limits.max_container_entries
        {
            return Err(PdfError::TokenLimit);
        }
        Ok(())
    }

    fn parse_value(&mut self, depth: usize) -> Result<PdfValue, PdfError> {
        if depth > self.limits.max_object_nesting {
            return Err(PdfError::NestingLimit);
        }
        match self.next()? {
            Token::Integer(number) => {
                let after_number = self.position;
                if let Ok((Token::Integer(generation), after_generation)) =
                    lex_token_at(self.source, after_number, self.end, self.limits)
                    && let Ok((Token::Keyword(reference), _)) =
                        lex_token_at(self.source, after_generation, self.end, self.limits)
                    && reference == b"R"
                {
                    let _ = self.next()?;
                    let _ = self.next()?;
                    let number = u32::try_from(number).map_err(|_| PdfError::Malformed)?;
                    let generation = u16::try_from(generation).map_err(|_| PdfError::Malformed)?;
                    return Ok(PdfValue::Reference(ObjectId { number, generation }));
                }
                Ok(PdfValue::Integer(number))
            }
            Token::Real => Ok(PdfValue::Real),
            Token::Name(name) => {
                reject_unsafe_name(&name)?;
                Ok(PdfValue::Name(name))
            }
            Token::String(value) => Ok(PdfValue::String(value)),
            Token::Keyword(keyword) if keyword == b"null" => Ok(PdfValue::Null),
            Token::Keyword(keyword) if matches!(keyword.as_slice(), b"true" | b"false") => {
                Ok(PdfValue::Boolean)
            }
            Token::ArrayStart => {
                let mut values = Vec::new();
                loop {
                    if self.peek()? == Token::ArrayEnd {
                        let _ = self.next()?;
                        break;
                    }
                    self.add_entry()?;
                    values.try_reserve(1).map_err(|_| PdfError::TokenLimit)?;
                    values.push(self.parse_value(depth + 1)?);
                }
                Ok(PdfValue::Array(values))
            }
            Token::DictionaryStart => {
                let mut values = BTreeMap::new();
                loop {
                    if self.peek()? == Token::DictionaryEnd {
                        let _ = self.next()?;
                        break;
                    }
                    let Token::Name(key) = self.next()? else {
                        return Err(PdfError::Malformed);
                    };
                    if !(self.allow_encrypt_name && depth == 0 && key == b"Encrypt") {
                        reject_unsafe_name(&key)?;
                    }
                    self.add_entry()?;
                    let value = self.parse_value(depth + 1)?;
                    if values.contains_key(&key) {
                        return Err(if self.allow_encrypt_name && key == b"Encrypt" {
                            PdfError::Encrypted
                        } else {
                            PdfError::Malformed
                        });
                    }
                    values.insert(key, value);
                }
                Ok(PdfValue::Dictionary(values))
            }
            _ => Err(PdfError::Malformed),
        }
    }
}

fn lex_token_at(
    source: &[u8],
    position: usize,
    end: usize,
    limits: &PdfLimits,
) -> Result<(Token, usize), PdfError> {
    lex_token_at_with_byte_limit(source, position, end, limits, limits.max_input_bytes)
}

fn lex_token_at_with_byte_limit(
    source: &[u8],
    position: usize,
    end: usize,
    limits: &PdfLimits,
    max_token_bytes: usize,
) -> Result<(Token, usize), PdfError> {
    let position = skip_space_and_comments(source, position, end);
    if position >= end {
        return Err(PdfError::Malformed);
    }
    let byte = source[position];
    match byte {
        b'[' => Ok((Token::ArrayStart, position + 1)),
        b']' => Ok((Token::ArrayEnd, position + 1)),
        b'<' if position + 1 < end && source[position + 1] == b'<' => {
            Ok((Token::DictionaryStart, position + 2))
        }
        b'>' if position + 1 < end && source[position + 1] == b'>' => {
            Ok((Token::DictionaryEnd, position + 2))
        }
        b'/' => lex_name(source, position, end, max_token_bytes),
        b'(' => lex_literal_string(source, position, end, limits, max_token_bytes),
        b'<' => lex_hex_string(source, position, end, max_token_bytes),
        b')' | b'>' | b'{' | b'}' => Err(PdfError::Malformed),
        _ => lex_bare_token(source, position, end, max_token_bytes),
    }
}

fn lex_name(
    source: &[u8],
    start: usize,
    end: usize,
    max_token_bytes: usize,
) -> Result<(Token, usize), PdfError> {
    let mut position = start + 1;
    let mut name = Vec::new();
    while position < end && !is_pdf_delimiter(source[position]) {
        let value = if source[position] == b'#' {
            if position.checked_add(2).is_none_or(|last| last >= end) {
                return Err(PdfError::Malformed);
            }
            let high = source[position + 1];
            let low = source[position + 2];
            position += 3;
            (hex_digit(high).ok_or(PdfError::Malformed)? << 4)
                | hex_digit(low).ok_or(PdfError::Malformed)?
        } else {
            let value = source[position];
            position += 1;
            value
        };
        name.try_reserve(1).map_err(|_| PdfError::TokenLimit)?;
        name.push(value);
        if name.len() > max_token_bytes {
            return Err(PdfError::ContentLimit);
        }
    }
    Ok((Token::Name(name), position))
}

fn reject_unsafe_name(name: &[u8]) -> Result<(), PdfError> {
    // Object streams and cross-reference streams are now supported; they are no
    // longer rejected at the name-lexing stage. Their structure is validated by
    // the dedicated parsers, which fail closed on malformed input.
    //
    // Only names that denote executable or externally reachable content are
    // rejected here. `Prev` is *not* on this list: pages-tree nodes carry
    // standard `/Prev` sibling references, and incremental-update ancestry is
    // established by exact trailer `/Prev` links in the xref parser. Marker-like
    // bytes inside streams are inert. `URI` is *not* on this list either:
    // the extractor never follows URIs and never publishes annotation action
    // strings — only content-stream page text and the eight Info-dictionary
    // metadata fields reach the graph, so plain link annotations cannot leak
    // payloads.
    match name {
        b"Encrypt" => Err(PdfError::Encrypted),
        b"JavaScript" | b"JS" | b"Launch" | b"GoToR" | b"GoToE" | b"SubmitForm" | b"ImportData"
        | b"RichMedia" | b"XFA" | b"AA" | b"OpenAction" | b"EmbeddedFile" | b"EmbeddedFiles"
        | b"Filespec" => Err(PdfError::ActiveContent),
        _ => Ok(()),
    }
}

fn lex_literal_string(
    source: &[u8],
    start: usize,
    end: usize,
    limits: &PdfLimits,
    max_token_bytes: usize,
) -> Result<(Token, usize), PdfError> {
    let mut position = start + 1;
    let mut depth = 1_usize;
    let mut value = Vec::new();
    while position < end {
        let byte = source[position];
        position += 1;
        match byte {
            b'(' => {
                depth = depth.checked_add(1).ok_or(PdfError::NestingLimit)?;
                if depth > limits.max_object_nesting {
                    return Err(PdfError::NestingLimit);
                }
                value.push(byte);
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok((Token::String(value), position));
                }
                value.push(byte);
            }
            b'\\' => {
                if position >= end {
                    return Err(PdfError::Malformed);
                }
                let escaped = source[position];
                position += 1;
                match escaped {
                    b'n' => value.push(b'\n'),
                    b'r' => value.push(b'\r'),
                    b't' => value.push(b'\t'),
                    b'b' => value.push(8),
                    b'f' => value.push(12),
                    b'(' | b')' | b'\\' => value.push(escaped),
                    b'\r' => {
                        if position < end && source[position] == b'\n' {
                            position += 1;
                        }
                    }
                    b'\n' => {}
                    b'0'..=b'7' => {
                        let mut octal = u16::from(escaped - b'0');
                        for _ in 0..2 {
                            if position >= end {
                                break;
                            }
                            let next = source[position];
                            if !matches!(next, b'0'..=b'7') {
                                break;
                            }
                            octal = octal * 8 + u16::from(next - b'0');
                            position += 1;
                        }
                        value.push((octal & 0xff) as u8);
                    }
                    _ => value.push(escaped),
                }
            }
            _ => value.push(byte),
        }
        if value.len() > max_token_bytes {
            return Err(PdfError::ContentLimit);
        }
    }
    Err(PdfError::Malformed)
}

fn lex_hex_string(
    source: &[u8],
    start: usize,
    end: usize,
    max_token_bytes: usize,
) -> Result<(Token, usize), PdfError> {
    let mut position = start + 1;
    let mut value = Vec::new();
    let mut pending = None;
    while position < end {
        let byte = source[position];
        position += 1;
        if byte == b'>' {
            if let Some(high) = pending {
                value.push(high << 4);
            }
            return Ok((Token::String(value), position));
        }
        if byte.is_ascii_whitespace() {
            continue;
        }
        let digit = hex_digit(byte).ok_or(PdfError::Malformed)?;
        if let Some(high) = pending.take() {
            value.push((high << 4) | digit);
        } else {
            pending = Some(digit);
        }
        if value.len() > max_token_bytes {
            return Err(PdfError::ContentLimit);
        }
    }
    Err(PdfError::Malformed)
}

fn lex_bare_token(
    source: &[u8],
    start: usize,
    end: usize,
    max_token_bytes: usize,
) -> Result<(Token, usize), PdfError> {
    let mut position = start;
    while position < end && !is_pdf_delimiter(source[position]) {
        position += 1;
    }
    if position == start {
        return Err(PdfError::Malformed);
    }
    let bytes = &source[start..position];
    if bytes.len() > max_token_bytes {
        return Err(PdfError::ContentLimit);
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        if let Ok(integer) = text.parse::<i64>() {
            return Ok((Token::Integer(integer), position));
        }
        if (text.contains('.') || text.starts_with('+') || text.starts_with('-'))
            && text.parse::<f64>().is_ok_and(f64::is_finite)
        {
            return Ok((Token::Real, position));
        }
    }
    Ok((Token::Keyword(bytes.to_vec()), position))
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_pdf_delimiter(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || matches!(
            byte,
            0 | b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
        )
}

fn skip_space_and_comments(source: &[u8], mut position: usize, end: usize) -> usize {
    loop {
        while position < end && (source[position].is_ascii_whitespace() || source[position] == 0) {
            position += 1;
        }
        if position >= end || source[position] != b'%' {
            return position;
        }
        while position < end && !matches!(source[position], b'\r' | b'\n') {
            position += 1;
        }
    }
}

fn skip_pdf_whitespace(source: &[u8], mut position: usize, end: usize) -> usize {
    while position < end && (source[position].is_ascii_whitespace() || source[position] == 0) {
        position += 1;
    }
    position
}

fn validate_pdf_header(source: &[u8]) -> Result<(), PdfError> {
    if source.len() < 9 || !source.starts_with(b"%PDF-") {
        return Err(PdfError::InvalidHeader);
    }
    let major = source[5];
    let minor = source[7];
    if !major.is_ascii_digit() || source[6] != b'.' || !minor.is_ascii_digit() {
        return Err(PdfError::InvalidHeader);
    }
    if !matches!((major, minor), (b'1', b'0'..=b'7') | (b'2', b'0')) {
        return Err(PdfError::InvalidHeader);
    }
    if !matches!(source[8], b'\r' | b'\n') {
        return Err(PdfError::InvalidHeader);
    }
    let after_header = consume_line_ending(source, 8, source.len());
    if after_header >= source.len() {
        return Err(PdfError::InvalidHeader);
    }
    // A binary marker, when present, must be a complete comment on the line
    // immediately following the header. It is never interpreted as syntax.
    if source[after_header] == b'%' {
        let marker_end = source[after_header..]
            .iter()
            .position(|byte| matches!(byte, b'\r' | b'\n'))
            .map(|offset| after_header + offset)
            .ok_or(PdfError::InvalidHeader)?;
        if marker_end == after_header + 1 {
            return Err(PdfError::InvalidHeader);
        }
    }
    Ok(())
}

/// Locate the final `startxref` and validate its trailing `%%EOF`, returning
/// the offset of the cross-reference structure and the byte position of the
/// marker (which bounds the classic xref/trailer scan).
fn locate_xref_offset(
    source: &[u8],
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<(usize, usize), PdfError> {
    let startxref_position =
        last_line_keyword_offset(source, b"startxref", cancelled)?.ok_or(PdfError::Malformed)?;
    let mut position = startxref_position + b"startxref".len();
    position = skip_space_and_comments(source, position, source.len());
    let (xref_offset_u64, after_offset) = parse_ascii_u64(source, position, source.len())?;
    let xref_offset = usize::try_from(xref_offset_u64).map_err(|_| PdfError::UnsupportedXref)?;
    if xref_offset >= startxref_position {
        return Err(PdfError::UnsupportedXref);
    }
    let eof_position = skip_pdf_whitespace(source, after_offset, source.len());
    if !source
        .get(eof_position..)
        .is_some_and(|tail| tail.starts_with(b"%%EOF"))
    {
        return Err(PdfError::Malformed);
    }
    let after_eof = eof_position + b"%%EOF".len();
    if !source[after_eof..]
        .iter()
        .all(|byte| byte.is_ascii_whitespace())
    {
        return Err(PdfError::Malformed);
    }
    Ok((xref_offset, startxref_position))
}

fn last_line_keyword_offset(
    source: &[u8],
    keyword: &[u8],
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<Option<usize>, PdfError> {
    let mut found = None;
    let mut line_start = 0;
    while line_start < source.len() {
        check_cancelled(cancelled)?;
        let mut position = line_start;
        while position < source.len() && matches!(source[position], 0 | b'\t' | 0x0c | b' ') {
            position += 1;
        }
        if source
            .get(position..)
            .is_some_and(|tail| tail.starts_with(keyword) && token_ends_at(tail, keyword.len()))
        {
            found = Some(position);
        }
        while position < source.len() && !matches!(source[position], b'\r' | b'\n') {
            position += 1;
        }
        line_start = consume_line_ending(source, position, source.len());
    }
    Ok(found)
}

/// Find the completed revision marker for a predecessor xref. The marker must
/// point at exactly `xref_offset`, have its own `%%EOF`, and precede the child
/// revision that references it; this avoids treating arbitrary stream text as
/// a revision boundary.
fn predecessor_startxref_position(
    source: &[u8],
    xref_offset: usize,
    before: usize,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<usize, PdfError> {
    let mut found = None;
    let mut line_start = 0;
    while line_start < before {
        check_cancelled(cancelled)?;
        let mut position = line_start;
        while position < before && matches!(source[position], 0 | b'\t' | 0x0c | b' ') {
            position += 1;
        }
        if source
            .get(position..before)
            .is_some_and(|tail| tail.starts_with(b"startxref") && token_ends_at(tail, 9))
        {
            let value_start = skip_space_and_comments(source, position + 9, before);
            if let Ok((candidate, after_value)) = parse_ascii_u64(source, value_start, before) {
                let eof = skip_pdf_whitespace(source, after_value, before);
                if usize::try_from(candidate).ok() == Some(xref_offset)
                    && source
                        .get(eof..before)
                        .is_some_and(|tail| tail.starts_with(b"%%EOF"))
                {
                    found = Some(position);
                }
            }
        }
        while position < before && !matches!(source[position], b'\r' | b'\n') {
            position += 1;
        }
        line_start = consume_line_ending(source, position, before);
    }
    found.ok_or(PdfError::Malformed)
}

/// Parse the cross-reference, dispatching to the classic `xref`/`trailer`
/// section or a cross-reference stream (type `/Type /XRef`).
fn parse_xref(
    source: &[u8],
    limits: &PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<XrefTable, PdfError> {
    let (mut xref_offset, mut startxref_position) = locate_xref_offset(source, cancelled)?;
    let mut entries = Vec::new();
    let mut seen_object_numbers = BTreeSet::new();
    let mut trailer = BTreeMap::new();
    let mut counters = ParseCounters::default();
    let mut xref_object_ids = BTreeSet::new();
    let mut objstm_members = BTreeMap::new();
    let mut seen_xref_offsets = BTreeSet::new();

    loop {
        if !seen_xref_offsets.insert(xref_offset) {
            return Err(PdfError::ReferenceLimit);
        }
        if seen_xref_offsets.len() > limits.max_reference_depth {
            return Err(PdfError::ReferenceLimit);
        }
        let section =
            parse_xref_section(source, xref_offset, startxref_position, limits, cancelled)?;
        counters.tokens = counters
            .tokens
            .checked_add(section.counters.tokens)
            .ok_or(PdfError::TokenLimit)?;
        counters.container_entries = counters
            .container_entries
            .checked_add(section.counters.container_entries)
            .ok_or(PdfError::TokenLimit)?;
        if counters.tokens > limits.max_tokens
            || counters.container_entries > limits.max_container_entries
        {
            return Err(PdfError::TokenLimit);
        }
        for (key, value) in &section.trailer {
            trailer.entry(key.clone()).or_insert_with(|| value.clone());
        }
        for mut entry in section.entries {
            entry.active = seen_object_numbers.insert(entry.id.number);
            entries.push(entry);
        }
        for (member, owner) in section.objstm_members {
            if seen_object_numbers.insert(member.number) {
                objstm_members.insert(member, owner);
            }
        }
        xref_object_ids.extend(section.xref_object_ids);

        if section.trailer.contains_key(b"Prev".as_slice())
            && !section.free_object_numbers.is_empty()
        {
            return Err(PdfError::UnsupportedIncremental);
        }

        let Some(PdfValue::Integer(previous)) = section.trailer.get(b"Prev".as_slice()) else {
            if section.trailer.contains_key(b"Prev".as_slice()) {
                return Err(PdfError::Malformed);
            }
            break;
        };
        let previous = usize::try_from(*previous).map_err(|_| PdfError::Malformed)?;
        if previous >= xref_offset {
            return Err(PdfError::Malformed);
        }
        startxref_position =
            predecessor_startxref_position(source, previous, xref_offset, cancelled)?;
        xref_offset = previous;
    }
    if trailer.contains_key(b"Encrypt".as_slice()) {
        return Err(PdfError::Encrypted);
    }
    if !matches!(
        trailer.get(b"Root".as_slice()),
        Some(PdfValue::Reference(_))
    ) {
        return Err(PdfError::Malformed);
    }
    if entries.is_empty() {
        return Err(PdfError::Malformed);
    }
    entries.sort_unstable_by_key(|entry| entry.offset);
    Ok(XrefTable {
        entries,
        trailer,
        counters,
        xref_object_ids,
        free_object_numbers: BTreeSet::new(),
        objstm_members,
    })
}

fn parse_xref_section(
    source: &[u8],
    xref_offset: usize,
    startxref_position: usize,
    limits: &PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<XrefTable, PdfError> {
    // The classic cross-reference section begins with the `xref` keyword at the
    // start of a line. A cross-reference *stream* object begins with `N 0 obj`,
    // so the byte before `xref` there is part of the object header; require a
    // preceding line break to disambiguate.
    let xref_is_classic = source
        .get(xref_offset..)
        .is_some_and(|tail| tail.starts_with(b"xref") && token_ends_at(tail, 4))
        && (xref_offset == 0 || matches!(source[xref_offset - 1], b'\n' | b'\r'));
    if xref_is_classic {
        parse_classic_xref(source, xref_offset, startxref_position, limits, cancelled)
    } else {
        parse_xref_stream(source, xref_offset, startxref_position, limits, cancelled)
    }
}

fn parse_xref_stream(
    source: &[u8],
    xref_offset: usize,
    section_end: usize,
    limits: &PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<XrefTable, PdfError> {
    // The xref stream is a cross-reference object: `id 0 obj << /Type /XRef
    // /Size N /W [w1 w2 w3] [/Index [...]] >> stream ... endstream endobj`.
    let mut counters = ParseCounters::default();
    let value_start = {
        let (_number, position) = parse_ascii_u64(source, xref_offset, section_end)?;
        let position = skip_required_space(source, position, section_end)?;
        let (_generation, position) = parse_ascii_u64(source, position, section_end)?;
        let position = skip_required_space(source, position, section_end)?;
        if !source
            .get(position..section_end)
            .is_some_and(|tail| tail.starts_with(b"obj") && token_ends_at(tail, 3))
        {
            return Err(PdfError::Malformed);
        }
        position + 3
    };
    let xref_object_id = {
        let number = u32::try_from(parse_ascii_u64(source, xref_offset, section_end)?.0)
            .map_err(|_| PdfError::ObjectLimit)?;
        ObjectId {
            number,
            generation: 0,
        }
    };
    let mut parser = ValueParser::new_trailer(
        source,
        value_start,
        section_end,
        limits,
        &mut counters,
        cancelled,
    );
    let value = parser.parse_value(0)?;
    // Capture the position right after the dictionary (before `dictionary` is
    // moved into the table at the end).
    let after_value = skip_space_and_comments(source, parser.position, section_end);
    let PdfValue::Dictionary(dictionary) = value else {
        return Err(PdfError::Malformed);
    };
    if !dictionary_name_is(&dictionary, b"Type", b"XRef") {
        return Err(PdfError::UnsupportedXref);
    }
    // Incremental xref streams have a separate object/stream lineage. Keep
    // that route fail-closed; the bounded merge below admits classic xref
    // revisions only.
    if dictionary.contains_key(b"Prev".as_slice()) {
        return Err(PdfError::UnsupportedIncremental);
    }
    // Read /W (fixed-width field sizes), /Size, and optional /Index.
    let width = xref_stream_width(&dictionary)?;
    let size_value = dictionary_integer(&dictionary, b"Size")?;
    let size = usize::try_from(size_value).map_err(|_| PdfError::ObjectLimit)?;
    if size == 0 || size > limits.max_objects {
        return Err(PdfError::ObjectLimit);
    }
    let index = xref_stream_index(&dictionary, size)?;

    // Consume `stream`, the data, and `endstream` / `endobj` to bound the
    // stream, then decode it.
    if !source
        .get(after_value..)
        .is_some_and(|tail| tail.starts_with(b"stream") && token_ends_at(tail, 6))
    {
        return Err(PdfError::InvalidStream);
    }
    let data_start = consume_stream_eol(source, after_value + 6, source.len()).unwrap();
    let length_value = dictionary
        .get(b"Length".as_slice())
        .ok_or(PdfError::InvalidStream)?;
    let PdfValue::Integer(length_value) = length_value else {
        return Err(PdfError::InvalidStream);
    };
    let length = usize::try_from(*length_value).map_err(|_| PdfError::InvalidStream)?;
    if length > limits.max_stream_input_bytes {
        return Err(PdfError::DecompressionLimit);
    }
    let data_end = data_start
        .checked_add(length)
        .filter(|end| *end <= section_end)
        .ok_or(PdfError::InvalidStream)?;
    let filter = stream_filter(&dictionary)?;
    let decoded = decode_stream_bytes(
        &source[data_start..data_end],
        filter,
        limits.max_stream_decoded_bytes,
        limits,
        cancelled,
    )?;

    // Walk the fixed-width entries, honoring the /Index subranges.
    let mut entries = Vec::new();
    let mut seen_ids = BTreeSet::new();
    let mut data_pos = 0_usize;
    // Type-2 members: member id -> (owning object-stream id, index within it).
    let mut objstm_members: BTreeMap<ObjectId, (u32, usize)> = BTreeMap::new();
    for &(range_start, range_count) in &index {
        let range_count = usize::try_from(range_count).map_err(|_| PdfError::ObjectLimit)?;
        for local in 0..range_count {
            if local % 1_024 == 0 {
                check_cancelled(cancelled)?;
            }
            let number = range_start
                .checked_add(local as u64)
                .ok_or(PdfError::ObjectLimit)?;
            let (kind, field2, field3, next) = read_xref_fields(&decoded, data_pos, &width)?;
            data_pos = next;
            if number == 0 {
                continue; // id 0 is the free head; never an object
            }
            let id_number = u32::try_from(number).map_err(|_| PdfError::ObjectLimit)?;
            if !seen_ids.insert(id_number) {
                return Err(PdfError::Malformed);
            }
            match kind {
                0 => { /* free */ }
                1 => {
                    let offset = usize::try_from(field2).map_err(|_| PdfError::Malformed)?;
                    let generation = u16::try_from(field3).map_err(|_| PdfError::Malformed)?;
                    if offset == 0 {
                        return Err(PdfError::Malformed);
                    }
                    validate_indirect_header(
                        source,
                        offset,
                        source.len(),
                        ObjectId {
                            number: id_number,
                            generation,
                        },
                    )?;
                    entries.push(XrefEntry {
                        id: ObjectId {
                            number: id_number,
                            generation,
                        },
                        offset,
                        section_end: xref_offset,
                        active: true,
                    });
                    if entries.len() > limits.max_objects {
                        return Err(PdfError::ObjectLimit);
                    }
                }
                2 => {
                    let owner_id = u32::try_from(field2).map_err(|_| PdfError::ObjectLimit)?;
                    let member_index = usize::try_from(field3).map_err(|_| PdfError::Malformed)?;
                    if objstm_members
                        .insert(
                            ObjectId {
                                number: id_number,
                                generation: 0,
                            },
                            (owner_id, member_index),
                        )
                        .is_some()
                    {
                        return Err(PdfError::Malformed);
                    }
                }
                _ => return Err(PdfError::Malformed),
            }
        }
    }
    if entries.is_empty() {
        return Err(PdfError::Malformed);
    }
    entries.sort_unstable_by_key(|entry| entry.offset);
    Ok(XrefTable {
        entries,
        trailer: dictionary,
        counters,
        xref_object_ids: BTreeSet::from([xref_object_id]),
        free_object_numbers: BTreeSet::new(),
        objstm_members,
    })
}

/// Read the fixed-width field sizes from a cross-reference stream's `/W`
/// array.
fn xref_stream_width(dictionary: &BTreeMap<Vec<u8>, PdfValue>) -> Result<[usize; 3], PdfError> {
    let PdfValue::Array(width) = dictionary.get(b"W".as_slice()).ok_or(PdfError::Malformed)? else {
        return Err(PdfError::Malformed);
    };
    if width.len() != 3 {
        return Err(PdfError::Malformed);
    }
    let mut out = [0_usize; 3];
    for (slot, value) in width.iter().enumerate() {
        let PdfValue::Integer(width) = value else {
            return Err(PdfError::Malformed);
        };
        let width = usize::try_from(*width).map_err(|_| PdfError::Malformed)?;
        if width == 0 || width > 10 {
            return Err(PdfError::Malformed);
        }
        out[slot] = width;
    }
    Ok(out)
}

/// Resolve the cross-reference stream's `/Index` subranges, defaulting to a
/// single `0 Size` range when absent.
fn xref_stream_index(
    dictionary: &BTreeMap<Vec<u8>, PdfValue>,
    size: usize,
) -> Result<Vec<(u64, u64)>, PdfError> {
    let Some(PdfValue::Array(index)) = dictionary.get(b"Index".as_slice()) else {
        return Ok(vec![(0, size as u64)]);
    };
    if index.len() % 2 != 0 || index.is_empty() {
        return Err(PdfError::Malformed);
    }
    let mut ranges = Vec::new();
    let mut seen = BTreeSet::new();
    for pair in index.chunks(2) {
        let start = match &pair[0] {
            PdfValue::Integer(value) => *value,
            _ => return Err(PdfError::Malformed),
        };
        let count = match &pair[1] {
            PdfValue::Integer(value) => *value,
            _ => return Err(PdfError::Malformed),
        };
        let start = usize::try_from(start).map_err(|_| PdfError::Malformed)?;
        let count = usize::try_from(count).map_err(|_| PdfError::Malformed)?;
        if count == 0 || start.saturating_add(count) > size || !seen.insert(start) {
            return Err(PdfError::Malformed);
        }
        ranges.push((start as u64, count as u64));
    }
    Ok(ranges)
}

/// Read three fixed-width big-endian integers from a cross-reference stream's
/// decoded payload, returning the values and the next byte position.
fn read_xref_fields(
    decoded: &[u8],
    pos: usize,
    width: &[usize; 3],
) -> Result<(u64, u64, u64, usize), PdfError> {
    let mut out = [0_u64; 3];
    let mut position = pos;
    for (slot, field_width) in width.iter().copied().enumerate() {
        let end = position
            .checked_add(field_width)
            .filter(|end| *end <= decoded.len())
            .ok_or(PdfError::Malformed)?;
        let mut value = 0_u64;
        for byte in &decoded[position..end] {
            value = value
                .checked_shl(8)
                .and_then(|v| v.checked_add(u64::from(*byte)))
                .ok_or(PdfError::Malformed)?;
        }
        out[slot] = value;
        position = end;
    }
    Ok((out[0], out[1], out[2], position))
}

fn parse_classic_xref(
    source: &[u8],
    xref_offset: usize,
    startxref_position: usize,
    limits: &PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<XrefTable, PdfError> {
    let mut entries = Vec::new();
    let mut seen_ids = BTreeSet::new();
    let mut normal_offsets = BTreeSet::new();
    let mut free_object_numbers = BTreeSet::new();
    let mut max_seen_id = 0_u32;
    let mut position = xref_offset + 4;
    loop {
        check_cancelled(cancelled)?;
        position = skip_blank_lines(source, position, startxref_position);
        if source
            .get(position..)
            .is_some_and(|tail| tail.starts_with(b"trailer") && token_ends_at(tail, 7))
        {
            position += 7;
            break;
        }
        let (header, next) = read_line(source, position, startxref_position)?;
        position = next;
        let mut fields = header
            .split(u8::is_ascii_whitespace)
            .filter(|field| !field.is_empty());
        let (Some(first), Some(count), None) = (fields.next(), fields.next(), fields.next()) else {
            return Err(PdfError::Malformed);
        };
        let first = parse_decimal_field(first)?;
        let count = parse_decimal_field(count)?;
        let end_id = first.checked_add(count).ok_or(PdfError::ObjectLimit)?;
        if count == 0 || end_id > limits.max_objects as u64 {
            return Err(PdfError::ObjectLimit);
        }
        for index in 0..count {
            if index % 1_024 == 0 {
                check_cancelled(cancelled)?;
            }
            let (row, next) = read_line(source, position, startxref_position)?;
            position = next;
            let mut fields = row
                .split(u8::is_ascii_whitespace)
                .filter(|field| !field.is_empty());
            let (Some(offset), Some(generation), Some(state), None) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                return Err(PdfError::Malformed);
            };
            if offset.len() != 10 || generation.len() != 5 || state.len() != 1 {
                return Err(PdfError::Malformed);
            }
            let offset = parse_decimal_field(offset)?;
            let generation = parse_decimal_field(generation)?;
            let number = first.checked_add(index).ok_or(PdfError::ObjectLimit)?;
            let id = ObjectId {
                number: u32::try_from(number).map_err(|_| PdfError::ObjectLimit)?,
                generation: u16::try_from(generation).map_err(|_| PdfError::Malformed)?,
            };
            if !seen_ids.insert(id.number) {
                return Err(PdfError::Malformed);
            }
            max_seen_id = max_seen_id.max(id.number);
            match state[0] {
                b'f' if id.number == 0 => {}
                b'f' => {
                    free_object_numbers.insert(id.number);
                }
                b'n' => {
                    let offset = usize::try_from(offset).map_err(|_| PdfError::Malformed)?;
                    if offset == 0 || offset >= xref_offset || !normal_offsets.insert(offset) {
                        return Err(PdfError::Malformed);
                    }
                    validate_indirect_header(source, offset, xref_offset, id)?;
                    entries.try_reserve(1).map_err(|_| PdfError::ObjectLimit)?;
                    entries.push(XrefEntry {
                        id,
                        offset,
                        section_end: xref_offset,
                        active: true,
                    });
                    if entries.len() > limits.max_objects {
                        return Err(PdfError::ObjectLimit);
                    }
                }
                _ => return Err(PdfError::Malformed),
            }
        }
    }
    if entries.is_empty() {
        return Err(PdfError::Malformed);
    }

    let mut counters = ParseCounters::default();
    let mut trailer_parser = ValueParser::new_trailer(
        source,
        position,
        startxref_position,
        limits,
        &mut counters,
        cancelled,
    );
    let PdfValue::Dictionary(trailer) = trailer_parser.parse_value(0)? else {
        return Err(PdfError::Malformed);
    };
    if trailer.contains_key(b"XRefStm".as_slice()) {
        return Err(PdfError::HybridXref);
    }
    if skip_space_and_comments(source, trailer_parser.position, startxref_position)
        != startxref_position
    {
        return Err(PdfError::Malformed);
    }
    let size = dictionary_integer(&trailer, b"Size")?;
    if size <= 0 {
        return Err(PdfError::Malformed);
    }
    let size = u32::try_from(size).map_err(|_| PdfError::ObjectLimit)?;
    if usize::try_from(size).unwrap_or(usize::MAX) > limits.max_objects {
        return Err(PdfError::ObjectLimit);
    }
    let incremental = trailer.contains_key(b"Prev".as_slice());
    if (!incremental && size != max_seen_id.checked_add(1).ok_or(PdfError::ObjectLimit)?)
        || (incremental && size <= max_seen_id)
    {
        return Err(PdfError::Malformed);
    }
    entries.sort_unstable_by_key(|entry| entry.offset);
    Ok(XrefTable {
        entries,
        trailer,
        counters,
        xref_object_ids: BTreeSet::new(),
        free_object_numbers,
        objstm_members: BTreeMap::new(),
    })
}

#[cfg(test)]
fn unique_line_keyword_offset(
    source: &[u8],
    keyword: &[u8],
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<Option<usize>, PdfError> {
    let mut found = None;
    let mut line_start = 0;
    while line_start < source.len() {
        check_cancelled(cancelled)?;
        let mut position = line_start;
        while position < source.len() && matches!(source[position], 0 | b'\t' | 0x0c | b' ') {
            position += 1;
            if position.is_multiple_of(DECODE_CHUNK_BYTES) {
                check_cancelled(cancelled)?;
            }
        }
        if source
            .get(position..)
            .is_some_and(|tail| tail.starts_with(keyword) && token_ends_at(tail, keyword.len()))
            && found.replace(position).is_some()
        {
            return Err(PdfError::UnsupportedIncremental);
        }
        let mut line_end = position;
        while line_end < source.len() && !matches!(source[line_end], b'\r' | b'\n') {
            line_end += 1;
            if line_end.is_multiple_of(DECODE_CHUNK_BYTES) {
                check_cancelled(cancelled)?;
            }
        }
        if line_end == source.len() {
            break;
        }
        line_start = consume_line_ending(source, line_end, source.len());
    }
    Ok(found)
}

fn token_ends_at(source: &[u8], length: usize) -> bool {
    source
        .get(length)
        .is_none_or(|byte| is_pdf_delimiter(*byte))
}

fn skip_blank_lines(source: &[u8], mut position: usize, end: usize) -> usize {
    while position < end {
        let line_end = source[position..end]
            .iter()
            .position(|byte| matches!(byte, b'\r' | b'\n'))
            .map_or(end, |offset| position + offset);
        let line = &source[position..line_end];
        if !line.iter().all(u8::is_ascii_whitespace) {
            return position;
        }
        position = consume_line_ending(source, line_end, end);
    }
    position
}

fn read_line(source: &[u8], position: usize, end: usize) -> Result<(&[u8], usize), PdfError> {
    if position >= end {
        return Err(PdfError::Malformed);
    }
    let line_end = source[position..end]
        .iter()
        .position(|byte| matches!(byte, b'\r' | b'\n'))
        .map_or(end, |offset| position + offset);
    if line_end == end {
        return Err(PdfError::Malformed);
    }
    Ok((
        &source[position..line_end],
        consume_line_ending(source, line_end, end),
    ))
}

fn consume_line_ending(source: &[u8], mut position: usize, end: usize) -> usize {
    if position < end && source[position] == b'\r' {
        position += 1;
        if position < end && source[position] == b'\n' {
            position += 1;
        }
    } else if position < end && source[position] == b'\n' {
        position += 1;
    }
    position
}

fn parse_decimal_field(field: &[u8]) -> Result<u64, PdfError> {
    if field.is_empty() || !field.iter().all(u8::is_ascii_digit) {
        return Err(PdfError::Malformed);
    }
    field.iter().try_fold(0_u64, |value, byte| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(*byte - b'0')))
            .ok_or(PdfError::ObjectLimit)
    })
}

fn parse_ascii_u64(source: &[u8], start: usize, end: usize) -> Result<(u64, usize), PdfError> {
    let mut position = start;
    while position < end && source[position].is_ascii_digit() {
        position += 1;
    }
    if position == start {
        return Err(PdfError::Malformed);
    }
    Ok((parse_decimal_field(&source[start..position])?, position))
}

fn validate_indirect_header(
    source: &[u8],
    offset: usize,
    end: usize,
    expected: ObjectId,
) -> Result<(), PdfError> {
    let (number, position) = parse_ascii_u64(source, offset, end)?;
    let position = skip_required_space(source, position, end)?;
    let (generation, position) = parse_ascii_u64(source, position, end)?;
    let position = skip_required_space(source, position, end)?;
    if !source
        .get(position..end)
        .is_some_and(|tail| tail.starts_with(b"obj") && token_ends_at(tail, 3))
        || number != u64::from(expected.number)
        || generation != u64::from(expected.generation)
    {
        return Err(PdfError::Malformed);
    }
    Ok(())
}

fn skip_required_space(source: &[u8], mut position: usize, end: usize) -> Result<usize, PdfError> {
    let start = position;
    while position < end && source[position].is_ascii_whitespace() {
        position += 1;
    }
    (position > start)
        .then_some(position)
        .ok_or(PdfError::Malformed)
}

fn dictionary_integer(
    dictionary: &BTreeMap<Vec<u8>, PdfValue>,
    key: &[u8],
) -> Result<i64, PdfError> {
    match dictionary.get(key) {
        Some(PdfValue::Integer(value)) => Ok(*value),
        _ => Err(PdfError::Malformed),
    }
}

fn parse_indirect_objects(
    source: &[u8],
    xref: XrefTable,
    limits: &PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<ParsedPdf, PdfError> {
    let XrefTable {
        entries,
        trailer,
        mut counters,
        xref_object_ids,
        objstm_members,
        ..
    } = xref;
    let mut objects = BTreeMap::new();
    let mut total_stream_input = 0_usize;
    let mut stream_count = 0_usize;

    for (index, entry) in entries.iter().enumerate() {
        check_cancelled(cancelled)?;
        if !entry.active {
            continue;
        }
        // Producers may legally list the xref stream object itself as a
        // type-1 entry in its own table. That object starts exactly at
        // `xref_offset` and extends to end-of-file, so the trailing span
        // bound would reject it. Its dictionary is already captured as the
        // trailer and its stream was decoded by `parse_xref_stream`, so skip
        // re-parsing it.
        if xref_object_ids.contains(&entry.id) {
            continue;
        }
        let span_end = entries
            .get(index + 1)
            .map_or(entry.section_end, |next| next.offset.min(entry.section_end));
        if entry.offset >= span_end {
            return Err(PdfError::Malformed);
        }
        let (value, stream, stream_len) =
            parse_object_at_offset(source, entry, span_end, &mut counters, limits, cancelled)?;
        total_stream_input = total_stream_input
            .checked_add(stream_len)
            .ok_or(PdfError::DecompressionLimit)?;
        if total_stream_input > limits.max_input_bytes {
            return Err(PdfError::DecompressionLimit);
        }
        if stream.is_some() {
            stream_count = stream_count
                .checked_add(1)
                .ok_or(PdfError::DecompressionLimit)?;
            if stream_count > limits.max_streams {
                return Err(PdfError::DecompressionLimit);
            }
        }
        if objects
            .insert(entry.id, PdfObject { value, stream })
            .is_some()
        {
            return Err(PdfError::Malformed);
        }
    }
    if objects.len() > limits.max_objects {
        return Err(PdfError::ObjectLimit);
    }
    // Expand object-stream members into their own objects. Two sources:
    //  * classic xref: any object whose value is `/Type /ObjStm`; and
    //  * xref streams: type-2 entries recorded in `objstm_members`.
    // The xref-stream object itself (a `/Type /XRef` stream) is not an object
    // stream, so it is never expanded here.
    let mut members = BTreeMap::new();
    // First pass: classic object streams (their values were parsed above).
    for object in objects.values() {
        let (PdfValue::Dictionary(dict), Some(spec)) = (&object.value, object.stream.as_ref())
        else {
            continue;
        };
        if !dictionary_name_is(dict, b"Type", b"ObjStm") {
            continue;
        }
        let count = dictionary_integer(dict, b"N")?;
        let first = dictionary_integer(dict, b"First")?;
        let count = usize::try_from(count).map_err(|_| PdfError::ObjectLimit)?;
        if count > limits.max_container_entries {
            return Err(PdfError::ObjectLimit);
        }
        let decoded = decode_stream_bytes(
            &source[spec.encoded.clone()],
            spec.filter,
            limits.max_stream_decoded_bytes,
            limits,
            cancelled,
        )?;
        let expanded =
            parse_objstm_members(&decoded, first, count, &mut counters, limits, cancelled)?;
        for (member_id, member) in expanded {
            if objects.contains_key(&member_id) || members.contains_key(&member_id) {
                return Err(PdfError::Malformed);
            }
            members.insert(member_id, member);
        }
    }
    // Second pass: xref-stream type-2 members, decoded from their owning
    // object stream (which must itself be a parsed object above).
    if !xref_object_ids.is_empty() && !objstm_members.is_empty() {
        // Resolve each owning object stream's full id (with generation) from
        // the type-1 entries.
        let owner_ids: BTreeMap<u32, ObjectId> = entries
            .iter()
            .filter(|entry| {
                objstm_members
                    .values()
                    .any(|(owner, _)| *owner == entry.id.number)
            })
            .map(|entry| (entry.id.number, entry.id))
            .collect();
        for (member_id, (owner_id, member_index)) in &objstm_members {
            // If the first pass already expanded this member (classic ObjStm
            // detection), skip it to avoid a duplicate-insert collision.
            if objects.contains_key(member_id) || members.contains_key(member_id) {
                continue;
            }
            let owner_id = owner_ids
                .get(owner_id)
                .copied()
                .ok_or(PdfError::UnsupportedObjectStream)?;
            let owner = objects
                .get(&owner_id)
                .ok_or(PdfError::UnsupportedObjectStream)?;
            let (PdfValue::Dictionary(dict), Some(spec)) = (&owner.value, owner.stream.as_ref())
            else {
                return Err(PdfError::UnsupportedObjectStream);
            };
            if !dictionary_name_is(dict, b"Type", b"ObjStm") {
                return Err(PdfError::UnsupportedObjectStream);
            }
            let count = dictionary_integer(dict, b"N")?;
            let first = dictionary_integer(dict, b"First")?;
            let count = usize::try_from(count).map_err(|_| PdfError::ObjectLimit)?;
            if *member_index >= count {
                return Err(PdfError::Malformed);
            }
            let decoded = decode_stream_bytes(
                &source[spec.encoded.clone()],
                spec.filter,
                limits.max_stream_decoded_bytes,
                limits,
                cancelled,
            )?;
            let list = parse_objstm_headers(&decoded, first, count)?;
            let (_, start, end) = list.get(*member_index).ok_or(PdfError::Malformed)?;
            let member_body = decoded[*start..*end].to_vec();
            let mut member_parser = ValueParser::new(
                &member_body,
                0,
                member_body.len(),
                limits,
                &mut counters,
                cancelled,
            );
            let value = member_parser.parse_value(0)?;
            if objects.contains_key(member_id) || members.contains_key(member_id) {
                return Err(PdfError::Malformed);
            }
            members.insert(
                *member_id,
                PdfObject {
                    value,
                    stream: None,
                },
            );
        }
    }
    for (id, object) in members {
        if objects.insert(id, object).is_some() {
            return Err(PdfError::Malformed);
        }
    }
    if objects.len() > limits.max_objects {
        return Err(PdfError::ObjectLimit);
    }
    Ok(ParsedPdf { objects, trailer })
}

/// Parse an object stream's `id offset` header list, returning the members'
/// id and value byte spans (relative to the decoded payload). The value spans
/// are `[offset, next_offset)` for each of the `count` members.
fn parse_objstm_headers(
    decoded: &[u8],
    first: i64,
    count: usize,
) -> Result<Vec<(u32, usize, usize)>, PdfError> {
    let first = usize::try_from(first).map_err(|_| PdfError::Malformed)?;
    if first > decoded.len() || count == 0 {
        return Err(PdfError::Malformed);
    }
    let mut ids = Vec::with_capacity(count);
    let mut offsets = Vec::with_capacity(count);
    let mut position = 0_usize;
    for _ in 0..count {
        if position + 1 > decoded.len() {
            return Err(PdfError::Malformed);
        }
        let (id, next) = parse_objstm_int(decoded, position)?;
        let (offset, next) = parse_objstm_int(decoded, next)?;
        ids.push(u32::try_from(id).map_err(|_| PdfError::ObjectLimit)?);
        let offset = usize::try_from(offset).map_err(|_| PdfError::Malformed)?;
        offsets.push(offset);
        position = next;
    }
    // Offsets are relative to `/First`; convert to absolute decoded-payload
    // indices. They must be contiguous, non-decreasing, and within the payload.
    let mut prev = first;
    for offset in &offsets {
        let abs = first
            .checked_add(*offset)
            .filter(|abs| *abs <= decoded.len())
            .ok_or(PdfError::Malformed)?;
        if abs < prev {
            return Err(PdfError::Malformed);
        }
        prev = abs;
    }
    let mut spans = Vec::with_capacity(count);
    for index in 0..count {
        let start = first + offsets[index];
        let end = offsets
            .get(index + 1)
            .map(|o| first + o)
            .unwrap_or(decoded.len());
        if start >= end {
            return Err(PdfError::Malformed);
        }
        spans.push((ids[index], start, end));
    }
    Ok(spans)
}

/// Parse the value (and optional stream) of the indirect object located at
/// `entry.offset`, returning the value, the stream spec (if any), and the raw
/// encoded stream byte length (for the cumulative stream-input budget).
fn parse_object_at_offset(
    source: &[u8],
    entry: &XrefEntry,
    span_end: usize,
    counters: &mut ParseCounters,
    limits: &PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<(PdfValue, Option<StreamSpec>, usize), PdfError> {
    let value_start = indirect_value_start(source, entry.offset, span_end, entry.id)?;
    let mut parser = ValueParser::new(source, value_start, span_end, limits, counters, cancelled);
    let value = parser.parse_value(0)?;
    let mut stream = None;
    let after_value = skip_space_and_comments(source, parser.position, span_end);
    let mut stream_len = 0_usize;
    if source
        .get(after_value..span_end)
        .is_some_and(|tail| tail.starts_with(b"stream") && token_ends_at(tail, 6))
    {
        let PdfValue::Dictionary(dictionary) = &value else {
            return Err(PdfError::InvalidStream);
        };
        parser.position = after_value;
        if parser.next()? != Token::Keyword(b"stream".to_vec()) {
            return Err(PdfError::InvalidStream);
        }
        let data_start = consume_stream_eol(source, parser.position, span_end)?;
        let length = dictionary
            .get(b"Length".as_slice())
            .ok_or(PdfError::InvalidStream)?;
        let PdfValue::Integer(length) = length else {
            // Indirect and cyclic lengths are deliberately unsupported;
            // no dependency gets a chance to cast or follow them.
            return Err(PdfError::InvalidStream);
        };
        let length = usize::try_from(*length).map_err(|_| PdfError::InvalidStream)?;
        if length > limits.max_stream_input_bytes {
            return Err(PdfError::DecompressionLimit);
        }
        stream_len = length;
        let data_end = data_start
            .checked_add(length)
            .filter(|end| *end <= span_end)
            .ok_or(PdfError::InvalidStream)?;
        let filter = stream_filter(dictionary)?;
        let after_endstream = consume_endstream(source, data_end, span_end)?;
        parser.position = after_endstream;
        let Token::Keyword(endobj) = parser.next()? else {
            return Err(PdfError::Malformed);
        };
        if endobj != b"endobj" {
            return Err(PdfError::Malformed);
        }
        if skip_space_and_comments(source, parser.position, span_end) != span_end {
            return Err(PdfError::Malformed);
        }
        stream = Some(StreamSpec {
            encoded: data_start..data_end,
            filter,
        });
    } else {
        parser.position = after_value;
        let Token::Keyword(endobj) = parser.next()? else {
            return Err(PdfError::Malformed);
        };
        if endobj != b"endobj"
            || skip_space_and_comments(source, parser.position, span_end) != span_end
        {
            return Err(PdfError::Malformed);
        }
    }
    Ok((value, stream, stream_len))
}

/// Expand the packed members of a decoded object stream into objects.
///
/// The decoded payload is `id_1 offset_1 id_2 offset_2 ...` (2 * N integers)
/// followed by the concatenated member values. Each member's value is re-parsed
/// from its byte range (offset relative to the start of the value section).
fn parse_objstm_members(
    decoded: &[u8],
    first: i64,
    count: usize,
    counters: &mut ParseCounters,
    limits: &PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<BTreeMap<ObjectId, PdfObject>, PdfError> {
    let spans = parse_objstm_headers(decoded, first, count)?;
    let mut members = BTreeMap::new();
    for (id, start, end) in spans {
        check_cancelled(cancelled)?;
        let member_body = decoded[start..end].to_vec();
        let mut member_parser = ValueParser::new(
            &member_body,
            0,
            member_body.len(),
            limits,
            counters,
            cancelled,
        );
        let value = member_parser.parse_value(0)?;
        let object_id = ObjectId {
            number: id,
            generation: 0,
        };
        members.insert(
            object_id,
            PdfObject {
                value,
                stream: None,
            },
        );
    }
    Ok(members)
}

/// Parse one non-negative decimal integer from a decoded object-stream header
/// or member, returning the value and the next position (after trailing space).
fn parse_objstm_int(source: &[u8], start: usize) -> Result<(i64, usize), PdfError> {
    let (value, position) = parse_ascii_u64(source, start, source.len())?;
    let position = skip_space_and_comments(source, position, source.len());
    let value = i64::try_from(value).map_err(|_| PdfError::Malformed)?;
    Ok((value, position))
}

fn indirect_value_start(
    source: &[u8],
    offset: usize,
    end: usize,
    expected: ObjectId,
) -> Result<usize, PdfError> {
    let (number, position) = parse_ascii_u64(source, offset, end)?;
    let position = skip_required_space(source, position, end)?;
    let (generation, position) = parse_ascii_u64(source, position, end)?;
    let position = skip_required_space(source, position, end)?;
    if number != u64::from(expected.number)
        || generation != u64::from(expected.generation)
        || !source
            .get(position..end)
            .is_some_and(|tail| tail.starts_with(b"obj") && token_ends_at(tail, 3))
    {
        return Err(PdfError::Malformed);
    }
    Ok(position + 3)
}

fn consume_stream_eol(source: &[u8], mut position: usize, end: usize) -> Result<usize, PdfError> {
    while position < end && matches!(source[position], b' ' | b'\t') {
        position += 1;
    }
    if position >= end || !matches!(source[position], b'\r' | b'\n') {
        return Err(PdfError::InvalidStream);
    }
    Ok(consume_line_ending(source, position, end))
}

fn consume_endstream(source: &[u8], data_end: usize, span_end: usize) -> Result<usize, PdfError> {
    for position in
        std::iter::once(data_end).chain(consume_one_line_ending(source, data_end, span_end))
    {
        if source
            .get(position..span_end)
            .is_some_and(|tail| tail.starts_with(b"endstream") && token_ends_at(tail, 9))
        {
            return Ok(position + 9);
        }
    }
    Err(PdfError::InvalidStream)
}

fn consume_one_line_ending(source: &[u8], position: usize, end: usize) -> Option<usize> {
    match source.get(position..end)? {
        [b'\r', b'\n', ..] => Some(position + 2),
        [b'\r' | b'\n', ..] => Some(position + 1),
        _ => None,
    }
}

fn stream_filter(dictionary: &BTreeMap<Vec<u8>, PdfValue>) -> Result<StreamFilter, PdfError> {
    if [b"F".as_slice(), b"FFilter", b"FDecodeParms"]
        .iter()
        .any(|key| dictionary.contains_key(*key))
    {
        // External-file streams are deliberately unsupported. Ignoring these
        // keys and indexing the inline bytes would publish text that a PDF
        // consumer does not treat as the authoritative stream contents.
        return Err(PdfError::ActiveContent);
    }
    // Filters this extractor cannot decode are recorded, not rejected: the
    // stream may be one graphoxide never decodes (image XObject, font file,
    // embedded file), in which case it is inert. `decode_stream_bytes` still
    // rejects `Unsupported` whenever a code path actually consumes the bytes,
    // so every decodable-by-us stream fails closed exactly as before.
    if dictionary
        .get(b"DecodeParms".as_slice())
        .is_some_and(|parameters| !matches!(parameters, PdfValue::Null))
    {
        // Predictors and parameterized decoding are excluded.
        return Ok(StreamFilter::Unsupported);
    }
    match dictionary.get(b"Filter".as_slice()) {
        None | Some(PdfValue::Null) => Ok(StreamFilter::Raw),
        Some(PdfValue::Name(filter)) if matches!(filter.as_slice(), b"FlateDecode" | b"Fl") => {
            Ok(StreamFilter::Flate)
        }
        _ => Ok(StreamFilter::Unsupported),
    }
}

fn resolve_reference_id(
    parsed: &ParsedPdf,
    start: ObjectId,
    limits: &PdfLimits,
) -> Result<ObjectId, PdfError> {
    let mut current = start;
    let mut seen = BTreeSet::new();
    for _ in 0..=limits.max_reference_depth {
        if !seen.insert(current) {
            return Err(PdfError::ReferenceLimit);
        }
        let object = parsed.objects.get(&current).ok_or(PdfError::Malformed)?;
        match object.value {
            PdfValue::Reference(next) => current = next,
            _ => return Ok(current),
        }
    }
    Err(PdfError::ReferenceLimit)
}

fn resolve_value<'a>(
    parsed: &'a ParsedPdf,
    value: &'a PdfValue,
    limits: &PdfLimits,
) -> Result<(Option<ObjectId>, &'a PdfValue), PdfError> {
    let PdfValue::Reference(id) = value else {
        return Ok((None, value));
    };
    let id = resolve_reference_id(parsed, *id, limits)?;
    let value = &parsed.objects.get(&id).ok_or(PdfError::Malformed)?.value;
    Ok((Some(id), value))
}

fn object_dictionary(
    parsed: &ParsedPdf,
    id: ObjectId,
) -> Result<&BTreeMap<Vec<u8>, PdfValue>, PdfError> {
    match &parsed.objects.get(&id).ok_or(PdfError::Malformed)?.value {
        PdfValue::Dictionary(dictionary) => Ok(dictionary),
        _ => Err(PdfError::Malformed),
    }
}

fn dictionary_name_is(dictionary: &BTreeMap<Vec<u8>, PdfValue>, key: &[u8], name: &[u8]) -> bool {
    matches!(dictionary.get(key), Some(PdfValue::Name(value)) if value == name)
}

fn collect_page_ids(
    parsed: &ParsedPdf,
    limits: &PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<Vec<ObjectId>, PdfError> {
    let root = parsed
        .trailer
        .get(b"Root".as_slice())
        .ok_or(PdfError::Malformed)?;
    let (Some(_catalog_id), PdfValue::Dictionary(catalog)) = resolve_value(parsed, root, limits)?
    else {
        return Err(PdfError::Malformed);
    };
    if !dictionary_name_is(catalog, b"Type", b"Catalog") {
        return Err(PdfError::Malformed);
    }
    let pages = catalog
        .get(b"Pages".as_slice())
        .ok_or(PdfError::Malformed)?;
    let (Some(pages_id), PdfValue::Dictionary(_)) = resolve_value(parsed, pages, limits)? else {
        return Err(PdfError::Malformed);
    };

    let mut stack = vec![(pages_id, 0_usize)];
    let mut seen = BTreeSet::new();
    let mut pages = Vec::new();
    while let Some((id, depth)) = stack.pop() {
        check_cancelled(cancelled)?;
        if depth > limits.max_page_tree_depth || !seen.insert(id) {
            return Err(PdfError::ReferenceLimit);
        }
        let dictionary = object_dictionary(parsed, id)?;
        if dictionary_name_is(dictionary, b"Type", b"Page") {
            pages.try_reserve(1).map_err(|_| PdfError::PageLimit)?;
            pages.push(id);
            if pages.len() > limits.max_pages {
                return Err(PdfError::PageLimit);
            }
            validate_page_parent_chain(parsed, id, pages_id, limits)?;
            continue;
        }
        if !dictionary_name_is(dictionary, b"Type", b"Pages") {
            return Err(PdfError::Malformed);
        }
        if let Some(PdfValue::Integer(count)) = dictionary.get(b"Count".as_slice())
            && (*count < 0 || usize::try_from(*count).unwrap_or(usize::MAX) > limits.max_pages)
        {
            return Err(PdfError::PageLimit);
        }
        let kids = dictionary
            .get(b"Kids".as_slice())
            .ok_or(PdfError::Malformed)?;
        let (_, PdfValue::Array(kids)) = resolve_value(parsed, kids, limits)? else {
            return Err(PdfError::Malformed);
        };
        if kids.len() > limits.max_pages {
            return Err(PdfError::PageLimit);
        }
        for kid in kids.iter().rev() {
            let (Some(kid_id), PdfValue::Dictionary(_)) = resolve_value(parsed, kid, limits)?
            else {
                return Err(PdfError::Malformed);
            };
            let kid_dictionary = object_dictionary(parsed, kid_id)?;
            let Some(PdfValue::Reference(parent)) = kid_dictionary.get(b"Parent".as_slice()) else {
                return Err(PdfError::Malformed);
            };
            if resolve_reference_id(parsed, *parent, limits)? != id {
                return Err(PdfError::Malformed);
            }
            stack.push((kid_id, depth + 1));
        }
    }
    let declared_count = dictionary_integer(object_dictionary(parsed, pages_id)?, b"Count")?;
    if declared_count < 0 || usize::try_from(declared_count).ok() != Some(pages.len()) {
        return Err(PdfError::Malformed);
    }
    Ok(pages)
}

/// Return only outline metadata that is safe to attach to an admitted page.
/// Outline failures are deliberately non-fatal: a bookmark is navigational
/// metadata, not source text. A malformed outline therefore cannot suppress
/// page extraction or make an incomplete outline appear authoritative.
fn collect_page_outlines(
    parsed: &ParsedPdf,
    page_ids: &[ObjectId],
    limits: &PdfLimits,
) -> BTreeMap<ObjectId, PdfPageOutline> {
    let Some(PdfValue::Reference(root)) = parsed.trailer.get(b"Root".as_slice()) else {
        return BTreeMap::new();
    };
    let Ok(root) = resolve_reference_id(parsed, *root, limits) else {
        return BTreeMap::new();
    };
    let Ok(catalog) = object_dictionary(parsed, root) else {
        return BTreeMap::new();
    };
    let Some(outlines) = catalog.get(b"Outlines".as_slice()) else {
        return BTreeMap::new();
    };
    let Ok((Some(_), PdfValue::Dictionary(outlines))) = resolve_value(parsed, outlines, limits)
    else {
        return BTreeMap::new();
    };
    let Some(first) = outlines.get(b"First".as_slice()) else {
        return BTreeMap::new();
    };
    if !valid_outline_tree(parsed, first, limits, 0, &mut BTreeSet::new()) {
        return BTreeMap::new();
    }

    let page_ids = page_ids.iter().copied().collect::<BTreeSet<_>>();
    let mut entries = BTreeMap::new();
    let mut metadata_bytes = 0usize;
    collect_outline_chain(
        parsed,
        first,
        &page_ids,
        limits,
        &[],
        &mut metadata_bytes,
        &mut entries,
    );
    entries
}

fn valid_outline_tree(
    parsed: &ParsedPdf,
    value: &PdfValue,
    limits: &PdfLimits,
    depth: usize,
    seen: &mut BTreeSet<ObjectId>,
) -> bool {
    if depth > MAX_OUTLINE_DEPTH || seen.len() >= limits.max_objects {
        return false;
    }
    let Ok((Some(id), PdfValue::Dictionary(dictionary))) = resolve_value(parsed, value, limits)
    else {
        return false;
    };
    if !seen.insert(id) {
        return false;
    }
    [b"First".as_slice(), b"Next".as_slice()]
        .into_iter()
        .all(|key| {
            dictionary
                .get(key)
                .is_none_or(|child| valid_outline_tree(parsed, child, limits, depth + 1, seen))
        })
}

fn collect_outline_chain(
    parsed: &ParsedPdf,
    value: &PdfValue,
    page_ids: &BTreeSet<ObjectId>,
    limits: &PdfLimits,
    parent_path: &[String],
    metadata_bytes: &mut usize,
    entries: &mut BTreeMap<ObjectId, PdfPageOutline>,
) {
    let Ok((_, PdfValue::Dictionary(dictionary))) = resolve_value(parsed, value, limits) else {
        return;
    };
    let title = dictionary
        .get(b"Title".as_slice())
        .and_then(|title| outline_title(parsed, title, limits));
    let mut path = parent_path.to_vec();
    let path_includes_title = if let Some(title) = &title {
        let next_len = path
            .iter()
            .map(String::len)
            .sum::<usize>()
            .saturating_add(path.len().saturating_mul(3))
            .saturating_add(title.len());
        if next_len <= MAX_OUTLINE_PATH_BYTES {
            path.push(title.clone());
            true
        } else {
            false
        }
    } else {
        true
    };
    if path_includes_title
        && let (Some(title), Some(path)) = (title, outline_path(&path))
        && let Some(page_id) = dictionary
            .get(b"Dest".as_slice())
            .and_then(|destination| outline_destination_page(parsed, destination, limits))
            .filter(|page_id| page_ids.contains(page_id))
    {
        let entry_bytes = title.len().checked_add(path.len());
        if !entries.contains_key(&page_id)
            && let Some(total_bytes) = entry_bytes
                .and_then(|entry_bytes| metadata_bytes.checked_add(entry_bytes))
                .filter(|total| *total <= limits.max_metadata_bytes)
        {
            *metadata_bytes = total_bytes;
            entries.insert(
                page_id,
                PdfPageOutline {
                    heading: title,
                    path,
                },
            );
        }
    }
    if let Some(first) = dictionary.get(b"First".as_slice()) {
        collect_outline_chain(
            parsed,
            first,
            page_ids,
            limits,
            &path,
            metadata_bytes,
            entries,
        );
    }
    if let Some(next) = dictionary.get(b"Next".as_slice()) {
        collect_outline_chain(
            parsed,
            next,
            page_ids,
            limits,
            parent_path,
            metadata_bytes,
            entries,
        );
    }
}

fn outline_title(parsed: &ParsedPdf, value: &PdfValue, limits: &PdfLimits) -> Option<String> {
    let (_, PdfValue::String(bytes)) = resolve_value(parsed, value, limits).ok()? else {
        return None;
    };
    let title =
        sanitize_metadata_string(decode_info_text_string(bytes, MAX_OUTLINE_TITLE_BYTES).ok()?);
    (!title.is_empty() && title.len() <= MAX_OUTLINE_TITLE_BYTES).then_some(title)
}

fn outline_path(path: &[String]) -> Option<String> {
    let path = path.join(" / ");
    (!path.is_empty() && path.len() <= MAX_OUTLINE_PATH_BYTES).then_some(path)
}

fn outline_destination_page(
    parsed: &ParsedPdf,
    destination: &PdfValue,
    limits: &PdfLimits,
) -> Option<ObjectId> {
    let (_, PdfValue::Array(destination)) = resolve_value(parsed, destination, limits).ok()? else {
        return None;
    };
    let PdfValue::Reference(page) = destination.first()? else {
        return None;
    };
    resolve_reference_id(parsed, *page, limits).ok()
}

fn validate_page_parent_chain(
    parsed: &ParsedPdf,
    page_id: ObjectId,
    pages_root_id: ObjectId,
    limits: &PdfLimits,
) -> Result<(), PdfError> {
    let mut current = page_id;
    let mut seen = BTreeSet::new();
    for _ in 0..=limits.max_page_tree_depth {
        if current == pages_root_id {
            return Ok(());
        }
        if !seen.insert(current) {
            return Err(PdfError::ReferenceLimit);
        }
        let dictionary = object_dictionary(parsed, current)?;
        let Some(PdfValue::Reference(parent)) = dictionary.get(b"Parent".as_slice()) else {
            return Err(PdfError::Malformed);
        };
        let parent = resolve_reference_id(parsed, *parent, limits)?;
        if !dictionary_name_is(object_dictionary(parsed, parent)?, b"Type", b"Pages") {
            return Err(PdfError::Malformed);
        }
        current = parent;
    }
    Err(PdfError::ReferenceLimit)
}

fn page_content_ids(
    parsed: &ParsedPdf,
    page_id: ObjectId,
    limits: &PdfLimits,
) -> Result<Vec<ObjectId>, PdfError> {
    let page = object_dictionary(parsed, page_id)?;
    let Some(contents) = page.get(b"Contents".as_slice()) else {
        return Ok(Vec::new());
    };
    let mut ids = Vec::new();
    match contents {
        PdfValue::Reference(id) => ids.push(resolve_reference_id(parsed, *id, limits)?),
        PdfValue::Array(values) => {
            if values.len() > limits.max_streams {
                return Err(PdfError::DecompressionLimit);
            }
            ids.try_reserve_exact(values.len())
                .map_err(|_| PdfError::DecompressionLimit)?;
            for value in values {
                let PdfValue::Reference(id) = value else {
                    return Err(PdfError::InvalidStream);
                };
                ids.push(resolve_reference_id(parsed, *id, limits)?);
            }
        }
        _ => return Err(PdfError::InvalidStream),
    }
    let mut seen = BTreeSet::new();
    for id in &ids {
        if !seen.insert(*id)
            || parsed
                .objects
                .get(id)
                .and_then(|object| object.stream.as_ref())
                .is_none()
        {
            return Err(PdfError::InvalidStream);
        }
    }
    Ok(ids)
}

struct DecodeBudget {
    limits: PdfLimits,
    decoded: BTreeMap<ObjectId, Vec<u8>>,
    decoded_streams: usize,
    total_decoded_bytes: usize,
    total_text_bytes: usize,
}

#[derive(Debug, Default)]
struct ContentBudget {
    tokens: usize,
    operations: usize,
}

/// A parsed ToUnicode CMap: code → Unicode scalar (one or more code bytes per
/// entry, since CID fonts use 2- or 4-byte codes). Bounded by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ToUnicodeCMap {
    /// Code width in bytes (2 or 4) for the font's CID encoding.
    code_width: usize,
    /// Ordered mapping from raw code bytes to a decoded Unicode string.
    /// Stored as a flat Vec for a bounded, deterministic lookup.
    entries: Vec<(Vec<u8>, String)>,
}

impl ToUnicodeCMap {
    /// Map a raw code (of `code_width` bytes) to its Unicode string, if any.
    fn translate(&self, code: &[u8]) -> Option<&str> {
        self.entries
            .iter()
            .find(|(key, _)| key == code)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FontEncoding {
    Standard,
    WinAnsi,
    /// A Type0/CID font whose text is decoded through its ToUnicode CMap.
    ToUnicode(ToUnicodeCMap),
}

fn page_resources<'a>(
    parsed: &'a ParsedPdf,
    page_id: ObjectId,
    limits: &PdfLimits,
) -> Result<Option<&'a BTreeMap<Vec<u8>, PdfValue>>, PdfError> {
    let mut current = page_id;
    let mut seen = BTreeSet::new();
    for _ in 0..=limits.max_reference_depth {
        if !seen.insert(current) {
            return Err(PdfError::ReferenceLimit);
        }
        let dictionary = object_dictionary(parsed, current)?;
        if let Some(resources) = dictionary.get(b"Resources".as_slice()) {
            let (_, PdfValue::Dictionary(resources)) = resolve_value(parsed, resources, limits)?
            else {
                return Err(PdfError::Malformed);
            };
            return Ok(Some(resources));
        }
        let Some(PdfValue::Reference(parent)) = dictionary.get(b"Parent".as_slice()) else {
            return Ok(None);
        };
        current = resolve_reference_id(parsed, *parent, limits)?;
    }
    Err(PdfError::ReferenceLimit)
}

fn page_visual_inventory(
    parsed: &ParsedPdf,
    page_id: ObjectId,
    limits: &PdfLimits,
) -> PdfPageVisualInventory {
    let mut inventory = PdfPageVisualInventory {
        media_box: inherited_page_value(parsed, page_id, b"MediaBox", limits)
            .and_then(pdf_box_dimensions),
        ..Default::default()
    };
    let Some(PdfValue::Dictionary(resources)) =
        inherited_page_value(parsed, page_id, b"Resources", limits)
    else {
        return inventory;
    };
    let Some(xobjects) = resources.get(b"XObject".as_slice()) else {
        inventory.xobject_resource_count = Some(0);
        inventory.image_xobject_count = Some(0);
        inventory.form_xobject_count = Some(0);
        return inventory;
    };
    let Some(PdfValue::Dictionary(xobjects)) = resolve_value(parsed, xobjects, limits)
        .ok()
        .map(|(_, value)| value)
    else {
        return inventory;
    };
    if xobjects.len() > MAX_VISUAL_XOBJECTS_PER_PAGE {
        inventory.xobject_resources_limited = true;
        return inventory;
    }

    let mut image_count = 0_usize;
    let mut form_count = 0_usize;
    for xobject in xobjects.values() {
        let Some(PdfValue::Dictionary(xobject)) = resolve_value(parsed, xobject, limits)
            .ok()
            .map(|(_, value)| value)
        else {
            continue;
        };
        if !dictionary_name_is(xobject, b"Type", b"XObject") {
            continue;
        }
        match xobject.get(b"Subtype".as_slice()) {
            Some(PdfValue::Name(subtype)) if subtype == b"Image" => {
                image_count += 1;
                if let Some(dimensions) = xobject_dimensions(xobject) {
                    inventory.image_xobject_dimensions.push(dimensions);
                }
            }
            Some(PdfValue::Name(subtype)) if subtype == b"Form" => form_count += 1,
            _ => {}
        }
    }
    inventory.xobject_resource_count = Some(xobjects.len());
    inventory.image_xobject_count = Some(image_count);
    inventory.form_xobject_count = Some(form_count);
    inventory
}

fn inherited_page_value<'a>(
    parsed: &'a ParsedPdf,
    page_id: ObjectId,
    key: &[u8],
    limits: &PdfLimits,
) -> Option<&'a PdfValue> {
    let mut current = page_id;
    let mut seen = BTreeSet::new();
    for _ in 0..=limits.max_reference_depth {
        if !seen.insert(current) {
            return None;
        }
        let dictionary = object_dictionary(parsed, current).ok()?;
        if let Some(value) = dictionary.get(key) {
            return resolve_value(parsed, value, limits)
                .ok()
                .map(|(_, value)| value);
        }
        let PdfValue::Reference(parent) = dictionary.get(b"Parent".as_slice())? else {
            return None;
        };
        current = resolve_reference_id(parsed, *parent, limits).ok()?;
    }
    None
}

fn pdf_box_dimensions(value: &PdfValue) -> Option<(i64, i64)> {
    let PdfValue::Array(values) = value else {
        return None;
    };
    let [PdfValue::Integer(left), PdfValue::Integer(bottom), PdfValue::Integer(right), PdfValue::Integer(top)] =
        values.as_slice()
    else {
        return None;
    };
    let width = right.checked_sub(*left)?;
    let height = top.checked_sub(*bottom)?;
    (width > 0 && height > 0).then_some((width, height))
}

fn xobject_dimensions(dictionary: &BTreeMap<Vec<u8>, PdfValue>) -> Option<(i64, i64)> {
    let (Some(PdfValue::Integer(width)), Some(PdfValue::Integer(height))) = (
        dictionary.get(b"Width".as_slice()),
        dictionary.get(b"Height".as_slice()),
    ) else {
        return None;
    };
    (*width > 0 && *height > 0).then_some((*width, *height))
}

fn decrypted_page_visual_inventory(
    document: &lopdf::Document,
    page_id: lopdf::ObjectId,
    limits: &PdfLimits,
) -> PdfPageVisualInventory {
    let mut inventory = PdfPageVisualInventory {
        media_box: decrypted_inherited_page_value(document, page_id, b"MediaBox", limits)
            .and_then(decrypted_box_dimensions),
        ..Default::default()
    };
    let Some(resources) = decrypted_inherited_page_value(document, page_id, b"Resources", limits)
        .and_then(decrypted_dictionary)
    else {
        return inventory;
    };
    let Some(xobjects) = resources
        .get(b"XObject")
        .ok()
        .and_then(|value| decrypted_resolve_value(document, value))
        .and_then(decrypted_dictionary)
    else {
        inventory.xobject_resource_count = Some(0);
        inventory.image_xobject_count = Some(0);
        inventory.form_xobject_count = Some(0);
        return inventory;
    };
    if xobjects.len() > MAX_VISUAL_XOBJECTS_PER_PAGE {
        inventory.xobject_resources_limited = true;
        return inventory;
    }

    let mut image_count = 0_usize;
    let mut form_count = 0_usize;
    for (_, xobject) in xobjects.iter() {
        let Some(xobject) =
            decrypted_resolve_value(document, xobject).and_then(decrypted_stream_or_dictionary)
        else {
            continue;
        };
        if xobject.get(b"Type").and_then(lopdf::Object::as_name).ok() != Some(b"XObject") {
            continue;
        }
        match xobject
            .get(b"Subtype")
            .and_then(lopdf::Object::as_name)
            .ok()
        {
            Some(b"Image") => {
                image_count += 1;
                if let Some(dimensions) = decrypted_xobject_dimensions(xobject) {
                    inventory.image_xobject_dimensions.push(dimensions);
                }
            }
            Some(b"Form") => form_count += 1,
            _ => {}
        }
    }
    inventory.xobject_resource_count = Some(xobjects.len());
    inventory.image_xobject_count = Some(image_count);
    inventory.form_xobject_count = Some(form_count);
    inventory
}

fn decrypted_inherited_page_value<'a>(
    document: &'a lopdf::Document,
    page_id: lopdf::ObjectId,
    key: &[u8],
    limits: &PdfLimits,
) -> Option<&'a lopdf::Object> {
    let mut current = page_id;
    let mut seen = BTreeSet::new();
    for _ in 0..=limits.max_reference_depth {
        if !seen.insert(current) {
            return None;
        }
        let dictionary = document.get_dictionary(current).ok()?;
        if let Ok(value) = dictionary.get(key) {
            return decrypted_resolve_value(document, value);
        }
        current = dictionary
            .get(b"Parent")
            .and_then(lopdf::Object::as_reference)
            .ok()?;
    }
    None
}

fn decrypted_resolve_value<'a>(
    document: &'a lopdf::Document,
    value: &'a lopdf::Object,
) -> Option<&'a lopdf::Object> {
    match value {
        lopdf::Object::Reference(id) => document.get_object(*id).ok(),
        _ => Some(value),
    }
}

fn decrypted_dictionary(value: &lopdf::Object) -> Option<&lopdf::Dictionary> {
    value.as_dict().ok()
}

fn decrypted_stream_or_dictionary(value: &lopdf::Object) -> Option<&lopdf::Dictionary> {
    match value {
        lopdf::Object::Stream(stream) => Some(&stream.dict),
        _ => value.as_dict().ok(),
    }
}

fn decrypted_box_dimensions(value: &lopdf::Object) -> Option<(i64, i64)> {
    let values = value.as_array().ok()?;
    let [left, bottom, right, top] = values.as_slice() else {
        return None;
    };
    let width = right.as_i64().ok()?.checked_sub(left.as_i64().ok()?)?;
    let height = top.as_i64().ok()?.checked_sub(bottom.as_i64().ok()?)?;
    (width > 0 && height > 0).then_some((width, height))
}

fn decrypted_xobject_dimensions(dictionary: &lopdf::Dictionary) -> Option<(i64, i64)> {
    let width = dictionary
        .get(b"Width")
        .and_then(lopdf::Object::as_i64)
        .ok()?;
    let height = dictionary
        .get(b"Height")
        .and_then(lopdf::Object::as_i64)
        .ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

fn parse_font_resources(
    parsed: &ParsedPdf,
    resources: &BTreeMap<Vec<u8>, PdfValue>,
    cmaps: &BTreeMap<ObjectId, ToUnicodeCMap>,
    limits: &PdfLimits,
) -> Result<BTreeMap<Vec<u8>, FontEncoding>, PdfError> {
    let Some(fonts) = resources.get(b"Font".as_slice()) else {
        return Ok(BTreeMap::new());
    };
    let (_, PdfValue::Dictionary(fonts)) = resolve_value(parsed, fonts, limits)? else {
        return Err(PdfError::UnsupportedFont);
    };
    if fonts.len() > limits.max_container_entries_per_object {
        return Err(PdfError::TokenLimit);
    }
    let mut encodings = BTreeMap::new();
    for (resource_name, font) in fonts {
        let (_, PdfValue::Dictionary(font)) = resolve_value(parsed, font, limits)? else {
            return Err(PdfError::UnsupportedFont);
        };
        let encoding = if dictionary_name_is(font, b"Subtype", b"Type0") {
            parse_type0_font(font, cmaps, limits, parsed)?
        } else {
            if !dictionary_name_is(font, b"Type", b"Font")
                || !dictionary_name_is(font, b"Subtype", b"Type1")
                || font.contains_key(b"ToUnicode".as_slice())
            {
                return Err(PdfError::UnsupportedFont);
            }
            let Some(PdfValue::Name(base_font)) = font.get(b"BaseFont".as_slice()) else {
                return Err(PdfError::UnsupportedFont);
            };
            match font.get(b"Encoding".as_slice()) {
                None if is_unmodified_standard_font(font, base_font) => FontEncoding::Standard,
                Some(PdfValue::Name(name))
                    if name == b"StandardEncoding"
                        && is_unmodified_standard_font(font, base_font) =>
                {
                    FontEncoding::Standard
                }
                Some(PdfValue::Name(name))
                    if name == b"WinAnsiEncoding"
                        && !matches!(base_font.as_slice(), b"Symbol" | b"ZapfDingbats") =>
                {
                    FontEncoding::WinAnsi
                }
                _ => return Err(PdfError::UnsupportedFont),
            }
        };
        if encodings.insert(resource_name.clone(), encoding).is_some() {
            return Err(PdfError::Malformed);
        }
    }
    Ok(encodings)
}

/// Collect every distinct ToUnicode CMap in the document, keyed by its stream
/// object id. Parsing them once up front (under the shared decode budget)
/// avoids re-decoding a CMap per page and keeps the total decoded bytes
/// bounded. CMaps that are malformed or fail closed simply are not inserted,
/// so fonts referencing them are rejected later as `UnsupportedFont`.
fn collect_tounicode_cmaps(
    source: &[u8],
    parsed: &ParsedPdf,
    limits: &PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
    decode: &mut DecodeBudget,
) -> Result<BTreeMap<ObjectId, ToUnicodeCMap>, PdfError> {
    let mut cmaps = BTreeMap::new();
    for (id, object) in &parsed.objects {
        let PdfValue::Dictionary(ref dict) = object.value else {
            continue;
        };
        if !dictionary_name_is(dict, b"Type", b"CMap") {
            continue;
        }
        check_cancelled(cancelled)?;
        let decoded = decode.stream(source, parsed, *id, cancelled)?;
        let entries = parse_cmap_entries(decoded, limits)?;
        let code_width = dominant_cmap_code_width(&entries);
        cmaps.insert(
            *id,
            ToUnicodeCMap {
                code_width,
                entries,
            },
        );
    }
    // Reference-driven second pass: the `/Type /CMap` key is optional on CMap
    // stream dictionaries (ISO 32000-1), and real producers omit it. Any
    // stream referenced as a Type0 font's `/ToUnicode` must be attempted as
    // a CMap. Streams that do not parse as CMaps are skipped so the
    // referencing font fails closed as `UnsupportedFont`, exactly as when the
    // CMap is absent.
    for object in parsed.objects.values() {
        let PdfValue::Dictionary(dict) = &object.value else {
            continue;
        };
        if !dictionary_name_is(dict, b"Subtype", b"Type0") {
            continue;
        }
        let Some(tounicode) = dict.get(b"ToUnicode".as_slice()) else {
            continue;
        };
        let (cmap_id, _) = resolve_value(parsed, tounicode, limits)?;
        let Some(cmap_id) = cmap_id else {
            continue;
        };
        if cmaps.contains_key(&cmap_id) {
            continue;
        }
        if parsed
            .objects
            .get(&cmap_id)
            .is_none_or(|object| object.stream.is_none())
        {
            continue;
        }
        check_cancelled(cancelled)?;
        let decoded = decode.stream(source, parsed, cmap_id, cancelled)?;
        let Ok(entries) = parse_cmap_entries(decoded, limits) else {
            // Not a CMap after all; the referencing font fails closed later.
            continue;
        };
        if entries.is_empty() {
            // A stream with no mappings is not accepted as an untagged CMap:
            // every CID would be silently dropped from the page text.
            continue;
        }
        let code_width = dominant_cmap_code_width(&entries);
        cmaps.insert(
            cmap_id,
            ToUnicodeCMap {
                code_width,
                entries,
            },
        );
    }
    Ok(cmaps)
}

/// The dominant source-code byte width across a CMap's entries; defaults to 2
/// (the common Identity-H / CID-2 case) when the CMap is empty.
fn dominant_cmap_code_width(entries: &[(Vec<u8>, String)]) -> usize {
    entries
        .iter()
        .map(|(code, _)| code.len())
        .filter(|len| *len > 1)
        .max()
        .unwrap_or(2)
        .max(2)
}

/// Parse a Type0 (composite/CID) font, decoding its text through the
/// pre-parsed mandatory ToUnicode CMap.
fn parse_type0_font(
    font: &BTreeMap<Vec<u8>, PdfValue>,
    cmaps: &BTreeMap<ObjectId, ToUnicodeCMap>,
    limits: &PdfLimits,
    parsed: &ParsedPdf,
) -> Result<FontEncoding, PdfError> {
    if !dictionary_name_is(font, b"Type", b"Font") {
        return Err(PdfError::UnsupportedFont);
    }
    // A Type0 font must carry a ToUnicode CMap to be decodable.
    let Some(tounicode) = font.get(b"ToUnicode".as_slice()) else {
        return Err(PdfError::UnsupportedFont);
    };
    let (cmap_id, _) = resolve_value(parsed, tounicode, limits)?;
    let Some(cmap_id) = cmap_id else {
        return Err(PdfError::UnsupportedFont);
    };
    let cmap = cmaps
        .get(&cmap_id)
        .ok_or(PdfError::UnsupportedFont)?
        .clone();
    Ok(FontEncoding::ToUnicode(cmap))
}

/// Parse the bounded bytes of a ToUnicode CMap into code → unicode entries.
///
/// Recognizes the conservative `beginbfchar`/`beginbfrange` sections used by
/// common producers. Each `<src> <dst>` mapping is recorded; a `beginbfrange`
/// line `<lo> <hi> <dststart>` maps the contiguous code range to sequential
/// Unicode scalars. Malformed or oversized CMaps fail closed.
fn parse_cmap_entries(
    decoded: &[u8],
    limits: &PdfLimits,
) -> Result<Vec<(Vec<u8>, String)>, PdfError> {
    let mut entries: Vec<(Vec<u8>, String)> = Vec::new();
    let text = std::str::from_utf8(decoded).map_err(|_| PdfError::Malformed)?;
    let mut in_char = false;
    let mut in_range = false;
    for line in text.lines() {
        let line = line.trim();
        // Section markers conventionally carry a count prefix
        // (`69 beginbfchar`), so match the trailing token instead of the line
        // prefix. Mapping lines end in hex tokens and cannot collide.
        let marker = line.rsplit(char::is_whitespace).next().unwrap_or_default();
        if marker == "beginbfchar" {
            in_char = true;
            continue;
        }
        if marker == "endbfchar" {
            in_char = false;
            continue;
        }
        if marker == "beginbfrange" {
            in_range = true;
            continue;
        }
        if marker == "endbfrange" {
            in_range = false;
            continue;
        }
        if line.is_empty() || line.starts_with('%') {
            continue;
        }
        if in_char {
            let (src, dst) = parse_cmap_bfchar_line(line)?;
            entries.push((src, dst));
        } else if in_range {
            let mappings = parse_cmap_bfrange_line(line, limits)?;
            for (src, dst) in mappings {
                entries.push((src, dst));
            }
        }
        if entries.len() > limits.max_container_entries {
            return Err(PdfError::TokenLimit);
        }
    }
    Ok(entries)
}

fn parse_cmap_bfchar_line(line: &str) -> Result<(Vec<u8>, String), PdfError> {
    let trimmed = line.trim();
    let (src, rest) = read_hex_token(trimmed)?;
    let (dst, rest) = read_hex_token(rest.trim())?;
    if !rest.trim().is_empty() {
        return Err(PdfError::Malformed);
    }
    Ok((src, hex_to_unicode(&dst)))
}

fn parse_cmap_bfrange_line(
    line: &str,
    limits: &PdfLimits,
) -> Result<Vec<(Vec<u8>, String)>, PdfError> {
    let trimmed = line.trim();
    let (lo, rest) = read_hex_token(trimmed)?;
    let (hi, rest) = read_hex_token(rest.trim())?;
    // The destination may be a hex string (start scalar) or an array of hex
    // strings (one per code). We only accept the simple scalar start form.
    let (start, rest) = read_hex_token(rest.trim())?;
    if !rest.trim().is_empty() {
        return Err(PdfError::UnsupportedFont);
    }
    let Some(lo_val) = hex_to_u32(&lo) else {
        return Err(PdfError::Malformed);
    };
    let Some(hi_val) = hex_to_u32(&hi) else {
        return Err(PdfError::Malformed);
    };
    let Some(start_val) = hex_to_u32(&start) else {
        return Err(PdfError::Malformed);
    };
    if hi_val < lo_val {
        return Err(PdfError::Malformed);
    }
    // Bound the range expansion against the per-object container ceiling so a
    // single `beginbfrange` line cannot enumerate an unbounded code space.
    let span = hi_val - lo_val;
    if span > (limits.max_container_entries_per_object as u32) {
        return Err(PdfError::TokenLimit);
    }
    // Expanded codes must carry the same byte width as the range's source
    // tokens (a 2-byte Identity-H range stays 2 bytes). Emitting a fixed
    // `u32::to_be_bytes` width would skew the CMap's dominant code width and
    // break every lookup of narrower content codes.
    let width = lo.len().max(hi.len()).clamp(1, 4);
    let mut out = Vec::new();
    for (offset, code) in (lo_val..=hi_val).enumerate() {
        let bytes = code.to_be_bytes();
        let src = bytes[4 - width..].to_vec();
        let scalar = start_val.checked_add(offset as u32);
        let ch = scalar.and_then(char::from_u32);
        if let Some(ch) = ch {
            out.push((src, ch.to_string()));
        }
    }
    Ok(out)
}

fn read_hex_token(input: &str) -> Result<(Vec<u8>, &str), PdfError> {
    let start = input.find('<').ok_or(PdfError::Malformed)?;
    let end = input[start + 1..]
        .find('>')
        .ok_or(PdfError::Malformed)?
        .checked_add(start + 1)
        .ok_or(PdfError::Malformed)?;
    let hex = &input[start + 1..end];
    let bytes = hex_bytes(hex)?;
    Ok((bytes, &input[end + 1..]))
}

fn hex_bytes(hex: &str) -> Result<Vec<u8>, PdfError> {
    if !hex.len().is_multiple_of(2) || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(PdfError::Malformed);
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for i in (0..hex.len()).step_by(2) {
        let byte = u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| PdfError::Malformed)?;
        out.push(byte);
    }
    Ok(out)
}

fn hex_to_u32(bytes: &[u8]) -> Option<u32> {
    let mut value = 0_u32;
    for byte in bytes {
        value = value.checked_shl(8)?.checked_add(u32::from(*byte))?;
    }
    Some(value)
}

fn hex_to_unicode(bytes: &[u8]) -> String {
    // A CMap destination is a Unicode code point (UTF-16BE in the CMap, but
    // producers commonly emit scalar hex). Interpret as a scalar when possible,
    // else UTF-8 of the raw bytes.
    if bytes.len() <= 4 {
        hex_to_u32(bytes)
            .and_then(char::from_u32)
            .map(|c| c.to_string())
            .unwrap_or_default()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn is_unmodified_standard_font(font: &BTreeMap<Vec<u8>, PdfValue>, base_font: &[u8]) -> bool {
    const LATIN_STANDARD_14: [&[u8]; 12] = [
        b"Times-Roman",
        b"Times-Bold",
        b"Times-Italic",
        b"Times-BoldItalic",
        b"Helvetica",
        b"Helvetica-Bold",
        b"Helvetica-Oblique",
        b"Helvetica-BoldOblique",
        b"Courier",
        b"Courier-Bold",
        b"Courier-Oblique",
        b"Courier-BoldOblique",
    ];
    LATIN_STANDARD_14.contains(&base_font)
        && [
            b"FirstChar".as_slice(),
            b"LastChar",
            b"Widths",
            b"FontDescriptor",
        ]
        .iter()
        .all(|key| !font.contains_key(*key))
}

impl DecodeBudget {
    fn new(limits: PdfLimits) -> Self {
        Self {
            limits,
            decoded: BTreeMap::new(),
            decoded_streams: 0,
            total_decoded_bytes: 0,
            total_text_bytes: 0,
        }
    }

    fn stream<'a>(
        &'a mut self,
        source: &[u8],
        parsed: &ParsedPdf,
        id: ObjectId,
        cancelled: Option<&dyn Fn() -> bool>,
    ) -> Result<&'a [u8], PdfError> {
        if !self.decoded.contains_key(&id) {
            check_cancelled(cancelled)?;
            let stream = parsed
                .objects
                .get(&id)
                .and_then(|object| object.stream.as_ref())
                .ok_or(PdfError::InvalidStream)?;
            let encoded = source
                .get(stream.encoded.clone())
                .ok_or(PdfError::InvalidStream)?;
            let remaining = self
                .limits
                .max_total_decoded_bytes
                .checked_sub(self.total_decoded_bytes)
                .ok_or(PdfError::DecompressionLimit)?;
            let decoded =
                decode_stream_bytes(encoded, stream.filter, remaining, &self.limits, cancelled)?;
            self.total_decoded_bytes = self
                .total_decoded_bytes
                .checked_add(decoded.len())
                .ok_or(PdfError::DecompressionLimit)?;
            if self.total_decoded_bytes > self.limits.max_total_decoded_bytes {
                return Err(PdfError::DecompressionLimit);
            }
            self.decoded_streams = self
                .decoded_streams
                .checked_add(1)
                .ok_or(PdfError::DecompressionLimit)?;
            if self.decoded_streams > self.limits.max_streams {
                return Err(PdfError::DecompressionLimit);
            }
            self.decoded.insert(id, decoded);
        }
        self.decoded
            .get(&id)
            .map(Vec::as_slice)
            .ok_or(PdfError::InvalidStream)
    }
}

fn decode_stream_bytes(
    encoded: &[u8],
    filter: StreamFilter,
    remaining: usize,
    limits: &PdfLimits,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<Vec<u8>, PdfError> {
    let per_stream = limits.max_stream_decoded_bytes.min(remaining);
    match filter {
        // Every stream class the extractor consumes (page content, ToUnicode
        // CMaps, object streams, the xref stream) passes through here, so an
        // undecodable filter still fails closed the moment it is needed.
        StreamFilter::Unsupported => Err(PdfError::UnsupportedFilter),
        StreamFilter::Raw => {
            if encoded.len() > per_stream {
                return Err(PdfError::DecompressionLimit);
            }
            let mut output = Vec::new();
            output
                .try_reserve_exact(encoded.len())
                .map_err(|_| PdfError::DecompressionLimit)?;
            for chunk in encoded.chunks(DECODE_CHUNK_BYTES) {
                check_cancelled(cancelled)?;
                output.extend_from_slice(chunk);
            }
            Ok(output)
        }
        StreamFilter::Flate => {
            if encoded.is_empty() {
                return Err(PdfError::InvalidStream);
            }
            let ratio_limit = encoded
                .len()
                .checked_mul(limits.max_expansion_ratio)
                .ok_or(PdfError::ExpansionRatioLimit)?;
            let allowed = per_stream.min(ratio_limit);
            let initial = encoded.len().saturating_mul(4).min(allowed);
            let mut output = Vec::new();
            output
                .try_reserve_exact(initial)
                .map_err(|_| PdfError::DecompressionLimit)?;
            let mut decoder = ZlibDecoder::new(encoded);
            let mut chunk = [0_u8; DECODE_CHUNK_BYTES];
            loop {
                check_cancelled(cancelled)?;
                let read = decoder
                    .read(&mut chunk)
                    .map_err(|_| PdfError::InvalidStream)?;
                if read == 0 {
                    break;
                }
                let next = output
                    .len()
                    .checked_add(read)
                    .ok_or(PdfError::DecompressionLimit)?;
                if next > allowed {
                    return if next > ratio_limit {
                        Err(PdfError::ExpansionRatioLimit)
                    } else {
                        Err(PdfError::DecompressionLimit)
                    };
                }
                output.extend_from_slice(&chunk[..read]);
            }
            if usize::try_from(decoder.total_in()).ok() != Some(encoded.len()) {
                return Err(PdfError::InvalidStream);
            }
            Ok(output)
        }
    }
}

#[derive(Debug, Clone)]
enum ContentOperand {
    String(Vec<u8>),
    Number,
    Name(Vec<u8>),
    Array(Vec<ContentArrayItem>),
}

#[derive(Debug, Clone)]
enum ContentArrayItem {
    String(Vec<u8>),
    Number(Option<i64>),
}

struct PageTextRequest<'a> {
    source: &'a [u8],
    parsed: &'a ParsedPdf,
    content_ids: &'a [ObjectId],
    fonts: &'a BTreeMap<Vec<u8>, FontEncoding>,
    resources: &'a BTreeMap<Vec<u8>, PdfValue>,
    cmaps: &'a BTreeMap<ObjectId, ToUnicodeCMap>,
    limits: &'a PdfLimits,
    cancelled: Option<&'a dyn Fn() -> bool>,
}

#[derive(Clone, Copy)]
struct ContentTextContext<'a> {
    source: &'a [u8],
    parsed: &'a ParsedPdf,
    fonts: &'a BTreeMap<Vec<u8>, FontEncoding>,
    resources: &'a BTreeMap<Vec<u8>, PdfValue>,
    cmaps: &'a BTreeMap<ObjectId, ToUnicodeCMap>,
    limits: &'a PdfLimits,
    cancelled: Option<&'a dyn Fn() -> bool>,
}

struct ContentTextRequest<'a, 'b> {
    content: &'a [u8],
    output: &'b mut String,
    context: ContentTextContext<'a>,
    current_font: &'b mut Option<FontEncoding>,
    decode: &'b mut DecodeBudget,
    budget: &'b mut ContentBudget,
    form_depth: usize,
}

struct FormXObjectContent<'a> {
    content: Vec<u8>,
    resources: &'a BTreeMap<Vec<u8>, PdfValue>,
}

fn extract_page_text(
    request: PageTextRequest<'_>,
    decode: &mut DecodeBudget,
    content_budget: &mut ContentBudget,
) -> Result<String, PdfError> {
    let PageTextRequest {
        source,
        parsed,
        content_ids,
        fonts,
        resources,
        cmaps,
        limits,
        cancelled,
    } = request;
    let mut page = String::new();
    let remaining_total = limits
        .max_total_text_bytes
        .checked_sub(decode.total_text_bytes)
        .ok_or(PdfError::TextLimit)?;
    let mut page_limits = *limits;
    page_limits.max_text_bytes_per_page = limits.max_text_bytes_per_page.min(remaining_total);
    page_limits.max_total_text_bytes = page_limits.max_text_bytes_per_page;
    let mut current_font = None;
    for id in content_ids {
        let content = decode.stream(source, parsed, *id, cancelled)?.to_vec();
        parse_content_text(ContentTextRequest {
            content: &content,
            output: &mut page,
            context: ContentTextContext {
                source,
                parsed,
                fonts,
                resources,
                cmaps,
                limits: &page_limits,
                cancelled,
            },
            current_font: &mut current_font,
            decode,
            budget: content_budget,
            form_depth: 0,
        })?;
    }
    trim_text_in_place(&mut page);
    if page.len() > limits.max_text_bytes_per_page {
        return Err(PdfError::TextLimit);
    }
    let total = decode
        .total_text_bytes
        .checked_add(page.len())
        .ok_or(PdfError::TextLimit)?;
    if total > limits.max_total_text_bytes {
        return Err(PdfError::TextLimit);
    }
    decode.total_text_bytes = total;
    Ok(page)
}

fn parse_content_text(request: ContentTextRequest<'_, '_>) -> Result<(), PdfError> {
    let ContentTextRequest {
        content,
        output,
        context:
            ContentTextContext {
                source,
                parsed,
                fonts,
                resources,
                cmaps,
                limits,
                cancelled,
            },
        current_font,
        decode,
        budget,
        form_depth,
    } = request;
    let mut position = 0_usize;
    let mut array = None::<Vec<ContentArrayItem>>;
    let mut in_text = false;
    let mut dict_depth = 0_usize;
    let mut operands = Vec::new();
    while skip_space_and_comments(content, position, content.len()) < content.len() {
        if budget.tokens.is_multiple_of(1_024) {
            check_cancelled(cancelled)?;
        }
        let (token, end) = lex_token_at_with_byte_limit(
            content,
            position,
            content.len(),
            limits,
            limits.max_text_bytes_per_page.saturating_add(1).max(4_096),
        )?;
        position = end;
        budget.tokens = budget.tokens.checked_add(1).ok_or(PdfError::ContentLimit)?;
        if budget.tokens > limits.max_tokens {
            return Err(PdfError::ContentLimit);
        }
        // Dictionaries in a content stream are operands of marked-content
        // (BDC/BMC) or shading (sh) operators — for example the tagged-PDF
        // sequence `/NonStruct <</MCID 0 >> BDC`. Their entries are
        // structural metadata that text extraction does not need, so a
        // balanced top-level dictionary is consumed without publishing any of
        // its values or executing any operators inside it.
        if dict_depth > 0 {
            match token {
                Token::DictionaryStart => {
                    dict_depth = dict_depth.checked_add(1).ok_or(PdfError::ContentLimit)?;
                    if dict_depth > limits.max_content_nesting {
                        return Err(PdfError::ContentLimit);
                    }
                }
                Token::DictionaryEnd => dict_depth -= 1,
                _ => {}
            }
            continue;
        }
        match token {
            Token::ArrayStart => {
                if array.is_some() {
                    return Err(PdfError::Malformed);
                }
                array = Some(Vec::new());
            }
            Token::ArrayEnd => {
                let values = array.take().ok_or(PdfError::Malformed)?;
                push_content_operand(&mut operands, ContentOperand::Array(values), limits)?;
            }
            Token::DictionaryStart => dict_depth = 1,
            Token::DictionaryEnd => return Err(PdfError::Malformed),
            Token::String(bytes) => {
                if let Some(values) = &mut array {
                    push_content_array_item(values, ContentArrayItem::String(bytes), limits)?;
                } else {
                    push_content_operand(&mut operands, ContentOperand::String(bytes), limits)?;
                }
            }
            Token::Integer(value) => {
                if let Some(values) = &mut array {
                    push_content_array_item(values, ContentArrayItem::Number(Some(value)), limits)?;
                } else {
                    push_content_operand(&mut operands, ContentOperand::Number, limits)?;
                }
            }
            Token::Real => {
                if let Some(values) = &mut array {
                    push_content_array_item(values, ContentArrayItem::Number(None), limits)?;
                } else {
                    push_content_operand(&mut operands, ContentOperand::Number, limits)?;
                }
            }
            Token::Name(name) => {
                if array.is_some() {
                    return Err(PdfError::Malformed);
                }
                push_content_operand(&mut operands, ContentOperand::Name(name), limits)?;
            }
            Token::Keyword(operator) => {
                if array.is_some() {
                    return Err(PdfError::Malformed);
                }
                budget.operations = budget
                    .operations
                    .checked_add(1)
                    .ok_or(PdfError::ContentLimit)?;
                if budget.operations > limits.max_content_operations {
                    return Err(PdfError::ContentLimit);
                }
                match operator.as_slice() {
                    b"BI" | b"ID" | b"EI" => return Err(PdfError::InlineImage),
                    b"BT" => {
                        require_no_operands(&operands)?;
                        if in_text {
                            return Err(PdfError::Malformed);
                        }
                        in_text = true;
                        *current_font = None;
                    }
                    b"ET" => {
                        require_no_operands(&operands)?;
                        if !in_text {
                            return Err(PdfError::Malformed);
                        }
                        append_line_break(output, limits)?;
                        in_text = false;
                    }
                    b"Tf" if in_text => {
                        let [ContentOperand::Name(name), ContentOperand::Number] =
                            operands.as_slice()
                        else {
                            return Err(PdfError::Malformed);
                        };
                        *current_font = Some(
                            fonts
                                .get(name.as_slice())
                                .ok_or(PdfError::UnsupportedFont)?
                                .clone(),
                        );
                    }
                    b"Tj" if in_text => {
                        let [ContentOperand::String(bytes)] = operands.as_slice() else {
                            return Err(PdfError::Malformed);
                        };
                        append_pdf_string(
                            output,
                            bytes,
                            current_font.as_ref().ok_or(PdfError::UnsupportedFont)?,
                            limits,
                        )?;
                    }
                    b"TJ" if in_text => {
                        let [ContentOperand::Array(values)] = operands.as_slice() else {
                            return Err(PdfError::Malformed);
                        };
                        append_text_array(
                            output,
                            values,
                            current_font.as_ref().ok_or(PdfError::UnsupportedFont)?,
                            limits,
                        )?;
                    }
                    b"'" if in_text => {
                        let [ContentOperand::String(bytes)] = operands.as_slice() else {
                            return Err(PdfError::Malformed);
                        };
                        append_line_break(output, limits)?;
                        append_pdf_string(
                            output,
                            bytes,
                            current_font.as_ref().ok_or(PdfError::UnsupportedFont)?,
                            limits,
                        )?;
                    }
                    b"\"" if in_text => {
                        let [ContentOperand::Number, ContentOperand::Number, ContentOperand::String(bytes)] =
                            operands.as_slice()
                        else {
                            return Err(PdfError::Malformed);
                        };
                        append_line_break(output, limits)?;
                        append_pdf_string(
                            output,
                            bytes,
                            current_font.as_ref().ok_or(PdfError::UnsupportedFont)?,
                            limits,
                        )?;
                    }
                    b"T*" if in_text => {
                        require_no_operands(&operands)?;
                        append_line_break(output, limits)?;
                    }
                    b"TD" | b"Td" if in_text => {
                        let [ContentOperand::Number, ContentOperand::Number] = operands.as_slice()
                        else {
                            return Err(PdfError::Malformed);
                        };
                        if operator == b"TD" || !output.is_empty() {
                            append_line_break(output, limits)?;
                        }
                    }
                    b"Do" => {
                        let [ContentOperand::Name(name)] = operands.as_slice() else {
                            return Err(PdfError::Malformed);
                        };
                        if form_depth >= limits.max_reference_depth {
                            return Err(PdfError::ReferenceLimit);
                        }
                        if let Some(FormXObjectContent {
                            content: form_content,
                            resources: form_resources,
                        }) = form_xobject_content(
                            source, parsed, resources, name, limits, decode, cancelled,
                        )? {
                            let form_fonts =
                                parse_font_resources(parsed, form_resources, cmaps, limits)?;
                            let mut form_font = current_font.clone();
                            parse_content_text(ContentTextRequest {
                                content: &form_content,
                                output: &mut *output,
                                context: ContentTextContext {
                                    source,
                                    parsed,
                                    fonts: &form_fonts,
                                    resources: form_resources,
                                    cmaps,
                                    limits,
                                    cancelled,
                                },
                                current_font: &mut form_font,
                                decode: &mut *decode,
                                budget: &mut *budget,
                                form_depth: form_depth + 1,
                            })?;
                        }
                    }
                    _ => {}
                }
                operands.clear();
            }
        }
    }
    if array.is_some() || in_text || dict_depth > 0 || !operands.is_empty() {
        return Err(PdfError::Malformed);
    }
    Ok(())
}

fn form_xobject_content<'a>(
    source: &'a [u8],
    parsed: &'a ParsedPdf,
    resources: &'a BTreeMap<Vec<u8>, PdfValue>,
    name: &[u8],
    limits: &PdfLimits,
    decode: &mut DecodeBudget,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<Option<FormXObjectContent<'a>>, PdfError> {
    let Some(xobjects) = resources.get(b"XObject".as_slice()) else {
        return Ok(None);
    };
    let (_, PdfValue::Dictionary(xobjects)) = resolve_value(parsed, xobjects, limits)? else {
        return Err(PdfError::Malformed);
    };
    let Some(xobject) = xobjects.get(name) else {
        return Err(PdfError::Malformed);
    };
    let (Some(object_id), PdfValue::Dictionary(form)) = resolve_value(parsed, xobject, limits)?
    else {
        return Err(PdfError::Malformed);
    };
    if !dictionary_name_is(form, b"Type", b"XObject")
        || !dictionary_name_is(form, b"Subtype", b"Form")
    {
        return Ok(None);
    }
    let form_resources = if let Some(form_resources) = form.get(b"Resources".as_slice()) {
        let (_, PdfValue::Dictionary(form_resources)) =
            resolve_value(parsed, form_resources, limits)?
        else {
            return Err(PdfError::Malformed);
        };
        form_resources
    } else {
        resources
    };
    Ok(Some(FormXObjectContent {
        content: decode
            .stream(source, parsed, object_id, cancelled)?
            .to_vec(),
        resources: form_resources,
    }))
}

fn require_no_operands(operands: &[ContentOperand]) -> Result<(), PdfError> {
    operands.is_empty().then_some(()).ok_or(PdfError::Malformed)
}

fn push_content_operand(
    operands: &mut Vec<ContentOperand>,
    value: ContentOperand,
    limits: &PdfLimits,
) -> Result<(), PdfError> {
    // Operand retention is per operation, not per page. This prevents a long
    // prefix with no operator from materializing a second operation graph.
    let max_operands = limits.max_content_nesting.saturating_mul(8).max(8);
    if operands.len() >= max_operands {
        return Err(PdfError::ContentLimit);
    }
    operands
        .try_reserve(1)
        .map_err(|_| PdfError::ContentLimit)?;
    operands.push(value);
    Ok(())
}

fn push_content_array_item(
    values: &mut Vec<ContentArrayItem>,
    value: ContentArrayItem,
    limits: &PdfLimits,
) -> Result<(), PdfError> {
    if values.len()
        >= limits
            .max_tokens_per_object
            .min(limits.max_content_operations)
    {
        return Err(PdfError::ContentLimit);
    }
    values.try_reserve(1).map_err(|_| PdfError::ContentLimit)?;
    values.push(value);
    Ok(())
}

fn append_text_array(
    output: &mut String,
    values: &[ContentArrayItem],
    encoding: &FontEncoding,
    limits: &PdfLimits,
) -> Result<(), PdfError> {
    for value in values {
        match value {
            ContentArrayItem::String(bytes) => append_pdf_string(output, bytes, encoding, limits)?,
            ContentArrayItem::Number(Some(value)) if *value < -100 => append_space(output, limits)?,
            ContentArrayItem::Number(_) => {}
        }
    }
    Ok(())
}

fn append_pdf_string(
    output: &mut String,
    bytes: &[u8],
    encoding: &FontEncoding,
    limits: &PdfLimits,
) -> Result<(), PdfError> {
    match encoding {
        FontEncoding::ToUnicode(cmap) => {
            if !bytes.len().is_multiple_of(cmap.code_width) {
                return Err(PdfError::UnsupportedFont);
            }
            for code in bytes.chunks(cmap.code_width) {
                let Some(text) = cmap.translate(code) else {
                    // A CID with no ToUnicode mapping is dropped (no guess);
                    // the fact remains bounded and deterministic.
                    continue;
                };
                for character in text.chars() {
                    append_source_char(output, character, limits)?;
                }
            }
            Ok(())
        }
        FontEncoding::Standard | FontEncoding::WinAnsi => {
            for byte in bytes {
                let character = match encoding {
                    FontEncoding::Standard => standard_encoding_character(*byte)?,
                    _ => win_ansi_character(*byte),
                };
                append_source_char(output, character, limits)?;
            }
            Ok(())
        }
    }
}

fn standard_encoding_character(byte: u8) -> Result<char, PdfError> {
    match byte {
        0x27 => Ok('\u{2019}'),
        0x60 => Ok('\u{2018}'),
        0x20..=0x7e => Ok(char::from(byte)),
        _ => Err(PdfError::UnsupportedFont),
    }
}

fn append_source_char(
    output: &mut String,
    character: char,
    limits: &PdfLimits,
) -> Result<(), PdfError> {
    if character.is_control() {
        return Ok(());
    }
    if character.is_whitespace() {
        append_space(output, limits)
    } else {
        append_char(output, character, limits)
    }
}

fn win_ansi_character(byte: u8) -> char {
    const WINDOWS_1252: [char; 32] = [
        '\u{20ac}', '\u{0081}', '\u{201a}', '\u{0192}', '\u{201e}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02c6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008d}',
        '\u{017d}', '\u{008f}', '\u{0090}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02dc}', '\u{2122}', '\u{0161}', '\u{203a}',
        '\u{0153}', '\u{009d}', '\u{017e}', '\u{0178}',
    ];
    if matches!(byte, 0x7f | 0x81 | 0x8d | 0x8f | 0x90 | 0x9d) {
        '\u{2022}'
    } else if byte == 0xa0 {
        ' '
    } else if byte == 0xad {
        '-'
    } else if (0x80..=0x9f).contains(&byte) {
        WINDOWS_1252[usize::from(byte - 0x80)]
    } else {
        char::from(byte)
    }
}

fn append_char(output: &mut String, character: char, limits: &PdfLimits) -> Result<(), PdfError> {
    let next = output
        .len()
        .checked_add(character.len_utf8())
        .ok_or(PdfError::TextLimit)?;
    if next > limits.max_text_bytes_per_page || next > limits.max_total_text_bytes {
        return Err(PdfError::TextLimit);
    }
    output.push(character);
    Ok(())
}

fn append_space(output: &mut String, limits: &PdfLimits) -> Result<(), PdfError> {
    if !output.ends_with(|character: char| character.is_whitespace()) {
        append_char(output, ' ', limits)?;
    }
    Ok(())
}

fn append_line_break(output: &mut String, limits: &PdfLimits) -> Result<(), PdfError> {
    while output.ends_with(' ') {
        output.pop();
    }
    if !output.is_empty() && !output.ends_with('\n') {
        if output.len() == limits.max_text_bytes_per_page
            || output.len() == limits.max_total_text_bytes
        {
            return Ok(());
        }
        append_char(output, '\n', limits)?;
    }
    Ok(())
}

/// Page text is source material, not display metadata: retain it exactly enough
/// for downstream Markdown rendering while removing only control characters
/// that cannot carry document content.
fn sanitize_decrypted_page_text(text: String) -> String {
    let mut text = text
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect();
    trim_text_in_place(&mut text);
    text
}

fn trim_text_in_place(text: &mut String) {
    let leading = text
        .char_indices()
        .find_map(|(index, character)| (!character.is_whitespace()).then_some(index));
    let Some(leading) = leading else {
        text.clear();
        return;
    };
    let trailing = text
        .char_indices()
        .rev()
        .find_map(|(index, character)| {
            (!character.is_whitespace()).then_some(index + character.len_utf8())
        })
        .unwrap_or(leading);
    text.truncate(trailing);
    if leading != 0 {
        text.drain(..leading);
    }
}

#[derive(Debug, Default)]
struct PdfMetadata {
    values: BTreeMap<String, String>,
    total_bytes: usize,
    decoded_bytes: usize,
}

fn extract_metadata(
    parsed: &ParsedPdf,
    limits: &PdfLimits,
    existing_text_bytes: usize,
) -> Result<PdfMetadata, PdfError> {
    let Some(info) = parsed.trailer.get(b"Info".as_slice()) else {
        return Ok(PdfMetadata::default());
    };
    let (_, PdfValue::Dictionary(info)) = resolve_value(parsed, info, limits)? else {
        return Err(PdfError::Malformed);
    };
    let fields: [(&[u8], &str); 8] = [
        (b"Title", "title"),
        (b"Author", "author"),
        (b"Subject", "subject"),
        (b"Keywords", "keywords"),
        (b"Creator", "creator"),
        (b"Producer", "producer"),
        (b"CreationDate", "creation_date"),
        (b"ModDate", "modification_date"),
    ];
    let mut metadata = PdfMetadata::default();
    for (pdf_key, graph_key) in fields {
        let Some(value) = info.get(pdf_key) else {
            continue;
        };
        let (_, PdfValue::String(bytes)) = resolve_value(parsed, value, limits)? else {
            return Err(PdfError::Malformed);
        };
        let remaining = limits
            .max_metadata_bytes
            .checked_sub(metadata.decoded_bytes)
            .ok_or(PdfError::MetadataLimit)?;
        let decoded = decode_info_text_string(bytes, remaining)?;
        metadata.decoded_bytes = metadata
            .decoded_bytes
            .checked_add(decoded.len())
            .ok_or(PdfError::MetadataLimit)?;
        if metadata.decoded_bytes > limits.max_metadata_bytes {
            return Err(PdfError::MetadataLimit);
        }
        let sanitized = sanitize_metadata_string(decoded);
        metadata.total_bytes = metadata
            .total_bytes
            .checked_add(sanitized.len())
            .ok_or(PdfError::MetadataLimit)?;
        if metadata.total_bytes > limits.max_metadata_bytes
            || existing_text_bytes
                .checked_add(metadata.total_bytes)
                .is_none_or(|total| total > limits.max_total_text_bytes)
        {
            return Err(PdfError::MetadataLimit);
        }
        if !sanitized.is_empty() {
            metadata.values.insert(graph_key.into(), sanitized);
        }
    }
    Ok(metadata)
}

fn decode_info_text_string(bytes: &[u8], max_bytes: usize) -> Result<String, PdfError> {
    let mut output = String::new();
    if bytes.starts_with(&[0xfe, 0xff]) {
        let mut units = bytes[2..].chunks_exact(2);
        for character in char::decode_utf16(
            units
                .by_ref()
                .map(|pair| u16::from_be_bytes([pair[0], pair[1]])),
        ) {
            push_bounded_metadata_char(
                &mut output,
                character.map_err(|_| PdfError::Malformed)?,
                max_bytes,
            )?;
        }
        if !units.remainder().is_empty() {
            return Err(PdfError::Malformed);
        }
        return Ok(output);
    }

    // The supported non-BOM subset is the ASCII intersection of
    // PDFDocEncoding. Reject high bytes rather than silently treating them as
    // WinAnsi glyphs.
    for byte in bytes {
        if *byte > 0x7f {
            return Err(PdfError::Malformed);
        }
        push_bounded_metadata_char(&mut output, char::from(*byte), max_bytes)?;
    }
    Ok(output)
}

fn push_bounded_metadata_char(
    output: &mut String,
    character: char,
    max_bytes: usize,
) -> Result<(), PdfError> {
    let next = output
        .len()
        .checked_add(character.len_utf8())
        .ok_or(PdfError::MetadataLimit)?;
    if next > max_bytes {
        return Err(PdfError::MetadataLimit);
    }
    output.push(character);
    Ok(())
}

fn materialize_extraction(
    path: &Path,
    source_file: &str,
    page_text: Vec<PdfPageMaterial>,
    metadata: PdfMetadata,
    decoded_streams: usize,
    decoded_bytes: usize,
    text_bytes: usize,
) -> Extraction {
    let mut root = document_node(path, source_file);
    root.extra
        .insert("page_count".into(), page_text.len().into());
    root.extra
        .insert("extracted_page_count".into(), page_text.len().into());
    root.extra
        .insert("decoded_stream_count".into(), decoded_streams.into());
    root.extra
        .insert("decompressed_bytes".into(), decoded_bytes.into());
    root.extra.insert("text_bytes".into(), text_bytes.into());
    for (key, value) in metadata.values {
        root.extra.insert(key, value.into());
    }

    let root_id = root.id.clone();
    let mut nodes = Vec::with_capacity(page_text.len() + 1);
    let mut edges = Vec::with_capacity(page_text.len());
    nodes.push(root);
    for PdfPageMaterial {
        number: page_number,
        text,
        outline,
        visual,
    } in page_text
    {
        let ordinal = format!("{page_number:06}");
        let page_id = make_id(&[&root_id, "page", &ordinal]);
        let page_label = format!("Page {page_number}");
        let mut extra = BTreeMap::from([
            ("_origin".into(), "pdf".into()),
            ("page_label".into(), page_label.clone().into()),
            ("page_number".into(), page_number.into()),
            ("text_bytes".into(), text.len().into()),
            ("text".into(), text.into()),
            ("type".into(), "pdf_page".into()),
        ]);
        if let Some(PdfPageOutline { heading, path }) = outline {
            extra.insert("outline_heading".into(), heading.into());
            extra.insert("outline_path".into(), path.into());
        }
        materialize_page_visual_inventory(&mut extra, visual);
        nodes.push(Node {
            id: page_id.clone(),
            label: page_label.clone(),
            file_type: "paper".into(),
            source_file: source_file.into(),
            source_location: None,
            community: None,
            extra,
        });
        edges.push(contains_edge(&root_id, &page_id, source_file));
    }
    Extraction {
        nodes,
        edges,
        hyperedges: Vec::new(),
    }
}

fn materialize_page_visual_inventory(
    extra: &mut BTreeMap<String, Value>,
    visual: PdfPageVisualInventory,
) {
    if let Some((width, height)) = visual.media_box {
        extra.insert("media_box_width".into(), width.into());
        extra.insert("media_box_height".into(), height.into());
        extra.insert("media_box_unit".into(), "default_user_space".into());
    }
    if visual.xobject_resources_limited {
        extra.insert("visual_inventory_status".into(), "partial".into());
        extra.insert(
            "visual_inventory_diagnostic".into(),
            "pdf_xobject_resource_limit".into(),
        );
    } else if let Some(count) = visual.xobject_resource_count {
        extra.insert("xobject_resource_count".into(), count.into());
        extra.insert(
            "image_xobject_count".into(),
            visual.image_xobject_count.unwrap_or_default().into(),
        );
        extra.insert(
            "form_xobject_count".into(),
            visual.form_xobject_count.unwrap_or_default().into(),
        );
        extra.insert("visual_inventory_status".into(), "complete".into());
        if !visual.image_xobject_dimensions.is_empty() {
            extra.insert(
                "image_xobject_dimensions".into(),
                Value::Array(
                    visual
                        .image_xobject_dimensions
                        .into_iter()
                        .map(|(width, height)| json!({ "width": width, "height": height }))
                        .collect(),
                ),
            );
        }
    }
    let captions = {
        let text = extra
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        [
            ("figure_caption_candidates", "figure"),
            ("table_caption_candidates", "table"),
        ]
        .into_iter()
        .map(|(field, keyword)| (field, caption_candidates(text, keyword)))
        .collect::<Vec<_>>()
    };
    for (field, captions) in captions {
        if !captions.is_empty() {
            extra.insert(
                field.into(),
                Value::Array(captions.into_iter().map(Value::String).collect()),
            );
        }
    }
}

fn caption_candidates(text: &str, kind: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            line.char_indices().find_map(|(start, _)| {
                let candidate = line.get(start..)?;
                let prefix = candidate.get(..kind.len())?;
                let starts_at_word_boundary = line[..start]
                    .chars()
                    .next_back()
                    .is_none_or(|character| !character.is_alphanumeric() && character != '_');
                let suffix = candidate.get(kind.len()..)?.trim_start();
                (starts_at_word_boundary
                    && prefix.eq_ignore_ascii_case(kind)
                    && suffix.as_bytes().first().is_some_and(u8::is_ascii_digit)
                    && suffix.chars().any(char::is_alphabetic))
                .then_some(candidate)
            })
        })
        .filter(|line| line.len() <= MAX_CAPTION_CANDIDATE_BYTES)
        .take(MAX_CAPTION_CANDIDATES_PER_KIND)
        .map(str::to_owned)
        .collect()
}

fn document_node(path: &Path, source_file: &str) -> Node {
    let stem = Path::new(source_file)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    let id = make_id(&[&stem]);
    Node {
        id,
        label: path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(source_file)
            .into(),
        file_type: "paper".into(),
        source_file: source_file.into(),
        source_location: None,
        community: None,
        extra: BTreeMap::from([
            ("_origin".into(), "pdf".into()),
            ("format".into(), "pdf".into()),
            ("format_capability".into(), "structural_partial".into()),
            ("parse_status".into(), "complete".into()),
            ("type".into(), "pdf_document".into()),
        ]),
    }
}

fn contains_edge(source: &str, target: &str, source_file: &str) -> Edge {
    Edge {
        source: source.into(),
        target: target.into(),
        relation: "contains".into(),
        confidence: Confidence::Extracted,
        source_file: source_file.into(),
        extra: BTreeMap::from([
            ("_origin".into(), "pdf".into()),
            ("_src".into(), source.into()),
            ("_tgt".into(), target.into()),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn caption_candidates_find_numbered_captions_after_same_line_text() {
        assert_eq!(
            caption_candidates(
                "Functional Description | 21 Figure  3-7. I2C_BMC_BUS6 Topology",
                "figure",
            ),
            ["Figure  3-7. I2C_BMC_BUS6 Topology"]
        );
        assert_eq!(
            caption_candidates(
                "The following figure gives the topology Figure  6-3. UPHY2 PCIe Topology",
                "figure",
            ),
            ["Figure  6-3. UPHY2 PCIe Topology"]
        );
        assert_eq!(
            caption_candidates(
                "PCIe loss budget | 66 Table  6-2. HPM Board UPHY2 PCIe Channel Insertion Loss",
                "table",
            ),
            ["Table  6-2. HPM Board UPHY2 PCIe Channel Insertion Loss"]
        );
    }

    #[test]
    fn caption_candidates_ignore_prose_mentions_and_word_substrings() {
        assert!(caption_candidates(
            "The following figure gives the topology; reconfigure 3 lanes.",
            "figure",
        )
        .is_empty());
        assert!(caption_candidates(
            "The next section is referred to as Segment 1 as per Figure  6-4 .",
            "figure",
        )
        .is_empty());
    }

    #[test]
    fn decrypted_page_text_preserves_complete_source_content_beyond_metadata_limit() {
        let input = format!("Before <value> & after\n{}", "x".repeat(513));

        let output = sanitize_decrypted_page_text(input);

        assert_eq!(output.len(), 536);
        assert!(output.contains("<value> & after"));
        assert!(!output.contains("&lt;value&gt;"));
        assert!(output.contains('\n'));
    }

    fn classic_xref_pdf(objects: &[Vec<u8>]) -> Vec<u8> {
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::with_capacity(objects.len());
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
            pdf.extend_from_slice(object);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    fn jpeg_xobject_pdf() -> (Vec<u8>, Vec<u8>) {
        let jpeg = vec![
            0xff, 0xd8, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x01, 0x00, 0x01, 0x03, 0x01, 0x11,
            0x00, 0x02, 0x11, 0x00, 0x03, 0x11, 0x00, 0xff, 0xda, 0x00, 0x08, 0x01, 0x01, 0x00,
            0x00, 0x3f, 0x00, 0x00, 0xff, 0xd9,
        ];
        let mut image = format!(
            "<< /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
            jpeg.len()
        )
        .into_bytes();
        image.extend_from_slice(&jpeg);
        image.extend_from_slice(b"\nendstream");
        (
            classic_xref_pdf(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
                b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_vec(),
                b"<< /Type /Page /Parent 2 0 R /Resources << /XObject << /Im1 4 0 R >> >> >>"
                    .to_vec(),
                image,
            ]),
            jpeg,
        )
    }

    fn classic_xref_pdf_with_pages(page_count: usize) -> Vec<u8> {
        let mut objects = Vec::with_capacity(page_count + 2);
        objects.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
        let mut pages = format!("<< /Type /Pages /Count {page_count} /Kids [");
        for page in 0..page_count {
            pages.push_str(&format!("{} 0 R ", page + 3));
        }
        pages.push_str("] >>");
        objects.push(pages.into_bytes());
        for _ in 0..page_count {
            objects.push(b"<< /Type /Page /Parent 2 0 R >>".to_vec());
        }

        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::with_capacity(objects.len());
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
            pdf.extend_from_slice(object);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    fn classic_xref_pdf_with_outlines(outline_objects: &[Vec<u8>]) -> Vec<u8> {
        let mut objects = vec![
            b"<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R >>".to_vec(),
            b"<< /Type /Pages /Count 2 /Kids [3 0 R 4 0 R] >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R >>".to_vec(),
        ];
        objects.extend_from_slice(outline_objects);
        classic_xref_pdf(&objects)
    }

    #[test]
    fn form_xobject_text_uses_its_local_font_resources() {
        let form_content = b"BT /F1 12 Tf (Nested form text) Tj ET\n";
        let page_content = b"/Nested Do\n";
        let source = classic_xref_pdf(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /Resources << /XObject << /Nested 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
            format!(
                "<< /Length {} >>\nstream\n{}endstream",
                page_content.len(),
                String::from_utf8_lossy(page_content)
            )
            .into_bytes(),
            format!(
                "<< /Type /XObject /Subtype /Form /Resources << /Font << /F1 6 0 R >> >> /Length {} >>\nstream\n{}endstream",
                form_content.len(),
                String::from_utf8_lossy(form_content)
            )
            .into_bytes(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        ]);

        let extraction = extract_pdf_bytes(
            Path::new("form.pdf"),
            "form.pdf",
            &source,
            PdfLimits::default(),
            None,
        )
        .expect("bounded form XObject text extraction");

        assert_eq!(extraction_text(&extraction), "Nested form text");
    }

    fn page_extra<'a>(extraction: &'a Extraction, page: u64, key: &str) -> Option<&'a Value> {
        extraction
            .nodes
            .iter()
            .find(|node| node.extra.get("page_number") == Some(&page.into()))
            .and_then(|node| node.extra.get(key))
    }

    #[test]
    fn pdf_outline_direct_destination_labels_the_target_page() {
        let source = classic_xref_pdf_with_outlines(&[
            b"<< /Type /Outlines /First 6 0 R /Last 6 0 R >>".to_vec(),
            b"<< /Title (Hardware) /Parent 5 0 R /Dest [3 0 R /Fit] >>".to_vec(),
        ]);

        let extraction = extract_pdf_bytes(
            Path::new("outline.pdf"),
            "outline.pdf",
            &source,
            PdfLimits::default(),
            None,
        )
        .expect("valid outline is optional metadata, never a rejection");

        assert_eq!(
            page_extra(&extraction, 1, "outline_heading"),
            Some(&Value::String("Hardware".into()))
        );
        assert_eq!(
            page_extra(&extraction, 1, "outline_path"),
            Some(&Value::String("Hardware".into()))
        );
        assert_eq!(page_extra(&extraction, 2, "outline_heading"), None);
    }

    #[test]
    fn pdf_outline_inherits_nested_path_for_direct_destination() {
        let source = classic_xref_pdf_with_outlines(&[
            b"<< /Type /Outlines /First 6 0 R /Last 6 0 R >>".to_vec(),
            b"<< /Title (Hardware) /Parent 5 0 R /First 7 0 R /Last 7 0 R >>".to_vec(),
            b"<< /Title (Power) /Parent 6 0 R /Dest [4 0 R /Fit] >>".to_vec(),
        ]);

        let extraction = extract_pdf_bytes(
            Path::new("outline.pdf"),
            "outline.pdf",
            &source,
            PdfLimits::default(),
            None,
        )
        .expect("nested outline metadata is optional");

        assert_eq!(
            page_extra(&extraction, 2, "outline_heading"),
            Some(&Value::String("Power".into()))
        );
        assert_eq!(
            page_extra(&extraction, 2, "outline_path"),
            Some(&Value::String("Hardware / Power".into()))
        );
    }

    #[test]
    fn pdf_outline_malformed_or_cyclic_entries_are_omitted() {
        let malformed = classic_xref_pdf_with_outlines(&[
            b"<< /Type /Outlines /First 6 0 R /Last 6 0 R >>".to_vec(),
            b"<< /Title (Broken) /Parent 5 0 R /Dest [99 0 R /Fit] >>".to_vec(),
        ]);
        let cyclic = classic_xref_pdf_with_outlines(&[
            b"<< /Type /Outlines /First 6 0 R /Last 6 0 R >>".to_vec(),
            b"<< /Title (Loop) /Parent 5 0 R /Next 6 0 R /Dest [3 0 R /Fit] >>".to_vec(),
        ]);

        for source in [&malformed, &cyclic] {
            let extraction = extract_pdf_bytes(
                Path::new("outline.pdf"),
                "outline.pdf",
                source,
                PdfLimits::default(),
                None,
            )
            .expect("bad outline must not suppress otherwise valid page extraction");
            assert_eq!(page_extra(&extraction, 1, "outline_heading"), None);
            assert_eq!(page_extra(&extraction, 1, "outline_path"), None);
        }
    }

    #[test]
    fn page_visual_artifact_preserves_one_bounded_direct_jpeg_xobject() {
        let (source, jpeg) = jpeg_xobject_pdf();

        let artifact = pdf_page_visual_artifact(&source, 1, jpeg.len())
            .expect("direct JPEG XObject becomes a vision artifact");

        assert_eq!(artifact.page, 1);
        assert_eq!(artifact.asset_index, 0);
        assert_eq!(artifact.media_type, "image/jpeg");
        assert_eq!(artifact.bytes, jpeg);
        assert_eq!(
            pdf_page_visual_artifact(&source, 1, artifact.bytes.len().saturating_sub(1)),
            Err(PdfPageVisualArtifactBlocker::ByteLimit)
        );
        assert_eq!(
            pdf_page_visual_artifact(&classic_xref_pdf_with_pages(1), 1, 1024),
            Err(PdfPageVisualArtifactBlocker::NoDirectImage)
        );
    }

    #[test]
    fn retains_numbered_page_locators_through_the_explicit_page_limit() {
        let pages = classic_xref_pdf_with_pages(1_024);
        let allowance_limits = PdfLimits::for_parser_allowance(16 * MIB, pages.len())
            .expect("compact PDF fits the maximum isolated parser allowance");
        assert_eq!(allowance_limits.max_pages, 1_024);
        assert_eq!(allowance_limits.max_facts, 2_049);
        let (extraction, exhausted) = crate::parser_budget::with_plan(
            crate::parser_budget::ParserPlan::for_fact_limit(allowance_limits.max_facts)
                .expect("positive PDF fact limit"),
            || {
                extract_pdf_bytes(
                    Path::new("manual.pdf"),
                    "manual.pdf",
                    &pages,
                    allowance_limits,
                    None,
                )
            },
        );
        let extraction = extraction.expect("1,024-page PDF is within the documented page limit");
        assert!(!exhausted);

        assert_eq!(extraction.nodes.len(), 1_025);
        assert_eq!(extraction.edges.len(), 1_024);
        assert_eq!(
            extraction
                .nodes
                .last()
                .and_then(|node| node.extra.get("page_number")),
            Some(&1_024.into())
        );
        assert_eq!(
            {
                let over_limit = classic_xref_pdf_with_pages(1_025);
                let over_limit_limits = PdfLimits::for_parser_allowance(16 * MIB, over_limit.len())
                    .expect("compact over-limit PDF fits the parser allowance");
                extract_pdf_bytes(
                    Path::new("over-limit.pdf"),
                    "over-limit.pdf",
                    &over_limit,
                    over_limit_limits,
                    None,
                )
            }
            .expect_err("a PDF above the explicit page limit must stay atomic"),
            PdfError::PageLimit
        );
    }

    #[test]
    fn allowance_tightens_page_text_and_fact_ceilings() {
        let limits =
            PdfLimits::for_parser_allowance(16 * MIB, 128 * 1024).expect("bounded allowance");
        assert!(limits.max_total_decoded_bytes <= PdfLimits::default().max_total_decoded_bytes);
        assert!(limits.max_total_text_bytes <= PdfLimits::default().max_total_text_bytes);
        assert!(limits.max_pages.saturating_mul(2).saturating_add(1) <= limits.max_facts);
    }

    #[test]
    fn allowance_admits_multimegabyte_sources_with_scaled_ceilings() {
        // The PDF scratch proof (source x2 plus separately capped decoded and
        // text classes) must admit multi-MiB documents under the 16 MiB
        // profile allowance; the old source x16 estimate capped admissible
        // PDFs at ~1 MiB (issue #132).
        let limits = PdfLimits::for_parser_allowance(16 * MIB, 5 * 1024 * 1024)
            .expect("5 MiB source admitted under the 16 MiB allowance");
        assert!(limits.max_total_decoded_bytes < PdfLimits::default().max_total_decoded_bytes);
        assert!(limits.max_total_decoded_bytes >= 64 * 1024);
        assert!(limits.max_total_text_bytes < PdfLimits::default().max_total_text_bytes);
        assert!(limits.max_total_text_bytes >= 4 * 1024);
        assert!(limits.max_facts >= 3);
        // A source whose x2 scratch plus fixed overhead exceeds the allowance
        // is still rejected.
        assert!(PdfLimits::for_parser_allowance(16 * MIB, 8 * 1024 * 1024).is_none());
        // The static input ceiling rejects regardless of the allowance.
        assert!(PdfLimits::for_parser_allowance(16 * MIB, 20 * 1024 * 1024).is_none());
    }

    #[test]
    fn allowance_can_admit_a_larger_explicitly_budgeted_source() {
        let limits = PdfLimits::for_parser_allowance(64 * MIB, 20 * MIB)
            .expect("20 MiB source admitted under the explicit 64 MiB allowance");
        assert_eq!(limits.max_input_bytes, 20 * MIB);
        assert!(limits.max_total_decoded_bytes <= PdfLimits::default().max_total_decoded_bytes);
        assert!(limits.max_total_text_bytes <= PdfLimits::default().max_total_text_bytes);
    }

    #[test]
    fn page_text_join_is_numeric_and_deterministic() {
        let mut extraction = Extraction::default();
        for (id, page, text) in [("p2", 2, "two"), ("p1", 1, "one")] {
            extraction.nodes.push(Node {
                id: id.into(),
                label: id.into(),
                file_type: "paper".into(),
                source_file: "paper.pdf".into(),
                source_location: None,
                community: None,
                extra: BTreeMap::from([
                    ("page_number".into(), page.into()),
                    ("text".into(), text.into()),
                ]),
            });
        }
        assert_eq!(extraction_text(&extraction), "one\ntwo");
    }

    #[test]
    fn immediate_cancellation_precedes_all_pdf_parsing_and_publication() {
        let cancelled = || true;
        let result = extract_pdf_bytes(
            Path::new("cancelled.pdf"),
            "cancelled.pdf",
            b"%PDF-1.7\n",
            PdfLimits::default(),
            Some(&cancelled),
        );
        assert_eq!(
            result.expect_err("cancel before parse"),
            PdfError::Cancelled
        );
        assert_eq!(PdfError::Cancelled.code(), "cancelled");
    }

    #[test]
    fn cancellation_during_embedded_name_tree_walk_is_preserved() {
        let document = lopdf::Document::new();
        let tree = lopdf::Object::Dictionary(lopdf::Dictionary::from_iter([(
            b"Names".to_vec(),
            lopdf::Object::Array(Vec::new()),
        )]));
        let mut visited = BTreeSet::new();
        let mut entries = Vec::new();
        let cancelled = || true;
        assert_eq!(
            collect_embedded_file_entries(
                &document,
                &tree,
                0,
                &mut visited,
                &mut entries,
                PdfLimits::default(),
                Some(&cancelled),
            )
            .expect_err("cancelled name-tree traversal"),
            PdfAttachmentBlocker::Cancelled
        );
    }

    #[test]
    fn cancellation_after_bounded_attachment_decode_is_preserved() {
        let document = lopdf::Document::new();
        let stream = lopdf::Stream::new(
            lopdf::Dictionary::from_iter([(
                b"Type".to_vec(),
                lopdf::Object::Name(b"EmbeddedFile".to_vec()),
            )]),
            b"bounded payload".to_vec(),
        );
        let embedded =
            lopdf::Dictionary::from_iter([(b"F".to_vec(), lopdf::Object::Stream(stream))]);
        let filespec = lopdf::Object::Dictionary(lopdf::Dictionary::from_iter([
            (b"Type".to_vec(), lopdf::Object::Name(b"Filespec".to_vec())),
            (b"EF".to_vec(), lopdf::Object::Dictionary(embedded)),
        ]));
        let calls = Cell::new(0_usize);
        let cancelled = || {
            let next = calls.get() + 1;
            calls.set(next);
            next >= 2
        };
        let mut total_decoded = 0;
        let mut decode_budget = AttachmentDecodeBudget {
            total_decoded: &mut total_decoded,
            total_limit: PdfLimits::default().max_total_decoded_bytes,
        };
        assert_eq!(
            extract_embedded_file(
                &document,
                0,
                b"attachment.bin",
                &filespec,
                &mut decode_budget,
                PdfLimits::default(),
                Some(&cancelled),
            )
            .expect_err("cancellation after decode"),
            PdfError::Cancelled
        );
        assert_eq!(total_decoded, b"bounded payload".len());
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn cancellation_interrupts_a_wide_direct_container() {
        let mut source = Vec::from(b"[".as_slice());
        for _ in 0..2_048 {
            source.extend_from_slice(b" 0");
        }
        source.extend_from_slice(b" ]");
        let calls = Cell::new(0_usize);
        let cancelled = || {
            let next = calls.get() + 1;
            calls.set(next);
            next >= 2
        };
        let limits = PdfLimits::default();
        let mut counters = ParseCounters::default();
        let mut parser = ValueParser::new(
            &source,
            0,
            source.len(),
            &limits,
            &mut counters,
            Some(&cancelled),
        );
        assert_eq!(
            parser.parse_value(0).expect_err("cancel wide array"),
            PdfError::Cancelled
        );
        assert!(counters.tokens <= 1_024);
    }

    #[test]
    fn cancellation_interrupts_chunked_stream_decode() {
        let encoded = vec![b'x'; DECODE_CHUNK_BYTES * 3];
        let calls = Cell::new(0_usize);
        let cancelled = || {
            let next = calls.get() + 1;
            calls.set(next);
            next >= 2
        };
        assert_eq!(
            decode_stream_bytes(
                &encoded,
                StreamFilter::Raw,
                encoded.len(),
                &PdfLimits::default(),
                Some(&cancelled),
            )
            .expect_err("cancel bounded decode"),
            PdfError::Cancelled
        );
    }

    #[test]
    fn cancellation_interrupts_a_long_marker_scan() {
        let source = vec![b' '; DECODE_CHUNK_BYTES * 3];
        let calls = Cell::new(0_usize);
        let cancelled = || {
            let next = calls.get() + 1;
            calls.set(next);
            next >= 2
        };
        assert_eq!(
            unique_line_keyword_offset(&source, b"startxref", Some(&cancelled))
                .expect_err("cancel marker scan"),
            PdfError::Cancelled
        );
    }

    #[test]
    fn external_file_stream_representations_fail_closed() {
        for key in [b"F".as_slice(), b"FFilter", b"FDecodeParms"] {
            let dictionary =
                BTreeMap::from([(key.to_vec(), PdfValue::String(b"SENTINEL".to_vec()))]);
            assert_eq!(
                stream_filter(&dictionary).expect_err("external stream representation"),
                PdfError::ActiveContent
            );
        }
    }

    #[test]
    fn supported_font_encodings_use_pdf_specific_character_maps() {
        let limits = PdfLimits::default();
        let mut standard = String::new();
        append_pdf_string(
            &mut standard,
            &[0x27, b' ', 0x60],
            &FontEncoding::Standard,
            &limits,
        )
        .expect("bounded StandardEncoding text");
        assert_eq!(standard, "’ ‘");

        let mut win_ansi = String::new();
        append_pdf_string(
            &mut win_ansi,
            &[0x7f, 0x81, 0x8d, 0x8f, 0x90, 0x9d, 0xa0, 0xad],
            &FontEncoding::WinAnsi,
            &limits,
        )
        .expect("bounded WinAnsi text");
        assert_eq!(win_ansi, "•••••• -");
    }
}
