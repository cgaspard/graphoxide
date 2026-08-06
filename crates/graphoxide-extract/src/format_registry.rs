//! Canonical format capability registry.
//!
//! The detector and watcher have compatibility-sensitive extension behavior.
//! This module keeps that behavior as an explicit *legacy admission* projection
//! while also describing every structured family handled by the byte-only
//! adapter. Registering a format never changes the legacy detector or watcher
//! projection; the isolated executor intentionally uses the registry to admit
//! explicitly bounded byte adapters and inventory records.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Extraction tier assigned to files admitted by the legacy detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileType {
    Code,
    Document,
    Paper,
    Image,
    Video,
}

impl FileType {
    pub const ALL: [Self; 5] = [
        Self::Code,
        Self::Document,
        Self::Paper,
        Self::Image,
        Self::Video,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Document => "document",
            Self::Paper => "paper",
            Self::Image => "image",
            Self::Video => "video",
        }
    }
}

/// Stable identifier for a registered format family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FormatId(&'static str);

impl FormatId {
    pub const fn new(value: &'static str) -> Self {
        Self(value)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

/// The strongest truthful result an adapter presently provides.
///
/// These states are intentionally not ordered: a safe container inventory is
/// not a weaker version of schema extraction, and callers must not infer facts
/// that the registered adapter does not emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormatCapability {
    SemanticFull,
    /// Bounded structural facts are available, but the adapter does not fully
    /// validate or decode every value permitted by the representation.
    StructuralPartial,
    SchemaFull,
    ContainerFull,
    InventoryOnly,
}

/// The bounded byte-only implementation selected for a registered family.
///
/// This is deliberately registry metadata rather than a path-based callback:
/// the CPU plane receives a ready byte lease and selects one of these adapters
/// without gaining a filesystem capability.  A format can still report
/// [`FormatCapability::InventoryOnly`] when the adapter only has enough
/// evidence to identify the representation safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ByteAdapterKind {
    Engine,
    Structured,
    Protocol,
    Diagram,
    Engineering,
    Simulation,
    ContainerMedia,
    Pdf,
    Inventory,
}

impl ByteAdapterKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Engine => "engine",
            Self::Structured => "structured",
            Self::Protocol => "protocol",
            Self::Diagram => "diagram",
            Self::Engineering => "engineering",
            Self::Simulation => "simulation",
            Self::ContainerMedia => "container_media",
            Self::Pdf => "pdf",
            Self::Inventory => "inventory",
        }
    }
}

impl FormatCapability {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SemanticFull => "semantic_full",
            Self::StructuralPartial => "structural_partial",
            Self::SchemaFull => "schema_full",
            Self::ContainerFull => "container_full",
            Self::InventoryOnly => "inventory_only",
        }
    }
}

/// Whether an instance requires an independently supplied schema or descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaRequirement {
    NotRequired,
    Optional,
    Required,
}

impl SchemaRequirement {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotRequired => "not_required",
            Self::Optional => "optional",
            Self::Required => "required",
        }
    }
}

/// A bounded byte prefix test usable without allocation or filesystem access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MagicRule {
    pub offset: usize,
    pub bytes: &'static [u8],
}

impl MagicRule {
    pub const fn new(offset: usize, bytes: &'static [u8]) -> Self {
        Self { offset, bytes }
    }

    pub fn matches(self, input: &[u8]) -> bool {
        self.offset
            .checked_add(self.bytes.len())
            .and_then(|end| input.get(self.offset..end))
            .is_some_and(|candidate| candidate == self.bytes)
    }
}

/// Resource ceilings for one format family.
///
/// The runtime owns actual allocation credits; these values are parser-facing
/// ceilings and must be checked before recursion, decompression, or record
/// materialization. Capability reports expose these values so callers can
/// budget work before invoking the selected adapter. `max_records` counts the
/// aggregate retained nodes, edges, and hyperedges for bounded byte adapters;
/// it is not a separate allowance for each fact kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FormatLimits {
    pub max_input_bytes: u64,
    pub max_nesting: usize,
    pub max_records: usize,
    pub max_container_members: usize,
    pub max_recursion_depth: usize,
    pub max_expansion_ratio: u64,
}

pub const TEXT_LIMITS: FormatLimits = FormatLimits {
    max_input_bytes: 64 * 1024 * 1024,
    max_nesting: 128,
    max_records: 1_000_000,
    max_container_members: 0,
    max_recursion_depth: 0,
    max_expansion_ratio: 1,
};

pub const BINARY_LIMITS: FormatLimits = FormatLimits {
    max_input_bytes: 256 * 1024 * 1024,
    max_nesting: 128,
    max_records: 1_000_000,
    max_container_members: 0,
    max_recursion_depth: 0,
    max_expansion_ratio: 1,
};

pub const CONTAINER_LIMITS: FormatLimits = FormatLimits {
    max_input_bytes: 512 * 1024 * 1024,
    max_nesting: 64,
    // Recursive archives may repeat long virtual member paths in every fact.
    // Keep one complete archive tree at the graph batch fact ceiling so it
    // cannot retain a million-fact extraction before downstream byte admission.
    max_records: 4_096,
    max_container_members: 4_096,
    max_recursion_depth: 4,
    max_expansion_ratio: 128,
};

pub const IMAGE_LIMITS: FormatLimits = FormatLimits {
    max_input_bytes: 128 * 1024 * 1024,
    max_nesting: 16,
    max_records: 100_000,
    max_container_members: 0,
    max_recursion_depth: 0,
    max_expansion_ratio: 1,
};

/// Effective ceilings used by the structured-text adapter.
///
/// Keep this distinct from [`TEXT_LIMITS`], which covers language and document
/// engines with different expansion behavior. Structured parsing counts nodes
/// and edges together as facts, so `max_records` is also its fact ceiling.
pub const STRUCTURED_TEXT_LIMITS: FormatLimits = FormatLimits {
    max_input_bytes: 16 * 1024 * 1024,
    max_nesting: 32,
    max_records: 4_096,
    max_container_members: 0,
    max_recursion_depth: 0,
    max_expansion_ratio: 1,
};

/// Effective ceilings enforced by the diagram byte adapter.
pub const DIAGRAM_LIMITS: FormatLimits = FormatLimits {
    max_input_bytes: 8 * 1024 * 1024,
    max_nesting: 64,
    max_records: 100_000 + 250_000,
    max_container_members: 0,
    max_recursion_depth: 0,
    max_expansion_ratio: 1,
};

/// Effective ceilings enforced by the engineering byte adapter.
pub const ENGINEERING_LIMITS: FormatLimits = FormatLimits {
    max_input_bytes: 8 * 1024 * 1024,
    max_nesting: 16,
    max_records: 4_096 + 8_192,
    max_container_members: 0,
    max_recursion_depth: 0,
    max_expansion_ratio: 1,
};

/// Effective ceilings enforced by the protocol/IDL byte adapter.
///
/// The adapter wrapper enforces the aggregate fact ceiling after parsing as a
/// final guard for reference nodes, which are not declaration or field records.
pub const PROTOCOL_LIMITS: FormatLimits = FormatLimits {
    max_input_bytes: 16 * 1024 * 1024,
    max_nesting: 64,
    max_records: 1 + 4_096 + 32_768 + 65_536,
    max_container_members: 0,
    max_recursion_depth: 0,
    max_expansion_ratio: 1,
};

