//! File discovery, classification, ignore handling, and portable scan manifests.
//!
//! This module intentionally owns the complete pre-extraction boundary.  A file
//! that disappears here can never appear in the graph, so unsupported,
//! sensitive, ignored, and unreadable paths are all reported explicitly.

pub use crate::format_registry::FileType;
use crate::{cache::AST_CACHE_VERSION, format_registry::format_registry};
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

/// Maximum bytes read from any one ignore-policy source.
///
/// Sources over this limit are rejected as a whole so a missing suffix (for
/// example, a later negation rule) can never be mistaken for complete policy.
pub const MAX_IGNORE_SOURCE_BYTES: usize = 1024 * 1024;

/// Maximum parsed rules accepted from any one ignore-policy source.
pub const MAX_IGNORE_PATTERNS_PER_SOURCE: usize = 10_000;

/// Maximum bytes accepted in one parsed ignore rule.
pub const MAX_IGNORE_PATTERN_BYTES: usize = 4 * 1024;

/// Maximum slash-delimited components accepted in one parsed ignore rule.
pub const MAX_IGNORE_PATTERN_SEGMENTS: usize = 128;

/// Maximum bytes in an ignore rule's absolute anchor. Admission charges this
/// fixed ceiling for every retained rule so clone-root length cannot change
/// which otherwise-identical policy is accepted.
pub const MAX_IGNORE_ANCHOR_BYTES: usize = 4 * 1024;

/// Maximum ignore rules retained across one discovery or coverage scan.
pub const MAX_IGNORE_PATTERNS: usize = 20_000;

/// Maximum aggregate bytes retained by ignore rule strings and their anchors.
pub const MAX_IGNORE_RETAINED_BYTES: usize = 64 * 1024 * 1024;

const MAX_RETAINED_IGNORE_DIAGNOSTICS: usize = 128;
const MAX_GIT_CONTROL_BYTES: usize = 64 * 1024;

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

#[derive(Debug, Default)]
pub(crate) struct IgnoreLoadResult {
    pub patterns: Vec<IgnorePattern>,
    pub diagnostics: Vec<String>,
    pub truncated_sources: usize,
    pub retained_bytes: usize,
}

impl IgnoreLoadResult {
    fn record_truncation(&mut self, diagnostic: String) {
        self.truncated_sources = self.truncated_sources.saturating_add(1);
        if self.diagnostics.len() < MAX_RETAINED_IGNORE_DIAGNOSTICS {
            self.diagnostics.push(diagnostic);
        }
    }

    pub(crate) fn merge(&mut self, mut other: Self) {
        self.patterns.append(&mut other.patterns);
        self.retained_bytes = self.retained_bytes.saturating_add(other.retained_bytes);
        self.truncated_sources = self
            .truncated_sources
            .saturating_add(other.truncated_sources);
        let remaining = MAX_RETAINED_IGNORE_DIAGNOSTICS.saturating_sub(self.diagnostics.len());
        self.diagnostics
            .extend(other.diagnostics.into_iter().take(remaining));
    }
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

fn open_ignore_source_nofollow(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // `O_NONBLOCK` ensures a regular-file-to-FIFO race cannot stall the
        // control plane between metadata validation and the no-follow open.
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "ignore source is not a regular file",
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

/// Whether entering a directory would cross a credential-store boundary.
pub(crate) fn is_sensitive_directory(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            Component::Normal(value)
                if value
                    .to_str()
                    .is_some_and(|value| [".ssh", ".gnupg", ".aws", ".gcloud"]
                        .iter()
                        .any(|sensitive| value.eq_ignore_ascii_case(sensitive)))
        )
    })
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

fn ignore_pattern_within_limits(pattern: &str) -> bool {
    if pattern.len() > MAX_IGNORE_PATTERN_BYTES {
        return false;
    }
    let raw = pattern.strip_prefix('!').unwrap_or(pattern);
    raw.trim_matches('/').split('/').count() <= MAX_IGNORE_PATTERN_SEGMENTS
}

fn retained_ignore_pattern_bytes(anchor: &Path, pattern: &str) -> Option<usize> {
    (anchor.as_os_str().len() <= MAX_IGNORE_ANCHOR_BYTES).then(|| {
        MAX_IGNORE_ANCHOR_BYTES
            .saturating_add(std::mem::size_of::<IgnorePattern>())
            .saturating_add(pattern.len())
    })
}

