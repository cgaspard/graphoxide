//! File discovery, classification, ignore handling, and portable scan manifests.
//!
//! This module intentionally owns the complete pre-extraction boundary.  A file
//! that disappears here can never appear in the graph, so unsupported,
//! sensitive, ignored, and unreadable paths are all reported explicitly.

use crate::format_registry::format_registry;
pub use crate::format_registry::FileType;
use md5::{Digest as _, Md5};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::Sha256;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{BufReader, Cursor, Read},
    path::{Component, Path, PathBuf},
    time::UNIX_EPOCH,
};
use unicode_normalization::UnicodeNormalization;

pub const OFFICE_MAX_RAW_BYTES: u64 = 50 * 1024 * 1024;
pub const OFFICE_MAX_DECOMPRESSED_BYTES: u64 = 512 * 1024 * 1024;
pub const OFFICE_MAX_COMPRESSION_RATIO: u64 = 200;
pub const OFFICE_MAX_MEMBERS: usize = 10_000;
pub const OFFICE_MAX_CENTRAL_DIRECTORY_BYTES: usize = 16 * 1024 * 1024;
const OFFICE_MAX_MARKDOWN_BYTES: usize = 16 * 1024 * 1024;
/// Maximum source bytes inspected solely for the corpus-size word heuristic.
///
/// Extraction has its own runtime admission. Discovery never needs to retain
/// an entire source merely to decide whether a graph is likely useful.
pub const WORD_COUNT_MAX_BYTES: usize = 16 * 1024 * 1024;
const PAPER_HEURISTIC_MAX_BYTES: u64 = 12_000;

/// Resource ceilings applied before any Office XML parser sees attacker-owned
/// `.docx` or `.xlsx` content. Central-directory materialization is separately
/// capped by [`OFFICE_MAX_CENTRAL_DIRECTORY_BYTES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OfficeLimits {
    pub max_raw_bytes: u64,
    pub max_decompressed_bytes: u64,
    pub max_compression_ratio: u64,
    pub max_members: usize,
}

impl Default for OfficeLimits {
    fn default() -> Self {
        Self {
            max_raw_bytes: OFFICE_MAX_RAW_BYTES,
            max_decompressed_bytes: OFFICE_MAX_DECOMPRESSED_BYTES,
            max_compression_ratio: OFFICE_MAX_COMPRESSION_RATIO,
            max_members: OFFICE_MAX_MEMBERS,
        }
    }
}
const SKIP_FILES: &[&str] = &[
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Cargo.lock",
    "poetry.lock",
    "uv.lock",
    "Gemfile.lock",
    "composer.lock",
    "go.sum",
    "go.work.sum",
    ".graphifyinclude",
    ".graphoxideinclude",
];
const SKIP_DIRS: &[&str] = &[
    "venv",
    ".venv",
    "node_modules",
    "__pycache__",
    ".git",
    "dist",
    "build",
    "target",
    "out",
    "site-packages",
    "lib64",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".nox",
    ".eggs",
    "graphoxide-out",
    "graphify-out",
    "lcov-report",
    "visual-tests",
    "visual-test",
    "__snapshots__",
    "storybook-static",
    "dist-protected",
    ".next",
    ".nuxt",
    ".turbo",
    ".angular",
    ".idea",
    ".cache",
    ".parcel-cache",
    ".svelte-kit",
    ".terraform",
    ".serverless",
    ".graphify",
    ".graphoxide",
    ".worktrees",
];
const CORPUS_WARN_THRESHOLD: usize = 50_000;
const CORPUS_UPPER_THRESHOLD: usize = 500_000;
const FILE_COUNT_UPPER: usize = 500;

#[derive(Debug, Clone)]
pub struct DetectOptions {
    pub follow_symlinks: bool,
    pub google_workspace: bool,
    /// Materialize legacy Markdown sidecars for Office documents during
    /// discovery. The isolated executor disables this so original containers
    /// first pass through runtime byte admission and bounded adapters.
    pub convert_office_sidecars: bool,
    pub extra_excludes: Vec<String>,
    pub output_dir: Option<PathBuf>,
    pub honor_gitignore: bool,
}

impl Default for DetectOptions {
    fn default() -> Self {
        Self {
            follow_symlinks: false,
            google_workspace: false,
            convert_office_sidecars: true,
            extra_excludes: Vec::new(),
            output_dir: None,
            honor_gitignore: true,
        }
    }
}

pub type DetectedFiles = BTreeMap<String, Vec<String>>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectResult {
    pub files: DetectedFiles,
    pub total_files: usize,
    pub total_words: usize,
    pub needs_graph: bool,
    pub warning: Option<String>,
    pub skipped_sensitive: Vec<String>,
    pub unclassified: Vec<String>,
    pub walk_errors: Vec<String>,
    /// Files whose corpus-size heuristic inspected only the documented prefix.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub word_count_truncations: Vec<String>,
    pub ignored: Vec<String>,
    pub pruned_noise_dirs: Vec<String>,
    pub graphifyignore_patterns: usize,
    pub scan_root: String,
    /// Stable logical-to-physical source bindings captured during discovery.
    ///
    /// This control-plane detail is deliberately excluded from serialized
    /// compatibility output. Runtime I/O uses it to avoid reopening a mutable
    /// symlink alias while graph provenance retains the logical path.
    #[serde(skip, default)]
    physical_sources: BTreeMap<String, String>,
}

impl DetectResult {
    /// Return the once-canonicalized physical source bound to a logical path.
    #[must_use]
    pub fn physical_source(&self, logical: &Path) -> PathBuf {
        self.physical_sources
            .get(logical.to_string_lossy().as_ref())
            .map_or_else(|| logical.to_path_buf(), PathBuf::from)
    }

    /// Apply the supported-source policy using the logical format identity and
    /// the physical path already approved by discovery.
    #[must_use]
    pub fn is_supported_source(&self, logical: &Path) -> bool {
        is_supported_path_at(logical, &self.physical_source(logical))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnorePattern {
    pub anchor: PathBuf,
    pub pattern: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ManifestKind {
    Ast,
    Semantic,
    #[default]
    Both,
}

#[derive(Debug, Clone, Default)]
pub struct SaveManifestOptions {
    pub kind: ManifestKind,
    pub root: Option<PathBuf>,
    pub scan_corpus: Option<BTreeSet<String>>,
    pub clear_semantic: BTreeSet<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IncrementalResult {
    pub detection: DetectResult,
    pub new_files: DetectedFiles,
    pub unchanged_files: DetectedFiles,
    pub new_total: usize,
    pub deleted_files: Vec<String>,
    pub excluded_files: Vec<String>,
}

fn lower_extension(path: &Path) -> String {
    path.extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_lowercase()
}

fn is_package_manifest(path: &Path) -> bool {
    if crate::manifest_ingest::is_package_manifest_path(path) {
        return true;
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_lowercase();
    matches!(
        name.as_str(),
        "package.json"
            | "cargo.toml"
            | "build.gradle"
            | "build.gradle.kts"
            | "composer.json"
            | "gemfile"
            | "mix.exs"
    ) || matches!(
        lower_extension(path).as_str(),
        "csproj" | "fsproj" | "vbproj"
    )
}

/// Classify a path into the extraction tier used by the upstream detector.
pub fn classify_file(path: &Path) -> Option<FileType> {
    classify_file_at(path, path)
}

fn classify_file_at(logical_path: &Path, physical_path: &Path) -> Option<FileType> {
    if is_package_manifest(logical_path)
        || logical_path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| name.to_lowercase().ends_with(".blade.php"))
    {
        return Some(FileType::Code);
    }
    let extension = lower_extension(logical_path);
    let registry = format_registry();
    if extension.is_empty() {
        return shebang_interpreter(physical_path).and_then(|interpreter| {
            SHEBANG_CODE_INTERPRETERS
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(&interpreter))
                .then_some(FileType::Code)
        });
    }
    if registry.classify_extension(&extension) == Some(FileType::Paper) {
        let asset = logical_path.components().any(|component| {
            let name = component.as_os_str().to_string_lossy().to_lowercase();
            [
                ".imageset",
                ".xcassets",
                ".appiconset",
                ".colorset",
                ".launchimage",
            ]
            .iter()
            .any(|marker| name.ends_with(marker))
        });
        return (!asset).then_some(FileType::Paper);
    }
    if registry.is_document_heuristic_extension(&extension) {
        return Some(if looks_like_paper(physical_path) {
            FileType::Paper
        } else {
            FileType::Document
        });
    }
    registry.classify_extension(&extension)
}

/// Heuristic used to distinguish converted papers from ordinary Markdown.
pub fn looks_like_paper(path: &Path) -> bool {
    let Ok(file) = open_source_nofollow(path) else {
        return false;
    };
    let mut bytes = Vec::with_capacity(PAPER_HEURISTIC_MAX_BYTES as usize);
    if file
        .take(PAPER_HEURISTIC_MAX_BYTES)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return false;
    }
    let text = String::from_utf8_lossy(&bytes);
    let signals = [
        r"(?i)\barxiv\b",
        r"(?i)\bdoi\s*:",
        r"(?i)\babstract\b",
        r"(?i)\bproceedings\b",
        r"(?i)\bjournal\b",
        r"(?i)\bpreprint\b",
        r"\\cite\{",
        r"\[\d+\]",
        r"(?i)eq\.\s*\d+|equation\s+\d+",
        r"\d{4}\.\d{4,5}",
        r"(?i)\bwe propose\b",
        r"(?i)\bliterature\b",
    ];
    signals
        .iter()
        .filter(|pattern| Regex::new(pattern).is_ok_and(|regex| regex.is_match(&text)))
        .count()
        >= 3
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct BoundedWordCount {
    words: usize,
    truncated: bool,
}

pub fn count_words(path: &Path) -> usize {
    if matches!(lower_extension(path).as_str(), "pdf" | "docx" | "xlsx") {
        return 0;
    }
    count_words_with_cap(path, WORD_COUNT_MAX_BYTES).map_or(0, |count| count.words)
}

pub(crate) fn open_source_nofollow(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        // FILE_FLAG_OPEN_REPARSE_POINT opens the final component itself rather
        // than following a last-moment symlink or junction replacement.
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "source is not a regular file",
        ));
    }
    Ok(file)
}