/// Columnar schema formats share the protocol fact and nesting ceilings but
/// admit a larger byte envelope for bounded footer/schema metadata reads.
pub const COLUMNAR_PROTOCOL_LIMITS: FormatLimits = FormatLimits {
    max_input_bytes: 256 * 1024 * 1024,
    ..PROTOCOL_LIMITS
};

/// Effective ceilings enforced by the simulation byte adapter.
pub const SIMULATION_LIMITS: FormatLimits = FormatLimits {
    max_input_bytes: 16 * 1024 * 1024,
    max_nesting: 128,
    max_records: 50_000 + 100_000,
    max_container_members: 0,
    max_recursion_depth: 0,
    max_expansion_ratio: 1,
};

/// Complete immutable metadata for one format family.
///
/// `legacy_file_type` and `watched` are compatibility projections, not claims
/// that the format already has a structured extractor. Future adapters use
/// `capability` to publish their actual support level and then deliberately
/// change admission in the detector contract with regression coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatSpec {
    pub id: FormatId,
    pub extensions: &'static [&'static str],
    pub file_names: &'static [&'static str],
    pub magic: &'static [MagicRule],
    pub capability: FormatCapability,
    pub schema_requirement: SchemaRequirement,
    pub limits: FormatLimits,
    pub legacy_file_type: Option<FileType>,
    pub watched: bool,
    document_heuristic: bool,
    office: bool,
    google_workspace: bool,
}

/// Stable, allocation-free capability reporting record for CLI and fixture
/// consumers. The slices are immutable registry data; callers can serialize
/// them without filesystem probing or format parser initialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FormatCapabilityReport {
    pub id: FormatId,
    pub capability: FormatCapability,
    pub schema_requirement: SchemaRequirement,
    pub adapter: ByteAdapterKind,
    pub extensions: &'static [&'static str],
    pub file_names: &'static [&'static str],
    pub limits: FormatLimits,
}

impl FormatSpec {
    fn contains_extension(self, extension: &str) -> bool {
        self.extensions
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(extension))
    }

    fn contains_file_name(self, file_name: &str) -> bool {
        self.file_names
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(file_name))
    }

    pub fn matches_magic(self, input: &[u8]) -> bool {
        self.magic.iter().copied().any(|rule| rule.matches(input))
    }

    pub fn adapter(self) -> ByteAdapterKind {
        match self.id.as_str() {
            "source-code" | "terraform-hcl" | "markdown" | "plain-text" | "package-manifest"
            | "mcp-configuration" => ByteAdapterKind::Engine,
            "json"
            | "json-variants"
            | "json5"
            | "json-lines"
            | "markup-documents"
            | "yaml"
            | "toml"
            | "xml"
            | "delimited-data"
            | "ini-properties-env"
            | "named-json-configuration"
            | "named-yaml-configuration"
            | "xml-schema-languages" => ByteAdapterKind::Structured,
            "protobuf-idl"
            | "protobuf-binary"
            | "flatbuffers-idl"
            | "flatbuffers-binary"
            | "capnproto"
            | "thrift"
            | "avro-schema"
            | "avro-idl"
            | "avro-container"
            | "arrow-ipc"
            | "parquet"
            | "orc"
            | "asn-1-text"
            | "asn-1-binary"
            | "semantic-schema-languages"
            | "api-idl"
            | "api-description" => ByteAdapterKind::Protocol,
            "graphviz-dot" | "mermaid" | "plantuml" | "d2" | "structurizr" | "dbml"
            | "bpmn-uml-sysml" | "drawio" | "whiteboard" => ByteAdapterKind::Diagram,
            "electrical-design" | "building-design" | "facility-metadata" => {
                ByteAdapterKind::Engineering
            }
            "openusd-ascii"
            | "openusd-inventory"
            | "simulation-assets"
            | "simulation-scenarios"
            | "simulation-fmi-model-description"
            | "simulation-inventory" => ByteAdapterKind::Simulation,
            "pdf" => ByteAdapterKind::Pdf,
            "office-open-xml"
            | "office-container-documents"
            | "office-documents"
            | "raster-image"
            | "additional-raster-image"
            | "svg"
            | "compressed-svg"
            | "media"
            | "additional-media"
            | "visio"
            | "building-container"
            | "zip-archive"
            | "tar-archive"
            | "archive"
            | "openusd-container"
            | "simulation-container" => ByteAdapterKind::ContainerMedia,
            "google-workspace-shortcut"
            | "configuration-languages"
            | "relax-ng-compact"
            | "dia"
            | "electrical-inventory"
            | "simulation-scenario-inventory" => ByteAdapterKind::Inventory,
            id => panic!("registered format {id} has no explicit byte-adapter owner"),
        }
    }

    pub fn capability_report(self) -> FormatCapabilityReport {
        FormatCapabilityReport {
            id: self.id,
            capability: self.capability,
            schema_requirement: self.schema_requirement,
            adapter: self.adapter(),
            extensions: self.extensions,
            file_names: self.file_names,
            limits: self.limits,
        }
    }
}

macro_rules! format_spec {
    (
        $id:expr,
        $extensions:expr,
        $file_names:expr,
        $magic:expr,
        $capability:expr,
        $schema_requirement:expr,
        $limits:expr,
        $legacy_file_type:expr,
        $watched:expr,
        $document_heuristic:expr,
        $office:expr,
        $google_workspace:expr $(,)?
    ) => {
        FormatSpec {
            id: FormatId::new($id),
            extensions: $extensions,
            file_names: $file_names,
            magic: $magic,
            capability: $capability,
            schema_requirement: $schema_requirement,
            limits: $limits,
            legacy_file_type: $legacy_file_type,
            watched: $watched,
            document_heuristic: $document_heuristic,
            office: $office,
            google_workspace: $google_workspace,
        }
    };
}

const MAGIC_PDF: &[MagicRule] = &[MagicRule::new(0, b"%PDF-")];
const MAGIC_RASTER: &[MagicRule] = &[
    MagicRule::new(0, b"\x89PNG\r\n\x1a\n"),
    MagicRule::new(0, b"\xff\xd8\xff"),
    MagicRule::new(0, b"GIF8"),
];
const MAGIC_ZIP: &[MagicRule] = &[MagicRule::new(0, b"PK\x03\x04")];
const MAGIC_ARCHIVE: &[MagicRule] = &[
    MagicRule::new(0, b"PK\x03\x04"),
    MagicRule::new(0, b"\x1f\x8b"),
    MagicRule::new(0, b"BZh"),
    MagicRule::new(0, b"\xfd7zXZ\x00"),
    MagicRule::new(0, b"\x28\xb5\x2f\xfd"),
    MagicRule::new(0, b"7z\xbc\xaf\x27\x1c"),
    MagicRule::new(0, b"Rar!\x1a\x07\x00"),
    MagicRule::new(0, b"Rar!\x1a\x07\x01\x00"),
];
const MAGIC_AVRO: &[MagicRule] = &[MagicRule::new(0, b"Obj\x01")];
const MAGIC_ARROW_IPC: &[MagicRule] = &[MagicRule::new(0, b"ARROW1")];
const MAGIC_PARQUET: &[MagicRule] = &[MagicRule::new(0, b"PAR1")];
const MAGIC_GLB: &[MagicRule] = &[MagicRule::new(0, b"glTF")];
const MAGIC_USDC: &[MagicRule] = &[MagicRule::new(0, b"PXR-USDC")];

