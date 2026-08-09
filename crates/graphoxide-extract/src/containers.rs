//! Bounded byte-only inventory for containers and media.
//!
//! This module deliberately accepts borrowed source bytes.  It performs no
//! path I/O, never writes extracted members, and has no facility for invoking
//! a child extractor.  The runtime owns recursive dispatch and passes the
//! current depth explicitly, so this layer can enforce its part of the
//! untrusted-input budget before an expensive parser or allocation is reached.

use flate2::bufread::GzDecoder;
use quick_xml::{events::Event, Reader};
use std::{
    alloc::{alloc_zeroed, Layout},
    cmp,
    collections::BTreeSet,
    io::{BufRead, Cursor, Read},
    ptr,
};
use unicode_normalization::UnicodeNormalization as _;

const READ_BUFFER_BYTES: usize = 64 * 1024;
const ZIP_EOCD_SIGNATURE: &[u8; 4] = b"PK\x05\x06";
const ZIP64_EOCD_SIGNATURE: &[u8; 4] = b"PK\x06\x06";
const ZIP64_LOCATOR_SIGNATURE: &[u8; 4] = b"PK\x06\x07";
const SVG_HREF_ATTRIBUTES: &[&[u8]] = &[b"href", b"src", b"xlink:href"];

/// Limits used for every container and media inspection.
///
/// The runtime provides the source allocation.  These ceilings protect parser
/// metadata, bounded scratch buffers, and semantic output independently of
/// that allocation.  All limits are checked before a corresponding resource is
/// admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerLimits {
    /// Largest source byte slice accepted by this module.
    pub max_input_bytes: usize,
    /// Maximum nesting level accepted from the runtime, with a root at zero.
    pub max_recursion_depth: u16,
    /// Maximum central-directory entries accepted from a ZIP package.
    pub max_members: usize,
    /// Maximum ZIP central-directory byte range admitted before `ZipArchive`.
    pub max_central_directory_bytes: usize,
    /// Maximum uncompressed bytes admitted from an individual member.
    pub max_member_uncompressed_bytes: u64,
    /// Maximum uncompressed bytes admitted from the complete container.
    pub max_total_uncompressed_bytes: u64,
    /// Largest allowed declared or observed decompression expansion factor.
    pub max_compression_ratio: u64,
    /// Maximum displayable member-name bytes.
    pub max_member_name_bytes: usize,
    /// Maximum SVG or decompressed SVGZ source bytes parsed as XML.
    pub max_svg_bytes: usize,
    /// Maximum bytes in a single XML event before XML parsing starts.
    pub max_svg_event_bytes: usize,
    /// Maximum XML nesting depth accepted in an SVG document.
    pub max_svg_depth: usize,
    /// Maximum retained SVG elements.
    pub max_svg_elements: usize,
    /// Maximum retained SVG references.
    pub max_svg_references: usize,
    /// Maximum retained label or reference bytes.
    pub max_svg_string_bytes: usize,
    /// Maximum bytes inspected for raster metadata after magic classification.
    pub max_media_probe_bytes: usize,
}

impl Default for ContainerLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 512 * 1024 * 1024,
            max_recursion_depth: 4,
            max_members: 4_096,
            max_central_directory_bytes: 16 * 1024 * 1024,
            max_member_uncompressed_bytes: 64 * 1024 * 1024,
            max_total_uncompressed_bytes: 256 * 1024 * 1024,
            max_compression_ratio: 128,
            max_member_name_bytes: 4 * 1024,
            max_svg_bytes: 32 * 1024 * 1024,
            max_svg_event_bytes: 256 * 1024,
            max_svg_depth: 128,
            max_svg_elements: 16_384,
            max_svg_references: 16_384,
            max_svg_string_bytes: 4 * 1024,
            max_media_probe_bytes: 1024 * 1024,
        }
    }
}

impl ContainerLimits {
    fn valid(self) -> bool {
        self.max_input_bytes > 0
            && self.max_members > 0
            && self.max_central_directory_bytes > 0
            && self.max_member_uncompressed_bytes > 0
            && self.max_total_uncompressed_bytes > 0
            && self.max_compression_ratio > 0
            && self.max_member_name_bytes > 0
            && self.max_svg_bytes > 0
            && self.max_svg_event_bytes > 0
            && self.max_svg_depth > 0
            && self.max_svg_elements > 0
            && self.max_svg_references > 0
            && self.max_svg_string_bytes > 0
            && self.max_media_probe_bytes > 0
    }
}

/// A recognized archive encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    Zip,
    Gzip,
    Tar,
    Bzip2,
    Xz,
    Zstd,
    SevenZip,
    Rar,
}

/// A recognized raster or vector media encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Svg,
    Svgz,
    Png,
    Jpeg,
    Gif,
    Webp,
    Avif,
    Heif,
    Bmp,
    Tiff,
    Ico,
}

/// Whether the bytes were fully parsed, safely inventoried, or rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectionStatus {
    Parsed,
    InventoryOnly,
    Rejected,
}

/// Deterministic, non-sensitive reason codes for an inspection result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectionDiagnostic {
    InvalidLimits,
    InputTooLarge,
    RecursionLimit,
    InvalidArchive,
    MultiDiskZipUnsupported,
    Zip64MetadataInvalid,
    CentralDirectoryLimit,
    MemberLimit,
    InvalidMemberName,
    MemberNameLimit,
    EncryptedMember,
    SymlinkMember,
    NonRegularMember,
    UnsupportedCompression,
    MemberSizeLimit,
    TotalSizeLimit,
    CompressionRatioLimit,
    DecompressionFailed,
    DeclaredSizeMismatch,
    GzipHeaderInvalid,
    GzipHeaderLimit,
    GzipMultipleMembers,
    GzipTrailingBytes,
    TarHeaderInvalid,
    TarChecksumInvalid,
    TarTruncated,
    TarUnsupportedEntry,
    Bzip2HeaderInvalid,
    XzHeaderInvalid,
    ZstdHeaderInvalid,
    DeclaredSizeUnavailable,
    DecoderUnavailable,
    MemberDispatchSkipped,
    NestedDispatchStopped,
    Cancelled,
    UnsupportedArchiveFormat,
    InvalidSvg,
    SvgDocumentTypeForbidden,
    SvgEventLimit,
    SvgDepthLimit,
    SvgElementLimit,
    SvgReferenceLimit,
    SvgStringLimit,
    SvgRootMissing,
    InvalidImage,
}

/// Classification assigned to an admitted ZIP member without opening it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerMemberKind {
    Directory,
    File,
    OfficePart,
    DrawioPart,
    Svg,
    RasterImage,
    NestedContainer,
}

/// Safe, normalized metadata for an admitted member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerMember {
    /// A relative, slash-separated name with no `.` or `..` components.
    pub path: String,
    pub kind: ContainerMemberKind,
    pub compressed_bytes: u64,
    pub declared_uncompressed_bytes: u64,
}

/// Bounded inventory returned for an archive payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerInspection {
    pub kind: ArchiveKind,
    pub status: InspectionStatus,
    pub members: Vec<ContainerMember>,
    /// Bytes actually decompressed by this inspection, not a declaration.
    pub decompressed_bytes: u64,
    pub diagnostics: Vec<InspectionDiagnostic>,
}

/// A zero-copy regular TAR member suitable for runtime-owned recursive
/// admission. The supplied byte slice is always a range of the caller's
/// source allocation; this module never writes it to disk or opens a child.
#[derive(Debug, Clone, Copy)]
pub struct DispatchableTarMember<'member, 'source> {
    pub member: &'member ContainerMember,
    pub bytes: &'source [u8],
}

/// A bounded ZIP member decompressed into a temporary, exact-size buffer.
///
/// ZIP compression prevents a zero-copy hand-off. The buffer is allocated only
/// after the central directory, member size, aggregate byte, and compression
/// ratio limits and caller admission have all been validated. The visitor
/// cannot retain it, and the allocation is dropped before its opaque admission
/// permit, so recursively parsing an archive cannot retain sibling payloads or
/// require filesystem staging.
#[derive(Debug, Clone, Copy)]
pub struct DispatchableZipMember<'member, 'bytes> {
    pub member: &'member ContainerMember,
    pub bytes: &'bytes [u8],
}

/// A bounded single-stream GZIP member decoded into an exact temporary buffer.
#[derive(Debug, Clone, Copy)]
pub struct DispatchableGzipMember<'member, 'bytes> {
    pub member: &'member ContainerMember,
    pub bytes: &'bytes [u8],
}

/// Pre-allocation decision for one compressed member.
///
/// A caller returns an owned permit with [`Self::Dispatch`]. The compressed
/// visitor keeps that opaque value alive until the exact decoded allocation,
/// decoder, and child visitor have all finished. [`Self::Skip`] inventories a
/// member without constructing its decoder, while [`Self::Stop`] ends the
/// deterministic walk without opening the current member.
#[derive(Debug)]
pub enum CompressedMemberAdmission<Permit> {
    Dispatch(Permit),
    Skip,
    Stop,
}

enum CompressedMemberVisitOutcome {
    Dispatched {
        decoded_bytes: u64,
        continue_visiting: bool,
    },
    Skipped,
    Stopped,
    Cancelled,
    Rejected(InspectionDiagnostic),
}

/// Allocate one fallible, exact-layout byte slice for decoded scratch.
///
/// Stable Rust does not yet expose the fallible boxed-slice allocator API.
/// Using the global allocator directly avoids a `Vec` whose capacity may
/// exceed the byte count reserved by the caller's opaque scratch permit.
fn try_zeroed_boxed_slice(length: usize) -> Option<Box<[u8]>> {
    if length == 0 {
        return Some(Box::default());
    }
    let layout = Layout::array::<u8>(length).ok()?;
    // SAFETY: `layout` describes exactly `length` initialized `u8` values.
    // A non-null result belongs to the global allocator and is immediately
    // converted to a uniquely owned slice with the same layout, so `Box`
    // later deallocates it correctly.
    let allocation = ptr::NonNull::new(unsafe { alloc_zeroed(layout) })?;
    let slice = ptr::slice_from_raw_parts_mut(allocation.as_ptr(), length);
    // SAFETY: `slice` was built from the allocation above, has its exact
    // length, and has not been aliased or transferred elsewhere.
    Some(unsafe { Box::from_raw(slice) })
}

/// Decode one compressed member only after the caller has supplied an opaque
/// scratch permit.
///
/// Both ZIP and GZIP use this boundary. `finish_reader` validates
/// representation-specific trailing input while the permit is still live.
#[allow(clippy::too_many_arguments)]
fn visit_admitted_compressed_member<Permit, Reader, Admission, Cancellation, Open, Finish, Visit>(
    member: &ContainerMember,
    limits: ContainerLimits,
    is_cancelled: &mut Cancellation,
    admit: &mut Admission,
    open_reader: Open,
    finish_reader: Finish,
    visit: Visit,
) -> CompressedMemberVisitOutcome
where
    Reader: Read,
    Admission: FnMut(&ContainerMember) -> CompressedMemberAdmission<Permit>,
    Cancellation: FnMut() -> bool,
    Open: FnOnce() -> Result<Reader, InspectionDiagnostic>,
    Finish: FnOnce(Reader) -> Result<(), InspectionDiagnostic>,
    Visit: FnOnce(&[u8]) -> bool,
{
    if is_cancelled() {
        return CompressedMemberVisitOutcome::Cancelled;
    }
    let permit = match admit(member) {
        CompressedMemberAdmission::Dispatch(permit) => permit,
        CompressedMemberAdmission::Skip => return CompressedMemberVisitOutcome::Skipped,
        CompressedMemberAdmission::Stop => return CompressedMemberVisitOutcome::Stopped,
    };
    if is_cancelled() {
        return CompressedMemberVisitOutcome::Cancelled;
    }

    let declared = member.declared_uncompressed_bytes;
    if declared > limits.max_member_uncompressed_bytes {
        return CompressedMemberVisitOutcome::Rejected(InspectionDiagnostic::MemberSizeLimit);
    }
    if declared > limits.max_total_uncompressed_bytes {
        return CompressedMemberVisitOutcome::Rejected(InspectionDiagnostic::TotalSizeLimit);
    }
    let payload_len = match usize::try_from(declared) {
        Ok(length) => length,
        Err(_) => {
            return CompressedMemberVisitOutcome::Rejected(InspectionDiagnostic::MemberSizeLimit)
        }
    };
    let mut payload = match try_zeroed_boxed_slice(payload_len) {
        Some(payload) => payload,
        None => {
            return CompressedMemberVisitOutcome::Rejected(InspectionDiagnostic::MemberSizeLimit)
        }
    };
    let mut reader = match open_reader() {
        Ok(reader) => reader,
        Err(diagnostic) => return CompressedMemberVisitOutcome::Rejected(diagnostic),
    };

    let mut offset = 0_usize;
    while offset < payload.len() {
        if is_cancelled() {
            return CompressedMemberVisitOutcome::Cancelled;
        }
        let end = offset.saturating_add(READ_BUFFER_BYTES).min(payload.len());
        match reader.read(&mut payload[offset..end]) {
            Ok(0) => {
                return CompressedMemberVisitOutcome::Rejected(
                    InspectionDiagnostic::DeclaredSizeMismatch,
                )
            }
            Ok(read) => offset += read,
            Err(_) => {
                return CompressedMemberVisitOutcome::Rejected(
                    InspectionDiagnostic::DecompressionFailed,
                )
            }
        }
    }
    if is_cancelled() {
        return CompressedMemberVisitOutcome::Cancelled;
    }
    let mut trailing = [0_u8; 1];
    match reader.read(&mut trailing) {
        Ok(0) => {}
        Ok(_) => {
            return CompressedMemberVisitOutcome::Rejected(
                InspectionDiagnostic::DeclaredSizeMismatch,
            )
        }
        Err(_) => {
            return CompressedMemberVisitOutcome::Rejected(
                InspectionDiagnostic::DecompressionFailed,
            )
        }
    }
    if let Err(diagnostic) = finish_reader(reader) {
        return CompressedMemberVisitOutcome::Rejected(diagnostic);
    }
    if is_cancelled() {
        return CompressedMemberVisitOutcome::Cancelled;
    }
    let continue_visiting = visit(&payload);
    // Make the lifetime contract explicit: decoded storage is freed before
    // the opaque scratch permit returned by the admission plane.
    drop(payload);
    drop(permit);
    CompressedMemberVisitOutcome::Dispatched {
        decoded_bytes: declared,
        continue_visiting,
    }
}

/// Dimensions and simple animation metadata discovered without decoding pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageMetadata {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub animated: Option<bool>,
}

/// An SVG element retained for graph adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SvgElement {
    pub ordinal: usize,
    pub name: String,
    pub id: Option<String>,
    pub label: Option<String>,
}

/// An SVG reference retained for graph adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SvgReference {
    pub source_ordinal: usize,
    /// Fragment identifiers omit their leading `#`; external values are kept
    /// as opaque, bounded strings and are never opened by this module.
    pub target: String,
    pub relation: SvgReferenceRelation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvgReferenceRelation {
    Fragment,
    External,
}

/// Semantic vector inventory for SVG and SVGZ input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SvgInspection {
    pub elements: Vec<SvgElement>,
    pub references: Vec<SvgReference>,
    pub title: Option<String>,
}

/// Bounded inventory returned for image or vector media.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaInspection {
    pub kind: MediaKind,
    pub status: InspectionStatus,
    pub metadata: Option<ImageMetadata>,
    pub svg: Option<SvgInspection>,
    pub diagnostics: Vec<InspectionDiagnostic>,
}

/// Result of byte-only container/media classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ByteInventory {
    Container(ContainerInspection),
    Media(MediaInspection),
    Unrecognized,
}

/// Inspect one source payload without path I/O or recursive extraction.
///
/// `source_name` is only a classification hint.  It is never opened and is
/// never placed in a diagnostic.  Magic has precedence except that `.svgz`
/// directs a valid gzip member through the SVG semantic parser.
pub fn inspect_bytes(
    source_name: &str,
    bytes: &[u8],
    recursion_depth: u16,
    limits: ContainerLimits,
) -> ByteInventory {
    if is_svgz_name(source_name) && looks_like_gzip(bytes) {
        return ByteInventory::Media(inspect_svgz(bytes, recursion_depth, limits));
    }
    if let Some(kind) = detect_archive_kind(source_name, bytes) {
        return ByteInventory::Container(inspect_container_bytes(
            kind,
            bytes,
            recursion_depth,
            limits,
        ));
    }
    if let Some(kind) = detect_media_kind(source_name, bytes) {
        return ByteInventory::Media(inspect_media_bytes(kind, bytes, recursion_depth, limits));
    }
    ByteInventory::Unrecognized
}

/// Return a recursively dispatchable archive kind for ready bytes.
///
/// This deliberately excludes `.svgz`, bzip2, xz, zstd, 7z, and RAR. ZIP,
/// TAR, and single-member GZIP are the encodings whose member bytes can
/// currently be handed to a child adapter under an explicit admission permit.
pub fn recursive_archive_kind(source_name: &str, bytes: &[u8]) -> Option<ArchiveKind> {
    if is_svgz_name(source_name) && looks_like_gzip(bytes) {
        return None;
    }
    match detect_archive_kind(source_name, bytes) {
        Some(ArchiveKind::Zip) => Some(ArchiveKind::Zip),
        Some(ArchiveKind::Gzip) => Some(ArchiveKind::Gzip),
        Some(ArchiveKind::Tar) => Some(ArchiveKind::Tar),
        _ => None,
    }
}