fn count_text(text: &str, words: &mut usize, in_word: &mut bool) {
    for character in text.chars() {
        if character.is_whitespace() {
            if *in_word {
                *words = words.saturating_add(1);
                *in_word = false;
            }
        } else {
            *in_word = true;
        }
    }
}

/// Apply `String::from_utf8_lossy` semantics without retaining the complete
/// file. Returns the number of input bytes consumed; an incomplete trailing
/// sequence is left for the next fixed-size read unless `final_chunk` is true.
fn count_lossy_utf8(
    bytes: &[u8],
    final_chunk: bool,
    words: &mut usize,
    in_word: &mut bool,
) -> usize {
    let mut consumed = 0;
    while consumed < bytes.len() {
        match std::str::from_utf8(&bytes[consumed..]) {
            Ok(text) => {
                count_text(text, words, in_word);
                return bytes.len();
            }
            Err(error) => {
                let valid_end = consumed.saturating_add(error.valid_up_to());
                // SAFETY is not needed: `valid_up_to` is the UTF-8 validator's
                // boundary and `from_utf8` verifies that exact prefix again.
                if let Ok(text) = std::str::from_utf8(&bytes[consumed..valid_end]) {
                    count_text(text, words, in_word);
                }
                consumed = valid_end;
                if let Some(invalid_len) = error.error_len() {
                    // The replacement character is not whitespace, matching
                    // `String::from_utf8_lossy` word-boundary behavior.
                    *in_word = true;
                    consumed = consumed.saturating_add(invalid_len);
                } else if final_chunk {
                    *in_word = true;
                    return bytes.len();
                } else {
                    return consumed;
                }
            }
        }
    }
    consumed
}

fn count_words_with_cap(path: &Path, cap: usize) -> std::io::Result<BoundedWordCount> {
    let mut file = open_source_nofollow(path)?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut pending = Vec::with_capacity(buffer.len().saturating_add(4));
    let mut inspected = 0_usize;
    let mut words = 0_usize;
    let mut in_word = false;
    while inspected < cap {
        let requested = buffer.len().min(cap - inspected);
        let read = file.read(&mut buffer[..requested])?;
        if read == 0 {
            break;
        }
        inspected = inspected.saturating_add(read);
        pending.extend_from_slice(&buffer[..read]);
        let consumed = count_lossy_utf8(&pending, false, &mut words, &mut in_word);
        if consumed > 0 {
            pending.drain(..consumed);
        }
    }
    let consumed = count_lossy_utf8(&pending, true, &mut words, &mut in_word);
    debug_assert_eq!(consumed, pending.len());
    if in_word {
        words = words.saturating_add(1);
    }
    let mut marker = [0_u8; 1];
    let truncated = inspected == cap && file.read(&mut marker)? != 0;
    Ok(BoundedWordCount { words, truncated })
}

/// Whether a file exists, is regular, and is no larger than `cap` bytes.
pub fn file_within_size_cap(path: &Path, cap: u64) -> bool {
    open_source_with_size_cap(path, cap).is_some()
}

fn open_source_with_size_cap(path: &Path, cap: u64) -> Option<fs::File> {
    let file = open_source_nofollow(path).ok()?;
    (file.metadata().ok()?.len() <= cap).then_some(file)
}

/// Validate an Office ZIP with production resource ceilings.
pub fn zip_within_caps(path: &Path) -> bool {
    zip_within_caps_with(path, OfficeLimits::default())
}

/// Validate an Office ZIP with explicit ceilings for tests and constrained
/// callers. Declared central-directory sizes are only a pre-filter; every
/// member is then stream-decompressed into a fixed buffer and charged against
/// the aggregate ceiling.
pub fn zip_within_caps_with(path: &Path, limits: OfficeLimits) -> bool {
    validated_office_zip(path, limits).is_some()
}

type OfficeZipArchive = zip::ZipArchive<Cursor<Vec<u8>>>;

fn validated_office_zip(path: &Path, limits: OfficeLimits) -> Option<OfficeZipArchive> {
    if limits.max_members == 0 || limits.max_compression_ratio == 0 {
        return None;
    }
    let file = open_source_with_size_cap(path, limits.max_raw_bytes)?;
    validated_office_zip_from_checked_file(file, limits)
}

fn validated_office_zip_from_checked_file(
    file: fs::File,
    limits: OfficeLimits,
) -> Option<OfficeZipArchive> {
    // Metadata is only a snapshot. Read at most one byte beyond the raw-file
    // ceiling from the same no-follow handle so in-place growth cannot move an
    // unchecked generation into the ZIP parser.
    let bytes = read_checked_source_with_cap(file, limits.max_raw_bytes)?;
    let central_directory_cap = bytes.len().min(OFFICE_MAX_CENTRAL_DIRECTORY_BYTES);
    if !crate::containers::preflight_zip_metadata_with_limits(
        &bytes,
        limits.max_members,
        central_directory_cap,
    ) {
        return None;
    }
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    // The preflight and ZIP reader must agree on the selected directory. Keep
    // this release-mode check as defense in depth if the dependency ever
    // changes its footer selection behavior.
    if archive.len() > limits.max_members {
        return None;
    }

    let mut declared = 0_u64;
    let mut compressed = 0_u64;
    for index in 0..archive.len() {
        let member = archive.by_index(index).ok()?;
        declared = declared.checked_add(member.size())?;
        compressed = compressed.checked_add(member.compressed_size())?;
    }
    if declared > limits.max_decompressed_bytes
        || declared
            > compressed
                .max(1)
                .saturating_mul(limits.max_compression_ratio)
    {
        return None;
    }

    let mut total = 0_u64;
    let mut buffer = [0_u8; 1024 * 1024];
    for index in 0..archive.len() {
        let mut member = archive.by_index(index).ok()?;
        loop {
            let read = member.read(&mut buffer).ok()?;
            if read == 0 {
                break;
            }
            total = total.checked_add(read as u64)?;
            if total > limits.max_decompressed_bytes {
                return None;
            }
        }
    }
    Some(archive)
}

/// Safely extract PDF text after enforcing the same raw-file ceiling used for
/// untrusted Office inputs.
pub fn extract_pdf_text(path: &Path) -> String {
    extract_pdf_text_with_cap(path, OFFICE_MAX_RAW_BYTES)
}

pub fn extract_pdf_text_with_cap(path: &Path, cap: u64) -> String {
    let Some(file) = open_source_with_size_cap(path, cap) else {
        return String::new();
    };
    let Some(bytes) = read_checked_source_with_cap(file, cap) else {
        return String::new();
    };
    pdf_extract::extract_text_from_mem(&bytes).unwrap_or_default()
}

fn read_checked_source_with_cap(file: fs::File, cap: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(cap.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    (u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= cap).then_some(bytes)
}

/// Convert a bounded `.docx` package into plain Markdown paragraphs. XML is
/// streamed directly from the validated archive, and generated text has its
/// own output ceiling independent of ZIP metadata.
pub fn docx_to_markdown(path: &Path) -> String {
    let Some(mut archive) = validated_office_zip(path, OfficeLimits::default()) else {
        return String::new();
    };
    let Ok(member) = archive.by_name("word/document.xml") else {
        return String::new();
    };
    let mut reader = quick_xml::Reader::from_reader(BufReader::new(member));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut output = String::new();
    let mut text_depth = 0_usize;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
                if office_xml_local_name(event.name().as_ref()) == b"t" =>
            {
                text_depth += 1;
            }
            Ok(quick_xml::events::Event::Empty(event))
                if matches!(office_xml_local_name(event.name().as_ref()), b"tab" | b"br")
                    && !append_office_text(&mut output, " ") =>
            {
                return String::new();
            }
            Ok(quick_xml::events::Event::Text(event)) if text_depth > 0 => {
                let Some(text) = decode_office_text(&event) else {
                    return String::new();
                };
                if !append_office_text(&mut output, &text) {
                    return String::new();
                }
            }
            Ok(quick_xml::events::Event::End(event)) => {
                match office_xml_local_name(event.name().as_ref()) {
                    b"t" => text_depth = text_depth.saturating_sub(1),
                    b"p" | b"tr" if !append_office_text(&mut output, "\n") => {
                        return String::new();
                    }
                    b"tc" if !append_office_text(&mut output, " | ") => {
                        return String::new();
                    }
                    _ => {}
                }
            }
            Ok(quick_xml::events::Event::DocType(_))
            | Ok(quick_xml::events::Event::GeneralRef(_))
            | Err(_) => return String::new(),
            Ok(quick_xml::events::Event::Eof) => break,
            _ => {}
        }
        buffer.clear();
    }
    normalize_office_markdown(&output)
}

/// Convert bounded worksheet XML to a compact Markdown representation.
pub fn xlsx_to_markdown(path: &Path) -> String {
    let Some(mut archive) = validated_office_zip(path, OfficeLimits::default()) else {
        return String::new();
    };
    let shared_strings = read_xlsx_shared_strings(&mut archive).unwrap_or_default();
    let mut worksheet_names = Vec::new();
    for index in 0..archive.len() {
        let Ok(member) = archive.by_index(index) else {
            return String::new();
        };
        let name = member.name().replace('\\', "/");
        if name.starts_with("xl/worksheets/") && name.ends_with(".xml") {
            worksheet_names.push(name);
        }
    }
    worksheet_names.sort();
    worksheet_names.dedup();

    let mut output = String::new();
    for worksheet in worksheet_names {
        let Ok(member) = archive.by_name(&worksheet) else {
            return String::new();
        };
        let label = Path::new(&worksheet)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("Sheet");
        if !append_office_text(&mut output, &format!("## Sheet: {label}\n\n")) {
            return String::new();
        }
        let Some(rows) = read_xlsx_rows(member, &shared_strings) else {
            return String::new();
        };
        for (index, row) in rows.into_iter().enumerate() {
            if !append_office_text(&mut output, &format!("| {} |\n", row.join(" | "))) {
                return String::new();
            }
            if index == 0
                && !append_office_text(
                    &mut output,
                    &format!("| {} |\n", vec!["---"; row.len()].join(" | ")),
                )
            {
                return String::new();
            }
        }
        if !append_office_text(&mut output, "\n") {
            return String::new();
        }
    }
    normalize_office_markdown(&output)
}