const SOURCE_CODE_EXTENSIONS: &[&str] = &[
    "py", "pyi", "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "ejs", "ets", "go", "rs",
    "java", "groovy", "gradle", "cpp", "cc", "cxx", "c", "h", "hpp", "hh", "cu", "cuh", "metal",
    "rb", "rake", "swift", "kt", "kts", "cs", "scala", "php", "lua", "luau", "toc", "zig", "ps1",
    "psm1", "psd1", "ex", "exs", "m", "mm", "jl", "vue", "svelte", "astro", "dart", "v", "sv",
    "svh", "sql", "r", "f", "f90", "f95", "f03", "f08", "pas", "pp", "dpr", "dpk", "lpr", "inc",
    "dfm", "lfm", "lpk", "sh", "bash", "dm", "dme", "dmi", "dmm", "dmf", "sln", "slnx", "csproj",
    "fsproj", "vbproj", "xaml", "razor", "cshtml", "cls", "trigger",
];
const JSON_EXTENSIONS: &[&str] = &["json"];
const JSON_VARIANT_EXTENSIONS: &[&str] = &[
    "jsonc",
    "geojson",
    "topojson",
    "har",
    "webmanifest",
    "ipynb",
];
const JSON5_EXTENSIONS: &[&str] = &["json5"];
const JSON_LINES_EXTENSIONS: &[&str] = &["jsonl", "ndjson"];
const TERRAFORM_EXTENSIONS: &[&str] = &["tf", "tfvars", "hcl"];
const MARKDOWN_EXTENSIONS: &[&str] = &["md", "markdown", "mdx", "qmd", "skill"];
const PLAIN_TEXT_EXTENSIONS: &[&str] = &["txt"];
const MARKUP_DOCUMENT_EXTENSIONS: &[&str] = &[
    "rst", "rest", "html", "htm", "xhtml", "adoc", "asciidoc", "asc",
];
const YAML_EXTENSIONS: &[&str] = &["yaml", "yml"];
const TOML_EXTENSIONS: &[&str] = &["toml"];
const XML_EXTENSIONS: &[&str] = &["xml", "xsl", "xslt"];
const LEGACY_OFFICE_EXTENSIONS: &[&str] = &["docx", "xlsx"];
const OFFICE_CONTAINER_DOCUMENT_EXTENSIONS: &[&str] = &["pptx", "odt", "ods", "odp", "epub"];
const OFFICE_DOCUMENT_EXTENSIONS: &[&str] = &["doc", "xls", "xlsm", "ppt", "rtf"];
const LEGACY_IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];
const IMAGE_EXTENSIONS: &[&str] = &[
    "bmp", "tif", "tiff", "ico", "avif", "heic", "heif", "jp2", "jxl",
];
const LEGACY_SVG_EXTENSIONS: &[&str] = &["svg"];
const SVG_EXTENSIONS: &[&str] = &["svgz"];
const LEGACY_MEDIA_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "webm", "mkv", "avi", "m4v", "mp3", "wav", "m4a", "ogg",
];
const MEDIA_EXTENSIONS: &[&str] = &["flac", "aac", "aiff", "opus"];
const GOOGLE_WORKSPACE_EXTENSIONS: &[&str] = &["gdoc", "gsheet", "gslides"];
const DELIMITED_EXTENSIONS: &[&str] = &["csv", "ccsv", "tsv", "tab", "psv"];
const INI_EXTENSIONS: &[&str] = &["ini", "cfg", "conf", "config", "properties", "env", "envrc"];
const CONFIG_LANGUAGE_EXTENSIONS: &[&str] =
    &["cue", "jsonnet", "libsonnet", "kdl", "nix", "dhall", "pkl"];
const PROTOBUF_IDL_EXTENSIONS: &[&str] = &["proto", "prototxt"];
const PROTOBUF_BINARY_EXTENSIONS: &[&str] =
    &["pb", "pbf", "pb3", "protobin", "protobuf", "desc", "fds"];
const FLATBUFFERS_IDL_EXTENSIONS: &[&str] = &["fbs"];
const FLATBUFFERS_BINARY_EXTENSIONS: &[&str] = &["bfbs"];
const CAPNPROTO_EXTENSIONS: &[&str] = &["capnp"];
const THRIFT_EXTENSIONS: &[&str] = &["thrift"];
const AVRO_SCHEMA_EXTENSIONS: &[&str] = &["avsc"];
const AVRO_IDL_EXTENSIONS: &[&str] = &["avdl"];
const AVRO_CONTAINER_EXTENSIONS: &[&str] = &["avro"];
const ARROW_IPC_EXTENSIONS: &[&str] = &["arrow", "arrows", "feather", "ipc"];
const PARQUET_EXTENSIONS: &[&str] = &["parquet"];
const ORC_EXTENSIONS: &[&str] = &["orc"];
const ASN1_TEXT_EXTENSIONS: &[&str] = &["asn", "asn1", "asn1txt"];
const ASN1_BINARY_EXTENSIONS: &[&str] = &["ber", "der", "cer", "pem"];
const SEMANTIC_SCHEMA_EXTENSIONS: &[&str] = &["cddl", "yang", "yin"];
const XML_SCHEMA_EXTENSIONS: &[&str] = &["xsd", "wsdl", "rng"];
const RNC_EXTENSIONS: &[&str] = &["rnc"];
const API_IDL_EXTENSIONS: &[&str] = &["graphql", "gql", "wit", "smithy"];
const API_DESCRIPTION_EXTENSIONS: &[&str] = &["openapi", "asyncapi"];
const DOT_EXTENSIONS: &[&str] = &["dot", "gv", "graphviz"];
const MERMAID_EXTENSIONS: &[&str] = &["mmd", "mermaid"];
const PLANTUML_EXTENSIONS: &[&str] = &["puml", "plantuml", "iuml", "pu"];
const D2_EXTENSIONS: &[&str] = &["d2"];
const STRUCTURIZR_EXTENSIONS: &[&str] = &["dsl", "structurizr"];
const DBML_EXTENSIONS: &[&str] = &["dbml"];
const BPMN_UML_EXTENSIONS: &[&str] = &["bpmn", "bpmn2", "xmi", "uml", "uml2", "sysml"];
const DRAWIO_EXTENSIONS: &[&str] = &["drawio"];
const DIA_EXTENSIONS: &[&str] = &["dio"];
const VISIO_EXTENSIONS: &[&str] = &["vsdx", "vsdm", "vssx", "vstx"];
const WHITEBOARD_EXTENSIONS: &[&str] = &["excalidraw", "tldr", "tldraw"];
const ELECTRICAL_SEMANTIC_EXTENSIONS: &[&str] = &[
    "kicad_sch",
    "kicad_pcb",
    "kicad_pro",
    "kicad_mod",
    "kicad_sym",
    "sch",
    "brd",
    "lbr",
    "net",
    "gbr",
    "ger",
    "gtl",
    "gbl",
    "gto",
    "gbo",
    "gts",
    "gbs",
    "ipc2581",
    "dxf",
];
const ELECTRICAL_INVENTORY_EXTENSIONS: &[&str] = &["gko", "emn", "emp"];
const BUILDING_SEMANTIC_EXTENSIONS: &[&str] = &[
    "ifc", "ifcxml", "bcf", "bcfxml", "ids", "gbxml", "citygml", "gml", "landxml", "dexpi", "pid",
    "pfd", "epjson", "idf", "mo", "modelica", "foam", "openfoam",
];
const BUILDING_CONTAINER_EXTENSIONS: &[&str] = &["ifczip", "bcfzip"];
const FACILITY_EXTENSIONS: &[&str] = &[
    "redfish",
    "netbox",
    "haystack",
    "hayson",
    "zinc",
    "trio",
    "skyarc",
    "brick",
    "openconfig",
    "ttl",
    "rdf",
    "nq",
];
const USD_ASCII_EXTENSIONS: &[&str] = &["usda"];
const USD_INVENTORY_EXTENSIONS: &[&str] = &["usd", "usdc"];
const USD_CONTAINER_EXTENSIONS: &[&str] = &["usdz"];
const SEMANTIC_SIM_ASSET_EXTENSIONS: &[&str] =
    &["mtlx", "materialx", "gltf", "urdf", "sdf", "mjcf"];
