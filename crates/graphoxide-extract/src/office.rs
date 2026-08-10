//! Bounded, byte-only provenance for ZIP-based document packages.
//!
//! This module deliberately implements a conservative structural subset of
//! OOXML, ODF, and EPUB. The caller supplies an already-admitted source byte
//! slice, and the existing container visitor remains the sole ZIP decoder. No
//! package member is written to disk, recursively dispatched, rendered, or
//! interpreted as executable content.

use crate::containers::{
    visit_zip_members_bounded_with_encounter, CompressedMemberAdmission, ContainerLimits,
    ContainerMember, ContainerMemberKind, InspectionDiagnostic, InspectionStatus,
};
use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use quick_xml::{
    events::{BytesCData, BytesRef, BytesStart, BytesText, Event},
    name::{ResolveResult, DEFAULT_MAX_DECLARATIONS_PER_ELEMENT},
    NsReader,
};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    io,
    path::Path,
    rc::Rc,
};
use unicode_normalization::UnicodeNormalization as _;

const MIB: usize = 1024 * 1024;
const FIXED_ALLOWANCE_BYTES: usize = 64 * 1024;
const SOURCE_SCRATCH_MULTIPLIER: usize = 2;
const RETAINED_BYTES_PER_FACT: usize = 2 * 1024;

const NS_WORD_TRANSITIONAL: &[u8] = b"http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const NS_WORD_STRICT: &[u8] = b"http://purl.oclc.org/ooxml/wordprocessingml/main";
const NS_SHEET_TRANSITIONAL: &[u8] = b"http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const NS_SHEET_STRICT: &[u8] = b"http://purl.oclc.org/ooxml/spreadsheetml/main";
const NS_PRESENTATION_TRANSITIONAL: &[u8] =
    b"http://schemas.openxmlformats.org/presentationml/2006/main";
const NS_PRESENTATION_STRICT: &[u8] = b"http://purl.oclc.org/ooxml/presentationml/main";
const NS_DRAWING_TRANSITIONAL: &[u8] = b"http://schemas.openxmlformats.org/drawingml/2006/main";
const NS_DRAWING_STRICT: &[u8] = b"http://purl.oclc.org/ooxml/drawingml/main";
const NS_OFFICE_REL_TRANSITIONAL: &[u8] =
    b"http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const NS_OFFICE_REL_STRICT: &[u8] = b"http://purl.oclc.org/ooxml/officeDocument/relationships";
const NS_PACKAGE_REL_TRANSITIONAL: &[u8] =
    b"http://schemas.openxmlformats.org/package/2006/relationships";
const NS_CONTENT_TYPES_TRANSITIONAL: &[u8] =
    b"http://schemas.openxmlformats.org/package/2006/content-types";
const NS_ODF_OFFICE: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:office:1.0";
const NS_ODF_TEXT: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:text:1.0";
const NS_ODF_TABLE: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:table:1.0";
const NS_ODF_DRAW: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:drawing:1.0";
const NS_ODF_MANIFEST: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:manifest:1.0";
const NS_ODF_SCRIPT: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:script:1.0";
const NS_XLINK: &[u8] = b"http://www.w3.org/1999/xlink";
const NS_EPUB_CONTAINER: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:container";
const NS_OPF: &[u8] = b"http://www.idpf.org/2007/opf";
const NS_XHTML: &[u8] = b"http://www.w3.org/1999/xhtml";
const NS_DC: &[u8] = b"http://purl.org/dc/elements/1.1/";
const NS_XML: &[u8] = b"http://www.w3.org/XML/1998/namespace";

/// One supported ZIP-based document representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OfficeKind {
    Docx,
    Xlsx,
    Pptx,
    Odt,
    Ods,
    Odp,
    Epub,
}

impl OfficeKind {
    pub(crate) fn from_extension(extension: &str) -> Option<Self> {
        if extension.eq_ignore_ascii_case("docx") {
            Some(Self::Docx)
        } else if extension.eq_ignore_ascii_case("xlsx") {
            Some(Self::Xlsx)
        } else if extension.eq_ignore_ascii_case("pptx") {
            Some(Self::Pptx)
        } else if extension.eq_ignore_ascii_case("odt") {
            Some(Self::Odt)
        } else if extension.eq_ignore_ascii_case("ods") {
            Some(Self::Ods)
        } else if extension.eq_ignore_ascii_case("odp") {
            Some(Self::Odp)
        } else if extension.eq_ignore_ascii_case("epub") {
            Some(Self::Epub)
        } else {
            None
        }
    }

    const fn format(self) -> &'static str {
        match self {
            Self::Docx => "docx",
            Self::Xlsx => "xlsx",
            Self::Pptx => "pptx",
            Self::Odt => "odt",
            Self::Ods => "ods",
            Self::Odp => "odp",
            Self::Epub => "epub",
        }
    }

    const fn document_type(self) -> &'static str {
        match self {
            Self::Docx => "docx_document",
            Self::Xlsx => "xlsx_workbook",
            Self::Pptx => "pptx_presentation",
            Self::Odt => "odt_document",
            Self::Ods => "ods_workbook",
            Self::Odp => "odp_presentation",
            Self::Epub => "epub_publication",
        }
    }

    const fn unit_type(self) -> &'static str {
        match self {
            Self::Docx | Self::Odt => "document_section",
            Self::Xlsx | Self::Ods => "workbook_sheet",
            Self::Pptx | Self::Odp => "presentation_slide",
            Self::Epub => "epub_spine_item",
        }
    }

    const fn unit_label(self) -> &'static str {
        match self {
            Self::Docx | Self::Odt => "Section",
            Self::Xlsx | Self::Ods => "Sheet",
            Self::Pptx | Self::Odp => "Slide",
            Self::Epub => "Spine item",
        }
    }

    const fn is_ooxml(self) -> bool {
        matches!(self, Self::Docx | Self::Xlsx | Self::Pptx)
    }

    const fn is_odf(self) -> bool {
        matches!(self, Self::Odt | Self::Ods | Self::Odp)
    }
}

/// Explicit independent ceilings for one document-package parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OfficeLimits {
    pub(crate) max_input_bytes: usize,
    pub(crate) max_members: usize,
    pub(crate) max_central_directory_bytes: usize,
    pub(crate) max_member_decoded_bytes: usize,
    pub(crate) max_total_decoded_bytes: usize,
    pub(crate) max_expansion_ratio: usize,
    pub(crate) max_relationships: usize,
    pub(crate) max_nesting: usize,
    pub(crate) max_xml_events: usize,
    pub(crate) max_xml_event_bytes: usize,
    pub(crate) max_attributes_per_element: usize,
    pub(crate) max_units: usize,
    pub(crate) max_text_bytes_per_unit: usize,
    pub(crate) max_total_text_bytes: usize,
    pub(crate) max_model_bytes: usize,
    pub(crate) max_string_bytes: usize,
    pub(crate) max_table_cells: usize,
    pub(crate) max_shared_strings: usize,
    pub(crate) max_facts: usize,
}

impl Default for OfficeLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 16 * MIB,
            max_members: 1_024,
            max_central_directory_bytes: 4 * MIB,
            max_member_decoded_bytes: 4 * MIB,
            max_total_decoded_bytes: 16 * MIB,
            max_expansion_ratio: 64,
            max_relationships: 2_048,
            max_nesting: 128,
            max_xml_events: 262_144,
            max_xml_event_bytes: 256 * 1024,
            max_attributes_per_element: 256,
            max_units: 1_024,
            max_text_bytes_per_unit: 256 * 1024,
            max_total_text_bytes: 4 * MIB,
            max_model_bytes: 4 * MIB,
            max_string_bytes: 4 * 1024,
            max_table_cells: 131_072,
            max_shared_strings: 65_536,
            max_facts: 4_096,
        }
    }
}

impl OfficeLimits {
    /// Tighten peak decoded, retained-text, and fact ceilings to one parser
    /// allowance. Aggregate decoded bytes remain an independent work limit.
    pub(crate) fn for_parser_allowance(allowance_bytes: usize, source_len: usize) -> Option<Self> {
        let mut limits = Self::default();
        if source_len > limits.max_input_bytes {
            return None;
        }
        let source_scratch = source_len
            .checked_mul(SOURCE_SCRATCH_MULTIPLIER)?
            .checked_add(FIXED_ALLOWANCE_BYTES)?;
        let available = allowance_bytes.checked_sub(source_scratch)?;
        let member = limits.max_member_decoded_bytes.min(available / 2);
        let text = limits.max_total_text_bytes.min(available / 6);
        let model = limits.max_model_bytes.min(available / 6);
        let retained = available
            .checked_sub(member)?
            .checked_sub(text)?
            .checked_sub(model)?;
        let facts = limits.max_facts.min(retained / RETAINED_BYTES_PER_FACT);
        if member < 64 * 1024 || text < 4 * 1024 || model < 64 * 1024 || facts < 3 {
            return None;
        }
        limits.max_member_decoded_bytes = member;
        limits.max_text_bytes_per_unit = limits.max_text_bytes_per_unit.min(text);
        limits.max_total_text_bytes = text;
        limits.max_model_bytes = model;
        limits.max_facts = facts;
        limits.max_units = limits.max_units.min(facts.saturating_sub(1) / 2);
        (limits.max_units > 0).then_some(limits)
    }

    fn valid(self) -> bool {
        self.max_input_bytes > 0
            && self.max_members > 0
            && self.max_central_directory_bytes > 0
            && self.max_member_decoded_bytes > 0
            && self.max_total_decoded_bytes > 0
            && self.max_expansion_ratio > 0
            && self.max_relationships > 0
            && self.max_nesting > 0
            && self.max_xml_events > 0
            && self.max_xml_event_bytes > 0
            && self.max_attributes_per_element > 0
            && self.max_units > 0
            && self.max_text_bytes_per_unit > 0
            && self.max_total_text_bytes > 0
            && self.max_model_bytes > 0
            && self.max_string_bytes > 0
            && self.max_table_cells > 0
            && self.max_shared_strings > 0
            && self.max_facts > 0
    }

    fn container(self) -> ContainerLimits {
        ContainerLimits {
            max_input_bytes: self.max_input_bytes,
            max_recursion_depth: 1,
            max_members: self.max_members,
            max_central_directory_bytes: self.max_central_directory_bytes,
            max_member_uncompressed_bytes: self.max_member_decoded_bytes as u64,
            max_total_uncompressed_bytes: self.max_total_decoded_bytes as u64,
            max_compression_ratio: self.max_expansion_ratio as u64,
            max_member_name_bytes: self.max_string_bytes,
            ..ContainerLimits::default()
        }
    }
}

/// Stable, source-free failure classes for adapter diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum OfficeError {
    #[error("document-package limits are invalid")]
    InvalidLimits,
    #[error("document-package input exceeds its byte ceiling")]
    InputLimit,
    #[error("document-package parsing was cancelled")]
    Cancelled,
    #[error("document-package ZIP representation is malformed")]
    InvalidArchive,
    #[error("document-package archive ceiling was exceeded")]
    ArchiveLimit,
    #[error("document-package encryption is unsupported")]
    Encrypted,
    #[error("document-package compression is unsupported")]
    UnsupportedCompression,
    #[error("document-package representation does not match its format")]
    FormatMismatch,
    #[error("document-package XML is malformed")]
    MalformedXml,
    #[error("document-package XML document types and entities are forbidden")]
    XmlDoctype,
    #[error("document-package XML event ceiling was exceeded")]
    XmlEventLimit,
    #[error("document-package XML attribute ceiling was exceeded")]
    XmlAttributeLimit,
    #[error("document-package XML nesting ceiling was exceeded")]
    XmlNestingLimit,
    #[error("document-package XML namespace is unsupported")]
    UnsupportedNamespace,
    #[error("document-package relationship ceiling was exceeded")]
    RelationshipLimit,
    #[error("document-package relationship is invalid")]
    InvalidRelationship,
    #[error("active or embedded document-package content is unsupported")]
    ActiveContent,
    #[error("a required document-package part is missing")]
    MissingPart,
    #[error("document-package semantic unit ceiling was exceeded")]
    UnitLimit,
    #[error("document-package text ceiling was exceeded")]
    TextLimit,
    #[error("document-package retained-model ceiling was exceeded")]
    ModelLimit,
    #[error("document-package string ceiling was exceeded")]
    StringLimit,
    #[error("document-package table-cell ceiling was exceeded")]
    CellLimit,
    #[error("document-package fact ceiling was exceeded")]
    FactLimit,
}

impl OfficeError {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::InvalidLimits => "office_invalid_limits",
            Self::InputLimit => "office_input_limit",
            Self::Cancelled => "cancelled",
            Self::InvalidArchive => "office_archive_invalid",
            Self::ArchiveLimit => "office_archive_limit",
            Self::Encrypted => "office_encrypted",
            Self::UnsupportedCompression => "office_compression_unsupported",
            Self::FormatMismatch => "office_format_mismatch",
            Self::MalformedXml => "office_xml_malformed",
            Self::XmlDoctype => "office_xml_doctype_forbidden",
            Self::XmlEventLimit => "office_xml_event_limit",
            Self::XmlAttributeLimit => "office_xml_attribute_limit",
            Self::XmlNestingLimit => "office_xml_nesting_limit",
            Self::UnsupportedNamespace => "office_xml_namespace_unsupported",
            Self::RelationshipLimit => "office_relationship_limit",
            Self::InvalidRelationship => "office_relationship_invalid",
            Self::ActiveContent => "office_active_content_unsupported",
            Self::MissingPart => "office_required_part_missing",
            Self::UnitLimit => "office_unit_limit",
            Self::TextLimit => "office_text_limit",
            Self::ModelLimit => "office_model_limit",
            Self::StringLimit => "office_string_limit",
            Self::CellLimit => "office_cell_limit",
            Self::FactLimit => "office_fact_limit",
        }
    }
}

#[derive(Debug, Clone)]
struct UnitDraft {
    label: String,
    part: String,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RelationshipDraft {
    source_part: Option<String>,
    id: String,
    target_part: String,
    kind: &'static str,
}

#[derive(Debug, Clone)]
struct PendingUnit {
    label: String,
    relationship_id: String,
    order_id: String,
}

#[derive(Debug, Clone)]
struct PendingSlide {
    relationship_id: String,
    order_id: String,
}

#[derive(Debug, Clone, Default)]
struct OpfDraft {
    manifest: BTreeMap<String, ManifestItem>,
    manifest_ids: BTreeSet<String>,
    spine: Vec<String>,
    title: Option<String>,
    external_items: usize,
}

#[derive(Debug, Clone)]
struct ManifestItem {
    path: String,
    media_type: String,
}

#[derive(Debug, Clone, Default)]
struct ContentTypesDraft {
    defaults: BTreeMap<String, String>,
    overrides: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
struct OdfManifestDraft {
    root_media_type: Option<String>,
    entries: BTreeSet<String>,
    relationships: Vec<RelationshipDraft>,
}

#[derive(Debug, Default)]
struct PackageScratch {
    content_types: Option<ContentTypesDraft>,
    mimetype: Option<String>,
    relationships: Vec<RelationshipDraft>,
    external_relationships: usize,
    docx_sections: Option<Vec<UnitDraft>>,
    xlsx_sheets: Option<Vec<PendingUnit>>,
    shared_strings: Vec<String>,
    worksheets: BTreeMap<String, String>,
    pptx_slides: Option<Vec<PendingSlide>>,
    slide_text: BTreeMap<String, String>,
    odf_units: Option<Vec<UnitDraft>>,
    odf_manifest: Option<OdfManifestDraft>,
    epub_rootfiles: Vec<String>,
    opfs: BTreeMap<String, OpfDraft>,
    xhtml: BTreeMap<String, XhtmlDraft>,
}

#[derive(Debug, Clone, Default)]
struct XhtmlDraft {
    title: Option<String>,
    text: String,
    links: Vec<String>,
    external_links: usize,
}

struct ParseBudget<'a> {
    limits: OfficeLimits,
    cancelled: Option<&'a dyn Fn() -> bool>,
    xml_events: usize,
    text_bytes: usize,
    model: Rc<ModelLedger>,
    relationships: usize,
    cells: usize,
}

impl<'a> ParseBudget<'a> {
    fn new(
        limits: OfficeLimits,
        cancelled: Option<&'a dyn Fn() -> bool>,
        model: Rc<ModelLedger>,
    ) -> Self {
        Self {
            limits,
            cancelled,
            xml_events: 0,
            text_bytes: 0,
            model,
            relationships: 0,
            cells: 0,
        }
    }