fn read_xlsx_shared_strings(archive: &mut OfficeZipArchive) -> Option<Vec<String>> {
    let Ok(member) = archive.by_name("xl/sharedStrings.xml") else {
        return Some(Vec::new());
    };
    let mut reader = quick_xml::Reader::from_reader(BufReader::new(member));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut values = Vec::new();
    let mut current = String::new();
    let mut total_bytes = 0_usize;
    let mut text_depth = 0_usize;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
                if office_xml_local_name(event.name().as_ref()) == b"t" =>
            {
                text_depth += 1;
            }
            Ok(quick_xml::events::Event::Text(event)) if text_depth > 0 => {
                let text = decode_office_text(&event)?;
                if !append_office_text(&mut current, &text) {
                    return None;
                }
            }
            Ok(quick_xml::events::Event::End(event)) => {
                match office_xml_local_name(event.name().as_ref()) {
                    b"t" => text_depth = text_depth.saturating_sub(1),
                    b"si" => {
                        total_bytes = total_bytes.checked_add(current.len())?;
                        if total_bytes > OFFICE_MAX_MARKDOWN_BYTES {
                            return None;
                        }
                        values.push(std::mem::take(&mut current));
                    }
                    _ => {}
                }
            }
            Ok(quick_xml::events::Event::DocType(_))
            | Ok(quick_xml::events::Event::GeneralRef(_))
            | Err(_) => return None,
            Ok(quick_xml::events::Event::Eof) => break,
            _ => {}
        }
        buffer.clear();
    }
    Some(values)
}

fn read_xlsx_rows<R: Read>(member: R, shared_strings: &[String]) -> Option<Vec<Vec<String>>> {
    let mut reader = quick_xml::Reader::from_reader(BufReader::new(member));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut value = String::new();
    let mut value_depth = 0_usize;
    let mut shared = false;
    let mut total_bytes = 0_usize;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event)) => {
                match office_xml_local_name(event.name().as_ref()) {
                    b"c" => {
                        value.clear();
                        shared = event.attributes().filter_map(Result::ok).any(|attribute| {
                            office_xml_local_name(attribute.key.as_ref()) == b"t"
                                && attribute.value.as_ref() == b"s"
                        });
                    }
                    b"v" | b"t" => value_depth += 1,
                    _ => {}
                }
            }
            Ok(quick_xml::events::Event::Text(event)) if value_depth > 0 => {
                let text = decode_office_text(&event)?;
                if !append_office_text(&mut value, &text) {
                    return None;
                }
            }
            Ok(quick_xml::events::Event::End(event)) => {
                match office_xml_local_name(event.name().as_ref()) {
                    b"v" | b"t" => value_depth = value_depth.saturating_sub(1),
                    b"c" => {
                        let cell = if shared {
                            value
                                .trim()
                                .parse::<usize>()
                                .ok()
                                .and_then(|index| shared_strings.get(index))
                                .cloned()
                                .unwrap_or_default()
                        } else {
                            value.trim().to_owned()
                        };
                        row.push(cell.replace('|', "\\|"));
                    }
                    b"row" => {
                        if row.iter().any(|cell| !cell.is_empty()) {
                            let row_bytes = row.iter().try_fold(0_usize, |total, cell| {
                                total.checked_add(cell.len().saturating_add(3))
                            })?;
                            total_bytes = total_bytes.checked_add(row_bytes)?;
                            if total_bytes > OFFICE_MAX_MARKDOWN_BYTES {
                                return None;
                            }
                            rows.push(std::mem::take(&mut row));
                        } else {
                            row.clear();
                        }
                    }
                    _ => {}
                }
            }
            Ok(quick_xml::events::Event::DocType(_))
            | Ok(quick_xml::events::Event::GeneralRef(_))
            | Err(_) => return None,
            Ok(quick_xml::events::Event::Eof) => break,
            _ => {}
        }
        buffer.clear();
    }
    Some(rows)
}

fn office_xml_local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|byte| matches!(byte, b':' | b'}'))
        .next()
        .unwrap_or(name)
}

fn decode_office_text(event: &quick_xml::events::BytesText<'_>) -> Option<String> {
    let decoded = event.decode().ok()?;
    quick_xml::escape::unescape(&decoded)
        .ok()
        .map(|text| text.into_owned())
}

fn append_office_text(output: &mut String, text: &str) -> bool {
    let Some(next) = output.len().checked_add(text.len()) else {
        return false;
    };
    if next > OFFICE_MAX_MARKDOWN_BYTES {
        return false;
    }
    output.push_str(text);
    true
}

fn normalize_office_markdown(raw: &str) -> String {
    let mut lines = Vec::new();
    let mut blank = false;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            if !blank && !lines.is_empty() {
                lines.push(String::new());
            }
            blank = true;
        } else {
            lines.push(line.to_owned());
            blank = false;
        }
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

const SHEBANG_CODE_INTERPRETERS: &[&str] = &[
    "python", "python3", "python2", "ruby", "perl", "node", "nodejs", "bash", "sh", "dash", "zsh",
    "fish", "ksh", "tcsh", "lua", "php", "julia", "rscript",
];