/// Inspect a known archive using only the supplied bytes.
pub fn inspect_container_bytes(
    kind: ArchiveKind,
    bytes: &[u8],
    recursion_depth: u16,
    limits: ContainerLimits,
) -> ContainerInspection {
    if !limits.valid() {
        return rejected_container(kind, InspectionDiagnostic::InvalidLimits);
    }
    if recursion_depth > limits.max_recursion_depth {
        return rejected_container(kind, InspectionDiagnostic::RecursionLimit);
    }
    if bytes.len() > limits.max_input_bytes {
        return rejected_container(kind, InspectionDiagnostic::InputTooLarge);
    }

    match kind {
        ArchiveKind::Zip => inspect_zip(bytes, limits),
        ArchiveKind::Gzip => inspect_gzip(bytes, limits),
        ArchiveKind::Tar => inspect_tar(bytes, limits),
        ArchiveKind::Bzip2 => inspect_bzip2(bytes, limits),
        ArchiveKind::Xz => inspect_xz(bytes, limits),
        ArchiveKind::Zstd => inspect_zstd(bytes, limits),
        // 7z and RAR remain inventory-only until a maintained, memory-bounded
        // decoder can be linked. Never infer members from their headers.
        ArchiveKind::SevenZip | ArchiveKind::Rar => unsupported_archive(kind),
    }
}

/// Visit regular TAR members using slices borrowed from the source allocation.
///
/// This is the recursive-dispatch boundary for the runtime: callers reserve
/// downstream capacity before invoking it, then classify each child at
/// `recursion_depth + 1`. The archive module intentionally does not invoke an
/// extractor itself, so parsing remains capability-free and byte-only.
///
/// TAR is the only container whose member payloads can be handed off without a
/// decompression allocation. Compressed archive families expose a truthful
/// inventory only until a separately bounded streaming decoder is available.
pub fn visit_tar_members<'a, F>(
    bytes: &'a [u8],
    recursion_depth: u16,
    limits: ContainerLimits,
    visitor: F,
) -> ContainerInspection
where
    F: for<'member> FnMut(DispatchableTarMember<'member, 'a>) -> bool,
{
    visit_tar_members_bounded(bytes, recursion_depth, limits, || false, visitor)
}

/// Visit regular TAR members with cancellation during metadata parsing and
/// before every child dispatch.
///
/// TAR payloads remain zero-copy, so no scratch permit is needed. Cancellation
/// is polled at every 512-byte header, while validating trailing zero blocks,
/// and at each member boundary before the visitor receives borrowed bytes.
pub fn visit_tar_members_bounded<'a, Cancellation, F>(
    bytes: &'a [u8],
    recursion_depth: u16,
    limits: ContainerLimits,
    is_cancelled: Cancellation,
    visitor: F,
) -> ContainerInspection
where
    Cancellation: FnMut() -> bool,
    F: for<'member> FnMut(DispatchableTarMember<'member, 'a>) -> bool,
{
    visit_tar_members_bounded_with_encounter(
        bytes,
        recursion_depth,
        limits,
        is_cancelled,
        |_| true,
        visitor,
    )
}

/// Visit TAR members with a tree-scoped admission decision for every entry.
///
/// `encounter` runs in normalized path order for directories and regular files
/// before any child payload is dispatched. This lets a recursive caller apply
/// one aggregate member-count ceiling across the complete archive tree.
pub fn visit_tar_members_bounded_with_encounter<'a, Cancellation, Encounter, F>(
    bytes: &'a [u8],
    recursion_depth: u16,
    limits: ContainerLimits,
    mut is_cancelled: Cancellation,
    mut encounter: Encounter,
    mut visitor: F,
) -> ContainerInspection
where
    Cancellation: FnMut() -> bool,
    Encounter: FnMut(&ContainerMember) -> bool,
    F: for<'member> FnMut(DispatchableTarMember<'member, 'a>) -> bool,
{
    if !limits.valid() {
        return rejected_container(ArchiveKind::Tar, InspectionDiagnostic::InvalidLimits);
    }
    if recursion_depth > limits.max_recursion_depth {
        return rejected_container(ArchiveKind::Tar, InspectionDiagnostic::RecursionLimit);
    }
    if bytes.len() > limits.max_input_bytes {
        return rejected_container(ArchiveKind::Tar, InspectionDiagnostic::InputTooLarge);
    }

    let parsed = match parse_tar(bytes, limits, &mut is_cancelled) {
        Ok(parsed) => parsed,
        Err(InspectionDiagnostic::Cancelled) => {
            return cancelled_container(ArchiveKind::Tar, Vec::new(), 0)
        }
        Err(diagnostic) => return rejected_container(ArchiveKind::Tar, diagnostic),
    };
    let mut member_order = (0..parsed.members.len()).collect::<Vec<_>>();
    member_order.sort_unstable_by(|left, right| {
        parsed.members[*left].path.cmp(&parsed.members[*right].path)
    });
    for index in member_order {
        if is_cancelled() {
            return cancelled_container(
                ArchiveKind::Tar,
                parsed.members,
                parsed.decompressed_bytes,
            );
        }
        if !encounter(&parsed.members[index]) {
            return ContainerInspection {
                kind: ArchiveKind::Tar,
                status: InspectionStatus::InventoryOnly,
                members: parsed.members,
                decompressed_bytes: parsed.decompressed_bytes,
                diagnostics: vec![InspectionDiagnostic::NestedDispatchStopped],
            };
        }
    }
    if recursion_depth == limits.max_recursion_depth && !parsed.dispatchable.is_empty() {
        return ContainerInspection {
            kind: ArchiveKind::Tar,
            status: InspectionStatus::InventoryOnly,
            members: parsed.members,
            decompressed_bytes: parsed.decompressed_bytes,
            diagnostics: vec![InspectionDiagnostic::RecursionLimit],
        };
    }

    let mut dispatch_order = (0..parsed.dispatchable.len()).collect::<Vec<_>>();
    dispatch_order.sort_unstable_by(|left, right| {
        let left_member = parsed.dispatchable[*left].member_index;
        let right_member = parsed.dispatchable[*right].member_index;
        parsed.members[left_member]
            .path
            .cmp(&parsed.members[right_member].path)
    });
    for index in dispatch_order {
        if is_cancelled() {
            return cancelled_container(
                ArchiveKind::Tar,
                parsed.members,
                parsed.decompressed_bytes,
            );
        }
        let entry = &parsed.dispatchable[index];
        if !visitor(DispatchableTarMember {
            member: &parsed.members[entry.member_index],
            bytes: entry.payload,
        }) {
            return ContainerInspection {
                kind: ArchiveKind::Tar,
                status: InspectionStatus::InventoryOnly,
                members: parsed.members,
                decompressed_bytes: parsed.decompressed_bytes,
                diagnostics: vec![InspectionDiagnostic::NestedDispatchStopped],
            };
        }
    }
    parsed.into_inspection()
}

/// Visit regular ZIP members using bounded, temporary payload storage.
///
/// Unlike TAR, ZIP members are compressed and cannot borrow the source
/// allocation. This function validates the entire central directory before
/// allocating a payload, then allocates one exact-layout boxed slice for each
/// admitted regular file. A visitor returning `false` stops dispatch and yields a
/// truthful inventory-only result. This function never opens a path, writes a
/// member, or invokes a parser.
pub fn visit_zip_members<F>(
    bytes: &[u8],
    recursion_depth: u16,
    limits: ContainerLimits,
    visitor: F,
) -> ContainerInspection
where
    F: for<'member, 'payload> FnMut(DispatchableZipMember<'member, 'payload>) -> bool,
{
    visit_zip_members_bounded(
        bytes,
        recursion_depth,
        limits,
        || false,
        |_| CompressedMemberAdmission::Dispatch(()),
        visitor,
    )
}

/// Visit ZIP members through caller-owned scratch admission and cancellation.
///
/// The central directory and every member's path, type, compression, declared
/// size, and expansion ratio are validated before `admit` is called. A
/// [`CompressedMemberAdmission::Dispatch`] permit is acquired before a decoder
/// or output allocation exists and remains live through the child visitor.
/// Cancellation is polled before every member and before every bounded decoder
/// read. Skipped members are never opened or decompressed.
pub fn visit_zip_members_bounded<Permit, Cancellation, Admission, F>(
    bytes: &[u8],
    recursion_depth: u16,
    limits: ContainerLimits,
    is_cancelled: Cancellation,
    admit: Admission,
    visitor: F,
) -> ContainerInspection
where
    Cancellation: FnMut() -> bool,
    Admission: FnMut(&ContainerMember) -> CompressedMemberAdmission<Permit>,
    F: for<'member, 'payload> FnMut(DispatchableZipMember<'member, 'payload>) -> bool,
{
    visit_zip_members_bounded_with_encounter(
        bytes,
        recursion_depth,
        limits,
        is_cancelled,
        |_| true,
        admit,
        visitor,
    )
}

/// Visit ZIP members with aggregate admission for every directory and file.
///
/// `encounter` is evaluated for the complete, path-sorted metadata inventory
/// before any decoder is opened. A recursive caller can therefore stop a tree
/// at one deterministic aggregate member limit without inflating a child.
pub fn visit_zip_members_bounded_with_encounter<Permit, Cancellation, Encounter, Admission, F>(
    bytes: &[u8],
    recursion_depth: u16,
    limits: ContainerLimits,
    mut is_cancelled: Cancellation,
    mut encounter: Encounter,
    mut admit: Admission,
    mut visitor: F,
) -> ContainerInspection
where
    Cancellation: FnMut() -> bool,
    Encounter: FnMut(&ContainerMember) -> bool,
    Admission: FnMut(&ContainerMember) -> CompressedMemberAdmission<Permit>,
    F: for<'member, 'payload> FnMut(DispatchableZipMember<'member, 'payload>) -> bool,
{
    if !limits.valid() {
        return rejected_container(ArchiveKind::Zip, InspectionDiagnostic::InvalidLimits);
    }
    if recursion_depth > limits.max_recursion_depth {
        return rejected_container(ArchiveKind::Zip, InspectionDiagnostic::RecursionLimit);
    }
    if bytes.len() > limits.max_input_bytes {
        return rejected_container(ArchiveKind::Zip, InspectionDiagnostic::InputTooLarge);
    }

    let mut validated = match validated_zip(bytes, limits) {
        Ok(validated) => validated,
        Err(inspection) => return inspection,
    };
    for index in validated.dispatch_order.iter().copied() {
        if is_cancelled() {
            return ContainerInspection {
                kind: ArchiveKind::Zip,
                status: InspectionStatus::InventoryOnly,
                members: validated.members,
                decompressed_bytes: 0,
                diagnostics: vec![InspectionDiagnostic::Cancelled],
            };
        }
        if !encounter(&validated.members[index]) {
            return ContainerInspection {
                kind: ArchiveKind::Zip,
                status: InspectionStatus::InventoryOnly,
                members: validated.members,
                decompressed_bytes: 0,
                diagnostics: vec![InspectionDiagnostic::NestedDispatchStopped],
            };
        }
    }
    if recursion_depth == limits.max_recursion_depth
        && validated
            .members
            .iter()
            .any(|member| member.kind != ContainerMemberKind::Directory)
    {
        return ContainerInspection {
            kind: ArchiveKind::Zip,
            status: InspectionStatus::InventoryOnly,
            members: validated.members,
            decompressed_bytes: 0,
            diagnostics: vec![InspectionDiagnostic::RecursionLimit],
        };
    }

    let mut actual_total = 0_u64;
    let mut skipped = false;
    for index in validated.dispatch_order.iter().copied() {
        let member_metadata = &validated.members[index];
        if is_cancelled() {
            return ContainerInspection {
                kind: ArchiveKind::Zip,
                status: InspectionStatus::InventoryOnly,
                members: validated.members,
                decompressed_bytes: actual_total,
                diagnostics: vec![InspectionDiagnostic::Cancelled],
            };
        }
        if member_metadata.kind == ContainerMemberKind::Directory {
            continue;
        }
        let outcome = visit_admitted_compressed_member(
            member_metadata,
            limits,
            &mut is_cancelled,
            &mut admit,
            || {
                validated
                    .archive
                    .by_index(index)
                    .map_err(|_| InspectionDiagnostic::DecompressionFailed)
            },
            |_| Ok(()),
            |payload| {
                visitor(DispatchableZipMember {
                    member: member_metadata,
                    bytes: payload,
                })
            },
        );
        match outcome {
            CompressedMemberVisitOutcome::Dispatched {
                decoded_bytes,
                continue_visiting,
            } => {
                actual_total = match actual_total.checked_add(decoded_bytes) {
                    Some(total) if total <= limits.max_total_uncompressed_bytes => total,
                    _ => {
                        return rejected_container(
                            ArchiveKind::Zip,
                            InspectionDiagnostic::TotalSizeLimit,
                        )
                    }
                };
                if !continue_visiting {
                    return ContainerInspection {
                        kind: ArchiveKind::Zip,
                        status: InspectionStatus::InventoryOnly,
                        members: validated.members,
                        decompressed_bytes: actual_total,
                        diagnostics: vec![InspectionDiagnostic::NestedDispatchStopped],
                    };
                }
            }
            CompressedMemberVisitOutcome::Skipped => skipped = true,
            CompressedMemberVisitOutcome::Stopped => {
                return ContainerInspection {
                    kind: ArchiveKind::Zip,
                    status: InspectionStatus::InventoryOnly,
                    members: validated.members,
                    decompressed_bytes: actual_total,
                    diagnostics: vec![InspectionDiagnostic::NestedDispatchStopped],
                }
            }
            CompressedMemberVisitOutcome::Cancelled => {
                return ContainerInspection {
                    kind: ArchiveKind::Zip,
                    status: InspectionStatus::InventoryOnly,
                    members: validated.members,
                    decompressed_bytes: actual_total,
                    diagnostics: vec![InspectionDiagnostic::Cancelled],
                }
            }
            CompressedMemberVisitOutcome::Rejected(diagnostic) => {
                return rejected_container(ArchiveKind::Zip, diagnostic)
            }
        }
    }
    ContainerInspection {
        kind: ArchiveKind::Zip,
        // A caller-directed skip is a safe, deliberate dispatch policy. It
        // must not suppress semantics already produced by admitted siblings.
        status: InspectionStatus::Parsed,
        members: validated.members,
        decompressed_bytes: actual_total,
        diagnostics: if skipped {
            vec![InspectionDiagnostic::MemberDispatchSkipped]
        } else {
            Vec::new()
        },
    }
}

/// Visit one safe GZIP stream with an unmetered compatibility permit.
///
/// New isolated callers should use [`visit_gzip_member_bounded`] so decoded
/// scratch is charged before allocation.
pub fn visit_gzip_member<F>(
    source_name: &str,
    bytes: &[u8],
    recursion_depth: u16,
    limits: ContainerLimits,
    visitor: F,
) -> ContainerInspection
where
    F: for<'member, 'payload> FnMut(DispatchableGzipMember<'member, 'payload>) -> bool,
{
    visit_gzip_member_bounded(
        source_name,
        bytes,
        recursion_depth,
        limits,
        || false,
        |_| CompressedMemberAdmission::Dispatch(()),
        visitor,
    )
}

/// Decode and visit one GZIP member under caller-owned scratch admission.
///
/// The optional FNAME is normalized with the same traversal and reserved
/// namespace policy as ZIP/TAR. Without FNAME, `.tgz` maps to a `.tar` child
/// and `.gz` strips one suffix, retaining an extension such as `.tar` or
/// `.json` for byte-only child dispatch. `.svgz` is deliberately inspected but
/// never handed to this generic archive visitor; its semantic media path is
/// owned by [`inspect_bytes`]. Concatenated members and trailing bytes are
/// rejected after exact-size decoding.
pub fn visit_gzip_member_bounded<Permit, Cancellation, Admission, F>(
    source_name: &str,
    bytes: &[u8],
    recursion_depth: u16,
    limits: ContainerLimits,
    is_cancelled: Cancellation,
    admit: Admission,
    visitor: F,
) -> ContainerInspection
where
    Cancellation: FnMut() -> bool,
    Admission: FnMut(&ContainerMember) -> CompressedMemberAdmission<Permit>,
    F: for<'member, 'payload> FnMut(DispatchableGzipMember<'member, 'payload>) -> bool,
{
    visit_gzip_member_bounded_with_encounter(
        source_name,
        bytes,
        recursion_depth,
        limits,
        is_cancelled,
        |_| true,
        admit,
        visitor,
    )
}