    fn check_cancelled(&self) -> Result<(), OfficeError> {
        if self.cancelled.is_some_and(|check| check()) {
            Err(OfficeError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn event(&mut self, bytes: usize) -> Result<(), OfficeError> {
        self.xml_events = self
            .xml_events
            .checked_add(1)
            .ok_or(OfficeError::XmlEventLimit)?;
        if self.xml_events > self.limits.max_xml_events || bytes > self.limits.max_xml_event_bytes {
            return Err(OfficeError::XmlEventLimit);
        }
        if self.xml_events.is_multiple_of(1_024) {
            self.check_cancelled()?;
        }
        Ok(())
    }

    fn event_with_attributes(
        &mut self,
        reader: &NsReader<&[u8]>,
        event: &Event<'_>,
    ) -> Result<(), OfficeError> {
        self.event(event_bytes(event))?;
        let element = match event {
            Event::Start(element) | Event::Empty(element) => Some(element),
            _ => None,
        };
        if let Some(element) = element {
            let mut attributes = 0_usize;
            for attribute in element.attributes().with_checks(true) {
                let attribute = attribute.map_err(|_| OfficeError::MalformedXml)?;
                if namespace_tag(reader.resolver().resolve_attribute(attribute.key).0)
                    == NamespaceTag::Unknown
                {
                    return Err(OfficeError::UnsupportedNamespace);
                }
                match crate::decode_xml_attribute(attribute.value.as_ref()) {
                    Ok(value) if value.chars().all(is_legal_xml10_character) => {}
                    Err(quick_xml::Error::Escape(
                        quick_xml::escape::EscapeError::UnrecognizedEntity(_, _),
                    )) => return Err(OfficeError::XmlDoctype),
                    _ => return Err(OfficeError::MalformedXml),
                }
                attributes = attributes
                    .checked_add(1)
                    .ok_or(OfficeError::XmlAttributeLimit)?;
                if attributes > self.limits.max_attributes_per_element {
                    return Err(OfficeError::XmlAttributeLimit);
                }
            }
        }
        Ok(())
    }

    fn relationship(&mut self) -> Result<(), OfficeError> {
        self.relationships = self
            .relationships
            .checked_add(1)
            .ok_or(OfficeError::RelationshipLimit)?;
        if self.relationships > self.limits.max_relationships {
            return Err(OfficeError::RelationshipLimit);
        }
        Ok(())
    }

    fn cell(&mut self) -> Result<(), OfficeError> {
        self.cells = self.cells.checked_add(1).ok_or(OfficeError::CellLimit)?;
        if self.cells > self.limits.max_table_cells {
            return Err(OfficeError::CellLimit);
        }
        Ok(())
    }

    fn charge_text(&mut self, bytes: usize) -> Result<(), OfficeError> {
        self.text_bytes = self
            .text_bytes
            .checked_add(bytes)
            .ok_or(OfficeError::TextLimit)?;
        if self.text_bytes > self.limits.max_total_text_bytes {
            return Err(OfficeError::TextLimit);
        }
        Ok(())
    }

    fn retain_model(&self, bytes: usize) -> Result<(), OfficeError> {
        self.model.retain(bytes)
    }

    fn validate_model_estimate(&self, bytes: usize) -> Result<(), OfficeError> {
        self.model.validate_estimate(bytes)
    }
}

#[derive(Debug)]
struct ModelLedger {
    limit: usize,
    retained: Cell<usize>,
    pending: Cell<usize>,
}

impl ModelLedger {
    fn new(limit: usize) -> Rc<Self> {
        Rc::new(Self {
            limit,
            retained: Cell::new(0),
            pending: Cell::new(0),
        })
    }

    fn ensure_total(&self, retained: usize, pending: usize) -> Result<(), OfficeError> {
        if retained
            .checked_add(pending)
            .is_none_or(|total| total > self.limit)
        {
            Err(OfficeError::ModelLimit)
        } else {
            Ok(())
        }
    }

    fn retain(&self, bytes: usize) -> Result<(), OfficeError> {
        let retained = self
            .retained
            .get()
            .checked_add(bytes)
            .ok_or(OfficeError::ModelLimit)?;
        self.ensure_total(retained, self.pending.get())?;
        self.retained.set(retained);
        Ok(())
    }

    fn reserve(self: &Rc<Self>, bytes: usize) -> Result<ModelReservation, OfficeError> {
        let pending = self
            .pending
            .get()
            .checked_add(bytes)
            .ok_or(OfficeError::ModelLimit)?;
        self.ensure_total(self.retained.get(), pending)?;
        self.pending.set(pending);
        Ok(ModelReservation {
            ledger: Rc::clone(self),
            bytes,
        })
    }

    fn validate_estimate(&self, bytes: usize) -> Result<(), OfficeError> {
        if bytes > self.limit || bytes > self.retained.get() {
            Err(OfficeError::ModelLimit)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug)]
struct ModelReservation {
    ledger: Rc<ModelLedger>,
    bytes: usize,
}

impl Drop for ModelReservation {
    fn drop(&mut self) {
        self.ledger
            .pending
            .set(self.ledger.pending.get().saturating_sub(self.bytes));
    }
}

#[derive(Debug)]
struct OfficeDecodePermit<Permit> {
    _outer: Permit,
    _model: ModelReservation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NamespaceTag {
    Unbound,
    Word,
    Sheet,
    Presentation,
    Drawing,
    OfficeRel,
    PackageRel,
    ContentTypes,
    OdfOffice,
    OdfText,
    OdfTable,
    OdfDraw,
    OdfManifest,
    OdfScript,
    Xlink,
    EpubContainer,
    Opf,
    Xhtml,
    Dc,
    Xml,
    Other,
    Unknown,
}

fn namespace_tag(resolved: ResolveResult<'_>) -> NamespaceTag {
    match resolved {
        ResolveResult::Unbound => NamespaceTag::Unbound,
        ResolveResult::Unknown(_) => NamespaceTag::Unknown,
        ResolveResult::Bound(namespace) => match namespace.as_ref() {
            NS_WORD_TRANSITIONAL | NS_WORD_STRICT => NamespaceTag::Word,
            NS_SHEET_TRANSITIONAL | NS_SHEET_STRICT => NamespaceTag::Sheet,
            NS_PRESENTATION_TRANSITIONAL | NS_PRESENTATION_STRICT => NamespaceTag::Presentation,
            NS_DRAWING_TRANSITIONAL | NS_DRAWING_STRICT => NamespaceTag::Drawing,
            NS_OFFICE_REL_TRANSITIONAL | NS_OFFICE_REL_STRICT => NamespaceTag::OfficeRel,
            NS_PACKAGE_REL_TRANSITIONAL => NamespaceTag::PackageRel,
            NS_CONTENT_TYPES_TRANSITIONAL => NamespaceTag::ContentTypes,
            NS_ODF_OFFICE => NamespaceTag::OdfOffice,
            NS_ODF_TEXT => NamespaceTag::OdfText,
            NS_ODF_TABLE => NamespaceTag::OdfTable,
            NS_ODF_DRAW => NamespaceTag::OdfDraw,
            NS_ODF_MANIFEST => NamespaceTag::OdfManifest,
            NS_ODF_SCRIPT => NamespaceTag::OdfScript,
            NS_XLINK => NamespaceTag::Xlink,
            NS_EPUB_CONTAINER => NamespaceTag::EpubContainer,
            NS_OPF => NamespaceTag::Opf,
            NS_XHTML => NamespaceTag::Xhtml,
            NS_DC => NamespaceTag::Dc,
            NS_XML => NamespaceTag::Xml,
            _ => NamespaceTag::Other,
        },
    }
}

fn bounded_xml_reader(bytes: &[u8], limits: OfficeLimits) -> NsReader<&[u8]> {
    let mut reader = NsReader::from_reader(bytes);
    reader.config_mut().trim_text(false);
    reader.resolver_mut().set_max_declarations_per_element(
        limits
            .max_attributes_per_element
            .min(DEFAULT_MAX_DECLARATIONS_PER_ELEMENT),
    );
    reader
}

fn event_bytes(event: &Event<'_>) -> usize {
    match event {
        Event::Start(value) | Event::Empty(value) => value.len(),
        Event::End(value) => value.len(),
        Event::Text(value) => value.len(),
        Event::CData(value) => value.len(),
        Event::Comment(value) => value.len(),
        Event::DocType(value) => value.len(),
        Event::GeneralRef(value) => value.len(),
        Event::Decl(value) => value.len(),
        Event::PI(value) => value.len(),
        Event::Eof => 0,
    }
}

fn local_name<'a>(event: &'a BytesStart<'a>) -> &'a [u8] {
    event.local_name().into_inner()
}

fn validate_depth(depth: usize, limits: OfficeLimits) -> Result<(), OfficeError> {
    if depth > limits.max_nesting {
        Err(OfficeError::XmlNestingLimit)
    } else {
        Ok(())
    }
}

fn decoded_text(event: &BytesText<'_>) -> Result<String, OfficeError> {
    let decoded = event
        .xml10_content()
        .map_err(|_| OfficeError::MalformedXml)?;
    quick_xml::escape::unescape(&decoded)
        .map(|value| value.into_owned())
        .map_err(|_| OfficeError::MalformedXml)
}

fn decoded_cdata(event: &BytesCData<'_>) -> Result<String, OfficeError> {
    event
        .xml10_content()
        .map(|value| value.into_owned())
        .map_err(|_| OfficeError::MalformedXml)
}

fn decoded_reference(reference: &BytesRef<'_>) -> Result<char, OfficeError> {
    let character = if reference.is_char_ref() {
        reference
            .resolve_char_ref()
            .map_err(|_| OfficeError::MalformedXml)?
            .ok_or(OfficeError::MalformedXml)?
    } else {
        match reference
            .decode()
            .map_err(|_| OfficeError::MalformedXml)?
            .as_ref()
        {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "apos" => '\'',
            "quot" => '"',
            _ => return Err(OfficeError::XmlDoctype),
        }
    };
    if !is_legal_xml10_character(character) {
        return Err(OfficeError::MalformedXml);
    }
    Ok(character)
}

fn is_legal_xml10_character(character: char) -> bool {
    matches!(
        character as u32,
        0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF
    )
}

fn append_reference(
    output: &mut String,
    reference: &BytesRef<'_>,
    budget: &mut ParseBudget<'_>,
) -> Result<(), OfficeError> {
    let character = decoded_reference(reference)?;
    let mut encoded = [0_u8; 4];
    append_text(output, character.encode_utf8(&mut encoded), budget)
}

fn validate_ignored_reference(reference: &BytesRef<'_>) -> Result<(), OfficeError> {
    decoded_reference(reference).map(drop)
}

fn decoded_attribute(
    reader: &NsReader<&[u8]>,
    event: &BytesStart<'_>,
    wanted_namespace: NamespaceTag,
    wanted_local: &[u8],
    max_bytes: usize,
) -> Result<Option<String>, OfficeError> {
    let mut found = None;
    for attribute in event.attributes().with_checks(true) {
        let attribute = attribute.map_err(|_| OfficeError::MalformedXml)?;
        let tag = namespace_tag(reader.resolver().resolve_attribute(attribute.key).0);
        if tag == NamespaceTag::Unknown {
            return Err(OfficeError::UnsupportedNamespace);
        }
        if tag != wanted_namespace || attribute.key.local_name().as_ref() != wanted_local {
            continue;
        }
        if found.is_some() {
            return Err(OfficeError::MalformedXml);
        }
        let value = crate::decode_xml_attribute(attribute.value.as_ref())
            .map_err(|_| OfficeError::MalformedXml)?;
        let value = bounded_clean_string(&value, max_bytes)?;
        found = Some(value);
    }
    Ok(found)
}

fn required_attribute(
    reader: &NsReader<&[u8]>,
    event: &BytesStart<'_>,
    namespace: NamespaceTag,
    local: &[u8],
    max_bytes: usize,
) -> Result<String, OfficeError> {
    decoded_attribute(reader, event, namespace, local, max_bytes)?.ok_or(OfficeError::MalformedXml)
}

fn bounded_clean_string(value: &str, max_bytes: usize) -> Result<String, OfficeError> {
    if value.len() > max_bytes
        || value.chars().any(|character| {
            character == '\0' || (character.is_control() && !character.is_whitespace())
        })
    {
        return Err(OfficeError::StringLimit);
    }
    Ok(value.to_owned())
}

fn append_text(
    output: &mut String,
    text: &str,
    budget: &mut ParseBudget<'_>,
) -> Result<(), OfficeError> {
    let cleaned = text
        .chars()
        .filter(|character| !character.is_control() || character.is_whitespace())
        .collect::<String>();
    let next = output
        .len()
        .checked_add(cleaned.len())
        .ok_or(OfficeError::TextLimit)?;
    if next > budget.limits.max_text_bytes_per_unit {
        return Err(OfficeError::TextLimit);
    }
    budget.charge_text(cleaned.len())?;
    output.push_str(&cleaned);
    Ok(())
}

fn append_separator(
    output: &mut String,
    separator: &str,
    budget: &mut ParseBudget<'_>,
) -> Result<(), OfficeError> {
    if output.is_empty() || output.ends_with(separator) {
        return Ok(());
    }
    append_text(output, separator, budget)
}

fn normalize_text(raw: String) -> String {
    let mut output = String::with_capacity(raw.len());
    let mut pending_space = false;
    let mut pending_newline = false;
    for character in raw.chars() {
        if matches!(character, '\n' | '\r') {
            pending_newline = !output.is_empty();
            pending_space = false;
        } else if character.is_whitespace() {
            pending_space = !output.is_empty() && !pending_newline;
        } else {
            if pending_newline {
                output.push('\n');
            } else if pending_space {
                output.push(' ');
            }
            pending_newline = false;
            pending_space = false;
            output.push(character);
        }
    }
    output
}

/// Extract with tree-scoped member and decoded-scratch admission supplied by
/// the runtime. Every opaque decode permit is acquired before Stage 5 opens a
/// member and remains live through the complete XML callback.
#[allow(clippy::too_many_arguments)]
pub(crate) fn extract_office_bytes_with_admission<Permit, Encounter, Admission>(
    path: &Path,
    source_file: &str,
    source: &[u8],
    kind: OfficeKind,
    limits: OfficeLimits,
    cancelled: Option<&dyn Fn() -> bool>,
    mut encounter_member: Encounter,
    mut admit_decode: Admission,
) -> Result<Extraction, OfficeError>
where
    Encounter: FnMut(&ContainerMember) -> bool,
    Admission: FnMut(&ContainerMember) -> Option<Permit>,
{
    if !limits.valid() {
        return Err(OfficeError::InvalidLimits);
    }
    if source.len() > limits.max_input_bytes {
        return Err(OfficeError::InputLimit);
    }
    if cancelled.is_some_and(|check| check()) {
        return Err(OfficeError::Cancelled);
    }

    let parse_error = Cell::new(None);
    let model = ModelLedger::new(limits.max_model_bytes);
    model.retain(
        std::mem::size_of::<PackageScratch>()
            .checked_add(256)
            .ok_or(OfficeError::ModelLimit)?,
    )?;
    let mut scratch = PackageScratch::default();
    let mut budget = ParseBudget::new(limits, cancelled, Rc::clone(&model));
    let inspection = visit_zip_members_bounded_with_encounter(
        source,
        0,
        limits.container(),
        || cancelled.is_some_and(|check| check()),
        |member| {
            if let Some(error) = unsafe_member_error(kind, member) {
                parse_error.set(Some(error));
                false
            } else if !encounter_member(member) {
                parse_error.set(Some(OfficeError::ArchiveLimit));
                false
            } else {
                true
            }
        },
        |member| {
            if parse_error.get().is_some() {
                CompressedMemberAdmission::Stop
            } else if crate::format_adapter::is_sensitive_container_member_path(&member.path) {
                CompressedMemberAdmission::Skip
            } else if relevant_member(kind, &member.path) {
                match admit_decode(member) {
                    Some(outer) => match model_decode_reservation(member, limits)
                        .and_then(|bytes| model.reserve(bytes))
                    {
                        Ok(model) => CompressedMemberAdmission::Dispatch(OfficeDecodePermit {
                            _outer: outer,
                            _model: model,
                        }),
                        Err(error) => {
                            parse_error.set(Some(error));
                            CompressedMemberAdmission::Stop
                        }
                    },
                    None => {
                        parse_error.set(Some(OfficeError::ArchiveLimit));
                        CompressedMemberAdmission::Stop
                    }
                }
            } else {
                CompressedMemberAdmission::Skip
            }
        },
        |member| match parse_member(
            kind,
            member.member.path.as_str(),
            member.bytes,
            &mut scratch,
            &mut budget,
        ) {
            Ok(()) => true,
            Err(error) => {
                parse_error.set(Some(error));
                false
            }
        },
    );
    if let Some(error) = parse_error.get() {
        return Err(error);
    }
    if inspection.status == InspectionStatus::Rejected {
        return Err(map_container_diagnostic(
            inspection.diagnostics.first().copied(),
        ));
    }
    if inspection
        .diagnostics
        .contains(&InspectionDiagnostic::Cancelled)
    {
        return Err(OfficeError::Cancelled);
    }
    if inspection
        .diagnostics
        .contains(&InspectionDiagnostic::NestedDispatchStopped)
    {
        return Err(OfficeError::InvalidArchive);
    }
    budget.check_cancelled()?;

    let members = inspection
        .members
        .iter()
        .filter(|member| member.kind != ContainerMemberKind::Directory)
        .map(|member| member.path.clone())
        .collect::<BTreeSet<_>>();
    let scratch_model = estimate_package_model(&scratch)?;
    budget.validate_model_estimate(scratch_model)?;
    let finalize_reserve = scratch_model
        .checked_add(budget.text_bytes)
        .ok_or(OfficeError::ModelLimit)?;
    let finalize_permit = model.reserve(finalize_reserve)?;
    let finalized = finalize_package(kind, scratch, &members, &mut budget)?;
    let finalized_model = estimate_finalized_model(&finalized)?;
    if finalized_model > model.retained.get() {
        budget.retain_model(finalized_model - model.retained.get())?;
    }
    drop(finalize_permit);
    let materialization_model = ModelLedger::new(limits.max_model_bytes);
    materialization_model.retain(finalized_model)?;
    let materialization_reserve = estimate_materialization_model(&finalized, source_file)?;
    let _materialization_permit = materialization_model.reserve(materialization_reserve)?;
    let FinalizedPackage {
        title,
        units,
        relationships,
        external_relationships,
    } = finalized;
    budget.check_cancelled()?;
    materialize_extraction(MaterializeRequest {
        path,
        source_file,
        kind,
        title,
        units,
        relationships,
        external_relationships,
        member_count: members.len(),
        decompressed_bytes: inspection.decompressed_bytes,
        xml_events: budget.xml_events,
        text_bytes: budget.text_bytes,
        limits,
    })
}

fn estimate_materialization_model(
    package: &FinalizedPackage,
    source_file: &str,
) -> Result<usize, OfficeError> {
    const MATERIALIZATION_STRING_COPIES: usize = 8;
    const MATERIALIZATION_ENTRY_OVERHEAD: usize = 512;

    let mut size = ModelSizer::default();
    size.add(16 * 1024)?;
    if let Some(title) = &package.title {
        size.add(
            title
                .capacity()
                .checked_mul(MATERIALIZATION_STRING_COPIES)
                .ok_or(OfficeError::ModelLimit)?,
        )?;
    }
    for unit in &package.units {
        let strings = source_file
            .len()
            .checked_add(unit.label.capacity())
            .and_then(|bytes| bytes.checked_add(unit.part.capacity()))
            .ok_or(OfficeError::ModelLimit)?;
        size.add(
            strings
                .checked_mul(MATERIALIZATION_STRING_COPIES)
                .and_then(|bytes| bytes.checked_add(MATERIALIZATION_ENTRY_OVERHEAD))
                .ok_or(OfficeError::ModelLimit)?,
        )?;
    }
    for relationship in &package.relationships {
        let strings = source_file
            .len()
            .checked_add(relationship.id.capacity())
            .and_then(|bytes| bytes.checked_add(relationship.target_part.capacity()))
            .and_then(|bytes| {
                bytes.checked_add(
                    relationship
                        .source_part
                        .as_ref()
                        .map_or(0, String::capacity),
                )
            })
            .and_then(|bytes| bytes.checked_add(relationship.kind.len()))
            .ok_or(OfficeError::ModelLimit)?;
        size.add(
            strings
                .checked_mul(MATERIALIZATION_STRING_COPIES)
                .and_then(|bytes| bytes.checked_add(MATERIALIZATION_ENTRY_OVERHEAD))
                .ok_or(OfficeError::ModelLimit)?,
        )?;
    }
    Ok(size.bytes)
}

const MODEL_DECODE_FIXED_BYTES: usize = 4 * 1024;
const MODEL_HEAVY_MEMBER_MULTIPLIER: usize = 8;

fn model_decode_reservation(
    member: &ContainerMember,
    limits: OfficeLimits,
) -> Result<usize, OfficeError> {
    let declared =
        usize::try_from(member.declared_uncompressed_bytes).map_err(|_| OfficeError::ModelLimit)?;
    let transient = declared
        .min(limits.max_xml_event_bytes)
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(declared.min(limits.max_text_bytes_per_unit)))
        .and_then(|bytes| bytes.checked_add(limits.max_string_bytes.saturating_mul(2)))
        .and_then(|bytes| bytes.checked_add(MODEL_DECODE_FIXED_BYTES))
        .ok_or(OfficeError::ModelLimit)?;
    let path = member.path.as_str();
    let model_heavy = path == "[Content_Types].xml"
        || path.ends_with(".rels")
        || matches!(
            path,
            "xl/workbook.xml"
                | "xl/sharedStrings.xml"
                | "ppt/presentation.xml"
                | "META-INF/manifest.xml"
                | "META-INF/container.xml"
        )
        || path.ends_with(".opf");
    if model_heavy {
        Ok(transient.max(
            declared
                .checked_mul(MODEL_HEAVY_MEMBER_MULTIPLIER)
                .and_then(|bytes| bytes.checked_add(MODEL_DECODE_FIXED_BYTES))
                .ok_or(OfficeError::ModelLimit)?,
        ))
    } else {
        Ok(transient)
    }
}

fn unsafe_member_error(kind: OfficeKind, member: &ContainerMember) -> Option<OfficeError> {
    if member.path.chars().any(|character| character.is_control()) {
        return Some(OfficeError::StringLimit);
    }
    let lower = member.path.to_ascii_lowercase();
    if (kind.is_odf() || kind == OfficeKind::Epub)
        && member.path == "mimetype"
        && member.zip.as_ref().is_none_or(|metadata| {
            metadata.local_header_offset != 0
                || metadata.local_data_offset != 38
                || metadata.local_extra_field_bytes != 0
                || !metadata.is_stored
        })
    {
        return Some(OfficeError::FormatMismatch);
    }
    if kind.is_ooxml()
        && (lower.ends_with("vbaproject.bin")
            || lower.ends_with("vbadata.xml")
            || lower.contains("/embeddings/")
            || lower.contains("/activex/")
            || lower.contains("/macrosheets/")
            || ((lower.starts_with("word/")
                || lower.starts_with("xl/")
                || lower.starts_with("ppt/"))
                && lower.ends_with(".bin")))
    {
        return Some(OfficeError::ActiveContent);
    }
    if kind.is_odf()
        && (lower.starts_with("scripts/")
            || lower.starts_with("basic/")
            || lower.contains("/scripts/")
            || lower.contains("/basic/"))
    {
        return Some(OfficeError::ActiveContent);
    }
    if kind == OfficeKind::Epub && lower == "meta-inf/encryption.xml" {
        return Some(OfficeError::Encrypted);
    }
    None
}

fn relevant_member(kind: OfficeKind, path: &str) -> bool {
    if path == "[Content_Types].xml" || path.ends_with(".rels") {
        return kind.is_ooxml();
    }
    match kind {
        OfficeKind::Docx => path == "word/document.xml",
        OfficeKind::Xlsx => {
            matches!(path, "xl/workbook.xml" | "xl/sharedStrings.xml")
                || (path.starts_with("xl/worksheets/") && path.ends_with(".xml"))
        }
        OfficeKind::Pptx => {
            path == "ppt/presentation.xml"
                || (path.starts_with("ppt/slides/") && path.ends_with(".xml"))
        }
        OfficeKind::Odt | OfficeKind::Ods | OfficeKind::Odp => {
            matches!(path, "mimetype" | "content.xml" | "META-INF/manifest.xml")
        }
        OfficeKind::Epub => {
            path == "mimetype"
                || path == "META-INF/container.xml"
                || path.ends_with(".opf")
                || path.ends_with(".xhtml")
                || path.ends_with(".html")
                || path.ends_with(".htm")
        }
    }
}

fn map_container_diagnostic(diagnostic: Option<InspectionDiagnostic>) -> OfficeError {
    match diagnostic {
        Some(InspectionDiagnostic::InvalidLimits) => OfficeError::InvalidLimits,
        Some(InspectionDiagnostic::InputTooLarge) => OfficeError::InputLimit,
        Some(InspectionDiagnostic::EncryptedMember) => OfficeError::Encrypted,
        Some(InspectionDiagnostic::UnsupportedCompression) => OfficeError::UnsupportedCompression,
        Some(InspectionDiagnostic::Cancelled) => OfficeError::Cancelled,
        Some(
            InspectionDiagnostic::MemberLimit
            | InspectionDiagnostic::CentralDirectoryLimit
            | InspectionDiagnostic::MemberNameLimit
            | InspectionDiagnostic::MemberSizeLimit
            | InspectionDiagnostic::TotalSizeLimit
            | InspectionDiagnostic::CompressionRatioLimit
            | InspectionDiagnostic::RecursionLimit,
        ) => OfficeError::ArchiveLimit,
        _ => OfficeError::InvalidArchive,
    }
}

fn parse_member(
    kind: OfficeKind,
    path: &str,
    bytes: &[u8],
    scratch: &mut PackageScratch,
    budget: &mut ParseBudget<'_>,
) -> Result<(), OfficeError> {
    budget.check_cancelled()?;
    if path != "mimetype" {
        validate_single_xml_document(bytes, budget)?;
    }
    if kind.is_ooxml() && path == "[Content_Types].xml" {
        if scratch.content_types.is_some() {
            return Err(OfficeError::MalformedXml);
        }
        let content_types = parse_content_types(bytes, budget)?;
        budget.retain_model(std::mem::size_of::<ContentTypesDraft>().saturating_add(64))?;
        scratch.content_types = Some(content_types);
    } else if kind.is_ooxml() && path.ends_with(".rels") {
        let (relationships, external) = parse_relationships(path, bytes, budget)?;
        scratch.relationships.extend(relationships);
        scratch.external_relationships = scratch
            .external_relationships
            .checked_add(external)
            .ok_or(OfficeError::RelationshipLimit)?;
    } else {
        match kind {
            OfficeKind::Docx if path == "word/document.xml" => {
                if scratch.docx_sections.is_some() {
                    return Err(OfficeError::MalformedXml);
                }
                let units = parse_docx(bytes, path, budget)?;
                scratch.docx_sections = Some(units);
            }
            OfficeKind::Xlsx if path == "xl/workbook.xml" => {
                if scratch.xlsx_sheets.is_some() {
                    return Err(OfficeError::MalformedXml);
                }
                let sheets = parse_xlsx_workbook(bytes, budget)?;
                scratch.xlsx_sheets = Some(sheets);
            }
            OfficeKind::Xlsx if path == "xl/sharedStrings.xml" => {
                if !scratch.shared_strings.is_empty() {
                    return Err(OfficeError::MalformedXml);
                }
                let strings = parse_xlsx_shared_strings(bytes, budget)?;
                scratch.shared_strings = strings;
            }
            OfficeKind::Xlsx if path.starts_with("xl/worksheets/") && path.ends_with(".xml") => {
                let text = parse_xlsx_worksheet(bytes, &scratch.shared_strings, budget)?;
                if scratch.worksheets.contains_key(path) {
                    return Err(OfficeError::MalformedXml);
                }
                let retained_path = path.to_owned();
                let mut size = ModelSizer::default();
                size.add(96)?;
                size.string(&retained_path)?;
                budget.retain_model(size.bytes)?;
                scratch.worksheets.insert(retained_path, text);
            }
            OfficeKind::Pptx if path == "ppt/presentation.xml" => {
                if scratch.pptx_slides.is_some() {
                    return Err(OfficeError::MalformedXml);
                }
                let slides = parse_pptx_presentation(bytes, budget)?;
                scratch.pptx_slides = Some(slides);
            }
            OfficeKind::Pptx if path.starts_with("ppt/slides/") && path.ends_with(".xml") => {
                let text = parse_pptx_slide(bytes, budget)?;
                if scratch.slide_text.contains_key(path) {
                    return Err(OfficeError::MalformedXml);
                }
                let retained_path = path.to_owned();
                let mut size = ModelSizer::default();
                size.add(96)?;
                size.string(&retained_path)?;
                budget.retain_model(size.bytes)?;
                scratch.slide_text.insert(retained_path, text);
            }
            OfficeKind::Odt | OfficeKind::Ods | OfficeKind::Odp if path == "mimetype" => {
                let value = std::str::from_utf8(bytes).map_err(|_| OfficeError::FormatMismatch)?;
                if scratch.mimetype.is_some() {
                    return Err(OfficeError::MalformedXml);
                }
                let mimetype = bounded_clean_string(value, budget.limits.max_string_bytes)?;
                let mut size = ModelSizer::default();
                size.string(&mimetype)?;
                budget.retain_model(size.bytes)?;
                scratch.mimetype = Some(mimetype);
            }
            OfficeKind::Epub if path == "mimetype" => {
                let value = std::str::from_utf8(bytes).map_err(|_| OfficeError::FormatMismatch)?;
                if scratch.mimetype.is_some() {
                    return Err(OfficeError::MalformedXml);
                }
                let mimetype = bounded_clean_string(value, budget.limits.max_string_bytes)?;
                let mut size = ModelSizer::default();
                size.string(&mimetype)?;
                budget.retain_model(size.bytes)?;
                scratch.mimetype = Some(mimetype);
            }
            OfficeKind::Odt | OfficeKind::Ods | OfficeKind::Odp if path == "content.xml" => {
                if scratch.odf_units.is_some() {
                    return Err(OfficeError::MalformedXml);
                }
                let units = parse_odf_content(kind, bytes, path, budget)?;
                scratch.odf_units = Some(units);
            }
            OfficeKind::Odt | OfficeKind::Ods | OfficeKind::Odp
                if path == "META-INF/manifest.xml" =>
            {
                if scratch.odf_manifest.is_some() {
                    return Err(OfficeError::MalformedXml);
                }
                let manifest = parse_odf_manifest(bytes, budget)?;
                budget.retain_model(std::mem::size_of::<OdfManifestDraft>().saturating_add(64))?;
                scratch.odf_manifest = Some(manifest);
            }
            OfficeKind::Epub if path == "META-INF/container.xml" => {
                if !scratch.epub_rootfiles.is_empty() {
                    return Err(OfficeError::MalformedXml);
                }
                let rootfiles = parse_epub_container(bytes, budget)?;
                scratch.epub_rootfiles = rootfiles;
            }
            OfficeKind::Epub if path.ends_with(".opf") => {
                let opf = parse_opf(path, bytes, budget)?;
                if scratch.opfs.contains_key(path) {
                    return Err(OfficeError::MalformedXml);
                }
                let retained_path = path.to_owned();
                let mut size = ModelSizer::default();
                size.add(
                    128_usize
                        .saturating_add(std::mem::size_of::<OpfDraft>())
                        .saturating_add(128),
                )?;
                size.string(&retained_path)?;
                budget.retain_model(size.bytes)?;
                scratch.opfs.insert(retained_path, opf);
            }
            OfficeKind::Epub
                if path.ends_with(".xhtml")
                    || path.ends_with(".html")
                    || path.ends_with(".htm") =>
            {
                let xhtml = parse_xhtml(path, bytes, budget)?;
                if scratch.xhtml.contains_key(path) {
                    return Err(OfficeError::MalformedXml);
                }
                let retained_path = path.to_owned();
                let mut size = ModelSizer::default();
                size.add(
                    96_usize
                        .saturating_add(std::mem::size_of::<XhtmlDraft>())
                        .saturating_add(96),
                )?;
                size.string(&retained_path)?;
                budget.retain_model(size.bytes)?;
                scratch.xhtml.insert(retained_path, xhtml);
            }
            _ => {}
        }
    }
    Ok(())
}

struct FinalizedPackage {
    title: Option<String>,
    units: Vec<UnitDraft>,
    relationships: Vec<RelationshipDraft>,
    external_relationships: usize,
}

#[derive(Debug, Default)]
struct ModelSizer {
    bytes: usize,
}

impl ModelSizer {
    fn add(&mut self, bytes: usize) -> Result<(), OfficeError> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or(OfficeError::ModelLimit)?;
        Ok(())
    }

    fn string(&mut self, value: &String) -> Result<(), OfficeError> {
        self.add(value.capacity().saturating_add(64))
    }

    fn string_bytes(&mut self, bytes: usize) -> Result<(), OfficeError> {
        self.add(bytes.saturating_add(64))
    }
}

fn estimate_content_types_model(
    content_types: &ContentTypesDraft,
    size: &mut ModelSizer,
) -> Result<(), OfficeError> {
    size.add(std::mem::size_of::<ContentTypesDraft>().saturating_add(64))?;
    for (key, value) in content_types
        .defaults
        .iter()
        .chain(content_types.overrides.iter())
    {
        size.add(96)?;
        size.string(key)?;
        size.string(value)?;
    }
    Ok(())
}

fn estimate_relationships_model(
    relationships: &[RelationshipDraft],
    size: &mut ModelSizer,
) -> Result<(), OfficeError> {
    for relationship in relationships {
        estimate_relationship_model(relationship, size)?;
    }
    Ok(())
}

fn estimate_units_model(units: &[UnitDraft], size: &mut ModelSizer) -> Result<(), OfficeError> {
    for unit in units {
        estimate_unit_model(unit, size)?;
    }
    Ok(())
}

fn estimate_pending_units_model(
    units: &[PendingUnit],
    size: &mut ModelSizer,
) -> Result<(), OfficeError> {
    for unit in units {
        size.add(std::mem::size_of::<PendingUnit>().saturating_add(64))?;
        size.string(&unit.label)?;
        size.string(&unit.relationship_id)?;
        size.string(&unit.order_id)?;
    }
    Ok(())
}

fn estimate_pending_slides_model(
    slides: &[PendingSlide],
    size: &mut ModelSizer,
) -> Result<(), OfficeError> {
    for slide in slides {
        size.add(std::mem::size_of::<PendingSlide>().saturating_add(64))?;
        size.string(&slide.relationship_id)?;
        size.string(&slide.order_id)?;
    }
    Ok(())
}

fn estimate_odf_manifest_model(
    manifest: &OdfManifestDraft,
    size: &mut ModelSizer,
) -> Result<(), OfficeError> {
    size.add(std::mem::size_of::<OdfManifestDraft>().saturating_add(64))?;
    if let Some(media_type) = &manifest.root_media_type {
        size.string(media_type)?;
    }
    for entry in &manifest.entries {
        size.add(64)?;
        size.string(entry)?;
    }
    estimate_relationships_model(&manifest.relationships, size)
}

fn estimate_opf_model(opf: &OpfDraft, size: &mut ModelSizer) -> Result<(), OfficeError> {
    size.add(std::mem::size_of::<OpfDraft>().saturating_add(128))?;
    if let Some(title) = &opf.title {
        size.string(title)?;
    }
    for id in &opf.manifest_ids {
        size.add(64)?;
        size.string(id)?;
    }
    for (id, item) in &opf.manifest {
        size.add(std::mem::size_of::<ManifestItem>().saturating_add(96))?;
        size.string(id)?;
        size.string(&item.path)?;
        size.string(&item.media_type)?;
    }
    for spine in &opf.spine {
        size.add(std::mem::size_of::<String>().saturating_add(16))?;
        size.string(spine)?;
    }
    Ok(())
}

fn estimate_xhtml_model(xhtml: &XhtmlDraft, size: &mut ModelSizer) -> Result<(), OfficeError> {
    size.add(std::mem::size_of::<XhtmlDraft>().saturating_add(96))?;
    if let Some(title) = &xhtml.title {
        size.string(title)?;
    }
    for link in &xhtml.links {
        size.add(std::mem::size_of::<String>().saturating_add(16))?;
        size.string(link)?;
    }
    Ok(())
}

fn estimate_relationship_model(
    relationship: &RelationshipDraft,
    size: &mut ModelSizer,
) -> Result<(), OfficeError> {
    size.add(std::mem::size_of::<RelationshipDraft>().saturating_add(64))?;
    if let Some(source) = &relationship.source_part {
        size.string(source)?;
    }
    size.string(&relationship.id)?;
    size.string(&relationship.target_part)
}

fn estimate_unit_model(unit: &UnitDraft, size: &mut ModelSizer) -> Result<(), OfficeError> {
    // Text heap bytes are independently charged by `max_total_text_bytes`.
    size.add(std::mem::size_of::<UnitDraft>().saturating_add(64))?;
    size.string(&unit.label)?;
    size.string(&unit.part)
}

fn retain_generated_unit_model(
    budget: &ParseBudget<'_>,
    label: &str,
    part: &str,
) -> Result<(), OfficeError> {
    let mut size = ModelSizer::default();
    size.add(std::mem::size_of::<UnitDraft>().saturating_add(64))?;
    size.string_bytes(label.len())?;
    size.string_bytes(part.len())?;
    budget.retain_model(size.bytes)
}

fn retain_generated_relationship_model(
    budget: &ParseBudget<'_>,
    source_part: Option<&str>,
    id_bytes: usize,
    target_part: &str,
) -> Result<(), OfficeError> {
    let mut size = ModelSizer::default();
    size.add(std::mem::size_of::<RelationshipDraft>().saturating_add(64))?;
    if let Some(source_part) = source_part {
        size.string_bytes(source_part.len())?;
    }
    size.string_bytes(id_bytes.saturating_mul(2))?;
    size.string_bytes(target_part.len())?;
    budget.retain_model(size.bytes)
}

fn estimate_package_model(scratch: &PackageScratch) -> Result<usize, OfficeError> {
    let mut size = ModelSizer::default();
    size.add(std::mem::size_of::<PackageScratch>())?;
    if let Some(mimetype) = &scratch.mimetype {
        size.string(mimetype)?;
    }
    if let Some(content_types) = &scratch.content_types {
        estimate_content_types_model(content_types, &mut size)?;
    }
    estimate_relationships_model(&scratch.relationships, &mut size)?;
    if let Some(units) = &scratch.docx_sections {
        estimate_units_model(units, &mut size)?;
    }
    if let Some(sheets) = &scratch.xlsx_sheets {
        estimate_pending_units_model(sheets, &mut size)?;
    }
    size.add(
        scratch
            .shared_strings
            .len()
            .saturating_mul(std::mem::size_of::<String>().saturating_add(16)),
    )?;
    for path in scratch.worksheets.keys().chain(scratch.slide_text.keys()) {
        size.add(96)?;
        size.string(path)?;
    }
    if let Some(slides) = &scratch.pptx_slides {
        estimate_pending_slides_model(slides, &mut size)?;
    }
    if let Some(units) = &scratch.odf_units {
        estimate_units_model(units, &mut size)?;
    }
    if let Some(manifest) = &scratch.odf_manifest {
        estimate_odf_manifest_model(manifest, &mut size)?;
    }
    for rootfile in &scratch.epub_rootfiles {
        size.string(rootfile)?;
    }
    for (path, opf) in &scratch.opfs {
        size.add(128)?;
        size.string(path)?;
        estimate_opf_model(opf, &mut size)?;
    }
    for (path, xhtml) in &scratch.xhtml {
        size.add(96)?;
        size.string(path)?;
        estimate_xhtml_model(xhtml, &mut size)?;
    }
    Ok(size.bytes)
}

fn estimate_finalized_model(package: &FinalizedPackage) -> Result<usize, OfficeError> {
    let mut size = ModelSizer::default();
    size.add(std::mem::size_of::<FinalizedPackage>())?;
    if let Some(title) = &package.title {
        size.string(title)?;
    }
    for unit in &package.units {
        estimate_unit_model(unit, &mut size)?;
    }
    for relationship in &package.relationships {
        estimate_relationship_model(relationship, &mut size)?;
    }
    Ok(size.bytes)
}

fn validate_single_xml_document(
    bytes: &[u8],
    budget: &mut ParseBudget<'_>,
) -> Result<(), OfficeError> {
    let mut reader = bounded_xml_reader(bytes, budget.limits);
    let mut depth = 0_usize;
    let mut root_seen = false;
    let mut root_closed = false;
    let mut declaration_seen = false;
    let mut prolog_content_seen = false;
    loop {
        let (resolved, event) = reader
            .read_resolved_event()
            .map_err(|_| OfficeError::MalformedXml)?;
        let namespace = namespace_tag(resolved);
        budget.event_with_attributes(&reader, &event)?;
        if namespace == NamespaceTag::Unknown {
            return Err(OfficeError::UnsupportedNamespace);
        }
        match event {
            Event::Start(_) => {
                if depth == 0 {
                    if root_seen || root_closed {
                        return Err(OfficeError::MalformedXml);
                    }
                    root_seen = true;
                }
                depth = depth.checked_add(1).ok_or(OfficeError::XmlNestingLimit)?;
                validate_depth(depth, budget.limits)?;
            }
            Event::Empty(_) => {
                validate_depth(
                    depth.checked_add(1).ok_or(OfficeError::XmlNestingLimit)?,
                    budget.limits,
                )?;
                if depth == 0 {
                    if root_seen || root_closed {
                        return Err(OfficeError::MalformedXml);
                    }
                    root_seen = true;
                    root_closed = true;
                }
            }
            Event::End(_) => {
                if depth == 0 {
                    return Err(OfficeError::MalformedXml);
                }
                depth -= 1;
                if depth == 0 {
                    root_closed = true;
                }
            }
            Event::Text(text) => {
                if depth == 0 && !decoded_text(&text)?.chars().all(char::is_whitespace) {
                    return Err(OfficeError::MalformedXml);
                }
                if depth == 0 && !root_seen {
                    prolog_content_seen = true;
                }
            }
            Event::CData(text) => {
                if depth == 0 {
                    return Err(OfficeError::MalformedXml);
                }
                decoded_cdata(&text)?;
            }
            Event::GeneralRef(reference) => {
                if depth == 0 {
                    return Err(OfficeError::MalformedXml);
                }
                decoded_reference(&reference)?;
            }
            Event::DocType(_) => return Err(OfficeError::XmlDoctype),
            Event::PI(_) => return Err(OfficeError::ActiveContent),
            Event::Decl(_) => {
                if declaration_seen || prolog_content_seen || root_seen || depth != 0 {
                    return Err(OfficeError::MalformedXml);
                }
                declaration_seen = true;
            }
            Event::Comment(_) if depth == 0 && !root_seen => prolog_content_seen = true,
            Event::Eof => break,
            _ => {}
        }
    }
    if !root_seen || !root_closed || depth != 0 {
        return Err(OfficeError::MalformedXml);
    }
    Ok(())
}

fn parse_content_types(
    bytes: &[u8],
    budget: &mut ParseBudget<'_>,
) -> Result<ContentTypesDraft, OfficeError> {
    let mut reader = bounded_xml_reader(bytes, budget.limits);
    let mut depth = 0_usize;
    let mut root_seen = false;
    let mut draft = ContentTypesDraft::default();
    loop {
        let (resolved, event) = reader
            .read_resolved_event()
            .map_err(|_| OfficeError::MalformedXml)?;
        let namespace = namespace_tag(resolved);
        budget.event_with_attributes(&reader, &event)?;
        if namespace == NamespaceTag::Unknown {
            return Err(OfficeError::UnsupportedNamespace);
        }
        match event {
            Event::Start(element) => {
                depth = depth.checked_add(1).ok_or(OfficeError::XmlNestingLimit)?;
                validate_depth(depth, budget.limits)?;
                if depth == 1 {
                    if namespace != NamespaceTag::ContentTypes || local_name(&element) != b"Types" {
                        return Err(OfficeError::FormatMismatch);
                    }
                    root_seen = true;
                } else if namespace == NamespaceTag::ContentTypes
                    && matches!(local_name(&element), b"Default" | b"Override")
                {
                    return Err(OfficeError::MalformedXml);
                }
            }
            Event::Empty(element) => {
                if depth == 0 {
                    return Err(OfficeError::FormatMismatch);
                }
                if namespace == NamespaceTag::ContentTypes
                    && matches!(local_name(&element), b"Default" | b"Override")
                {
                    if depth != 1 {
                        return Err(OfficeError::MalformedXml);
                    }
                    push_content_type_declaration(&reader, &element, &mut draft, budget)?;
                }
            }
            Event::End(_) => {
                if depth == 0 {
                    return Err(OfficeError::MalformedXml);
                }
                depth -= 1;
            }
            Event::GeneralRef(reference) => validate_ignored_reference(&reference)?,
            Event::DocType(_) => return Err(OfficeError::XmlDoctype),
            Event::PI(_) => return Err(OfficeError::ActiveContent),
            Event::Eof => break,
            _ => {}
        }
    }
    if !root_seen || depth != 0 {
        return Err(OfficeError::MalformedXml);
    }
    Ok(draft)
}

fn push_content_type_declaration(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    draft: &mut ContentTypesDraft,
    budget: &ParseBudget<'_>,
) -> Result<(), OfficeError> {
    let max_bytes = budget.limits.max_string_bytes;
    let Some(content_type) = decoded_attribute(
        reader,
        element,
        NamespaceTag::Unbound,
        b"ContentType",
        max_bytes,
    )?
    else {
        return Err(OfficeError::MalformedXml);
    };
    let lower = content_type.to_ascii_lowercase();
    if lower.contains("macroenabled")
        || lower.contains("vbaproject")
        || lower.contains("oleobject")
        || lower.contains("activex")
    {
        return Err(OfficeError::ActiveContent);
    }
    match local_name(element) {
        b"Override" => {
            let part_name = required_attribute(
                reader,
                element,
                NamespaceTag::Unbound,
                b"PartName",
                max_bytes,
            )?;
            if !part_name.starts_with('/') {
                return Err(OfficeError::MalformedXml);
            }
            let part = resolve_package_target(None, &part_name, false, true, max_bytes)?;
            if draft.overrides.contains_key(&part) {
                return Err(OfficeError::MalformedXml);
            }
            let mut size = ModelSizer::default();
            size.add(96)?;
            size.string(&part)?;
            size.string(&content_type)?;
            budget.retain_model(size.bytes)?;
            draft.overrides.insert(part, content_type);
        }
        b"Default" => {
            let extension = required_attribute(
                reader,
                element,
                NamespaceTag::Unbound,
                b"Extension",
                max_bytes,
            )?
            .to_ascii_lowercase();
            if extension.is_empty()
                || !extension
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            {
                return Err(OfficeError::MalformedXml);
            }
            if draft.defaults.contains_key(&extension) {
                return Err(OfficeError::MalformedXml);
            }
            let mut size = ModelSizer::default();
            size.add(96)?;
            size.string(&extension)?;
            size.string(&content_type)?;
            budget.retain_model(size.bytes)?;
            draft.defaults.insert(extension, content_type);
        }
        _ => return Err(OfficeError::MalformedXml),
    }
    Ok(())
}

fn parse_relationships(
    rels_path: &str,
    bytes: &[u8],
    budget: &mut ParseBudget<'_>,
) -> Result<(Vec<RelationshipDraft>, usize), OfficeError> {
    let source_part = relationship_source_part(rels_path)?;
    let mut reader = bounded_xml_reader(bytes, budget.limits);
    let mut depth = 0_usize;
    let mut root_seen = false;
    let mut relationships = Vec::new();
    let mut external = 0_usize;
    let mut ids = BTreeSet::new();
    loop {
        let (resolved, event) = reader
            .read_resolved_event()
            .map_err(|_| OfficeError::MalformedXml)?;
        let namespace = namespace_tag(resolved);
        budget.event_with_attributes(&reader, &event)?;
        if namespace == NamespaceTag::Unknown {
            return Err(OfficeError::UnsupportedNamespace);
        }
        match event {
            Event::Start(element) => {
                depth = depth.checked_add(1).ok_or(OfficeError::XmlNestingLimit)?;
                validate_depth(depth, budget.limits)?;
                if depth == 1 {
                    if namespace != NamespaceTag::PackageRel
                        || local_name(&element) != b"Relationships"
                    {
                        return Err(OfficeError::FormatMismatch);
                    }
                    root_seen = true;
                } else if namespace == NamespaceTag::PackageRel
                    && local_name(&element) == b"Relationship"
                {
                    return Err(OfficeError::MalformedXml);
                }
            }
            Event::Empty(element) => {
                if depth == 0 {
                    return Err(OfficeError::FormatMismatch);
                }
                if namespace == NamespaceTag::PackageRel && local_name(&element) == b"Relationship"
                {
                    if depth != 1 {
                        return Err(OfficeError::MalformedXml);
                    }
                    parse_relationship_element(
                        &reader,
                        &element,
                        source_part.as_deref(),
                        &mut ids,
                        &mut relationships,
                        &mut external,
                        budget,
                    )?;
                }
            }
            Event::End(_) => {
                if depth == 0 {
                    return Err(OfficeError::MalformedXml);
                }
                depth -= 1;
            }
            Event::GeneralRef(reference) => validate_ignored_reference(&reference)?,
            Event::DocType(_) => return Err(OfficeError::XmlDoctype),
            Event::PI(_) => return Err(OfficeError::ActiveContent),
            Event::Eof => break,
            _ => {}
        }
    }
    if !root_seen || depth != 0 {
        return Err(OfficeError::MalformedXml);
    }
    Ok((relationships, external))
}

#[allow(clippy::too_many_arguments)]
fn parse_relationship_element(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    source_part: Option<&str>,
    ids: &mut BTreeSet<String>,
    relationships: &mut Vec<RelationshipDraft>,
    external: &mut usize,
    budget: &mut ParseBudget<'_>,
) -> Result<(), OfficeError> {
    budget.relationship()?;
    let id = required_attribute(
        reader,
        element,
        NamespaceTag::Unbound,
        b"Id",
        budget.limits.max_string_bytes,
    )?;
    if id.is_empty() || !ids.insert(id.clone()) {
        return Err(OfficeError::InvalidRelationship);
    }
    let target_mode = decoded_attribute(
        reader,
        element,
        NamespaceTag::Unbound,
        b"TargetMode",
        budget.limits.max_string_bytes,
    )?;
    let raw_type = required_attribute(
        reader,
        element,
        NamespaceTag::Unbound,
        b"Type",
        budget.limits.max_string_bytes,
    )?;
    if target_mode.as_deref() == Some("External") {
        // Validate presence and bounded decoding, but deliberately retain no
        // attacker-controlled external target or sentinel.
        let _ = required_attribute(
            reader,
            element,
            NamespaceTag::Unbound,
            b"Target",
            budget.limits.max_string_bytes,
        )?;
        *external = external
            .checked_add(1)
            .ok_or(OfficeError::RelationshipLimit)?;
        return Ok(());
    }
    if target_mode
        .as_deref()
        .is_some_and(|mode| mode != "Internal")
    {
        return Err(OfficeError::InvalidRelationship);
    }
    let raw_target = required_attribute(
        reader,
        element,
        NamespaceTag::Unbound,
        b"Target",
        budget.limits.max_string_bytes,
    )?;
    let target_part = resolve_package_target(
        source_part,
        &raw_target,
        true,
        true,
        budget.limits.max_string_bytes,
    )?;
    let mut size = ModelSizer::default();
    size.add(std::mem::size_of::<RelationshipDraft>().saturating_add(64))?;
    if let Some(source_part) = source_part {
        size.string_bytes(source_part.len())?;
    }
    size.string(&id)?;
    size.string(&target_part)?;
    budget.retain_model(size.bytes)?;
    relationships
        .try_reserve(1)
        .map_err(|_| OfficeError::RelationshipLimit)?;
    relationships.push(RelationshipDraft {
        source_part: source_part.map(str::to_owned),
        id,
        target_part,
        kind: relationship_kind(&raw_type),
    });
    Ok(())
}

fn relationship_source_part(path: &str) -> Result<Option<String>, OfficeError> {
    if path == "_rels/.rels" {
        return Ok(None);
    }
    let Some((directory, file)) = path.rsplit_once("/_rels/") else {
        return Err(OfficeError::InvalidRelationship);
    };
    let Some(file) = file.strip_suffix(".rels") else {
        return Err(OfficeError::InvalidRelationship);
    };
    if directory.is_empty() || file.is_empty() {
        return Err(OfficeError::InvalidRelationship);
    }
    Ok(Some(format!("{directory}/{file}")))
}

fn relationship_kind(raw_type: &str) -> &'static str {
    match raw_type {
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"
        | "http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument" => {
            "office_document"
        }
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet"
        | "http://purl.oclc.org/ooxml/officeDocument/relationships/worksheet" => "worksheet",
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide"
        | "http://purl.oclc.org/ooxml/officeDocument/relationships/slide" => "slide",
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink"
        | "http://purl.oclc.org/ooxml/officeDocument/relationships/hyperlink" => "hyperlink",
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image"
        | "http://purl.oclc.org/ooxml/officeDocument/relationships/image" => "image",
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/header"
        | "http://purl.oclc.org/ooxml/officeDocument/relationships/header" => "header",
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer"
        | "http://purl.oclc.org/ooxml/officeDocument/relationships/footer" => "footer",
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles"
        | "http://purl.oclc.org/ooxml/officeDocument/relationships/styles" => "styles",
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings"
        | "http://purl.oclc.org/ooxml/officeDocument/relationships/sharedStrings" => {
            "shared_strings"
        }
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme"
        | "http://purl.oclc.org/ooxml/officeDocument/relationships/theme" => "theme",
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments"
        | "http://purl.oclc.org/ooxml/officeDocument/relationships/comments" => "comments",
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering"
        | "http://purl.oclc.org/ooxml/officeDocument/relationships/numbering" => "numbering",
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/fontTable"
        | "http://purl.oclc.org/ooxml/officeDocument/relationships/fontTable" => "font_table",
        "http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties"
        | "http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" => {
            "metadata"
        }
        _ => "internal",
    }
}

fn resolve_package_target(
    source_part: Option<&str>,
    raw_target: &str,
    allow_fragment: bool,
    allow_leading_slash: bool,
    max_bytes: usize,
) -> Result<String, OfficeError> {
    if raw_target.is_empty()
        || raw_target.contains('\\')
        || raw_target.contains('!')
        || raw_target.contains('?')
        || raw_target.chars().any(|character| character.is_control())
    {
        return Err(OfficeError::InvalidRelationship);
    }
    let (path, fragment) = raw_target
        .split_once('#')
        .map_or((raw_target, None), |(path, fragment)| {
            (path, Some(fragment))
        });
    if fragment.is_some() && !allow_fragment {
        return Err(OfficeError::InvalidRelationship);
    }
    if fragment.is_some_and(|fragment| fragment.contains('#')) {
        return Err(OfficeError::InvalidRelationship);
    }
    if path.is_empty() {
        let source = source_part.ok_or(OfficeError::InvalidRelationship)?;
        if source.len() > max_bytes {
            return Err(OfficeError::StringLimit);
        }
        return Ok(source.to_owned());
    }
    let path = {
        if path.starts_with('/') && !allow_leading_slash {
            return Err(OfficeError::InvalidRelationship);
        }
        decode_uri_path(path, max_bytes)?
    };
    if path.starts_with("//") || has_uri_scheme(&path) {
        return Err(OfficeError::InvalidRelationship);
    }

    let mut components = Vec::new();
    if !path.starts_with('/')
        && let Some(source) = source_part
        && let Some((directory, _)) = source.rsplit_once('/')
    {
        components.extend(directory.split('/').map(str::to_owned));
    }
    for component in path.trim_start_matches('/').split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if components.pop().is_none() {
                    return Err(OfficeError::InvalidRelationship);
                }
            }
            value => {
                if value.contains(':') || value.chars().any(char::is_control) {
                    return Err(OfficeError::InvalidRelationship);
                }
                components.push(value.to_owned());
            }
        }
    }
    if components.is_empty() {
        return Err(OfficeError::InvalidRelationship);
    }
    let resolved = components.join("/");
    if resolved.len() > max_bytes {
        return Err(OfficeError::StringLimit);
    }
    Ok(resolved)
}