const SIMULATION_CONTAINER_EXTENSIONS: &[&str] = &["fmu"];
const SIMULATION_INVENTORY_EXTENSIONS: &[&str] = &["glb", "fmi"];
const SCENARIO_EXTENSIONS: &[&str] = &["xodr", "xosc"];
const SCENARIO_INVENTORY_EXTENSIONS: &[&str] = &["osc"];
const ZIP_ARCHIVE_EXTENSIONS: &[&str] = &["zip"];
const TAR_ARCHIVE_EXTENSIONS: &[&str] = &["tar"];
const ARCHIVE_EXTENSIONS: &[&str] = &[
    "tgz", "gz", "bz2", "xz", "zst", "7z", "rar", "cpio", "cab", "lz4", "lz",
];

const PACKAGE_MANIFEST_NAMES: &[&str] = &[
    "package.json",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "cargo.toml",
    "cargo.lock",
    "pyproject.toml",
    "poetry.lock",
    "uv.lock",
    "go.mod",
    "go.sum",
    "composer.json",
    "composer.lock",
    "gemfile",
    "gemfile.lock",
    "mix.exs",
    "build.gradle",
    "build.gradle.kts",
    "pom.xml",
    "settings.gradle",
    "settings.gradle.kts",
];
const API_FILE_NAMES: &[&str] = &[
    "openapi.json",
    "openapi.yaml",
    "openapi.yml",
    "swagger.json",
    "swagger.yaml",
    "swagger.yml",
    "asyncapi.json",
    "asyncapi.yaml",
    "asyncapi.yml",
    "schema.json",
    "schema.graphql",
];
const MCP_FILE_NAMES: &[&str] = &[
    ".mcp.json",
    "mcp.json",
    "claude_desktop_config.json",
    "mcp_servers.json",
];
const CONFIG_INI_FILE_NAMES: &[&str] = &[
    ".env",
    ".envrc",
    ".editorconfig",
    ".npmrc",
    ".yarnrc",
    ".gitmodules",
];
const CONFIG_YAML_FILE_NAMES: &[&str] = &[".yarnrc.yml"];
const CONFIG_JSON_FILE_NAMES: &[&str] = &[".prettierrc", ".eslintrc", ".babelrc"];
const FMI_FILE_NAMES: &[&str] = &["modeldescription.xml"];