/// Visit one GZIP member with aggregate tree-member admission.
#[allow(clippy::too_many_arguments)]
pub fn visit_gzip_member_bounded_with_encounter<Permit, Cancellation, Encounter, Admission, F>(
    source_name: &str,
    bytes: &[u8],
    recursion_depth: u16,
    limits: ContainerLimits,
    mut is_cancelled: Cancellation,
    mut encounter: Encounter,
    mut admit: Admission,
    mut visitor: F,
) -> ContainerInspection
where
    Cancellation: FnMut() -> bool,
    Encounter: FnMut(&ContainerMember) -> bool,
    Admission: FnMut(&ContainerMember) -> CompressedMemberAdmission<Permit>,
    F: for<'member, 'payload> FnMut(DispatchableGzipMember<'member, 'payload>) -> bool,
{
    if !limits.valid() {
        return rejected_container(ArchiveKind::Gzip, InspectionDiagnostic::InvalidLimits);
    }
    if recursion_depth > limits.max_recursion_depth {
        return rejected_container(ArchiveKind::Gzip, InspectionDiagnostic::RecursionLimit);
    }
    if bytes.len() > limits.max_input_bytes {
        return rejected_container(ArchiveKind::Gzip, InspectionDiagnostic::InputTooLarge);
    }
    if is_svgz_name(source_name) {
        return inspect_gzip(bytes, limits);
    }
    let header_name = match gzip_header_name(bytes, limits.max_member_name_bytes) {
        Ok(name) => name,
        Err(diagnostic) => return rejected_container(ArchiveKind::Gzip, diagnostic),
    };
    let declared = match gzip_declared_size(bytes) {
        Ok(size) => size,
        Err(diagnostic) => return rejected_container(ArchiveKind::Gzip, diagnostic),
    };
    if declared > limits.max_member_uncompressed_bytes {
        return rejected_container(ArchiveKind::Gzip, InspectionDiagnostic::MemberSizeLimit);
    }
    if declared > limits.max_total_uncompressed_bytes {
        return rejected_container(ArchiveKind::Gzip, InspectionDiagnostic::TotalSizeLimit);
    }
    if compression_ratio_exceeded(declared, bytes.len() as u64, limits.max_compression_ratio) {
        return rejected_container(
            ArchiveKind::Gzip,
            InspectionDiagnostic::CompressionRatioLimit,
        );
    }
    let path = match header_name
        .or_else(|| inferred_gzip_member_path(source_name, limits.max_member_name_bytes))
    {
        Some(path) => path,
        None => "gzip-stream".to_owned(),
    };
    let member = ContainerMember {
        kind: classify_member(&path, false),
        path,
        compressed_bytes: bytes.len() as u64,
        declared_uncompressed_bytes: declared,
    };
    if !encounter(&member) {
        return ContainerInspection {
            kind: ArchiveKind::Gzip,
            status: InspectionStatus::InventoryOnly,
            members: vec![member],
            decompressed_bytes: 0,
            diagnostics: vec![InspectionDiagnostic::NestedDispatchStopped],
        };
    }
    if recursion_depth == limits.max_recursion_depth {
        return ContainerInspection {
            kind: ArchiveKind::Gzip,
            status: InspectionStatus::InventoryOnly,
            members: vec![member],
            decompressed_bytes: 0,
            diagnostics: vec![InspectionDiagnostic::RecursionLimit],
        };
    }

    let outcome = visit_admitted_compressed_member(
        &member,
        limits,
        &mut is_cancelled,
        &mut admit,
        || Ok(GzDecoder::new(bytes)),
        |decoder| {
            let trailing = decoder.into_inner();
            if trailing.is_empty() {
                Ok(())
            } else if looks_like_gzip(trailing) {
                Err(InspectionDiagnostic::GzipMultipleMembers)
            } else {
                Err(InspectionDiagnostic::GzipTrailingBytes)
            }
        },
        |payload| {
            visitor(DispatchableGzipMember {
                member: &member,
                bytes: payload,
            })
        },
    );
    match outcome {
        CompressedMemberVisitOutcome::Dispatched {
            decoded_bytes,
            continue_visiting,
        } if continue_visiting => ContainerInspection {
            kind: ArchiveKind::Gzip,
            status: InspectionStatus::Parsed,
            members: vec![member],
            decompressed_bytes: decoded_bytes,
            diagnostics: Vec::new(),
        },
        CompressedMemberVisitOutcome::Dispatched { decoded_bytes, .. } => ContainerInspection {
            kind: ArchiveKind::Gzip,
            status: InspectionStatus::InventoryOnly,
            members: vec![member],
            decompressed_bytes: decoded_bytes,
            diagnostics: vec![InspectionDiagnostic::NestedDispatchStopped],
        },
        CompressedMemberVisitOutcome::Skipped => ContainerInspection {
            kind: ArchiveKind::Gzip,
            status: InspectionStatus::InventoryOnly,
            members: vec![member],
            decompressed_bytes: 0,
            diagnostics: vec![InspectionDiagnostic::MemberDispatchSkipped],
        },
        CompressedMemberVisitOutcome::Stopped => ContainerInspection {
            kind: ArchiveKind::Gzip,
            status: InspectionStatus::InventoryOnly,
            members: vec![member],
            decompressed_bytes: 0,
            diagnostics: vec![InspectionDiagnostic::NestedDispatchStopped],
        },
        CompressedMemberVisitOutcome::Cancelled => ContainerInspection {
            kind: ArchiveKind::Gzip,
            status: InspectionStatus::InventoryOnly,
            members: vec![member],
            decompressed_bytes: 0,
            diagnostics: vec![InspectionDiagnostic::Cancelled],
        },
        CompressedMemberVisitOutcome::Rejected(diagnostic) => {
            rejected_container(ArchiveKind::Gzip, diagnostic)
        }
    }
}

/// Inspect a known raster or vector payload using only the supplied bytes.
pub fn inspect_media_bytes(
    kind: MediaKind,
    bytes: &[u8],
    recursion_depth: u16,
    limits: ContainerLimits,
) -> MediaInspection {
    if !limits.valid() {
        return rejected_media(kind, InspectionDiagnostic::InvalidLimits);
    }
    if recursion_depth > limits.max_recursion_depth {
        return rejected_media(kind, InspectionDiagnostic::RecursionLimit);
    }
    if bytes.len() > limits.max_input_bytes {
        return rejected_media(kind, InspectionDiagnostic::InputTooLarge);
    }

    match kind {
        MediaKind::Svg => inspect_svg(bytes, limits),
        MediaKind::Svgz => inspect_svgz(bytes, recursion_depth, limits),
        _ => inspect_raster(kind, bytes, limits.max_media_probe_bytes),
    }
}

fn rejected_container(kind: ArchiveKind, diagnostic: InspectionDiagnostic) -> ContainerInspection {
    ContainerInspection {
        kind,
        status: InspectionStatus::Rejected,
        members: Vec::new(),
        decompressed_bytes: 0,
        diagnostics: vec![diagnostic],
    }
}

fn cancelled_container(
    kind: ArchiveKind,
    members: Vec<ContainerMember>,
    decompressed_bytes: u64,
) -> ContainerInspection {
    ContainerInspection {
        kind,
        status: InspectionStatus::InventoryOnly,
        members,
        decompressed_bytes,
        diagnostics: vec![InspectionDiagnostic::Cancelled],
    }
}

fn unsupported_archive(kind: ArchiveKind) -> ContainerInspection {
    ContainerInspection {
        kind,
        status: InspectionStatus::InventoryOnly,
        members: Vec::new(),
        decompressed_bytes: 0,
        diagnostics: vec![InspectionDiagnostic::UnsupportedArchiveFormat],
    }
}

struct ParsedTar<'a> {
    members: Vec<ContainerMember>,
    dispatchable: Vec<ParsedTarMember<'a>>,
    decompressed_bytes: u64,
}

struct ParsedTarMember<'a> {
    member_index: usize,
    payload: &'a [u8],
}

impl ParsedTar<'_> {
    fn into_inspection(self) -> ContainerInspection {
        ContainerInspection {
            kind: ArchiveKind::Tar,
            status: InspectionStatus::Parsed,
            members: self.members,
            decompressed_bytes: self.decompressed_bytes,
            diagnostics: Vec::new(),
        }
    }
}

fn inspect_tar(bytes: &[u8], limits: ContainerLimits) -> ContainerInspection {
    match parse_tar(bytes, limits, &mut || false) {
        Ok(parsed) => parsed.into_inspection(),
        Err(diagnostic) => rejected_container(ArchiveKind::Tar, diagnostic),
    }
}

/// Parse POSIX/ustar entries without allocating or extracting member payloads.
///
/// Header parsing deliberately supports only direct regular-file and directory
/// entries. PAX, GNU long-name, sparse, link, and device forms need additional
/// semantics before they can safely participate in recursive dispatch, so they
/// stop at truthful inventory rather than guessing a pathname or target.
fn parse_tar<'a, Cancellation>(
    bytes: &'a [u8],
    limits: ContainerLimits,
    is_cancelled: &mut Cancellation,
) -> Result<ParsedTar<'a>, InspectionDiagnostic>
where
    Cancellation: FnMut() -> bool,
{
    const TAR_BLOCK_BYTES: usize = 512;
    if bytes.is_empty() {
        return Err(InspectionDiagnostic::TarTruncated);
    }

    let mut offset = 0_usize;
    let mut members = Vec::with_capacity(cmp::min(limits.max_members, 64));
    let mut dispatchable = Vec::with_capacity(cmp::min(limits.max_members, 64));
    let mut seen_paths = BTreeSet::new();
    let mut total = 0_u64;

    while offset < bytes.len() {
        if is_cancelled() {
            return Err(InspectionDiagnostic::Cancelled);
        }
        let header_end = offset
            .checked_add(TAR_BLOCK_BYTES)
            .ok_or(InspectionDiagnostic::TarTruncated)?;
        let header = bytes
            .get(offset..header_end)
            .ok_or(InspectionDiagnostic::TarTruncated)?;
        if header.iter().all(|byte| *byte == 0) {
            for block in bytes[header_end..].chunks(TAR_BLOCK_BYTES) {
                if is_cancelled() {
                    return Err(InspectionDiagnostic::Cancelled);
                }
                if block.iter().any(|byte| *byte != 0) {
                    return Err(InspectionDiagnostic::TarHeaderInvalid);
                }
            }
            return Ok(ParsedTar {
                members,
                dispatchable,
                decompressed_bytes: total,
            });
        }
        if members.len() >= limits.max_members {
            return Err(InspectionDiagnostic::MemberLimit);
        }
        validate_tar_checksum(header)?;

        let size = parse_tar_number(&header[124..136])?;
        if size > limits.max_member_uncompressed_bytes {
            return Err(InspectionDiagnostic::MemberSizeLimit);
        }
        total = total
            .checked_add(size)
            .filter(|total| *total <= limits.max_total_uncompressed_bytes)
            .ok_or(InspectionDiagnostic::TotalSizeLimit)?;
        if compression_ratio_exceeded(size, size, limits.max_compression_ratio) {
            return Err(InspectionDiagnostic::CompressionRatioLimit);
        }

        let data_start = header_end;
        let data_end = data_start
            .checked_add(usize::try_from(size).map_err(|_| InspectionDiagnostic::MemberSizeLimit)?)
            .ok_or(InspectionDiagnostic::TarTruncated)?;
        let payload = bytes
            .get(data_start..data_end)
            .ok_or(InspectionDiagnostic::TarTruncated)?;
        let padded_size = usize::try_from(size)
            .map_err(|_| InspectionDiagnostic::MemberSizeLimit)?
            .checked_add(TAR_BLOCK_BYTES - 1)
            .ok_or(InspectionDiagnostic::TarTruncated)?
            / TAR_BLOCK_BYTES
            * TAR_BLOCK_BYTES;
        offset = data_start
            .checked_add(padded_size)
            .ok_or(InspectionDiagnostic::TarTruncated)?;
        if offset > bytes.len() {
            return Err(InspectionDiagnostic::TarTruncated);
        }

        let type_flag = header[156];
        let is_directory = type_flag == b'5';
        let path = tar_member_path(header, limits.max_member_name_bytes)?;
        if !seen_paths.insert(path.clone()) {
            return Err(InspectionDiagnostic::InvalidMemberName);
        }
        let member = ContainerMember {
            kind: classify_member(&path, is_directory),
            path,
            // TAR member bytes are already stored in the source allocation.
            compressed_bytes: size,
            declared_uncompressed_bytes: size,
        };
        match type_flag {
            b'\0' | b'0' | b'7' => {
                let member_index = members.len();
                members.push(member);
                dispatchable.push(ParsedTarMember {
                    member_index,
                    payload,
                });
            }
            b'5' => members.push(member),
            // Links, device nodes, PAX/GNU extension headers, and sparse files
            // cannot safely supply a direct bounded child payload.
            b'1' | b'2' => return Err(InspectionDiagnostic::SymlinkMember),
            _ => return Err(InspectionDiagnostic::TarUnsupportedEntry),
        }
    }
    Err(InspectionDiagnostic::TarTruncated)
}

fn validate_tar_checksum(header: &[u8]) -> Result<(), InspectionDiagnostic> {
    if header.len() != 512 {
        return Err(InspectionDiagnostic::TarHeaderInvalid);
    }
    let expected = parse_tar_number(&header[148..156])?;
    let unsigned = header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                u64::from(b' ')
            } else {
                u64::from(*byte)
            }
        })
        .sum::<u64>();
    let signed = header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                i64::from(b' ')
            } else {
                i64::from(*byte as i8)
            }
        })
        .sum::<i64>();
    if expected == unsigned || i64::try_from(expected).ok() == Some(signed) {
        Ok(())
    } else {
        Err(InspectionDiagnostic::TarChecksumInvalid)
    }
}

fn parse_tar_number(field: &[u8]) -> Result<u64, InspectionDiagnostic> {
    let Some(first) = field.first().copied() else {
        return Err(InspectionDiagnostic::TarHeaderInvalid);
    };
    if first & 0x80 != 0 {
        // POSIX base-256 values use bit 7 as the marker. Negative values are
        // not valid for sizes or checksums and are rejected rather than cast.
        if first & 0x40 != 0 {
            return Err(InspectionDiagnostic::TarHeaderInvalid);
        }
        return field[1..]
            .iter()
            .try_fold(u64::from(first & 0x7f), |value, byte| {
                value
                    .checked_mul(256)
                    .and_then(|value| value.checked_add(u64::from(*byte)))
                    .ok_or(InspectionDiagnostic::TarHeaderInvalid)
            });
    }

    let mut value = 0_u64;
    let mut started = false;
    for byte in field {
        match *byte {
            b'0'..=b'7' => {
                started = true;
                value = value
                    .checked_mul(8)
                    .and_then(|value| value.checked_add(u64::from(*byte - b'0')))
                    .ok_or(InspectionDiagnostic::TarHeaderInvalid)?;
            }
            b' ' | b'\0' if !started => {}
            b' ' | b'\0' => break,
            _ => return Err(InspectionDiagnostic::TarHeaderInvalid),
        }
    }
    Ok(value)
}

fn tar_member_path(header: &[u8], maximum_bytes: usize) -> Result<String, InspectionDiagnostic> {
    let name = tar_field_string(&header[..100])?;
    let prefix = tar_field_string(&header[345..500])?;
    let raw = if prefix.is_empty() {
        name
    } else if name.is_empty() {
        prefix
    } else {
        format!("{prefix}/{name}")
    };
    normalized_member_path(&raw, maximum_bytes).ok_or(InspectionDiagnostic::InvalidMemberName)
}

fn tar_field_string(field: &[u8]) -> Result<String, InspectionDiagnostic> {
    let field = field.split(|byte| *byte == 0).next().unwrap_or_default();
    std::str::from_utf8(field)
        .map(str::to_owned)
        .map_err(|_| InspectionDiagnostic::InvalidMemberName)
}

fn inspect_bzip2(bytes: &[u8], limits: ContainerLimits) -> ContainerInspection {
    if bytes.len() < 4 || !bytes.starts_with(b"BZh") || !matches!(bytes[3], b'1'..=b'9') {
        return rejected_container(ArchiveKind::Bzip2, InspectionDiagnostic::Bzip2HeaderInvalid);
    }
    opaque_compressed_inventory(ArchiveKind::Bzip2, "bzip2-stream", bytes, limits)
}

fn inspect_xz(bytes: &[u8], limits: ContainerLimits) -> ContainerInspection {
    if bytes.len() < 12
        || !bytes.starts_with(b"\xfd7zXZ\0")
        || bytes[6] != 0
        || bytes[7] & 0xf0 != 0
    {
        return rejected_container(ArchiveKind::Xz, InspectionDiagnostic::XzHeaderInvalid);
    }
    let Some(expected_crc) = read_u32_le(bytes, 8) else {
        return rejected_container(ArchiveKind::Xz, InspectionDiagnostic::XzHeaderInvalid);
    };
    if crc32fast::hash(&bytes[6..8]) != expected_crc {
        return rejected_container(ArchiveKind::Xz, InspectionDiagnostic::XzHeaderInvalid);
    }
    opaque_compressed_inventory(ArchiveKind::Xz, "xz-stream", bytes, limits)
}

fn inspect_zstd(bytes: &[u8], limits: ContainerLimits) -> ContainerInspection {
    let Some(header) = zstd_frame_header(bytes) else {
        return rejected_container(ArchiveKind::Zstd, InspectionDiagnostic::ZstdHeaderInvalid);
    };
    if header
        .window_size
        .is_some_and(|window_size| window_size > limits.max_member_uncompressed_bytes)
    {
        return rejected_container(ArchiveKind::Zstd, InspectionDiagnostic::MemberSizeLimit);
    }
    let Some(declared_size) = header.declared_size else {
        return ContainerInspection {
            kind: ArchiveKind::Zstd,
            status: InspectionStatus::InventoryOnly,
            members: Vec::new(),
            decompressed_bytes: 0,
            diagnostics: vec![InspectionDiagnostic::DeclaredSizeUnavailable],
        };
    };
    if declared_size > limits.max_member_uncompressed_bytes {
        return rejected_container(ArchiveKind::Zstd, InspectionDiagnostic::MemberSizeLimit);
    }
    if declared_size > limits.max_total_uncompressed_bytes {
        return rejected_container(ArchiveKind::Zstd, InspectionDiagnostic::TotalSizeLimit);
    }
    if compression_ratio_exceeded(
        declared_size,
        bytes.len() as u64,
        limits.max_compression_ratio,
    ) {
        return rejected_container(
            ArchiveKind::Zstd,
            InspectionDiagnostic::CompressionRatioLimit,
        );
    }
    ContainerInspection {
        kind: ArchiveKind::Zstd,
        status: InspectionStatus::InventoryOnly,
        members: vec![ContainerMember {
            path: "zstd-frame".into(),
            kind: ContainerMemberKind::File,
            compressed_bytes: bytes.len() as u64,
            declared_uncompressed_bytes: declared_size,
        }],
        decompressed_bytes: 0,
        diagnostics: vec![InspectionDiagnostic::DecoderUnavailable],
    }
}