fn read_ignore_file(
    path: &Path,
    anchor: &Path,
    remaining_patterns: usize,
    remaining_bytes: usize,
) -> IgnoreLoadResult {
    if anchor.as_os_str().len() > MAX_IGNORE_ANCHOR_BYTES {
        let mut result = IgnoreLoadResult::default();
        result.record_truncation(format!(
            "{}: ignore source anchor exceeds the {}-byte portable limit; no rules from this source were applied",
            path.display(),
            MAX_IGNORE_ANCHOR_BYTES
        ));
        return result;
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return IgnoreLoadResult::default();
        }
        Err(_) => {
            let mut result = IgnoreLoadResult::default();
            result.record_truncation(format!(
                "{}: ignore source metadata could not be read safely; no rules from this source were applied",
                path.display()
            ));
            return result;
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        let mut result = IgnoreLoadResult::default();
        result.record_truncation(format!(
            "{}: ignore source is not a regular non-symlink file; no rules from this source were applied",
            path.display()
        ));
        return result;
    }
    let Ok(file) = open_ignore_source_nofollow(path) else {
        let mut result = IgnoreLoadResult::default();
        result.record_truncation(format!(
            "{}: ignore source could not be opened safely; no rules from this source were applied",
            path.display()
        ));
        return result;
    };
    let mut bytes = Vec::new();
    let byte_limit = u64::try_from(MAX_IGNORE_SOURCE_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    if file.take(byte_limit).read_to_end(&mut bytes).is_err() {
        let mut result = IgnoreLoadResult::default();
        result.record_truncation(format!(
            "{}: ignore source could not be read safely; no rules from this source were applied",
            path.display()
        ));
        return result;
    }
    if bytes.len() > MAX_IGNORE_SOURCE_BYTES {
        let mut result = IgnoreLoadResult::default();
        result.record_truncation(format!(
            "{}: ignore source exceeds the {}-byte limit; no rules from this source were applied",
            path.display(),
            MAX_IGNORE_SOURCE_BYTES
        ));
        return result;
    }

    let content = String::from_utf8_lossy(&bytes);
    let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
    let mut patterns = Vec::new();
    let mut retained_bytes = 0usize;
    for pattern in content.lines().filter_map(parse_ignore_line) {
        if !ignore_pattern_within_limits(&pattern) {
            let mut result = IgnoreLoadResult::default();
            result.record_truncation(format!(
                "{}: ignore source contains a rule exceeding the {}-byte or {}-segment limit; no rules from this source were applied",
                path.display(),
                MAX_IGNORE_PATTERN_BYTES,
                MAX_IGNORE_PATTERN_SEGMENTS
            ));
            return result;
        }
        if patterns.len() >= MAX_IGNORE_PATTERNS_PER_SOURCE {
            let mut result = IgnoreLoadResult::default();
            result.record_truncation(format!(
                "{}: ignore source exceeds the {}-pattern source limit; no rules from this source were applied",
                path.display(),
                MAX_IGNORE_PATTERNS_PER_SOURCE
            ));
            return result;
        }
        retained_bytes = retained_bytes.saturating_add(
            retained_ignore_pattern_bytes(anchor, &pattern)
                .expect("validated ignore anchor must have a deterministic charge"),
        );
        if retained_bytes > remaining_bytes {
            let mut result = IgnoreLoadResult::default();
            result.record_truncation(format!(
                "{}: ignore source exceeds the remaining scan-wide retained-byte budget of {}; no rules from this source were applied",
                path.display(),
                remaining_bytes
            ));
            return result;
        }
        patterns.push(IgnorePattern {
            anchor: anchor.to_path_buf(),
            pattern,
        });
    }
    if patterns.len() > remaining_patterns {
        let mut result = IgnoreLoadResult::default();
        result.record_truncation(format!(
            "{}: ignore source exceeds the remaining scan-wide pattern budget of {}; no rules from this source were applied",
            path.display(),
            remaining_patterns
        ));
        return result;
    }
    IgnoreLoadResult {
        patterns,
        retained_bytes,
        ..IgnoreLoadResult::default()
    }
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
    fs::symlink_metadata(directory.join(".git")).is_ok()
        || [".hg", ".svn", "_darcs", ".fossil"]
            .iter()
            .any(|marker| directory.join(marker).exists())
}

fn read_git_control_file(path: &Path) -> std::io::Result<Option<String>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Git control path is not a regular non-symlink file",
        ));
    }
    let mut file = open_ignore_source_nofollow(path)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(
            u64::try_from(MAX_GIT_CONTROL_BYTES)
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        )
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_GIT_CONTROL_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Git control file exceeds its byte limit",
        ));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid UTF-8"))
}