const FORMAT_SPECS: &[FormatSpec] = &[
    format_spec!(
        "source-code",
        SOURCE_CODE_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        TEXT_LIMITS,
        Some(FileType::Code),
        true,
        false,
        false,
        false,
    ),
    format_spec!(
        "json",
        JSON_EXTENSIONS,
        &[],
        &[],
        FormatCapability::SemanticFull,
        SchemaRequirement::NotRequired,
        STRUCTURED_TEXT_LIMITS,
        Some(FileType::Code),
        true,
        false,
        false,
        false,
    ),
    format_spec!(
        "json-variants",
        JSON_VARIANT_EXTENSIONS,
        &[],
        &[],
        FormatCapability::SemanticFull,
        SchemaRequirement::NotRequired,
        STRUCTURED_TEXT_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "json5",
        JSON5_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        STRUCTURED_TEXT_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "json-lines",
        JSON_LINES_EXTENSIONS,
        &[],
        &[],
        FormatCapability::SemanticFull,
        SchemaRequirement::NotRequired,
        STRUCTURED_TEXT_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "terraform-hcl",
        TERRAFORM_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        TEXT_LIMITS,
        Some(FileType::Code),
        true,
        false,
        false,
        false,
    ),
    format_spec!(
        "markdown",
        MARKDOWN_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        TEXT_LIMITS,
        Some(FileType::Document),
        true,
        true,
        false,
        false,
    ),
    format_spec!(
        "plain-text",
        PLAIN_TEXT_EXTENSIONS,
        &[],
        &[],
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        TEXT_LIMITS,
        Some(FileType::Document),
        true,
        true,
        false,
        false,
    ),
    format_spec!(
        "markup-documents",
        MARKUP_DOCUMENT_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        STRUCTURED_TEXT_LIMITS,
        Some(FileType::Document),
        true,
        true,
        false,
        false,
    ),
    format_spec!(
        "yaml",
        YAML_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        STRUCTURED_TEXT_LIMITS,
        Some(FileType::Document),
        true,
        true,
        false,
        false,
    ),
    format_spec!(
        "toml",
        TOML_EXTENSIONS,
        &[],
        &[],
        FormatCapability::SemanticFull,
        SchemaRequirement::NotRequired,
        STRUCTURED_TEXT_LIMITS,
        Some(FileType::Document),
        true,
        true,
        false,
        false,
    ),
    format_spec!(
        "xml",
        XML_EXTENSIONS,
        &[],
        &[],
        FormatCapability::SemanticFull,
        SchemaRequirement::NotRequired,
        STRUCTURED_TEXT_LIMITS,
        Some(FileType::Document),
        true,
        true,
        false,
        false,
    ),
    format_spec!(
        "pdf",
        &["pdf"],
        &[],
        MAGIC_PDF,
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        BINARY_LIMITS,
        Some(FileType::Paper),
        true,
        false,
        false,
        false,
    ),
    format_spec!(
        "office-open-xml",
        LEGACY_OFFICE_EXTENSIONS,
        &[],
        &[],
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        CONTAINER_LIMITS,
        Some(FileType::Document),
        false,
        false,
        true,
        false,
    ),
    format_spec!(
        "office-container-documents",
        OFFICE_CONTAINER_DOCUMENT_EXTENSIONS,
        &[],
        MAGIC_ZIP,
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        CONTAINER_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "office-documents",
        OFFICE_DOCUMENT_EXTENSIONS,
        &[],
        &[],
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        CONTAINER_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "raster-image",
        LEGACY_IMAGE_EXTENSIONS,
        &[],
        MAGIC_RASTER,
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        IMAGE_LIMITS,
        Some(FileType::Image),
        true,
        false,
        false,
        false,
    ),
    format_spec!(
        "additional-raster-image",
        IMAGE_EXTENSIONS,
        &[],
        MAGIC_RASTER,
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        IMAGE_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "svg",
        LEGACY_SVG_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        TEXT_LIMITS,
        Some(FileType::Image),
        true,
        false,
        false,
        false,
    ),
    format_spec!(
        "compressed-svg",
        SVG_EXTENSIONS,
        &[],
        &[],
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        TEXT_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "media",
        LEGACY_MEDIA_EXTENSIONS,
        &[],
        &[],
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        BINARY_LIMITS,
        Some(FileType::Video),
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "additional-media",
        MEDIA_EXTENSIONS,
        &[],
        &[],
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        BINARY_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "google-workspace-shortcut",
        GOOGLE_WORKSPACE_EXTENSIONS,
        &[],
        &[],
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        TEXT_LIMITS,
        Some(FileType::Document),
        false,
        false,
        false,
        true,
    ),
    format_spec!(
        "delimited-data",
        DELIMITED_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        STRUCTURED_TEXT_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "ini-properties-env",
        INI_EXTENSIONS,
        CONFIG_INI_FILE_NAMES,
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        STRUCTURED_TEXT_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "named-json-configuration",
        &[],
        CONFIG_JSON_FILE_NAMES,
        &[],
        FormatCapability::SemanticFull,
        SchemaRequirement::NotRequired,
        STRUCTURED_TEXT_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "named-yaml-configuration",
        &[],
        CONFIG_YAML_FILE_NAMES,
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        STRUCTURED_TEXT_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "configuration-languages",
        CONFIG_LANGUAGE_EXTENSIONS,
        &[],
        &[],
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        TEXT_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "package-manifest",
        &[],
        PACKAGE_MANIFEST_NAMES,
        &[],
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        TEXT_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "api-description",
        API_DESCRIPTION_EXTENSIONS,
        API_FILE_NAMES,
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        PROTOCOL_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "protobuf-idl",
        PROTOBUF_IDL_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        PROTOCOL_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "protobuf-binary",
        PROTOBUF_BINARY_EXTENSIONS,
        &[],
        &[],
        FormatCapability::InventoryOnly,
        SchemaRequirement::Required,
        PROTOCOL_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "flatbuffers-idl",
        FLATBUFFERS_IDL_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        PROTOCOL_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "flatbuffers-binary",
        FLATBUFFERS_BINARY_EXTENSIONS,
        &[],
        &[],
        FormatCapability::InventoryOnly,
        SchemaRequirement::Required,
        PROTOCOL_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "capnproto",
        CAPNPROTO_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::Optional,
        PROTOCOL_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "thrift",
        THRIFT_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        PROTOCOL_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "avro-schema",
        AVRO_SCHEMA_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        PROTOCOL_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "avro-idl",
        AVRO_IDL_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        PROTOCOL_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "avro-container",
        AVRO_CONTAINER_EXTENSIONS,
        &[],
        MAGIC_AVRO,
        FormatCapability::InventoryOnly,
        SchemaRequirement::Optional,
        PROTOCOL_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "arrow-ipc",
        ARROW_IPC_EXTENSIONS,
        &[],
        MAGIC_ARROW_IPC,
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        COLUMNAR_PROTOCOL_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "parquet",
        PARQUET_EXTENSIONS,
        &[],
        MAGIC_PARQUET,
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        COLUMNAR_PROTOCOL_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "orc",
        ORC_EXTENSIONS,
        &[],
        &[],
        FormatCapability::InventoryOnly,
        SchemaRequirement::Optional,
        PROTOCOL_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "asn-1-text",
        ASN1_TEXT_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        PROTOCOL_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "semantic-schema-languages",
        SEMANTIC_SCHEMA_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        PROTOCOL_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "xml-schema-languages",
        XML_SCHEMA_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        STRUCTURED_TEXT_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "relax-ng-compact",
        RNC_EXTENSIONS,
        &[],
        &[],
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        TEXT_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "asn-1-binary",
        ASN1_BINARY_EXTENSIONS,
        &[],
        &[],
        FormatCapability::InventoryOnly,
        SchemaRequirement::Required,
        PROTOCOL_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "api-idl",
        API_IDL_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        PROTOCOL_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "graphviz-dot",
        DOT_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        DIAGRAM_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "mermaid",
        MERMAID_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        DIAGRAM_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "plantuml",
        PLANTUML_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        DIAGRAM_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "d2",
        D2_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        DIAGRAM_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "structurizr",
        STRUCTURIZR_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        DIAGRAM_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "dbml",
        DBML_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        DIAGRAM_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "bpmn-uml-sysml",
        BPMN_UML_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        DIAGRAM_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "drawio",
        DRAWIO_EXTENSIONS,
        &[],
        MAGIC_ZIP,
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        DIAGRAM_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "dia",
        DIA_EXTENSIONS,
        &[],
        &[],
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        TEXT_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "visio",
        VISIO_EXTENSIONS,
        &[],
        MAGIC_ZIP,
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        CONTAINER_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "whiteboard",
        WHITEBOARD_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        DIAGRAM_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "electrical-design",
        ELECTRICAL_SEMANTIC_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        ENGINEERING_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "electrical-inventory",
        ELECTRICAL_INVENTORY_EXTENSIONS,
        &[],
        &[],
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        CONTAINER_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "building-design",
        BUILDING_SEMANTIC_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        ENGINEERING_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "building-container",
        BUILDING_CONTAINER_EXTENSIONS,
        &[],
        MAGIC_ZIP,
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        CONTAINER_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "facility-metadata",
        FACILITY_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        ENGINEERING_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "openusd-ascii",
        USD_ASCII_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        SIMULATION_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "openusd-inventory",
        USD_INVENTORY_EXTENSIONS,
        &[],
        MAGIC_USDC,
        FormatCapability::InventoryOnly,
        SchemaRequirement::Optional,
        SIMULATION_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "openusd-container",
        USD_CONTAINER_EXTENSIONS,
        &[],
        MAGIC_ZIP,
        FormatCapability::InventoryOnly,
        SchemaRequirement::Optional,
        CONTAINER_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "simulation-assets",
        SEMANTIC_SIM_ASSET_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        SIMULATION_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "simulation-fmi-model-description",
        &[],
        FMI_FILE_NAMES,
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        SIMULATION_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "simulation-container",
        SIMULATION_CONTAINER_EXTENSIONS,
        &[],
        MAGIC_ZIP,
        FormatCapability::InventoryOnly,
        SchemaRequirement::Optional,
        CONTAINER_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "simulation-inventory",
        SIMULATION_INVENTORY_EXTENSIONS,
        &[],
        MAGIC_GLB,
        FormatCapability::InventoryOnly,
        SchemaRequirement::Optional,
        SIMULATION_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "simulation-scenarios",
        SCENARIO_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        SIMULATION_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "simulation-scenario-inventory",
        SCENARIO_INVENTORY_EXTENSIONS,
        &[],
        &[],
        FormatCapability::InventoryOnly,
        SchemaRequirement::Optional,
        BINARY_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "zip-archive",
        ZIP_ARCHIVE_EXTENSIONS,
        &[],
        MAGIC_ZIP,
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        CONTAINER_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "tar-archive",
        TAR_ARCHIVE_EXTENSIONS,
        &[],
        &[],
        FormatCapability::StructuralPartial,
        SchemaRequirement::NotRequired,
        CONTAINER_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "archive",
        ARCHIVE_EXTENSIONS,
        &[],
        MAGIC_ARCHIVE,
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        CONTAINER_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
    format_spec!(
        "mcp-configuration",
        &[],
        MCP_FILE_NAMES,
        &[],
        FormatCapability::InventoryOnly,
        SchemaRequirement::NotRequired,
        TEXT_LIMITS,
        None,
        false,
        false,
        false,
        false,
    ),
];

/// Compatibility projection for callers that need the exact historical watcher
/// suffix list. New registry entries remain unwatched until a targeted watch
/// contract changes this constant and its tests.
pub const WATCHED_EXTENSIONS: &[&str] = &[
    ".py",
    ".pyi",
    ".ts",
    ".tsx",
    ".mts",
    ".cts",
    ".js",
    ".jsx",
    ".mjs",
    ".cjs",
    ".ejs",
    ".ets",
    ".go",
    ".rs",
    ".java",
    ".groovy",
    ".gradle",
    ".cpp",
    ".cc",
    ".cxx",
    ".c",
    ".h",
    ".hpp",
    ".hh",
    ".cu",
    ".cuh",
    ".metal",
    ".rb",
    ".rake",
    ".swift",
    ".kt",
    ".kts",
    ".cs",
    ".scala",
    ".php",
    ".lua",
    ".luau",
    ".toc",
    ".zig",
    ".ps1",
    ".psm1",
    ".psd1",
    ".ex",
    ".exs",
    ".m",
    ".mm",
    ".jl",
    ".vue",
    ".svelte",
    ".astro",
    ".dart",
    ".v",
    ".sv",
    ".svh",
    ".sql",
    ".r",
    ".f",
    ".f90",
    ".f95",
    ".f03",
    ".f08",
    ".pas",
    ".pp",
    ".dpr",
    ".dpk",
    ".lpr",
    ".inc",
    ".dfm",
    ".lfm",
    ".lpk",
    ".sh",
    ".bash",
    ".json",
    ".tf",
    ".tfvars",
    ".hcl",
    ".dm",
    ".dme",
    ".dmi",
    ".dmm",
    ".dmf",
    ".sln",
    ".slnx",
    ".csproj",
    ".fsproj",
    ".vbproj",
    ".xaml",
    ".razor",
    ".cshtml",
    ".cls",
    ".trigger",
    ".md",
    ".markdown",
    ".mdx",
    ".qmd",
    ".skill",
    ".txt",
    ".rst",
    ".html",
    ".yaml",
    ".yml",
    ".toml",
    ".xml",
    ".pdf",
    ".png",
    ".jpg",
    ".jpeg",
    ".gif",
    ".webp",
    ".svg",
];

/// Immutable lookup table for classification, capability reporting, and future
/// byte extractor routing.
#[derive(Debug, Clone, Copy)]
pub struct FormatRegistry {
    specs: &'static [FormatSpec],
}

impl FormatRegistry {
    const fn new(specs: &'static [FormatSpec]) -> Self {
        Self { specs }
    }

    pub fn specs(&self) -> &'static [FormatSpec] {
        self.specs
    }

    /// Iterate stable, allocation-free capability records in registry order.
    /// This is the single reporting source for CLI/UI/fixture consumers.
    pub fn capability_reports(&self) -> impl Iterator<Item = FormatCapabilityReport> + '_ {
        self.specs
            .iter()
            .copied()
            .map(FormatSpec::capability_report)
    }

    pub fn find_by_id(&self, id: &str) -> Option<&'static FormatSpec> {
        self.specs.iter().find(|spec| spec.id.as_str() == id)
    }

    /// Look up a registered extension. The leading dot is accepted for CLI and
    /// watcher callers; no allocation is performed.
    pub fn find_by_extension(&self, extension: &str) -> Option<&'static FormatSpec> {
        let normalized = extension.strip_prefix('.').unwrap_or(extension);
        (!normalized.is_empty())
            .then(|| {
                self.specs
                    .iter()
                    .find(|spec| spec.contains_extension(normalized))
            })
            .flatten()
    }

    pub fn find_by_file_name(&self, file_name: &str) -> Option<&'static FormatSpec> {
        self.specs
            .iter()
            .find(|spec| spec.contains_file_name(file_name))
    }

    /// File-name-specific formats (for example `.mcp.json`) take precedence
    /// over an extension family. This lookup does not alter detector admission.
    pub fn find_by_path(&self, path: &Path) -> Option<&'static FormatSpec> {
        let name = path.file_name()?.to_str()?;
        self.find_by_file_name(name)
            .or_else(|| self.facility_spec_for_path(path))
            .or_else(|| {
                path.extension()
                    .and_then(|extension| extension.to_str())
                    .and_then(|extension| self.find_by_extension(extension))
            })
    }

    /// Return every safe prefix-compatible candidate. Ambiguous container
    /// signatures (such as ZIP) deliberately return multiple candidates; an
    /// adapter must use path/container evidence before selecting a subtype.
    pub fn find_by_magic<'a>(
        &'a self,
        input: &'a [u8],
    ) -> impl Iterator<Item = &'static FormatSpec> + 'a {
        self.specs
            .iter()
            .filter(move |spec| spec.matches_magic(input))
    }

    /// Legacy detector projection. Formats registered for future adapters but
    /// not admitted today return `None`, preserving existing classification.
    pub fn classify_extension(&self, extension: &str) -> Option<FileType> {
        self.find_by_extension(extension)
            .and_then(|spec| spec.legacy_file_type)
    }

    pub fn capability_for_extension(&self, extension: &str) -> Option<FormatCapability> {
        self.find_by_extension(extension)
            .map(|spec| spec.capability)
    }

    pub fn schema_requirement_for_extension(&self, extension: &str) -> Option<SchemaRequirement> {
        self.find_by_extension(extension)
            .map(|spec| spec.schema_requirement)
    }

    pub fn is_document_heuristic_extension(&self, extension: &str) -> bool {
        self.find_by_extension(extension)
            .is_some_and(|spec| spec.document_heuristic)
    }

    pub fn is_office_extension(&self, extension: &str) -> bool {
        self.find_by_extension(extension)
            .is_some_and(|spec| spec.office)
    }

    pub fn is_google_workspace_extension(&self, extension: &str) -> bool {
        self.find_by_extension(extension)
            .is_some_and(|spec| spec.google_workspace)
    }

    /// Accept either `.rs` or `rs`, exactly as the existing CLI watcher did.
    pub fn is_watched_extension(&self, extension: &str) -> bool {
        let normalized = extension.strip_prefix('.').unwrap_or(extension);
        WATCHED_EXTENSIONS.iter().any(|watched| {
            watched
                .strip_prefix('.')
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(normalized))
        })
    }

    pub fn watched_extensions(&self) -> impl Iterator<Item = &'static str> + '_ {
        WATCHED_EXTENSIONS.iter().map(|extension| &extension[1..])
    }

    fn facility_spec_for_path(&self, path: &Path) -> Option<&'static FormatSpec> {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())?
            .to_ascii_lowercase();
        if !matches!(extension.as_str(), "json" | "yaml" | "yml") {
            return None;
        }
        let name = path.file_name()?.to_str()?.to_ascii_lowercase();
        [
            "redfish",
            "netbox",
            "openconfig",
            "haystack",
            "brick",
            "building",
            "facility",
        ]
        .iter()
        .any(|marker| name.contains(marker))
        .then(|| self.find_by_id("facility-metadata"))
        .flatten()
    }
}