fn shell_words(value: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(mark) = quote {
            if ch == mark {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        if matches!(ch, '\'' | '"') {
            quote = Some(ch);
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if escaped || quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        words.push(current);
    }
    Some(words)
}

fn env_command_args(args: &[String], allow_split: bool) -> Vec<String> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return args[index + 1..].to_vec();
        }
        if allow_split {
            let split = if matches!(arg.as_str(), "-S" | "-vS" | "--split-string") {
                if index + 1 >= args.len() {
                    return Vec::new();
                }
                Some(args[index + 1..].join(" "))
            } else if let Some(value) = arg.strip_prefix("--split-string=") {
                Some(
                    std::iter::once(value)
                        .chain(args[index + 1..].iter().map(String::as_str))
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            } else if let Some(value) = arg.strip_prefix("-vS") {
                (!value.is_empty()).then(|| {
                    std::iter::once(value)
                        .chain(args[index + 1..].iter().map(String::as_str))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
            } else if let Some(value) = arg.strip_prefix("-S") {
                (!value.is_empty()).then(|| {
                    std::iter::once(value)
                        .chain(args[index + 1..].iter().map(String::as_str))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
            } else {
                None
            };
            if let Some(split) = split {
                return shell_words(&split)
                    .map(|parts| env_command_args(&parts, false))
                    .unwrap_or_default();
            }
        }
        if ["-u", "-C", "-P", "-a", "--unset", "--chdir", "--argv0"].contains(&arg.as_str()) {
            if index + 1 >= args.len() {
                return Vec::new();
            }
            index += 2;
            continue;
        }
        if ["-u", "-C", "-P", "-a"]
            .iter()
            .any(|prefix| arg.starts_with(prefix) && arg.len() > 2 && !arg.starts_with("--"))
            || ["--unset=", "--chdir=", "--argv0="]
                .iter()
                .any(|prefix| arg.starts_with(prefix))
        {
            index += 1;
            continue;
        }
        if matches!(
            arg.as_str(),
            "-" | "-i"
                | "-0"
                | "-v"
                | "--ignore-environment"
                | "--null"
                | "--debug"
                | "--list-signal-handling"
        ) || ["--default-signal", "--ignore-signal", "--block-signal"]
            .iter()
            .any(|prefix| arg.starts_with(prefix))
        {
            index += 1;
            continue;
        }
        if arg.starts_with('-') {
            return Vec::new();
        }
        if arg.contains('=') {
            index += 1;
            continue;
        }
        return args[index..].to_vec();
    }
    Vec::new()
}

/// Return the basename of an extensionless script's shebang interpreter.
pub fn shebang_interpreter(path: &Path) -> Option<String> {
    let mut file = open_source_nofollow(path).ok()?;
    let mut bytes = [0_u8; 256];
    let read = file.read(&mut bytes).ok()?;
    shebang_interpreter_bytes(&bytes[..read])
}

/// Parse the basename of an extensionless script's shebang interpreter from
/// already admitted source bytes. The compute plane uses this variant so an
/// extractor never reopens a source path after I/O admission.
pub(crate) fn shebang_interpreter_bytes(bytes: &[u8]) -> Option<String> {
    let bytes = &bytes[..bytes.len().min(256)];
    if !bytes.starts_with(b"#!") {
        return None;
    }
    let first = bytes.split(|byte| *byte == b'\n').next()?;
    let line = String::from_utf8_lossy(&first[2..]);
    let parts = shell_words(line.trim())?;
    let first = parts.first()?;
    let mut interpreter = Path::new(first).file_name()?.to_string_lossy().into_owned();
    if interpreter == "env" {
        let args = env_command_args(&parts[1..], true);
        interpreter = Path::new(args.first()?)
            .file_name()?
            .to_string_lossy()
            .into_owned();
    }
    Some(interpreter)
}

pub(crate) fn has_code_shebang(path: &Path) -> bool {
    shebang_interpreter(path).is_some_and(|interpreter| {
        SHEBANG_CODE_INTERPRETERS
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(&interpreter))
    })
}

fn is_graphable_source(path: &Path) -> bool {
    classify_file(path) == Some(FileType::Code)
        && !matches!(
            lower_extension(path).as_str(),
            "json"
                | "yaml"
                | "yml"
                | "toml"
                | "ini"
                | "cfg"
                | "conf"
                | "config"
                | "xml"
                | "properties"
                | "env"
                | "txt"
                | "tfvars"
        )
}

fn env_template(name: &str) -> bool {
    let lower = name.to_lowercase();
    [".example", ".sample", ".template", ".dist"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
        && (lower.starts_with(".env.") || lower.starts_with(".envrc."))
}

fn stem_for_sensitive(name: &str) -> String {
    let stripped = name.strip_prefix('.').unwrap_or(name);
    stripped
        .rsplit_once('.')
        .map_or(stripped, |(stem, _)| stem)
        .trim_start_matches('.')
        .to_lowercase()
}

fn keyword_occurrences(stem: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = stem.chars().collect();
    let mut hits = Vec::new();
    for index in 0..chars.len() {
        if index > 0 && chars[index - 1].is_ascii_alphanumeric() {
            continue;
        }
        let tail: String = chars[index..].iter().collect();
        for keyword in [
            "credential",
            "credentials",
            "secret",
            "secrets",
            "passwd",
            "passwds",
            "password",
            "passwords",
            "private_key",
            "private_keys",
            "token",
            "tokens",
            "service_account",
            "service-account",
            "service.account",
            "serviceaccount",
        ] {
            if tail.starts_with(keyword) {
                let end = index + keyword.chars().count();
                if end == chars.len() || !chars[end].is_ascii_alphabetic() {
                    hits.push((index, end));
                }
            }
        }
    }
    hits
}

fn generic_keyword_hit(name: &str) -> bool {
    let stem = stem_for_sensitive(name);
    let hits = keyword_occurrences(&stem);
    if hits.iter().any(|(_, end)| *end == stem.chars().count()) {
        return true;
    }
    let words = stem
        .split(['-', '_', '.', ' ', '\t'])
        .filter(|word| !word.is_empty())
        .count();
    !hits.is_empty() && words <= 2
}

fn prose_note(path: &Path) -> bool {
    if !["md", "markdown", "rst", "org", "adoc", "tex"].contains(&lower_extension(path).as_str()) {
        return false;
    }
    let stem = stem_for_sensitive(
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default(),
    );
    let bare = [
        "credential",
        "credentials",
        "secret",
        "secrets",
        "passwd",
        "passwds",
        "password",
        "passwords",
        "private_key",
        "private_keys",
        "token",
        "tokens",
        "service_account",
        "service-account",
        "serviceaccount",
    ];
    !bare.contains(&stem.as_str())
}

/// Whether a path is likely to be a live credential or key store.
pub fn is_sensitive(path: &Path) -> bool {
    is_sensitive_with_path_policy(path, true)
}

/// Apply the sensitive-path policy without opening or inspecting the payload.
///
/// Coverage discovery uses this conservative variant before it attempts any
/// source handle. Extensionless names that would require shebang inspection
/// therefore remain sensitive when their names match credential policy.
pub(crate) fn is_sensitive_path_only(path: &Path) -> bool {
    is_sensitive_with_path_policy(path, false)
}

fn is_graphable_path_without_content(path: &Path) -> bool {
    let code = is_package_manifest(path)
        || path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| name.to_lowercase().ends_with(".blade.php"))
        || format_registry()
            .find_by_path(path)
            .and_then(|spec| spec.legacy_file_type)
            == Some(FileType::Code);
    code && !matches!(
        lower_extension(path).as_str(),
        "json"
            | "jsonl"
            | "ndjson"
            | "yaml"
            | "yml"
            | "toml"
            | "ini"
            | "cfg"
            | "conf"
            | "config"
            | "xml"
            | "properties"
            | "env"
            | "txt"
            | "tfvars"
    )
}

fn is_sensitive_with_path_policy(path: &Path, inspect_content: bool) -> bool {
    let graphable = || {
        if inspect_content {
            is_graphable_source(path)
        } else {
            is_graphable_path_without_content(path)
        }
    };
    let parent_names: Vec<String> = path
        .parent()
        .map(|parent| {
            parent
                .components()
                .filter_map(|component| match component {
                    Component::Normal(value) => Some(value.to_string_lossy().to_lowercase()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    if parent_names
        .iter()
        .any(|part| [".ssh", ".gnupg", ".aws", ".gcloud"].contains(&part.as_str()))
    {
        return true;
    }
    if parent_names
        .iter()
        .any(|part| ["secrets", ".secrets", "credentials"].contains(&part.as_str()))
        && !graphable()
    {
        return true;
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let lower = name.to_lowercase();
    let private_key_stem = lower.strip_suffix(".pub").unwrap_or(&lower);
    let private_key_name = ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"]
        .iter()
        .any(|key| {
            private_key_stem.strip_suffix(key).is_some_and(|prefix| {
                prefix
                    .chars()
                    .next_back()
                    .is_none_or(|ch| !ch.is_ascii_alphanumeric())
            })
        });
    let specific = (lower.starts_with(".env") || lower.starts_with(".envrc"))
        || [
            ".pem", ".key", ".p12", ".pfx", ".cert", ".crt", ".der", ".p8",
        ]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
        || private_key_name
        || lower == "secring"
        || lower == "secring.gpg"
        || lower == "secring.pgp"
        || [
            ".netrc",
            ".pgpass",
            ".htpasswd",
            ".npmrc",
            ".pypirc",
            ".git-credentials",
            ".boto",
        ]
        .contains(&lower.as_str());
    if specific && !env_template(name) {
        return true;
    }
    if generic_keyword_hit(name) {
        return !(graphable() || prose_note(path));
    }
    false
}

fn has_coverage_artifacts(path: &Path) -> bool {
    [
        "lcov.info",
        "coverage-final.json",
        "coverage-summary.json",
        "clover.xml",
        "coverage.xml",
        "cobertura-coverage.xml",
        "jacoco.xml",
        ".coverage",
        "index.html",
    ]
    .iter()
    .any(|name| path.join(name).is_file())
        || ["lcov-report", "html-report"]
            .iter()
            .any(|name| path.join(name).is_dir())
}

fn has_venv_markers(path: &Path) -> bool {
    path.join("pyvenv.cfg").is_file()
        || path.join("bin/activate").is_file()
        || path.join("Scripts/activate").is_file()
        || path.join("conda-meta").is_dir()
        || fs::read_dir(path.join("lib")).ok().is_some_and(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                entry.file_type().ok().is_some_and(|kind| kind.is_dir())
                    && entry.file_name().to_string_lossy().starts_with("python")
            })
        })
}

/// Evidence-gated noise-directory predicate.
pub fn is_noise_dir(name: &str, parent: Option<&Path>) -> bool {
    if SKIP_DIRS.contains(&name) || name.ends_with("_venv") || name.ends_with(".egg-info") {
        return true;
    }
    if matches!(name, "env" | ".env") || name.ends_with("_env") {
        return parent.is_some_and(|parent| has_venv_markers(&parent.join(name)));
    }
    if name == "coverage" {
        return parent.is_some_and(|parent| has_coverage_artifacts(&parent.join(name)));
    }
    if name == "snapshots" {
        return parent.is_some_and(|parent| {
            ["__tests__", "__test__"].contains(
                &parent
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default(),
            ) || fs::read_dir(parent.join(name)).ok().is_some_and(|entries| {
                entries.filter_map(Result::ok).any(|entry| {
                    entry.path().extension().and_then(|value| value.to_str()) == Some("snap")
                })
            })
        });
    }
    name == "worktrees"
        && parent
            .and_then(Path::file_name)
            .and_then(|value| value.to_str())
            .is_some_and(|parent| parent.starts_with('.'))
}

pub(crate) fn parse_ignore_line(raw: &str) -> Option<String> {
    let mut line = raw.trim_end_matches(['\n', '\r']).trim_start().to_owned();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    if let Some(index) = line
        .char_indices()
        .find(|(index, ch)| {
            *ch == '#'
                && *index > 0
                && line[..*index]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace)
        })
        .map(|(index, _)| index)
    {
        line.truncate(index);
    }
    line = line.replace("\\#", "#");
    while line.ends_with(' ') && !line.ends_with("\\ ") {
        line.pop();
    }
    (!line.is_empty()).then_some(line)
}

fn read_ignore_file(path: &Path, anchor: &Path) -> Vec<IgnorePattern> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    let mut content = String::from_utf8_lossy(&bytes).into_owned();
    if content.starts_with('\u{feff}') {
        content.remove(0);
    }
    content
        .lines()
        .filter_map(parse_ignore_line)
        .map(|pattern| IgnorePattern {
            anchor: anchor.to_path_buf(),
            pattern,
        })
        .collect()
}

fn find_vcs_root(start: &Path) -> Option<PathBuf> {
    let mut current = fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|path| fs::canonicalize(path).ok());
    loop {
        if is_vcs_root(&current) {
            return Some(current);
        }
        if home.as_ref().is_some_and(|home| *home == current) {
            return None;
        }
        let parent = current.parent()?.to_path_buf();
        if parent == current {
            return None;
        }
        current = parent;
    }
}

fn is_vcs_root(directory: &Path) -> bool {
    let dot_git = directory.join(".git");
    let git_valid = if dot_git.is_dir() {
        dot_git.join("HEAD").is_file()
    } else if dot_git.is_file() {
        fs::read_to_string(&dot_git)
            .ok()
            .and_then(|value| {
                value
                    .trim()
                    .strip_prefix("gitdir:")
                    .map(str::trim)
                    .map(PathBuf::from)
            })
            .map(|git_dir| {
                if git_dir.is_absolute() {
                    git_dir
                } else {
                    directory.join(git_dir)
                }
            })
            .is_some_and(|git_dir| git_dir.join("HEAD").is_file())
    } else {
        false
    };
    git_valid
        || [".hg", ".svn", "_darcs", ".fossil"]
            .iter()
            .any(|marker| directory.join(marker).exists())
}

fn git_info_exclude(vcs_root: &Path) -> Option<PathBuf> {
    let dot_git = vcs_root.join(".git");
    let mut git_dir = if dot_git.is_dir() {
        dot_git
    } else if dot_git.is_file() {
        let value = fs::read_to_string(&dot_git).ok()?;
        let value = value.trim().strip_prefix("gitdir:")?.trim();
        let value = PathBuf::from(value);
        if value.is_absolute() {
            value
        } else {
            vcs_root.join(value)
        }
    } else {
        return None;
    };
    if let Ok(common) = fs::read_to_string(git_dir.join("commondir")) {
        let common = PathBuf::from(common.trim());
        git_dir = if common.is_absolute() {
            common
        } else {
            git_dir.join(common)
        };
    }
    let path = git_dir.join("info/exclude");
    path.is_file().then_some(path)
}

pub(crate) fn load_dir_ignore(directory: &Path, honor_gitignore: bool) -> Vec<IgnorePattern> {
    let mut patterns = Vec::new();
    if honor_gitignore {
        patterns.extend(read_ignore_file(&directory.join(".gitignore"), directory));
    }
    patterns.extend(read_ignore_file(
        &directory.join(".graphifyignore"),
        directory,
    ));
    patterns.extend(read_ignore_file(
        &directory.join(".graphoxideignore"),
        directory,
    ));
    patterns
}

/// Load ignore rules from the VCS ceiling down to the scan root.
pub fn load_ignore_patterns(root: &Path, honor_gitignore: bool) -> Vec<IgnorePattern> {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let ceiling = find_vcs_root(&root).unwrap_or_else(|| root.clone());
    let mut directories = vec![root.clone()];
    while directories
        .last()
        .is_some_and(|directory| *directory != ceiling)
    {
        let Some(parent) = directories.last().and_then(|directory| directory.parent()) else {
            break;
        };
        directories.push(parent.to_path_buf());
    }
    directories.reverse();
    let mut patterns = Vec::new();
    if honor_gitignore && let Some(exclude) = git_info_exclude(&ceiling) {
        patterns.extend(read_ignore_file(&exclude, &ceiling));
    }
    for directory in directories {
        patterns.extend(load_dir_ignore(&directory, honor_gitignore));
    }
    patterns
}

fn component_glob(pattern: &str, value: &str) -> bool {
    let mut regex = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            '[' => {
                regex.push('[');
                if chars.peek() == Some(&'!') {
                    chars.next();
                    regex.push('^');
                }
                for class in chars.by_ref() {
                    regex.push(class);
                    if class == ']' {
                        break;
                    }
                }
            }
            other => regex.push_str(&regex::escape(&other.to_string())),
        }
    }
    regex.push('$');
    Regex::new(&regex).is_ok_and(|regex| regex.is_match(value))
}

fn anchored_glob(path: &str, pattern: &str) -> bool {
    fn matches(path: &[&str], pattern: &[&str]) -> bool {
        if pattern.is_empty() {
            return path.is_empty();
        }
        if pattern[0] == "**" {
            if pattern.len() == 1 {
                return !path.is_empty();
            }
            return matches(path, &pattern[1..])
                || (!path.is_empty() && matches(&path[1..], pattern));
        }
        !path.is_empty()
            && component_glob(pattern[0], path[0])
            && matches(&path[1..], &pattern[1..])
    }
    matches(
        &path.split('/').collect::<Vec<_>>(),
        &pattern.split('/').collect::<Vec<_>>(),
    )
}

fn eval_ignore(target: &Path, patterns: &[IgnorePattern]) -> bool {
    let mut result = false;
    for entry in patterns {
        let negated = entry.pattern.starts_with('!');
        let raw = entry.pattern.strip_prefix('!').unwrap_or(&entry.pattern);
        let directory_only = raw.ends_with('/');
        let path_relative = raw.trim_end_matches('/').contains('/');
        let pattern = raw.trim_matches('/');
        if pattern.is_empty() {
            continue;
        }
        let Ok(relative) = target.strip_prefix(&entry.anchor) else {
            continue;
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        let relative = relative.to_string_lossy().replace('\\', "/");
        let matched = if path_relative {
            anchored_glob(&relative, pattern)
        } else {
            relative
                .split('/')
                .any(|component| component_glob(pattern, component))
        };
        if matched && (!directory_only || target.is_dir()) {
            result = !negated;
        }
    }
    result
}

/// Apply last-match-wins ignore rules and git's parent-exclusion rule.
pub fn is_ignored(path: &Path, root: &Path, patterns: &[IgnorePattern]) -> bool {
    // macOS commonly exposes the same temporary tree as both `/var/...` and
    // `/private/var/...`. Re-anchor lexically instead of canonicalizing the
    // target itself, which would erase an in-root symlink's alias name.
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let target = path
        .strip_prefix(root)
        .map(|relative| canonical_root.join(relative))
        .unwrap_or_else(|_| path.to_path_buf());
    let Ok(relative) = target.strip_prefix(&canonical_root) else {
        return eval_ignore(&target, patterns);
    };
    let mut ancestor = canonical_root;
    let components: Vec<_> = relative.components().collect();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        ancestor.push(component.as_os_str());
        if eval_ignore(&ancestor, patterns) {
            return true;
        }
    }
    eval_ignore(&target, patterns)
}

/// Cached variant used by the directory walker. Shared ancestors are evaluated
/// once even when a subtree contains thousands of sibling files.
pub fn is_ignored_with_cache(
    path: &Path,
    root: &Path,
    patterns: &[IgnorePattern],
    cache: &mut HashMap<PathBuf, bool>,
) -> bool {
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let target = path
        .strip_prefix(root)
        .map(|relative| canonical_root.join(relative))
        .unwrap_or_else(|_| path.to_path_buf());
    let mut evaluate = |target: &Path| {
        if let Some(value) = cache.get(target) {
            return *value;
        }
        let value = eval_ignore(target, patterns);
        cache.insert(target.to_path_buf(), value);
        value
    };
    let Ok(relative) = target.strip_prefix(&canonical_root) else {
        return evaluate(&target);
    };
    let mut ancestor = canonical_root;
    let components: Vec<_> = relative.components().collect();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        ancestor.push(component.as_os_str());
        if evaluate(&ancestor) {
            return true;
        }
    }
    evaluate(&target)
}