fn git_info_exclude(vcs_root: &Path) -> Result<Option<PathBuf>, String> {
    let dot_git = vcs_root.join(".git");
    let metadata = match fs::symlink_metadata(&dot_git) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(format!(
                "{}: Git control metadata could not be read safely",
                dot_git.display()
            ));
        }
    };
    let mut git_dir = if metadata.is_dir() && !metadata.file_type().is_symlink() {
        dot_git.clone()
    } else if metadata.is_file() && !metadata.file_type().is_symlink() {
        let value = read_git_control_file(&dot_git)
            .map_err(|_| {
                format!(
                    "{}: Git pointer could not be read safely",
                    dot_git.display()
                )
            })?
            .ok_or_else(|| format!("{}: Git pointer disappeared", dot_git.display()))?;
        let value = value
            .trim()
            .strip_prefix("gitdir:")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("{}: Git pointer is malformed", dot_git.display()))?;
        let value = PathBuf::from(value);
        if value.is_absolute() {
            value
        } else {
            vcs_root.join(value)
        }
    } else {
        return Err(format!(
            "{}: Git control path is not a regular file or directory",
            dot_git.display()
        ));
    };
    let commondir_path = git_dir.join("commondir");
    match read_git_control_file(&commondir_path) {
        Ok(Some(common)) => {
            let common = common.trim();
            if common.is_empty() {
                return Err(format!(
                    "{}: Git commondir pointer is empty",
                    commondir_path.display()
                ));
            }
            let common = PathBuf::from(common);
            git_dir = if common.is_absolute() {
                common
            } else {
                git_dir.join(common)
            };
        }
        Ok(None) => {}
        Err(_) => {
            return Err(format!(
                "{}: Git commondir pointer could not be read safely",
                commondir_path.display()
            ));
        }
    }
    let path = git_dir.join("info/exclude");
    match fs::symlink_metadata(&path) {
        Ok(_) => Ok(Some(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(format!(
            "{}: Git info/exclude metadata could not be read safely",
            path.display()
        )),
    }
}

pub(crate) fn load_dir_ignore(
    directory: &Path,
    honor_gitignore: bool,
    max_patterns: usize,
    max_bytes: usize,
) -> IgnoreLoadResult {
    let mut result = IgnoreLoadResult::default();
    if honor_gitignore {
        let remaining = max_patterns.saturating_sub(result.patterns.len());
        result.merge(read_ignore_file(
            &directory.join(".gitignore"),
            directory,
            remaining,
            max_bytes.saturating_sub(result.retained_bytes),
        ));
    }
    let remaining = max_patterns.saturating_sub(result.patterns.len());
    result.merge(read_ignore_file(
        &directory.join(".graphifyignore"),
        directory,
        remaining,
        max_bytes.saturating_sub(result.retained_bytes),
    ));
    let remaining = max_patterns.saturating_sub(result.patterns.len());
    result.merge(read_ignore_file(
        &directory.join(".graphoxideignore"),
        directory,
        remaining,
        max_bytes.saturating_sub(result.retained_bytes),
    ));
    result
}

/// Load bounded ignore rules from the VCS ceiling down to the scan root.
pub(crate) fn load_ignore_patterns_bounded(root: &Path, honor_gitignore: bool) -> IgnoreLoadResult {
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
    let mut result = IgnoreLoadResult::default();
    if honor_gitignore {
        match git_info_exclude(&ceiling) {
            Ok(Some(exclude)) => result.merge(read_ignore_file(
                &exclude,
                &ceiling,
                MAX_IGNORE_PATTERNS,
                MAX_IGNORE_RETAINED_BYTES,
            )),
            Ok(None) => {}
            Err(diagnostic) => result.record_truncation(format!(
                "{diagnostic}; ignore policy is incomplete and no project files were scanned"
            )),
        }
    }
    for directory in directories {
        let remaining = MAX_IGNORE_PATTERNS.saturating_sub(result.patterns.len());
        let remaining_bytes = MAX_IGNORE_RETAINED_BYTES.saturating_sub(result.retained_bytes);
        result.merge(load_dir_ignore(
            &directory,
            honor_gitignore,
            remaining,
            remaining_bytes,
        ));
    }
    result
}

pub(crate) fn load_extra_ignore_patterns(
    root: &Path,
    raw_patterns: &[String],
    max_patterns: usize,
    max_bytes: usize,
) -> IgnoreLoadResult {
    let mut result = IgnoreLoadResult::default();
    for (index, raw) in raw_patterns.iter().enumerate() {
        if raw.len() > MAX_IGNORE_SOURCE_BYTES {
            result.record_truncation(format!(
                "extra exclude #{} exceeds the {}-byte ignore-source limit; the rule was not applied",
                index + 1,
                MAX_IGNORE_SOURCE_BYTES
            ));
            continue;
        }
        let Some(pattern) = parse_ignore_line(raw) else {
            continue;
        };
        if !ignore_pattern_within_limits(&pattern) {
            result.record_truncation(format!(
                "extra exclude #{} exceeds the {}-byte or {}-segment ignore-rule limit; the rule was not applied",
                index + 1,
                MAX_IGNORE_PATTERN_BYTES,
                MAX_IGNORE_PATTERN_SEGMENTS
            ));
            continue;
        }
        if result.patterns.len() >= max_patterns {
            result.record_truncation(format!(
                "extra exclude #{} exceeds the remaining scan-wide ignore-pattern budget; the rule was not applied",
                index + 1
            ));
            continue;
        }
        let Some(retained_bytes) = retained_ignore_pattern_bytes(root, &pattern) else {
            result.record_truncation(format!(
                "extra excludes use an anchor exceeding the {}-byte portable limit; the rule was not applied",
                MAX_IGNORE_ANCHOR_BYTES
            ));
            continue;
        };
        if retained_bytes > max_bytes.saturating_sub(result.retained_bytes) {
            result.record_truncation(format!(
                "extra exclude #{} exceeds the remaining scan-wide retained ignore-pattern byte budget; the rule was not applied",
                index + 1
            ));
            continue;
        }
        result.retained_bytes = result.retained_bytes.saturating_add(retained_bytes);
        result.patterns.push(IgnorePattern {
            anchor: root.to_path_buf(),
            pattern,
        });
    }
    result
}

/// Load ignore rules from the VCS ceiling down to the scan root.
///
/// Rules are bounded by the documented per-source and scan-wide ceilings.
/// Discovery and coverage use the diagnostic-bearing internal form so a
/// rejected source cannot be mistaken for a complete policy load. Legacy
/// callers receive a match-all rule when a source is rejected, which safely
/// suppresses their secondary traversal instead of applying a partial prefix.
pub fn load_ignore_patterns(root: &Path, honor_gitignore: bool) -> Vec<IgnorePattern> {
    let loaded = load_ignore_patterns_bounded(root, honor_gitignore);
    if loaded.truncated_sources == 0 {
        return loaded.patterns;
    }
    vec![IgnorePattern {
        anchor: fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()),
        pattern: "**".to_owned(),
    }]
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ComponentGlobToken {
    Star,
    Any,
    Literal(char),
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
}

fn component_glob_tokens(pattern: &str) -> Option<Vec<ComponentGlobToken>> {
    if pattern.len() > MAX_IGNORE_PATTERN_BYTES {
        return None;
    }
    let characters = pattern.chars().collect::<Vec<_>>();
    let mut tokens = Vec::with_capacity(characters.len());
    let mut index = 0usize;
    while index < characters.len() {
        match characters[index] {
            '*' => {
                if !matches!(tokens.last(), Some(ComponentGlobToken::Star)) {
                    tokens.push(ComponentGlobToken::Star);
                }
                index += 1;
            }
            '?' => {
                tokens.push(ComponentGlobToken::Any);
                index += 1;
            }
            '[' => {
                let mut cursor = index + 1;
                let negated = characters.get(cursor) == Some(&'!');
                if negated {
                    cursor += 1;
                }
                let class_start = cursor;
                while cursor < characters.len() && characters[cursor] != ']' {
                    cursor += 1;
                }
                if cursor == characters.len() || cursor == class_start {
                    return None;
                }
                let class = &characters[class_start..cursor];
                let mut ranges = Vec::new();
                let mut class_index = 0usize;
                while class_index < class.len() {
                    if class_index + 2 < class.len() && class[class_index + 1] == '-' {
                        let start = class[class_index];
                        let end = class[class_index + 2];
                        if start > end {
                            return None;
                        }
                        ranges.push((start, end));
                        class_index += 3;
                    } else {
                        let value = class[class_index];
                        ranges.push((value, value));
                        class_index += 1;
                    }
                }
                tokens.push(ComponentGlobToken::Class { negated, ranges });
                index = cursor + 1;
            }
            literal => {
                tokens.push(ComponentGlobToken::Literal(literal));
                index += 1;
            }
        }
    }
    Some(tokens)
}

fn component_token_matches(token: &ComponentGlobToken, value: char) -> bool {
    match token {
        ComponentGlobToken::Any => true,
        ComponentGlobToken::Literal(expected) => *expected == value,
        ComponentGlobToken::Class { negated, ranges } => {
            let contained = ranges
                .iter()
                .any(|(start, end)| *start <= value && value <= *end);
            contained != *negated
        }
        ComponentGlobToken::Star => false,
    }
}

fn component_glob(pattern: &str, value: &str) -> bool {
    let Some(tokens) = component_glob_tokens(pattern) else {
        return false;
    };
    let mut previous = vec![false; tokens.len() + 1];
    previous[0] = true;
    for (index, token) in tokens.iter().enumerate() {
        if matches!(token, ComponentGlobToken::Star) {
            previous[index + 1] = previous[index];
        } else {
            break;
        }
    }
    for character in value.chars() {
        let mut current = vec![false; tokens.len() + 1];
        for (index, token) in tokens.iter().enumerate() {
            current[index + 1] = match token {
                ComponentGlobToken::Star => current[index] || previous[index + 1],
                _ => previous[index] && component_token_matches(token, character),
            };
        }
        previous = current;
    }
    previous[tokens.len()]
}

fn anchored_glob(path: &str, pattern: &str) -> bool {
    if !ignore_pattern_within_limits(pattern) {
        return false;
    }
    let path = path.split('/').collect::<Vec<_>>();
    let pattern = pattern.split('/').collect::<Vec<_>>();
    let mut previous = vec![false; pattern.len() + 1];
    previous[0] = true;
    for index in 0..pattern.len() {
        if pattern[index] == "**" && index + 1 < pattern.len() {
            previous[index + 1] = previous[index];
        } else {
            break;
        }
    }
    for component in path {
        let mut current = vec![false; pattern.len() + 1];
        for index in 0..pattern.len() {
            current[index + 1] = if pattern[index] == "**" {
                if index + 1 == pattern.len() {
                    previous[index + 1] || previous[index]
                } else {
                    current[index] || previous[index + 1]
                }
            } else {
                previous[index] && component_glob(pattern[index], component)
            };
        }
        previous = current;
    }
    previous[pattern.len()]
}

fn eval_ignore(target: &Path, patterns: &[IgnorePattern]) -> bool {
    let mut result = false;
    for entry in patterns {
        if !ignore_pattern_within_limits(&entry.pattern) {
            return true;
        }
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

fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(windows))]
    false
}