static FORMAT_REGISTRY: FormatRegistry = FormatRegistry::new(FORMAT_SPECS);

pub const fn format_registry() -> &'static FormatRegistry {
    &FORMAT_REGISTRY
}

#[cfg(test)]
mod tests {
    use super::{
        format_registry, ByteAdapterKind, FormatCapability, MagicRule, SchemaRequirement,
        COLUMNAR_PROTOCOL_LIMITS, CONTAINER_LIMITS, DIAGRAM_LIMITS, ENGINEERING_LIMITS,
        PROTOCOL_LIMITS, SIMULATION_LIMITS, STRUCTURED_TEXT_LIMITS, WATCHED_EXTENSIONS,
    };
    use std::path::Path;

    #[test]
    fn legacy_classification_and_watch_projection_remain_exact() {
        let registry = format_registry();
        for extension in ["rs", "JSON", "tfvars", "csproj"] {
            assert_eq!(
                registry.classify_extension(extension).unwrap().as_str(),
                "code"
            );
        }
        for extension in ["md", "yaml", "toml", "xml"] {
            assert_eq!(
                registry.classify_extension(extension).unwrap().as_str(),
                "document"
            );
        }
        assert_eq!(
            registry.classify_extension("pdf").unwrap().as_str(),
            "paper"
        );
        assert_eq!(
            registry.capability_for_extension("pdf"),
            Some(FormatCapability::InventoryOnly)
        );
        assert_eq!(
            registry.classify_extension("png").unwrap().as_str(),
            "image"
        );
        assert_eq!(
            registry.classify_extension("mp4").unwrap().as_str(),
            "video"
        );
        assert_eq!(registry.classify_extension("csv"), None);
        assert_eq!(registry.classify_extension("zip"), None);

        let projected: Vec<_> = registry
            .watched_extensions()
            .map(|extension| format!(".{extension}"))
            .collect();
        assert_eq!(projected, WATCHED_EXTENSIONS);
        assert!(registry.is_watched_extension(".RS"));
        assert!(!registry.is_watched_extension("csv"));
    }