fn resolves_under(path: &Path, root: &Path) -> bool {
    fs::canonicalize(path)
        .ok()
        .and_then(|resolved| {
            fs::canonicalize(root)
                .ok()
                .map(|root| resolved.starts_with(root))
        })
        .unwrap_or(false)
}

pub(crate) fn output_dir(root: &Path, options: &DetectOptions) -> PathBuf {
    options.output_dir.clone().map_or_else(
        || {
            std::env::var_os("GRAPH_OXIDE_OUT")
                .or_else(|| std::env::var_os("GRAPHIFY_OUT"))
                .map(PathBuf::from)
                .map(|path| {
                    if path.is_absolute() {
                        path
                    } else {
                        root.join(path)
                    }
                })
                .unwrap_or_else(|| root.join("graphoxide-out"))
        },
        |path| {
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        },
    )
}

struct WalkState<'a> {
    root: &'a Path,
    options: &'a DetectOptions,
    configured_output: PathBuf,
    patterns: Vec<IgnorePattern>,
    paths: Vec<DiscoveredPath>,
    ignored: Vec<String>,
    pruned_noise: Vec<String>,
    skipped_sensitive: Vec<String>,
    errors: Vec<String>,
    active_targets: HashSet<PathBuf>,
    ignore_cache: HashMap<PathBuf, bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiscoveredPath {
    logical: PathBuf,
    physical: PathBuf,
}

impl WalkState<'_> {
    fn walk(&mut self, directory: &Path, memory_tree: bool) {
        let target = fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
        if !self.active_targets.insert(target.clone()) {
            return;
        }
        if !memory_tree && directory != self.root {
            self.patterns
                .extend(load_dir_ignore(directory, self.options.honor_gitignore));
        }
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) => {
                self.errors
                    .push(format!("{}: {error}", directory.display()));
                self.active_targets.remove(&target);
                return;
            }
        };
        let mut collected = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => collected.push(entry),
                Err(error) => self
                    .errors
                    .push(format!("{}: {error}", directory.display())),
            }
        }
        let mut entries = collected;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else {
                self.errors
                    .push(format!("{}: unable to inspect file type", path.display()));
                continue;
            };
            let symlink = kind.is_symlink();
            let directory_entry = kind.is_dir() || (symlink && path.is_dir());
            if directory_entry {
                // `outer!/member` is the serialized identity reserved for
                // logical container members. Do not admit a physical
                // directory whose name would create the same project-relative
                // boundary and make graph IDs/provenance ambiguous.
                if name.ends_with('!') {
                    self.ignored.push(format!(
                        "{} [directory name ending in ! is reserved for virtual container members]",
                        path.display()
                    ));
                    continue;
                }
                if !memory_tree {
                    let configured = fs::canonicalize(&path)
                        .ok()
                        .zip(fs::canonicalize(&self.configured_output).ok())
                        .is_some_and(|(left, right)| left == right)
                        || path == self.configured_output;
                    if configured || is_noise_dir(&name, Some(directory)) {
                        self.pruned_noise.push(format!("{}/", path.display()));
                        continue;
                    }
                    if is_ignored_with_cache(
                        &path,
                        self.root,
                        &self.patterns,
                        &mut self.ignore_cache,
                    ) {
                        self.ignored.push(format!("{}/", path.display()));
                        continue;
                    }
                    if symlink {
                        if !self.options.follow_symlinks {
                            continue;
                        }
                        if !resolves_under(&path, self.root) {
                            self.skipped_sensitive.push(format!(
                                "{} [symlink target outside scan root]",
                                path.display()
                            ));
                            continue;
                        }
                    }
                }
                self.walk(&path, memory_tree);
            } else if symlink {
                if !self.options.follow_symlinks {
                    self.skipped_sensitive.push(format!(
                        "{} [file symlink skipped - enable follow symlinks]",
                        path.display()
                    ));
                    continue;
                }
                let Ok(physical) = fs::canonicalize(&path) else {
                    self.skipped_sensitive.push(format!(
                        "{} [symlink target outside scan root or unavailable]",
                        path.display()
                    ));
                    continue;
                };
                if !physical.starts_with(self.root) {
                    self.skipped_sensitive.push(format!(
                        "{} [symlink target outside scan root]",
                        path.display()
                    ));
                    continue;
                }
                if !fs::symlink_metadata(&physical)
                    .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
                {
                    self.skipped_sensitive.push(format!(
                        "{} [symlink target outside scan root or unavailable]",
                        path.display()
                    ));
                    continue;
                }
                if physical != path && (is_sensitive(&path) || is_sensitive(&physical)) {
                    self.skipped_sensitive
                        .push(path.to_string_lossy().into_owned());
                    continue;
                }
                if !SKIP_FILES.contains(&name.as_str()) {
                    self.paths.push(DiscoveredPath {
                        logical: path,
                        physical,
                    });
                }
            } else if kind.is_file() && !SKIP_FILES.contains(&name.as_str()) {
                let Ok(physical) = fs::canonicalize(&path) else {
                    self.errors.push(format!(
                        "{}: unable to resolve regular source",
                        path.display()
                    ));
                    continue;
                };
                if !physical.starts_with(self.root)
                    || !fs::symlink_metadata(&physical).is_ok_and(|metadata| {
                        metadata.is_file() && !metadata.file_type().is_symlink()
                    })
                {
                    self.skipped_sensitive.push(format!(
                        "{} [resolved source outside scan root or unavailable]",
                        path.display()
                    ));
                    continue;
                }
                if physical != path && (is_sensitive(&path) || is_sensitive(&physical)) {
                    self.skipped_sensitive
                        .push(path.to_string_lossy().into_owned());
                    continue;
                }
                self.paths.push(DiscoveredPath {
                    logical: path,
                    physical,
                });
            }
        }
        self.active_targets.remove(&target);
    }
}