fn decode_uri_path(value: &str, max_bytes: usize) -> Result<String, OfficeError> {
    let mut normalized_components = Vec::new();
    for raw_component in value.split('/') {
        let bytes = raw_component.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut index = 0_usize;
        let mut encoded = false;
        while index < bytes.len() {
            if bytes[index] == b'%' {
                encoded = true;
                let high = hex_nibble(
                    *bytes
                        .get(index + 1)
                        .ok_or(OfficeError::InvalidRelationship)?,
                )
                .ok_or(OfficeError::InvalidRelationship)?;
                let low = hex_nibble(
                    *bytes
                        .get(index + 2)
                        .ok_or(OfficeError::InvalidRelationship)?,
                )
                .ok_or(OfficeError::InvalidRelationship)?;
                let byte = (high << 4) | low;
                if matches!(byte, 0 | b'/' | b'\\' | b'!') || byte.is_ascii_control() {
                    return Err(OfficeError::InvalidRelationship);
                }
                decoded.push(byte);
                index += 3;
            } else {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
        let decoded =
            std::str::from_utf8(&decoded).map_err(|_| OfficeError::InvalidRelationship)?;
        let normalized = decoded.nfc().collect::<String>();
        if encoded && matches!(normalized.as_str(), "." | "..") {
            return Err(OfficeError::InvalidRelationship);
        }
        if normalized.contains(':')
            || normalized.contains('!')
            || normalized.chars().any(char::is_control)
        {
            return Err(OfficeError::InvalidRelationship);
        }
        normalized_components.push(normalized);
    }
    let normalized = normalized_components.join("/");
    if normalized.len() > max_bytes {
        return Err(OfficeError::StringLimit);
    }
    Ok(normalized)
}

fn normalize_zip_member_path(value: &str, max_bytes: usize) -> Result<String, OfficeError> {
    if value.is_empty()
        || value.starts_with('/')
        || value.contains('\\')
        || value.contains('!')
        || value.contains('\0')
    {
        return Err(OfficeError::InvalidRelationship);
    }
    let mut components = Vec::new();
    for raw in value.split('/') {
        if raw.is_empty() {
            continue;
        }
        let component = raw.nfc().collect::<String>();
        if matches!(component.as_str(), "." | "..")
            || component.contains(':')
            || component.contains('!')
            || component.chars().any(char::is_control)
        {
            return Err(OfficeError::InvalidRelationship);
        }
        components.push(component);
    }
    let normalized = components.join("/");
    if normalized.is_empty() || normalized.len() > max_bytes {
        return Err(OfficeError::StringLimit);
    }
    Ok(normalized)
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn has_uri_scheme(value: &str) -> bool {
    let Some((scheme, _)) = value.split_once(':') else {
        return false;
    };
    !scheme.is_empty()
        && scheme.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphabetic()
                || (index > 0 && (byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.')))
        })
}

fn parse_docx(
    bytes: &[u8],
    part: &str,
    budget: &mut ParseBudget<'_>,
) -> Result<Vec<UnitDraft>, OfficeError> {
    let mut reader = bounded_xml_reader(bytes, budget.limits);
    let mut depth = 0_usize;
    let mut root_seen = false;
    let mut body_seen = false;
    let mut body_depth = 0_usize;
    let mut table_depth = 0_usize;
    let mut row_depth = 0_usize;
    let mut cell_depth = 0_usize;
    let mut paragraph_depth = 0_usize;
    let mut paragraph_properties_depth = 0_usize;
    let mut hyperlink_depth = 0_usize;
    let mut run_depth = 0_usize;
    let mut text_depth = 0_usize;
    let mut section_depth = 0_usize;
    let mut section_owned_by_paragraph = false;
    let mut paragraph_section_break = false;
    let mut foreign_depth = 0_usize;
    let mut current = String::new();
    let mut units = Vec::new();
    loop {
        let (resolved, event) = reader
            .read_resolved_event()
            .map_err(|_| OfficeError::MalformedXml)?;
        let namespace = namespace_tag(resolved);
        budget.event_with_attributes(&reader, &event)?;
        if namespace == NamespaceTag::Unknown {
            return Err(OfficeError::UnsupportedNamespace);
        }
        match event {
            Event::Start(element) => {
                depth = depth.checked_add(1).ok_or(OfficeError::XmlNestingLimit)?;
                validate_depth(depth, budget.limits)?;
                if depth == 1 {
                    if namespace != NamespaceTag::Word || local_name(&element) != b"document" {
                        return Err(OfficeError::FormatMismatch);
                    }
                    root_seen = true;
                }
                if namespace == NamespaceTag::Word
                    && matches!(
                        local_name(&element),
                        b"altChunk" | b"object" | b"oleObject" | b"control"
                    )
                {
                    return Err(OfficeError::ActiveContent);
                }
                if body_depth > 0 && namespace != NamespaceTag::Word && foreign_depth == 0 {
                    foreign_depth = depth;
                }
                if namespace == NamespaceTag::Word && foreign_depth == 0 {
                    match local_name(&element) {
                        b"body" => {
                            if depth != 2 || body_seen || body_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            body_seen = true;
                            body_depth = depth;
                        }
                        b"tbl" => {
                            let direct_body = body_depth > 0 && depth == body_depth + 1;
                            let direct_cell = cell_depth > 0 && depth == cell_depth + 1;
                            if (!direct_body && !direct_cell) || table_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            table_depth = depth;
                        }
                        b"tr" => {
                            if table_depth == 0 || depth != table_depth + 1 || row_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            row_depth = depth;
                        }
                        b"tc" => {
                            if row_depth == 0 || depth != row_depth + 1 || cell_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            cell_depth = depth;
                        }
                        b"p" => {
                            let direct_body = body_depth > 0 && depth == body_depth + 1;
                            let direct_cell = cell_depth > 0 && depth == cell_depth + 1;
                            if (!direct_body && !direct_cell) || paragraph_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            paragraph_depth = depth;
                            paragraph_section_break = false;
                        }
                        b"pPr" => {
                            if paragraph_depth == 0
                                || depth != paragraph_depth + 1
                                || paragraph_properties_depth != 0
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            paragraph_properties_depth = depth;
                        }
                        b"hyperlink" => {
                            if paragraph_depth == 0
                                || depth != paragraph_depth + 1
                                || hyperlink_depth != 0
                                || run_depth != 0
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            hyperlink_depth = depth;
                        }
                        b"r" => {
                            let direct_paragraph =
                                paragraph_depth > 0 && depth == paragraph_depth + 1;
                            let direct_hyperlink =
                                hyperlink_depth > 0 && depth == hyperlink_depth + 1;
                            if (!direct_paragraph && !direct_hyperlink) || run_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            run_depth = depth;
                        }
                        b"t" => {
                            if run_depth == 0 || depth != run_depth + 1 || text_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            text_depth = depth;
                        }
                        b"tab" | b"br" | b"cr" => return Err(OfficeError::MalformedXml),
                        b"sectPr" => {
                            let direct_body = body_depth > 0 && depth == body_depth + 1;
                            let direct_properties = paragraph_properties_depth > 0
                                && depth == paragraph_properties_depth + 1;
                            if (!direct_body && !direct_properties) || section_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            section_depth = depth;
                            section_owned_by_paragraph = direct_properties;
                            paragraph_section_break |= direct_properties;
                        }
                        _ => {}
                    }
                }
            }
            Event::Empty(element) => {
                if depth == 0 {
                    return Err(OfficeError::FormatMismatch);
                }
                if namespace == NamespaceTag::Word
                    && matches!(
                        local_name(&element),
                        b"altChunk" | b"object" | b"oleObject" | b"control"
                    )
                {
                    return Err(OfficeError::ActiveContent);
                }
                if namespace == NamespaceTag::Word && foreign_depth == 0 {
                    match local_name(&element) {
                        b"body" => {
                            if depth != 1 || body_seen {
                                return Err(OfficeError::MalformedXml);
                            }
                            body_seen = true;
                        }
                        b"tbl" => {
                            let direct_body = body_depth > 0 && depth == body_depth;
                            let direct_cell = cell_depth > 0 && depth == cell_depth;
                            if (!direct_body && !direct_cell) || table_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                        }
                        b"tr" => {
                            if table_depth == 0 || depth != table_depth || row_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                        }
                        b"tc" => {
                            if row_depth == 0 || depth != row_depth || cell_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                        }
                        b"p" => {
                            let direct_body = body_depth > 0 && depth == body_depth;
                            let direct_cell = cell_depth > 0 && depth == cell_depth;
                            if (!direct_body && !direct_cell) || paragraph_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                        }
                        b"pPr" => {
                            if paragraph_depth == 0
                                || depth != paragraph_depth
                                || paragraph_properties_depth != 0
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                        }
                        b"hyperlink" => {
                            if paragraph_depth == 0
                                || depth != paragraph_depth
                                || hyperlink_depth != 0
                                || run_depth != 0
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                        }
                        b"r" => {
                            let direct_paragraph = paragraph_depth > 0 && depth == paragraph_depth;
                            let direct_hyperlink = hyperlink_depth > 0 && depth == hyperlink_depth;
                            if (!direct_paragraph && !direct_hyperlink) || run_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                        }
                        b"t" => {
                            if run_depth == 0 || depth != run_depth {
                                return Err(OfficeError::MalformedXml);
                            }
                        }
                        b"tab" => {
                            if run_depth == 0 || depth != run_depth {
                                return Err(OfficeError::MalformedXml);
                            }
                            append_text(&mut current, "\t", budget)?;
                        }
                        b"br" | b"cr" => {
                            if run_depth == 0 || depth != run_depth {
                                return Err(OfficeError::MalformedXml);
                            }
                            append_separator(&mut current, "\n", budget)?;
                        }
                        b"sectPr" => {
                            let direct_body = body_depth > 0 && depth == body_depth;
                            let direct_properties = paragraph_properties_depth > 0
                                && depth == paragraph_properties_depth;
                            if !direct_body && !direct_properties {
                                return Err(OfficeError::MalformedXml);
                            }
                            if direct_properties {
                                paragraph_section_break = true;
                            } else {
                                finish_unit(&mut units, &mut current, part, None, budget)?;
                            }
                        }
                        _ => {}
                    }
                }
            }
            Event::Text(text) if text_depth > 0 && foreign_depth == 0 => {
                append_text(&mut current, &decoded_text(&text)?, budget)?;
            }
            Event::CData(text) if text_depth > 0 && foreign_depth == 0 => {
                append_text(&mut current, &decoded_cdata(&text)?, budget)?;
            }
            Event::GeneralRef(reference) if text_depth > 0 && foreign_depth == 0 => {
                append_reference(&mut current, &reference, budget)?;
            }
            Event::GeneralRef(reference) => validate_ignored_reference(&reference)?,
            Event::End(element) => {
                if depth == 0 {
                    return Err(OfficeError::MalformedXml);
                }
                if namespace == NamespaceTag::Word && foreign_depth == 0 {
                    match element.local_name().as_ref() {
                        b"t" => {
                            if text_depth != depth {
                                return Err(OfficeError::MalformedXml);
                            }
                            text_depth = 0;
                        }
                        b"r" => {
                            if run_depth != depth || text_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            run_depth = 0;
                        }
                        b"hyperlink" => {
                            if hyperlink_depth != depth || run_depth != 0 || text_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            hyperlink_depth = 0;
                        }
                        b"pPr" => {
                            if paragraph_properties_depth != depth || section_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            paragraph_properties_depth = 0
                        }
                        b"sectPr" => {
                            if section_depth != depth {
                                return Err(OfficeError::MalformedXml);
                            }
                            if !section_owned_by_paragraph {
                                finish_unit(&mut units, &mut current, part, None, budget)?;
                            }
                            section_depth = 0;
                            section_owned_by_paragraph = false;
                        }
                        b"p" => {
                            if paragraph_depth != depth
                                || run_depth != 0
                                || hyperlink_depth != 0
                                || paragraph_properties_depth != 0
                                || section_depth != 0
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            append_separator(&mut current, "\n", budget)?;
                            if paragraph_section_break {
                                finish_unit(&mut units, &mut current, part, None, budget)?;
                            }
                            paragraph_section_break = false;
                            paragraph_depth = 0;
                        }
                        b"tc" => {
                            if cell_depth != depth || paragraph_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            append_text(&mut current, "\t", budget)?;
                            cell_depth = 0;
                        }
                        b"tr" => {
                            if row_depth != depth || cell_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            append_separator(&mut current, "\n", budget)?;
                            row_depth = 0;
                        }
                        b"tbl" => {
                            if table_depth != depth || row_depth != 0 || cell_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            table_depth = 0;
                        }
                        b"body" => {
                            if body_depth != depth
                                || table_depth != 0
                                || row_depth != 0
                                || cell_depth != 0
                                || paragraph_depth != 0
                                || paragraph_properties_depth != 0
                                || hyperlink_depth != 0
                                || run_depth != 0
                                || text_depth != 0
                                || section_depth != 0
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            body_depth = 0;
                        }
                        b"tab" | b"br" | b"cr" => return Err(OfficeError::MalformedXml),
                        _ => {}
                    }
                }
                if foreign_depth == depth {
                    foreign_depth = 0;
                }
                depth -= 1;
            }
            Event::DocType(_) => return Err(OfficeError::XmlDoctype),
            Event::PI(_) => return Err(OfficeError::ActiveContent),
            Event::Eof => break,
            _ => {}
        }
    }
    if !root_seen
        || !body_seen
        || depth != 0
        || body_depth != 0
        || table_depth != 0
        || row_depth != 0
        || cell_depth != 0
        || paragraph_depth != 0
        || paragraph_properties_depth != 0
        || hyperlink_depth != 0
        || run_depth != 0
        || text_depth != 0
        || section_depth != 0
        || section_owned_by_paragraph
        || paragraph_section_break
        || foreign_depth != 0
    {
        return Err(OfficeError::MalformedXml);
    }
    if !current.is_empty() || units.is_empty() {
        finish_unit(&mut units, &mut current, part, None, budget)?;
    }
    Ok(units)
}

fn finish_unit(
    units: &mut Vec<UnitDraft>,
    current: &mut String,
    part: &str,
    label: Option<String>,
    budget: &ParseBudget<'_>,
) -> Result<(), OfficeError> {
    if units.len() >= budget.limits.max_units {
        return Err(OfficeError::UnitLimit);
    }
    let ordinal = units.len() + 1;
    let label = label.unwrap_or_else(|| format!("Section {ordinal}"));
    let text = normalize_text(std::mem::take(current));
    let mut size = ModelSizer::default();
    size.add(std::mem::size_of::<UnitDraft>().saturating_add(64))?;
    size.string(&label)?;
    size.string_bytes(part.len())?;
    budget.retain_model(size.bytes)?;
    units.try_reserve(1).map_err(|_| OfficeError::UnitLimit)?;
    units.push(UnitDraft {
        label,
        part: part.to_owned(),
        text,
    });
    Ok(())
}

fn parse_xlsx_workbook(
    bytes: &[u8],
    budget: &mut ParseBudget<'_>,
) -> Result<Vec<PendingUnit>, OfficeError> {
    let mut reader = bounded_xml_reader(bytes, budget.limits);
    let mut depth = 0_usize;
    let mut root_seen = false;
    let mut sheets_seen = false;
    let mut sheets_depth = 0_usize;
    let mut sheets = Vec::new();
    loop {
        let (resolved, event) = reader
            .read_resolved_event()
            .map_err(|_| OfficeError::MalformedXml)?;
        let namespace = namespace_tag(resolved);
        budget.event_with_attributes(&reader, &event)?;
        if namespace == NamespaceTag::Unknown {
            return Err(OfficeError::UnsupportedNamespace);
        }
        match event {
            Event::Start(element) => {
                depth = depth.checked_add(1).ok_or(OfficeError::XmlNestingLimit)?;
                validate_depth(depth, budget.limits)?;
                if depth == 1 {
                    if namespace != NamespaceTag::Sheet || local_name(&element) != b"workbook" {
                        return Err(OfficeError::FormatMismatch);
                    }
                    root_seen = true;
                } else if namespace == NamespaceTag::Sheet {
                    match local_name(&element) {
                        b"sheets" => {
                            if depth != 2 || sheets_seen || sheets_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            sheets_seen = true;
                            sheets_depth = depth;
                        }
                        b"sheet" => {
                            if sheets_depth == 0 || depth != sheets_depth + 1 {
                                return Err(OfficeError::MalformedXml);
                            }
                            push_xlsx_sheet(&reader, &element, &mut sheets, budget)?;
                        }
                        _ => {}
                    }
                }
            }
            Event::Empty(element) => {
                if namespace == NamespaceTag::Sheet {
                    match local_name(&element) {
                        b"sheets" => {
                            if depth != 1 || sheets_seen {
                                return Err(OfficeError::MalformedXml);
                            }
                            sheets_seen = true;
                        }
                        b"sheet" => {
                            if sheets_depth == 0 || depth != sheets_depth {
                                return Err(OfficeError::MalformedXml);
                            }
                            push_xlsx_sheet(&reader, &element, &mut sheets, budget)?;
                        }
                        _ => {}
                    }
                }
            }
            Event::End(element) => {
                if depth == 0 {
                    return Err(OfficeError::MalformedXml);
                }
                if namespace == NamespaceTag::Sheet
                    && element.local_name().as_ref() == b"sheets"
                    && sheets_depth == depth
                {
                    sheets_depth = 0;
                }
                depth -= 1;
            }
            Event::GeneralRef(reference) => validate_ignored_reference(&reference)?,
            Event::DocType(_) => return Err(OfficeError::XmlDoctype),
            Event::PI(_) => return Err(OfficeError::ActiveContent),
            Event::Eof => break,
            _ => {}
        }
    }
    if !root_seen || !sheets_seen || depth != 0 || sheets_depth != 0 || sheets.is_empty() {
        return Err(OfficeError::MalformedXml);
    }
    Ok(sheets)
}

fn push_xlsx_sheet(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    sheets: &mut Vec<PendingUnit>,
    budget: &ParseBudget<'_>,
) -> Result<(), OfficeError> {
    if sheets.len() >= budget.limits.max_units {
        return Err(OfficeError::UnitLimit);
    }
    let label = required_attribute(
        reader,
        element,
        NamespaceTag::Unbound,
        b"name",
        budget.limits.max_string_bytes,
    )?;
    let relationship_id = required_attribute(
        reader,
        element,
        NamespaceTag::OfficeRel,
        b"id",
        budget.limits.max_string_bytes,
    )?;
    let order_id = required_attribute(
        reader,
        element,
        NamespaceTag::Unbound,
        b"sheetId",
        budget.limits.max_string_bytes,
    )?;
    if label.is_empty()
        || relationship_id.is_empty()
        || order_id.is_empty()
        || sheets
            .iter()
            .any(|sheet| sheet.relationship_id == relationship_id || sheet.order_id == order_id)
    {
        return Err(OfficeError::MalformedXml);
    }
    let mut size = ModelSizer::default();
    size.add(std::mem::size_of::<PendingUnit>().saturating_add(64))?;
    size.string(&label)?;
    size.string(&relationship_id)?;
    size.string(&order_id)?;
    budget.retain_model(size.bytes)?;
    sheets.try_reserve(1).map_err(|_| OfficeError::UnitLimit)?;
    sheets.push(PendingUnit {
        label,
        relationship_id,
        order_id,
    });
    Ok(())
}

fn parse_xlsx_shared_strings(
    bytes: &[u8],
    budget: &mut ParseBudget<'_>,
) -> Result<Vec<String>, OfficeError> {
    let mut reader = bounded_xml_reader(bytes, budget.limits);
    let mut depth = 0_usize;
    let mut root_seen = false;
    let mut text_depth = 0_usize;
    let mut item_depth = 0_usize;
    let mut run_depth = 0_usize;
    let mut foreign_depth = 0_usize;
    let mut current = String::new();
    let mut strings = Vec::new();
    loop {
        let (resolved, event) = reader
            .read_resolved_event()
            .map_err(|_| OfficeError::MalformedXml)?;
        let namespace = namespace_tag(resolved);
        budget.event_with_attributes(&reader, &event)?;
        if namespace == NamespaceTag::Unknown {
            return Err(OfficeError::UnsupportedNamespace);
        }
        match event {
            Event::Start(element) => {
                depth = depth.checked_add(1).ok_or(OfficeError::XmlNestingLimit)?;
                validate_depth(depth, budget.limits)?;
                if depth == 1 {
                    if namespace != NamespaceTag::Sheet || local_name(&element) != b"sst" {
                        return Err(OfficeError::FormatMismatch);
                    }
                    root_seen = true;
                }
                if namespace == NamespaceTag::Sheet
                    && matches!(
                        local_name(&element),
                        b"oleObject" | b"oleObjects" | b"controls"
                    )
                {
                    return Err(OfficeError::ActiveContent);
                }
                if root_seen && depth > 1 && namespace != NamespaceTag::Sheet && foreign_depth == 0
                {
                    foreign_depth = depth;
                }
                if depth > 1 && namespace == NamespaceTag::Sheet && foreign_depth == 0 {
                    match local_name(&element) {
                        b"si" => {
                            if depth != 2 || item_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            item_depth = depth;
                            current.clear();
                        }
                        b"r" => {
                            if item_depth == 0
                                || depth != item_depth + 1
                                || run_depth != 0
                                || text_depth != 0
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            run_depth = depth;
                        }
                        b"t" => {
                            let direct_item = item_depth > 0 && depth == item_depth + 1;
                            let direct_run = run_depth > 0 && depth == run_depth + 1;
                            if text_depth != 0 || (!direct_item && !direct_run) {
                                return Err(OfficeError::MalformedXml);
                            }
                            text_depth = depth;
                        }
                        _ => {}
                    }
                }
            }
            Event::Empty(element) => {
                if namespace == NamespaceTag::Sheet
                    && matches!(
                        local_name(&element),
                        b"oleObject" | b"oleObjects" | b"controls"
                    )
                {
                    return Err(OfficeError::ActiveContent);
                }
                if namespace == NamespaceTag::Sheet && foreign_depth == 0 {
                    match local_name(&element) {
                        b"si" => {
                            if depth != 1 {
                                return Err(OfficeError::MalformedXml);
                            }
                            push_shared_string(&mut strings, String::new(), budget)?;
                        }
                        b"r" => {
                            if item_depth == 0 || depth != item_depth || run_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                        }
                        b"t" => {
                            let direct_item = item_depth > 0 && depth == item_depth;
                            let direct_run = run_depth > 0 && depth == run_depth;
                            if !direct_item && !direct_run {
                                return Err(OfficeError::MalformedXml);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Event::Text(text) if text_depth > 0 && foreign_depth == 0 => {
                append_text(&mut current, &decoded_text(&text)?, budget)?;
            }
            Event::CData(text) if text_depth > 0 && foreign_depth == 0 => {
                append_text(&mut current, &decoded_cdata(&text)?, budget)?;
            }
            Event::GeneralRef(reference) if text_depth > 0 && foreign_depth == 0 => {
                append_reference(&mut current, &reference, budget)?;
            }
            Event::GeneralRef(reference) => validate_ignored_reference(&reference)?,
            Event::End(element) => {
                if depth == 0 {
                    return Err(OfficeError::MalformedXml);
                }
                if namespace == NamespaceTag::Sheet && foreign_depth == 0 {
                    match element.local_name().as_ref() {
                        b"t" if text_depth == depth => text_depth = 0,
                        b"r" if run_depth == depth => run_depth = 0,
                        b"si" if item_depth == depth => {
                            push_shared_string(
                                &mut strings,
                                normalize_text(std::mem::take(&mut current)),
                                budget,
                            )?;
                            item_depth = 0;
                        }
                        _ => {}
                    }
                }
                if foreign_depth == depth {
                    foreign_depth = 0;
                }
                depth -= 1;
            }
            Event::DocType(_) => return Err(OfficeError::XmlDoctype),
            Event::PI(_) => return Err(OfficeError::ActiveContent),
            Event::Eof => break,
            _ => {}
        }
    }
    if !root_seen
        || depth != 0
        || text_depth != 0
        || item_depth != 0
        || run_depth != 0
        || foreign_depth != 0
    {
        return Err(OfficeError::MalformedXml);
    }
    Ok(strings)
}

fn push_shared_string(
    strings: &mut Vec<String>,
    value: String,
    budget: &ParseBudget<'_>,
) -> Result<(), OfficeError> {
    if strings.len() >= budget.limits.max_shared_strings {
        return Err(OfficeError::TextLimit);
    }
    budget.retain_model(std::mem::size_of::<String>().saturating_add(16))?;
    strings.try_reserve(1).map_err(|_| OfficeError::TextLimit)?;
    strings.push(value);
    Ok(())
}

fn parse_xlsx_worksheet(
    bytes: &[u8],
    shared_strings: &[String],
    budget: &mut ParseBudget<'_>,
) -> Result<String, OfficeError> {
    let mut reader = bounded_xml_reader(bytes, budget.limits);
    let mut depth = 0_usize;
    let mut root_seen = false;
    let mut sheet_data_seen = false;
    let mut sheet_data_depth = 0_usize;
    let mut row_depth = 0_usize;
    let mut cell_depth = 0_usize;
    let mut inline_depth = 0_usize;
    let mut run_depth = 0_usize;
    let mut value_depth = 0_usize;
    let mut foreign_depth = 0_usize;
    let mut cell_type = String::new();
    let mut cell_reference = String::new();
    let mut value = String::new();
    let mut output = String::new();
    loop {
        let (resolved, event) = reader
            .read_resolved_event()
            .map_err(|_| OfficeError::MalformedXml)?;
        let namespace = namespace_tag(resolved);
        budget.event_with_attributes(&reader, &event)?;
        if namespace == NamespaceTag::Unknown {
            return Err(OfficeError::UnsupportedNamespace);
        }
        match event {
            Event::Start(element) => {
                depth = depth.checked_add(1).ok_or(OfficeError::XmlNestingLimit)?;
                validate_depth(depth, budget.limits)?;
                if depth == 1 {
                    if namespace != NamespaceTag::Sheet || local_name(&element) != b"worksheet" {
                        return Err(OfficeError::FormatMismatch);
                    }
                    root_seen = true;
                }
                if namespace == NamespaceTag::Sheet
                    && matches!(
                        local_name(&element),
                        b"oleObject" | b"oleObjects" | b"controls"
                    )
                {
                    return Err(OfficeError::ActiveContent);
                }
                if root_seen && depth > 1 && namespace != NamespaceTag::Sheet && foreign_depth == 0
                {
                    foreign_depth = depth;
                }
                if depth > 1 && namespace == NamespaceTag::Sheet && foreign_depth == 0 {
                    match local_name(&element) {
                        b"sheetData" => {
                            if depth != 2 || sheet_data_seen || sheet_data_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            sheet_data_seen = true;
                            sheet_data_depth = depth;
                        }
                        b"row" => {
                            if sheet_data_depth == 0
                                || depth != sheet_data_depth + 1
                                || row_depth != 0
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            row_depth = depth;
                        }
                        b"c" => {
                            if row_depth == 0 || depth != row_depth + 1 || cell_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            budget.cell()?;
                            cell_depth = depth;
                            cell_type = decoded_attribute(
                                &reader,
                                &element,
                                NamespaceTag::Unbound,
                                b"t",
                                budget.limits.max_string_bytes,
                            )?
                            .unwrap_or_default();
                            cell_reference = decoded_attribute(
                                &reader,
                                &element,
                                NamespaceTag::Unbound,
                                b"r",
                                budget.limits.max_string_bytes,
                            )?
                            .unwrap_or_default();
                            if !cell_reference.is_empty() && !valid_cell_reference(&cell_reference)
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            value.clear();
                        }
                        b"is" => {
                            if cell_depth == 0
                                || depth != cell_depth + 1
                                || inline_depth != 0
                                || value_depth != 0
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            inline_depth = depth;
                        }
                        b"r" => {
                            if inline_depth == 0
                                || depth != inline_depth + 1
                                || run_depth != 0
                                || value_depth != 0
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            run_depth = depth;
                        }
                        b"v" => {
                            if cell_depth == 0
                                || depth != cell_depth + 1
                                || inline_depth != 0
                                || value_depth != 0
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            value_depth = depth;
                        }
                        b"t" => {
                            let direct_inline = inline_depth > 0 && depth == inline_depth + 1;
                            let direct_run = run_depth > 0 && depth == run_depth + 1;
                            if value_depth != 0 || (!direct_inline && !direct_run) {
                                return Err(OfficeError::MalformedXml);
                            }
                            value_depth = depth;
                        }
                        _ => {}
                    }
                }
            }
            Event::Empty(element) => {
                if namespace == NamespaceTag::Sheet
                    && matches!(
                        local_name(&element),
                        b"oleObject" | b"oleObjects" | b"controls"
                    )
                {
                    return Err(OfficeError::ActiveContent);
                }
                if namespace == NamespaceTag::Sheet && foreign_depth == 0 {
                    match local_name(&element) {
                        b"sheetData" => {
                            if depth != 1 || sheet_data_seen {
                                return Err(OfficeError::MalformedXml);
                            }
                            sheet_data_seen = true;
                        }
                        b"row" => {
                            if sheet_data_depth == 0 || depth != sheet_data_depth {
                                return Err(OfficeError::MalformedXml);
                            }
                        }
                        b"c" => {
                            if row_depth == 0 || depth != row_depth {
                                return Err(OfficeError::MalformedXml);
                            }
                            budget.cell()?;
                        }
                        b"is" => {
                            if cell_depth == 0 || depth != cell_depth || inline_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                        }
                        b"r" => {
                            if inline_depth == 0 || depth != inline_depth || run_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                        }
                        b"v" => {
                            if cell_depth == 0 || depth != cell_depth || inline_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                        }
                        b"t" => {
                            let direct_inline = inline_depth > 0 && depth == inline_depth;
                            let direct_run = run_depth > 0 && depth == run_depth;
                            if !direct_inline && !direct_run {
                                return Err(OfficeError::MalformedXml);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Event::Text(text) if value_depth > 0 && foreign_depth == 0 => {
                append_text(&mut value, &decoded_text(&text)?, budget)?;
            }
            Event::CData(text) if value_depth > 0 && foreign_depth == 0 => {
                append_text(&mut value, &decoded_cdata(&text)?, budget)?;
            }
            Event::GeneralRef(reference) if value_depth > 0 && foreign_depth == 0 => {
                append_reference(&mut value, &reference, budget)?;
            }
            Event::GeneralRef(reference) => validate_ignored_reference(&reference)?,
            Event::End(element) => {
                if depth == 0 {
                    return Err(OfficeError::MalformedXml);
                }
                if namespace == NamespaceTag::Sheet && foreign_depth == 0 {
                    match element.local_name().as_ref() {
                        b"v" | b"t" if value_depth == depth => value_depth = 0,
                        b"r" if run_depth == depth => run_depth = 0,
                        b"is" if inline_depth == depth => inline_depth = 0,
                        b"c" if cell_depth == depth => {
                            if inline_depth != 0 || run_depth != 0 || value_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            let rendered = render_cell_value(&cell_type, &value, shared_strings)?;
                            if !rendered.is_empty() {
                                if !cell_reference.is_empty() {
                                    append_text(&mut output, &cell_reference, budget)?;
                                    append_text(&mut output, ": ", budget)?;
                                }
                                append_text(&mut output, &rendered, budget)?;
                                append_separator(&mut output, "\n", budget)?;
                            }
                            cell_depth = 0;
                            value_depth = 0;
                        }
                        b"row" if row_depth == depth => row_depth = 0,
                        b"sheetData" if sheet_data_depth == depth => sheet_data_depth = 0,
                        _ => {}
                    }
                }
                if foreign_depth == depth {
                    foreign_depth = 0;
                }
                depth -= 1;
            }
            Event::DocType(_) => return Err(OfficeError::XmlDoctype),
            Event::PI(_) => return Err(OfficeError::ActiveContent),
            Event::Eof => break,
            _ => {}
        }
    }
    if !root_seen
        || !sheet_data_seen
        || depth != 0
        || sheet_data_depth != 0
        || row_depth != 0
        || cell_depth != 0
        || inline_depth != 0
        || run_depth != 0
        || value_depth != 0
        || foreign_depth != 0
    {
        return Err(OfficeError::MalformedXml);
    }
    Ok(normalize_text(output))
}

fn valid_cell_reference(value: &str) -> bool {
    let letters = value.bytes().take_while(u8::is_ascii_alphabetic).count();
    let digits = value.len().saturating_sub(letters);
    (1..=4).contains(&letters)
        && digits > 0
        && value[..letters]
            .bytes()
            .all(|byte| byte.is_ascii_uppercase())
        && value[letters..].bytes().all(|byte| byte.is_ascii_digit())
}

fn render_cell_value(
    cell_type: &str,
    value: &str,
    shared_strings: &[String],
) -> Result<String, OfficeError> {
    match cell_type {
        "s" => value
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|index| shared_strings.get(index))
            .cloned()
            .ok_or(OfficeError::MalformedXml),
        "" | "inlineStr" | "str" | "b" | "n" | "e" | "d" => Ok(normalize_text(value.to_owned())),
        _ => Err(OfficeError::MalformedXml),
    }
}

fn parse_pptx_presentation(
    bytes: &[u8],
    budget: &mut ParseBudget<'_>,
) -> Result<Vec<PendingSlide>, OfficeError> {
    let mut reader = bounded_xml_reader(bytes, budget.limits);
    let mut depth = 0_usize;
    let mut root_seen = false;
    let mut list_seen = false;
    let mut list_depth = 0_usize;
    let mut slide_ids = Vec::new();
    loop {
        let (resolved, event) = reader
            .read_resolved_event()
            .map_err(|_| OfficeError::MalformedXml)?;
        let namespace = namespace_tag(resolved);
        budget.event_with_attributes(&reader, &event)?;
        if namespace == NamespaceTag::Unknown {
            return Err(OfficeError::UnsupportedNamespace);
        }
        match event {
            Event::Start(element) => {
                depth = depth.checked_add(1).ok_or(OfficeError::XmlNestingLimit)?;
                validate_depth(depth, budget.limits)?;
                if depth == 1 {
                    if namespace != NamespaceTag::Presentation
                        || local_name(&element) != b"presentation"
                    {
                        return Err(OfficeError::FormatMismatch);
                    }
                    root_seen = true;
                } else if namespace == NamespaceTag::Presentation {
                    match local_name(&element) {
                        b"sldIdLst" => {
                            if depth != 2 || list_seen || list_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            list_seen = true;
                            list_depth = depth;
                        }
                        b"sldId" => {
                            if list_depth == 0 || depth != list_depth + 1 {
                                return Err(OfficeError::MalformedXml);
                            }
                            push_pptx_slide_id(&reader, &element, &mut slide_ids, budget)?;
                        }
                        _ => {}
                    }
                }
            }
            Event::Empty(element) => {
                if namespace == NamespaceTag::Presentation {
                    match local_name(&element) {
                        b"sldIdLst" => {
                            if depth != 1 || list_seen {
                                return Err(OfficeError::MalformedXml);
                            }
                            list_seen = true;
                        }
                        b"sldId" => {
                            if list_depth == 0 || depth != list_depth {
                                return Err(OfficeError::MalformedXml);
                            }
                            push_pptx_slide_id(&reader, &element, &mut slide_ids, budget)?;
                        }
                        _ => {}
                    }
                }
            }
            Event::End(element) => {
                if depth == 0 {
                    return Err(OfficeError::MalformedXml);
                }
                if namespace == NamespaceTag::Presentation
                    && element.local_name().as_ref() == b"sldIdLst"
                    && list_depth == depth
                {
                    list_depth = 0;
                }
                depth -= 1;
            }
            Event::GeneralRef(reference) => validate_ignored_reference(&reference)?,
            Event::DocType(_) => return Err(OfficeError::XmlDoctype),
            Event::PI(_) => return Err(OfficeError::ActiveContent),
            Event::Eof => break,
            _ => {}
        }
    }
    if !root_seen || !list_seen || depth != 0 || list_depth != 0 || slide_ids.is_empty() {
        return Err(OfficeError::MalformedXml);
    }
    Ok(slide_ids)
}

fn push_pptx_slide_id(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    slide_ids: &mut Vec<PendingSlide>,
    budget: &ParseBudget<'_>,
) -> Result<(), OfficeError> {
    if slide_ids.len() >= budget.limits.max_units {
        return Err(OfficeError::UnitLimit);
    }
    let id = required_attribute(
        reader,
        element,
        NamespaceTag::OfficeRel,
        b"id",
        budget.limits.max_string_bytes,
    )?;
    let order_id = required_attribute(
        reader,
        element,
        NamespaceTag::Unbound,
        b"id",
        budget.limits.max_string_bytes,
    )?;
    if id.is_empty()
        || order_id.is_empty()
        || slide_ids
            .iter()
            .any(|slide| slide.relationship_id == id || slide.order_id == order_id)
    {
        return Err(OfficeError::MalformedXml);
    }
    let mut size = ModelSizer::default();
    size.add(std::mem::size_of::<PendingSlide>().saturating_add(64))?;
    size.string(&id)?;
    size.string(&order_id)?;
    budget.retain_model(size.bytes)?;
    slide_ids
        .try_reserve(1)
        .map_err(|_| OfficeError::UnitLimit)?;
    slide_ids.push(PendingSlide {
        relationship_id: id,
        order_id,
    });
    Ok(())
}

fn parse_pptx_slide(bytes: &[u8], budget: &mut ParseBudget<'_>) -> Result<String, OfficeError> {
    let mut reader = bounded_xml_reader(bytes, budget.limits);
    let mut depth = 0_usize;
    let mut root_seen = false;
    let mut content_seen = false;
    let mut content_depth = 0_usize;
    let mut shape_tree_seen = false;
    let mut shape_tree_depth = 0_usize;
    let mut shape_depth = 0_usize;
    let mut text_body_depth = 0_usize;
    let mut paragraph_depth = 0_usize;
    let mut run_depth = 0_usize;
    let mut field_depth = 0_usize;
    let mut text_depth = 0_usize;
    let mut foreign_depth = 0_usize;
    let mut output = String::new();
    loop {
        let (resolved, event) = reader
            .read_resolved_event()
            .map_err(|_| OfficeError::MalformedXml)?;
        let namespace = namespace_tag(resolved);
        budget.event_with_attributes(&reader, &event)?;
        if namespace == NamespaceTag::Unknown {
            return Err(OfficeError::UnsupportedNamespace);
        }
        match event {
            Event::Start(element) => {
                depth = depth.checked_add(1).ok_or(OfficeError::XmlNestingLimit)?;
                validate_depth(depth, budget.limits)?;
                if depth == 1 {
                    if namespace != NamespaceTag::Presentation || local_name(&element) != b"sld" {
                        return Err(OfficeError::FormatMismatch);
                    }
                    root_seen = true;
                }
                if namespace == NamespaceTag::Presentation
                    && matches!(local_name(&element), b"oleObj" | b"control")
                {
                    return Err(OfficeError::ActiveContent);
                }
                if root_seen
                    && depth > 1
                    && !matches!(
                        namespace,
                        NamespaceTag::Presentation | NamespaceTag::Drawing
                    )
                    && foreign_depth == 0
                {
                    foreign_depth = depth;
                }
                if namespace == NamespaceTag::Presentation && foreign_depth == 0 {
                    match local_name(&element) {
                        b"cSld" => {
                            if depth != 2 || content_seen || content_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            content_seen = true;
                            content_depth = depth;
                        }
                        b"spTree" => {
                            if content_depth == 0
                                || depth != content_depth + 1
                                || shape_tree_seen
                                || shape_tree_depth != 0
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            shape_tree_seen = true;
                            shape_tree_depth = depth;
                        }
                        b"sp" => {
                            if shape_tree_depth == 0
                                || depth != shape_tree_depth + 1
                                || shape_depth != 0
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            shape_depth = depth;
                        }
                        b"txBody" => {
                            if shape_depth == 0 || depth != shape_depth + 1 || text_body_depth != 0
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            text_body_depth = depth;
                        }
                        _ => {}
                    }
                }
                if namespace == NamespaceTag::Drawing && foreign_depth == 0 {
                    match local_name(&element) {
                        b"p" => {
                            if text_body_depth == 0
                                || depth != text_body_depth + 1
                                || paragraph_depth != 0
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            paragraph_depth = depth;
                        }
                        b"r" => {
                            if paragraph_depth == 0
                                || depth != paragraph_depth + 1
                                || run_depth != 0
                                || field_depth != 0
                                || text_depth != 0
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            run_depth = depth;
                        }
                        b"fld" => {
                            if paragraph_depth == 0
                                || depth != paragraph_depth + 1
                                || run_depth != 0
                                || field_depth != 0
                                || text_depth != 0
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            field_depth = depth;
                        }
                        b"t" => {
                            let direct_run = run_depth > 0 && depth == run_depth + 1;
                            let direct_field = field_depth > 0 && depth == field_depth + 1;
                            if text_depth != 0 || (!direct_run && !direct_field) {
                                return Err(OfficeError::MalformedXml);
                            }
                            text_depth = depth;
                        }
                        _ => {}
                    }
                }
            }
            Event::Empty(element) => {
                if namespace == NamespaceTag::Presentation
                    && matches!(local_name(&element), b"oleObj" | b"control")
                {
                    return Err(OfficeError::ActiveContent);
                }
                if namespace == NamespaceTag::Drawing && foreign_depth == 0 {
                    match local_name(&element) {
                        b"r" | b"fld" => {
                            if paragraph_depth == 0
                                || depth != paragraph_depth
                                || run_depth != 0
                                || field_depth != 0
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                        }
                        b"t" => {
                            let direct_run = run_depth > 0 && depth == run_depth;
                            let direct_field = field_depth > 0 && depth == field_depth;
                            if !direct_run && !direct_field {
                                return Err(OfficeError::MalformedXml);
                            }
                        }
                        b"br" => {
                            if paragraph_depth == 0 || depth != paragraph_depth {
                                return Err(OfficeError::MalformedXml);
                            }
                            append_separator(&mut output, "\n", budget)?;
                        }
                        _ => {}
                    }
                }
            }
            Event::Text(text) if text_depth > 0 && foreign_depth == 0 => {
                append_text(&mut output, &decoded_text(&text)?, budget)?;
            }
            Event::CData(text) if text_depth > 0 && foreign_depth == 0 => {
                append_text(&mut output, &decoded_cdata(&text)?, budget)?;
            }
            Event::GeneralRef(reference) if text_depth > 0 && foreign_depth == 0 => {
                append_reference(&mut output, &reference, budget)?;
            }
            Event::GeneralRef(reference) => validate_ignored_reference(&reference)?,
            Event::End(element) => {
                if depth == 0 {
                    return Err(OfficeError::MalformedXml);
                }
                if namespace == NamespaceTag::Drawing && foreign_depth == 0 {
                    match element.local_name().as_ref() {
                        b"t" if text_depth == depth => text_depth = 0,
                        b"r" if run_depth == depth => run_depth = 0,
                        b"fld" if field_depth == depth => field_depth = 0,
                        b"p" if paragraph_depth == depth => {
                            if run_depth != 0 || field_depth != 0 || text_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            append_separator(&mut output, "\n", budget)?;
                            paragraph_depth = 0;
                        }
                        _ => {}
                    }
                }
                if namespace == NamespaceTag::Presentation && foreign_depth == 0 {
                    match element.local_name().as_ref() {
                        b"txBody" if text_body_depth == depth => text_body_depth = 0,
                        b"sp" if shape_depth == depth => shape_depth = 0,
                        b"spTree" if shape_tree_depth == depth => shape_tree_depth = 0,
                        b"cSld" if content_depth == depth => content_depth = 0,
                        _ => {}
                    }
                }
                if foreign_depth == depth {
                    foreign_depth = 0;
                }
                depth -= 1;
            }
            Event::DocType(_) => return Err(OfficeError::XmlDoctype),
            Event::PI(_) => return Err(OfficeError::ActiveContent),
            Event::Eof => break,
            _ => {}
        }
    }
    if !root_seen
        || !content_seen
        || !shape_tree_seen
        || depth != 0
        || content_depth != 0
        || shape_tree_depth != 0
        || shape_depth != 0
        || text_body_depth != 0
        || paragraph_depth != 0
        || run_depth != 0
        || field_depth != 0
        || text_depth != 0
        || foreign_depth != 0
    {
        return Err(OfficeError::MalformedXml);
    }
    Ok(normalize_text(output))
}

fn parse_odf_manifest(
    bytes: &[u8],
    budget: &mut ParseBudget<'_>,
) -> Result<OdfManifestDraft, OfficeError> {
    let mut reader = bounded_xml_reader(bytes, budget.limits);
    let mut depth = 0_usize;
    let mut root_seen = false;
    let mut file_entry_depth = 0_usize;
    let mut draft = OdfManifestDraft::default();
    let mut ordinal = 0_usize;
    loop {
        let (resolved, event) = reader
            .read_resolved_event()
            .map_err(|_| OfficeError::MalformedXml)?;
        let namespace = namespace_tag(resolved);
        budget.event_with_attributes(&reader, &event)?;
        if namespace == NamespaceTag::Unknown {
            return Err(OfficeError::UnsupportedNamespace);
        }
        match event {
            Event::Start(element) => {
                depth = depth.checked_add(1).ok_or(OfficeError::XmlNestingLimit)?;
                validate_depth(depth, budget.limits)?;
                if namespace == NamespaceTag::OdfManifest
                    && matches!(
                        local_name(&element),
                        b"encryption-data" | b"algorithm" | b"key-derivation" | b"encrypted-key"
                    )
                {
                    return Err(OfficeError::Encrypted);
                }
                if depth == 1 {
                    if namespace != NamespaceTag::OdfManifest || local_name(&element) != b"manifest"
                    {
                        return Err(OfficeError::FormatMismatch);
                    }
                    root_seen = true;
                } else if namespace == NamespaceTag::OdfManifest {
                    match local_name(&element) {
                        b"file-entry" => {
                            if depth != 2 || file_entry_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            push_odf_manifest_entry(
                                &reader,
                                &element,
                                &mut draft,
                                &mut ordinal,
                                budget,
                            )?;
                            file_entry_depth = depth;
                        }
                        b"encryption-data" | b"algorithm" | b"key-derivation" => {
                            return Err(OfficeError::Encrypted)
                        }
                        _ => {}
                    }
                }
            }
            Event::Empty(element) => {
                if namespace == NamespaceTag::OdfManifest
                    && matches!(
                        local_name(&element),
                        b"encryption-data" | b"algorithm" | b"key-derivation" | b"encrypted-key"
                    )
                {
                    return Err(OfficeError::Encrypted);
                }
                if namespace == NamespaceTag::OdfManifest {
                    match local_name(&element) {
                        b"file-entry" => {
                            if depth != 1 {
                                return Err(OfficeError::MalformedXml);
                            }
                            push_odf_manifest_entry(
                                &reader,
                                &element,
                                &mut draft,
                                &mut ordinal,
                                budget,
                            )?;
                        }
                        b"encryption-data" | b"algorithm" | b"key-derivation" => {
                            return Err(OfficeError::Encrypted)
                        }
                        _ => {}
                    }
                }
            }
            Event::End(element) => {
                if depth == 0 {
                    return Err(OfficeError::MalformedXml);
                }
                if namespace == NamespaceTag::OdfManifest
                    && element.local_name().as_ref() == b"file-entry"
                    && file_entry_depth == depth
                {
                    file_entry_depth = 0;
                }
                depth -= 1;
            }
            Event::GeneralRef(reference) => validate_ignored_reference(&reference)?,
            Event::DocType(_) => return Err(OfficeError::XmlDoctype),
            Event::PI(_) => return Err(OfficeError::ActiveContent),
            Event::Eof => break,
            _ => {}
        }
    }
    if !root_seen || depth != 0 || file_entry_depth != 0 {
        return Err(OfficeError::MalformedXml);
    }
    if draft.root_media_type.is_none() {
        return Err(OfficeError::MalformedXml);
    }
    Ok(draft)
}

fn push_odf_manifest_entry(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    draft: &mut OdfManifestDraft,
    ordinal: &mut usize,
    budget: &mut ParseBudget<'_>,
) -> Result<(), OfficeError> {
    let raw_path = required_attribute(
        reader,
        element,
        NamespaceTag::OdfManifest,
        b"full-path",
        budget.limits.max_string_bytes,
    )?;
    let media_type = required_attribute(
        reader,
        element,
        NamespaceTag::OdfManifest,
        b"media-type",
        budget.limits.max_string_bytes,
    )?;
    let directory = raw_path.ends_with('/') && raw_path != "/";
    let mut path = if raw_path == "/" {
        raw_path
    } else {
        normalize_zip_member_path(&raw_path, budget.limits.max_string_bytes)?
    };
    if directory {
        path.push('/');
        if path.len() > budget.limits.max_string_bytes {
            return Err(OfficeError::StringLimit);
        }
    }
    if draft.entries.contains(&path) {
        return Err(OfficeError::MalformedXml);
    }
    let mut entry_size = ModelSizer::default();
    entry_size.add(64)?;
    entry_size.string(&path)?;
    budget.retain_model(entry_size.bytes)?;
    draft.entries.insert(path.clone());
    if path == "/" {
        if draft.root_media_type.is_some() {
            return Err(OfficeError::MalformedXml);
        }
        let mut size = ModelSizer::default();
        size.string(&media_type)?;
        budget.retain_model(size.bytes)?;
        draft.root_media_type = Some(media_type);
        return Ok(());
    }
    if path.ends_with('/') {
        return Ok(());
    }
    budget.relationship()?;
    *ordinal = ordinal
        .checked_add(1)
        .ok_or(OfficeError::RelationshipLimit)?;
    let target_part = path;
    let id = format!("manifest:{ordinal:06}");
    let mut size = ModelSizer::default();
    size.add(std::mem::size_of::<RelationshipDraft>().saturating_add(64))?;
    size.string(&id)?;
    size.string(&target_part)?;
    budget.retain_model(size.bytes)?;
    draft
        .relationships
        .try_reserve(1)
        .map_err(|_| OfficeError::RelationshipLimit)?;
    draft.relationships.push(RelationshipDraft {
        source_part: None,
        id,
        target_part,
        kind: "manifest",
    });
    Ok(())
}

fn parse_odf_content(
    kind: OfficeKind,
    bytes: &[u8],
    part: &str,
    budget: &mut ParseBudget<'_>,
) -> Result<Vec<UnitDraft>, OfficeError> {
    let mut reader = bounded_xml_reader(bytes, budget.limits);
    let mut depth = 0_usize;
    let mut root_seen = false;
    let mut body_depth = 0_usize;
    let mut body_seen = false;
    let mut typed_body_depth = 0_usize;
    let mut typed_body_seen = false;
    let mut paragraph_depth = 0_usize;
    let mut foreign_depth = 0_usize;
    let mut section_depth = 0_usize;
    let mut active_depth = 0_usize;
    let mut row_depth = 0_usize;
    let mut cell_depth = 0_usize;
    let mut frame_depth = 0_usize;
    let mut text_box_depth = 0_usize;
    let mut current = String::new();
    let mut current_label = None;
    let mut units = Vec::new();
    loop {
        let (resolved, event) = reader
            .read_resolved_event()
            .map_err(|_| OfficeError::MalformedXml)?;
        let namespace = namespace_tag(resolved);
        budget.event_with_attributes(&reader, &event)?;
        if namespace == NamespaceTag::Unknown {
            return Err(OfficeError::UnsupportedNamespace);
        }
        match event {
            Event::Start(element) => {
                depth = depth.checked_add(1).ok_or(OfficeError::XmlNestingLimit)?;
                validate_depth(depth, budget.limits)?;
                if depth == 1 {
                    if namespace != NamespaceTag::OdfOffice
                        || local_name(&element) != b"document-content"
                    {
                        return Err(OfficeError::FormatMismatch);
                    }
                    root_seen = true;
                }
                if namespace == NamespaceTag::OdfOffice
                    && matches!(
                        local_name(&element),
                        b"scripts" | b"binary-data" | b"event-listeners"
                    )
                {
                    return Err(OfficeError::ActiveContent);
                }
                if namespace == NamespaceTag::OdfScript {
                    return Err(OfficeError::ActiveContent);
                }
                if namespace == NamespaceTag::OdfDraw
                    && matches!(local_name(&element), b"object" | b"object-ole" | b"plugin")
                {
                    return Err(OfficeError::ActiveContent);
                }
                let supported_content_namespace = match kind {
                    OfficeKind::Odt => {
                        matches!(namespace, NamespaceTag::OdfOffice | NamespaceTag::OdfText)
                    }
                    OfficeKind::Ods => matches!(
                        namespace,
                        NamespaceTag::OdfOffice | NamespaceTag::OdfText | NamespaceTag::OdfTable
                    ),
                    OfficeKind::Odp => matches!(
                        namespace,
                        NamespaceTag::OdfOffice | NamespaceTag::OdfText | NamespaceTag::OdfDraw
                    ),
                    _ => return Err(OfficeError::FormatMismatch),
                };
                if typed_body_depth > 0 && foreign_depth == 0 && !supported_content_namespace {
                    foreign_depth = depth;
                }
                if foreign_depth == 0
                    && namespace == NamespaceTag::OdfText
                    && matches!(local_name(&element), b"tab" | b"line-break")
                {
                    return Err(OfficeError::MalformedXml);
                }
                if foreign_depth == 0
                    && namespace == NamespaceTag::OdfOffice
                    && local_name(&element) == b"body"
                {
                    if depth != 2 || body_depth != 0 || body_seen {
                        return Err(OfficeError::FormatMismatch);
                    }
                    body_depth = depth;
                    body_seen = true;
                }
                if foreign_depth == 0
                    && namespace == NamespaceTag::OdfOffice
                    && matches!(
                        local_name(&element),
                        b"text" | b"spreadsheet" | b"presentation"
                    )
                {
                    let expected = match kind {
                        OfficeKind::Odt => b"text".as_slice(),
                        OfficeKind::Ods => b"spreadsheet".as_slice(),
                        OfficeKind::Odp => b"presentation".as_slice(),
                        _ => return Err(OfficeError::FormatMismatch),
                    };
                    if body_depth == 0
                        || depth != body_depth + 1
                        || local_name(&element) != expected
                        || typed_body_seen
                    {
                        return Err(OfficeError::FormatMismatch);
                    }
                    typed_body_depth = depth;
                    typed_body_seen = true;
                }
                match kind {
                    OfficeKind::Odt => {
                        if foreign_depth == 0
                            && namespace == NamespaceTag::OdfText
                            && local_name(&element) == b"section"
                        {
                            if typed_body_depth == 0
                                || depth != typed_body_depth + 1
                                || section_depth != 0
                                || paragraph_depth != 0
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            if !current.is_empty() {
                                finish_odf_unit(
                                    &mut units,
                                    &mut current,
                                    &mut current_label,
                                    part,
                                    kind,
                                    budget,
                                )?;
                            }
                            current_label = decoded_attribute(
                                &reader,
                                &element,
                                NamespaceTag::OdfText,
                                b"name",
                                budget.limits.max_string_bytes,
                            )?;
                            section_depth = depth;
                        }
                    }
                    OfficeKind::Ods => {
                        if foreign_depth == 0 && namespace == NamespaceTag::OdfTable {
                            match local_name(&element) {
                                b"table" => {
                                    if typed_body_depth == 0
                                        || depth != typed_body_depth + 1
                                        || active_depth != 0
                                    {
                                        return Err(OfficeError::MalformedXml);
                                    }
                                    active_depth = depth;
                                    current.clear();
                                    current_label = decoded_attribute(
                                        &reader,
                                        &element,
                                        NamespaceTag::OdfTable,
                                        b"name",
                                        budget.limits.max_string_bytes,
                                    )?;
                                }
                                b"table-row" => {
                                    if active_depth == 0 || row_depth != 0 || cell_depth != 0 {
                                        return Err(OfficeError::MalformedXml);
                                    }
                                    row_depth = depth;
                                }
                                b"table-cell" | b"covered-table-cell" => {
                                    if row_depth == 0 || depth != row_depth + 1 || cell_depth != 0 {
                                        return Err(OfficeError::MalformedXml);
                                    }
                                    budget.cell()?;
                                    cell_depth = depth;
                                }
                                _ => {}
                            }
                        }
                    }
                    OfficeKind::Odp => {
                        if foreign_depth == 0 && namespace == NamespaceTag::OdfDraw {
                            match local_name(&element) {
                                b"page" => {
                                    if typed_body_depth == 0
                                        || depth != typed_body_depth + 1
                                        || active_depth != 0
                                    {
                                        return Err(OfficeError::MalformedXml);
                                    }
                                    active_depth = depth;
                                    current.clear();
                                    current_label = decoded_attribute(
                                        &reader,
                                        &element,
                                        NamespaceTag::OdfDraw,
                                        b"name",
                                        budget.limits.max_string_bytes,
                                    )?;
                                }
                                b"frame" => {
                                    if active_depth == 0 || frame_depth != 0 || text_box_depth != 0
                                    {
                                        return Err(OfficeError::MalformedXml);
                                    }
                                    frame_depth = depth;
                                }
                                b"text-box" => {
                                    if frame_depth == 0
                                        || depth != frame_depth + 1
                                        || text_box_depth != 0
                                    {
                                        return Err(OfficeError::MalformedXml);
                                    }
                                    text_box_depth = depth;
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => return Err(OfficeError::FormatMismatch),
                }
                if foreign_depth == 0
                    && namespace == NamespaceTag::OdfText
                    && matches!(local_name(&element), b"p" | b"h")
                {
                    let owned = match kind {
                        OfficeKind::Odt => typed_body_depth > 0,
                        OfficeKind::Ods => cell_depth > 0,
                        OfficeKind::Odp => text_box_depth > 0,
                        _ => false,
                    };
                    if !owned || paragraph_depth != 0 {
                        return Err(OfficeError::MalformedXml);
                    }
                    paragraph_depth = depth;
                }
            }
            Event::Empty(element) => {
                if namespace == NamespaceTag::OdfOffice
                    && matches!(
                        local_name(&element),
                        b"scripts" | b"binary-data" | b"event-listeners"
                    )
                {
                    return Err(OfficeError::ActiveContent);
                }
                if namespace == NamespaceTag::OdfScript {
                    return Err(OfficeError::ActiveContent);
                }
                if namespace == NamespaceTag::OdfDraw
                    && matches!(local_name(&element), b"object" | b"object-ole" | b"plugin")
                {
                    return Err(OfficeError::ActiveContent);
                }
                if foreign_depth == 0 {
                    if namespace == NamespaceTag::OdfText
                        && matches!(local_name(&element), b"p" | b"h")
                    {
                        let owned = match kind {
                            OfficeKind::Odt => typed_body_depth > 0,
                            OfficeKind::Ods => cell_depth > 0,
                            OfficeKind::Odp => text_box_depth > 0,
                            _ => false,
                        };
                        if !owned || paragraph_depth != 0 {
                            return Err(OfficeError::MalformedXml);
                        }
                    }
                    if namespace == NamespaceTag::OdfText
                        && matches!(local_name(&element), b"tab" | b"line-break")
                    {
                        if paragraph_depth == 0 {
                            return Err(OfficeError::MalformedXml);
                        }
                        match local_name(&element) {
                            b"tab" => append_text(&mut current, "\t", budget)?,
                            b"line-break" => append_separator(&mut current, "\n", budget)?,
                            _ => unreachable!(),
                        }
                    }
                    match kind {
                        OfficeKind::Odt => {
                            if namespace == NamespaceTag::OdfText
                                && local_name(&element) == b"section"
                            {
                                if typed_body_depth == 0
                                    || depth != typed_body_depth
                                    || section_depth != 0
                                    || paragraph_depth != 0
                                {
                                    return Err(OfficeError::MalformedXml);
                                }
                                if !current.is_empty() {
                                    finish_odf_unit(
                                        &mut units,
                                        &mut current,
                                        &mut current_label,
                                        part,
                                        kind,
                                        budget,
                                    )?;
                                }
                                current_label = decoded_attribute(
                                    &reader,
                                    &element,
                                    NamespaceTag::OdfText,
                                    b"name",
                                    budget.limits.max_string_bytes,
                                )?;
                                finish_odf_unit(
                                    &mut units,
                                    &mut current,
                                    &mut current_label,
                                    part,
                                    kind,
                                    budget,
                                )?;
                            }
                        }
                        OfficeKind::Ods => {
                            if namespace == NamespaceTag::OdfTable {
                                match local_name(&element) {
                                    b"table" => {
                                        if typed_body_depth == 0
                                            || depth != typed_body_depth
                                            || active_depth != 0
                                        {
                                            return Err(OfficeError::MalformedXml);
                                        }
                                        current.clear();
                                        current_label = decoded_attribute(
                                            &reader,
                                            &element,
                                            NamespaceTag::OdfTable,
                                            b"name",
                                            budget.limits.max_string_bytes,
                                        )?;
                                        finish_odf_unit(
                                            &mut units,
                                            &mut current,
                                            &mut current_label,
                                            part,
                                            kind,
                                            budget,
                                        )?;
                                    }
                                    b"table-row" => {
                                        if active_depth == 0 || row_depth != 0 || cell_depth != 0 {
                                            return Err(OfficeError::MalformedXml);
                                        }
                                    }
                                    b"table-cell" | b"covered-table-cell" => {
                                        if row_depth == 0 || depth != row_depth || cell_depth != 0 {
                                            return Err(OfficeError::MalformedXml);
                                        }
                                        budget.cell()?;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        OfficeKind::Odp => {
                            if namespace == NamespaceTag::OdfDraw {
                                match local_name(&element) {
                                    b"page" => {
                                        if typed_body_depth == 0
                                            || depth != typed_body_depth
                                            || active_depth != 0
                                        {
                                            return Err(OfficeError::MalformedXml);
                                        }
                                        current.clear();
                                        current_label = decoded_attribute(
                                            &reader,
                                            &element,
                                            NamespaceTag::OdfDraw,
                                            b"name",
                                            budget.limits.max_string_bytes,
                                        )?;
                                        finish_odf_unit(
                                            &mut units,
                                            &mut current,
                                            &mut current_label,
                                            part,
                                            kind,
                                            budget,
                                        )?;
                                    }
                                    b"frame"
                                        if active_depth == 0
                                            || frame_depth != 0
                                            || text_box_depth != 0 =>
                                    {
                                        return Err(OfficeError::MalformedXml);
                                    }
                                    b"text-box"
                                        if frame_depth == 0
                                            || depth != frame_depth
                                            || text_box_depth != 0 =>
                                    {
                                        return Err(OfficeError::MalformedXml);
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => return Err(OfficeError::FormatMismatch),
                    }
                }
            }
            Event::Text(text)
                if typed_body_depth > 0 && paragraph_depth > 0 && foreign_depth == 0 =>
            {
                if kind == OfficeKind::Odt || active_depth > 0 {
                    append_text(&mut current, &decoded_text(&text)?, budget)?;
                }
            }
            Event::CData(text)
                if typed_body_depth > 0 && paragraph_depth > 0 && foreign_depth == 0 =>
            {
                if kind == OfficeKind::Odt || active_depth > 0 {
                    append_text(&mut current, &decoded_cdata(&text)?, budget)?;
                }
            }
            Event::GeneralRef(reference)
                if typed_body_depth > 0 && paragraph_depth > 0 && foreign_depth == 0 =>
            {
                if kind == OfficeKind::Odt || active_depth > 0 {
                    append_reference(&mut current, &reference, budget)?;
                } else {
                    validate_ignored_reference(&reference)?;
                }
            }
            Event::GeneralRef(reference) => validate_ignored_reference(&reference)?,
            Event::End(element) => {
                if depth == 0 {
                    return Err(OfficeError::MalformedXml);
                }
                if foreign_depth == 0 && namespace == NamespaceTag::OdfText {
                    match element.local_name().as_ref() {
                        b"p" | b"h" => {
                            if paragraph_depth != depth {
                                return Err(OfficeError::MalformedXml);
                            }
                            append_separator(&mut current, "\n", budget)?;
                            paragraph_depth = 0;
                        }
                        b"section" if kind == OfficeKind::Odt => {
                            if section_depth != depth || paragraph_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            finish_odf_unit(
                                &mut units,
                                &mut current,
                                &mut current_label,
                                part,
                                kind,
                                budget,
                            )?;
                            section_depth = 0;
                        }
                        b"tab" | b"line-break" => return Err(OfficeError::MalformedXml),
                        _ => {}
                    }
                }
                if foreign_depth == 0 {
                    match kind {
                        OfficeKind::Ods if namespace == NamespaceTag::OdfTable => {
                            match element.local_name().as_ref() {
                                b"table-cell" | b"covered-table-cell" => {
                                    if cell_depth != depth || paragraph_depth != 0 {
                                        return Err(OfficeError::MalformedXml);
                                    }
                                    cell_depth = 0;
                                }
                                b"table-row" => {
                                    if row_depth != depth || cell_depth != 0 {
                                        return Err(OfficeError::MalformedXml);
                                    }
                                    row_depth = 0;
                                }
                                b"table" => {
                                    if active_depth != depth
                                        || row_depth != 0
                                        || cell_depth != 0
                                        || paragraph_depth != 0
                                    {
                                        return Err(OfficeError::MalformedXml);
                                    }
                                    finish_odf_unit(
                                        &mut units,
                                        &mut current,
                                        &mut current_label,
                                        part,
                                        kind,
                                        budget,
                                    )?;
                                    active_depth = 0;
                                }
                                _ => {}
                            }
                        }
                        OfficeKind::Odp if namespace == NamespaceTag::OdfDraw => {
                            match element.local_name().as_ref() {
                                b"text-box" => {
                                    if text_box_depth != depth || paragraph_depth != 0 {
                                        return Err(OfficeError::MalformedXml);
                                    }
                                    text_box_depth = 0;
                                }
                                b"frame" => {
                                    if frame_depth != depth || text_box_depth != 0 {
                                        return Err(OfficeError::MalformedXml);
                                    }
                                    frame_depth = 0;
                                }
                                b"page" => {
                                    if active_depth != depth
                                        || frame_depth != 0
                                        || text_box_depth != 0
                                        || paragraph_depth != 0
                                    {
                                        return Err(OfficeError::MalformedXml);
                                    }
                                    finish_odf_unit(
                                        &mut units,
                                        &mut current,
                                        &mut current_label,
                                        part,
                                        kind,
                                        budget,
                                    )?;
                                    active_depth = 0;
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
                if foreign_depth == depth {
                    foreign_depth = 0;
                }
                if namespace == NamespaceTag::OdfOffice && typed_body_depth == depth {
                    typed_body_depth = 0;
                }
                if namespace == NamespaceTag::OdfOffice && body_depth == depth {
                    body_depth = 0;
                }
                depth -= 1;
            }
            Event::DocType(_) => return Err(OfficeError::XmlDoctype),
            Event::PI(_) => return Err(OfficeError::ActiveContent),
            Event::Eof => break,
            _ => {}
        }
    }
    if !root_seen
        || depth != 0
        || paragraph_depth != 0
        || foreign_depth != 0
        || section_depth != 0
        || active_depth != 0
        || row_depth != 0
        || cell_depth != 0
        || frame_depth != 0
        || text_box_depth != 0
        || body_depth != 0
        || typed_body_depth != 0
        || !body_seen
        || !typed_body_seen
    {
        return Err(OfficeError::MalformedXml);
    }
    if kind == OfficeKind::Odt && (!current.is_empty() || units.is_empty()) {
        finish_odf_unit(
            &mut units,
            &mut current,
            &mut current_label,
            part,
            kind,
            budget,
        )?;
    }
    if units.is_empty() {
        return Err(OfficeError::MissingPart);
    }
    Ok(units)
}

fn finish_odf_unit(
    units: &mut Vec<UnitDraft>,
    current: &mut String,
    label: &mut Option<String>,
    part: &str,
    kind: OfficeKind,
    budget: &ParseBudget<'_>,
) -> Result<(), OfficeError> {
    if units.len() >= budget.limits.max_units {
        return Err(OfficeError::UnitLimit);
    }
    let ordinal = units.len() + 1;
    let label = label
        .take()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("{} {ordinal}", kind.unit_label()));
    let text = normalize_text(std::mem::take(current));
    let mut size = ModelSizer::default();
    size.add(std::mem::size_of::<UnitDraft>().saturating_add(64))?;
    size.string(&label)?;
    size.string_bytes(part.len())?;
    budget.retain_model(size.bytes)?;
    units.try_reserve(1).map_err(|_| OfficeError::UnitLimit)?;
    units.push(UnitDraft {
        label,
        part: part.to_owned(),
        text,
    });
    Ok(())
}

fn parse_epub_container(
    bytes: &[u8],
    budget: &mut ParseBudget<'_>,
) -> Result<Vec<String>, OfficeError> {
    let mut reader = bounded_xml_reader(bytes, budget.limits);
    let mut depth = 0_usize;
    let mut root_seen = false;
    let mut rootfiles_seen = false;
    let mut rootfiles_depth = 0_usize;
    let mut rootfiles = Vec::new();
    loop {
        let (resolved, event) = reader
            .read_resolved_event()
            .map_err(|_| OfficeError::MalformedXml)?;
        let namespace = namespace_tag(resolved);
        budget.event_with_attributes(&reader, &event)?;
        if namespace == NamespaceTag::Unknown {
            return Err(OfficeError::UnsupportedNamespace);
        }
        match event {
            Event::Start(element) => {
                depth = depth.checked_add(1).ok_or(OfficeError::XmlNestingLimit)?;
                validate_depth(depth, budget.limits)?;
                if depth == 1 {
                    if namespace != NamespaceTag::EpubContainer
                        || local_name(&element) != b"container"
                    {
                        return Err(OfficeError::FormatMismatch);
                    }
                    if required_attribute(
                        &reader,
                        &element,
                        NamespaceTag::Unbound,
                        b"version",
                        budget.limits.max_string_bytes,
                    )? != "1.0"
                    {
                        return Err(OfficeError::FormatMismatch);
                    }
                    root_seen = true;
                } else if namespace == NamespaceTag::EpubContainer {
                    match local_name(&element) {
                        b"rootfiles" => {
                            if depth != 2 || rootfiles_seen || rootfiles_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            rootfiles_seen = true;
                            rootfiles_depth = depth;
                        }
                        b"rootfile" => {
                            if rootfiles_depth == 0 || depth != rootfiles_depth + 1 {
                                return Err(OfficeError::MalformedXml);
                            }
                            push_epub_rootfile(&reader, &element, &mut rootfiles, budget)?;
                        }
                        _ => {}
                    }
                }
            }
            Event::Empty(element) => {
                if namespace == NamespaceTag::EpubContainer {
                    match local_name(&element) {
                        b"rootfiles" => {
                            if depth != 1 || rootfiles_seen {
                                return Err(OfficeError::MalformedXml);
                            }
                            rootfiles_seen = true;
                        }
                        b"rootfile" => {
                            if rootfiles_depth == 0 || depth != rootfiles_depth {
                                return Err(OfficeError::MalformedXml);
                            }
                            push_epub_rootfile(&reader, &element, &mut rootfiles, budget)?;
                        }
                        _ => {}
                    }
                }
            }
            Event::End(element) => {
                if depth == 0 {
                    return Err(OfficeError::MalformedXml);
                }
                if namespace == NamespaceTag::EpubContainer
                    && element.local_name().as_ref() == b"rootfiles"
                    && rootfiles_depth == depth
                {
                    rootfiles_depth = 0;
                }
                depth -= 1;
            }
            Event::GeneralRef(reference) => validate_ignored_reference(&reference)?,
            Event::DocType(_) => return Err(OfficeError::XmlDoctype),
            Event::PI(_) => return Err(OfficeError::ActiveContent),
            Event::Eof => break,
            _ => {}
        }
    }
    if !root_seen || !rootfiles_seen || depth != 0 || rootfiles_depth != 0 || rootfiles.is_empty() {
        return Err(OfficeError::MalformedXml);
    }
    Ok(rootfiles)
}

fn push_epub_rootfile(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    rootfiles: &mut Vec<String>,
    budget: &mut ParseBudget<'_>,
) -> Result<(), OfficeError> {
    budget.relationship()?;
    let media_type = required_attribute(
        reader,
        element,
        NamespaceTag::Unbound,
        b"media-type",
        budget.limits.max_string_bytes,
    )?;
    if media_type != "application/oebps-package+xml" {
        return Err(OfficeError::FormatMismatch);
    }
    let raw_path = required_attribute(
        reader,
        element,
        NamespaceTag::Unbound,
        b"full-path",
        budget.limits.max_string_bytes,
    )?;
    let path = resolve_package_target(
        None,
        &raw_path,
        false,
        false,
        budget.limits.max_string_bytes,
    )?;
    if rootfiles.contains(&path) {
        return Err(OfficeError::MalformedXml);
    }
    let mut size = ModelSizer::default();
    size.add(std::mem::size_of::<String>().saturating_add(16))?;
    size.string(&path)?;
    budget.retain_model(size.bytes)?;
    rootfiles
        .try_reserve(1)
        .map_err(|_| OfficeError::RelationshipLimit)?;
    rootfiles.push(path);
    Ok(())
}

fn parse_opf(
    path: &str,
    bytes: &[u8],
    budget: &mut ParseBudget<'_>,
) -> Result<OpfDraft, OfficeError> {
    let mut reader = bounded_xml_reader(bytes, budget.limits);
    let mut depth = 0_usize;
    let mut root_seen = false;
    let mut metadata_depth = 0_usize;
    let mut metadata_seen = false;
    let mut manifest_depth = 0_usize;
    let mut manifest_seen = false;
    let mut spine_depth = 0_usize;
    let mut spine_seen = false;
    let mut title_depth = 0_usize;
    let mut title_foreign_depth = 0_usize;
    let mut title = String::new();
    let mut draft = OpfDraft::default();
    loop {
        let (resolved, event) = reader
            .read_resolved_event()
            .map_err(|_| OfficeError::MalformedXml)?;
        let namespace = namespace_tag(resolved);
        budget.event_with_attributes(&reader, &event)?;
        if namespace == NamespaceTag::Unknown {
            return Err(OfficeError::UnsupportedNamespace);
        }
        match event {
            Event::Start(element) => {
                depth = depth.checked_add(1).ok_or(OfficeError::XmlNestingLimit)?;
                validate_depth(depth, budget.limits)?;
                if depth == 1 {
                    if namespace != NamespaceTag::Opf || local_name(&element) != b"package" {
                        return Err(OfficeError::FormatMismatch);
                    }
                    let version = required_attribute(
                        &reader,
                        &element,
                        NamespaceTag::Unbound,
                        b"version",
                        budget.limits.max_string_bytes,
                    )?;
                    if !matches!(version.as_str(), "2.0" | "3.0") {
                        return Err(OfficeError::FormatMismatch);
                    }
                    root_seen = true;
                }
                if namespace == NamespaceTag::Opf && local_name(&element) == b"script" {
                    return Err(OfficeError::ActiveContent);
                }
                if title_depth > 0 && depth > title_depth {
                    if title_foreign_depth == 0 {
                        if namespace == NamespaceTag::Dc {
                            return Err(OfficeError::MalformedXml);
                        }
                        title_foreign_depth = depth;
                    }
                } else if depth > 1 && namespace == NamespaceTag::Opf {
                    match local_name(&element) {
                        b"metadata" => {
                            if depth != 2
                                || metadata_seen
                                || metadata_depth != 0
                                || manifest_seen
                                || spine_seen
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            metadata_seen = true;
                            metadata_depth = depth;
                        }
                        b"manifest" => {
                            if depth != 2
                                || !metadata_seen
                                || metadata_depth != 0
                                || manifest_seen
                                || manifest_depth != 0
                                || spine_seen
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            manifest_seen = true;
                            manifest_depth = depth;
                        }
                        b"spine" => {
                            if depth != 2
                                || !manifest_seen
                                || manifest_depth != 0
                                || spine_seen
                                || spine_depth != 0
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            spine_seen = true;
                            spine_depth = depth;
                        }
                        b"item" => {
                            if manifest_depth == 0 || depth != manifest_depth + 1 {
                                return Err(OfficeError::MalformedXml);
                            }
                            push_opf_manifest_item(path, &reader, &element, &mut draft, budget)?;
                        }
                        b"itemref" => {
                            if spine_depth == 0 || depth != spine_depth + 1 {
                                return Err(OfficeError::MalformedXml);
                            }
                            push_opf_spine_item(&reader, &element, &mut draft, budget)?;
                        }
                        _ => {}
                    }
                } else if namespace == NamespaceTag::Dc
                    && local_name(&element) == b"title"
                    && metadata_depth > 0
                {
                    if depth != metadata_depth + 1 || title_depth != 0 {
                        return Err(OfficeError::MalformedXml);
                    }
                    title_depth = depth;
                } else if namespace == NamespaceTag::Dc && local_name(&element) == b"title" {
                    return Err(OfficeError::MalformedXml);
                }
            }
            Event::Empty(element) => {
                if namespace == NamespaceTag::Opf && local_name(&element) == b"script" {
                    return Err(OfficeError::ActiveContent);
                }
                if title_depth > 0 {
                    if title_foreign_depth == 0 && namespace == NamespaceTag::Dc {
                        return Err(OfficeError::MalformedXml);
                    }
                } else if namespace == NamespaceTag::Opf {
                    match local_name(&element) {
                        b"metadata" => {
                            return Err(OfficeError::MalformedXml);
                        }
                        b"manifest" => {
                            if depth != 1
                                || !metadata_seen
                                || metadata_depth != 0
                                || manifest_seen
                                || spine_seen
                            {
                                return Err(OfficeError::MalformedXml);
                            }
                            manifest_seen = true;
                        }
                        b"spine" => {
                            if depth != 1 || !manifest_seen || manifest_depth != 0 || spine_seen {
                                return Err(OfficeError::MalformedXml);
                            }
                            spine_seen = true;
                        }
                        b"item" => {
                            if manifest_depth == 0 || depth != manifest_depth {
                                return Err(OfficeError::MalformedXml);
                            }
                            push_opf_manifest_item(path, &reader, &element, &mut draft, budget)?;
                        }
                        b"itemref" => {
                            if spine_depth == 0 || depth != spine_depth {
                                return Err(OfficeError::MalformedXml);
                            }
                            push_opf_spine_item(&reader, &element, &mut draft, budget)?;
                        }
                        _ => {}
                    }
                }
            }
            Event::Text(text) if title_depth > 0 && title_foreign_depth == 0 => {
                append_text(&mut title, &decoded_text(&text)?, budget)?;
            }
            Event::CData(text) if title_depth > 0 && title_foreign_depth == 0 => {
                append_text(&mut title, &decoded_cdata(&text)?, budget)?;
            }
            Event::GeneralRef(reference) if title_depth > 0 && title_foreign_depth == 0 => {
                append_reference(&mut title, &reference, budget)?;
            }
            Event::GeneralRef(reference) => validate_ignored_reference(&reference)?,
            Event::End(element) => {
                if depth == 0 {
                    return Err(OfficeError::MalformedXml);
                }
                if title_foreign_depth == 0
                    && namespace == NamespaceTag::Dc
                    && element.local_name().as_ref() == b"title"
                    && title_depth == depth
                {
                    title_depth = 0;
                }
                if title_foreign_depth == 0 && namespace == NamespaceTag::Opf {
                    match element.local_name().as_ref() {
                        b"metadata" if metadata_depth == depth => metadata_depth = 0,
                        b"manifest" if manifest_depth == depth => manifest_depth = 0,
                        b"spine" if spine_depth == depth => spine_depth = 0,
                        _ => {}
                    }
                }
                if title_foreign_depth == depth {
                    title_foreign_depth = 0;
                }
                depth -= 1;
            }
            Event::DocType(_) => return Err(OfficeError::XmlDoctype),
            Event::PI(_) => return Err(OfficeError::ActiveContent),
            Event::Eof => break,
            _ => {}
        }
    }
    if !root_seen
        || !metadata_seen
        || !manifest_seen
        || !spine_seen
        || depth != 0
        || metadata_depth != 0
        || manifest_depth != 0
        || spine_depth != 0
        || title_depth != 0
        || title_foreign_depth != 0
        || draft.spine.is_empty()
    {
        return Err(OfficeError::MalformedXml);
    }
    let title = normalize_text(title);
    if !title.is_empty() {
        let mut size = ModelSizer::default();
        size.string(&title)?;
        budget.retain_model(size.bytes)?;
        draft.title = Some(title);
    }
    Ok(draft)
}

fn push_opf_manifest_item(
    opf_path: &str,
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    draft: &mut OpfDraft,
    budget: &mut ParseBudget<'_>,
) -> Result<(), OfficeError> {
    budget.relationship()?;
    let id = required_attribute(
        reader,
        element,
        NamespaceTag::Unbound,
        b"id",
        budget.limits.max_string_bytes,
    )?;
    let href = required_attribute(
        reader,
        element,
        NamespaceTag::Unbound,
        b"href",
        budget.limits.max_string_bytes,
    )?;
    let media_type = required_attribute(
        reader,
        element,
        NamespaceTag::Unbound,
        b"media-type",
        budget.limits.max_string_bytes,
    )?;
    let properties = decoded_attribute(
        reader,
        element,
        NamespaceTag::Unbound,
        b"properties",
        budget.limits.max_string_bytes,
    )?
    .unwrap_or_default();
    if properties
        .split_ascii_whitespace()
        .any(|value| value == "scripted")
    {
        return Err(OfficeError::ActiveContent);
    }
    if id.is_empty() || draft.manifest_ids.contains(&id) {
        return Err(OfficeError::MalformedXml);
    }
    let mut id_size = ModelSizer::default();
    id_size.add(64)?;
    id_size.string(&id)?;
    budget.retain_model(id_size.bytes)?;
    draft.manifest_ids.insert(id.clone());
    if href.starts_with("//") || has_uri_scheme(&href) {
        draft.external_items = draft
            .external_items
            .checked_add(1)
            .ok_or(OfficeError::RelationshipLimit)?;
        return Ok(());
    }
    let path = resolve_package_target(
        Some(opf_path),
        &href,
        true,
        false,
        budget.limits.max_string_bytes,
    )?;
    let mut size = ModelSizer::default();
    size.add(std::mem::size_of::<ManifestItem>().saturating_add(96))?;
    size.string(&id)?;
    size.string(&path)?;
    size.string(&media_type)?;
    budget.retain_model(size.bytes)?;
    draft.manifest.insert(id, ManifestItem { path, media_type });
    Ok(())
}

fn push_opf_spine_item(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    draft: &mut OpfDraft,
    budget: &ParseBudget<'_>,
) -> Result<(), OfficeError> {
    if draft.spine.len() >= budget.limits.max_units {
        return Err(OfficeError::UnitLimit);
    }
    let id = required_attribute(
        reader,
        element,
        NamespaceTag::Unbound,
        b"idref",
        budget.limits.max_string_bytes,
    )?;
    if id.is_empty() || draft.spine.contains(&id) {
        return Err(OfficeError::MalformedXml);
    }
    let mut size = ModelSizer::default();
    size.add(std::mem::size_of::<String>().saturating_add(16))?;
    size.string(&id)?;
    budget.retain_model(size.bytes)?;
    draft
        .spine
        .try_reserve(1)
        .map_err(|_| OfficeError::UnitLimit)?;
    draft.spine.push(id);
    Ok(())
}

fn parse_xhtml(
    path: &str,
    bytes: &[u8],
    budget: &mut ParseBudget<'_>,
) -> Result<XhtmlDraft, OfficeError> {
    let mut reader = bounded_xml_reader(bytes, budget.limits);
    let mut depth = 0_usize;
    let mut root_seen = false;
    let mut head_seen = false;
    let mut head_depth = 0_usize;
    let mut body_seen = false;
    let mut body_depth = 0_usize;
    let mut title_depth = 0_usize;
    let mut title_foreign_depth = 0_usize;
    let mut ignored_depth = 0_usize;
    let mut foreign_depth = 0_usize;
    let mut output = String::new();
    let mut title = String::new();
    let mut links = Vec::new();
    let mut external_links = 0_usize;
    loop {
        let (resolved, event) = reader
            .read_resolved_event()
            .map_err(|_| OfficeError::MalformedXml)?;
        let namespace = namespace_tag(resolved);
        budget.event_with_attributes(&reader, &event)?;
        if namespace == NamespaceTag::Unknown {
            return Err(OfficeError::UnsupportedNamespace);
        }
        match event {
            Event::Start(element) => {
                depth = depth.checked_add(1).ok_or(OfficeError::XmlNestingLimit)?;
                validate_depth(depth, budget.limits)?;
                if depth == 1 {
                    if namespace != NamespaceTag::Xhtml || local_name(&element) != b"html" {
                        return Err(OfficeError::FormatMismatch);
                    }
                    root_seen = true;
                }
                if matches!(
                    local_name(&element),
                    b"script" | b"object" | b"embed" | b"iframe"
                ) {
                    return Err(OfficeError::ActiveContent);
                }
                if namespace == NamespaceTag::Xhtml && local_name(&element) == b"base" {
                    return Err(OfficeError::InvalidRelationship);
                }
                if title_depth > 0 && depth > title_depth && title_foreign_depth == 0 {
                    if namespace == NamespaceTag::Xhtml {
                        return Err(OfficeError::MalformedXml);
                    }
                    title_foreign_depth = depth;
                }
                if namespace == NamespaceTag::Xhtml && title_foreign_depth == 0 {
                    match local_name(&element) {
                        b"head" => {
                            if depth != 2 || head_seen || head_depth != 0 || body_seen {
                                return Err(OfficeError::MalformedXml);
                            }
                            head_seen = true;
                            head_depth = depth;
                        }
                        b"body" => {
                            if depth != 2 || body_seen || body_depth != 0 || head_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            body_seen = true;
                            body_depth = depth;
                        }
                        b"title" => {
                            if head_depth == 0 || depth != head_depth + 1 || title_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            title_depth = depth;
                        }
                        b"style" if ignored_depth == 0 => ignored_depth = depth,
                        b"a" => {
                            if body_depth == 0 || foreign_depth != 0 || ignored_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            push_xhtml_link(
                                path,
                                &reader,
                                &element,
                                &mut links,
                                &mut external_links,
                                budget,
                            )?;
                        }
                        _ => {}
                    }
                }
                if body_depth > 0 && namespace != NamespaceTag::Xhtml && foreign_depth == 0 {
                    foreign_depth = depth;
                }
            }
            Event::Empty(element) => {
                if matches!(
                    local_name(&element),
                    b"script" | b"object" | b"embed" | b"iframe"
                ) {
                    return Err(OfficeError::ActiveContent);
                }
                if namespace == NamespaceTag::Xhtml && local_name(&element) == b"base" {
                    return Err(OfficeError::InvalidRelationship);
                }
                if title_depth > 0 {
                    if title_foreign_depth == 0 && namespace == NamespaceTag::Xhtml {
                        return Err(OfficeError::MalformedXml);
                    }
                } else if namespace == NamespaceTag::Xhtml {
                    match local_name(&element) {
                        b"head" => {
                            if depth != 1 || head_seen || body_seen {
                                return Err(OfficeError::MalformedXml);
                            }
                            head_seen = true;
                        }
                        b"body" => {
                            if depth != 1 || body_seen || head_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            body_seen = true;
                        }
                        b"title" => return Err(OfficeError::MalformedXml),
                        b"br" | b"hr"
                            if body_depth > 0 && ignored_depth == 0 && foreign_depth == 0 =>
                        {
                            append_separator(&mut output, "\n", budget)?
                        }
                        b"a" => {
                            if body_depth == 0 || foreign_depth != 0 || ignored_depth != 0 {
                                return Err(OfficeError::MalformedXml);
                            }
                            push_xhtml_link(
                                path,
                                &reader,
                                &element,
                                &mut links,
                                &mut external_links,
                                budget,
                            )?;
                        }
                        _ => {}
                    }
                }
            }
            Event::Text(text) => {
                let text = decoded_text(&text)?;
                if title_depth > 0 && title_foreign_depth == 0 {
                    append_text(&mut title, &text, budget)?;
                }
                if body_depth > 0 && ignored_depth == 0 && foreign_depth == 0 {
                    append_text(&mut output, &text, budget)?;
                }
            }
            Event::CData(text) => {
                let text = decoded_cdata(&text)?;
                if title_depth > 0 && title_foreign_depth == 0 {
                    append_text(&mut title, &text, budget)?;
                }
                if body_depth > 0 && ignored_depth == 0 && foreign_depth == 0 {
                    append_text(&mut output, &text, budget)?;
                }
            }
            Event::GeneralRef(reference) => {
                let mut retained = false;
                if title_depth > 0 && title_foreign_depth == 0 {
                    append_reference(&mut title, &reference, budget)?;
                    retained = true;
                }
                if body_depth > 0 && ignored_depth == 0 && foreign_depth == 0 {
                    append_reference(&mut output, &reference, budget)?;
                    retained = true;
                }
                if !retained {
                    validate_ignored_reference(&reference)?;
                }
            }
            Event::End(element) => {
                if depth == 0 {
                    return Err(OfficeError::MalformedXml);
                }
                if namespace == NamespaceTag::Xhtml && title_foreign_depth == 0 {
                    match element.local_name().as_ref() {
                        b"title" if title_depth == depth => title_depth = 0,
                        b"style" if ignored_depth == depth => ignored_depth = 0,
                        b"body" if body_depth == depth => body_depth = 0,
                        b"head" if head_depth == depth => head_depth = 0,
                        b"p" | b"div" | b"h1" | b"h2" | b"h3" | b"h4" | b"h5" | b"h6" | b"li"
                        | b"tr" | b"section" | b"article" => {
                            if body_depth > 0 && ignored_depth == 0 && foreign_depth == 0 {
                                append_separator(&mut output, "\n", budget)?;
                            }
                        }
                        b"td" | b"th"
                            if body_depth > 0 && ignored_depth == 0 && foreign_depth == 0 =>
                        {
                            append_text(&mut output, "\t", budget)?
                        }
                        _ => {}
                    }
                }
                if title_foreign_depth == depth {
                    title_foreign_depth = 0;
                }
                if foreign_depth == depth {
                    foreign_depth = 0;
                }
                depth -= 1;
            }
            Event::DocType(_) => return Err(OfficeError::XmlDoctype),
            Event::PI(_) => return Err(OfficeError::ActiveContent),
            Event::Eof => break,
            _ => {}
        }
    }
    if !root_seen
        || !body_seen
        || depth != 0
        || head_depth != 0
        || body_depth != 0
        || title_depth != 0
        || title_foreign_depth != 0
        || ignored_depth != 0
        || foreign_depth != 0
    {
        return Err(OfficeError::MalformedXml);
    }
    let title = normalize_text(title);
    let title = if title.is_empty() {
        None
    } else {
        let mut size = ModelSizer::default();
        size.string(&title)?;
        budget.retain_model(size.bytes)?;
        Some(title)
    };
    Ok(XhtmlDraft {
        title,
        text: normalize_text(output),
        links,
        external_links,
    })
}

fn push_xhtml_link(
    source_path: &str,
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    links: &mut Vec<String>,
    external_links: &mut usize,
    budget: &mut ParseBudget<'_>,
) -> Result<(), OfficeError> {
    let Some(href) = decoded_attribute(
        reader,
        element,
        NamespaceTag::Unbound,
        b"href",
        budget.limits.max_string_bytes,
    )?
    else {
        return Ok(());
    };
    budget.relationship()?;
    if href.starts_with("//") || has_uri_scheme(&href) {
        *external_links = external_links
            .checked_add(1)
            .ok_or(OfficeError::RelationshipLimit)?;
        return Ok(());
    }
    let target = resolve_package_target(
        Some(source_path),
        &href,
        true,
        false,
        budget.limits.max_string_bytes,
    )?;
    let mut size = ModelSizer::default();
    size.add(std::mem::size_of::<String>().saturating_add(16))?;
    size.string(&target)?;
    budget.retain_model(size.bytes)?;
    links
        .try_reserve(1)
        .map_err(|_| OfficeError::RelationshipLimit)?;
    links.push(target);
    Ok(())
}

fn finalize_package(
    kind: OfficeKind,
    mut scratch: PackageScratch,
    members: &BTreeSet<String>,
    budget: &mut ParseBudget<'_>,
) -> Result<FinalizedPackage, OfficeError> {
    let mut title = None;
    let mut units = Vec::new();
    if kind.is_ooxml() {
        let content_types = scratch
            .content_types
            .as_ref()
            .ok_or(OfficeError::MissingPart)?;
        let (main_part, main_content_type) = match kind {
            OfficeKind::Docx => (
                "word/document.xml",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
            ),
            OfficeKind::Xlsx => (
                "xl/workbook.xml",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml",
            ),
            OfficeKind::Pptx => (
                "ppt/presentation.xml",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
            ),
            _ => return Err(OfficeError::FormatMismatch),
        };
        let root_documents = scratch
            .relationships
            .iter()
            .filter(|relationship| {
                relationship.source_part.is_none() && relationship.kind == "office_document"
            })
            .collect::<Vec<_>>();
        if !members.contains("[Content_Types].xml")
            || !members.contains(main_part)
            || content_types.overrides.get(main_part).map(String::as_str) != Some(main_content_type)
            || root_documents.len() != 1
            || root_documents[0].target_part != main_part
        {
            return Err(OfficeError::FormatMismatch);
        }

        match kind {
            OfficeKind::Docx => {
                units = scratch
                    .docx_sections
                    .take()
                    .ok_or(OfficeError::MissingPart)?;
            }
            OfficeKind::Xlsx => {
                let pending = scratch.xlsx_sheets.take().ok_or(OfficeError::MissingPart)?;
                let relationship_map = relationship_id_map(
                    &scratch.relationships,
                    Some("xl/workbook.xml"),
                    "worksheet",
                )?;
                let mut seen_parts = BTreeSet::new();
                for sheet in pending {
                    let part = relationship_map
                        .get(&sheet.relationship_id)
                        .ok_or(OfficeError::InvalidRelationship)?;
                    if !seen_parts.insert(part.clone()) {
                        return Err(OfficeError::InvalidRelationship);
                    }
                    let text = scratch
                        .worksheets
                        .remove(part)
                        .ok_or(OfficeError::MissingPart)?;
                    retain_generated_unit_model(budget, &sheet.label, part)?;
                    units.push(UnitDraft {
                        label: sheet.label,
                        part: part.clone(),
                        text,
                    });
                }
            }
            OfficeKind::Pptx => {
                let pending = scratch.pptx_slides.take().ok_or(OfficeError::MissingPart)?;
                let relationship_map = relationship_id_map(
                    &scratch.relationships,
                    Some("ppt/presentation.xml"),
                    "slide",
                )?;
                let mut seen_parts = BTreeSet::new();
                for (index, slide) in pending.into_iter().enumerate() {
                    let part = relationship_map
                        .get(&slide.relationship_id)
                        .ok_or(OfficeError::InvalidRelationship)?;
                    if !seen_parts.insert(part.clone()) {
                        return Err(OfficeError::InvalidRelationship);
                    }
                    let text = scratch
                        .slide_text
                        .remove(part)
                        .ok_or(OfficeError::MissingPart)?;
                    let label = format!("Slide {}", index + 1);
                    retain_generated_unit_model(budget, &label, part)?;
                    units.push(UnitDraft {
                        label,
                        part: part.clone(),
                        text,
                    });
                }
            }
            _ => return Err(OfficeError::FormatMismatch),
        }
    } else if kind.is_odf() {
        let expected_mime = match kind {
            OfficeKind::Odt => "application/vnd.oasis.opendocument.text",
            OfficeKind::Ods => "application/vnd.oasis.opendocument.spreadsheet",
            OfficeKind::Odp => "application/vnd.oasis.opendocument.presentation",
            _ => return Err(OfficeError::FormatMismatch),
        };
        let manifest = scratch
            .odf_manifest
            .take()
            .ok_or(OfficeError::MissingPart)?;
        if scratch.mimetype.as_deref() != Some(expected_mime)
            || manifest.root_media_type.as_deref() != Some(expected_mime)
            || !manifest.entries.contains("content.xml")
            || !members.contains("mimetype")
            || !members.contains("content.xml")
            || !members.contains("META-INF/manifest.xml")
        {
            return Err(OfficeError::FormatMismatch);
        }
        scratch.relationships.extend(manifest.relationships);
        units = scratch.odf_units.take().ok_or(OfficeError::MissingPart)?;
    } else if kind == OfficeKind::Epub {
        if scratch.mimetype.as_deref() != Some("application/epub+zip")
            || !members.contains("mimetype")
            || !members.contains("META-INF/container.xml")
            || scratch.epub_rootfiles.len() != 1
        {
            return Err(OfficeError::FormatMismatch);
        }
        let opf_path = scratch.epub_rootfiles.remove(0);
        let opf = scratch
            .opfs
            .remove(&opf_path)
            .ok_or(OfficeError::MissingPart)?;
        title = opf.title;
        retain_generated_relationship_model(budget, None, "container:000001".len(), &opf_path)?;
        scratch.relationships.push(RelationshipDraft {
            source_part: None,
            id: "container:000001".into(),
            target_part: opf_path.clone(),
            kind: "publication",
        });
        scratch.external_relationships = scratch
            .external_relationships
            .checked_add(opf.external_items)
            .ok_or(OfficeError::RelationshipLimit)?;
        let mut spine_paths = BTreeSet::new();
        for id in &opf.spine {
            let item = opf.manifest.get(id).ok_or(OfficeError::MissingPart)?;
            if item.media_type != "application/xhtml+xml" || !spine_paths.insert(item.path.clone())
            {
                return Err(OfficeError::FormatMismatch);
            }
            let xhtml = scratch
                .xhtml
                .get(&item.path)
                .ok_or(OfficeError::MissingPart)?;
            let label = xhtml
                .title
                .clone()
                .unwrap_or_else(|| format!("Spine item {}", units.len() + 1));
            retain_generated_unit_model(budget, &label, &item.path)?;
            units.push(UnitDraft {
                label,
                part: item.path.clone(),
                text: xhtml.text.clone(),
            });
        }
        for (id, item) in &opf.manifest {
            let relationship_id = format!("manifest:{id}");
            retain_generated_relationship_model(
                budget,
                Some(&opf_path),
                relationship_id.len(),
                &item.path,
            )?;
            scratch.relationships.push(RelationshipDraft {
                source_part: Some(opf_path.clone()),
                id: relationship_id,
                target_part: item.path.clone(),
                kind: "manifest",
            });
        }
        for source_part in &spine_paths {
            let xhtml = scratch
                .xhtml
                .get(source_part)
                .ok_or(OfficeError::MissingPart)?;
            scratch.external_relationships = scratch
                .external_relationships
                .checked_add(xhtml.external_links)
                .ok_or(OfficeError::RelationshipLimit)?;
            for (index, target_part) in xhtml.links.iter().enumerate() {
                let relationship_id = format!("link:{:06}", index + 1);
                retain_generated_relationship_model(
                    budget,
                    Some(source_part),
                    relationship_id.len(),
                    target_part,
                )?;
                scratch.relationships.push(RelationshipDraft {
                    source_part: Some(source_part.clone()),
                    id: relationship_id,
                    target_part: target_part.clone(),
                    kind: "hyperlink",
                });
            }
        }
    } else {
        return Err(OfficeError::FormatMismatch);
    }

    if units.is_empty() || units.len() > budget.limits.max_units {
        return Err(OfficeError::UnitLimit);
    }
    for unit in &units {
        if !members.contains(&unit.part)
            || unit.part.len() > budget.limits.max_string_bytes
            || unit.text.len() > budget.limits.max_text_bytes_per_unit
        {
            return Err(OfficeError::MissingPart);
        }
    }
    scratch.relationships.sort();
    scratch.relationships.dedup();
    if scratch.relationships.len() > budget.limits.max_relationships {
        return Err(OfficeError::RelationshipLimit);
    }
    for relationship in &scratch.relationships {
        if !members.contains(&relationship.target_part)
            || relationship
                .source_part
                .as_ref()
                .is_some_and(|source| !members.contains(source))
        {
            return Err(OfficeError::InvalidRelationship);
        }
    }
    Ok(FinalizedPackage {
        title,
        units,
        relationships: scratch.relationships,
        external_relationships: scratch.external_relationships,
    })
}

fn relationship_id_map(
    relationships: &[RelationshipDraft],
    source_part: Option<&str>,
    kind: &str,
) -> Result<BTreeMap<String, String>, OfficeError> {
    let mut map = BTreeMap::new();
    for relationship in relationships.iter().filter(|relationship| {
        relationship.source_part.as_deref() == source_part && relationship.kind == kind
    }) {
        if map
            .insert(relationship.id.clone(), relationship.target_part.clone())
            .is_some()
        {
            return Err(OfficeError::InvalidRelationship);
        }
    }
    Ok(map)
}

const MAX_SERIALIZED_FACT_BYTES: usize = 512 * 1024;

#[derive(Debug, Default)]
struct FactSizeWriter {
    bytes: usize,
}

impl io::Write for FactSizeWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let next = self
            .bytes
            .checked_add(buffer.len())
            .ok_or_else(|| io::Error::other("fact size overflow"))?;
        if next >= MAX_SERIALIZED_FACT_BYTES {
            return Err(io::Error::other("fact size limit"));
        }
        self.bytes = next;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn fact_within_size_limit(value: &impl serde::Serialize) -> bool {
    serde_json::to_writer(FactSizeWriter::default(), value).is_ok()
}

struct MaterializeRequest<'a> {
    path: &'a Path,
    source_file: &'a str,
    kind: OfficeKind,
    title: Option<String>,
    units: Vec<UnitDraft>,
    relationships: Vec<RelationshipDraft>,
    external_relationships: usize,
    member_count: usize,
    decompressed_bytes: u64,
    xml_events: usize,
    text_bytes: usize,
    limits: OfficeLimits,
}

fn materialize_extraction(request: MaterializeRequest<'_>) -> Result<Extraction, OfficeError> {
    let stem = Path::new(request.source_file)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    let root_id = make_id(&[&stem]);
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let unit_count = request.units.len();
    let mut root_extra = BTreeMap::from([
        ("_origin".into(), "document_package".into()),
        ("format".into(), request.kind.format().into()),
        ("format_capability".into(), "structural_partial".into()),
        ("parse_status".into(), "complete".into()),
        ("type".into(), request.kind.document_type().into()),
        ("unit_count".into(), unit_count.into()),
        ("member_count".into(), request.member_count.into()),
        (
            "internal_relationship_count".into(),
            request.relationships.len().into(),
        ),
        (
            "external_relationship_count".into(),
            request.external_relationships.into(),
        ),
        (
            "decompressed_bytes".into(),
            request.decompressed_bytes.into(),
        ),
        ("xml_event_count".into(), request.xml_events.into()),
        ("text_bytes".into(), request.text_bytes.into()),
    ]);
    if let Some(title) = &request.title {
        root_extra.insert("title".into(), title.clone().into());
    }
    push_node(
        &mut nodes,
        &edges,
        Node {
            id: root_id.clone(),
            label: request.title.clone().unwrap_or_else(|| {
                request
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(request.source_file)
                    .to_owned()
            }),
            file_type: "document".into(),
            source_file: request.source_file.into(),
            source_location: None,
            community: None,
            extra: root_extra,
        },
        request.limits,
    )?;

    let mut unit_nodes_by_part = BTreeMap::new();
    let mut unit_part_counts = BTreeMap::<String, usize>::new();
    for unit in &request.units {
        *unit_part_counts.entry(unit.part.clone()).or_default() += 1;
    }
    for (index, unit) in request.units.into_iter().enumerate() {
        let UnitDraft { label, part, text } = unit;
        let ordinal = index + 1;
        let ordinal_id = format!("{ordinal:06}");
        let unique_part = unit_part_counts.get(&part) == Some(&1);
        let unit_id = if unique_part {
            path_owned_id(&root_id, request.kind.unit_type(), &part)
        } else {
            make_id(&[&root_id, request.kind.unit_type(), &ordinal_id])
        };
        if unique_part {
            unit_nodes_by_part.insert(part.clone(), unit_id.clone());
        }
        push_node(
            &mut nodes,
            &edges,
            Node {
                id: unit_id.clone(),
                label,
                file_type: "document".into(),
                source_file: request.source_file.into(),
                source_location: None,
                community: None,
                extra: BTreeMap::from([
                    ("_origin".into(), "document_package".into()),
                    ("format".into(), request.kind.format().into()),
                    ("type".into(), request.kind.unit_type().into()),
                    ("unit_ordinal".into(), ordinal.into()),
                    ("internal_part".into(), part.into()),
                    ("text_bytes".into(), text.len().into()),
                    ("text".into(), text.into()),
                ]),
            },
            request.limits,
        )?;
        push_edge(
            &nodes,
            &mut edges,
            relationship_edge(
                &root_id,
                &unit_id,
                "contains",
                request.source_file,
                None,
                None,
            ),
            request.limits,
        )?;
    }

    let mut part_paths = BTreeSet::new();
    for relationship in &request.relationships {
        if let Some(source) = &relationship.source_part
            && !unit_nodes_by_part.contains_key(source)
        {
            part_paths.insert(source.clone());
        }
        if !unit_nodes_by_part.contains_key(&relationship.target_part) {
            part_paths.insert(relationship.target_part.clone());
        }
    }
    let mut part_nodes = BTreeMap::new();
    for part in part_paths {
        let part_id = path_owned_id(&root_id, "part", &part);
        part_nodes.insert(part.clone(), part_id.clone());
        push_node(
            &mut nodes,
            &edges,
            Node {
                id: part_id,
                label: part.rsplit('/').next().unwrap_or(&part).to_owned(),
                file_type: "document".into(),
                source_file: request.source_file.into(),
                source_location: None,
                community: None,
                extra: BTreeMap::from([
                    ("_origin".into(), "document_package".into()),
                    ("format".into(), request.kind.format().into()),
                    ("type".into(), "document_package_part".into()),
                    ("internal_part".into(), part.into()),
                ]),
            },
            request.limits,
        )?;
    }

    let mut relationship_evidence =
        BTreeMap::<(String, String), BTreeSet<(String, &'static str)>>::new();
    for relationship in &request.relationships {
        let source_id = relationship
            .source_part
            .as_ref()
            .and_then(|part| {
                unit_nodes_by_part
                    .get(part)
                    .or_else(|| part_nodes.get(part))
            })
            .unwrap_or(&root_id);
        let target_id = unit_nodes_by_part
            .get(&relationship.target_part)
            .or_else(|| part_nodes.get(&relationship.target_part))
            .ok_or(OfficeError::FactLimit)?;
        relationship_evidence
            .entry((source_id.clone(), target_id.clone()))
            .or_default()
            .insert((relationship.id.clone(), relationship.kind));
    }
    for ((source_id, target_id), evidence) in relationship_evidence {
        push_edge(
            &nodes,
            &mut edges,
            relationship_evidence_edge(&source_id, &target_id, request.source_file, evidence),
            request.limits,
        )?;
    }
    Ok(Extraction {
        nodes,
        edges,
        hyperedges: Vec::new(),
    })
}

fn relationship_evidence_edge(
    source: &str,
    target: &str,
    source_file: &str,
    evidence: BTreeSet<(String, &'static str)>,
) -> Edge {
    let mut edge = relationship_edge(source, target, "references", source_file, None, None);
    let ids = evidence
        .iter()
        .map(|(id, _)| id.clone())
        .collect::<BTreeSet<_>>();
    let kinds = evidence
        .iter()
        .map(|(_, kind)| *kind)
        .collect::<BTreeSet<_>>();
    if ids.len() == 1 {
        edge.extra.insert(
            "relationship_id".into(),
            ids.first().expect("one id").clone().into(),
        );
    }
    if kinds.len() == 1 {
        edge.extra.insert(
            "relationship_kind".into(),
            (*kinds.first().expect("one kind")).into(),
        );
    }
    edge.extra.insert(
        "relationship_ids".into(),
        serde_json::Value::Array(ids.into_iter().map(serde_json::Value::String).collect()),
    );
    edge.extra.insert(
        "relationship_kinds".into(),
        serde_json::Value::Array(
            kinds
                .into_iter()
                .map(|kind| serde_json::Value::String(kind.into()))
                .collect(),
        ),
    );
    edge.extra.insert(
        "relationship_evidence".into(),
        serde_json::Value::Array(
            evidence
                .into_iter()
                .map(|(id, kind)| {
                    serde_json::Value::Object(serde_json::Map::from_iter([
                        ("id".into(), serde_json::Value::String(id)),
                        ("kind".into(), serde_json::Value::String(kind.into())),
                    ]))
                })
                .collect(),
        ),
    );
    edge
}

fn path_owned_id(root_id: &str, role: &str, normalized_part: &str) -> String {
    let readable = make_id(&[root_id, role, normalized_part]);
    let digest = blake3::hash(normalized_part.as_bytes()).to_hex();
    format!("{readable}_{digest}")
}

fn relationship_edge(
    source: &str,
    target: &str,
    relation: &str,
    source_file: &str,
    kind: Option<&str>,
    id: Option<&str>,
) -> Edge {
    let mut extra = BTreeMap::from([
        ("_origin".into(), "document_package".into()),
        ("_src".into(), source.into()),
        ("_tgt".into(), target.into()),
    ]);
    if let Some(kind) = kind {
        extra.insert("relationship_kind".into(), kind.into());
    }
    if let Some(id) = id {
        extra.insert("relationship_id".into(), id.into());
    }
    Edge {
        source: source.into(),
        target: target.into(),
        relation: relation.into(),
        confidence: Confidence::Extracted,
        source_file: source_file.into(),
        extra,
    }
}

fn push_node(
    nodes: &mut Vec<Node>,
    edges: &[Edge],
    node: Node,
    limits: OfficeLimits,
) -> Result<(), OfficeError> {
    if nodes.len().saturating_add(edges.len()) >= limits.max_facts
        || !fact_within_size_limit(&node)
        || !crate::parser_budget::try_reserve_facts(1)
    {
        return Err(OfficeError::FactLimit);
    }
    nodes.try_reserve(1).map_err(|_| OfficeError::FactLimit)?;
    nodes.push(node);
    Ok(())
}

fn push_edge(
    nodes: &[Node],
    edges: &mut Vec<Edge>,
    edge: Edge,
    limits: OfficeLimits,
) -> Result<(), OfficeError> {
    if nodes.len().saturating_add(edges.len()) >= limits.max_facts
        || !fact_within_size_limit(&edge)
        || !crate::parser_budget::try_reserve_facts(1)
    {
        return Err(OfficeError::FactLimit);
    }
    edges.try_reserve(1).map_err(|_| OfficeError::FactLimit)?;
    edges.push(edge);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser_budget::{with_plan, ParserPlan};
    use std::{cell::Cell, fmt::Write as _, io::Write as _, path::Path};
    use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

    const CONTENT_TYPES: &str = r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;
    const ROOT_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;

    macro_rules! assert_office_error {
        ($expression:expr, $error:path) => {
            assert!(matches!($expression, Err($error)));
        };
    }

    fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let cursor = std::io::Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        for (name, bytes) in entries {
            writer
                .start_file(
                    *name,
                    SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
                )
                .expect("start member");
            writer.write_all(bytes).expect("write member");
        }
        writer.finish().expect("finish ZIP").into_inner()
    }

    fn docx(document: &str) -> Vec<u8> {
        zip_bytes(&[
            ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
            ("_rels/.rels", ROOT_RELS.as_bytes()),
            ("word/document.xml", document.as_bytes()),
        ])
    }

    fn parse_budget(limits: OfficeLimits) -> ParseBudget<'static> {
        ParseBudget::new(limits, None, ModelLedger::new(limits.max_model_bytes))
    }

    fn extract_docx(
        source: &[u8],
        limits: OfficeLimits,
        cancelled: Option<&dyn Fn() -> bool>,
    ) -> Result<Extraction, OfficeError> {
        let plan = ParserPlan::for_fact_limit(limits.max_facts).expect("positive fact limit");
        with_plan(plan, || {
            extract_office_bytes_with_admission(
                Path::new("fixture.docx"),
                "fixture.docx",
                source,
                OfficeKind::Docx,
                limits,
                cancelled,
                |_| true,
                |_| Some(()),
            )
        })
        .0
    }

    #[test]
    fn archive_input_member_and_decoded_limits_are_independent() {
        let source = docx(
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>safe</w:t></w:r></w:p></w:body></w:document>"#,
        );

        let limits = OfficeLimits {
            max_input_bytes: source.len() - 1,
            ..OfficeLimits::default()
        };
        assert_office_error!(extract_docx(&source, limits, None), OfficeError::InputLimit);

        let limits = OfficeLimits {
            max_members: 2,
            ..OfficeLimits::default()
        };
        assert_office_error!(
            extract_docx(&source, limits, None),
            OfficeError::ArchiveLimit
        );

        let limits = OfficeLimits {
            max_member_decoded_bytes: 32,
            ..OfficeLimits::default()
        };
        assert_office_error!(
            extract_docx(&source, limits, None),
            OfficeError::ArchiveLimit
        );

        let limits = OfficeLimits {
            max_total_decoded_bytes: 64,
            ..OfficeLimits::default()
        };
        assert_office_error!(
            extract_docx(&source, limits, None),
            OfficeError::ArchiveLimit
        );
    }

    #[test]
    fn xml_event_byte_attribute_and_depth_limits_are_independent() {
        let xml = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p a="1" b="2"/></w:body></w:document>"#;

        let limits = OfficeLimits {
            max_xml_events: 1,
            ..OfficeLimits::default()
        };
        assert_office_error!(
            validate_single_xml_document(xml, &mut parse_budget(limits)),
            OfficeError::XmlEventLimit
        );

        let limits = OfficeLimits {
            max_xml_event_bytes: 8,
            ..OfficeLimits::default()
        };
        assert_office_error!(
            validate_single_xml_document(xml, &mut parse_budget(limits)),
            OfficeError::XmlEventLimit
        );

        let limits = OfficeLimits {
            max_attributes_per_element: 1,
            ..OfficeLimits::default()
        };
        assert_office_error!(
            validate_single_xml_document(xml, &mut parse_budget(limits)),
            OfficeError::XmlAttributeLimit
        );

        let limits = OfficeLimits {
            max_nesting: 2,
            ..OfficeLimits::default()
        };
        assert_office_error!(
            validate_single_xml_document(xml, &mut parse_budget(limits)),
            OfficeError::XmlNestingLimit
        );

        let limits = OfficeLimits::default();
        let safe_attribute = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:e="urn:example" e:flag="A &amp; &#x2026;"><w:body/></w:document>"#;
        assert!(validate_single_xml_document(safe_attribute, &mut parse_budget(limits)).is_ok());
        let custom_attribute = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:e="urn:example" e:flag="&custom;"><w:body/></w:document>"#;
        assert_office_error!(
            validate_single_xml_document(custom_attribute, &mut parse_budget(limits)),
            OfficeError::XmlDoctype
        );
        let illegal_attribute = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:e="urn:example" e:flag="&#x1;"><w:body/></w:document>"#;
        assert_office_error!(
            validate_single_xml_document(illegal_attribute, &mut parse_budget(limits)),
            OfficeError::MalformedXml
        );
        for malformed in [
            br#"<?xml version="1.0"?><?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"/>"#.as_slice(),
            br#" <![CDATA[ ]]><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"/>"#.as_slice(),
            br#" &amp;<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"/>"#.as_slice(),
        ] {
            assert_office_error!(
                validate_single_xml_document(malformed, &mut parse_budget(limits)),
                OfficeError::MalformedXml
            );
        }
    }

    #[test]
    fn quick_xml_attribute_and_namespace_advisory_bounds_are_deterministic() {
        let mut duplicate = String::from(
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#,
        );
        for index in 0..40 {
            write!(duplicate, r#" a{index}="{index}""#).expect("append unique attribute");
        }
        duplicate.push_str(r#" a0="duplicate"/>"#);
        assert_office_error!(
            validate_single_xml_document(
                duplicate.as_bytes(),
                &mut parse_budget(OfficeLimits::default()),
            ),
            OfficeError::MalformedXml
        );

        let mut ordinary_overflow = String::from(
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#,
        );
        for index in 0..DEFAULT_MAX_DECLARATIONS_PER_ELEMENT {
            write!(ordinary_overflow, r#" a{index}="{index}""#).expect("append bounded attribute");
        }
        ordinary_overflow.push_str("/>");
        assert_office_error!(
            validate_single_xml_document(
                ordinary_overflow.as_bytes(),
                &mut parse_budget(OfficeLimits::default()),
            ),
            OfficeError::XmlAttributeLimit
        );

        let namespace_document = |extra_declarations: usize| {
            let mut xml = String::from(
                r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#,
            );
            for index in 0..extra_declarations {
                write!(xml, r#" xmlns:n{index}="urn:graphoxide:test:{index}""#)
                    .expect("append namespace declaration");
            }
            xml.push_str("/>");
            xml
        };
        let at_limit = namespace_document(DEFAULT_MAX_DECLARATIONS_PER_ELEMENT - 1);
        assert!(validate_single_xml_document(
            at_limit.as_bytes(),
            &mut parse_budget(OfficeLimits::default()),
        )
        .is_ok());

        let over_limit = namespace_document(DEFAULT_MAX_DECLARATIONS_PER_ELEMENT);
        assert_office_error!(
            extract_docx(&docx(&over_limit), OfficeLimits::default(), None),
            OfficeError::MalformedXml
        );
    }

    #[test]
    fn declared_foreign_subtrees_cannot_spoof_docx_or_odf_units() {
        let document = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:evil="urn:evil"><w:body><w:p><w:r><w:t>visible</w:t></w:r></w:p><evil:payload><w:t>DOCX_SENTINEL</w:t><w:sectPr/></evil:payload></w:body></w:document>"#;
        let docx_units = parse_docx(
            document,
            "word/document.xml",
            &mut parse_budget(OfficeLimits::default()),
        )
        .expect("declared extension subtree is inert");
        assert_eq!(docx_units.len(), 1);
        assert_eq!(docx_units[0].text, "visible");

        let spreadsheet = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:evil="urn:evil"><office:body><office:spreadsheet><evil:payload><table:table table:name="Hidden"><table:table-row><table:table-cell><text:p>ODS_SENTINEL</text:p></table:table-cell></table:table-row></table:table></evil:payload><table:table table:name="Visible"><table:table-row><table:table-cell><text:p>visible</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let ods_units = parse_odf_content(
            OfficeKind::Ods,
            spreadsheet,
            "content.xml",
            &mut parse_budget(OfficeLimits::default()),
        )
        .expect("declared extension subtree is inert");
        assert_eq!(ods_units.len(), 1);
        assert_eq!(ods_units[0].label, "Visible");
        assert_eq!(ods_units[0].text, "visible");

        let presentation = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:evil="urn:evil"><office:body><office:presentation><evil:payload><draw:page draw:name="Hidden"><draw:frame><draw:text-box><text:p>ODP_SENTINEL</text:p></draw:text-box></draw:frame></draw:page></evil:payload><draw:page draw:name="Visible"><draw:frame><draw:text-box><text:p>visible</text:p></draw:text-box></draw:frame></draw:page></office:presentation></office:body></office:document-content>"#;
        let odp_units = parse_odf_content(
            OfficeKind::Odp,
            presentation,
            "content.xml",
            &mut parse_budget(OfficeLimits::default()),
        )
        .expect("declared extension subtree is inert");
        assert_eq!(odp_units.len(), 1);
        assert_eq!(odp_units[0].label, "Visible");
        assert_eq!(odp_units[0].text, "visible");
    }

    #[test]
    fn odf_units_must_be_direct_typed_body_children() {
        let nested_table = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><text:p><table:table table:name="Spoofed"><table:table-row><table:table-cell><text:p>ODS_SENTINEL</text:p></table:table-cell></table:table-row></table:table></text:p></office:spreadsheet></office:body></office:document-content>"#;
        assert_office_error!(
            parse_odf_content(
                OfficeKind::Ods,
                nested_table,
                "content.xml",
                &mut parse_budget(OfficeLimits::default()),
            ),
            OfficeError::MalformedXml
        );

        let nested_page = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:presentation><draw:frame><draw:page draw:name="Spoofed"><text:p>ODP_SENTINEL</text:p></draw:page></draw:frame></office:presentation></office:body></office:document-content>"#;
        assert_office_error!(
            parse_odf_content(
                OfficeKind::Odp,
                nested_page,
                "content.xml",
                &mut parse_budget(OfficeLimits::default()),
            ),
            OfficeError::MalformedXml
        );
    }

    #[test]
    fn docx_and_odf_text_require_exact_semantic_owners() {
        for malformed in [
            br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:sectPr><w:t>DOCX_SENTINEL</w:t></w:sectPr></w:body></w:document>"#.as_slice(),
            br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:t>DOCX_SENTINEL</w:t></w:p></w:body></w:document>"#.as_slice(),
            br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:tab></w:tab></w:r></w:p></w:body></w:document>"#.as_slice(),
        ] {
            assert_office_error!(
                parse_docx(
                    malformed,
                    "word/document.xml",
                    &mut parse_budget(OfficeLimits::default()),
                ),
                OfficeError::MalformedXml
            );
        }
        let valid_docx = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:tbl><w:tr><w:tc><w:p><w:r><w:t>A</w:t><w:tab/><w:t>B</w:t><w:br/><w:t>C</w:t></w:r></w:p></w:tc></w:tr></w:tbl><w:p><w:pPr><w:sectPr/></w:pPr><w:r><w:t>one</w:t></w:r></w:p><w:p><w:hyperlink><w:r><w:t>two</w:t></w:r></w:hyperlink></w:p></w:body></w:document>"#;
        let units = parse_docx(
            valid_docx,
            "word/document.xml",
            &mut parse_budget(OfficeLimits::default()),
        )
        .expect("owned DOCX controls and section break");
        assert_eq!(units.len(), 2);
        assert!(
            units[0].text.contains('A')
                && units[0].text.contains('B')
                && units[0].text.contains('C')
        );
        assert!(units[0].text.ends_with("one"));
        assert_eq!(units[1].text, "two");

        let nested_odt_paragraph = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:text><text:p><text:p>ODT_SENTINEL</text:p></text:p></office:text></office:body></office:document-content>"#;
        assert_office_error!(
            parse_odf_content(
                OfficeKind::Odt,
                nested_odt_paragraph,
                "content.xml",
                &mut parse_budget(OfficeLimits::default()),
            ),
            OfficeError::MalformedXml
        );
        let table_owned_text = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Spoofed"><text:p>ODS_SENTINEL</text:p></table:table></office:spreadsheet></office:body></office:document-content>"#;
        assert_office_error!(
            parse_odf_content(
                OfficeKind::Ods,
                table_owned_text,
                "content.xml",
                &mut parse_budget(OfficeLimits::default()),
            ),
            OfficeError::MalformedXml
        );
        let page_owned_text = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:presentation><draw:page draw:name="Spoofed"><text:p>ODP_SENTINEL</text:p></draw:page></office:presentation></office:body></office:document-content>"#;
        assert_office_error!(
            parse_odf_content(
                OfficeKind::Odp,
                page_owned_text,
                "content.xml",
                &mut parse_budget(OfficeLimits::default()),
            ),
            OfficeError::MalformedXml
        );
        let start_form_control = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:text><text:p><text:tab></text:tab></text:p></office:text></office:body></office:document-content>"#;
        assert_office_error!(
            parse_odf_content(
                OfficeKind::Odt,
                start_form_control,
                "content.xml",
                &mut parse_budget(OfficeLimits::default()),
            ),
            OfficeError::MalformedXml
        );
        let listed_odt = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:text><text:list><text:list-item><text:p>A<text:tab/>B<text:line-break/>C</text:p></text:list-item></text:list></office:text></office:body></office:document-content>"#;
        let listed_odt_units = parse_odf_content(
            OfficeKind::Odt,
            listed_odt,
            "content.xml",
            &mut parse_budget(OfficeLimits::default()),
        )
        .expect("ODT list paragraph remains owned by the typed body");
        assert_eq!(listed_odt_units[0].text, "A B\nC");
        let listed_ods = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="List"><table:table-rows><table:table-row><table:table-cell><text:list><text:list-item><text:p>ODS list</text:p></text:list-item></text:list></table:table-cell></table:table-row></table:table-rows></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let listed_ods_units = parse_odf_content(
            OfficeKind::Ods,
            listed_ods,
            "content.xml",
            &mut parse_budget(OfficeLimits::default()),
        )
        .expect("ODS list paragraph remains owned by its cell");
        assert_eq!(listed_ods_units[0].text, "ODS list");
        let listed_odp = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:presentation><draw:page draw:name="List"><draw:g><draw:frame><draw:text-box><text:list><text:list-item><text:p>ODP list</text:p></text:list-item></text:list></draw:text-box></draw:frame></draw:g></draw:page></office:presentation></office:body></office:document-content>"#;
        let listed_odp_units = parse_odf_content(
            OfficeKind::Odp,
            listed_odp,
            "content.xml",
            &mut parse_budget(OfficeLimits::default()),
        )
        .expect("ODP list paragraph remains owned by its text box");
        assert_eq!(listed_odp_units[0].text, "ODP list");
    }

    #[test]
    fn empty_odf_units_are_materialized_with_owned_labels() {
        let fixtures = [
            (
                OfficeKind::Odt,
                br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:text><text:section text:name="Empty ODT"/></office:text></office:body></office:document-content>"#.as_slice(),
                "Empty ODT",
            ),
            (
                OfficeKind::Ods,
                br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Empty ODS"/></office:spreadsheet></office:body></office:document-content>"#.as_slice(),
                "Empty ODS",
            ),
            (
                OfficeKind::Odp,
                br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"><office:body><office:presentation><draw:page draw:name="Empty ODP"/></office:presentation></office:body></office:document-content>"#.as_slice(),
                "Empty ODP",
            ),
        ];
        for (kind, xml, expected_label) in fixtures {
            let units = parse_odf_content(
                kind,
                xml,
                "content.xml",
                &mut parse_budget(OfficeLimits::default()),
            )
            .expect("empty owned unit is retained");
            assert_eq!(units.len(), 1);
            assert_eq!(units[0].label, expected_label);
            assert!(units[0].text.is_empty());
        }
    }

    #[test]
    fn ooxml_text_controls_require_owned_ancestry_and_ignore_foreign_subtrees() {
        let shared_strings = br#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:evil="urn:evil"><si><t>safe</t><evil:payload><t>SHARED_SENTINEL</t></evil:payload><r><t> rich</t></r></si></sst>"#;
        let strings =
            parse_xlsx_shared_strings(shared_strings, &mut parse_budget(OfficeLimits::default()))
                .expect("foreign shared-string content is inert");
        assert_eq!(strings, ["safe rich"]);
        let malformed_shared_strings = br#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><r><rPr><t>SHARED_SENTINEL</t></rPr></r></si></sst>"#;
        assert_office_error!(
            parse_xlsx_shared_strings(
                malformed_shared_strings,
                &mut parse_budget(OfficeLimits::default()),
            ),
            OfficeError::MalformedXml
        );

        let worksheet = br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:evil="urn:evil"><sheetData><row><c r="A1"><evil:payload><v>CELL_SENTINEL</v></evil:payload><v>7</v></c><c r="A2" t="inlineStr"><is><t>safe</t><evil:payload><t>INLINE_SENTINEL</t></evil:payload></is></c></row></sheetData></worksheet>"#;
        let worksheet_text =
            parse_xlsx_worksheet(worksheet, &[], &mut parse_budget(OfficeLimits::default()))
                .expect("foreign cell content is inert");
        assert_eq!(worksheet_text, "A1: 7\nA2: safe");
        let malformed_worksheet = br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row><c r="A1"><f><v>CELL_SENTINEL</v></f></c></row></sheetData></worksheet>"#;
        assert_office_error!(
            parse_xlsx_worksheet(
                malformed_worksheet,
                &[],
                &mut parse_budget(OfficeLimits::default()),
            ),
            OfficeError::MalformedXml
        );

        let slide = br#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:evil="urn:evil"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>safe</a:t></a:r><evil:payload><a:r><a:t>SLIDE_SENTINEL</a:t></a:r></evil:payload><a:fld><a:t> field</a:t></a:fld></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#;
        let slide_text = parse_pptx_slide(slide, &mut parse_budget(OfficeLimits::default()))
            .expect("foreign slide content is inert");
        assert_eq!(slide_text, "safe field");
        let malformed_slide = br#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:rPr><a:t>SLIDE_SENTINEL</a:t></a:rPr></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#;
        assert_office_error!(
            parse_pptx_slide(malformed_slide, &mut parse_budget(OfficeLimits::default()),),
            OfficeError::MalformedXml
        );
        let active_slide = br#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:evil="urn:evil"><p:cSld><p:spTree><p:sp><p:txBody><a:p><evil:payload><p:oleObj/></evil:payload></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#;
        assert_office_error!(
            parse_pptx_slide(active_slide, &mut parse_budget(OfficeLimits::default())),
            OfficeError::ActiveContent
        );
    }

    #[test]
    fn epub_titles_and_ignored_subtrees_are_text_only_and_inert() {
        let opf = br#"<package xmlns="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:evil="urn:evil" version="3.0"><metadata><dc:title>Publication<evil:span>OPF_TITLE_SENTINEL</evil:span></dc:title></metadata><manifest><item id="chapter" href="chapter.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="chapter"/></spine></package>"#;
        let draft = parse_opf(
            "OPS/package.opf",
            opf,
            &mut parse_budget(OfficeLimits::default()),
        )
        .expect("declared foreign title subtree is inert");
        assert_eq!(draft.title.as_deref(), Some("Publication"));
        let malformed_opf = br#"<package xmlns="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/" version="3.0"><metadata><dc:title>Publication<dc:creator>OPF_TITLE_SENTINEL</dc:creator></dc:title></metadata><manifest><item id="chapter" href="chapter.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="chapter"/></spine></package>"#;
        assert_office_error!(
            parse_opf(
                "OPS/package.opf",
                malformed_opf,
                &mut parse_budget(OfficeLimits::default()),
            ),
            OfficeError::MalformedXml
        );

        let xhtml = br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:evil="urn:evil"><head><title>Chapter<evil:span>XHTML_TITLE_SENTINEL</evil:span></title></head><body><p>left<style>STYLE_SENTINEL<style>NESTED_STYLE_SENTINEL</style>AFTER_NESTED_STYLE<br/></style><evil:wrap><br/><div>FOREIGN_BREAK_SENTINEL</div></evil:wrap>right</p></body></html>"#;
        let draft = parse_xhtml(
            "OPS/chapter.xhtml",
            xhtml,
            &mut parse_budget(OfficeLimits::default()),
        )
        .expect("ignored XHTML scopes are inert");
        assert_eq!(draft.title.as_deref(), Some("Chapter"));
        assert_eq!(draft.text, "leftright");
        let malformed_xhtml = br#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Chapter<span>XHTML_TITLE_SENTINEL</span></title></head><body/></html>"#;
        assert_office_error!(
            parse_xhtml(
                "OPS/chapter.xhtml",
                malformed_xhtml,
                &mut parse_budget(OfficeLimits::default()),
            ),
            OfficeError::MalformedXml
        );
    }

    #[test]
    fn epub_xhtml_base_is_rejected_before_relative_links() {
        for base in [
            r#"<base href="https://attacker.invalid/"></base>"#,
            r#"<base href="https://attacker.invalid/"/>"#,
        ] {
            let xhtml = format!(
                r#"<html xmlns="http://www.w3.org/1999/xhtml"><head>{base}</head><body><a href="relative.xhtml">LINK_SENTINEL</a></body></html>"#
            );
            assert_office_error!(
                parse_xhtml(
                    "OPS/chapter.xhtml",
                    xhtml.as_bytes(),
                    &mut parse_budget(OfficeLimits::default()),
                ),
                OfficeError::InvalidRelationship
            );
        }
    }

    #[test]
    fn relationship_unit_text_shared_string_and_cell_limits_are_independent() {
        let relationships = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="r1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/a.png"/><Relationship Id="r2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/b.png"/></Relationships>"#;
        let limits = OfficeLimits {
            max_relationships: 1,
            ..OfficeLimits::default()
        };
        assert_office_error!(
            parse_relationships(
                "word/_rels/document.xml.rels",
                relationships,
                &mut parse_budget(limits)
            ),
            OfficeError::RelationshipLimit
        );

        let sections = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>one</w:t></w:r></w:p><w:sectPr/><w:p><w:r><w:t>two</w:t></w:r></w:p></w:body></w:document>"#;
        let limits = OfficeLimits {
            max_units: 1,
            ..OfficeLimits::default()
        };
        assert_office_error!(
            parse_docx(sections, "word/document.xml", &mut parse_budget(limits)),
            OfficeError::UnitLimit
        );

        let text = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>abcd</w:t></w:r></w:p></w:body></w:document>"#;
        let limits = OfficeLimits {
            max_text_bytes_per_unit: 3,
            ..OfficeLimits::default()
        };
        assert_office_error!(
            parse_docx(text, "word/document.xml", &mut parse_budget(limits)),
            OfficeError::TextLimit
        );
        let limits = OfficeLimits {
            max_total_text_bytes: 3,
            ..OfficeLimits::default()
        };
        assert_office_error!(
            parse_docx(text, "word/document.xml", &mut parse_budget(limits)),
            OfficeError::TextLimit
        );

        let strings = br#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><t>a</t></si><si><t>b</t></si></sst>"#;
        let limits = OfficeLimits {
            max_shared_strings: 1,
            ..OfficeLimits::default()
        };
        assert_office_error!(
            parse_xlsx_shared_strings(strings, &mut parse_budget(limits)),
            OfficeError::TextLimit
        );

        let worksheet = br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row><c/><c/></row></sheetData></worksheet>"#;
        let limits = OfficeLimits {
            max_table_cells: 1,
            ..OfficeLimits::default()
        };
        assert_office_error!(
            parse_xlsx_worksheet(worksheet, &[], &mut parse_budget(limits)),
            OfficeError::CellLimit
        );
    }

    #[test]
    fn retained_model_pending_and_fact_limits_fail_before_publication() {
        let ledger = ModelLedger::new(1_024);
        let reservation = ledger.reserve(768).expect("pending reservation");
        assert_eq!(ledger.retain(257), Err(OfficeError::ModelLimit));
        ledger.retain(256).expect("retained within live pending");
        drop(reservation);
        ledger.retain(768).expect("released pending is reusable");
        assert_eq!(ledger.retain(1), Err(OfficeError::ModelLimit));

        let source = docx(
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>safe</w:t></w:r></w:p></w:body></w:document>"#,
        );
        let limits = OfficeLimits {
            max_model_bytes: 1_024,
            ..OfficeLimits::default()
        };
        assert_office_error!(extract_docx(&source, limits, None), OfficeError::ModelLimit);

        let source_directory = "a".repeat(3_900);
        let rels_path = format!("{source_directory}/_rels/document.xml.rels");
        let mut relationships = String::from(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
        );
        for index in 0..32 {
            relationships.push_str(&format!(
                r#"<Relationship Id="r{index}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/{index}.png"/>"#,
            ));
        }
        relationships.push_str("</Relationships>");
        let limits = OfficeLimits {
            max_model_bytes: 64 * 1024,
            ..OfficeLimits::default()
        };
        let mut budget = parse_budget(limits);
        assert_office_error!(
            parse_relationships(&rels_path, relationships.as_bytes(), &mut budget),
            OfficeError::ModelLimit
        );
        assert!(budget.model.retained.get() <= limits.max_model_bytes);

        let long = "p".repeat(2_000);
        let finalized = FinalizedPackage {
            title: None,
            units: vec![UnitDraft {
                label: "unit".into(),
                part: long.clone(),
                text: String::new(),
            }],
            relationships: (0..8)
                .map(|index| RelationshipDraft {
                    source_part: Some(long.clone()),
                    id: format!("{long}{index}"),
                    target_part: long.clone(),
                    kind: "image",
                })
                .collect(),
            external_relationships: 0,
        };
        let finalized_bytes = estimate_finalized_model(&finalized).expect("finalized model");
        let materialization_bytes =
            estimate_materialization_model(&finalized, "fixture.docx").expect("material model");
        let materialization_ledger = ModelLedger::new(128 * 1024);
        materialization_ledger
            .retain(finalized_bytes)
            .expect("finalized model fits alone");
        assert_office_error!(
            materialization_ledger.reserve(materialization_bytes),
            OfficeError::ModelLimit
        );

        let limits = OfficeLimits {
            max_facts: 1,
            ..OfficeLimits::default()
        };
        let result = materialize_extraction(MaterializeRequest {
            path: Path::new("fixture.docx"),
            source_file: "fixture.docx",
            kind: OfficeKind::Docx,
            title: None,
            units: vec![UnitDraft {
                label: "Section 1".into(),
                part: "word/document.xml".into(),
                text: "safe".into(),
            }],
            relationships: Vec::new(),
            external_relationships: 0,
            member_count: 3,
            decompressed_bytes: 1,
            xml_events: 1,
            text_bytes: 4,
            limits,
        });
        assert_office_error!(result, OfficeError::FactLimit);
    }

    #[test]
    fn epub_finalize_debits_repeated_link_sources_before_cloning() {
        let source_part = format!("OPS/{}/chapter.xhtml", "s".repeat(3_800));
        let opf_path = "OPS/package.opf".to_owned();
        let target_part = "OPS/target.xhtml".to_owned();
        let mut scratch = PackageScratch {
            mimetype: Some("application/epub+zip".into()),
            epub_rootfiles: vec![opf_path.clone()],
            ..PackageScratch::default()
        };
        scratch.opfs.insert(
            opf_path.clone(),
            OpfDraft {
                manifest: BTreeMap::from([(
                    "chapter".into(),
                    ManifestItem {
                        path: source_part.clone(),
                        media_type: "application/xhtml+xml".into(),
                    },
                )]),
                manifest_ids: BTreeSet::from(["chapter".into()]),
                spine: vec!["chapter".into()],
                title: None,
                external_items: 0,
            },
        );
        scratch.xhtml.insert(
            source_part.clone(),
            XhtmlDraft {
                title: None,
                text: String::new(),
                links: vec![target_part.clone(); 64],
                external_links: 0,
            },
        );
        let members = BTreeSet::from([
            "mimetype".into(),
            "META-INF/container.xml".into(),
            opf_path,
            source_part,
            target_part,
        ]);
        let scratch_model = estimate_package_model(&scratch).expect("bounded scratch model");
        let max_model_bytes = scratch_model
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(16 * 1024))
            .expect("test limit");
        let limits = OfficeLimits {
            max_model_bytes,
            ..OfficeLimits::default()
        };
        let model = ModelLedger::new(max_model_bytes);
        model.retain(scratch_model).expect("scratch fits");
        let _finalize_permit = model.reserve(scratch_model).expect("finalize scratch fits");
        let mut budget = ParseBudget::new(limits, None, Rc::clone(&model));

        assert_office_error!(
            finalize_package(OfficeKind::Epub, scratch, &members, &mut budget),
            OfficeError::ModelLimit
        );
        assert!(model.retained.get() <= max_model_bytes);
    }

    #[test]
    fn cancellation_releases_decode_permits_and_interrupts_xml_work() {
        struct TrackingPermit<'a>(&'a Cell<usize>);
        impl Drop for TrackingPermit<'_> {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        let source = docx(
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body/></w:document>"#,
        );
        let cancelled = Cell::new(false);
        let drops = Cell::new(0);
        let check = || cancelled.get();
        let result = extract_office_bytes_with_admission(
            Path::new("fixture.docx"),
            "fixture.docx",
            &source,
            OfficeKind::Docx,
            OfficeLimits::default(),
            Some(&check),
            |_| true,
            |_| {
                cancelled.set(true);
                Some(TrackingPermit(&drops))
            },
        );
        assert_office_error!(result, OfficeError::Cancelled);
        assert_eq!(drops.get(), 1);

        let mut document = String::from(
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
        );
        document.push_str(&"<w:p/>".repeat(1_100));
        document.push_str("</w:body></w:document>");
        let always_cancelled = || true;
        let mut budget = ParseBudget::new(
            OfficeLimits::default(),
            Some(&always_cancelled),
            ModelLedger::new(OfficeLimits::default().max_model_bytes),
        );
        assert_office_error!(
            parse_docx(document.as_bytes(), "word/document.xml", &mut budget),
            OfficeError::Cancelled
        );
    }
}