    #[test]
    fn new_formats_do_not_change_legacy_classification() {
        let registry = format_registry();
        for extension in ["bfbs", "orc", "7z", "avif", "flac"] {
            assert_eq!(
                registry.capability_for_extension(extension),
                Some(FormatCapability::InventoryOnly),
                "{extension}"
            );
            assert_eq!(registry.classify_extension(extension), None, "{extension}");
        }
        for extension in ["jsonc", "jsonl", "ndjson"] {
            assert!(matches!(
                registry.capability_for_extension(extension),
                Some(FormatCapability::SemanticFull)
            ));
            assert_eq!(registry.classify_extension(extension), None, "{extension}");
        }
        for extension in [
            "csv",
            "psv",
            "proto",
            "prototxt",
            "kicad_pcb",
            "ifc",
            "openapi",
            "xosc",
            "usda",
            "json5",
            "yaml",
            "yml",
            "dot",
            "mmd",
            "drawio",
            "arrow",
            "arrows",
            "feather",
            "ipc",
            "parquet",
        ] {
            assert_eq!(
                registry.capability_for_extension(extension),
                Some(FormatCapability::StructuralPartial),
                "{extension}"
            );
            if !matches!(extension, "yaml" | "yml") {
                assert_eq!(registry.classify_extension(extension), None, "{extension}");
            }
        }
        assert_eq!(
            registry.schema_requirement_for_extension("pb"),
            Some(SchemaRequirement::Required)
        );
        assert_eq!(
            registry.schema_requirement_for_extension("bfbs"),
            Some(SchemaRequirement::Required)
        );
    }

    #[test]
    fn path_and_magic_lookup_are_safe_and_specific() {
        let registry = format_registry();
        assert_eq!(
            registry
                .find_by_path(Path::new(".mcp.json"))
                .unwrap()
                .id
                .as_str(),
            "mcp-configuration"
        );
        assert_eq!(
            registry
                .find_by_path(Path::new("openapi.yaml"))
                .unwrap()
                .id
                .as_str(),
            "api-description"
        );
        assert_eq!(
            registry
                .find_by_magic(b"%PDF-1.7\n")
                .next()
                .unwrap()
                .id
                .as_str(),
            "pdf"
        );
        assert!(registry
            .find_by_magic(b"PK\x03\x04rest")
            .any(|spec| spec.id.as_str() == "archive"));
        assert!(!MagicRule::new(usize::MAX, b"x").matches(b"x"));
    }

    #[test]
    fn every_magic_rule_matches_only_its_complete_registered_prefix() {
        let registry = format_registry();
        let mut checked_rules = 0usize;
        for spec in registry.specs() {
            for rule in spec.magic {
                checked_rules += 1;
                let mut sample = vec![0_u8; rule.offset + rule.bytes.len()];
                sample[rule.offset..].copy_from_slice(rule.bytes);
                assert!(
                    rule.matches(&sample),
                    "{} magic rule did not match its own bytes",
                    spec.id.as_str()
                );
                assert!(
                    registry
                        .find_by_magic(&sample)
                        .any(|candidate| candidate.id == spec.id),
                    "{} magic rule was not discoverable from the registry",
                    spec.id.as_str()
                );
                let mut corrupted = sample;
                corrupted[rule.offset] ^= 0xff;
                assert!(
                    !rule.matches(&corrupted),
                    "{} magic rule accepted a changed prefix",
                    spec.id.as_str()
                );
            }
        }
        assert!(checked_rules > 0, "the registry has no magic rules");
    }

    #[test]
    fn every_registered_extension_and_file_name_has_one_owner() {
        let registry = format_registry();
        for spec in registry.specs() {
            for extension in spec.extensions {
                let owner = registry.find_by_extension(extension).unwrap();
                assert_eq!(owner.id, spec.id, "duplicate extension {extension}");
            }
            for file_name in spec.file_names {
                let owner = registry.find_by_file_name(file_name).unwrap();
                assert_eq!(owner.id, spec.id, "duplicate file name {file_name}");
            }
        }
    }

    #[test]
    fn capability_reports_are_complete_bounded_and_explicitly_owned() {
        let registry = format_registry();
        let reports = registry.capability_reports().collect::<Vec<_>>();
        assert_eq!(reports.len(), registry.specs().len());
        for (spec, report) in registry.specs().iter().zip(reports) {
            assert_eq!(report.id, spec.id);
            assert_eq!(report.capability, spec.capability);
            assert_eq!(report.schema_requirement, spec.schema_requirement);
            assert_eq!(report.adapter, spec.adapter());
            assert!(report.limits.max_input_bytes > 0);
            assert!(report.limits.max_nesting > 0);
            assert!(report.limits.max_records > 0);
            assert!(
                !report.extensions.is_empty() || !report.file_names.is_empty(),
                "{} must have a deterministic discriminator",
                report.id.as_str()
            );
        }
    }

    #[test]
    fn structured_reports_publish_their_effective_parser_ceilings() {
        let registry = format_registry();
        let mut structured_specs = 0usize;
        for spec in registry.specs() {
            if spec.adapter() != ByteAdapterKind::Structured {
                continue;
            }
            structured_specs += 1;
            assert_eq!(
                spec.limits,
                STRUCTURED_TEXT_LIMITS,
                "{} reports limits that differ from the structured parser",
                spec.id.as_str()
            );
        }
        assert!(structured_specs > 0);
    }