/// Enumerate a corpus with complete diagnostics.
pub fn detect(root: &Path, options: &DetectOptions) -> anyhow::Result<DetectResult> {
    let root = fs::canonicalize(root)?;
    if root.join(".graphifyinclude").is_file() {
        eprintln!(
            "[graphoxide] WARNING: .graphifyinclude is no longer supported; use ! negation patterns in .graphifyignore"
        );
    }
    let configured_output = output_dir(&root, options);
    let mut patterns = load_ignore_patterns(&root, options.honor_gitignore);
    patterns.extend(options.extra_excludes.iter().filter_map(|pattern| {
        parse_ignore_line(pattern).map(|pattern| IgnorePattern {
            anchor: root.clone(),
            pattern,
        })
    }));
    let mut state = WalkState {
        root: &root,
        options,
        configured_output: configured_output.clone(),
        patterns,
        paths: Vec::new(),
        ignored: Vec::new(),
        pruned_noise: Vec::new(),
        skipped_sensitive: Vec::new(),
        errors: Vec::new(),
        active_targets: HashSet::new(),
        ignore_cache: HashMap::new(),
    };
    state.walk(&root, false);
    let memory = configured_output.join("memory");
    if memory.is_dir() {
        state.walk(&memory, true);
    }
    // A physical source is indexed once. Prefer its ordinary in-tree spelling
    // over a symlink alias, then use lexical order as the stable tie-breaker.
    state.paths.sort_by(|left, right| {
        left.physical
            .cmp(&right.physical)
            .then_with(|| (left.logical != left.physical).cmp(&(right.logical != right.physical)))
            .then_with(|| left.logical.cmp(&right.logical))
    });
    state
        .paths
        .dedup_by(|left, right| left.physical == right.physical);
    state
        .paths
        .sort_by(|left, right| left.logical.cmp(&right.logical));

    let mut files: DetectedFiles = FileType::ALL
        .iter()
        .map(|kind| (kind.as_str().to_owned(), Vec::new()))
        .collect();
    let mut total_words = 0_usize;
    let mut unclassified = Vec::new();
    let mut word_count_truncations = Vec::new();
    let mut physical_sources = BTreeMap::new();
    let converted = configured_output.join("converted");
    for discovered in state.paths {
        let path = discovered.logical;
        let physical = discovered.physical;
        let in_memory = memory.is_dir() && path.starts_with(&memory);
        if path.starts_with(&converted) || physical.starts_with(&converted) {
            continue;
        }
        if !in_memory
            && is_ignored_with_cache(&path, &root, &state.patterns, &mut state.ignore_cache)
        {
            state.ignored.push(path.to_string_lossy().into_owned());
            continue;
        }
        if !physical.starts_with(&root)
            || !fs::symlink_metadata(&physical)
                .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        {
            state.skipped_sensitive.push(format!(
                "{} [resolved source changed or left scan root]",
                path.display()
            ));
            continue;
        }
        if is_sensitive(&path) || is_sensitive(&physical) {
            state
                .skipped_sensitive
                .push(path.to_string_lossy().into_owned());
            continue;
        }
        // Keep `classify_file` as the compatibility-facing legacy projection,
        // but admit registered byte-only formats into the document work queue.
        // This adds no watch suffixes and retains `None` for callers that ask
        // whether a path belongs to the historical classification contract.
        let kind = classify_file_at(&path, &physical).or_else(|| {
            format_registry()
                .find_by_path(&path)
                .map(|_| FileType::Document)
        });
        let Some(kind) = kind else {
            unclassified.push(path.to_string_lossy().into_owned());
            continue;
        };
        if format_registry().is_google_workspace_extension(&lower_extension(&path)) {
            if !options.google_workspace {
                state.skipped_sensitive.push(format!(
                    "{} [Google Workspace shortcut skipped - enable Google Workspace conversion]",
                    path.display()
                ));
                continue;
            }
            let text = fs::read_to_string(&physical).unwrap_or_default();
            let body = format!(
                "# {}\n\n{}",
                path.file_stem().unwrap_or_default().to_string_lossy(),
                text
            );
            if let Some(sidecar) = convert_office_text(&path, &converted, Some(&root), &body)? {
                if is_ignored_with_cache(&sidecar, &root, &state.patterns, &mut state.ignore_cache)
                {
                    continue;
                }
                if let Ok(count) = count_words_with_cap(&sidecar, WORD_COUNT_MAX_BYTES) {
                    total_words = total_words.saturating_add(count.words);
                    if count.truncated {
                        word_count_truncations.push(format!(
                            "{} [word count truncated at {} bytes]",
                            sidecar.display(),
                            WORD_COUNT_MAX_BYTES
                        ));
                    }
                }
                let sidecar_physical =
                    fs::canonicalize(&sidecar).unwrap_or_else(|_| sidecar.clone());
                physical_sources.insert(
                    sidecar.to_string_lossy().into_owned(),
                    sidecar_physical.to_string_lossy().into_owned(),
                );
                files
                    .get_mut(FileType::Document.as_str())
                    .expect("document bucket")
                    .push(sidecar.to_string_lossy().into_owned());
            }
            continue;
        }
        if options.convert_office_sidecars
            && format_registry().is_office_extension(&lower_extension(&path))
        {
            let body = match lower_extension(&path).as_str() {
                "docx" => docx_to_markdown(&physical),
                "xlsx" => xlsx_to_markdown(&physical),
                _ => String::new(),
            };
            if let Some(sidecar) = convert_office_text(&path, &converted, Some(&root), &body)? {
                if is_ignored_with_cache(&sidecar, &root, &state.patterns, &mut state.ignore_cache)
                {
                    continue;
                }
                if let Ok(count) = count_words_with_cap(&sidecar, WORD_COUNT_MAX_BYTES) {
                    total_words = total_words.saturating_add(count.words);
                    if count.truncated {
                        word_count_truncations.push(format!(
                            "{} [word count truncated at {} bytes]",
                            sidecar.display(),
                            WORD_COUNT_MAX_BYTES
                        ));
                    }
                }
                let sidecar_physical =
                    fs::canonicalize(&sidecar).unwrap_or_else(|_| sidecar.clone());
                physical_sources.insert(
                    sidecar.to_string_lossy().into_owned(),
                    sidecar_physical.to_string_lossy().into_owned(),
                );
                files
                    .get_mut(FileType::Document.as_str())
                    .expect("document bucket")
                    .push(sidecar.to_string_lossy().into_owned());
            } else {
                state.skipped_sensitive.push(format!(
                    "{} [office conversion failed or resource limits rejected the file]",
                    path.display()
                ));
            }
            continue;
        }
        if kind != FileType::Video
            && !matches!(lower_extension(&path).as_str(), "pdf" | "docx" | "xlsx")
            && let Ok(count) = count_words_with_cap(&physical, WORD_COUNT_MAX_BYTES)
        {
            total_words = total_words.saturating_add(count.words);
            if count.truncated {
                word_count_truncations.push(format!(
                    "{} [word count truncated at {} bytes]",
                    path.display(),
                    WORD_COUNT_MAX_BYTES
                ));
            }
        }
        physical_sources.insert(
            path.to_string_lossy().into_owned(),
            physical.to_string_lossy().into_owned(),
        );
        files
            .get_mut(kind.as_str())
            .expect("file-type bucket")
            .push(path.to_string_lossy().into_owned());
    }
    for paths in files.values_mut() {
        paths.sort();
    }
    unclassified.sort();
    state.ignored.sort();
    state.pruned_noise.sort();
    state.skipped_sensitive.sort();
    word_count_truncations.sort();
    let total_files = files.values().map(Vec::len).sum();
    let needs_graph = total_words >= CORPUS_WARN_THRESHOLD;
    let warning = if !needs_graph {
        Some(format!(
            "Corpus is ~{total_words} words - fits in a single context window. You may not need a graph."
        ))
    } else if total_words >= CORPUS_UPPER_THRESHOLD || total_files >= FILE_COUNT_UPPER {
        Some(format!(
            "Large corpus: {total_files} files · ~{total_words} words. Semantic extraction may be expensive."
        ))
    } else {
        None
    };
    Ok(DetectResult {
        files,
        total_files,
        total_words,
        needs_graph,
        warning,
        skipped_sensitive: state.skipped_sensitive,
        unclassified,
        walk_errors: state.errors,
        word_count_truncations,
        ignored: state.ignored,
        pruned_noise_dirs: state.pruned_noise,
        graphifyignore_patterns: state.patterns.len(),
        scan_root: root.to_string_lossy().into_owned(),
        physical_sources,
    })
}

/// Source files accepted by the offline extraction pipeline.
pub fn collect_files(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let result = detect(root, &DetectOptions::default())?;
    let mut paths: Vec<_> = result
        .files
        .values()
        .flatten()
        .map(PathBuf::from)
        .filter(|path| result.is_supported_source(path))
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Whether discovery intentionally excludes this exact file name from the
/// legacy extraction queue.
///
/// Coverage reporting uses this policy predicate to keep such files visible
/// without claiming that a registered suffix means they are currently routed
/// to an extractor.
pub(crate) fn is_policy_excluded_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| SKIP_FILES.contains(&name))
}