/// Resolve the separately indexed managed-memory tree without crossing a
/// symlink/reparse boundary or leaving the canonical source root.
pub(crate) fn managed_memory_directory(root: &Path, configured_output: &Path) -> Option<PathBuf> {
    let root = fs::canonicalize(root).ok()?;
    let memory = configured_output.join("memory");
    let metadata = fs::symlink_metadata(&memory).ok()?;
    if !metadata.is_dir() || metadata_is_reparse_point(&metadata) {
        return None;
    }
    let resolved = fs::canonicalize(&memory).ok()?;
    if !resolved.starts_with(&root) {
        return None;
    }
    let resolved_metadata = fs::symlink_metadata(&resolved).ok()?;
    (resolved_metadata.is_dir() && !metadata_is_reparse_point(&resolved_metadata))
        .then_some(resolved)
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
    patterns_retained_bytes: usize,
    paths: Vec<DiscoveredPath>,
    ignored: Vec<String>,
    pruned_noise: Vec<String>,
    skipped_sensitive: Vec<String>,
    errors: Vec<String>,
    ignore_diagnostics_retained: usize,
    fatal_ignore_error: Option<String>,
    active_targets: HashSet<PathBuf>,
    ignore_cache: HashMap<PathBuf, bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiscoveredPath {
    logical: PathBuf,
    physical: PathBuf,
}

impl WalkState<'_> {
    fn extend_ignore_policy(&mut self, mut loaded: IgnoreLoadResult) -> Option<String> {
        let truncated = loaded.truncated_sources > 0;
        let fatal = truncated.then(|| {
            loaded.diagnostics.first().cloned().unwrap_or_else(|| {
                "ignore policy source was rejected without a retained diagnostic".to_owned()
            })
        });
        self.patterns.append(&mut loaded.patterns);
        self.patterns_retained_bytes = self
            .patterns_retained_bytes
            .saturating_add(loaded.retained_bytes);
        let remaining =
            MAX_RETAINED_IGNORE_DIAGNOSTICS.saturating_sub(self.ignore_diagnostics_retained);
        let retained = loaded.diagnostics.len().min(remaining);
        self.errors
            .extend(loaded.diagnostics.into_iter().take(retained));
        self.ignore_diagnostics_retained =
            self.ignore_diagnostics_retained.saturating_add(retained);
        fatal
    }

    fn walk(&mut self, directory: &Path, memory_tree: bool) {
        if self.fatal_ignore_error.is_some() {
            return;
        }
        let target = fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
        if !self.active_targets.insert(target.clone()) {
            return;
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
        if !memory_tree && directory != self.root {
            let remaining = MAX_IGNORE_PATTERNS.saturating_sub(self.patterns.len());
            if let Some(diagnostic) = self.extend_ignore_policy(load_dir_ignore(
                directory,
                self.options.honor_gitignore,
                remaining,
                MAX_IGNORE_RETAINED_BYTES.saturating_sub(self.patterns_retained_bytes),
            )) {
                self.fatal_ignore_error = Some(diagnostic);
                self.active_targets.remove(&target);
                return;
            }
        }
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
            if self.fatal_ignore_error.is_some() {
                break;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else {
                self.errors
                    .push(format!("{}: unable to inspect file type", path.display()));
                continue;
            };
            if memory_tree {
                match fs::symlink_metadata(&path) {
                    Ok(metadata) if !metadata_is_reparse_point(&metadata) => {}
                    Ok(_) => {
                        self.skipped_sensitive.push(format!(
                            "{} [managed memory symlink or reparse point]",
                            path.display()
                        ));
                        continue;
                    }
                    Err(error) => {
                        self.errors.push(format!("{}: {error}", path.display()));
                        continue;
                    }
                }
            }
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
                if path
                    .strip_prefix(self.root)
                    .unwrap_or(&path)
                    .components()
                    .any(|component| {
                        matches!(component, Component::Normal(value) if value.to_str().is_none())
                    })
                {
                    self.skipped_sensitive.push(format!(
                        "{} [non-Unicode directory boundary]",
                        path.display()
                    ));
                    continue;
                }
                if is_sensitive_directory(&path) {
                    self.skipped_sensitive
                        .push(format!("{} [sensitive directory]", path.display()));
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
                        if fs::canonicalize(&path)
                            .ok()
                            .is_some_and(|target| is_sensitive_directory(&target))
                        {
                            self.skipped_sensitive
                                .push(format!("{} [sensitive symlink target]", path.display()));
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
    let mut ignore_policy = load_ignore_patterns_bounded(&root, options.honor_gitignore);
    let remaining = MAX_IGNORE_PATTERNS.saturating_sub(ignore_policy.patterns.len());
    let remaining_bytes = MAX_IGNORE_RETAINED_BYTES.saturating_sub(ignore_policy.retained_bytes);
    ignore_policy.merge(load_extra_ignore_patterns(
        &root,
        &options.extra_excludes,
        remaining,
        remaining_bytes,
    ));
    if ignore_policy.truncated_sources > 0 {
        let diagnostic = ignore_policy.diagnostics.first().map_or(
            "ignore policy source was rejected without a retained diagnostic",
            String::as_str,
        );
        anyhow::bail!("ignore policy could not be loaded safely: {diagnostic}");
    }
    let ignore_diagnostics_retained = ignore_policy.diagnostics.len();
    let memory = managed_memory_directory(&root, &configured_output);
    let mut state = WalkState {
        root: &root,
        options,
        configured_output: configured_output.clone(),
        patterns: ignore_policy.patterns,
        patterns_retained_bytes: ignore_policy.retained_bytes,
        paths: Vec::new(),
        ignored: Vec::new(),
        pruned_noise: Vec::new(),
        skipped_sensitive: Vec::new(),
        errors: ignore_policy.diagnostics,
        ignore_diagnostics_retained,
        fatal_ignore_error: None,
        active_targets: HashSet::new(),
        ignore_cache: HashMap::new(),
    };
    state.walk(&root, false);
    if state.fatal_ignore_error.is_none()
        && let Some(memory) = &memory
    {
        state.walk(memory, true);
    }
    if let Some(diagnostic) = &state.fatal_ignore_error {
        anyhow::bail!("ignore policy could not be loaded safely: {diagnostic}");
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
        let in_memory = memory
            .as_ref()
            .is_some_and(|memory| path.starts_with(memory));
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
            ("ast_version".into(), Value::from(0)),
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
    entry.entry("ast_version").or_insert_with(|| Value::from(0));
    Some(entry)
}

fn manifest_ast_version(entry: &Map<String, Value>) -> u32 {
    entry
        .get("ast_version")
        .and_then(Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .unwrap_or_default()
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
        let previous_ast_version = manifest_ast_version(&previous);
        let previous_semantic = previous
            .get("semantic_hash")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let writes_ast = matches!(options.kind, ManifestKind::Ast | ManifestKind::Both);
        let ast = if writes_ast {
            hash.clone()
        } else {
            previous_ast.to_owned()
        };
        let ast_version = if writes_ast {
            AST_CACHE_VERSION
        } else if hash == previous_ast {
            previous_ast_version
        } else {
            // A semantic-only pass has observed bytes that do not match the
            // retained AST hash. Keep the old hash for compatibility, but
            // invalidate its schema marker so an AST pass cannot skip it.
            0
        };
        let semantic = if matches!(options.kind, ManifestKind::Semantic | ManifestKind::Both) {
            hash.clone()
        } else if hash == previous_ast && previous_ast_version == AST_CACHE_VERSION {
            previous_semantic.to_owned()
        } else {
            String::new()
        };
        manifest.insert(
            nfc(path),
            serde_json::json!({
                "mtime": mtime,
                "ast_version": ast_version,
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
                kind != ManifestKind::Semantic || current_mtime != number
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
                let stale_ast_schema = kind != ManifestKind::Semantic
                    && manifest_ast_version(&normalized) != AST_CACHE_VERSION;
                stale_ast_schema
                    || hash.is_empty()
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
    fn retained_ignore_byte_budget_rejects_whole_large_sources() {
        let fixture = tempdir().expect("temporary ignore fixture");
        let anchor = fixture.path();
        let mut loaded = IgnoreLoadResult::default();
        let byte_budget = 12usize * 1024;
        for index in 0..64 {
            let path = anchor.join(format!("ignore-{index}"));
            let pattern = format!("{}-{index}\n", "x".repeat(2 * 1024));
            fs::write(&path, pattern).expect("write large one-rule source");
            let remaining_patterns = 64usize.saturating_sub(loaded.patterns.len());
            let remaining_bytes = byte_budget.saturating_sub(loaded.retained_bytes);
            loaded.merge(read_ignore_file(
                &path,
                anchor,
                remaining_patterns,
                remaining_bytes,
            ));
        }

        assert!(loaded.retained_bytes <= byte_budget);
        assert!(loaded.patterns.len() < 64);
        assert!(loaded.truncated_sources > 0);
        assert!(loaded
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("retained-byte budget")));
    }

    #[test]
    fn retained_ignore_admission_is_independent_of_clone_root_length() {
        let fixture = tempdir().expect("temporary clone-root fixture");
        let short = fixture.path().join("a");
        let long = fixture.path().join("a-very-much-longer-checkout-directory");
        fs::create_dir_all(&short).expect("create short checkout");
        fs::create_dir_all(&long).expect("create long checkout");
        let rules = (0..100)
            .map(|index| format!("generated-{index}\n"))
            .collect::<String>();
        fs::write(short.join(".graphoxideignore"), &rules).expect("write short policy");
        fs::write(long.join(".graphoxideignore"), &rules).expect("write long policy");

        let short = load_ignore_patterns_bounded(&short, true);
        let long = load_ignore_patterns_bounded(&long, true);

        assert_eq!(short.truncated_sources, 0);
        assert_eq!(long.truncated_sources, 0);
        assert_eq!(short.patterns.len(), long.patterns.len());
        assert_eq!(short.retained_bytes, long.retained_bytes);
    }

    #[test]
    fn git_control_pointers_are_bounded_and_fail_closed() {
        let fixture = tempdir().expect("temporary Git-control fixture");
        let project = fixture.path().join("project");
        fs::create_dir_all(&project).expect("create project");
        fs::write(project.join(".git"), vec![b'x'; MAX_GIT_CONTROL_BYTES + 1])
            .expect("write oversized Git pointer");

        let oversized = load_ignore_patterns_bounded(&project, true);
        assert_eq!(oversized.truncated_sources, 1);
        assert!(oversized
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("Git pointer could not be read safely")));

        let worktree = fixture.path().join("worktree");
        let git_dir = fixture.path().join("git-data");
        fs::create_dir_all(&worktree).expect("create worktree");
        fs::create_dir_all(&git_dir).expect("create Git directory");
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .expect("write Git pointer");
        fs::write(
            git_dir.join("commondir"),
            vec![b'x'; MAX_GIT_CONTROL_BYTES + 1],
        )
        .expect("write oversized commondir pointer");

        let commondir = load_ignore_patterns_bounded(&worktree, true);
        assert_eq!(commondir.truncated_sources, 1);
        assert!(commondir
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("commondir pointer could not be read safely")));
    }

    #[cfg(unix)]
    #[test]
    fn git_info_exclude_metadata_errors_abort_detection() {
        let fixture = tempdir().expect("temporary Git-info fixture");
        let project = fixture.path().join("project");
        let git_dir = project.join(".git");
        fs::create_dir_all(&git_dir).expect("create Git directory");
        fs::write(project.join("source.rs"), "fn main() {}\n").expect("write project source");
        std::os::unix::fs::symlink("info", git_dir.join("info"))
            .expect("create self-referential info path");

        let loaded = load_ignore_patterns_bounded(&project, true);
        assert_eq!(loaded.truncated_sources, 1);
        assert!(loaded
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("Git info/exclude metadata")));

        let error = detect(&project, &DetectOptions::default())
            .expect_err("unsafe Git ignore metadata must abort detection")
            .to_string();
        assert!(error.contains("ignore policy could not be loaded safely"));
        assert!(error.contains("Git info/exclude metadata"));
    }

    #[test]
    fn adversarial_double_star_matching_is_iterative_and_rule_bounded() {
        let mut segments = Vec::new();
        for _ in 0..60 {
            segments.push("**");
            segments.push("a");
        }
        segments.push("target[0-9]?.rs");
        let pattern = segments.join("/");
        let matching_path = format!("{}/target7x.rs", vec!["a"; 60].join("/"));
        let nonmatching_path = format!("{}/never.rs", vec!["a"; 120].join("/"));

        assert!(ignore_pattern_within_limits(&pattern));
        assert!(anchored_glob(&matching_path, &pattern));
        assert!(!anchored_glob(&nonmatching_path, &pattern));

        let over_segment_limit = format!("{}/target.rs", vec!["**"; 129].join("/"));
        assert!(!ignore_pattern_within_limits(&over_segment_limit));
        assert!(is_ignored(
            Path::new("/repo/ordinary.rs"),
            Path::new("/repo"),
            &[IgnorePattern {
                anchor: PathBuf::from("/repo"),
                pattern: over_segment_limit,
            }]
        ));
    }

    #[cfg(unix)]
    #[test]
    fn ignore_sources_reject_symlinks_fifos_and_unreadable_files() {
        use std::{ffi::CString, os::unix::ffi::OsStrExt as _, os::unix::fs::PermissionsExt as _};

        let fixture = tempdir().expect("temporary unsafe-ignore fixture");
        let outside = tempdir().expect("outside ignore fixture");
        let target = outside.path().join("outside-ignore");
        fs::write(&target, "private.rs\n").expect("write outside ignore");
        let alias = fixture.path().join("symlink-ignore");
        std::os::unix::fs::symlink(&target, &alias).expect("create ignore symlink");
        let symlink_result = read_ignore_file(
            &alias,
            fixture.path(),
            MAX_IGNORE_PATTERNS,
            MAX_IGNORE_RETAINED_BYTES,
        );
        assert!(symlink_result.patterns.is_empty());
        assert_eq!(symlink_result.truncated_sources, 1);

        let fifo = fixture.path().join("fifo-ignore");
        let fifo_path = CString::new(fifo.as_os_str().as_bytes()).expect("FIFO path");
        // SAFETY: `fifo_path` is a valid NUL-terminated pathname owned for the
        // duration of the call.
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        let fifo_result = read_ignore_file(
            &fifo,
            fixture.path(),
            MAX_IGNORE_PATTERNS,
            MAX_IGNORE_RETAINED_BYTES,
        );
        assert!(fifo_result.patterns.is_empty());
        assert_eq!(fifo_result.truncated_sources, 1);

        let unreadable = fixture.path().join("unreadable-ignore");
        fs::write(&unreadable, "private.rs\n").expect("write unreadable ignore");
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000))
            .expect("lock ignore source");
        let runner_cannot_open = open_source_nofollow(&unreadable).is_err();
        let unreadable_result = read_ignore_file(
            &unreadable,
            fixture.path(),
            MAX_IGNORE_PATTERNS,
            MAX_IGNORE_RETAINED_BYTES,
        );
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o600))
            .expect("restore ignore source");
        if runner_cannot_open {
            assert!(unreadable_result.patterns.is_empty());
            assert_eq!(unreadable_result.truncated_sources, 1);
        }

        let project = fixture.path().join("symlinked-git-project");
        fs::create_dir(&project).expect("create symlinked Git project");
        std::os::unix::fs::symlink(outside.path(), project.join(".git"))
            .expect("create Git control symlink");
        let git_result = load_ignore_patterns_bounded(&project, true);
        assert_eq!(git_result.truncated_sources, 1);
        assert!(git_result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("Git control path")));
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