fn opaque_compressed_inventory(
    kind: ArchiveKind,
    stream_name: &str,
    bytes: &[u8],
    _limits: ContainerLimits,
) -> ContainerInspection {
    ContainerInspection {
        kind,
        status: InspectionStatus::InventoryOnly,
        members: vec![ContainerMember {
            path: stream_name.into(),
            kind: ContainerMemberKind::File,
            compressed_bytes: bytes.len() as u64,
            // A zero value is not used as a size claim: the accompanying
            // diagnostic states that the codec has no trustworthy declaration.
            declared_uncompressed_bytes: 0,
        }],
        decompressed_bytes: 0,
        diagnostics: vec![
            InspectionDiagnostic::DeclaredSizeUnavailable,
            InspectionDiagnostic::DecoderUnavailable,
        ],
    }
}

struct ZstdFrameHeader {
    declared_size: Option<u64>,
    window_size: Option<u64>,
}

/// Parses only the bounded zstd frame header. The payload itself is not
/// decoded: the caller can enforce declared-size, window, and ratio limits
/// without allocating a decompression window.
fn zstd_frame_header(bytes: &[u8]) -> Option<ZstdFrameHeader> {
    if !bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return None;
    }
    let descriptor = *bytes.get(4)?;
    if descriptor & 0x18 != 0 {
        return None;
    }
    let single_segment = descriptor & 0x20 != 0;
    let dictionary_size = match descriptor & 0x03 {
        0 => 0,
        1 => 1,
        2 => 2,
        _ => 4,
    };
    let frame_size_flag = descriptor >> 6;
    let content_size_bytes = match frame_size_flag {
        0 if single_segment => 1,
        0 => 0,
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let window_size = if single_segment {
        None
    } else {
        let descriptor = *bytes.get(5)?;
        let exponent = u32::from(descriptor >> 3);
        let base = 1_u64.checked_shl(10 + exponent)?;
        let add = (base >> 3).checked_mul(u64::from(descriptor & 0x07))?;
        Some(base.checked_add(add)?)
    };
    let window_descriptor_bytes = usize::from(!single_segment);
    let content_offset = 5usize
        .checked_add(window_descriptor_bytes)?
        .checked_add(dictionary_size)?;
    let content = bytes.get(content_offset..content_offset.checked_add(content_size_bytes)?)?;
    let declared_size = match content_size_bytes {
        0 => None,
        1 => Some(u64::from(content[0])),
        2 => Some(u64::from(u16::from_le_bytes([content[0], content[1]])) + 256),
        4 => Some(u64::from(u32::from_le_bytes(content.try_into().ok()?))),
        8 => Some(u64::from_le_bytes(content.try_into().ok()?)),
        _ => return None,
    };
    Some(ZstdFrameHeader {
        declared_size,
        window_size,
    })
}

fn rejected_media(kind: MediaKind, diagnostic: InspectionDiagnostic) -> MediaInspection {
    MediaInspection {
        kind,
        status: InspectionStatus::Rejected,
        metadata: None,
        svg: None,
        diagnostics: vec![diagnostic],
    }
}

fn detect_archive_kind(source_name: &str, bytes: &[u8]) -> Option<ArchiveKind> {
    if looks_like_zip(bytes) {
        return Some(ArchiveKind::Zip);
    }
    if looks_like_gzip(bytes) {
        return Some(ArchiveKind::Gzip);
    }
    if bytes.starts_with(b"BZh") {
        return Some(ArchiveKind::Bzip2);
    }
    if bytes.starts_with(b"\xfd7zXZ\0") {
        return Some(ArchiveKind::Xz);
    }
    if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return Some(ArchiveKind::Zstd);
    }
    if bytes.starts_with(b"7z\xbc\xaf\x27\x1c") {
        return Some(ArchiveKind::SevenZip);
    }
    if bytes.starts_with(b"Rar!\x1a\x07\x00") || bytes.starts_with(b"Rar!\x1a\x07\x01\x00") {
        return Some(ArchiveKind::Rar);
    }
    if bytes.len() >= 262 && bytes.get(257..262) == Some(b"ustar") {
        return Some(ArchiveKind::Tar);
    }

    let lower = source_name.to_ascii_lowercase();
    if lower.ends_with(".tar") {
        Some(ArchiveKind::Tar)
    } else if lower.ends_with(".bz2") || lower.ends_with(".tbz") || lower.ends_with(".tbz2") {
        Some(ArchiveKind::Bzip2)
    } else if lower.ends_with(".xz") || lower.ends_with(".txz") {
        Some(ArchiveKind::Xz)
    } else if lower.ends_with(".zst") || lower.ends_with(".zstd") {
        Some(ArchiveKind::Zstd)
    } else if lower.ends_with(".7z") {
        Some(ArchiveKind::SevenZip)
    } else if lower.ends_with(".rar") {
        Some(ArchiveKind::Rar)
    } else if lower.ends_with(".zip")
        || lower.ends_with(".docx")
        || lower.ends_with(".xlsx")
        || lower.ends_with(".pptx")
        || lower.ends_with(".vsdx")
        || lower.ends_with(".odt")
        || lower.ends_with(".ods")
        || lower.ends_with(".odp")
    {
        Some(ArchiveKind::Zip)
    } else if lower.ends_with(".gz") || lower.ends_with(".tgz") {
        Some(ArchiveKind::Gzip)
    } else {
        None
    }
}

fn detect_media_kind(source_name: &str, bytes: &[u8]) -> Option<MediaKind> {
    if looks_like_svg(bytes) {
        return Some(MediaKind::Svg);
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(MediaKind::Png);
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some(MediaKind::Jpeg);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(MediaKind::Gif);
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return Some(MediaKind::Webp);
    }
    if bytes.starts_with(b"BM") {
        return Some(MediaKind::Bmp);
    }
    if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        return Some(MediaKind::Tiff);
    }
    if bytes.starts_with(&[0, 0, 1, 0]) {
        return Some(MediaKind::Ico);
    }
    if looks_like_isobmff(bytes) {
        return Some(
            if has_bmff_brand(bytes, b"avif") || has_bmff_brand(bytes, b"avis") {
                MediaKind::Avif
            } else {
                MediaKind::Heif
            },
        );
    }

    let lower = source_name.to_ascii_lowercase();
    if lower.ends_with(".svg") {
        Some(MediaKind::Svg)
    } else if lower.ends_with(".svgz") {
        Some(MediaKind::Svgz)
    } else if lower.ends_with(".png") {
        Some(MediaKind::Png)
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        Some(MediaKind::Jpeg)
    } else if lower.ends_with(".gif") {
        Some(MediaKind::Gif)
    } else if lower.ends_with(".webp") {
        Some(MediaKind::Webp)
    } else if lower.ends_with(".bmp") {
        Some(MediaKind::Bmp)
    } else if lower.ends_with(".avif") {
        Some(MediaKind::Avif)
    } else if lower.ends_with(".heic") || lower.ends_with(".heif") || lower.ends_with(".heifs") {
        Some(MediaKind::Heif)
    } else if lower.ends_with(".tif") || lower.ends_with(".tiff") {
        Some(MediaKind::Tiff)
    } else if lower.ends_with(".ico") {
        Some(MediaKind::Ico)
    } else {
        None
    }
}

fn is_svgz_name(source_name: &str) -> bool {
    source_name.to_ascii_lowercase().ends_with(".svgz")
}

fn looks_like_zip(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
}

fn looks_like_gzip(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x1f, 0x8b])
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let probe = &bytes[..cmp::min(bytes.len(), 4 * 1024)];
    let mut offset = if probe.starts_with(&[0xef, 0xbb, 0xbf]) {
        3
    } else {
        0
    };
    offset += probe[offset..]
        .iter()
        .take_while(|byte| byte.is_ascii_whitespace())
        .count();
    if probe
        .get(offset..)
        .is_some_and(|suffix| suffix.starts_with(b"<?xml"))
    {
        let Some(end) = crate::bytes::find_subslice(&probe[offset..], b"?>") else {
            return false;
        };
        offset += end + 2;
        offset += probe[offset..]
            .iter()
            .take_while(|byte| byte.is_ascii_whitespace())
            .count();
    }
    let Some(after_tag) = probe
        .get(offset..)
        .and_then(|suffix| suffix.strip_prefix(b"<svg"))
    else {
        return false;
    };
    after_tag
        .first()
        .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(*byte, b'/' | b'>'))
}

fn looks_like_isobmff(bytes: &[u8]) -> bool {
    bytes.get(4..8) == Some(b"ftyp") && bytes.len() >= 16
}

fn has_bmff_brand(bytes: &[u8], brand: &[u8; 4]) -> bool {
    bytes.get(8..12) == Some(brand)
        || bytes
            .get(16..cmp::min(bytes.len(), 64))
            .is_some_and(|brands| brands.windows(4).any(|candidate| candidate == brand))
}

#[derive(Debug, Clone, Copy)]
struct ZipPreflight {
    entries: usize,
}

struct ValidatedZip<'a> {
    archive: zip::ZipArchive<Cursor<&'a [u8]>>,
    members: Vec<ContainerMember>,
    /// Archive offsets ordered by normalized member path. This prevents the
    /// archive writer's central-directory order from perturbing recursive
    /// extraction fact order.
    dispatch_order: Vec<usize>,
}

fn zip_compression_is_supported(method: zip::CompressionMethod) -> bool {
    method == zip::CompressionMethod::STORE || method == zip::CompressionMethod::DEFLATE
}

fn zip_nonregular_entry(
    is_directory: bool,
    is_symlink: bool,
    unix_mode: Option<u32>,
    declared_size: u64,
) -> Option<InspectionDiagnostic> {
    if is_symlink {
        return Some(InspectionDiagnostic::SymlinkMember);
    }
    // ZIP creators may omit Unix type bits. When they are present, require the
    // pathname spelling and file type to agree and reject devices, sockets,
    // FIFOs, and other link-like entries before a decoder can be constructed.
    const UNIX_FILE_TYPE_MASK: u32 = 0o170_000;
    const UNIX_DIRECTORY: u32 = 0o040_000;
    const UNIX_REGULAR_FILE: u32 = 0o100_000;
    if let Some(mode) = unix_mode {
        let file_type = mode & UNIX_FILE_TYPE_MASK;
        let expected = if is_directory {
            UNIX_DIRECTORY
        } else {
            UNIX_REGULAR_FILE
        };
        if file_type != 0 && file_type != expected {
            return Some(InspectionDiagnostic::NonRegularMember);
        }
    }
    (is_directory && declared_size != 0).then_some(InspectionDiagnostic::NonRegularMember)
}

/// Open and validate ZIP metadata before any member payload allocation.
///
/// Invalid, encrypted, symlink, or unsupported-compression entries never
/// become dispatchable child bytes. The returned inspection on failure is
/// already the stable result the caller should report.
fn validated_zip<'a>(
    bytes: &'a [u8],
    limits: ContainerLimits,
) -> Result<ValidatedZip<'a>, ContainerInspection> {
    let preflight = match preflight_zip(bytes, limits) {
        Ok(preflight) => preflight,
        Err(diagnostic) => return Err(rejected_container(ArchiveKind::Zip, diagnostic)),
    };
    let mut archive = match zip::ZipArchive::new(Cursor::new(bytes)) {
        Ok(archive) => archive,
        Err(_) => {
            return Err(rejected_container(
                ArchiveKind::Zip,
                InspectionDiagnostic::InvalidArchive,
            ))
        }
    };
    if archive.len() != preflight.entries {
        return Err(rejected_container(
            ArchiveKind::Zip,
            InspectionDiagnostic::InvalidArchive,
        ));
    }

    let mut members = Vec::with_capacity(archive.len());
    let mut seen_paths = BTreeSet::new();
    let mut unsafe_entries = Vec::new();
    let mut declared_total = 0_u64;
    let mut compressed_total = 0_u64;
    for index in 0..archive.len() {
        let member = match archive.by_index_raw(index) {
            Ok(member) => member,
            Err(_) => {
                return Err(rejected_container(
                    ArchiveKind::Zip,
                    InspectionDiagnostic::InvalidArchive,
                ))
            }
        };
        if member.name().len() > limits.max_member_name_bytes {
            return Err(rejected_container(
                ArchiveKind::Zip,
                InspectionDiagnostic::MemberNameLimit,
            ));
        }
        let Some(path) = normalized_member_path(member.name(), limits.max_member_name_bytes) else {
            return Err(rejected_container(
                ArchiveKind::Zip,
                InspectionDiagnostic::InvalidMemberName,
            ));
        };
        if !seen_paths.insert(path.clone()) {
            return Err(rejected_container(
                ArchiveKind::Zip,
                InspectionDiagnostic::InvalidMemberName,
            ));
        };
        let declared = member.size();
        let compressed = member.compressed_size();
        let unsafe_diagnostic = if member.encrypted() {
            Some(InspectionDiagnostic::EncryptedMember)
        } else if let Some(diagnostic) = zip_nonregular_entry(
            member.is_dir(),
            member.is_symlink(),
            member.unix_mode(),
            declared,
        ) {
            Some(diagnostic)
        } else if !zip_compression_is_supported(member.compression()) {
            Some(InspectionDiagnostic::UnsupportedCompression)
        } else {
            None
        };
        if let Some(diagnostic) = unsafe_diagnostic {
            unsafe_entries.push((path.clone(), diagnostic));
        }
        if declared > limits.max_member_uncompressed_bytes {
            return Err(rejected_container(
                ArchiveKind::Zip,
                InspectionDiagnostic::MemberSizeLimit,
            ));
        }
        declared_total = match declared_total.checked_add(declared) {
            Some(total) if total <= limits.max_total_uncompressed_bytes => total,
            _ => {
                return Err(rejected_container(
                    ArchiveKind::Zip,
                    InspectionDiagnostic::TotalSizeLimit,
                ))
            }
        };
        compressed_total = match compressed_total.checked_add(compressed) {
            Some(total) => total,
            None => {
                return Err(rejected_container(
                    ArchiveKind::Zip,
                    InspectionDiagnostic::CompressionRatioLimit,
                ))
            }
        };
        if compression_ratio_exceeded(declared, compressed, limits.max_compression_ratio) {
            return Err(rejected_container(
                ArchiveKind::Zip,
                InspectionDiagnostic::CompressionRatioLimit,
            ));
        }
        members.push(ContainerMember {
            path: path.clone(),
            kind: classify_member(&path, member.is_dir()),
            compressed_bytes: compressed,
            declared_uncompressed_bytes: declared,
        });
    }
    if !unsafe_entries.is_empty() {
        unsafe_entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        return Err(rejected_container(ArchiveKind::Zip, unsafe_entries[0].1));
    }
    if compression_ratio_exceeded(
        declared_total,
        compressed_total,
        limits.max_compression_ratio,
    ) {
        return Err(rejected_container(
            ArchiveKind::Zip,
            InspectionDiagnostic::CompressionRatioLimit,
        ));
    }

    let mut dispatch_order = (0..members.len()).collect::<Vec<_>>();
    dispatch_order.sort_unstable_by(|left, right| members[*left].path.cmp(&members[*right].path));
    Ok(ValidatedZip {
        archive,
        members,
        dispatch_order,
    })
}

fn inspect_zip(bytes: &[u8], limits: ContainerLimits) -> ContainerInspection {
    let mut validated = match validated_zip(bytes, limits) {
        Ok(validated) => validated,
        Err(inspection) => return inspection,
    };

    let mut actual_total = 0_u64;
    let mut buffer = [0_u8; READ_BUFFER_BYTES];
    for index in 0..validated.archive.len() {
        let declared = validated.members[index].declared_uncompressed_bytes;
        let mut member = match validated.archive.by_index(index) {
            Ok(member) => member,
            Err(_) => {
                return rejected_container(
                    ArchiveKind::Zip,
                    InspectionDiagnostic::DecompressionFailed,
                )
            }
        };
        let mut actual_member = 0_u64;
        loop {
            let read = match member.read(&mut buffer) {
                Ok(read) => read,
                Err(_) => {
                    return rejected_container(
                        ArchiveKind::Zip,
                        InspectionDiagnostic::DecompressionFailed,
                    )
                }
            };
            if read == 0 {
                break;
            }
            actual_member = match actual_member.checked_add(read as u64) {
                Some(total) if total <= limits.max_member_uncompressed_bytes => total,
                _ => {
                    return rejected_container(
                        ArchiveKind::Zip,
                        InspectionDiagnostic::MemberSizeLimit,
                    )
                }
            };
            actual_total = match actual_total.checked_add(read as u64) {
                Some(total) if total <= limits.max_total_uncompressed_bytes => total,
                _ => {
                    return rejected_container(
                        ArchiveKind::Zip,
                        InspectionDiagnostic::TotalSizeLimit,
                    )
                }
            };
        }
        if actual_member != declared {
            return rejected_container(
                ArchiveKind::Zip,
                InspectionDiagnostic::DeclaredSizeMismatch,
            );
        }
    }
    ContainerInspection {
        kind: ArchiveKind::Zip,
        status: InspectionStatus::Parsed,
        members: validated.members,
        decompressed_bytes: actual_total,
        diagnostics: Vec::new(),
    }
}