    #[test]
    fn specialized_byte_adapters_publish_their_effective_parser_ceilings() {
        assert_eq!(DIAGRAM_LIMITS.max_input_bytes, 8 * 1024 * 1024);
        assert_eq!(DIAGRAM_LIMITS.max_records, 350_000);
        assert_eq!(ENGINEERING_LIMITS.max_input_bytes, 8 * 1024 * 1024);
        assert_eq!(ENGINEERING_LIMITS.max_records, 12_288);
        assert_eq!(PROTOCOL_LIMITS.max_input_bytes, 16 * 1024 * 1024);
        assert_eq!(PROTOCOL_LIMITS.max_records, 102_401);
        assert_eq!(COLUMNAR_PROTOCOL_LIMITS.max_input_bytes, 256 * 1024 * 1024);
        assert_eq!(
            COLUMNAR_PROTOCOL_LIMITS.max_records,
            PROTOCOL_LIMITS.max_records
        );
        assert_eq!(SIMULATION_LIMITS.max_input_bytes, 16 * 1024 * 1024);
        assert_eq!(SIMULATION_LIMITS.max_records, 150_000);

        let registry = format_registry();
        let mut family_counts = [0usize; 4];
        for spec in registry.specs() {
            let expected = match spec.adapter() {
                ByteAdapterKind::Diagram => {
                    family_counts[0] += 1;
                    Some(DIAGRAM_LIMITS)
                }
                ByteAdapterKind::Engineering => {
                    family_counts[1] += 1;
                    Some(ENGINEERING_LIMITS)
                }
                ByteAdapterKind::Protocol => {
                    family_counts[2] += 1;
                    Some(if matches!(spec.id.as_str(), "arrow-ipc" | "parquet") {
                        COLUMNAR_PROTOCOL_LIMITS
                    } else {
                        PROTOCOL_LIMITS
                    })
                }
                ByteAdapterKind::Simulation => {
                    family_counts[3] += 1;
                    Some(SIMULATION_LIMITS)
                }
                _ => None,
            };
            if let Some(expected) = expected {
                assert_eq!(
                    spec.limits,
                    expected,
                    "{} reports limits that differ from its byte adapter",
                    spec.id.as_str()
                );
            }
        }
        assert!(family_counts.into_iter().all(|count| count > 0));
    }

    #[test]
    fn recursive_container_reports_publish_the_aggregate_tree_fact_ceiling() {
        assert_eq!(CONTAINER_LIMITS.max_records, 4_096);
        let tar = format_registry()
            .find_by_extension("tar")
            .expect("tar registry entry");
        assert_eq!(tar.capability, FormatCapability::StructuralPartial);
        assert_eq!(tar.limits, CONTAINER_LIMITS);
    }

    #[test]
    fn alias_and_representation_splits_resolve_to_the_correct_contract() {
        let registry = format_registry();
        for (extension, id, capability) in [
            ("json5", "json5", FormatCapability::StructuralPartial),
            ("jsonl", "json-lines", FormatCapability::SemanticFull),
            ("ndjson", "json-lines", FormatCapability::SemanticFull),
            ("psv", "delimited-data", FormatCapability::StructuralPartial),
            (
                "ccsv",
                "delimited-data",
                FormatCapability::StructuralPartial,
            ),
            (
                "prototxt",
                "protobuf-idl",
                FormatCapability::StructuralPartial,
            ),
            ("pb3", "protobuf-binary", FormatCapability::InventoryOnly),
            (
                "protobin",
                "protobuf-binary",
                FormatCapability::InventoryOnly,
            ),
            ("arrows", "arrow-ipc", FormatCapability::StructuralPartial),
            ("feather", "arrow-ipc", FormatCapability::StructuralPartial),
            ("parquet", "parquet", FormatCapability::StructuralPartial),
            ("orc", "orc", FormatCapability::InventoryOnly),
            ("avdl", "avro-idl", FormatCapability::StructuralPartial),
            (
                "ttl",
                "facility-metadata",
                FormatCapability::StructuralPartial,
            ),
            (
                "rdf",
                "facility-metadata",
                FormatCapability::StructuralPartial,
            ),
            (
                "nq",
                "facility-metadata",
                FormatCapability::StructuralPartial,
            ),
            (
                "uml2",
                "bpmn-uml-sysml",
                FormatCapability::StructuralPartial,
            ),
            ("dio", "dia", FormatCapability::InventoryOnly),
            ("tldraw", "whiteboard", FormatCapability::StructuralPartial),
            (
                "gko",
                "electrical-inventory",
                FormatCapability::InventoryOnly,
            ),
            (
                "emn",
                "electrical-inventory",
                FormatCapability::InventoryOnly,
            ),
            (
                "emp",
                "electrical-inventory",
                FormatCapability::InventoryOnly,
            ),
            (
                "bcfzip",
                "building-container",
                FormatCapability::InventoryOnly,
            ),
            (
                "fmi",
                "simulation-inventory",
                FormatCapability::InventoryOnly,
            ),
            (
                "fmu",
                "simulation-container",
                FormatCapability::InventoryOnly,
            ),
            ("usdz", "openusd-container", FormatCapability::InventoryOnly),
            (
                "osc",
                "simulation-scenario-inventory",
                FormatCapability::InventoryOnly,
            ),
        ] {
            let spec = registry
                .find_by_extension(extension)
                .unwrap_or_else(|| panic!("missing alias {extension}"));
            assert_eq!(spec.id.as_str(), id, "{extension}");
            assert_eq!(spec.capability, capability, "{extension}");
        }
        assert_eq!(
            registry
                .find_by_path(Path::new("rack.redfish.json"))
                .expect("facility path")
                .id
                .as_str(),
            "facility-metadata"
        );
        assert_eq!(
            registry
                .find_by_path(Path::new("modelDescription.xml"))
                .expect("FMI model description")
                .id
                .as_str(),
            "simulation-fmi-model-description"
        );
        for path in ["claude_desktop_config.json", "mcp_servers.json"] {
            assert_eq!(
                registry
                    .find_by_path(Path::new(path))
                    .map(|spec| spec.id.as_str()),
                Some("mcp-configuration"),
                "{path} must retain MCP redaction routing"
            );
        }
    }

    #[test]
    fn tar_dispatch_is_truthfully_partial() {
        let registry = format_registry();
        assert_eq!(
            registry.capability_for_extension("tar"),
            Some(FormatCapability::StructuralPartial)
        );
        assert_eq!(
            registry
                .find_by_extension("tar")
                .map(|spec| spec.id.as_str()),
            Some("tar-archive")
        );
        for extension in [
            "zip", "tar", "docx", "xlsx", "pptx", "odt", "epub", "vsdx", "ifczip", "bcfzip", "fmu",
            "usdz",
        ] {
            if extension == "tar" {
                continue;
            }
            assert_eq!(
                registry.capability_for_extension(extension),
                Some(FormatCapability::InventoryOnly),
                "{extension}"
            );
        }
        assert_eq!(
            registry
                .find_by_extension("zip")
                .map(|spec| spec.id.as_str()),
            Some("zip-archive")
        );
        assert_eq!(registry.classify_extension("usdz"), None);
        for extension in ["gz", "tgz", "bz2", "xz", "zst", "7z", "rar", "glb", "fmi"] {
            assert_eq!(
                registry.capability_for_extension(extension),
                Some(FormatCapability::InventoryOnly),
                "{extension}"
            );
        }
    }
}