/// Whether a changed path belongs to the offline structural extraction tier.
pub fn is_supported_path(path: &Path) -> bool {
    is_supported_path_at(path, path)
}

fn is_supported_path_at(logical_path: &Path, physical_path: &Path) -> bool {
    let name = logical_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    !SKIP_FILES.contains(&name)
        && !is_sensitive(logical_path)
        && !is_sensitive(physical_path)
        && (matches!(
            classify_file_at(logical_path, physical_path),
            Some(FileType::Code | FileType::Document | FileType::Paper | FileType::Image)
        ) || crate::format_registry::format_registry()
            .find_by_path(logical_path)
            .is_some()
            || (logical_path.extension().is_none() && has_code_shebang(physical_path)))
}

fn nfc(value: &str) -> String {
    value.nfc().collect()
}

/// Render a path for Windows extended-length file APIs.
///
/// `windows` is explicit so the transformation can be regression-tested on
/// every host. Production callers pass `cfg!(windows)` through [`os_path`].
pub fn os_path_for(path: &Path, windows: bool) -> String {
    let original = path.to_string_lossy();
    if !windows || original.starts_with(r"\\?\") {
        return original.into_owned();
    }
    let absolute = if path.is_absolute() {
        original.into_owned()
    } else {
        std::env::current_dir()
            .map(|root| root.join(path).to_string_lossy().into_owned())
            .unwrap_or_else(|_| original.into_owned())
    };
    if let Some(unc) = absolute.strip_prefix(r"\\") {
        format!(r"\\?\UNC\{unc}")
    } else {
        format!(r"\\?\{absolute}")
    }
}

pub fn os_path(path: &Path) -> String {
    os_path_for(path, cfg!(windows))
}

/// MD5 of file contents, used solely for change detection.
pub fn md5_file(path: &Path) -> String {
    let Ok(mut file) = fs::File::open(os_path(path)) else {
        return String::new();
    };
    let mut hash = Md5::new();
    let mut buffer = [0_u8; 65_536];
    loop {
        let Ok(read) = file.read(&mut buffer) else {
            return String::new();
        };
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    format!("{:x}", hash.finalize())
}

/// Read a file's modification time and digest as one incremental-scan fact.
pub fn stat_and_hash(path: &str) -> Option<(String, f64, String)> {
    let physical = Path::new(path);
    let modified = fs::metadata(os_path(physical))
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs_f64();
    Some((path.to_owned(), modified, md5_file(physical)))
}

fn content_md5(path: &Path) -> String {
    md5_file(path)
}

fn modified_time(path: &Path) -> Option<f64> {
    fs::metadata(os_path(path))
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs_f64())
}

fn normalize_manifest_entry(value: &Value) -> Option<Map<String, Value>> {
    if let Some(number) = value.as_f64() {
        return Some(Map::from_iter([
            ("mtime".into(), Value::from(number)),
            ("ast_hash".into(), Value::String(String::new())),
            ("semantic_hash".into(), Value::String(String::new())),
        ]));
    }
    let mut entry = value.as_object()?.clone();
    if entry.contains_key("hash") && !entry.contains_key("ast_hash") {
        let hash = entry.remove("hash").unwrap_or(Value::String(String::new()));
        entry.insert("ast_hash".into(), hash);
        entry.insert("semantic_hash".into(), Value::String(String::new()));
    }
    Some(entry)
}