fn preflight_zip(
    bytes: &[u8],
    limits: ContainerLimits,
) -> Result<ZipPreflight, InspectionDiagnostic> {
    let eocd = find_zip_eocd(bytes).ok_or(InspectionDiagnostic::InvalidArchive)?;
    let disk = read_u16_le_at(bytes, eocd, 4).ok_or(InspectionDiagnostic::InvalidArchive)?;
    let directory_disk =
        read_u16_le_at(bytes, eocd, 6).ok_or(InspectionDiagnostic::InvalidArchive)?;
    let entries_on_disk =
        read_u16_le_at(bytes, eocd, 8).ok_or(InspectionDiagnostic::InvalidArchive)?;
    let entries = read_u16_le_at(bytes, eocd, 10).ok_or(InspectionDiagnostic::InvalidArchive)?;
    let directory_size =
        read_u32_le_at(bytes, eocd, 12).ok_or(InspectionDiagnostic::InvalidArchive)?;
    let directory_offset =
        read_u32_le_at(bytes, eocd, 16).ok_or(InspectionDiagnostic::InvalidArchive)?;
    if disk != 0 || directory_disk != 0 || entries_on_disk != entries {
        return Err(InspectionDiagnostic::MultiDiskZipUnsupported);
    }

    let (entries, directory_size, directory_offset) =
        if entries == u16::MAX || directory_size == u32::MAX || directory_offset == u32::MAX {
            preflight_zip64(bytes, eocd)?
        } else {
            (
                u64::from(entries),
                u64::from(directory_size),
                u64::from(directory_offset),
            )
        };
    let entries = usize::try_from(entries).map_err(|_| InspectionDiagnostic::MemberLimit)?;
    if entries > limits.max_members {
        return Err(InspectionDiagnostic::MemberLimit);
    }
    let directory_size =
        usize::try_from(directory_size).map_err(|_| InspectionDiagnostic::CentralDirectoryLimit)?;
    if directory_size > limits.max_central_directory_bytes {
        return Err(InspectionDiagnostic::CentralDirectoryLimit);
    }
    let directory_offset = usize::try_from(directory_offset)
        .map_err(|_| InspectionDiagnostic::CentralDirectoryLimit)?;
    let directory_end = directory_offset
        .checked_add(directory_size)
        .ok_or(InspectionDiagnostic::CentralDirectoryLimit)?;
    if directory_end > eocd {
        return Err(InspectionDiagnostic::InvalidArchive);
    }
    Ok(ZipPreflight { entries })
}

/// Validate the bounded ZIP metadata needed before a general-purpose ZIP
/// reader is allowed to materialize its file table.
///
/// Legacy Office conversion owns its raw-byte admission separately, but it
/// shares this preflight so a forged classic or ZIP64 entry count cannot make
/// `ZipArchive::new` allocate beyond the caller's member policy first.
pub(crate) fn preflight_zip_metadata_with_limits(
    bytes: &[u8],
    max_members: usize,
    max_central_directory_bytes: usize,
) -> bool {
    if max_members == 0 || max_central_directory_bytes == 0 {
        return false;
    }
    preflight_zip(
        bytes,
        ContainerLimits {
            max_members,
            max_central_directory_bytes,
            ..ContainerLimits::default()
        },
    )
    .is_ok()
}

fn preflight_zip64(bytes: &[u8], eocd: usize) -> Result<(u64, u64, u64), InspectionDiagnostic> {
    let locator = eocd
        .checked_sub(20)
        .filter(|offset| checked_slice(bytes, *offset, 4) == Some(ZIP64_LOCATOR_SIGNATURE))
        .ok_or(InspectionDiagnostic::Zip64MetadataInvalid)?;
    let locator_disk =
        read_u32_le_at(bytes, locator, 4).ok_or(InspectionDiagnostic::Zip64MetadataInvalid)?;
    let record_offset =
        read_u64_le_at(bytes, locator, 8).ok_or(InspectionDiagnostic::Zip64MetadataInvalid)?;
    let total_disks =
        read_u32_le_at(bytes, locator, 16).ok_or(InspectionDiagnostic::Zip64MetadataInvalid)?;
    if locator_disk != 0 || total_disks != 1 {
        return Err(InspectionDiagnostic::MultiDiskZipUnsupported);
    }
    let record_offset =
        usize::try_from(record_offset).map_err(|_| InspectionDiagnostic::Zip64MetadataInvalid)?;
    if checked_slice(bytes, record_offset, 4) != Some(ZIP64_EOCD_SIGNATURE) {
        return Err(InspectionDiagnostic::Zip64MetadataInvalid);
    }
    let record_size = read_u64_le_at(bytes, record_offset, 4)
        .ok_or(InspectionDiagnostic::Zip64MetadataInvalid)?;
    let record_end = record_offset
        .checked_add(12)
        .and_then(|offset| offset.checked_add(usize::try_from(record_size).ok()?))
        .ok_or(InspectionDiagnostic::Zip64MetadataInvalid)?;
    if record_size < 44 || record_end > locator {
        return Err(InspectionDiagnostic::Zip64MetadataInvalid);
    }
    let disk = read_u32_le_at(bytes, record_offset, 16)
        .ok_or(InspectionDiagnostic::Zip64MetadataInvalid)?;
    let directory_disk = read_u32_le_at(bytes, record_offset, 20)
        .ok_or(InspectionDiagnostic::Zip64MetadataInvalid)?;
    let entries_on_disk = read_u64_le_at(bytes, record_offset, 24)
        .ok_or(InspectionDiagnostic::Zip64MetadataInvalid)?;
    let entries = read_u64_le_at(bytes, record_offset, 32)
        .ok_or(InspectionDiagnostic::Zip64MetadataInvalid)?;
    let directory_size = read_u64_le_at(bytes, record_offset, 40)
        .ok_or(InspectionDiagnostic::Zip64MetadataInvalid)?;
    let directory_offset = read_u64_le_at(bytes, record_offset, 48)
        .ok_or(InspectionDiagnostic::Zip64MetadataInvalid)?;
    if disk != 0 || directory_disk != 0 || entries_on_disk != entries {
        return Err(InspectionDiagnostic::MultiDiskZipUnsupported);
    }
    Ok((entries, directory_size, directory_offset))
}

fn find_zip_eocd(bytes: &[u8]) -> Option<usize> {
    let start = bytes.len().saturating_sub(65_557);
    (start..bytes.len().saturating_sub(3)).rev().find(|offset| {
        checked_slice(bytes, *offset, 4) == Some(ZIP_EOCD_SIGNATURE)
            && read_u16_le_at(bytes, *offset, 20).is_some_and(|comment_len| {
                offset
                    .checked_add(22)
                    .and_then(|end| end.checked_add(usize::from(comment_len)))
                    == Some(bytes.len())
            })
    })
}

fn checked_slice(bytes: &[u8], offset: usize, len: usize) -> Option<&[u8]> {
    bytes.get(offset..offset.checked_add(len)?)
}

fn read_u16_le_at(bytes: &[u8], base: usize, relative: usize) -> Option<u16> {
    read_u16_le(bytes, base.checked_add(relative)?)
}

fn read_u32_le_at(bytes: &[u8], base: usize, relative: usize) -> Option<u32> {
    read_u32_le(bytes, base.checked_add(relative)?)
}

fn read_u64_le_at(bytes: &[u8], base: usize, relative: usize) -> Option<u64> {
    read_u64_le(bytes, base.checked_add(relative)?)
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Option<u16> {
    let bytes = checked_slice(bytes, offset, 2)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    let bytes = checked_slice(bytes, offset, 4)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u32_be(bytes: &[u8], offset: usize) -> Option<u32> {
    let bytes = checked_slice(bytes, offset, 4)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64_le(bytes: &[u8], offset: usize) -> Option<u64> {
    let bytes = checked_slice(bytes, offset, 8)?;
    Some(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

fn normalized_member_path(name: &str, max_bytes: usize) -> Option<String> {
    if name.is_empty()
        || name.len() > max_bytes
        || name.contains('\0')
        || name.starts_with('/')
        || name.starts_with('\\')
    {
        return None;
    }
    let mut normalized = String::with_capacity(name.len());
    let slash_normalized = name.replace('\\', "/");
    for raw_component in slash_normalized.split('/') {
        if raw_component.is_empty() {
            continue;
        }
        let component = raw_component.nfc().collect::<String>();
        if matches!(component.as_str(), "." | "..") || component.contains(':') {
            return None;
        }
        if !normalized.is_empty() {
            normalized.push('/');
        }
        normalized.push_str(&component);
    }
    (!normalized.is_empty()
        && normalized.len() <= max_bytes
        // `!/` is the canonical virtual-container boundary. Accepting the
        // same spelling inside one member would let a flat archive path
        // collide with a genuinely nested provenance path.
        && !normalized.contains("!/"))
    .then_some(normalized)
}

fn classify_member(path: &str, is_directory: bool) -> ContainerMemberKind {
    if is_directory {
        return ContainerMemberKind::Directory;
    }
    let lower = path.to_ascii_lowercase();
    if lower.starts_with("word/")
        || lower.starts_with("xl/")
        || lower.starts_with("ppt/")
        || lower.starts_with("visio/")
        || lower.starts_with("docprops/")
        || matches!(lower.as_str(), "[content_types].xml" | "_rels/.rels")
    {
        ContainerMemberKind::OfficePart
    } else if lower.ends_with(".drawio")
        || lower.ends_with(".drawio.xml")
        || matches!(lower.as_str(), "diagram.xml" | "content.xml")
    {
        ContainerMemberKind::DrawioPart
    } else if lower.ends_with(".svg") || lower.ends_with(".svgz") {
        ContainerMemberKind::Svg
    } else if matches!(
        lower.rsplit('.').next(),
        Some(
            "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "webp"
                | "avif"
                | "heic"
                | "heif"
                | "bmp"
                | "tif"
                | "tiff"
                | "ico"
        )
    ) {
        ContainerMemberKind::RasterImage
    } else if matches!(
        lower.rsplit('.').next(),
        Some("zip" | "gz" | "tgz" | "tar" | "bz2" | "xz" | "zst" | "zstd" | "7z" | "rar")
    ) {
        ContainerMemberKind::NestedContainer
    } else {
        ContainerMemberKind::File
    }
}

fn compression_ratio_exceeded(uncompressed: u64, compressed: u64, maximum_ratio: u64) -> bool {
    if compressed == 0 {
        uncompressed != 0
    } else {
        uncompressed > compressed.saturating_mul(maximum_ratio)
    }
}

fn inspect_gzip(bytes: &[u8], limits: ContainerLimits) -> ContainerInspection {
    let header_name = match gzip_header_name(bytes, limits.max_member_name_bytes) {
        Ok(name) => name,
        Err(diagnostic) => return rejected_container(ArchiveKind::Gzip, diagnostic),
    };
    let decompressed = match consume_single_gzip(bytes, limits, None) {
        Ok(total) => total,
        Err(diagnostic) => return rejected_container(ArchiveKind::Gzip, diagnostic),
    };
    let path = header_name.unwrap_or_else(|| "gzip-stream".to_owned());
    ContainerInspection {
        kind: ArchiveKind::Gzip,
        status: InspectionStatus::Parsed,
        members: vec![ContainerMember {
            kind: classify_member(&path, false),
            path,
            compressed_bytes: bytes.len() as u64,
            declared_uncompressed_bytes: decompressed,
        }],
        decompressed_bytes: decompressed,
        diagnostics: Vec::new(),
    }
}

fn gzip_declared_size(bytes: &[u8]) -> Result<u64, InspectionDiagnostic> {
    // A minimal GZIP stream has a ten-byte header and an eight-byte trailer.
    // ISIZE is modulo 2^32, but every admitted member is capped well below
    // that range. Exact-size decoding below rejects a forged wrapped value
    // before allocating or reading one byte beyond the admitted buffer.
    if bytes.len() < 18 {
        return Err(InspectionDiagnostic::GzipHeaderInvalid);
    }
    read_u32_le(bytes, bytes.len() - 4)
        .map(u64::from)
        .ok_or(InspectionDiagnostic::GzipHeaderInvalid)
}

fn inferred_gzip_member_path(source_name: &str, maximum_name_bytes: usize) -> Option<String> {
    let normalized_source = source_name.replace('\\', "/");
    let name = normalized_source.rsplit('/').next()?;
    let lower = name.to_ascii_lowercase();
    let inferred = if lower.ends_with(".tgz") {
        let stem = name.get(..name.len().checked_sub(4)?)?;
        (!stem.is_empty()).then(|| format!("{stem}.tar"))?
    } else if lower.ends_with(".gz") {
        name.get(..name.len().checked_sub(3)?)?.to_owned()
    } else {
        return None;
    };
    normalized_member_path(&inferred, maximum_name_bytes)
}

fn gzip_header_name(
    bytes: &[u8],
    maximum_name_bytes: usize,
) -> Result<Option<String>, InspectionDiagnostic> {
    if bytes.len() < 10 || !looks_like_gzip(bytes) || bytes[2] != 8 || bytes[3] & 0xe0 != 0 {
        return Err(InspectionDiagnostic::GzipHeaderInvalid);
    }
    let flags = bytes[3];
    let mut offset = 10_usize;
    if flags & 0x04 != 0 {
        let extra_len =
            usize::from(read_u16_le(bytes, offset).ok_or(InspectionDiagnostic::GzipHeaderInvalid)?);
        offset = offset
            .checked_add(2)
            .and_then(|value| value.checked_add(extra_len))
            .ok_or(InspectionDiagnostic::GzipHeaderInvalid)?;
        if offset > bytes.len() || extra_len > maximum_name_bytes {
            return Err(InspectionDiagnostic::GzipHeaderLimit);
        }
    }
    let filename = if flags & 0x08 != 0 {
        let name = read_zero_terminated(bytes, &mut offset, maximum_name_bytes)?;
        Some(
            normalized_member_path(&name, maximum_name_bytes)
                .ok_or(InspectionDiagnostic::InvalidMemberName)?,
        )
    } else {
        None
    };
    if flags & 0x10 != 0 {
        let _ = read_zero_terminated(bytes, &mut offset, maximum_name_bytes)?;
    }
    if flags & 0x02 != 0 {
        offset = offset
            .checked_add(2)
            .ok_or(InspectionDiagnostic::GzipHeaderInvalid)?;
        if offset > bytes.len() {
            return Err(InspectionDiagnostic::GzipHeaderInvalid);
        }
    }
    Ok(filename)
}

fn read_zero_terminated(
    bytes: &[u8],
    offset: &mut usize,
    limit: usize,
) -> Result<String, InspectionDiagnostic> {
    let remaining = bytes
        .get(*offset..)
        .ok_or(InspectionDiagnostic::GzipHeaderInvalid)?;
    let Some(length) = remaining.iter().position(|byte| *byte == 0) else {
        return Err(InspectionDiagnostic::GzipHeaderInvalid);
    };
    if length > limit {
        return Err(InspectionDiagnostic::GzipHeaderLimit);
    }
    let value = std::str::from_utf8(&remaining[..length])
        .map_err(|_| InspectionDiagnostic::GzipHeaderInvalid)?
        .to_owned();
    *offset = offset
        .checked_add(length + 1)
        .ok_or(InspectionDiagnostic::GzipHeaderInvalid)?;
    Ok(value)
}

fn consume_single_gzip(
    bytes: &[u8],
    limits: ContainerLimits,
    mut output: Option<&mut Vec<u8>>,
) -> Result<u64, InspectionDiagnostic> {
    let mut decoder = GzDecoder::new(bytes);
    let mut total = 0_u64;
    let mut buffer = [0_u8; READ_BUFFER_BYTES];
    loop {
        let read = decoder
            .read(&mut buffer)
            .map_err(|_| InspectionDiagnostic::DecompressionFailed)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or(InspectionDiagnostic::TotalSizeLimit)?;
        if total > limits.max_total_uncompressed_bytes
            || total > limits.max_member_uncompressed_bytes
        {
            return Err(InspectionDiagnostic::TotalSizeLimit);
        }
        if total > limits.max_svg_bytes as u64 && output.is_some() {
            return Err(InspectionDiagnostic::InputTooLarge);
        }
        if compression_ratio_exceeded(total, bytes.len() as u64, limits.max_compression_ratio) {
            return Err(InspectionDiagnostic::CompressionRatioLimit);
        }
        if let Some(output) = output.as_deref_mut() {
            output.extend_from_slice(&buffer[..read]);
        }
    }
    let trailing = decoder.into_inner();
    if !trailing.is_empty() {
        return Err(if looks_like_gzip(trailing) {
            InspectionDiagnostic::GzipMultipleMembers
        } else {
            InspectionDiagnostic::GzipTrailingBytes
        });
    }
    Ok(total)
}

fn inspect_svg(bytes: &[u8], limits: ContainerLimits) -> MediaInspection {
    if bytes.len() > limits.max_svg_bytes {
        return rejected_media(MediaKind::Svg, InspectionDiagnostic::InputTooLarge);
    }
    if let Err(diagnostic) = preflight_svg_tokens(bytes, limits.max_svg_event_bytes) {
        return rejected_media(MediaKind::Svg, diagnostic);
    }
    match parse_svg(Reader::from_reader(bytes), limits) {
        Ok(svg) => MediaInspection {
            kind: MediaKind::Svg,
            status: InspectionStatus::Parsed,
            metadata: None,
            svg: Some(svg),
            diagnostics: Vec::new(),
        },
        Err(diagnostic) => rejected_media(MediaKind::Svg, diagnostic),
    }
}

fn inspect_svgz(bytes: &[u8], recursion_depth: u16, limits: ContainerLimits) -> MediaInspection {
    inspect_svgz_bounded(bytes, recursion_depth, limits, || false, |_| Some(()))
}

/// Decode and inspect SVGZ while a caller-owned aggregate scratch permit is
/// held across allocation, decompression, XML preflight, and SVG parsing.
pub fn inspect_svgz_bounded<Permit, Cancellation, Admission>(
    bytes: &[u8],
    recursion_depth: u16,
    limits: ContainerLimits,
    mut is_cancelled: Cancellation,
    mut admit_scratch: Admission,
) -> MediaInspection
where
    Cancellation: FnMut() -> bool,
    Admission: FnMut(usize) -> Option<Permit>,
{
    if !limits.valid() {
        return rejected_media(MediaKind::Svgz, InspectionDiagnostic::InvalidLimits);
    }
    if recursion_depth > limits.max_recursion_depth {
        return rejected_media(MediaKind::Svgz, InspectionDiagnostic::RecursionLimit);
    }
    if bytes.len() > limits.max_input_bytes {
        return rejected_media(MediaKind::Svgz, InspectionDiagnostic::InputTooLarge);
    }
    if let Err(diagnostic) = gzip_header_name(bytes, limits.max_member_name_bytes) {
        return rejected_media(MediaKind::Svgz, diagnostic);
    }
    let declared = match gzip_declared_size(bytes) {
        Ok(declared) => declared,
        Err(diagnostic) => return rejected_media(MediaKind::Svgz, diagnostic),
    };
    if declared > limits.max_svg_bytes as u64 {
        return rejected_media(MediaKind::Svgz, InspectionDiagnostic::InputTooLarge);
    }
    if compression_ratio_exceeded(declared, bytes.len() as u64, limits.max_compression_ratio) {
        return rejected_media(MediaKind::Svgz, InspectionDiagnostic::CompressionRatioLimit);
    }
    let payload_len = match usize::try_from(declared) {
        Ok(length) => length,
        Err(_) => return rejected_media(MediaKind::Svgz, InspectionDiagnostic::InputTooLarge),
    };
    let member = ContainerMember {
        path: "svgz-stream.svg".into(),
        kind: ContainerMemberKind::Svg,
        compressed_bytes: bytes.len() as u64,
        declared_uncompressed_bytes: declared,
    };
    let mut parsed = None;
    let outcome = visit_admitted_compressed_member(
        &member,
        limits,
        &mut is_cancelled,
        &mut |_| match admit_scratch(payload_len) {
            Some(permit) => CompressedMemberAdmission::Dispatch(permit),
            None => CompressedMemberAdmission::Stop,
        },
        || Ok(GzDecoder::new(bytes)),
        |decoder| {
            let trailing = decoder.into_inner();
            if trailing.is_empty() {
                Ok(())
            } else if looks_like_gzip(trailing) {
                Err(InspectionDiagnostic::GzipMultipleMembers)
            } else {
                Err(InspectionDiagnostic::GzipTrailingBytes)
            }
        },
        |payload| {
            parsed = Some(inspect_svg(payload, limits));
            true
        },
    );
    match outcome {
        CompressedMemberVisitOutcome::Dispatched { .. } => {
            let mut inspection = parsed.unwrap_or_else(|| {
                rejected_media(MediaKind::Svgz, InspectionDiagnostic::InvalidSvg)
            });
            inspection.kind = MediaKind::Svgz;
            inspection
        }
        CompressedMemberVisitOutcome::Stopped | CompressedMemberVisitOutcome::Skipped => {
            MediaInspection {
                kind: MediaKind::Svgz,
                status: InspectionStatus::InventoryOnly,
                metadata: None,
                svg: None,
                diagnostics: vec![InspectionDiagnostic::NestedDispatchStopped],
            }
        }
        CompressedMemberVisitOutcome::Cancelled => MediaInspection {
            kind: MediaKind::Svgz,
            status: InspectionStatus::InventoryOnly,
            metadata: None,
            svg: None,
            diagnostics: vec![InspectionDiagnostic::Cancelled],
        },
        CompressedMemberVisitOutcome::Rejected(diagnostic) => {
            rejected_media(MediaKind::Svgz, diagnostic)
        }
    }
}

/// Ensure Quick XML never has to grow its reusable event buffer beyond the
/// configured ceiling.  The scanner is intentionally strict: DTD/entity
/// declarations are rejected before XML parsing and only quoted attributes may
/// contain `>` or `<` inside a start tag.
fn preflight_svg_tokens(
    bytes: &[u8],
    maximum_event_bytes: usize,
) -> Result<(), InspectionDiagnostic> {
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let next_markup = crate::bytes::find_subslice(&bytes[offset..], b"<")
            .map(|relative| offset + relative)
            .unwrap_or(bytes.len());
        if next_markup - offset > maximum_event_bytes {
            return Err(InspectionDiagnostic::SvgEventLimit);
        }
        if next_markup == bytes.len() {
            break;
        }
        if bytes[next_markup..].starts_with(b"<!DOCTYPE")
            || bytes[next_markup..].starts_with(b"<!ENTITY")
        {
            return Err(InspectionDiagnostic::SvgDocumentTypeForbidden);
        }
        if bytes[next_markup..].starts_with(b"<![CDATA[") {
            let content = next_markup + 9;
            let Some(end) = crate::bytes::find_subslice(&bytes[content..], b"]]>") else {
                return Err(InspectionDiagnostic::InvalidSvg);
            };
            if content + end + 3 - next_markup > maximum_event_bytes {
                return Err(InspectionDiagnostic::SvgEventLimit);
            }
            offset = content + end + 3;
            continue;
        }
        if bytes[next_markup..].starts_with(b"<!--") {
            let content = next_markup + 4;
            let Some(end) = crate::bytes::find_subslice(&bytes[content..], b"-->") else {
                return Err(InspectionDiagnostic::InvalidSvg);
            };
            if content + end + 3 - next_markup > maximum_event_bytes {
                return Err(InspectionDiagnostic::SvgEventLimit);
            }
            offset = content + end + 3;
            continue;
        }
        if bytes[next_markup..].starts_with(b"<?") {
            let content = next_markup + 2;
            let Some(end) = crate::bytes::find_subslice(&bytes[content..], b"?>") else {
                return Err(InspectionDiagnostic::InvalidSvg);
            };
            if content + end + 2 - next_markup > maximum_event_bytes {
                return Err(InspectionDiagnostic::SvgEventLimit);
            }
            offset = content + end + 2;
            continue;
        }

        let mut cursor = next_markup + 1;
        let mut quote = None;
        loop {
            let byte = *bytes.get(cursor).ok_or(InspectionDiagnostic::InvalidSvg)?;
            if cursor - next_markup > maximum_event_bytes {
                return Err(InspectionDiagnostic::SvgEventLimit);
            }
            match quote {
                Some(delimiter) if byte == delimiter => quote = None,
                Some(_) => {}
                None if matches!(byte, b'\'' | b'\"') => quote = Some(byte),
                None if byte == b'>' => {
                    offset = cursor + 1;
                    break;
                }
                None if byte == b'<' => return Err(InspectionDiagnostic::InvalidSvg),
                None => {}
            }
            cursor += 1;
        }
    }
    Ok(())
}

fn parse_svg<R: BufRead>(
    mut reader: Reader<R>,
    limits: ContainerLimits,
) -> Result<SvgInspection, InspectionDiagnostic> {
    reader.config_mut().trim_text(true);
    reader.config_mut().check_end_names = true;
    let mut event_buffer = Vec::with_capacity(cmp::min(limits.max_svg_event_bytes, 8 * 1024));
    let mut elements = Vec::with_capacity(cmp::min(limits.max_svg_elements, 256));
    let mut references = Vec::with_capacity(cmp::min(limits.max_svg_references, 256));
    let mut stack = Vec::with_capacity(cmp::min(limits.max_svg_depth, 32));
    let mut root_seen = false;
    let mut title_depth = None;
    let mut title = None;

    loop {
        match reader.read_event_into(&mut event_buffer) {
            Ok(Event::Start(event)) => {
                let ordinal = push_svg_element(&event, &mut elements, &mut references, limits)?;
                if stack.len() >= limits.max_svg_depth {
                    return Err(InspectionDiagnostic::SvgDepthLimit);
                }
                if !root_seen {
                    if xml_local_name(event.name().as_ref()) != b"svg" {
                        return Err(InspectionDiagnostic::SvgRootMissing);
                    }
                    root_seen = true;
                }
                if xml_local_name(event.name().as_ref()) == b"title" && title.is_none() {
                    title_depth = Some(stack.len() + 1);
                }
                stack.push(ordinal);
            }
            Ok(Event::Empty(event)) => {
                push_svg_element(&event, &mut elements, &mut references, limits)?;
                if !root_seen {
                    if xml_local_name(event.name().as_ref()) != b"svg" {
                        return Err(InspectionDiagnostic::SvgRootMissing);
                    }
                    root_seen = true;
                }
            }
            Ok(Event::Text(event)) if title_depth == Some(stack.len()) && title.is_none() => {
                let decoded = event
                    .decode()
                    .map_err(|_| InspectionDiagnostic::InvalidSvg)?;
                let decoded = quick_xml::escape::unescape(&decoded)
                    .map_err(|_| InspectionDiagnostic::InvalidSvg)?;
                title = Some(bounded_svg_string(&decoded, limits.max_svg_string_bytes)?);
            }
            Ok(Event::CData(event)) if title_depth == Some(stack.len()) && title.is_none() => {
                let decoded = event
                    .decode()
                    .map_err(|_| InspectionDiagnostic::InvalidSvg)?;
                title = Some(bounded_svg_string(&decoded, limits.max_svg_string_bytes)?);
            }
            Ok(Event::End(_)) => {
                if title_depth == Some(stack.len()) {
                    title_depth = None;
                }
                if stack.pop().is_none() {
                    return Err(InspectionDiagnostic::InvalidSvg);
                }
            }
            Ok(Event::DocType(_)) | Ok(Event::GeneralRef(_)) => {
                return Err(InspectionDiagnostic::SvgDocumentTypeForbidden)
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => return Err(InspectionDiagnostic::InvalidSvg),
        }
        event_buffer.clear();
    }
    if !root_seen || !stack.is_empty() {
        return Err(InspectionDiagnostic::SvgRootMissing);
    }
    Ok(SvgInspection {
        elements,
        references,
        title,
    })
}

fn push_svg_element(
    event: &quick_xml::events::BytesStart<'_>,
    elements: &mut Vec<SvgElement>,
    references: &mut Vec<SvgReference>,
    limits: ContainerLimits,
) -> Result<usize, InspectionDiagnostic> {
    if elements.len() >= limits.max_svg_elements {
        return Err(InspectionDiagnostic::SvgElementLimit);
    }
    let ordinal = elements.len();
    let name = bounded_svg_string(
        &String::from_utf8_lossy(xml_local_name(event.name().as_ref())),
        limits.max_svg_string_bytes,
    )?;
    let mut id = None;
    let mut label = None;
    let mut pending_references = Vec::new();
    for attribute in event.attributes().with_checks(true) {
        let attribute = attribute.map_err(|_| InspectionDiagnostic::InvalidSvg)?;
        let key = xml_local_name(attribute.key.as_ref());
        let value = attribute
            .unescape_value()
            .map_err(|_| InspectionDiagnostic::InvalidSvg)?;
        if key == b"id" {
            id = Some(bounded_svg_string(&value, limits.max_svg_string_bytes)?);
        } else if matches!(key, b"label" | b"title" | b"aria-label") && label.is_none() {
            label = Some(bounded_svg_string(&value, limits.max_svg_string_bytes)?);
        }
        if SVG_HREF_ATTRIBUTES.contains(&key) {
            pending_references.push(bounded_svg_string(&value, limits.max_svg_string_bytes)?);
        }
        for fragment in url_fragments(&value) {
            pending_references.push(bounded_svg_string(
                &format!("#{fragment}"),
                limits.max_svg_string_bytes,
            )?);
        }
    }
    if references.len() + pending_references.len() > limits.max_svg_references {
        return Err(InspectionDiagnostic::SvgReferenceLimit);
    }
    elements.push(SvgElement {
        ordinal,
        name,
        id,
        label,
    });
    references.extend(pending_references.into_iter().map(|target| SvgReference {
        relation: if target.starts_with('#') {
            SvgReferenceRelation::Fragment
        } else {
            SvgReferenceRelation::External
        },
        target: target.strip_prefix('#').unwrap_or(&target).to_owned(),
        source_ordinal: ordinal,
    }));
    Ok(ordinal)
}

fn url_fragments(value: &str) -> impl Iterator<Item = &str> {
    value
        .match_indices("url(#")
        .filter_map(|(offset, _)| {
            value
                .get(offset + 4..)
                .and_then(|suffix| suffix.split_once(')'))
        })
        .map(|(fragment, _)| fragment)
}

fn xml_local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|byte| matches!(byte, b':' | b'}'))
        .next()
        .unwrap_or(name)
}

fn bounded_svg_string(value: &str, maximum_bytes: usize) -> Result<String, InspectionDiagnostic> {
    if value.len() > maximum_bytes {
        return Err(InspectionDiagnostic::SvgStringLimit);
    }
    Ok(value.to_owned())
}

fn inspect_raster(kind: MediaKind, bytes: &[u8], maximum_probe_bytes: usize) -> MediaInspection {
    let bytes = &bytes[..cmp::min(bytes.len(), maximum_probe_bytes)];
    let metadata = match kind {
        MediaKind::Png => parse_png(bytes),
        MediaKind::Jpeg => parse_jpeg(bytes),
        MediaKind::Gif => parse_gif(bytes),
        MediaKind::Webp => parse_webp(bytes),
        MediaKind::Bmp => parse_bmp(bytes),
        MediaKind::Ico => parse_ico(bytes),
        MediaKind::Avif | MediaKind::Heif => parse_bmff(bytes),
        MediaKind::Tiff => Some(ImageMetadata {
            width: None,
            height: None,
            animated: None,
        }),
        MediaKind::Svg | MediaKind::Svgz => None,
    };
    match metadata {
        Some(metadata) => MediaInspection {
            kind,
            status: InspectionStatus::Parsed,
            metadata: Some(metadata),
            svg: None,
            diagnostics: Vec::new(),
        },
        None => rejected_media(kind, InspectionDiagnostic::InvalidImage),
    }
}

fn parse_png(bytes: &[u8]) -> Option<ImageMetadata> {
    (bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.get(12..16) == Some(b"IHDR")).then(|| {
        ImageMetadata {
            width: read_u32_be(bytes, 16),
            height: read_u32_be(bytes, 20),
            animated: Some(crate::bytes::find_subslice(bytes, b"acTL").is_some()),
        }
    })
}

fn parse_jpeg(bytes: &[u8]) -> Option<ImageMetadata> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut cursor = 2;
    while cursor + 1 < bytes.len() {
        if bytes[cursor] != 0xff {
            cursor += 1;
            continue;
        }
        while bytes.get(cursor) == Some(&0xff) {
            cursor += 1;
        }
        let marker = *bytes.get(cursor)?;
        cursor += 1;
        if marker == 0xd9 || marker == 0xda {
            break;
        }
        if matches!(marker, 0x01 | 0xd0..=0xd7) {
            continue;
        }
        let length = usize::from(read_u16_be(bytes, cursor)?);
        if length < 2 || cursor.checked_add(length)? > bytes.len() {
            return None;
        }
        if is_jpeg_sof(marker) {
            if length < 8 {
                return None;
            }
            return Some(ImageMetadata {
                height: Some(u32::from(read_u16_be(bytes, cursor + 3)?)),
                width: Some(u32::from(read_u16_be(bytes, cursor + 5)?)),
                animated: None,
            });
        }
        cursor += length;
    }
    None
}

fn read_u16_be(bytes: &[u8], offset: usize) -> Option<u16> {
    let bytes = bytes.get(offset..offset + 2)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn is_jpeg_sof(marker: u8) -> bool {
    matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf)
}

fn parse_gif(bytes: &[u8]) -> Option<ImageMetadata> {
    (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")).then(|| ImageMetadata {
        width: read_u16_le(bytes, 6).map(u32::from),
        height: read_u16_le(bytes, 8).map(u32::from),
        animated: Some(crate::bytes::find_subslice(bytes, b"NETSCAPE2.0").is_some()),
    })
}

fn parse_webp(bytes: &[u8]) -> Option<ImageMetadata> {
    if !bytes.starts_with(b"RIFF") || bytes.get(8..12) != Some(b"WEBP") {
        return None;
    }
    match bytes.get(12..16)? {
        b"VP8X" if bytes.len() >= 30 => Some(ImageMetadata {
            animated: Some(bytes[20] & 0x02 != 0),
            width: read_u24_le(bytes, 24).and_then(|width| width.checked_add(1)),
            height: read_u24_le(bytes, 27).and_then(|height| height.checked_add(1)),
        }),
        b"VP8L" if bytes.len() >= 25 && bytes[20] == 0x2f => {
            let width = 1 + u32::from(bytes[21] & 0x3f) + (u32::from(bytes[22] & 0x0f) << 6);
            let height = 1
                + u32::from(bytes[22] >> 4)
                + (u32::from(bytes[23]) << 4)
                + (u32::from(bytes[24] & 0x03) << 12);
            Some(ImageMetadata {
                width: Some(width),
                height: Some(height),
                animated: None,
            })
        }
        _ => Some(ImageMetadata {
            width: None,
            height: None,
            animated: None,
        }),
    }
}

fn read_u24_le(bytes: &[u8], offset: usize) -> Option<u32> {
    let bytes = bytes.get(offset..offset + 3)?;
    Some(u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16))
}

fn parse_bmp(bytes: &[u8]) -> Option<ImageMetadata> {
    (bytes.starts_with(b"BM") && bytes.len() >= 26).then(|| ImageMetadata {
        width: read_u32_le(bytes, 18),
        height: read_u32_le(bytes, 22).map(|height| height & 0x7fff_ffff),
        animated: None,
    })
}

fn parse_ico(bytes: &[u8]) -> Option<ImageMetadata> {
    (bytes.starts_with(&[0, 0, 1, 0]) && bytes.len() >= 8).then(|| ImageMetadata {
        width: Some(if bytes[6] == 0 {
            256
        } else {
            u32::from(bytes[6])
        }),
        height: Some(if bytes[7] == 0 {
            256
        } else {
            u32::from(bytes[7])
        }),
        animated: None,
    })
}