fn lexical_absolute(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn storage_key(path: &str, root: Option<&Path>) -> String {
    let path = Path::new(path);
    let Some(root) = root else {
        return nfc(&lexical_absolute(path).to_string_lossy());
    };
    let lexical_root = lexical_absolute(root);
    let root = fs::canonicalize(root).unwrap_or_else(|_| lexical_root.clone());
    let absolute = lexical_absolute(path);
    absolute
        .strip_prefix(&lexical_root)
        .or_else(|_| absolute.strip_prefix(&root))
        .map_or_else(
            |_| nfc(&absolute.to_string_lossy()),
            |relative| nfc(&relative.to_string_lossy().replace('\\', "/")),
        )
}

/// Load a manifest, optionally re-anchoring portable relative keys.
pub fn load_manifest(path: &Path, root: Option<&Path>) -> BTreeMap<String, Value> {
    let Ok(bytes) = fs::read(path) else {
        return BTreeMap::new();
    };
    let Ok(raw) = serde_json::from_slice::<BTreeMap<String, Value>>(&bytes) else {
        return BTreeMap::new();
    };
    raw.into_iter()
        .map(|(key, value)| {
            let key_path = Path::new(&key);
            let key = if let Some(root) = root {
                let lexical_root = lexical_absolute(root);
                let canonical_root =
                    fs::canonicalize(root).unwrap_or_else(|_| lexical_root.clone());
                if !key_path.is_absolute() {
                    nfc(&canonical_root.join(key_path).to_string_lossy())
                } else if let Ok(relative) = key_path.strip_prefix(&lexical_root) {
                    nfc(&canonical_root.join(relative).to_string_lossy())
                } else {
                    nfc(&key)
                }
            } else {
                nfc(&key)
            };
            (key, value)
        })
        .collect()
}

/// Save a portable, content-hashed scan manifest without erasing untouched rows.
pub fn save_manifest(
    files: &DetectedFiles,
    path: &Path,
    options: &SaveManifestOptions,
) -> anyhow::Result<()> {
    let existing = load_manifest(path, options.root.as_deref());
    let lexical_root = options.root.as_deref().map(lexical_absolute);
    let root = options
        .root
        .as_deref()
        .map(|root| fs::canonicalize(root).unwrap_or_else(|_| lexical_absolute(root)));
    let path_index = |paths: &BTreeSet<String>| {
        let mut indexed = HashSet::new();
        for path in paths {
            indexed.insert(path.clone());
            indexed.insert(nfc(path));
            if let Ok(resolved) = fs::canonicalize(path) {
                let resolved = resolved.to_string_lossy().into_owned();
                indexed.insert(resolved.clone());
                indexed.insert(nfc(&resolved));
            } else if let (Some(lexical_root), Some(root)) = (&lexical_root, &root)
                && let Ok(relative) = Path::new(path).strip_prefix(lexical_root)
            {
                let rebased = root.join(relative).to_string_lossy().into_owned();
                indexed.insert(rebased.clone());
                indexed.insert(nfc(&rebased));
            }
        }
        indexed
    };
    let scan_set = options.scan_corpus.as_ref().map(&path_index);
    let clear_set = path_index(&options.clear_semantic);
    let in_root = |path: &str| {
        root.as_ref()
            .is_some_and(|root| lexical_absolute(Path::new(path)).starts_with(root))
    };
    let in_set = |path: &str, set: &HashSet<String>| {
        set.contains(path)
            || set.contains(&nfc(path))
            || fs::canonicalize(path)
                .ok()
                .is_some_and(|resolved| set.contains(&resolved.to_string_lossy().into_owned()))
    };

    let mut manifest = BTreeMap::new();
    for (path, value) in &existing {
        let Some(mut entry) = normalize_manifest_entry(value) else {
            continue;
        };
        if !Path::new(path).exists() {
            continue;
        }
        if scan_set
            .as_ref()
            .is_some_and(|set| !in_set(path, set) && in_root(path))
        {
            continue;
        }
        if in_set(path, &clear_set) {
            entry.insert("semantic_hash".into(), Value::String(String::new()));
        }
        manifest.insert(path.clone(), Value::Object(entry));
    }
    for path in files.values().flatten() {
        let Some(mtime) = modified_time(Path::new(path)) else {
            continue;
        };
        let hash = content_md5(Path::new(path));
        let rebased = lexical_root
            .as_ref()
            .zip(root.as_ref())
            .and_then(|(lexical_root, root)| {
                Path::new(path)
                    .strip_prefix(lexical_root)
                    .ok()
                    .map(|relative| nfc(&root.join(relative).to_string_lossy()))
            });
        let previous = existing
            .get(&nfc(path))
            .or_else(|| rebased.as_ref().and_then(|key| existing.get(key)))
            .and_then(normalize_manifest_entry)
            .unwrap_or_default();
        let previous_ast = previous
            .get("ast_hash")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let previous_semantic = previous
            .get("semantic_hash")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let ast = if matches!(options.kind, ManifestKind::Ast | ManifestKind::Both) {
            hash.clone()
        } else {
            previous_ast.to_owned()
        };
        let semantic = if matches!(options.kind, ManifestKind::Semantic | ManifestKind::Both) {
            hash.clone()
        } else if hash == previous_ast {
            previous_semantic.to_owned()
        } else {
            String::new()
        };
        manifest.insert(
            nfc(path),
            serde_json::json!({
                "mtime": mtime,
                "ast_hash": ast,
                "semantic_hash": semantic,
            }),
        );
    }
    let stored: BTreeMap<_, _> = manifest
        .into_iter()
        .map(|(key, value)| (storage_key(&key, options.root.as_deref()), value))
        .collect();
    graphoxide_core::write_json_atomic(path, &stored, true)
}

fn stored_mtime(entry: &Map<String, Value>) -> Option<f64> {
    entry.get("mtime").and_then(|value| {
        value.as_f64().or_else(|| {
            value
                .as_object()
                .and_then(|nested| nested.get("mtime"))
                .and_then(Value::as_f64)
        })
    })
}

/// Compare a full detection against the prior manifest.
pub fn detect_incremental(
    root: &Path,
    manifest_path: &Path,
    options: &DetectOptions,
    kind: ManifestKind,
) -> anyhow::Result<IncrementalResult> {
    let detection = detect(root, options)?;
    let manifest = load_manifest(manifest_path, Some(root));
    let mut new_files: DetectedFiles = detection
        .files
        .keys()
        .map(|key| (key.clone(), Vec::new()))
        .collect();
    let mut unchanged_files = new_files.clone();
    if manifest.is_empty() {
        return Ok(IncrementalResult {
            new_total: detection.total_files,
            new_files: detection.files.clone(),
            unchanged_files,
            detection,
            deleted_files: Vec::new(),
            excluded_files: Vec::new(),
        });
    }
    for (file_type, paths) in &detection.files {
        for path in paths {
            let physical = detection.physical_source(Path::new(path));
            let current_mtime = modified_time(&physical).unwrap_or_default();
            let stored = manifest.get(&nfc(path));
            let changed = if let Some(number) = stored.and_then(Value::as_f64) {
                current_mtime != number
            } else if let Some(entry) = stored.and_then(Value::as_object) {
                let normalized =
                    normalize_manifest_entry(&Value::Object(entry.clone())).unwrap_or_default();
                let hash_key = if kind == ManifestKind::Semantic {
                    "semantic_hash"
                } else {
                    "ast_hash"
                };
                let hash = normalized
                    .get(hash_key)
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                hash.is_empty()
                    || stored_mtime(&normalized).is_none_or(|mtime| {
                        current_mtime != mtime && content_md5(&physical) != hash
                    })
            } else {
                true
            };
            if changed {
                new_files
                    .get_mut(file_type)
                    .expect("new bucket")
                    .push(path.clone());
            } else {
                unchanged_files
                    .get_mut(file_type)
                    .expect("unchanged bucket")
                    .push(path.clone());
            }
        }
    }
    let current: HashSet<_> = detection
        .files
        .values()
        .flatten()
        .map(|path| nfc(path))
        .collect();
    let mut deleted_files = Vec::new();
    let mut excluded_files = Vec::new();
    for path in manifest.keys().filter(|path| !current.contains(&nfc(path))) {
        if Path::new(path).exists() {
            excluded_files.push(path.clone());
        } else {
            deleted_files.push(path.clone());
        }
    }
    deleted_files.sort();
    excluded_files.sort();
    let new_total = new_files.values().map(Vec::len).sum();
    Ok(IncrementalResult {
        detection,
        new_files,
        unchanged_files,
        new_total,
        deleted_files,
        excluded_files,
    })
}

/// Write deterministic Office/Google-Workspace Markdown sidecars.
pub fn convert_office_text(
    source: &Path,
    out_dir: &Path,
    root: Option<&Path>,
    text: &str,
) -> anyhow::Result<Option<PathBuf>> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    let inferred_root = out_dir.parent().and_then(Path::parent);
    let root = root.or(inferred_root);
    let absolute = lexical_absolute(source);
    let key = root
        .and_then(|root| absolute.strip_prefix(lexical_absolute(root)).ok())
        .map_or_else(
            || absolute.to_string_lossy().into_owned(),
            |relative| relative.to_string_lossy().replace('\\', "/"),
        );
    let key = nfc(&key);
    let mut hash = Sha256::new();
    hash.update(key.as_bytes());
    let suffix = hex::encode(hash.finalize());
    let stem = source
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("converted");
    let sidecar = out_dir.join(format!("{stem}_{}.md", &suffix[..8]));
    if sidecar.exists() {
        let source_mtime = modified_time(source);
        let sidecar_mtime = modified_time(&sidecar);
        if source_mtime.is_none()
            || source_mtime
                .zip(sidecar_mtime)
                .is_some_and(|(source, sidecar)| sidecar >= source)
        {
            return Ok(Some(sidecar));
        }
    }
    fs::create_dir_all(out_dir)?;
    fs::write(
        &sidecar,
        format!(
            "<!-- converted from {} -->\n\n{text}",
            source.file_name().unwrap_or_default().to_string_lossy()
        ),
    )?;
    Ok(Some(sidecar))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;
    use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

    fn write_test_zip(path: &Path) {
        let file = fs::File::create(path).expect("create Office ZIP fixture");
        let mut writer = ZipWriter::new(file);
        writer
            .start_file(
                "word/document.xml",
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
            )
            .expect("start Office ZIP member");
        writer
            .write_all(b"<document>opened generation</document>")
            .expect("write Office ZIP member");
        writer.finish().expect("finish Office ZIP fixture");
    }

    #[test]
    fn compiled_cpp_suffixes_are_detectable() {
        for suffix in ["cc", "cpp", "cxx", "hpp", "hh"] {
            assert!(is_supported_path(Path::new(&format!(
                "src/header.{suffix}"
            ))));
        }
    }

    #[test]
    fn markdown_suffixes_reach_the_fallback_tier() {
        for suffix in ["md", "markdown"] {
            assert!(is_supported_path(Path::new(&format!(
                "docs/guide.{suffix}"
            ))));
        }
    }

    #[test]
    fn registered_structured_formats_are_admitted_without_expanding_watch_projection() {
        for suffix in [
            "csv",
            "proto",
            "dot",
            "kicad_sch",
            "ifc",
            "usda",
            "usdz",
            "parquet",
            "zip",
            "avif",
        ] {
            let path = format!("design/input.{suffix}");
            assert!(is_supported_path(Path::new(&path)), "{suffix}");
        }
        for suffix in ["csv", "proto", "dot", "kicad_sch", "ifc", "usda"] {
            assert!(
                !crate::format_registry::format_registry().is_watched_extension(suffix),
                "new dispatcher admission must not change watch projection: {suffix}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn checked_office_handle_is_not_swapped_by_path_replacement() {
        let fixture = tempdir().expect("temporary Office fixture");
        let path = fixture.path().join("document.docx");
        let replacement = fixture.path().join("replacement.docx");
        write_test_zip(&path);
        fs::write(&replacement, b"not a ZIP").expect("write replacement");

        let file = open_source_with_size_cap(&path, OFFICE_MAX_RAW_BYTES)
            .expect("open checked Office generation");
        fs::rename(&replacement, &path).expect("atomically replace Office path");

        let archive = validated_office_zip_from_checked_file(file, OfficeLimits::default())
            .expect("parse the checked Office generation");
        assert_eq!(archive.len(), 1);
    }

    #[test]
    fn checked_office_read_rejects_growth_past_the_raw_cap() {
        let fixture = tempdir().expect("temporary Office fixture");
        let path = fixture.path().join("document.docx");
        write_test_zip(&path);
        let checked_size = fs::metadata(&path).expect("Office metadata").len();
        let file =
            open_source_with_size_cap(&path, checked_size).expect("open checked Office generation");
        let mut writer = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open Office file for growth");
        writer.write_all(b"x").expect("grow Office file");

        let limits = OfficeLimits {
            max_raw_bytes: checked_size,
            ..OfficeLimits::default()
        };
        assert!(validated_office_zip_from_checked_file(file, limits).is_none());
    }

    #[test]
    fn office_preflight_rejects_classic_large_member_count_before_zip_reader() {
        const EOCD_BYTES: usize = 22;
        let directory_offset = 60_000_usize;
        let mut bytes = vec![0_u8; directory_offset + EOCD_BYTES];
        bytes[directory_offset..directory_offset + 4].copy_from_slice(b"PK\x05\x06");
        bytes[directory_offset + 8..directory_offset + 10]
            .copy_from_slice(&50_000_u16.to_le_bytes());
        bytes[directory_offset + 10..directory_offset + 12]
            .copy_from_slice(&50_000_u16.to_le_bytes());
        bytes[directory_offset + 16..directory_offset + 20]
            .copy_from_slice(&(directory_offset as u32).to_le_bytes());

        assert!(!crate::containers::preflight_zip_metadata_with_limits(
            &bytes,
            10_000,
            OFFICE_MAX_CENTRAL_DIRECTORY_BYTES,
        ));
    }

    #[test]
    fn office_preflight_rejects_zip64_large_member_count_before_zip_reader() {
        const ZIP64_RECORD_BYTES: usize = 56;
        const ZIP64_LOCATOR_BYTES: usize = 20;
        const EOCD_BYTES: usize = 22;
        let locator = ZIP64_RECORD_BYTES;
        let eocd = locator + ZIP64_LOCATOR_BYTES;
        let mut bytes = vec![0_u8; eocd + EOCD_BYTES];

        bytes[0..4].copy_from_slice(b"PK\x06\x06");
        bytes[4..12].copy_from_slice(&44_u64.to_le_bytes());
        bytes[24..32].copy_from_slice(&50_000_u64.to_le_bytes());
        bytes[32..40].copy_from_slice(&50_000_u64.to_le_bytes());
        bytes[locator..locator + 4].copy_from_slice(b"PK\x06\x07");
        bytes[locator + 16..locator + 20].copy_from_slice(&1_u32.to_le_bytes());
        bytes[eocd..eocd + 4].copy_from_slice(b"PK\x05\x06");
        bytes[eocd + 8..eocd + 10].copy_from_slice(&u16::MAX.to_le_bytes());
        bytes[eocd + 10..eocd + 12].copy_from_slice(&u16::MAX.to_le_bytes());
        bytes[eocd + 12..eocd + 16].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[eocd + 16..eocd + 20].copy_from_slice(&u32::MAX.to_le_bytes());

        assert!(!crate::containers::preflight_zip_metadata_with_limits(
            &bytes,
            10_000,
            OFFICE_MAX_CENTRAL_DIRECTORY_BYTES,
        ));
    }

    #[test]
    fn checked_pdf_read_rejects_growth_past_the_raw_cap() {
        let fixture = tempdir().expect("temporary PDF fixture");
        let path = fixture.path().join("document.pdf");
        fs::write(&path, b"%PDF").expect("write PDF prefix");
        let file = open_source_with_size_cap(&path, 4).expect("open checked PDF generation");
        let mut writer = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open PDF for growth");
        writer.write_all(b"-").expect("grow PDF");

        assert!(read_checked_source_with_cap(file, 4).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn legacy_office_admission_does_not_follow_the_final_symlink() {
        use std::os::unix::fs::symlink;

        let fixture = tempdir().expect("temporary Office fixture");
        let target = fixture.path().join("target.docx");
        let alias = fixture.path().join("alias.docx");
        write_test_zip(&target);
        symlink(&target, &alias).expect("create Office symlink");

        assert!(!file_within_size_cap(&alias, OFFICE_MAX_RAW_BYTES));
        assert!(!zip_within_caps(&alias));
    }
}