fn parse_bmff(bytes: &[u8]) -> Option<ImageMetadata> {
    looks_like_isobmff(bytes).then_some(ImageMetadata {
        width: None,
        height: None,
        animated: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression, GzBuilder};
    use std::{cell::Cell, io::Write, rc::Rc};
    use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

    fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        zip_bytes_with_method(entries, CompressionMethod::Deflated)
    }

    fn zip_bytes_with_method(entries: &[(&str, &[u8])], compression: CompressionMethod) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(compression);
        for (name, value) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(value).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn gzip_bytes(value: &[u8]) -> Vec<u8> {
        let mut writer = GzEncoder::new(Vec::new(), Compression::default());
        writer.write_all(value).unwrap();
        writer.finish().unwrap()
    }

    fn named_gzip_bytes(name: &str, value: &[u8]) -> Vec<u8> {
        let mut writer = GzBuilder::new()
            .filename(name)
            .write(Vec::new(), Compression::default());
        writer.write_all(value).unwrap();
        writer.finish().unwrap()
    }

    fn patch_single_zip_central_u32(bytes: &mut [u8], relative: usize, value: u32) {
        let central = single_zip_central_offset(bytes);
        bytes[central + relative..central + relative + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn patch_single_zip_central_u16(bytes: &mut [u8], relative: usize, value: u16) {
        let central = single_zip_central_offset(bytes);
        bytes[central + relative..central + relative + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn single_zip_central_offset(bytes: &[u8]) -> usize {
        bytes
            .windows(4)
            .position(|window| window == b"PK\x01\x02")
            .expect("central-directory signature")
    }

    fn corrupt_first_stored_zip_payload(bytes: &mut [u8]) {
        assert_eq!(bytes.get(..4), Some(b"PK\x03\x04".as_slice()));
        let name_length = usize::from(read_u16_le(bytes, 26).expect("local name length"));
        let extra_length = usize::from(read_u16_le(bytes, 28).expect("local extra length"));
        let payload_offset = 30 + name_length + extra_length;
        bytes[payload_offset] ^= 0xff;
    }

    struct TrackingPermit(Rc<Cell<bool>>);

    impl Drop for TrackingPermit {
        fn drop(&mut self) {
            assert!(self.0.replace(false), "permit must be live before drop");
        }
    }

    fn tar_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        const BLOCK: usize = 512;
        let mut archive = Vec::new();
        for (name, value) in entries {
            assert!(
                name.len() <= 100,
                "test TAR name fits the direct header field"
            );
            let mut header = [0_u8; BLOCK];
            header[..name.len()].copy_from_slice(name.as_bytes());
            header[100..108].copy_from_slice(b"0000644\0");
            header[108..116].copy_from_slice(b"0000000\0");
            header[116..124].copy_from_slice(b"0000000\0");
            let size = format!("{:011o}\0", value.len());
            header[124..136].copy_from_slice(size.as_bytes());
            header[136..148].copy_from_slice(b"00000000000\0");
            header[148..156].fill(b' ');
            header[156] = b'0';
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            let checksum = header.iter().map(|byte| u32::from(*byte)).sum::<u32>();
            let checksum = format!("{:06o}\0 ", checksum);
            header[148..156].copy_from_slice(checksum.as_bytes());
            archive.extend_from_slice(&header);
            archive.extend_from_slice(value);
            archive.resize(archive.len().div_ceil(BLOCK) * BLOCK, 0);
        }
        archive.resize(archive.len() + (BLOCK * 2), 0);
        archive
    }

    fn xz_header() -> Vec<u8> {
        let mut header = b"\xfd7zXZ\0\0\0".to_vec();
        header.extend(crc32fast::hash(&header[6..8]).to_le_bytes());
        header
    }

    #[test]
    fn zip_inventory_is_bounded_and_classifies_office_drawio_and_svg_parts() {
        let bytes = zip_bytes(&[
            ("word/document.xml", b"<w:document />"),
            ("diagram.drawio", b"<mxfile />"),
            ("assets/a.svg", b"<svg />"),
        ]);
        let ByteInventory::Container(inspected) =
            inspect_bytes("design.docx", &bytes, 0, ContainerLimits::default())
        else {
            panic!("ZIP must be recognized");
        };
        assert_eq!(inspected.status, InspectionStatus::Parsed);
        assert_eq!(inspected.members.len(), 3);
        assert_eq!(inspected.members[0].kind, ContainerMemberKind::OfficePart);
        assert_eq!(inspected.members[1].kind, ContainerMemberKind::DrawioPart);
        assert_eq!(inspected.members[2].kind, ContainerMemberKind::Svg);
        assert_eq!(
            inspected.decompressed_bytes,
            (b"<w:document />".len() + b"<mxfile />".len() + b"<svg />".len()) as u64
        );
    }

    #[test]
    fn zip_limit_is_checked_before_archive_inventory_is_materialized() {
        let bytes = zip_bytes(&[("a.txt", b"a"), ("b.txt", b"b")]);
        let inspected = inspect_container_bytes(
            ArchiveKind::Zip,
            &bytes,
            0,
            ContainerLimits {
                max_members: 1,
                ..ContainerLimits::default()
            },
        );
        assert_eq!(inspected.status, InspectionStatus::Rejected);
        assert_eq!(
            inspected.diagnostics,
            vec![InspectionDiagnostic::MemberLimit]
        );
    }

    #[test]
    fn zip64_extreme_record_offset_is_rejected_without_arithmetic_panic() {
        // A ZIP64 locator sits immediately before the ordinary EOCD. On a
        // 64-bit host this offset converts to `usize`, so every subsequent
        // relative offset and range must remain checked explicitly.
        let mut bytes = vec![0_u8; 42];
        bytes[0..4].copy_from_slice(ZIP64_LOCATOR_SIGNATURE);
        bytes[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        bytes[16..20].copy_from_slice(&1_u32.to_le_bytes());
        bytes[20..24].copy_from_slice(ZIP_EOCD_SIGNATURE);
        bytes[28..30].copy_from_slice(&u16::MAX.to_le_bytes());
        bytes[30..32].copy_from_slice(&u16::MAX.to_le_bytes());
        bytes[32..36].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[36..40].copy_from_slice(&u32::MAX.to_le_bytes());

        assert!(matches!(
            preflight_zip(&bytes, ContainerLimits::default()),
            Err(InspectionDiagnostic::Zip64MetadataInvalid)
        ));
    }

    #[test]
    fn zip_compression_ratio_limit_rejects_a_bomb_shape() {
        let payload = vec![b'a'; 64 * 1024];
        let bytes = zip_bytes(&[("large.txt", &payload)]);
        let inspected = inspect_container_bytes(
            ArchiveKind::Zip,
            &bytes,
            0,
            ContainerLimits {
                max_compression_ratio: 1,
                ..ContainerLimits::default()
            },
        );
        assert_eq!(inspected.status, InspectionStatus::Rejected);
        assert_eq!(
            inspected.diagnostics,
            vec![InspectionDiagnostic::CompressionRatioLimit]
        );
    }

    #[test]
    fn zip_recursive_dispatch_is_ordered_bounded_and_path_safe() {
        let bytes = zip_bytes(&[
            ("z-last.txt", b"last"),
            ("nested/diagram.dot", b"digraph rack { api -> storage; }"),
        ]);
        let mut dispatched = Vec::new();
        let inspected = visit_zip_members(&bytes, 0, ContainerLimits::default(), |child| {
            dispatched.push((child.member.path.clone(), child.bytes.to_vec()));
            true
        });
        assert_eq!(inspected.status, InspectionStatus::Parsed);
        assert_eq!(
            dispatched,
            vec![
                (
                    "nested/diagram.dot".into(),
                    b"digraph rack { api -> storage; }".to_vec(),
                ),
                ("z-last.txt".into(), b"last".to_vec()),
            ]
        );

        let traversal = zip_bytes(&[("../outside.dot", b"digraph unsafe {}")]);
        let rejected = visit_zip_members(&traversal, 0, ContainerLimits::default(), |_| {
            panic!("a traversal member must never be handed to a child parser")
        });
        assert_eq!(rejected.status, InspectionStatus::Rejected);
        assert_eq!(
            rejected.diagnostics,
            vec![InspectionDiagnostic::InvalidMemberName]
        );

        let member_limit = visit_zip_members(
            &bytes,
            0,
            ContainerLimits {
                max_members: 1,
                ..ContainerLimits::default()
            },
            |_| panic!("members beyond the configured limit must not dispatch"),
        );
        assert_eq!(
            member_limit.diagnostics,
            vec![InspectionDiagnostic::MemberLimit]
        );

        let ratio_limit = visit_zip_members(
            &zip_bytes(&[("compressible.txt", &vec![b'x'; 16 * 1024])]),
            0,
            ContainerLimits {
                max_compression_ratio: 1,
                ..ContainerLimits::default()
            },
            |_| panic!("a compression-ratio failure must not dispatch a payload"),
        );
        assert_eq!(
            ratio_limit.diagnostics,
            vec![InspectionDiagnostic::CompressionRatioLimit]
        );

        let depth_limit = visit_zip_members(
            &zip_bytes(&[("child.dot", b"digraph depth {}")]),
            0,
            ContainerLimits {
                max_recursion_depth: 0,
                ..ContainerLimits::default()
            },
            |_| panic!("a child beyond the depth budget must not dispatch"),
        );
        assert_eq!(
            depth_limit.diagnostics,
            vec![InspectionDiagnostic::RecursionLimit]
        );
    }

    #[test]
    fn bounded_zip_holds_permit_through_decode_and_polls_each_read_chunk() {
        let payload = (0..(READ_BUFFER_BYTES * 2 + 17))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let bytes = zip_bytes_with_method(&[("large.bin", &payload)], CompressionMethod::Stored);
        let permit_active = Rc::new(Cell::new(false));
        let admitted_active = Rc::clone(&permit_active);
        let visited_active = Rc::clone(&permit_active);
        let mut cancellation_polls = 0_usize;
        let inspected = visit_zip_members_bounded(
            &bytes,
            0,
            ContainerLimits::default(),
            || {
                cancellation_polls += 1;
                false
            },
            |_| {
                assert!(!admitted_active.replace(true));
                CompressedMemberAdmission::Dispatch(TrackingPermit(Rc::clone(&admitted_active)))
            },
            |child| {
                assert!(visited_active.get(), "permit must cover the visitor");
                assert_eq!(child.bytes, payload);
                true
            },
        );
        assert_eq!(inspected.status, InspectionStatus::Parsed);
        assert!(
            !permit_active.get(),
            "permit must be dropped after dispatch"
        );
        assert!(
            cancellation_polls >= 7,
            "member, admission, chunks, and visitor boundary are cancellable"
        );

        let permit_active = Rc::new(Cell::new(false));
        let admitted_active = Rc::clone(&permit_active);
        let mut cancellation_polls = 0_usize;
        let cancelled = visit_zip_members_bounded(
            &bytes,
            0,
            ContainerLimits::default(),
            || {
                cancellation_polls += 1;
                cancellation_polls >= 5
            },
            |_| {
                assert!(!admitted_active.replace(true));
                CompressedMemberAdmission::Dispatch(TrackingPermit(Rc::clone(&admitted_active)))
            },
            |_| panic!("cancellation during chunked decode must prevent dispatch"),
        );
        assert_eq!(cancelled.status, InspectionStatus::InventoryOnly);
        assert_eq!(cancelled.diagnostics, vec![InspectionDiagnostic::Cancelled]);
        assert!(!permit_active.get(), "cancellation must release the permit");
    }

    #[test]
    fn zip_skip_and_stop_leave_unadmitted_payloads_inert() {
        let mut bytes = zip_bytes_with_method(
            &[
                ("a-sensitive.txt", b"do not decode"),
                ("b-safe.txt", b"safe"),
            ],
            CompressionMethod::Stored,
        );
        corrupt_first_stored_zip_payload(&mut bytes);

        let mut dispatched = Vec::new();
        let skipped = visit_zip_members_bounded(
            &bytes,
            0,
            ContainerLimits::default(),
            || false,
            |member| {
                if member.path == "a-sensitive.txt" {
                    CompressedMemberAdmission::Skip
                } else {
                    CompressedMemberAdmission::Dispatch(())
                }
            },
            |child| {
                dispatched.push((child.member.path.clone(), child.bytes.to_vec()));
                true
            },
        );
        assert_eq!(skipped.status, InspectionStatus::Parsed);
        assert_eq!(
            skipped.diagnostics,
            vec![InspectionDiagnostic::MemberDispatchSkipped]
        );
        assert_eq!(dispatched, vec![("b-safe.txt".into(), b"safe".to_vec())]);

        let stopped = visit_zip_members_bounded(
            &bytes,
            0,
            ContainerLimits::default(),
            || false,
            |_| CompressedMemberAdmission::<()>::Stop,
            |_| panic!("a stopped member must not be decoded or dispatched"),
        );
        assert_eq!(stopped.status, InspectionStatus::InventoryOnly);
        assert_eq!(
            stopped.diagnostics,
            vec![InspectionDiagnostic::NestedDispatchStopped]
        );

        let opened = visit_zip_members(&bytes, 0, ContainerLimits::default(), |_| true);
        assert_eq!(opened.status, InspectionStatus::Rejected);
        assert_eq!(
            opened.diagnostics,
            vec![InspectionDiagnostic::DecompressionFailed]
        );
    }

    #[test]
    fn zip_member_names_are_canonical_and_reserve_virtual_boundaries() {
        for invalid in ["flat!/nested.dot", "folder/drive:name.txt"] {
            let rejected = visit_zip_members(
                &zip_bytes(&[(invalid, b"inert")]),
                0,
                ContainerLimits::default(),
                |_| panic!("an invalid member name must never dispatch"),
            );
            assert_eq!(rejected.status, InspectionStatus::Rejected);
            assert_eq!(
                rejected.diagnostics,
                vec![InspectionDiagnostic::InvalidMemberName]
            );
        }

        let canonical_duplicate = zip_bytes(&[
            ("caf\u{e9}.txt", b"precomposed"),
            ("cafe\u{301}.txt", b"decomposed"),
        ]);
        let rejected =
            visit_zip_members(&canonical_duplicate, 0, ContainerLimits::default(), |_| {
                panic!("canonically equivalent paths must fail before dispatch")
            });
        assert_eq!(rejected.status, InspectionStatus::Rejected);
        assert_eq!(
            rejected.diagnostics,
            vec![InspectionDiagnostic::InvalidMemberName]
        );
    }

    #[test]
    fn zip_unsafe_entry_types_fail_closed_before_dispatch() {
        let mut unsupported =
            zip_bytes_with_method(&[("unsafe.bin", b"payload")], CompressionMethod::Stored);
        unsupported[8..10].copy_from_slice(&9_u16.to_le_bytes());
        patch_single_zip_central_u16(&mut unsupported, 10, 9);

        let mut symlink = zip_bytes_with_method(&[("link", b"target")], CompressionMethod::Stored);
        let central = single_zip_central_offset(&symlink);
        symlink[central + 5] = 3;
        patch_single_zip_central_u32(&mut symlink, 38, 0o120_777_u32 << 16);

        let mut fifo = zip_bytes_with_method(&[("pipe", b"payload")], CompressionMethod::Stored);
        let central = single_zip_central_offset(&fifo);
        fifo[central + 5] = 3;
        patch_single_zip_central_u32(&mut fifo, 38, 0o010_644_u32 << 16);

        let mut encrypted =
            zip_bytes_with_method(&[("secret", b"payload")], CompressionMethod::Stored);
        let local_flags = read_u16_le(&encrypted, 6).expect("local flags") | 1;
        encrypted[6..8].copy_from_slice(&local_flags.to_le_bytes());
        let central = single_zip_central_offset(&encrypted);
        let central_flags = read_u16_le(&encrypted, central + 8).expect("central flags") | 1;
        encrypted[central + 8..central + 10].copy_from_slice(&central_flags.to_le_bytes());

        for (bytes, expected) in [
            (unsupported, InspectionDiagnostic::UnsupportedCompression),
            (symlink, InspectionDiagnostic::SymlinkMember),
            (fifo, InspectionDiagnostic::NonRegularMember),
            (encrypted, InspectionDiagnostic::EncryptedMember),
        ] {
            let rejected = visit_zip_members(&bytes, 0, ContainerLimits::default(), |_| {
                panic!("an unsafe ZIP entry must fail before dispatch")
            });
            assert_eq!(rejected.status, InspectionStatus::Rejected);
            assert!(
                rejected.members.is_empty(),
                "failure must not expose a prefix"
            );
            assert_eq!(rejected.diagnostics, vec![expected]);
        }
    }

    #[test]
    fn gzip_is_single_member_and_is_streamed_under_limits() {
        let bytes = gzip_bytes(b"bounded gzip source");
        let inspected =
            inspect_container_bytes(ArchiveKind::Gzip, &bytes, 0, ContainerLimits::default());
        assert_eq!(inspected.status, InspectionStatus::Parsed);
        assert_eq!(
            inspected.decompressed_bytes,
            b"bounded gzip source".len() as u64
        );
        assert_eq!(inspected.members[0].path, "gzip-stream");
    }

    #[test]
    fn concatenated_gzip_members_are_rejected() {
        let mut bytes = gzip_bytes(b"first");
        bytes.extend(gzip_bytes(b"second"));
        let inspected =
            inspect_container_bytes(ArchiveKind::Gzip, &bytes, 0, ContainerLimits::default());
        assert_eq!(inspected.status, InspectionStatus::Rejected);
        assert_eq!(
            inspected.diagnostics,
            vec![InspectionDiagnostic::GzipMultipleMembers]
        );
    }

    #[test]
    fn bounded_gzip_uses_safe_child_names_and_holds_its_permit() {
        let payload = b"digraph rack { api -> storage; }";
        let bytes = named_gzip_bytes("nested/diagram.dot", payload);
        let permit_active = Rc::new(Cell::new(false));
        let admitted_active = Rc::clone(&permit_active);
        let visited_active = Rc::clone(&permit_active);
        let inspected = visit_gzip_member_bounded(
            "ignored.gz",
            &bytes,
            0,
            ContainerLimits::default(),
            || false,
            |member| {
                assert_eq!(member.path, "nested/diagram.dot");
                assert!(!admitted_active.replace(true));
                CompressedMemberAdmission::Dispatch(TrackingPermit(Rc::clone(&admitted_active)))
            },
            |child| {
                assert!(visited_active.get(), "permit must cover GZIP visitor");
                assert_eq!(child.bytes, payload);
                true
            },
        );
        assert_eq!(inspected.status, InspectionStatus::Parsed);
        assert_eq!(inspected.members[0].path, "nested/diagram.dot");
        assert!(!permit_active.get());

        for (source_name, expected) in [
            ("bundle.tar.gz", "bundle.tar"),
            ("bundle.tgz", "bundle.tar"),
        ] {
            let mut visited = None;
            let inspected = visit_gzip_member(
                source_name,
                &gzip_bytes(b"tar bytes"),
                0,
                ContainerLimits::default(),
                |child| {
                    visited = Some(child.member.path.clone());
                    true
                },
            );
            assert_eq!(inspected.status, InspectionStatus::Parsed);
            assert_eq!(visited.as_deref(), Some(expected));
        }

        let svgz = gzip_bytes(b"<svg/>");
        assert_eq!(recursive_archive_kind("icon.svgz", &svgz), None);
        let inspected = visit_gzip_member_bounded(
            "icon.svgz",
            &svgz,
            0,
            ContainerLimits::default(),
            || false,
            |_| -> CompressedMemberAdmission<()> {
                panic!("SVGZ must remain outside generic child admission")
            },
            |_| panic!("SVGZ must remain outside generic child dispatch"),
        );
        assert_eq!(inspected.status, InspectionStatus::Parsed);
    }

    #[test]
    fn bounded_gzip_rejects_reserved_names_and_extra_stream_bytes() {
        for invalid in ["flat!/nested.dot", "nested/drive:name.txt"] {
            let rejected = visit_gzip_member(
                "ignored.gz",
                &named_gzip_bytes(invalid, b"inert"),
                0,
                ContainerLimits::default(),
                |_| panic!("an invalid FNAME must never dispatch"),
            );
            assert_eq!(rejected.status, InspectionStatus::Rejected);
            assert_eq!(
                rejected.diagnostics,
                vec![InspectionDiagnostic::InvalidMemberName]
            );
        }

        let mut concatenated = gzip_bytes(b"equal");
        concatenated.extend(gzip_bytes(b"equal"));
        let rejected = visit_gzip_member(
            "joined.gz",
            &concatenated,
            0,
            ContainerLimits::default(),
            |_| panic!("concatenated GZIP streams must never dispatch"),
        );
        assert_eq!(rejected.status, InspectionStatus::Rejected);
        assert_eq!(
            rejected.diagnostics,
            vec![InspectionDiagnostic::GzipMultipleMembers]
        );

        let payload = b"trailing";
        let mut trailing = gzip_bytes(payload);
        trailing.push(0xaa);
        trailing.extend((payload.len() as u32).to_le_bytes());
        let rejected = visit_gzip_member(
            "trailing.gz",
            &trailing,
            0,
            ContainerLimits::default(),
            |_| panic!("trailing GZIP bytes must never dispatch"),
        );
        assert_eq!(rejected.status, InspectionStatus::Rejected);
        assert_eq!(
            rejected.diagnostics,
            vec![InspectionDiagnostic::GzipTrailingBytes]
        );
    }

    #[test]
    fn bounded_gzip_cancellation_releases_permit_before_dispatch() {
        let payload = (0..(READ_BUFFER_BYTES * 2 + 17))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let bytes = gzip_bytes(&payload);
        let permit_active = Rc::new(Cell::new(false));
        let admitted_active = Rc::clone(&permit_active);
        let mut cancellation_polls = 0_usize;
        let cancelled = visit_gzip_member_bounded(
            "large.bin.gz",
            &bytes,
            0,
            ContainerLimits {
                max_compression_ratio: u64::MAX,
                ..ContainerLimits::default()
            },
            || {
                cancellation_polls += 1;
                cancellation_polls >= 4
            },
            |_| {
                assert!(!admitted_active.replace(true));
                CompressedMemberAdmission::Dispatch(TrackingPermit(Rc::clone(&admitted_active)))
            },
            |_| panic!("cancelled GZIP must not dispatch"),
        );
        assert_eq!(cancelled.status, InspectionStatus::InventoryOnly);
        assert_eq!(cancelled.diagnostics, vec![InspectionDiagnostic::Cancelled]);
        assert!(!permit_active.get());
    }

    #[test]
    fn tar_inventory_and_zero_copy_recursive_dispatch_are_bounded() {
        let nested = zip_bytes(&[("diagram.dot", b"digraph rack { a -> b; }")]);
        let tar = tar_bytes(&[
            ("nested/diagram.zip", &nested),
            ("notes/readme.txt", b"bounded"),
        ]);
        let ByteInventory::Container(inspected) =
            inspect_bytes("rack.tar", &tar, 0, ContainerLimits::default())
        else {
            panic!("TAR must be recognized");
        };
        assert_eq!(inspected.kind, ArchiveKind::Tar);
        assert_eq!(inspected.status, InspectionStatus::Parsed);
        assert_eq!(inspected.members.len(), 2);
        assert_eq!(
            inspected.members[0].kind,
            ContainerMemberKind::NestedContainer
        );
        assert_eq!(
            inspected.decompressed_bytes,
            (nested.len() + b"bounded".len()) as u64
        );

        let source_start = tar.as_ptr() as usize;
        let source_end = source_start + tar.len();
        let mut dispatched = Vec::new();
        let result = visit_tar_members(&tar, 0, ContainerLimits::default(), |child| {
            let child_start = child.bytes.as_ptr() as usize;
            assert!(child_start >= source_start && child_start + child.bytes.len() <= source_end);
            if child.member.kind == ContainerMemberKind::NestedContainer {
                let ByteInventory::Container(nested) = inspect_bytes(
                    &child.member.path,
                    child.bytes,
                    1,
                    ContainerLimits::default(),
                ) else {
                    panic!("nested ZIP must be recursively admissible");
                };
                assert_eq!(nested.kind, ArchiveKind::Zip);
                assert_eq!(nested.status, InspectionStatus::Parsed);
            }
            dispatched.push((child.member.path.clone(), child.bytes.len()));
            true
        });
        assert_eq!(result.status, InspectionStatus::Parsed);
        assert_eq!(
            dispatched,
            vec![
                ("nested/diagram.zip".into(), nested.len()),
                ("notes/readme.txt".into(), b"bounded".len()),
            ]
        );
    }

    #[test]
    fn bounded_tar_polls_during_headers_and_at_member_dispatch() {
        let tar = tar_bytes(&[("safe.txt", b"safe")]);
        let mut polls = 0_usize;
        let during_headers = visit_tar_members_bounded(
            &tar,
            0,
            ContainerLimits::default(),
            || {
                polls += 1;
                polls >= 2
            },
            |_| panic!("header-loop cancellation must prevent dispatch"),
        );
        assert_eq!(during_headers.status, InspectionStatus::InventoryOnly);
        assert_eq!(
            during_headers.diagnostics,
            vec![InspectionDiagnostic::Cancelled]
        );

        let mut polls = 0_usize;
        let at_dispatch = visit_tar_members_bounded(
            &tar,
            0,
            ContainerLimits::default(),
            || {
                polls += 1;
                polls >= 4
            },
            |_| panic!("member-boundary cancellation must prevent dispatch"),
        );
        assert_eq!(at_dispatch.status, InspectionStatus::InventoryOnly);
        assert_eq!(at_dispatch.members.len(), 1);
        assert_eq!(
            at_dispatch.diagnostics,
            vec![InspectionDiagnostic::Cancelled]
        );
    }

    #[test]
    fn tar_member_names_share_canonical_reserved_path_policy() {
        for invalid in ["flat!/nested.dot", "folder/drive:name.txt"] {
            let rejected = visit_tar_members(
                &tar_bytes(&[(invalid, b"inert")]),
                0,
                ContainerLimits::default(),
                |_| panic!("an invalid TAR member must never dispatch"),
            );
            assert_eq!(rejected.status, InspectionStatus::Rejected);
            assert_eq!(
                rejected.diagnostics,
                vec![InspectionDiagnostic::InvalidMemberName]
            );
        }

        let duplicate = tar_bytes(&[
            ("caf\u{e9}.txt", b"precomposed"),
            ("cafe\u{301}.txt", b"decomposed"),
        ]);
        let rejected = visit_tar_members(&duplicate, 0, ContainerLimits::default(), |_| {
            panic!("canonically equivalent TAR paths must fail before dispatch")
        });
        assert_eq!(rejected.status, InspectionStatus::Rejected);
        assert_eq!(
            rejected.diagnostics,
            vec![InspectionDiagnostic::InvalidMemberName]
        );
    }

    #[test]
    fn tar_rejects_malformed_and_over_budget_inputs_before_dispatch() {
        let valid = tar_bytes(&[("safe.txt", b"safe")]);
        let mut corrupt = valid.clone();
        corrupt[0] ^= 1;
        let checksum =
            inspect_container_bytes(ArchiveKind::Tar, &corrupt, 0, ContainerLimits::default());
        assert_eq!(checksum.status, InspectionStatus::Rejected);
        assert_eq!(
            checksum.diagnostics,
            vec![InspectionDiagnostic::TarChecksumInvalid]
        );

        let truncated = inspect_container_bytes(
            ArchiveKind::Tar,
            &valid[..600],
            0,
            ContainerLimits::default(),
        );
        assert_eq!(truncated.status, InspectionStatus::Rejected);
        assert_eq!(
            truncated.diagnostics,
            vec![InspectionDiagnostic::TarTruncated]
        );

        let member_limit = inspect_container_bytes(
            ArchiveKind::Tar,
            &tar_bytes(&[("one", b"1"), ("two", b"2")]),
            0,
            ContainerLimits {
                max_members: 1,
                ..ContainerLimits::default()
            },
        );
        assert_eq!(
            member_limit.diagnostics,
            vec![InspectionDiagnostic::MemberLimit]
        );

        let byte_limit = inspect_container_bytes(
            ArchiveKind::Tar,
            &valid,
            0,
            ContainerLimits {
                max_member_uncompressed_bytes: 3,
                ..ContainerLimits::default()
            },
        );
        assert_eq!(
            byte_limit.diagnostics,
            vec![InspectionDiagnostic::MemberSizeLimit]
        );

        let max_depth = visit_tar_members(
            &valid,
            0,
            ContainerLimits {
                max_recursion_depth: 0,
                ..ContainerLimits::default()
            },
            |_| panic!("a child must not be dispatched beyond the depth budget"),
        );
        assert_eq!(max_depth.status, InspectionStatus::InventoryOnly);
        assert_eq!(
            max_depth.diagnostics,
            vec![InspectionDiagnostic::RecursionLimit]
        );
    }

    #[test]
    fn compressed_inventory_validates_headers_and_never_claims_decoding() {
        let bzip = inspect_bytes("fixture.bz2", b"BZh9", 0, ContainerLimits::default());
        let ByteInventory::Container(bzip) = bzip else {
            panic!("BZIP2 must be recognized");
        };
        assert_eq!(bzip.kind, ArchiveKind::Bzip2);
        assert_eq!(bzip.status, InspectionStatus::InventoryOnly);
        assert_eq!(
            bzip.diagnostics,
            vec![
                InspectionDiagnostic::DeclaredSizeUnavailable,
                InspectionDiagnostic::DecoderUnavailable,
            ]
        );
        let invalid_bzip =
            inspect_container_bytes(ArchiveKind::Bzip2, b"BZh0", 0, ContainerLimits::default());
        assert_eq!(
            invalid_bzip.diagnostics,
            vec![InspectionDiagnostic::Bzip2HeaderInvalid]
        );

        let xz = inspect_bytes("fixture.xz", &xz_header(), 0, ContainerLimits::default());
        let ByteInventory::Container(xz) = xz else {
            panic!("XZ must be recognized");
        };
        assert_eq!(xz.status, InspectionStatus::InventoryOnly);
        assert!(xz
            .diagnostics
            .contains(&InspectionDiagnostic::DecoderUnavailable));
        let mut invalid_xz = xz_header();
        invalid_xz[8] ^= 1;
        let invalid_xz =
            inspect_container_bytes(ArchiveKind::Xz, &invalid_xz, 0, ContainerLimits::default());
        assert_eq!(
            invalid_xz.diagnostics,
            vec![InspectionDiagnostic::XzHeaderInvalid]
        );

        let zstd = inspect_bytes(
            "fixture.zst",
            &[0x28, 0xb5, 0x2f, 0xfd, 0x20, 42],
            0,
            ContainerLimits::default(),
        );
        let ByteInventory::Container(zstd) = zstd else {
            panic!("Zstd must be recognized");
        };
        assert_eq!(zstd.status, InspectionStatus::InventoryOnly);
        assert_eq!(zstd.members[0].declared_uncompressed_bytes, 42);
        assert_eq!(
            zstd.diagnostics,
            vec![InspectionDiagnostic::DecoderUnavailable]
        );
        let ratio = inspect_container_bytes(
            ArchiveKind::Zstd,
            &[0x28, 0xb5, 0x2f, 0xfd, 0x20, 42],
            0,
            ContainerLimits {
                max_compression_ratio: 1,
                ..ContainerLimits::default()
            },
        );
        assert_eq!(
            ratio.diagnostics,
            vec![InspectionDiagnostic::CompressionRatioLimit]
        );
        let unknown_zstd = inspect_container_bytes(
            ArchiveKind::Zstd,
            &[0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x00],
            0,
            ContainerLimits::default(),
        );
        assert_eq!(unknown_zstd.status, InspectionStatus::InventoryOnly);
        assert_eq!(
            unknown_zstd.diagnostics,
            vec![InspectionDiagnostic::DeclaredSizeUnavailable]
        );
        let oversized_window = inspect_container_bytes(
            ArchiveKind::Zstd,
            &[0x28, 0xb5, 0x2f, 0xfd, 0x00, 0xff],
            0,
            ContainerLimits::default(),
        );
        assert_eq!(
            oversized_window.diagnostics,
            vec![InspectionDiagnostic::MemberSizeLimit]
        );
    }

    #[test]
    fn svg_records_elements_and_internal_external_references() {
        let source = br##"<svg xmlns="http://www.w3.org/2000/svg"><title>Diagram</title><defs><path id="p"/></defs><use href="#p"/><image href="images/a.png"/></svg>"##;
        let ByteInventory::Media(inspected) =
            inspect_bytes("diagram.svg", source, 0, ContainerLimits::default())
        else {
            panic!("SVG must be recognized");
        };
        let svg = inspected.svg.expect("SVG semantic inventory");
        assert_eq!(inspected.status, InspectionStatus::Parsed);
        assert_eq!(svg.title.as_deref(), Some("Diagram"));
        assert_eq!(svg.elements.len(), 6);
        assert_eq!(svg.references.len(), 2);
        assert_eq!(svg.references[0].relation, SvgReferenceRelation::Fragment);
        assert_eq!(svg.references[0].target, "p");
        assert_eq!(svg.references[1].relation, SvgReferenceRelation::External);
    }

    #[test]
    fn svgz_uses_the_same_bounded_semantic_parser() {
        let source = br##"<svg><rect id="rack"/><use href="#rack"/></svg>"##;
        let bytes = gzip_bytes(source);
        let ByteInventory::Media(inspected) =
            inspect_bytes("rack.svgz", &bytes, 0, ContainerLimits::default())
        else {
            panic!("SVGZ must be recognized");
        };
        assert_eq!(inspected.kind, MediaKind::Svgz);
        assert_eq!(inspected.status, InspectionStatus::Parsed);
        assert_eq!(
            inspected
                .svg
                .expect("SVGZ semantic inventory")
                .references
                .len(),
            1
        );
    }

    #[test]
    fn svg_doctype_and_large_event_are_rejected_before_xml_allocates() {
        let limits = ContainerLimits {
            max_svg_event_bytes: 16,
            ..ContainerLimits::default()
        };
        let doctype = inspect_media_bytes(MediaKind::Svg, b"<!DOCTYPE svg><svg/>", 0, limits);
        assert_eq!(doctype.status, InspectionStatus::Rejected);
        assert_eq!(
            doctype.diagnostics,
            vec![InspectionDiagnostic::SvgDocumentTypeForbidden]
        );
        let event =
            inspect_media_bytes(MediaKind::Svg, b"<svg a=\"this is too large\"/>", 0, limits);
        assert_eq!(event.status, InspectionStatus::Rejected);
        assert_eq!(event.diagnostics, vec![InspectionDiagnostic::SvgEventLimit]);
    }

    #[test]
    fn raster_magic_yields_dimensions_without_pixel_decoding() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend(640_u32.to_be_bytes());
        png.extend(480_u32.to_be_bytes());
        png.extend([8, 6, 0, 0, 0]);
        let ByteInventory::Media(inspected) =
            inspect_bytes("diagram.png", &png, 0, ContainerLimits::default())
        else {
            panic!("PNG must be recognized");
        };
        assert_eq!(inspected.status, InspectionStatus::Parsed);
        assert_eq!(
            inspected.metadata,
            Some(ImageMetadata {
                width: Some(640),
                height: Some(480),
                animated: Some(false),
            })
        );
    }

    #[test]
    fn recognized_unsupported_archives_are_truthful_inventory_only() {
        let inspected = inspect_bytes(
            "archive.7z",
            b"7z\xbc\xaf\x27\x1c",
            0,
            ContainerLimits::default(),
        );
        let ByteInventory::Container(inspected) = inspected else {
            panic!("7z must be recognized");
        };
        assert_eq!(inspected.kind, ArchiveKind::SevenZip);
        assert_eq!(inspected.status, InspectionStatus::InventoryOnly);
        assert_eq!(
            inspected.diagnostics,
            vec![InspectionDiagnostic::UnsupportedArchiveFormat]
        );
    }
}
