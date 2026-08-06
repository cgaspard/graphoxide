//! Conservative byte-only extraction for protocol descriptions and IDLs.
//!
//! This module deliberately accepts an already-admitted byte slice and never
//! probes, opens, or otherwise uses its [`Path`] argument as a filesystem
//! capability. Text parsers are bounded scanners rather than permissive value
//! decoders: they retain declarations and explicit relationships while avoiding
//! invented semantics for malformed input. Binary wire payloads are never
//! guessed. Without a verified schema binding they receive a deterministic
//! inventory record explaining why semantic extraction was refused.

use anyhow::Context as _;
use graphoxide_core::{make_id, sanitize_label, Confidence, Edge, Extraction, Node};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

/// Maximum source size accepted by the protocol scanner. Callers must enforce
/// their global byte budget before dispatch; this secondary limit keeps a
/// direct adapter call bounded as well.
pub(crate) const MAX_PROTOCOL_BYTES: usize = 16 * 1024 * 1024;
const MAX_DECLARATIONS: usize = 4_096;
const MAX_FIELDS: usize = 32_768;
const MAX_EDGES: usize = 65_536;
const MAX_NESTING: usize = 64;
const MAX_LINE_BYTES: usize = 16 * 1024;
const MAX_DESCRIPTOR_FILES: usize = 1_024;
const MAX_DESCRIPTOR_MESSAGES: usize = 4_096;
const MAX_DESCRIPTOR_FIELDS: usize = 32_768;
/// Columnar files can contain large record batches, but their self-describing
/// schema metadata is deliberately parsed under a much smaller independent
/// ceiling. The input ceiling matches the registry's binary admission limit;
/// no record batch or column page is ever decoded by this module.
const MAX_COLUMNAR_INPUT_BYTES: usize = 256 * 1024 * 1024;
const MAX_COLUMNAR_METADATA_BYTES: usize = 16 * 1024 * 1024;
const MAX_COLUMNAR_FIELDS: usize = 4_096;
const MAX_COLUMNAR_METADATA_ITEMS: usize = 4_096;
const MAX_COLUMNAR_NESTING: usize = 64;
const MAX_COLUMNAR_STRING_BYTES: usize = 16 * 1024;
const MAX_THRIFT_VALUES: usize = 131_072;

/// The family of an explicitly bound binary protocol instance.
///
/// A binary protocol value does not contain enough information to identify a
/// schema safely. Callers must construct a [`VerifiedBinarySchemaBinding`]
/// before asking Graphoxide to emit schema-derived facts for an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryProtocolKind {
    /// A Protocol Buffers wire-format message with a FileDescriptorSet binding.
    Protobuf,
    /// A FlatBuffers table with verified schema metadata.
    Flatbuffers,
}

/// A validated descriptor or schema binding for a binary protocol instance.
///
/// This type has no public constructor. Its constructors validate both the
/// descriptor/schema and the selected root message/table, preventing callers
/// from relabelling an arbitrary binary payload as schema-full extraction.
#[derive(Debug, Clone)]
pub struct VerifiedBinarySchemaBinding {
    kind: BinaryProtocolKind,
    schema_id: String,
    root_type: String,
    schema_fingerprint: String,
    data: VerifiedBindingData,
}

#[derive(Debug, Clone)]
enum VerifiedBindingData {
    Protobuf {
        fields: Vec<BoundProtobufField>,
    },
    Flatbuffers {
        schema_text: String,
        file_identifier: Option<[u8; 4]>,
    },
}

#[derive(Debug, Clone)]
struct BoundProtobufField {
    number: u32,
    name: String,
    declared_type: String,
    wire_types: Vec<u8>,
}

/// Errors emitted while validating an explicit binary schema binding.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SchemaBindingError {
    /// The caller supplied a blank, unsafe, or excessively long schema id.
    #[error("schema id must be non-empty printable text no longer than 256 bytes")]
    InvalidSchemaId,
    /// The selected message/table cannot be represented as a qualified name.
    #[error("root type must be a non-empty qualified identifier")]
    InvalidRootType,
    /// The descriptor/schema input exceeded the bounded parser limit.
    #[error("schema or descriptor exceeds {MAX_PROTOCOL_BYTES} byte limit")]
    OversizedSchema,
    /// The FileDescriptorSet is malformed or violates the bounded decoder's contract.
    #[error("invalid protobuf FileDescriptorSet: {0}")]
    InvalidProtobufDescriptor(&'static str),
    /// The selected protobuf message is not supplied by the descriptor.
    #[error("protobuf descriptor does not declare root message {0}")]
    MissingProtobufMessage(String),
    /// The FlatBuffers schema is malformed for the selected root table.
    #[error("invalid FlatBuffers schema metadata: {0}")]
    InvalidFlatbuffersSchema(&'static str),
    /// The schema's identifier and the caller's expected identifier disagree.
    #[error("FlatBuffers schema file identifier does not match expected metadata")]
    FlatbuffersIdentifierMismatch,
}

impl VerifiedBinarySchemaBinding {
    /// Validate a binary `google.protobuf.FileDescriptorSet` and bind one
    /// declared message type. `message_type` may be fully qualified (without
    /// a leading dot) or an unambiguous short message name.
    pub fn protobuf_descriptor(
        schema_id: impl AsRef<str>,
        descriptor_set: &[u8],
        message_type: impl AsRef<str>,
    ) -> Result<Self, SchemaBindingError> {
        let schema_id = checked_schema_id(schema_id.as_ref())?;
        let requested = checked_root_type(message_type.as_ref())?;
        if descriptor_set.len() > MAX_PROTOCOL_BYTES {
            return Err(SchemaBindingError::OversizedSchema);
        }
        let messages = parse_file_descriptor_set(descriptor_set)?;
        let mut selected = messages
            .iter()
            .filter(|message| qualified_name_matches(&requested, &message.name))
            .collect::<Vec<_>>();
        if selected.len() != 1 {
            return Err(SchemaBindingError::MissingProtobufMessage(requested));
        }
        let message = selected.pop().expect("exactly one selected message");
        Ok(Self {
            kind: BinaryProtocolKind::Protobuf,
            schema_id,
            root_type: message.name.clone(),
            schema_fingerprint: blake3::hash(descriptor_set).to_hex().to_string(),
            data: VerifiedBindingData::Protobuf {
                fields: message.fields.clone(),
            },
        })
    }

    /// Validate FlatBuffers text schema metadata and bind a table or struct.
    ///
    /// `expected_file_identifier` is optional because not every FlatBuffers
    /// schema declares one. When supplied, the schema must declare exactly the
    /// same four-byte identifier and each payload is checked against it.
    pub fn flatbuffers_schema_metadata(
        schema_id: impl AsRef<str>,
        schema: &[u8],
        root_type: impl AsRef<str>,
        expected_file_identifier: Option<[u8; 4]>,
    ) -> Result<Self, SchemaBindingError> {
        let schema_id = checked_schema_id(schema_id.as_ref())?;
        let root_type = checked_root_type(root_type.as_ref())?;
        if schema.len() > MAX_PROTOCOL_BYTES {
            return Err(SchemaBindingError::OversizedSchema);
        }
        let text = simdutf8::basic::from_utf8(schema)
            .or_else(|_| std::str::from_utf8(schema))
            .map_err(|_| SchemaBindingError::InvalidFlatbuffersSchema("schema is not UTF-8"))?;
        let declared = flatbuffers_declared_types(text);
        if !declared.iter().any(|name| name == &root_type) {
            return Err(SchemaBindingError::InvalidFlatbuffersSchema(
                "selected root table or struct is not declared",
            ));
        }
        let schema_identifier = flatbuffers_file_identifier(text)?;
        if expected_file_identifier.is_some() && schema_identifier != expected_file_identifier {
            return Err(SchemaBindingError::FlatbuffersIdentifierMismatch);
        }
        Ok(Self {
            kind: BinaryProtocolKind::Flatbuffers,
            schema_id,
            root_type,
            schema_fingerprint: blake3::hash(schema).to_hex().to_string(),
            data: VerifiedBindingData::Flatbuffers {
                schema_text: text.to_owned(),
                file_identifier: expected_file_identifier.or(schema_identifier),
            },
        })
    }

    /// Return the binary protocol family validated by this binding.
    pub const fn kind(&self) -> BinaryProtocolKind {
        self.kind
    }

    /// Stable caller-provided identity for the validated schema.
    pub fn schema_id(&self) -> &str {
        &self.schema_id
    }

    /// Fully qualified protobuf message or FlatBuffers root table/struct.
    pub fn root_type(&self) -> &str {
        &self.root_type
    }

    /// BLAKE3 fingerprint of exactly the descriptor/schema bytes validated.
    pub fn schema_fingerprint(&self) -> &str {
        &self.schema_fingerprint
    }
}

/// Structured protocol families recognized by the byte adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProtocolFormat {
    Protobuf,
    Flatbuffers,
    Thrift,
    Capnp,
    AvroSchema,
    AvroIdl,
    GraphQl,
    OpenApi,
    AsyncApi,
    Wit,
    Smithy,
    Cddl,
    Yang,
    Asn1,
}

impl ProtocolFormat {
    pub(crate) const fn id(self) -> &'static str {
        match self {
            Self::Protobuf => "protobuf",
            Self::Flatbuffers => "flatbuffers",
            Self::Thrift => "thrift",
            Self::Capnp => "capnp",
            Self::AvroSchema => "avro_schema",
            Self::AvroIdl => "avro_idl",
            Self::GraphQl => "graphql",
            Self::OpenApi => "openapi",
            Self::AsyncApi => "asyncapi",
            Self::Wit => "wit",
            Self::Smithy => "smithy",
            Self::Cddl => "cddl",
            Self::Yang => "yang",
            Self::Asn1 => "asn1",
        }
    }

    const fn capability(self) -> &'static str {
        // Every text-family implementation below is a bounded declaration or
        // subset scanner.  Even when the source is fully parsed as JSON first
        // (Avro and API descriptions), only selected schema/domain constructs
        // become facts, so these routes must not promise complete semantics.
        "structural_partial"
    }
}

/// Test-only registry projection for protocol adapter aliases. Runtime dispatch
/// reads [`crate::format_registry::FormatSpec::adapter`] directly so an alias
/// cannot silently diverge from the canonical capability contract.
#[cfg(test)]
pub(crate) fn supports_extension(extension: &str) -> bool {
    crate::format_registry::format_registry()
        .find_by_extension(extension)
        .is_some_and(|spec| spec.adapter() == crate::format_registry::ByteAdapterKind::Protocol)
}

/// Return whether a generic JSON or YAML buffer is an OpenAPI or AsyncAPI
/// document. The check reads only bounded leading structure and is deliberately
/// stricter than filename guessing.
pub(crate) fn looks_like_api_description(path: &Path, bytes: &[u8]) -> bool {
    let extension = extension(path);
    if !matches!(extension.as_str(), "json" | "yaml" | "yml") || bytes.len() > MAX_PROTOCOL_BYTES {
        return false;
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    if let Ok(value) = graphoxide_core::parse_jsonc(text)
        && let Some(object) = value.as_object()
    {
        return object.contains_key("openapi") || object.contains_key("asyncapi");
    }
    text.lines().take(128).any(|line| {
        let line = line.trim_start();
        line.starts_with("openapi:") || line.starts_with("asyncapi:")
    })
}

/// Extract protocol and IDL facts from a borrowed input buffer.
///
/// Text input is parsed only for an explicitly recognized source family. For
/// generic JSON/YAML, a document must contain the OpenAPI or AsyncAPI root
/// marker. Binary protocol values always produce inventory-only facts without
/// a verified descriptor. Arrow IPC and Parquet are the narrow exception:
/// their embedded, self-describing schema metadata is parsed without decoding
/// any data values, under independent metadata and record limits.
pub(crate) fn extract_protocol_bytes(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
) -> anyhow::Result<Extraction> {
    if let Some(format) = columnar_format(path, bytes) {
        if bytes.len() > MAX_COLUMNAR_INPUT_BYTES {
            return Ok(columnar_inventory_extraction(
                path,
                source_file,
                bytes.len(),
                format,
                "columnar_input_limit_exceeded",
            ));
        }
        return Ok(
            match extract_columnar_schema(path, source_file, bytes, format) {
                Ok(extraction) => extraction,
                Err(_) => columnar_inventory_extraction(
                    path,
                    source_file,
                    bytes.len(),
                    format,
                    if format == ColumnarFormat::ArrowIpc && bytes.starts_with(b"FEA1") {
                        "feather_v1_schema_metadata_unsupported"
                    } else {
                        "columnar_schema_metadata_invalid_or_unavailable"
                    },
                ),
            },
        );
    }
    anyhow::ensure!(
        bytes.len() <= MAX_PROTOCOL_BYTES,
        "protocol input exceeds {} byte extraction limit",
        MAX_PROTOCOL_BYTES
    );

    if is_binary_protocol(path, bytes) {
        return Ok(inventory_extraction(path, source_file, bytes.len()));
    }
    let Some(format) = format_for_text(path, bytes) else {
        return Ok(Extraction::default());
    };
    let text = crate::bytes::validate_utf8(bytes)
        .with_context(|| format!("protocol source {source_file} is not valid UTF-8"))?;

    let mut state = ProtocolState::new(path, source_file, format);
    match format {
        ProtocolFormat::OpenApi | ProtocolFormat::AsyncApi => {
            if let Ok(value) = graphoxide_core::parse_jsonc(text) {
                parse_api_json(&mut state, &value);
            } else {
                parse_api_yaml(&mut state, text);
            }
        }
        ProtocolFormat::AvroSchema => parse_avro_schema(&mut state, text),
        _ => parse_text_idl(&mut state, text),
    }
    Ok(state.finish())
}

/// Extract a binary protocol payload through an explicit verified binding.
///
/// Unlike [`extract_protocol_bytes`], this API is intentionally opt-in: a
/// payload is admitted as `schema_full` only after the supplied binding and
/// the payload's wire/layout invariants both validate. No values are rendered
/// into graph facts; doing so would require a complete descriptor-aware value
/// decoder. The emitted nodes instead describe the verified schema contract
/// that made the payload safe to classify.
pub fn extract_bound_binary_protocol_bytes(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
    binding: &VerifiedBinarySchemaBinding,
) -> anyhow::Result<Extraction> {
    anyhow::ensure!(
        bytes.len() <= MAX_PROTOCOL_BYTES,
        "protocol input exceeds {} byte extraction limit",
        MAX_PROTOCOL_BYTES
    );
    verify_path_family(path, binding.kind)?;
    match &binding.data {
        VerifiedBindingData::Protobuf { fields } => {
            verify_protobuf_payload(bytes, fields)?;
            Ok(bound_protobuf_extraction(
                path,
                source_file,
                bytes.len(),
                binding,
                fields,
            ))
        }
        VerifiedBindingData::Flatbuffers {
            schema_text,
            file_identifier,
        } => {
            verify_flatbuffers_payload(bytes, *file_identifier)?;
            Ok(bound_flatbuffers_extraction(
                path,
                source_file,
                bytes.len(),
                binding,
                schema_text,
            ))
        }
    }
}

/// Extract a binary protocol with an optional verified binding.
///
/// This is the non-fatal admission boundary for callers that process a mixed
/// corpus. An absent, malformed, or incompatible binding never permits a
/// schema-derived extraction: the payload is retained as a deterministic
/// inventory-only record instead. Use [`extract_bound_binary_protocol_bytes`]
/// when the caller needs the rejection reason as an error rather than a safe
/// inventory fallback.
pub fn extract_binary_protocol_with_binding_or_inventory(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
    binding: Option<&VerifiedBinarySchemaBinding>,
) -> anyhow::Result<Extraction> {
    anyhow::ensure!(
        bytes.len() <= MAX_PROTOCOL_BYTES,
        "protocol input exceeds {} byte extraction limit",
        MAX_PROTOCOL_BYTES
    );
    let Some(binding) = binding else {
        return Ok(inventory_extraction(path, source_file, bytes.len()));
    };
    match extract_bound_binary_protocol_bytes(path, source_file, bytes, binding) {
        Ok(extraction) => Ok(extraction),
        Err(_) => Ok(inventory_extraction_with_diagnostic(
            path,
            source_file,
            bytes.len(),
            "schema_binding_rejected_payload_not_decoded",
        )),
    }
}

fn checked_schema_id(value: &str) -> Result<String, SchemaBindingError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 256
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || !byte.is_ascii())
    {
        return Err(SchemaBindingError::InvalidSchemaId);
    }
    Ok(value.to_owned())
}

fn checked_root_type(value: &str) -> Result<String, SchemaBindingError> {
    let value = value.trim().trim_start_matches('.');
    if value.is_empty() || value.len() > 512 || !value.split('.').all(valid_name) {
        return Err(SchemaBindingError::InvalidRootType);
    }
    Ok(value.to_owned())
}

fn verify_path_family(path: &Path, binding_kind: BinaryProtocolKind) -> anyhow::Result<()> {
    let expected = match extension(path).as_str() {
        "pbf" | "pb" | "pb3" | "protobin" | "protobuf" | "desc" | "fds" => {
            Some(BinaryProtocolKind::Protobuf)
        }
        "bfbs" => Some(BinaryProtocolKind::Flatbuffers),
        _ => None,
    };
    if let Some(expected) = expected {
        anyhow::ensure!(
            expected == binding_kind,
            "binary payload extension is incompatible with supplied schema binding"
        );
    }
    Ok(())
}

fn bound_protobuf_extraction(
    path: &Path,
    source_file: &str,
    byte_len: usize,
    binding: &VerifiedBinarySchemaBinding,
    fields: &[BoundProtobufField],
) -> Extraction {
    if !crate::parser_budget::try_reserve_facts(3) {
        return inventory_extraction_with_diagnostic(
            path,
            source_file,
            byte_len,
            "parser_arena_fact_limit",
        );
    }
    let stem = normalized_stem(source_file);
    let file_id = make_id(&[&stem, "protobuf_binary", "file"]);
    let label = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(source_file);
    let mut file = protocol_node(
        file_id.clone(),
        label,
        source_file,
        1,
        "protocol_file",
        "protobuf_binary",
    );
    insert_bound_metadata(
        &mut file,
        binding,
        byte_len,
        "wire_syntax_and_declared_field_types",
    );

    let message_id = make_id(&[
        &stem,
        "protobuf_binary",
        "schema_message",
        binding.root_type(),
    ]);
    let mut message = protocol_node(
        message_id.clone(),
        binding.root_type(),
        source_file,
        1,
        "schema_message",
        "protobuf_binary",
    );
    message
        .extra
        .insert("schema_binding_id".into(), binding.schema_id().into());
    let mut nodes = vec![file, message];
    let mut edges = vec![protocol_edge(
        file_id,
        message_id.clone(),
        "contains",
        source_file,
        1,
    )];
    for field in fields {
        if !crate::parser_budget::try_reserve_facts(2) {
            break;
        }
        let field_id = make_id(&[
            &stem,
            "protobuf_binary",
            "schema_field",
            binding.root_type(),
            &field.number.to_string(),
            &field.name,
        ]);
        let mut node = protocol_node(
            field_id.clone(),
            &field.name,
            source_file,
            1,
            "schema_field",
            "protobuf_binary",
        );
        node.extra
            .insert("field_number".into(), field.number.into());
        node.extra
            .insert("declared_type".into(), field.declared_type.clone().into());
        node.extra
            .insert("schema_binding_id".into(), binding.schema_id().into());
        nodes.push(node);
        edges.push(protocol_edge(
            message_id.clone(),
            field_id,
            "contains",
            source_file,
            1,
        ));
    }
    Extraction {
        nodes,
        edges,
        hyperedges: Vec::new(),
    }
}

fn bound_flatbuffers_extraction(
    path: &Path,
    source_file: &str,
    byte_len: usize,
    binding: &VerifiedBinarySchemaBinding,
    schema_text: &str,
) -> Extraction {
    let mut state = ProtocolState::new(path, source_file, ProtocolFormat::Flatbuffers);
    parse_text_idl(&mut state, schema_text);
    let mut extraction = state.finish();
    if let Some(file) = extraction.nodes.first_mut() {
        insert_bound_metadata(file, binding, byte_len, "flatbuffer_table_layout");
    }
    for node in &mut extraction.nodes {
        node.extra
            .insert("schema_binding_id".into(), binding.schema_id().into());
    }
    extraction
}

fn insert_bound_metadata(
    node: &mut Node,
    binding: &VerifiedBinarySchemaBinding,
    byte_len: usize,
    payload_validation: &'static str,
) {
    node.extra
        .insert("format_capability".into(), "schema_full".into());
    node.extra
        .insert("schema_requirement".into(), "verified_binding".into());
    node.extra
        .insert("schema_binding_id".into(), binding.schema_id().into());
    node.extra.insert(
        "schema_fingerprint".into(),
        binding.schema_fingerprint().into(),
    );
    node.extra
        .insert("bound_root_type".into(), binding.root_type().into());
    node.extra
        .insert("payload_validation".into(), payload_validation.into());
    node.extra
        .insert("byte_length".into(), (byte_len as u64).into());
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn format_for_text(path: &Path, bytes: &[u8]) -> Option<ProtocolFormat> {
    let extension = extension(path);
    let format = match extension.as_str() {
        "proto" | "prototxt" => ProtocolFormat::Protobuf,
        "fbs" => ProtocolFormat::Flatbuffers,
        "thrift" => ProtocolFormat::Thrift,
        "capnp" => ProtocolFormat::Capnp,
        "avsc" => ProtocolFormat::AvroSchema,
        "avdl" => ProtocolFormat::AvroIdl,
        "graphql" | "gql" => ProtocolFormat::GraphQl,
        "wit" => ProtocolFormat::Wit,
        "smithy" => ProtocolFormat::Smithy,
        "cddl" => ProtocolFormat::Cddl,
        "yang" | "yin" => ProtocolFormat::Yang,
        "asn" | "asn1" | "asn1txt" => ProtocolFormat::Asn1,
        "openapi" => ProtocolFormat::OpenApi,
        "asyncapi" => ProtocolFormat::AsyncApi,
        "json" | "yaml" | "yml" if looks_like_api_description(path, bytes) => {
            let text = std::str::from_utf8(bytes).ok()?;
            if text
                .lines()
                .take(128)
                .any(|line| line.trim_start().starts_with("asyncapi:"))
                || graphoxide_core::parse_jsonc(text)
                    .ok()
                    .is_some_and(|value| value.get("asyncapi").is_some())
            {
                ProtocolFormat::AsyncApi
            } else {
                ProtocolFormat::OpenApi
            }
        }
        _ => return None,
    };
    Some(format)
}

fn is_binary_protocol(path: &Path, bytes: &[u8]) -> bool {
    let extension = extension(path);
    matches!(
        extension.as_str(),
        "bfbs"
            | "pbf"
            | "pb"
            | "pb3"
            | "protobin"
            | "protobuf"
            | "avro"
            | "arrow"
            | "arrows"
            | "feather"
            | "parquet"
            | "orc"
            | "ber"
            | "der"
            | "cer"
            | "pem"
            | "ipc"
            | "desc"
            | "fds"
    ) || (extension == "capnp" && std::str::from_utf8(bytes).is_err())
        || bytes.starts_with(b"Obj\x01")
        || bytes.starts_with(b"PAR1")
        || bytes.starts_with(b"ARROW1")
}

fn inventory_extraction(path: &Path, source_file: &str, byte_len: usize) -> Extraction {
    inventory_extraction_with_diagnostic(
        path,
        source_file,
        byte_len,
        "schema_required_binary_payload_not_decoded",
    )
}

fn inventory_extraction_with_diagnostic(
    path: &Path,
    source_file: &str,
    byte_len: usize,
    diagnostic: &'static str,
) -> Extraction {
    let format = match extension(path).as_str() {
        "bfbs" => "flatbuffers_binary_schema",
        "pbf" | "pb" | "pb3" | "protobin" | "protobuf" | "desc" | "fds" => "protobuf_binary",
        "capnp" => "capnp_binary",
        "avro" => "avro_object_container",
        "arrow" | "arrows" | "feather" | "ipc" => "arrow_ipc",
        "parquet" => "parquet",
        "orc" => "orc",
        _ => "binary_protocol",
    };
    let stem = normalized_stem(source_file);
    let id = make_id(&[&stem, "protocol_inventory"]);
    let label = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(source_file);
    let mut node = protocol_node(id, label, source_file, 1, "protocol_inventory", format);
    node.extra
        .insert("format_capability".into(), "inventory_only".into());
    node.extra.insert(
        "schema_requirement".into(),
        "verified_descriptor_or_schema".into(),
    );
    node.extra.insert("diagnostic".into(), diagnostic.into());
    node.extra
        .insert("byte_length".into(), (byte_len as u64).into());
    Extraction {
        nodes: vec![node],
        edges: Vec::new(),
        hyperedges: Vec::new(),
    }
}

/// A self-describing columnar representation that can expose its schema
/// without interpreting any stored record values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnarFormat {
    ArrowIpc,
    Parquet,
}

impl ColumnarFormat {
    const fn id(self) -> &'static str {
        match self {
            Self::ArrowIpc => "arrow_ipc",
            Self::Parquet => "parquet",
        }
    }

    const fn schema_source(self) -> &'static str {
        match self {
            Self::ArrowIpc => "arrow_ipc_schema_metadata",
            Self::Parquet => "parquet_file_metadata",
        }
    }
}

/// Classify only the columnar encodings whose embedded schema metadata is
/// presently understood. ORC remains on the binary inventory path rather than
/// inheriting a stronger capability from Arrow or Parquet.
fn columnar_format(path: &Path, bytes: &[u8]) -> Option<ColumnarFormat> {
    if bytes.starts_with(b"PAR1") {
        return Some(ColumnarFormat::Parquet);
    }
    if bytes.starts_with(b"ARROW1") {
        return Some(ColumnarFormat::ArrowIpc);
    }
    match extension(path).as_str() {
        "parquet" => Some(ColumnarFormat::Parquet),
        "arrow" | "arrows" | "feather" | "ipc" => Some(ColumnarFormat::ArrowIpc),
        _ => None,
    }
}

fn extract_columnar_schema(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
    format: ColumnarFormat,
) -> anyhow::Result<Extraction> {
    match format {
        ColumnarFormat::ArrowIpc => extract_arrow_schema(path, source_file, bytes),
        ColumnarFormat::Parquet => extract_parquet_schema(path, source_file, bytes),
    }
}

fn columnar_inventory_extraction(
    path: &Path,
    source_file: &str,
    byte_len: usize,
    format: ColumnarFormat,
    diagnostic: &'static str,
) -> Extraction {
    let mut extraction =
        inventory_extraction_with_diagnostic(path, source_file, byte_len, diagnostic);
    let root = extraction
        .nodes
        .first_mut()
        .expect("inventory extraction always contains a root node");
    root.extra
        .insert("protocol_format".into(), format.id().into());
    root.extra.insert(
        "schema_requirement".into(),
        "embedded_schema_metadata".into(),
    );
    extraction
}

/// A bounded schema-only graph builder shared by Arrow IPC and Parquet.
///
/// The builder deliberately records declarations, hierarchy, declared type,
/// nullability/repetition, and custom metadata *keys*. It never reads a body
/// buffer, a row group, a page, or any value payload.
struct ColumnarSchemaState<'a> {
    source_file: &'a str,
    stem: String,
    format: ColumnarFormat,
    schema_id: String,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    fields: usize,
    metadata_items: usize,
}

impl<'a> ColumnarSchemaState<'a> {
    fn new(
        path: &Path,
        source_file: &'a str,
        format: ColumnarFormat,
        byte_len: usize,
        metadata_len: usize,
    ) -> Self {
        // Root facts are admitted before any owned node or edge payload is
        // built. This is a retained-fact estimate, not allocator accounting.
        let initial_facts_admitted = crate::parser_budget::try_reserve_facts(3);
        if !initial_facts_admitted {
            return Self {
                source_file,
                stem: String::new(),
                format,
                schema_id: String::new(),
                nodes: Vec::new(),
                edges: Vec::new(),
                fields: 0,
                metadata_items: 0,
            };
        }
        let stem = normalized_stem(source_file);
        let file_id = make_id(&[&stem, format.id(), "file"]);
        let schema_id = make_id(&[&stem, format.id(), "schema"]);
        let schema_edge = protocol_edge(
            file_id.clone(),
            schema_id.clone(),
            "contains",
            source_file,
            1,
        );
        let label = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(source_file);
        let mut file = protocol_node(
            file_id.clone(),
            label,
            source_file,
            1,
            "columnar_schema_file",
            format.id(),
        );
        file.extra
            .insert("format_capability".into(), "structural_partial".into());
        file.extra.insert("parse_status".into(), "partial".into());
        file.extra.insert(
            "schema_requirement".into(),
            "embedded_schema_metadata".into(),
        );
        file.extra
            .insert("schema_source".into(), format.schema_source().into());
        file.extra
            .insert("data_values_decoded".into(), false.into());
        file.extra
            .insert("byte_length".into(), (byte_len as u64).into());
        file.extra
            .insert("schema_metadata_bytes".into(), (metadata_len as u64).into());

        let mut schema = protocol_node(
            schema_id.clone(),
            "schema",
            source_file,
            1,
            "schema",
            format.id(),
        );
        schema
            .extra
            .insert("schema_source".into(), format.schema_source().into());
        schema
            .extra
            .insert("data_values_decoded".into(), false.into());
        Self {
            source_file,
            stem,
            format,
            schema_id,
            nodes: vec![file, schema],
            edges: vec![schema_edge],
            fields: 0,
            metadata_items: 0,
        }
    }

    fn field(
        &mut self,
        parent: &str,
        name: &str,
        declared_type: &str,
        nullable: Option<bool>,
        repetition: Option<&str>,
        ordinal: usize,
    ) -> anyhow::Result<String> {
        anyhow::ensure!(
            self.fields < MAX_COLUMNAR_FIELDS,
            "columnar schema field limit exceeded"
        );
        anyhow::ensure!(
            !name.is_empty() && name.len() <= MAX_COLUMNAR_STRING_BYTES,
            "columnar field name is missing or too long"
        );
        anyhow::ensure!(
            !declared_type.is_empty() && declared_type.len() <= MAX_COLUMNAR_STRING_BYTES,
            "columnar field type is missing or too long"
        );
        anyhow::ensure!(
            crate::parser_budget::try_reserve_facts(2),
            "parser arena fact limit exceeded"
        );
        self.fields += 1;
        let id = make_id(&[
            &self.stem,
            self.format.id(),
            "schema_field",
            parent,
            &ordinal.to_string(),
            name,
        ]);
        let mut field = protocol_node(
            id.clone(),
            name,
            self.source_file,
            1,
            "schema_field",
            self.format.id(),
        );
        field
            .extra
            .insert("declared_type".into(), sanitize_label(declared_type).into());
        field
            .extra
            .insert("ordinal".into(), (ordinal as u64).into());
        if let Some(nullable) = nullable {
            field.extra.insert("nullable".into(), nullable.into());
        }
        if let Some(repetition) = repetition {
            field.extra.insert("repetition".into(), repetition.into());
        }
        self.nodes.push(field);
        self.edges.push(protocol_edge(
            parent.into(),
            id.clone(),
            "contains",
            self.source_file,
            1,
        ));
        Ok(id)
    }

    fn metadata_key(&mut self, key: &str, ordinal: usize) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.metadata_items < MAX_COLUMNAR_METADATA_ITEMS,
            "columnar schema metadata item limit exceeded"
        );
        anyhow::ensure!(
            !key.is_empty() && key.len() <= MAX_COLUMNAR_STRING_BYTES,
            "columnar schema metadata key is missing or too long"
        );
        anyhow::ensure!(
            crate::parser_budget::try_reserve_facts(2),
            "parser arena fact limit exceeded"
        );
        self.metadata_items += 1;
        let id = make_id(&[
            &self.stem,
            self.format.id(),
            "schema_metadata_key",
            &ordinal.to_string(),
            key,
        ]);
        let mut metadata = protocol_node(
            id.clone(),
            key,
            self.source_file,
            1,
            "schema_metadata_key",
            self.format.id(),
        );
        // Values can be arbitrary binary or contain secrets. Their presence
        // is represented without copying or surfacing the value itself.
        metadata
            .extra
            .insert("metadata_value_decoded".into(), false.into());
        self.nodes.push(metadata);
        self.edges.push(protocol_edge(
            self.schema_id.clone(),
            id,
            "metadata_key",
            self.source_file,
            1,
        ));
        Ok(())
    }

    fn finish(mut self) -> Extraction {
        if let Some(file) = self.nodes.first_mut() {
            file.extra
                .insert("schema_field_count".into(), (self.fields as u64).into());
            file.extra.insert(
                "schema_metadata_key_count".into(),
                (self.metadata_items as u64).into(),
            );
        }
        Extraction {
            nodes: self.nodes,
            edges: self.edges,
            hyperedges: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct FlatbufferTable<'a> {
    bytes: &'a [u8],
    table: usize,
    vtable: usize,
    vtable_len: usize,
    object_len: usize,
}

impl<'a> FlatbufferTable<'a> {
    fn root(bytes: &'a [u8]) -> anyhow::Result<Self> {
        let table = read_u32_at(bytes, 0)? as usize;
        anyhow::ensure!(table > 0, "flatbuffer root offset is zero");
        Self::at(bytes, table)
    }

    fn at(bytes: &'a [u8], table: usize) -> anyhow::Result<Self> {
        let vtable_offset = read_i32_at(bytes, table)?;
        anyhow::ensure!(
            vtable_offset > 0,
            "flatbuffer table vtable offset is invalid"
        );
        let vtable = table
            .checked_sub(vtable_offset as usize)
            .ok_or_else(|| anyhow::anyhow!("flatbuffer table vtable underflow"))?;
        let vtable_len = read_u16_at(bytes, vtable)? as usize;
        let object_len = read_u16_at(
            bytes,
            vtable
                .checked_add(2)
                .ok_or_else(|| anyhow::anyhow!("flatbuffer vtable offset overflow"))?,
        )? as usize;
        anyhow::ensure!(
            vtable_len >= 4 && vtable_len.is_multiple_of(2),
            "flatbuffer vtable length is invalid"
        );
        let vtable_end = vtable
            .checked_add(vtable_len)
            .ok_or_else(|| anyhow::anyhow!("flatbuffer vtable overflow"))?;
        let object_end = table
            .checked_add(object_len)
            .ok_or_else(|| anyhow::anyhow!("flatbuffer object overflow"))?;
        anyhow::ensure!(
            vtable_end <= bytes.len() && object_end <= bytes.len(),
            "flatbuffer table is truncated"
        );
        Ok(Self {
            bytes,
            table,
            vtable,
            vtable_len,
            object_len,
        })
    }

    fn field(&self, slot: usize, width: usize) -> anyhow::Result<Option<usize>> {
        let entry = self
            .vtable
            .checked_add(4)
            .and_then(|offset| offset.checked_add(slot.checked_mul(2)?))
            .ok_or_else(|| anyhow::anyhow!("flatbuffer field offset overflow"))?;
        if entry
            .checked_add(2)
            .is_none_or(|end| end > self.vtable + self.vtable_len)
        {
            return Ok(None);
        }
        let relative = read_u16_at(self.bytes, entry)? as usize;
        if relative == 0 {
            return Ok(None);
        }
        let field = self
            .table
            .checked_add(relative)
            .ok_or_else(|| anyhow::anyhow!("flatbuffer field position overflow"))?;
        let end = field
            .checked_add(width)
            .ok_or_else(|| anyhow::anyhow!("flatbuffer field width overflow"))?;
        anyhow::ensure!(
            end <= self.table + self.object_len,
            "flatbuffer field exceeds table object"
        );
        Ok(Some(field))
    }

    fn u8(&self, slot: usize) -> anyhow::Result<Option<u8>> {
        self.field(slot, 1)
            .map(|field| field.and_then(|offset| self.bytes.get(offset).copied()))
    }

    fn bool(&self, slot: usize) -> anyhow::Result<Option<bool>> {
        self.u8(slot).map(|value| value.map(|value| value != 0))
    }

    fn i32(&self, slot: usize) -> anyhow::Result<Option<i32>> {
        self.field(slot, 4).and_then(|field| {
            field
                .map(|offset| read_i32_at(self.bytes, offset))
                .transpose()
        })
    }

    fn indirect(&self, slot: usize) -> anyhow::Result<Option<usize>> {
        let Some(field) = self.field(slot, 4)? else {
            return Ok(None);
        };
        let relative = read_u32_at(self.bytes, field)? as usize;
        if relative == 0 {
            return Ok(None);
        }
        let target = field
            .checked_add(relative)
            .ok_or_else(|| anyhow::anyhow!("flatbuffer indirect offset overflow"))?;
        anyhow::ensure!(
            target < self.bytes.len(),
            "flatbuffer indirect target is truncated"
        );
        Ok(Some(target))
    }

    fn table(&self, slot: usize) -> anyhow::Result<Option<Self>> {
        self.indirect(slot)?
            .map(|target| Self::at(self.bytes, target))
            .transpose()
    }

    fn string(&self, slot: usize) -> anyhow::Result<Option<&'a str>> {
        let Some(target) = self.indirect(slot)? else {
            return Ok(None);
        };
        let len = read_u32_at(self.bytes, target)? as usize;
        anyhow::ensure!(
            len <= MAX_COLUMNAR_STRING_BYTES,
            "flatbuffer string exceeds columnar limit"
        );
        let start = target
            .checked_add(4)
            .ok_or_else(|| anyhow::anyhow!("flatbuffer string offset overflow"))?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| anyhow::anyhow!("flatbuffer string length overflow"))?;
        let bytes = self
            .bytes
            .get(start..end)
            .ok_or_else(|| anyhow::anyhow!("flatbuffer string is truncated"))?;
        let value = crate::bytes::validate_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("flatbuffer string is not UTF-8"))?;
        Ok(Some(value))
    }

    fn table_vector(&self, slot: usize) -> anyhow::Result<Option<FlatbufferTableVector<'a>>> {
        let Some(target) = self.indirect(slot)? else {
            return Ok(None);
        };
        let len = read_u32_at(self.bytes, target)? as usize;
        anyhow::ensure!(
            len <= MAX_COLUMNAR_FIELDS.max(MAX_COLUMNAR_METADATA_ITEMS),
            "flatbuffer vector exceeds columnar record limit"
        );
        let items = target
            .checked_add(4)
            .ok_or_else(|| anyhow::anyhow!("flatbuffer vector offset overflow"))?;
        let end = items
            .checked_add(
                len.checked_mul(4)
                    .ok_or_else(|| anyhow::anyhow!("flatbuffer vector length overflow"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("flatbuffer vector end overflow"))?;
        anyhow::ensure!(
            end <= self.bytes.len(),
            "flatbuffer table vector is truncated"
        );
        Ok(Some(FlatbufferTableVector {
            bytes: self.bytes,
            items,
            len,
        }))
    }
}

#[derive(Debug, Clone, Copy)]
struct FlatbufferTableVector<'a> {
    bytes: &'a [u8],
    items: usize,
    len: usize,
}

impl<'a> FlatbufferTableVector<'a> {
    fn get(&self, index: usize) -> anyhow::Result<FlatbufferTable<'a>> {
        anyhow::ensure!(index < self.len, "flatbuffer vector index out of bounds");
        let item = self
            .items
            .checked_add(
                index
                    .checked_mul(4)
                    .ok_or_else(|| anyhow::anyhow!("flatbuffer vector index overflow"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("flatbuffer vector item overflow"))?;
        let relative = read_u32_at(self.bytes, item)? as usize;
        anyhow::ensure!(relative > 0, "flatbuffer vector contains a null table");
        let table = item
            .checked_add(relative)
            .ok_or_else(|| anyhow::anyhow!("flatbuffer vector table offset overflow"))?;
        FlatbufferTable::at(self.bytes, table)
    }
}

fn read_u16_at(bytes: &[u8], offset: usize) -> anyhow::Result<u16> {
    let values = bytes
        .get(
            offset
                ..offset
                    .checked_add(2)
                    .ok_or_else(|| anyhow::anyhow!("offset overflow"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("truncated u16"))?;
    Ok(u16::from_le_bytes([values[0], values[1]]))
}

fn read_u32_at(bytes: &[u8], offset: usize) -> anyhow::Result<u32> {
    let values = bytes
        .get(
            offset
                ..offset
                    .checked_add(4)
                    .ok_or_else(|| anyhow::anyhow!("offset overflow"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("truncated u32"))?;
    Ok(u32::from_le_bytes([
        values[0], values[1], values[2], values[3],
    ]))
}

fn read_i32_at(bytes: &[u8], offset: usize) -> anyhow::Result<i32> {
    Ok(read_u32_at(bytes, offset)? as i32)
}

fn extract_arrow_schema(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
) -> anyhow::Result<Extraction> {
    let (schema, metadata_len, source) = arrow_schema_table(bytes)?;
    let mut state = ColumnarSchemaState::new(
        path,
        source_file,
        ColumnarFormat::ArrowIpc,
        bytes.len(),
        metadata_len,
    );
    let fields = schema
        .table_vector(1)?
        .ok_or_else(|| anyhow::anyhow!("Arrow schema has no fields vector"))?;
    anyhow::ensure!(
        fields.len <= MAX_COLUMNAR_FIELDS,
        "Arrow schema field limit exceeded"
    );

    let mut pending = Vec::with_capacity(fields.len);
    for index in (0..fields.len).rev() {
        pending.push((fields.get(index)?, state.schema_id.clone(), 0_usize, index));
    }
    let mut next_ordinal = 0_usize;
    while let Some((field, parent, depth, source_ordinal)) = pending.pop() {
        anyhow::ensure!(
            depth <= MAX_COLUMNAR_NESTING,
            "Arrow schema nesting limit exceeded"
        );
        let name = field
            .string(0)?
            .filter(|name| !name.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Arrow field name is missing"))?;
        let type_tag = field
            .u8(2)?
            .ok_or_else(|| anyhow::anyhow!("Arrow field type discriminator is missing"))?;
        let declared_type = arrow_declared_type(field, type_tag)?;
        let id = state.field(
            &parent,
            name,
            &declared_type,
            field.bool(1)?,
            None,
            next_ordinal,
        )?;
        let node = state
            .nodes
            .last_mut()
            .expect("field insertion always appends a node");
        node.extra.insert("arrow_type_tag".into(), type_tag.into());
        node.extra.insert(
            "arrow_source_ordinal".into(),
            (source_ordinal as u64).into(),
        );
        next_ordinal += 1;

        if let Some(children) = field.table_vector(5)? {
            anyhow::ensure!(
                children.len <= MAX_COLUMNAR_FIELDS,
                "Arrow field child vector exceeds field limit"
            );
            anyhow::ensure!(
                pending
                    .len()
                    .checked_add(children.len)
                    .and_then(|pending| pending.checked_add(state.fields))
                    .is_some_and(|pending| pending <= MAX_COLUMNAR_FIELDS),
                "Arrow schema exceeds total field limit"
            );
            for index in (0..children.len).rev() {
                pending.push((children.get(index)?, id.clone(), depth + 1, index));
            }
        }
    }

    if let Some(metadata) = schema.table_vector(2)? {
        anyhow::ensure!(
            metadata.len <= MAX_COLUMNAR_METADATA_ITEMS,
            "Arrow schema custom metadata limit exceeded"
        );
        for index in 0..metadata.len {
            let entry = metadata.get(index)?;
            let key = entry
                .string(0)?
                .filter(|key| !key.is_empty())
                .ok_or_else(|| anyhow::anyhow!("Arrow schema metadata key is missing"))?;
            // Access the value only as a FlatBuffer string boundary check; it
            // is intentionally not copied into graph facts.
            let _ = entry.string(1)?;
            state.metadata_key(key, index)?;
        }
    }
    let mut extraction = state.finish();
    let root = extraction
        .nodes
        .first_mut()
        .expect("columnar schema extraction has a root node");
    root.extra.insert("arrow_layout".into(), source.into());
    Ok(extraction)
}

/// Return the Arrow `Schema` table from either the IPC file footer or the
/// first IPC stream message. Feather v2 is an Arrow IPC file and therefore
/// follows the same path. Feather v1's legacy footer is intentionally not
/// guessed as an Arrow schema and falls back to inventory.
fn arrow_schema_table(bytes: &[u8]) -> anyhow::Result<(FlatbufferTable<'_>, usize, &'static str)> {
    if bytes.starts_with(b"ARROW1") {
        anyhow::ensure!(
            bytes.len() >= 10 && bytes.ends_with(b"ARROW1"),
            "Arrow IPC file magic is missing or truncated"
        );
        let footer_len_offset = bytes.len() - 10;
        let footer_len = read_u32_at(bytes, footer_len_offset)? as usize;
        anyhow::ensure!(
            footer_len > 0 && footer_len <= MAX_COLUMNAR_METADATA_BYTES,
            "Arrow IPC footer exceeds metadata limit"
        );
        let footer_start = footer_len_offset
            .checked_sub(footer_len)
            .ok_or_else(|| anyhow::anyhow!("Arrow IPC footer underflows file"))?;
        anyhow::ensure!(footer_start >= 8, "Arrow IPC footer overlaps file header");
        let footer = bytes
            .get(footer_start..footer_len_offset)
            .ok_or_else(|| anyhow::anyhow!("Arrow IPC footer is truncated"))?;
        let footer = FlatbufferTable::root(footer)?;
        let schema = footer
            .table(1)?
            .ok_or_else(|| anyhow::anyhow!("Arrow IPC footer has no schema"))?;
        return Ok((schema, footer_len, "ipc_file_footer"));
    }

    let (metadata_start, metadata_len): (usize, usize) =
        if bytes.starts_with(&0xffff_ffff_u32.to_le_bytes()) {
            anyhow::ensure!(
                bytes.len() >= 8,
                "Arrow IPC stream continuation is truncated"
            );
            (8, read_u32_at(bytes, 4)? as usize)
        } else {
            anyhow::ensure!(bytes.len() >= 4, "Arrow IPC stream length is truncated");
            (4, read_u32_at(bytes, 0)? as usize)
        };
    anyhow::ensure!(
        metadata_len > 0 && metadata_len <= MAX_COLUMNAR_METADATA_BYTES,
        "Arrow IPC stream schema metadata exceeds limit"
    );
    let metadata_end = metadata_start
        .checked_add(metadata_len)
        .ok_or_else(|| anyhow::anyhow!("Arrow IPC stream metadata length overflow"))?;
    let message = FlatbufferTable::root(
        bytes
            .get(metadata_start..metadata_end)
            .ok_or_else(|| anyhow::anyhow!("Arrow IPC stream schema metadata is truncated"))?,
    )?;
    // `MessageHeader::Schema` is discriminant 1. The direct header table is
    // the schema itself, not a record batch and not its body.
    anyhow::ensure!(
        message.u8(1)? == Some(1),
        "Arrow IPC stream first message is not a schema"
    );
    let schema = message
        .table(2)?
        .ok_or_else(|| anyhow::anyhow!("Arrow IPC stream schema header is missing"))?;
    Ok((schema, metadata_len, "ipc_stream_message"))
}

fn arrow_declared_type(field: FlatbufferTable<'_>, tag: u8) -> anyhow::Result<String> {
    let name = match tag {
        1 => "null".to_owned(),
        2 => {
            let int = field
                .table(3)?
                .ok_or_else(|| anyhow::anyhow!("Arrow int field has no type table"))?;
            let width = int
                .i32(0)?
                .ok_or_else(|| anyhow::anyhow!("Arrow int field has no bit width"))?;
            anyhow::ensure!(
                matches!(width, 8 | 16 | 32 | 64),
                "Arrow int bit width is invalid"
            );
            let signed = int
                .bool(1)?
                .ok_or_else(|| anyhow::anyhow!("Arrow int field has no signedness"))?;
            format!("{}int{width}", if signed { "" } else { "u" })
        }
        3 => {
            let floating = field
                .table(3)?
                .ok_or_else(|| anyhow::anyhow!("Arrow floating field has no type table"))?;
            match floating.u8(0)?.unwrap_or(2) {
                0 => "float16".to_owned(),
                1 => "float32".to_owned(),
                2 => "float64".to_owned(),
                _ => anyhow::bail!("Arrow floating point precision is invalid"),
            }
        }
        4 => "binary".to_owned(),
        5 => "utf8".to_owned(),
        6 => "bool".to_owned(),
        7 => "decimal".to_owned(),
        8 => "date".to_owned(),
        9 => "time".to_owned(),
        10 => "timestamp".to_owned(),
        11 => "interval".to_owned(),
        12 => "list".to_owned(),
        13 => "struct".to_owned(),
        14 => "union".to_owned(),
        15 => "fixed_size_binary".to_owned(),
        16 => "fixed_size_list".to_owned(),
        17 => "map".to_owned(),
        18 => "duration".to_owned(),
        19 => "large_binary".to_owned(),
        20 => "large_utf8".to_owned(),
        21 => "large_list".to_owned(),
        22 => "run_end_encoded".to_owned(),
        23 => "binary_view".to_owned(),
        24 => "utf8_view".to_owned(),
        25 => "list_view".to_owned(),
        26 => "large_list_view".to_owned(),
        _ => format!("arrow_extension_type_{tag}"),
    };
    Ok(name)
}

#[derive(Debug, Default)]
struct ParquetFileMetadata {
    schema: Vec<ParquetSchemaElement>,
    custom_metadata_keys: Vec<String>,
}

#[derive(Debug, Default)]
struct ParquetSchemaElement {
    name: String,
    physical_type: Option<i32>,
    repetition_type: Option<i32>,
    num_children: usize,
    converted_type: Option<i32>,
    logical_type: Option<&'static str>,
}

fn extract_parquet_schema(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
) -> anyhow::Result<Extraction> {
    anyhow::ensure!(
        bytes.starts_with(b"PAR1") && bytes.ends_with(b"PAR1"),
        "Parquet file magic is missing or truncated"
    );
    anyhow::ensure!(
        bytes.len() >= 12,
        "Parquet file is too short for footer metadata"
    );
    let metadata_len_offset = bytes.len() - 8;
    let metadata_len = read_u32_at(bytes, metadata_len_offset)? as usize;
    anyhow::ensure!(
        metadata_len > 0 && metadata_len <= MAX_COLUMNAR_METADATA_BYTES,
        "Parquet footer metadata exceeds limit"
    );
    let metadata_start = metadata_len_offset
        .checked_sub(metadata_len)
        .ok_or_else(|| anyhow::anyhow!("Parquet footer metadata underflows file"))?;
    anyhow::ensure!(
        metadata_start >= 4,
        "Parquet footer metadata overlaps file magic"
    );
    let metadata = bytes
        .get(metadata_start..metadata_len_offset)
        .ok_or_else(|| anyhow::anyhow!("Parquet footer metadata is truncated"))?;
    let parsed = parse_parquet_file_metadata(metadata)?;
    anyhow::ensure!(
        !parsed.schema.is_empty(),
        "Parquet metadata has no schema elements"
    );

    let mut state = ColumnarSchemaState::new(
        path,
        source_file,
        ColumnarFormat::Parquet,
        bytes.len(),
        metadata_len,
    );
    let schema_id = state.schema_id.clone();
    let mut parents = vec![(schema_id, usize::MAX)];
    for (ordinal, element) in parsed.schema.iter().enumerate() {
        while parents.last().is_some_and(|(_, remaining)| *remaining == 0) {
            parents.pop();
        }
        let (parent, remaining) = parents
            .last_mut()
            .ok_or_else(|| anyhow::anyhow!("Parquet schema hierarchy has too many children"))?;
        if *remaining != usize::MAX {
            *remaining = remaining
                .checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("Parquet schema child count underflow"))?;
        }
        let declared_type = parquet_declared_type(element)?;
        let repetition = element
            .repetition_type
            .map(parquet_repetition_type)
            .transpose()?;
        let field_id = state.field(
            parent,
            &element.name,
            &declared_type,
            repetition.map(|value| value == "optional"),
            repetition,
            ordinal,
        )?;
        if let Some(node) = state.nodes.last_mut() {
            if let Some(physical) = element.physical_type {
                node.extra.insert(
                    "parquet_physical_type".into(),
                    parquet_physical_type(physical)?.into(),
                );
            }
            if let Some(converted) = element.converted_type {
                node.extra.insert(
                    "parquet_converted_type".into(),
                    parquet_converted_type(converted)?.into(),
                );
            }
            if let Some(logical) = element.logical_type {
                node.extra
                    .insert("parquet_logical_type".into(), logical.into());
            }
        }
        if element.num_children > 0 {
            anyhow::ensure!(
                element.num_children <= MAX_COLUMNAR_FIELDS,
                "Parquet schema child count exceeds limit"
            );
            parents.push((field_id, element.num_children));
        }
    }
    anyhow::ensure!(
        parents.iter().skip(1).all(|(_, remaining)| *remaining == 0),
        "Parquet schema hierarchy is incomplete"
    );
    for (ordinal, key) in parsed.custom_metadata_keys.iter().enumerate() {
        state.metadata_key(key, ordinal)?;
    }
    Ok(state.finish())
}

fn parquet_declared_type(element: &ParquetSchemaElement) -> anyhow::Result<String> {
    let physical = element
        .physical_type
        .map(parquet_physical_type)
        .transpose()?
        .unwrap_or("group");
    let logical = element.logical_type.or_else(|| {
        element
            .converted_type
            .and_then(|value| parquet_converted_type(value).ok())
    });
    Ok(logical
        .map(|logical| format!("{logical} ({physical})"))
        .unwrap_or_else(|| physical.to_owned()))
}

fn parquet_physical_type(value: i32) -> anyhow::Result<&'static str> {
    match value {
        0 => Ok("boolean"),
        1 => Ok("int32"),
        2 => Ok("int64"),
        3 => Ok("int96"),
        4 => Ok("float"),
        5 => Ok("double"),
        6 => Ok("byte_array"),
        7 => Ok("fixed_len_byte_array"),
        _ => anyhow::bail!("Parquet physical type is invalid"),
    }
}

fn parquet_repetition_type(value: i32) -> anyhow::Result<&'static str> {
    match value {
        0 => Ok("required"),
        1 => Ok("optional"),
        2 => Ok("repeated"),
        _ => anyhow::bail!("Parquet repetition type is invalid"),
    }
}

fn parquet_converted_type(value: i32) -> anyhow::Result<&'static str> {
    match value {
        0 => Ok("utf8"),
        1 => Ok("map"),
        2 => Ok("map_key_value"),
        3 => Ok("list"),
        4 => Ok("enum"),
        5 => Ok("decimal"),
        6 => Ok("date"),
        7 => Ok("time_millis"),
        8 => Ok("time_micros"),
        9 => Ok("timestamp_millis"),
        10 => Ok("timestamp_micros"),
        11 => Ok("uint_8"),
        12 => Ok("uint_16"),
        13 => Ok("uint_32"),
        14 => Ok("uint_64"),
        15 => Ok("int_8"),
        16 => Ok("int_16"),
        17 => Ok("int_32"),
        18 => Ok("int_64"),
        19 => Ok("json"),
        20 => Ok("bson"),
        21 => Ok("interval"),
        _ => anyhow::bail!("Parquet converted type is invalid"),
    }
}

fn parse_parquet_file_metadata(bytes: &[u8]) -> anyhow::Result<ParquetFileMetadata> {
    let mut reader = CompactThriftReader::new(bytes);
    let mut last_field = 0_i16;
    let mut result = ParquetFileMetadata::default();
    let mut has_schema = false;
    while let Some(field) = reader.next_field(&mut last_field)? {
        match (field.id, field.kind) {
            (2, CompactType::List) => {
                anyhow::ensure!(!has_schema, "Parquet metadata repeats schema field");
                result.schema = parse_parquet_schema_list(&mut reader, 1)?;
                has_schema = true;
            }
            (5, CompactType::List) => {
                result.custom_metadata_keys = parse_parquet_key_values(&mut reader, 1)?;
            }
            _ => reader.skip(field.kind, 1)?,
        }
    }
    anyhow::ensure!(reader.is_exhausted(), "Parquet metadata has trailing bytes");
    anyhow::ensure!(has_schema, "Parquet metadata has no schema field");
    Ok(result)
}

fn parse_parquet_schema_list(
    reader: &mut CompactThriftReader<'_>,
    depth: usize,
) -> anyhow::Result<Vec<ParquetSchemaElement>> {
    let (kind, len) = reader.list_header()?;
    anyhow::ensure!(
        kind == CompactType::Struct,
        "Parquet schema list has non-struct entries"
    );
    anyhow::ensure!(
        len > 0 && len <= MAX_COLUMNAR_FIELDS,
        "Parquet schema list exceeds field limit"
    );
    let mut schema = Vec::with_capacity(len);
    for _ in 0..len {
        schema.push(parse_parquet_schema_element(reader, depth + 1)?);
    }
    Ok(schema)
}

fn parse_parquet_schema_element(
    reader: &mut CompactThriftReader<'_>,
    depth: usize,
) -> anyhow::Result<ParquetSchemaElement> {
    anyhow::ensure!(
        depth <= MAX_COLUMNAR_NESTING,
        "Parquet schema nesting limit exceeded"
    );
    let mut last_field = 0_i16;
    let mut element = ParquetSchemaElement::default();
    while let Some(field) = reader.next_field(&mut last_field)? {
        match field.id {
            1 => {
                anyhow::ensure!(
                    field.kind == CompactType::I32,
                    "Parquet physical type has invalid encoding"
                );
                element.physical_type = Some(reader.i32()?);
            }
            3 => {
                anyhow::ensure!(
                    field.kind == CompactType::I32,
                    "Parquet repetition type has invalid encoding"
                );
                element.repetition_type = Some(reader.i32()?);
            }
            4 => {
                anyhow::ensure!(
                    field.kind == CompactType::Binary,
                    "Parquet field name has invalid encoding"
                );
                element.name = reader.utf8_string("Parquet field name")?;
            }
            5 => {
                anyhow::ensure!(
                    field.kind == CompactType::I32,
                    "Parquet child count has invalid encoding"
                );
                let count = reader.i32()?;
                anyhow::ensure!(count >= 0, "Parquet child count is negative");
                element.num_children = count as usize;
            }
            6 => {
                anyhow::ensure!(
                    field.kind == CompactType::I32,
                    "Parquet converted type has invalid encoding"
                );
                element.converted_type = Some(reader.i32()?);
            }
            10 => {
                anyhow::ensure!(
                    field.kind == CompactType::Struct,
                    "Parquet logical type has invalid encoding"
                );
                element.logical_type = Some(parse_parquet_logical_type(reader, depth + 1)?);
            }
            _ => reader.skip(field.kind, depth + 1)?,
        }
    }
    anyhow::ensure!(
        !element.name.is_empty(),
        "Parquet schema element name is missing"
    );
    anyhow::ensure!(
        element.name.len() <= MAX_COLUMNAR_STRING_BYTES,
        "Parquet schema element name exceeds limit"
    );
    Ok(element)
}

fn parse_parquet_logical_type(
    reader: &mut CompactThriftReader<'_>,
    depth: usize,
) -> anyhow::Result<&'static str> {
    anyhow::ensure!(
        depth <= MAX_COLUMNAR_NESTING,
        "Parquet logical type nesting exceeds limit"
    );
    let mut last_field = 0_i16;
    let mut logical = None;
    while let Some(field) = reader.next_field(&mut last_field)? {
        let name = match field.id {
            1 => "string",
            2 => "map",
            3 => "list",
            4 => "enum",
            5 => "decimal",
            6 => "date",
            7 => "time",
            8 => "timestamp",
            9 => "integer",
            10 => "unknown",
            11 => "json",
            12 => "bson",
            13 => "uuid",
            14 => "float16",
            15 => "null",
            _ => "extension",
        };
        if logical.replace(name).is_some() {
            anyhow::bail!("Parquet logical type union has multiple variants");
        }
        reader.skip(field.kind, depth + 1)?;
    }
    logical.ok_or_else(|| anyhow::anyhow!("Parquet logical type union is empty"))
}

fn parse_parquet_key_values(
    reader: &mut CompactThriftReader<'_>,
    depth: usize,
) -> anyhow::Result<Vec<String>> {
    let (kind, len) = reader.list_header()?;
    anyhow::ensure!(
        kind == CompactType::Struct,
        "Parquet metadata list has non-struct entries"
    );
    anyhow::ensure!(
        len <= MAX_COLUMNAR_METADATA_ITEMS,
        "Parquet metadata list exceeds limit"
    );
    let mut keys = Vec::with_capacity(len);
    for _ in 0..len {
        let mut last_field = 0_i16;
        let mut key = None;
        while let Some(field) = reader.next_field(&mut last_field)? {
            if field.id == 1 {
                anyhow::ensure!(
                    field.kind == CompactType::Binary,
                    "Parquet metadata key has invalid encoding"
                );
                key = Some(reader.utf8_string("Parquet metadata key")?);
            } else {
                reader.skip(field.kind, depth + 1)?;
            }
        }
        if let Some(key) = key.filter(|key| !key.is_empty()) {
            keys.push(key);
        }
    }
    Ok(keys)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactType {
    Stop,
    BoolTrue,
    BoolFalse,
    Byte,
    I16,
    I32,
    I64,
    Double,
    Binary,
    List,
    Set,
    Map,
    Struct,
    Uuid,
}

impl TryFrom<u8> for CompactType {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Stop),
            1 => Ok(Self::BoolTrue),
            2 => Ok(Self::BoolFalse),
            3 => Ok(Self::Byte),
            4 => Ok(Self::I16),
            5 => Ok(Self::I32),
            6 => Ok(Self::I64),
            7 => Ok(Self::Double),
            8 => Ok(Self::Binary),
            9 => Ok(Self::List),
            10 => Ok(Self::Set),
            11 => Ok(Self::Map),
            12 => Ok(Self::Struct),
            13 => Ok(Self::Uuid),
            _ => anyhow::bail!("unsupported Thrift compact type"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct CompactField {
    id: i16,
    kind: CompactType,
}

struct CompactThriftReader<'a> {
    bytes: &'a [u8],
    position: usize,
    values: usize,
}

impl<'a> CompactThriftReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            position: 0,
            values: 0,
        }
    }

    fn is_exhausted(&self) -> bool {
        self.position == self.bytes.len()
    }

    fn next_field(&mut self, last_field: &mut i16) -> anyhow::Result<Option<CompactField>> {
        let header = self.byte()?;
        let kind = CompactType::try_from(header & 0x0f)?;
        if kind == CompactType::Stop {
            return Ok(None);
        }
        let delta = (header >> 4) as i16;
        let id = if delta == 0 {
            self.i16()?
        } else {
            last_field
                .checked_add(delta)
                .ok_or_else(|| anyhow::anyhow!("Thrift compact field id overflow"))?
        };
        anyhow::ensure!(id > 0, "Thrift compact field id must be positive");
        *last_field = id;
        Ok(Some(CompactField { id, kind }))
    }

    fn byte(&mut self) -> anyhow::Result<u8> {
        self.bump()?;
        let byte = *self
            .bytes
            .get(self.position)
            .ok_or_else(|| anyhow::anyhow!("truncated Thrift compact value"))?;
        self.position += 1;
        Ok(byte)
    }

    fn bump(&mut self) -> anyhow::Result<()> {
        self.values += 1;
        anyhow::ensure!(
            self.values <= MAX_THRIFT_VALUES,
            "Thrift compact value limit exceeded"
        );
        Ok(())
    }

    fn varint(&mut self) -> anyhow::Result<u64> {
        let mut result = 0_u64;
        for shift in (0..64).step_by(7) {
            let byte = self.byte()?;
            if shift == 63 && byte & 0x7e != 0 {
                anyhow::bail!("Thrift compact varint overflows u64");
            }
            result |= ((byte & 0x7f) as u64)
                .checked_shl(shift as u32)
                .ok_or_else(|| anyhow::anyhow!("Thrift compact varint overflow"))?;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
        }
        anyhow::bail!("Thrift compact varint is too long")
    }

    fn i16(&mut self) -> anyhow::Result<i16> {
        let value = self.varint()?;
        anyhow::ensure!(value <= u16::MAX as u64, "Thrift compact i16 overflows");
        Ok(((value >> 1) as i16) ^ -((value & 1) as i16))
    }

    fn i32(&mut self) -> anyhow::Result<i32> {
        let value = self.varint()?;
        anyhow::ensure!(value <= u32::MAX as u64, "Thrift compact i32 overflows");
        Ok(((value >> 1) as i32) ^ -((value & 1) as i32))
    }

    fn binary(&mut self) -> anyhow::Result<&'a [u8]> {
        let len = usize::try_from(self.varint()?)
            .map_err(|_| anyhow::anyhow!("Thrift compact binary length overflows usize"))?;
        let end = self
            .position
            .checked_add(len)
            .ok_or_else(|| anyhow::anyhow!("Thrift compact binary length overflow"))?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| anyhow::anyhow!("truncated Thrift compact binary"))?;
        self.position = end;
        Ok(value)
    }

    fn utf8_string(&mut self, context: &str) -> anyhow::Result<String> {
        let bytes = self.binary()?;
        anyhow::ensure!(
            bytes.len() <= MAX_COLUMNAR_STRING_BYTES,
            "{context} exceeds string limit"
        );
        Ok(crate::bytes::validate_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("{context} is not UTF-8"))?
            .to_owned())
    }

    fn list_header(&mut self) -> anyhow::Result<(CompactType, usize)> {
        let header = self.byte()?;
        let compact_len = (header >> 4) as usize;
        let len = if compact_len == 15 {
            usize::try_from(self.varint()?)
                .map_err(|_| anyhow::anyhow!("Thrift compact list length overflows usize"))?
        } else {
            compact_len
        };
        let kind = CompactType::try_from(header & 0x0f)?;
        anyhow::ensure!(
            kind != CompactType::Stop,
            "Thrift compact list element type is STOP"
        );
        Ok((kind, len))
    }

    fn skip(&mut self, kind: CompactType, depth: usize) -> anyhow::Result<()> {
        anyhow::ensure!(
            depth <= MAX_COLUMNAR_NESTING,
            "Thrift compact nesting limit exceeded"
        );
        match kind {
            CompactType::Stop => anyhow::bail!("Thrift compact STOP cannot be a field value"),
            CompactType::BoolTrue | CompactType::BoolFalse => Ok(()),
            CompactType::Byte => {
                let _ = self.byte()?;
                Ok(())
            }
            CompactType::I16 | CompactType::I32 | CompactType::I64 => {
                let _ = self.varint()?;
                Ok(())
            }
            CompactType::Double => {
                let end = self
                    .position
                    .checked_add(8)
                    .ok_or_else(|| anyhow::anyhow!("Thrift compact double length overflow"))?;
                anyhow::ensure!(end <= self.bytes.len(), "truncated Thrift compact double");
                self.position = end;
                Ok(())
            }
            CompactType::Binary => {
                let _ = self.binary()?;
                Ok(())
            }
            CompactType::List | CompactType::Set => {
                let (item_kind, len) = self.list_header()?;
                for _ in 0..len {
                    self.skip(item_kind, depth + 1)?;
                }
                Ok(())
            }
            CompactType::Map => {
                let len = usize::try_from(self.varint()?)
                    .map_err(|_| anyhow::anyhow!("Thrift compact map length overflows usize"))?;
                if len == 0 {
                    return Ok(());
                }
                let types = self.byte()?;
                let key_kind = CompactType::try_from(types >> 4)?;
                let value_kind = CompactType::try_from(types & 0x0f)?;
                for _ in 0..len {
                    self.skip(key_kind, depth + 1)?;
                    self.skip(value_kind, depth + 1)?;
                }
                Ok(())
            }
            CompactType::Struct => {
                let mut last_field = 0_i16;
                while let Some(field) = self.next_field(&mut last_field)? {
                    self.skip(field.kind, depth + 1)?;
                }
                Ok(())
            }
            CompactType::Uuid => {
                let end = self
                    .position
                    .checked_add(16)
                    .ok_or_else(|| anyhow::anyhow!("Thrift compact UUID length overflow"))?;
                anyhow::ensure!(end <= self.bytes.len(), "truncated Thrift compact UUID");
                self.position = end;
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone)]
struct BoundProtobufMessage {
    name: String,
    fields: Vec<BoundProtobufField>,
}

#[derive(Debug, Clone, Copy)]
struct WireField<'a> {
    number: u32,
    wire_type: u8,
    value: &'a [u8],
}

fn parse_file_descriptor_set(
    descriptor_set: &[u8],
) -> Result<Vec<BoundProtobufMessage>, SchemaBindingError> {
    let fields =
        parse_wire_fields(descriptor_set).map_err(SchemaBindingError::InvalidProtobufDescriptor)?;
    let mut messages = Vec::new();
    let mut files = 0usize;
    for field in fields {
        if field.number != 1 {
            continue;
        }
        if field.wire_type != 2 {
            return Err(SchemaBindingError::InvalidProtobufDescriptor(
                "FileDescriptorSet file field has invalid wire type",
            ));
        }
        files += 1;
        if files > MAX_DESCRIPTOR_FILES {
            return Err(SchemaBindingError::InvalidProtobufDescriptor(
                "too many files",
            ));
        }
        parse_file_descriptor_proto(field.value, &mut messages)?;
        if messages.len() > MAX_DESCRIPTOR_MESSAGES {
            return Err(SchemaBindingError::InvalidProtobufDescriptor(
                "too many messages",
            ));
        }
    }
    if files == 0 {
        return Err(SchemaBindingError::InvalidProtobufDescriptor(
            "descriptor set has no files",
        ));
    }
    Ok(messages)
}

fn parse_file_descriptor_proto(
    bytes: &[u8],
    messages: &mut Vec<BoundProtobufMessage>,
) -> Result<(), SchemaBindingError> {
    let fields = parse_wire_fields(bytes).map_err(SchemaBindingError::InvalidProtobufDescriptor)?;
    let package = fields
        .iter()
        .find(|field| field.number == 2)
        .map(|field| wire_utf8(*field, "package is not UTF-8"))
        .transpose()?
        .unwrap_or_default();
    if !package.is_empty() && !package.split('.').all(valid_protobuf_identifier) {
        return Err(SchemaBindingError::InvalidProtobufDescriptor(
            "package is not a qualified identifier",
        ));
    }
    for field in fields {
        if field.number != 4 {
            continue;
        }
        if field.wire_type != 2 {
            return Err(SchemaBindingError::InvalidProtobufDescriptor(
                "message type field has invalid wire type",
            ));
        }
        parse_descriptor_proto(field.value, &package, None, messages, 0)?;
    }
    Ok(())
}

fn parse_descriptor_proto(
    bytes: &[u8],
    package: &str,
    parent: Option<&str>,
    messages: &mut Vec<BoundProtobufMessage>,
    depth: usize,
) -> Result<(), SchemaBindingError> {
    if depth >= MAX_NESTING {
        return Err(SchemaBindingError::InvalidProtobufDescriptor(
            "message nesting exceeds bound",
        ));
    }
    let fields = parse_wire_fields(bytes).map_err(SchemaBindingError::InvalidProtobufDescriptor)?;
    let name = fields
        .iter()
        .find(|field| field.number == 1)
        .ok_or(SchemaBindingError::InvalidProtobufDescriptor(
            "message has no name",
        ))
        .and_then(|field| wire_utf8(*field, "message name is not UTF-8"))?;
    if !valid_protobuf_identifier(&name) {
        return Err(SchemaBindingError::InvalidProtobufDescriptor(
            "message name is not an identifier",
        ));
    }
    let qualified = match (package.is_empty(), parent) {
        (true, None) => name.clone(),
        (false, None) => format!("{package}.{name}"),
        (true, Some(parent)) => format!("{parent}.{name}"),
        (false, Some(parent)) => format!("{package}.{parent}.{name}"),
    };
    let mut bound_fields = Vec::new();
    let mut field_numbers = BTreeSet::new();
    let mut field_names = BTreeSet::new();
    for field in &fields {
        if field.number == 2 {
            if field.wire_type != 2 {
                return Err(SchemaBindingError::InvalidProtobufDescriptor(
                    "field descriptor has invalid wire type",
                ));
            }
            let field = parse_field_descriptor_proto(field.value)?;
            if !field_numbers.insert(field.number) {
                return Err(SchemaBindingError::InvalidProtobufDescriptor(
                    "message declares duplicate field number",
                ));
            }
            if !field_names.insert(field.name.clone()) {
                return Err(SchemaBindingError::InvalidProtobufDescriptor(
                    "message declares duplicate field name",
                ));
            }
            bound_fields.push(field);
            if bound_fields.len() > MAX_DESCRIPTOR_FIELDS {
                return Err(SchemaBindingError::InvalidProtobufDescriptor(
                    "too many fields",
                ));
            }
        }
    }
    messages.push(BoundProtobufMessage {
        name: qualified,
        fields: bound_fields,
    });
    let nested_parent = match parent {
        Some(parent) => format!("{parent}.{name}"),
        None => name,
    };
    for field in fields {
        if field.number == 3 {
            if field.wire_type != 2 {
                return Err(SchemaBindingError::InvalidProtobufDescriptor(
                    "nested message field has invalid wire type",
                ));
            }
            parse_descriptor_proto(
                field.value,
                package,
                Some(&nested_parent),
                messages,
                depth + 1,
            )?;
        }
    }
    Ok(())
}

fn parse_field_descriptor_proto(bytes: &[u8]) -> Result<BoundProtobufField, SchemaBindingError> {
    let fields = parse_wire_fields(bytes).map_err(SchemaBindingError::InvalidProtobufDescriptor)?;
    let name = fields
        .iter()
        .find(|field| field.number == 1)
        .ok_or(SchemaBindingError::InvalidProtobufDescriptor(
            "field has no name",
        ))
        .and_then(|field| wire_utf8(*field, "field name is not UTF-8"))?;
    if !valid_protobuf_identifier(&name) {
        return Err(SchemaBindingError::InvalidProtobufDescriptor(
            "field name is not an identifier",
        ));
    }
    let number = fields
        .iter()
        .find(|field| field.number == 3)
        .ok_or(SchemaBindingError::InvalidProtobufDescriptor(
            "field has no number",
        ))
        .and_then(|field| wire_varint(*field, "field number is not varint"))?;
    if number == 0 || number > 536_870_911 || (19_000..=19_999).contains(&number) {
        return Err(SchemaBindingError::InvalidProtobufDescriptor(
            "field number is out of range",
        ));
    }
    let field_type = fields
        .iter()
        .find(|field| field.number == 5)
        .ok_or(SchemaBindingError::InvalidProtobufDescriptor(
            "field has no type",
        ))
        .and_then(|field| wire_varint(*field, "field type is not varint"))?;
    let label = fields
        .iter()
        .find(|field| field.number == 4)
        .map(|field| wire_varint(*field, "field label is not varint"))
        .transpose()?
        .unwrap_or(1);
    if !matches!(label, 1..=3) {
        return Err(SchemaBindingError::InvalidProtobufDescriptor(
            "field has invalid label",
        ));
    }
    let repeated = label == 3;
    let declared_type = fields
        .iter()
        .find(|field| field.number == 6)
        .map(|field| wire_utf8(*field, "field type name is not UTF-8"))
        .transpose()?
        .unwrap_or_else(|| protobuf_type_name(field_type).to_owned());
    let mut wire_types = protobuf_wire_types(field_type)?;
    if repeated && !wire_types.contains(&2) && protobuf_packable(field_type) {
        wire_types.push(2);
    }
    Ok(BoundProtobufField {
        number: number as u32,
        name,
        declared_type,
        wire_types,
    })
}

fn wire_utf8(field: WireField<'_>, error: &'static str) -> Result<String, SchemaBindingError> {
    if field.wire_type != 2 {
        return Err(SchemaBindingError::InvalidProtobufDescriptor(error));
    }
    std::str::from_utf8(field.value)
        .map(str::to_owned)
        .map_err(|_| SchemaBindingError::InvalidProtobufDescriptor(error))
}

fn wire_varint(field: WireField<'_>, error: &'static str) -> Result<u64, SchemaBindingError> {
    if field.wire_type != 0 {
        return Err(SchemaBindingError::InvalidProtobufDescriptor(error));
    }
    parse_varint(field.value).map_err(|_| SchemaBindingError::InvalidProtobufDescriptor(error))
}

fn protobuf_type_name(field_type: u64) -> &'static str {
    match field_type {
        1 => "double",
        2 => "float",
        3 => "int64",
        4 => "uint64",
        5 => "int32",
        6 => "fixed64",
        7 => "fixed32",
        8 => "bool",
        9 => "string",
        10 => "group",
        11 => "message",
        12 => "bytes",
        13 => "uint32",
        14 => "enum",
        15 => "sfixed32",
        16 => "sfixed64",
        17 => "sint32",
        18 => "sint64",
        _ => "unknown",
    }
}

fn protobuf_wire_types(field_type: u64) -> Result<Vec<u8>, SchemaBindingError> {
    let wire_types = match field_type {
        1 | 6 | 16 => vec![1],
        2 | 7 | 15 => vec![5],
        3 | 4 | 5 | 8 | 13 | 14 | 17 | 18 => vec![0],
        9 | 11 | 12 => vec![2],
        _ => {
            return Err(SchemaBindingError::InvalidProtobufDescriptor(
                "field has unsupported type",
            ));
        }
    };
    Ok(wire_types)
}

fn protobuf_packable(field_type: u64) -> bool {
    !matches!(field_type, 9..=12)
}

fn parse_wire_fields(bytes: &[u8]) -> Result<Vec<WireField<'_>>, &'static str> {
    let mut offset = 0usize;
    let mut fields = Vec::new();
    while offset < bytes.len() {
        if fields.len() >= MAX_DESCRIPTOR_FIELDS {
            return Err("too many wire fields");
        }
        let key = read_varint(bytes, &mut offset)?;
        let number = key >> 3;
        let wire_type = (key & 7) as u8;
        if number == 0 || number > u32::MAX as u64 || matches!(wire_type, 3 | 4 | 6 | 7) {
            return Err("invalid wire tag");
        }
        let start = offset;
        let value = match wire_type {
            0 => {
                let _ = read_varint(bytes, &mut offset)?;
                &bytes[start..offset]
            }
            1 => {
                offset = offset.checked_add(8).ok_or("wire length overflow")?;
                bytes.get(start..offset).ok_or("truncated fixed64 field")?
            }
            2 => {
                let length = read_varint(bytes, &mut offset)?;
                let length = usize::try_from(length).map_err(|_| "wire length overflow")?;
                let end = offset.checked_add(length).ok_or("wire length overflow")?;
                let value = bytes.get(offset..end).ok_or("truncated length field")?;
                offset = end;
                value
            }
            5 => {
                offset = offset.checked_add(4).ok_or("wire length overflow")?;
                bytes.get(start..offset).ok_or("truncated fixed32 field")?
            }
            _ => return Err("unsupported wire type"),
        };
        fields.push(WireField {
            number: number as u32,
            wire_type,
            value,
        });
    }
    Ok(fields)
}

fn read_varint(bytes: &[u8], offset: &mut usize) -> Result<u64, &'static str> {
    let start = *offset;
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*offset).ok_or("truncated varint")?;
        *offset += 1;
        if shift == 63 && byte > 1 {
            return Err("varint overflow");
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            if *offset - start > 1 && byte == 0 && value < (1u64 << shift) {
                return Err("non-canonical varint");
            }
            return Ok(value);
        }
    }
    Err("varint too long")
}

fn parse_varint(bytes: &[u8]) -> Result<u64, &'static str> {
    let mut offset = 0;
    let value = read_varint(bytes, &mut offset)?;
    if offset != bytes.len() {
        return Err("trailing varint bytes");
    }
    Ok(value)
}

fn qualified_name_matches(requested: &str, declared: &str) -> bool {
    requested == declared
        || (!requested.contains('.') && declared.rsplit('.').next() == Some(requested))
}

fn verify_protobuf_payload(bytes: &[u8], fields: &[BoundProtobufField]) -> anyhow::Result<()> {
    let wire = parse_wire_fields(bytes)
        .map_err(|error| anyhow::anyhow!("malformed protobuf payload: {error}"))?;
    let expected = fields
        .iter()
        .map(|field| (field.number, field))
        .collect::<BTreeMap<_, _>>();
    for field in wire {
        if let Some(schema_field) = expected.get(&field.number) {
            anyhow::ensure!(
                schema_field.wire_types.contains(&field.wire_type),
                "protobuf payload field {} has wire type {} incompatible with bound field {}",
                field.number,
                field.wire_type,
                schema_field.name,
            );
        }
    }
    Ok(())
}

fn flatbuffers_declared_types(schema: &str) -> Vec<String> {
    let mut result = BTreeSet::new();
    let mut in_block_comment = false;
    for line in schema.lines().take(MAX_DECLARATIONS) {
        let Some(line) = source_line(line, &mut in_block_comment) else {
            continue;
        };
        for keyword in ["table", "struct"] {
            if let Some(name) = keyword_name(line, keyword) {
                result.insert(name.to_owned());
            }
        }
    }
    result.into_iter().collect()
}

fn flatbuffers_file_identifier(schema: &str) -> Result<Option<[u8; 4]>, SchemaBindingError> {
    let mut in_block_comment = false;
    let mut found = None;
    for line in schema.lines().take(MAX_DECLARATIONS) {
        let Some(line) = source_line(line, &mut in_block_comment) else {
            continue;
        };
        let Some(rest) = line.strip_prefix("file_identifier") else {
            continue;
        };
        let value = rest
            .trim()
            .strip_prefix('"')
            .and_then(|rest| rest.split_once('"'));
        let Some((identifier, tail)) = value else {
            return Err(SchemaBindingError::InvalidFlatbuffersSchema(
                "invalid file_identifier declaration",
            ));
        };
        if !tail.trim().is_empty() && tail.trim() != ";" {
            return Err(SchemaBindingError::InvalidFlatbuffersSchema(
                "invalid file_identifier suffix",
            ));
        }
        let bytes: [u8; 4] = identifier.as_bytes().try_into().map_err(|_| {
            SchemaBindingError::InvalidFlatbuffersSchema("identifier must be four bytes")
        })?;
        if bytes.iter().any(|byte| !byte.is_ascii_graphic()) {
            return Err(SchemaBindingError::InvalidFlatbuffersSchema(
                "identifier must use printable ASCII",
            ));
        }
        if found.replace(bytes).is_some() {
            return Err(SchemaBindingError::InvalidFlatbuffersSchema(
                "multiple file identifiers",
            ));
        }
    }
    Ok(found)
}

fn verify_flatbuffers_payload(
    bytes: &[u8],
    expected_file_identifier: Option<[u8; 4]>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        bytes.len() >= 8,
        "malformed FlatBuffers payload: buffer is too short"
    );
    if let Some(identifier) = expected_file_identifier {
        anyhow::ensure!(
            bytes.get(4..8) == Some(identifier.as_slice()),
            "FlatBuffers payload file identifier does not match verified schema binding"
        );
    }
    let root_offset = read_u32_le(bytes, 0)? as usize;
    anyhow::ensure!(
        root_offset >= 8
            && root_offset
                .checked_add(4)
                .is_some_and(|end| end <= bytes.len()),
        "malformed FlatBuffers payload: root table offset is out of bounds"
    );
    let vtable_offset = read_i32_le(bytes, root_offset)?;
    anyhow::ensure!(
        vtable_offset > 0 && (vtable_offset as usize) <= root_offset,
        "malformed FlatBuffers payload: invalid vtable offset"
    );
    let vtable = root_offset - vtable_offset as usize;
    let vtable_size = read_u16_le(bytes, vtable)? as usize;
    let object_size = read_u16_le(
        bytes,
        vtable
            .checked_add(2)
            .ok_or_else(|| anyhow::anyhow!("FlatBuffers vtable overflow"))?,
    )? as usize;
    anyhow::ensure!(
        vtable_size >= 4
            && object_size >= 4
            && vtable
                .checked_add(vtable_size)
                .is_some_and(|end| end <= bytes.len())
            && root_offset
                .checked_add(object_size)
                .is_some_and(|end| end <= bytes.len()),
        "malformed FlatBuffers payload: table or vtable exceeds buffer"
    );
    Ok(())
}

fn read_u16_le(bytes: &[u8], offset: usize) -> anyhow::Result<u16> {
    let slice = bytes
        .get(
            offset
                ..offset
                    .checked_add(2)
                    .ok_or_else(|| anyhow::anyhow!("offset overflow"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("malformed FlatBuffers payload: truncated u16"))?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> anyhow::Result<u32> {
    let slice = bytes
        .get(
            offset
                ..offset
                    .checked_add(4)
                    .ok_or_else(|| anyhow::anyhow!("offset overflow"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("malformed FlatBuffers payload: truncated u32"))?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_i32_le(bytes: &[u8], offset: usize) -> anyhow::Result<i32> {
    Ok(read_u32_le(bytes, offset)? as i32)
}

struct ProtocolState<'a> {
    source_file: &'a str,
    stem: String,
    format: ProtocolFormat,
    file_id: String,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    seen_nodes: BTreeSet<String>,
    seen_edges: BTreeSet<(String, String, String)>,
    declarations: usize,
    fields: usize,
}

impl<'a> ProtocolState<'a> {
    fn new(path: &Path, source_file: &'a str, format: ProtocolFormat) -> Self {
        let stem = normalized_stem(source_file);
        let file_id = make_id(&[&stem, format.id(), "file"]);
        let label = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(source_file);
        let mut state = Self {
            source_file,
            stem,
            format,
            file_id: file_id.clone(),
            nodes: Vec::new(),
            edges: Vec::new(),
            seen_nodes: BTreeSet::new(),
            seen_edges: BTreeSet::new(),
            declarations: 0,
            fields: 0,
        };
        if crate::parser_budget::try_reserve_facts(1) {
            let mut file =
                protocol_node(file_id, label, source_file, 1, "protocol_file", format.id());
            file.extra
                .insert("format_capability".into(), format.capability().into());
            file.extra.insert("parse_status".into(), "partial".into());
            state.seen_nodes.insert(file.id.clone());
            state.nodes.push(file);
        }
        state
    }

    fn declaration(
        &mut self,
        parent: Option<&str>,
        kind: &str,
        name: &str,
        line: usize,
    ) -> Option<String> {
        if self.declarations >= MAX_DECLARATIONS || !valid_name(name) {
            return None;
        }
        let id = {
            let scope = parent.unwrap_or(&self.file_id);
            make_id(&[&self.stem, self.format.id(), kind, scope, name])
        };
        if id.is_empty() {
            return None;
        }
        if !self.seen_nodes.contains(&id) {
            if !crate::parser_budget::try_reserve_facts(1) {
                return None;
            }
            self.seen_nodes.insert(id.clone());
            self.declarations += 1;
            self.nodes.push(protocol_node(
                id.clone(),
                name,
                self.source_file,
                line,
                kind,
                self.format.id(),
            ));
        }
        if let Some(parent) = parent {
            self.relation(parent, &id, "contains", line);
        } else {
            let file_id = self.file_id.clone();
            self.relation(&file_id, &id, "contains", line);
        }
        Some(id)
    }

    fn field(&mut self, parent: &str, name: &str, declared_type: Option<&str>, line: usize) {
        if self.fields >= MAX_FIELDS || !valid_name(name) {
            return;
        }
        let id = make_id(&[&self.stem, self.format.id(), "field", parent, name]);
        if id.is_empty() {
            return;
        }
        if !self.seen_nodes.contains(&id) {
            if !crate::parser_budget::try_reserve_facts(1) {
                return;
            }
            self.seen_nodes.insert(id.clone());
            self.fields += 1;
            let mut field = protocol_node(
                id.clone(),
                name,
                self.source_file,
                line,
                "field",
                self.format.id(),
            );
            if let Some(declared_type) = declared_type.filter(|value| valid_name(value)) {
                field
                    .extra
                    .insert("declared_type".into(), sanitize_label(declared_type).into());
            }
            self.nodes.push(field);
        }
        self.relation(parent, &id, "contains", line);
        if let Some(declared_type) = declared_type.filter(|value| !is_builtin_type(value)) {
            self.reference(&id, declared_type, line);
        }
    }

    fn operation(&mut self, parent: Option<&str>, name: &str, line: usize) {
        let _ = self.declaration(parent, "operation", name, line);
    }

    fn import(&mut self, target: &str, line: usize) {
        self.reference(&self.file_id.clone(), target, line);
        let target_id = make_id(&["protocol_reference", target]);
        self.relation(&self.file_id.clone(), &target_id, "imports", line);
    }

    fn reference(&mut self, source: &str, target: &str, line: usize) {
        let target = target.trim_matches(|value| matches!(value, '"' | '\'' | '`' | ';' | ','));
        if !valid_reference(target) {
            return;
        }
        let id = make_id(&["protocol_reference", target]);
        if !self.seen_nodes.contains(&id) {
            if !crate::parser_budget::try_reserve_facts(1) {
                return;
            }
            self.seen_nodes.insert(id.clone());
            self.nodes.push(protocol_node(
                id.clone(),
                target,
                self.source_file,
                line,
                "external_reference",
                self.format.id(),
            ));
        }
        self.relation(source, &id, "references", line);
    }

    fn relation(&mut self, source: &str, target: &str, relation: &str, line: usize) {
        if self.edges.len() >= MAX_EDGES
            || source.is_empty()
            || target.is_empty()
            || source == target
        {
            return;
        }
        if !crate::parser_budget::try_reserve_facts(1) {
            return;
        }
        let identity = (source.to_owned(), target.to_owned(), relation.to_owned());
        if self.seen_edges.contains(&identity) {
            return;
        }
        self.seen_edges.insert(identity);
        self.edges.push(protocol_edge(
            source.to_owned(),
            target.to_owned(),
            relation,
            self.source_file,
            line,
        ));
    }

    fn finish(self) -> Extraction {
        Extraction {
            nodes: self.nodes,
            edges: self.edges,
            hyperedges: Vec::new(),
        }
    }
}

fn normalized_stem(source_file: &str) -> String {
    Path::new(source_file)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/")
}

fn protocol_node(
    id: String,
    label: &str,
    source_file: &str,
    line: usize,
    kind: &str,
    format: &str,
) -> Node {
    Node {
        id,
        label: sanitize_label(label),
        file_type: "code".into(),
        source_file: source_file.into(),
        source_location: Some(format!("L{line}")),
        community: None,
        extra: BTreeMap::from([
            ("_origin".into(), "protocols".into()),
            ("type".into(), kind.into()),
            ("protocol_format".into(), format.into()),
        ]),
    }
}

fn protocol_edge(
    source: String,
    target: String,
    relation: &str,
    source_file: &str,
    line: usize,
) -> Edge {
    Edge {
        source: source.clone(),
        target: target.clone(),
        relation: relation.into(),
        confidence: Confidence::Extracted,
        source_file: source_file.into(),
        extra: BTreeMap::from([
            ("source_location".into(), format!("L{line}").into()),
            ("weight".into(), 1.0.into()),
            ("_src".into(), source.into()),
            ("_tgt".into(), target.into()),
        ]),
    }
}

#[derive(Debug, Clone)]
struct Scope {
    id: String,
    kind: String,
}

fn parse_text_idl(state: &mut ProtocolState<'_>, text: &str) {
    let mut scopes = Vec::<Scope>::new();
    let mut in_block_comment = false;
    for (index, raw_line) in text.lines().enumerate().take(MAX_DECLARATIONS + MAX_FIELDS) {
        if raw_line.len() > MAX_LINE_BYTES {
            continue;
        }
        let Some(line) = source_line(raw_line, &mut in_block_comment) else {
            continue;
        };
        let line_no = index + 1;
        if let Some(target) = import_target(line, state.format) {
            state.import(target, line_no);
        }
        if matches!(state.format, ProtocolFormat::Asn1) {
            parse_asn1_line(state, &mut scopes, line, line_no);
            update_scopes(&mut scopes, line, None);
            continue;
        }
        if matches!(state.format, ProtocolFormat::Cddl) {
            parse_cddl_line(state, line, line_no);
            continue;
        }

        let parent = scopes.last().map(|scope| scope.id.as_str());
        let declaration = declaration_for_line(state.format, line).and_then(|(kind, name)| {
            state
                .declaration(parent, kind, name, line_no)
                .map(|id| Scope {
                    id,
                    kind: kind.into(),
                })
        });
        if declaration.is_none() {
            parse_context_member(state, &scopes, line, line_no);
        }
        update_scopes(&mut scopes, line, declaration);
    }
}

fn source_line<'a>(line: &'a str, in_block_comment: &mut bool) -> Option<&'a str> {
    let mut line = line;
    if *in_block_comment {
        let end = line.find("*/")?;
        *in_block_comment = false;
        line = &line[end + 2..];
    }
    let mut cut = line.len();
    if let Some(index) = line.find("//") {
        cut = cut.min(index);
    }
    if let Some(index) = line.find("/*") {
        cut = cut.min(index);
        if line[index + 2..].find("*/").is_none() {
            *in_block_comment = true;
        }
    }
    let line = line[..cut].trim();
    (!line.is_empty() && !line.starts_with('#')).then_some(line)
}

fn declaration_for_line(format: ProtocolFormat, line: &str) -> Option<(&'static str, &str)> {
    let declarations: &[(&str, &str)] = match format {
        ProtocolFormat::Protobuf => &[
            ("message", "message"),
            ("enum", "enum"),
            ("service", "service"),
            ("oneof", "oneof"),
        ],
        ProtocolFormat::Flatbuffers => &[
            ("table", "table"),
            ("struct", "struct"),
            ("enum", "enum"),
            ("union", "union"),
            ("rpc_service", "service"),
        ],
        ProtocolFormat::Thrift => &[
            ("struct", "struct"),
            ("union", "union"),
            ("exception", "exception"),
            ("enum", "enum"),
            ("service", "service"),
        ],
        ProtocolFormat::Capnp => &[
            ("struct", "struct"),
            ("interface", "interface"),
            ("enum", "enum"),
        ],
        ProtocolFormat::AvroIdl => &[
            ("protocol", "service"),
            ("record", "record"),
            ("enum", "enum"),
            ("fixed", "type"),
            ("error", "error"),
        ],
        ProtocolFormat::GraphQl => &[
            ("type", "type"),
            ("interface", "interface"),
            ("input", "input"),
            ("enum", "enum"),
            ("union", "union"),
            ("scalar", "scalar"),
            ("schema", "schema"),
        ],
        ProtocolFormat::Wit => &[
            ("package", "package"),
            ("world", "world"),
            ("interface", "interface"),
            ("record", "record"),
            ("variant", "variant"),
            ("enum", "enum"),
            ("flags", "flags"),
            ("resource", "resource"),
        ],
        ProtocolFormat::Smithy => &[
            ("service", "service"),
            ("operation", "operation"),
            ("resource", "resource"),
            ("structure", "struct"),
            ("union", "union"),
            ("enum", "enum"),
            ("intEnum", "enum"),
            ("list", "type"),
            ("map", "type"),
        ],
        ProtocolFormat::Yang => &[
            ("module", "module"),
            ("submodule", "module"),
            ("container", "container"),
            ("list", "list"),
            ("leaf-list", "field"),
            ("leaf", "field"),
            ("grouping", "grouping"),
            ("typedef", "type"),
            ("rpc", "operation"),
            ("action", "operation"),
            ("notification", "operation"),
        ],
        ProtocolFormat::OpenApi
        | ProtocolFormat::AsyncApi
        | ProtocolFormat::AvroSchema
        | ProtocolFormat::Cddl
        | ProtocolFormat::Asn1 => &[],
    };
    declarations
        .iter()
        .find_map(|(keyword, kind)| keyword_name(line, keyword).map(|name| (*kind, name)))
}

fn keyword_name<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(keyword)?;
    if !rest.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    first_name(rest)
}

fn first_name(text: &str) -> Option<&str> {
    let name = text
        .trim_start()
        .split(|value: char| {
            value.is_whitespace() || matches!(value, '{' | '}' | '(' | ')' | ':' | ';' | '=' | ',')
        })
        .next()?;
    valid_name(name).then_some(name)
}

fn update_scopes(scopes: &mut Vec<Scope>, line: &str, declaration: Option<Scope>) {
    let opens = line.bytes().filter(|value| *value == b'{').count();
    let closes = line.bytes().filter(|value| *value == b'}').count();
    let has_declaration = declaration.is_some();
    if let Some(scope) = declaration
        && opens > closes
        && scopes.len() < MAX_NESTING
    {
        scopes.push(scope);
    }
    // A declaration's braces describe that declaration. Only excess closing
    // braces can close parent scopes; `message X {}` must not pop its parent.
    let existing_closes = if has_declaration {
        closes.saturating_sub(opens)
    } else {
        closes
    };
    for _ in 0..existing_closes.min(scopes.len()) {
        scopes.pop();
    }
}

fn parse_context_member(
    state: &mut ProtocolState<'_>,
    scopes: &[Scope],
    line: &str,
    line_no: usize,
) {
    let Some(parent) = scopes.last() else {
        return;
    };
    if matches!(state.format, ProtocolFormat::Protobuf) {
        if let Some(name) = keyword_name(line, "rpc") {
            state.operation(Some(&parent.id), name, line_no);
            return;
        }
        if is_field_scope(&parent.kind)
            && let Some((name, ty)) = equals_field(line)
        {
            state.field(&parent.id, name, Some(ty), line_no);
        }
        return;
    }
    if matches!(state.format, ProtocolFormat::Flatbuffers) {
        if keyword_name(line, "rpc").is_some() {
            if let Some(name) = keyword_name(line, "rpc") {
                state.operation(Some(&parent.id), name, line_no);
            }
            return;
        }
        if is_field_scope(&parent.kind)
            && let Some((name, ty)) = colon_field(line, state.format)
        {
            state.field(&parent.id, name, Some(ty), line_no);
        }
        return;
    }
    if matches!(state.format, ProtocolFormat::GraphQl) {
        if is_field_scope(&parent.kind)
            && let Some((name, ty)) = graphql_field(line)
        {
            state.field(&parent.id, name, Some(ty), line_no);
        }
        return;
    }
    if matches!(state.format, ProtocolFormat::Wit) {
        if let Some(name) = keyword_name(line, "func") {
            state.operation(Some(&parent.id), name, line_no);
        } else if is_field_scope(&parent.kind)
            && let Some((name, ty)) = colon_field(line, state.format)
        {
            state.field(&parent.id, name, Some(ty), line_no);
        }
        return;
    }
    if matches!(state.format, ProtocolFormat::Capnp) {
        if line.contains("->") && line.contains('@') {
            if let Some(name) = first_name(line) {
                state.operation(Some(&parent.id), name, line_no);
            }
        } else if is_field_scope(&parent.kind)
            && let Some((name, ty)) = capnp_field(line)
        {
            state.field(&parent.id, name, Some(ty), line_no);
        }
        return;
    }
    if matches!(state.format, ProtocolFormat::AvroIdl) {
        if parent.kind == "service" && line.contains('(') {
            if let Some(name) = name_before_paren(line) {
                state.operation(Some(&parent.id), name, line_no);
            }
        } else if parent.kind == "record"
            && let Some((name, ty)) = avro_field(line)
        {
            state.field(&parent.id, name, Some(ty), line_no);
        }
        return;
    }
    if matches!(state.format, ProtocolFormat::Yang) {
        if matches!(parent.kind.as_str(), "container" | "list")
            && let Some((name, ty)) = keyword_value(line, "type")
        {
            state.field(&parent.id, name, Some(ty), line_no);
        }
        return;
    }
    if matches!(
        state.format,
        ProtocolFormat::Thrift | ProtocolFormat::Smithy
    ) {
        if parent.kind == "service" && line.contains('(') {
            if let Some(name) = name_before_paren(line) {
                state.operation(Some(&parent.id), name, line_no);
            }
        } else if is_field_scope(&parent.kind)
            && let Some((name, ty)) = colon_field(line, state.format)
        {
            state.field(&parent.id, name, Some(ty), line_no);
        }
    }
}

fn is_field_scope(kind: &str) -> bool {
    matches!(
        kind,
        "message" | "struct" | "record" | "table" | "union" | "type" | "interface" | "input"
    )
}

fn equals_field(line: &str) -> Option<(&str, &str)> {
    let (left, _) = line.split_once('=')?;
    if ["option", "reserved", "syntax", "package"]
        .iter()
        .any(|prefix| left.trim_start().starts_with(prefix))
    {
        return None;
    }
    let mut tokens = left.split_whitespace();
    let ty = tokens.find(|token| !matches!(*token, "optional" | "required" | "repeated"))?;
    let name = tokens
        .last()
        .unwrap_or(ty)
        .trim_matches(|value: char| !value.is_ascii_alphanumeric() && value != '_');
    (valid_name(name) && valid_name(ty)).then_some((name, ty))
}

fn colon_field(line: &str, format: ProtocolFormat) -> Option<(&str, &str)> {
    let (left, right) = line.split_once(':')?;
    let name = match format {
        ProtocolFormat::Thrift => right
            .split(|value: char| value.is_whitespace() || matches!(value, '=' | ';' | ','))
            .rfind(|value| valid_name(value))?,
        ProtocolFormat::Smithy | ProtocolFormat::Wit | ProtocolFormat::Flatbuffers => {
            left.split_whitespace().last()?
        }
        _ => left.split_whitespace().last()?,
    };
    let ty = match format {
        ProtocolFormat::Thrift => right
            .split_whitespace()
            .find(|value| valid_name(value) && !matches!(*value, "required" | "optional"))?,
        _ => right
            .trim_start()
            .split(|value: char| {
                value.is_whitespace() || matches!(value, ';' | ',' | '=' | '[' | '{')
            })
            .next()?,
    };
    (valid_name(name) && valid_name(ty)).then_some((name, ty))
}

fn capnp_field(line: &str) -> Option<(&str, &str)> {
    let (name, tagged) = line.split_once('@')?;
    let (_, ty) = tagged.split_once(':')?;
    let name = name.trim();
    let ty = ty
        .trim()
        .split(|value: char| value.is_whitespace() || value == ';')
        .next()?;
    (valid_name(name) && valid_name(ty)).then_some((name, ty))
}

fn graphql_field(line: &str) -> Option<(&str, &str)> {
    let (left, right) = line.split_once(':')?;
    let name = left.trim().split('(').next()?.trim();
    let ty = right
        .trim()
        .trim_matches(|value| matches!(value, '[' | ']' | '!'));
    (valid_name(name) && valid_name(ty)).then_some((name, ty))
}

fn avro_field(line: &str) -> Option<(&str, &str)> {
    let line = line.trim().trim_end_matches(',').trim_end_matches(';');
    let mut parts = line.split_whitespace();
    let ty = parts.next()?;
    let name = parts.next()?;
    (parts.next().is_none() && valid_name(name) && valid_name(ty)).then_some((name, ty))
}

fn name_before_paren(line: &str) -> Option<&str> {
    let prefix = line.split_once('(')?.0;
    prefix
        .split_whitespace()
        .last()
        .filter(|name| valid_name(name))
}

fn keyword_value<'a>(line: &'a str, keyword: &str) -> Option<(&'a str, &'a str)> {
    let value = line
        .strip_prefix(keyword)?
        .trim_start()
        .trim_end_matches(';')
        .trim();
    valid_name(value).then_some((value, value))
}

fn import_target(line: &str, format: ProtocolFormat) -> Option<&str> {
    let words = match format {
        ProtocolFormat::Thrift => &["include", "cpp_include", "namespace"] as &[&str],
        ProtocolFormat::Capnp => &["using", "import"] as &[&str],
        ProtocolFormat::Smithy | ProtocolFormat::Wit => &["use", "import"] as &[&str],
        ProtocolFormat::Yang => &["import", "include"] as &[&str],
        ProtocolFormat::Asn1 => &["FROM", "IMPORTS"] as &[&str],
        _ => &["import", "include"] as &[&str],
    };
    for keyword in words {
        if let Some(rest) = line.strip_prefix(keyword) {
            if !rest.is_empty() && !rest.chars().next().is_some_and(char::is_whitespace) {
                continue;
            }
            let rest = rest.trim_start();
            if let Some(quoted) = rest
                .strip_prefix('"')
                .and_then(|value| value.split_once('"'))
            {
                return Some(quoted.0);
            }
            if let Some(quoted) = rest
                .strip_prefix('\'')
                .and_then(|value| value.split_once('\''))
            {
                return Some(quoted.0);
            }
            let target = rest
                .split(|value: char| value.is_whitespace() || matches!(value, ';' | '{' | '}'))
                .next()
                .unwrap_or_default()
                .trim_end_matches(',');
            if valid_reference(target) {
                return Some(target);
            }
        }
    }
    None
}

fn parse_cddl_line(state: &mut ProtocolState<'_>, line: &str, line_no: usize) {
    let Some((name, _)) = line.split_once('=') else {
        return;
    };
    let name = name.trim().trim_end_matches('/').trim();
    if valid_name(name) {
        let _ = state.declaration(None, "type", name, line_no);
    }
}

fn parse_asn1_line(
    state: &mut ProtocolState<'_>,
    scopes: &mut Vec<Scope>,
    line: &str,
    line_no: usize,
) {
    if let Some((name, rest)) = line.split_once("::=") {
        let name = name.trim();
        if valid_name(name) {
            let kind = if rest.contains("SEQUENCE") || rest.contains("SET") {
                "struct"
            } else if rest.contains("ENUMERATED") {
                "enum"
            } else {
                "type"
            };
            let parent = scopes.last().map(|scope| scope.id.as_str());
            if let Some(id) = state.declaration(parent, kind, name, line_no)
                && line.contains('{')
            {
                scopes.push(Scope {
                    id,
                    kind: kind.into(),
                });
            }
        }
    } else if let Some(parent) = scopes.last()
        && let Some((name, ty)) = asn1_field(line)
    {
        state.field(&parent.id, name, Some(ty), line_no);
    }
}

fn asn1_field(line: &str) -> Option<(&str, &str)> {
    let line = line.trim().trim_end_matches(',');
    let mut fields = line.split_whitespace();
    let name = fields.next()?;
    let ty = fields.next()?;
    (valid_name(name) && valid_name(ty)).then_some((name, ty))
}

fn parse_avro_schema(state: &mut ProtocolState<'_>, text: &str) {
    let Ok(value) = graphoxide_core::parse_jsonc(text) else {
        return;
    };
    parse_avro_value(state, &value, None, 1, 0);
}

fn parse_avro_value(
    state: &mut ProtocolState<'_>,
    value: &Value,
    parent: Option<&str>,
    line: usize,
    depth: usize,
) {
    if depth > MAX_NESTING {
        return;
    }
    let Some(object) = value.as_object() else {
        return;
    };
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("schema");
    let name = object.get("name").and_then(Value::as_str);
    let this = name.and_then(|name| state.declaration(parent, avro_kind(kind), name, line));
    let field_parent = this.as_deref().or(parent);
    if let Some(fields) = object.get("fields").and_then(Value::as_array) {
        for field in fields.iter().take(MAX_FIELDS) {
            let Some(field_object) = field.as_object() else {
                continue;
            };
            let Some(name) = field_object.get("name").and_then(Value::as_str) else {
                continue;
            };
            let declared_type = avro_type_name(field_object.get("type"));
            if let Some(parent) = field_parent {
                state.field(parent, name, declared_type.as_deref(), line);
            }
            if let Some(nested) = field_object.get("type") {
                parse_avro_value(state, nested, field_parent, line, depth + 1);
            }
        }
    }
    for key in ["items", "values"] {
        if let Some(nested) = object.get(key) {
            parse_avro_value(state, nested, field_parent, line, depth + 1);
        }
    }
}

fn avro_kind(kind: &str) -> &'static str {
    match kind {
        "record" => "record",
        "enum" => "enum",
        "fixed" => "type",
        _ => "schema",
    }
}

fn avro_type_name(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) => Some(value.clone()),
        Value::Object(value) => value.get("name").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

fn parse_api_json(state: &mut ProtocolState<'_>, value: &Value) {
    let Some(root) = value.as_object() else {
        return;
    };
    let title = root
        .get("info")
        .and_then(Value::as_object)
        .and_then(|info| info.get("title"))
        .and_then(Value::as_str)
        .unwrap_or(state.format.id());
    let api = state.declaration(None, "api", title, 1);
    let parent = api.as_deref();
    if matches!(state.format, ProtocolFormat::OpenApi) {
        if let Some(paths) = root.get("paths").and_then(Value::as_object) {
            parse_openapi_paths(state, paths, parent);
        }
    } else if let Some(channels) = root.get("channels").and_then(Value::as_object) {
        parse_asyncapi_channels(state, channels, parent);
    }
    if let Some(components) = root.get("components").and_then(Value::as_object) {
        parse_api_components(state, components, parent);
    }
    let reference_source = parent.unwrap_or(&state.file_id).to_owned();
    parse_json_references(state, &reference_source, value, 0);
}

fn parse_openapi_paths(
    state: &mut ProtocolState<'_>,
    paths: &serde_json::Map<String, Value>,
    parent: Option<&str>,
) {
    for (path, item) in paths.iter().take(MAX_DECLARATIONS) {
        let Some(endpoint) = state.declaration(parent, "endpoint", path, 1) else {
            continue;
        };
        let Some(item) = item.as_object() else {
            continue;
        };
        for (method, operation) in item {
            if !is_http_method(method) || !operation.is_object() {
                continue;
            }
            let label = operation
                .get("operationId")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{} {}", method.to_ascii_uppercase(), path));
            state.operation(Some(&endpoint), &label, 1);
        }
    }
}

fn parse_asyncapi_channels(
    state: &mut ProtocolState<'_>,
    channels: &serde_json::Map<String, Value>,
    parent: Option<&str>,
) {
    for (channel, item) in channels.iter().take(MAX_DECLARATIONS) {
        let Some(channel_id) = state.declaration(parent, "channel", channel, 1) else {
            continue;
        };
        let Some(item) = item.as_object() else {
            continue;
        };
        for operation in ["publish", "subscribe"] {
            if item.contains_key(operation) {
                state.operation(Some(&channel_id), operation, 1);
            }
        }
    }
}

fn parse_api_components(
    state: &mut ProtocolState<'_>,
    components: &serde_json::Map<String, Value>,
    parent: Option<&str>,
) {
    for section in [
        "schemas",
        "messages",
        "parameters",
        "responses",
        "securitySchemes",
    ] {
        let Some(items) = components.get(section).and_then(Value::as_object) else {
            continue;
        };
        for (name, item) in items.iter().take(MAX_DECLARATIONS) {
            let kind = if section == "schemas" {
                "schema"
            } else {
                "component"
            };
            let id = state.declaration(parent, kind, name, 1);
            if let (Some(id), Some(object)) = (id.as_deref(), item.as_object())
                && let Some(properties) = object.get("properties").and_then(Value::as_object)
            {
                for (name, property) in properties.iter().take(MAX_FIELDS) {
                    let ty = property.get("type").and_then(Value::as_str);
                    state.field(id, name, ty, 1);
                }
            }
        }
    }
}

fn parse_json_references(state: &mut ProtocolState<'_>, source: &str, value: &Value, depth: usize) {
    if depth > MAX_NESTING {
        return;
    }
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                state.reference(source, reference, 1);
            }
            for value in object.values() {
                parse_json_references(state, source, value, depth + 1);
            }
        }
        Value::Array(values) => {
            for value in values.iter().take(MAX_FIELDS) {
                parse_json_references(state, source, value, depth + 1);
            }
        }
        _ => {}
    }
}

fn parse_api_yaml(state: &mut ProtocolState<'_>, text: &str) {
    let mut in_block_comment = false;
    let mut api = None::<String>;
    let mut endpoint = None::<(usize, String)>;
    let mut channel = None::<(usize, String)>;
    let mut schemas_indent = None::<usize>;
    let mut api_kind_seen = false;
    for (index, raw_line) in text.lines().enumerate().take(MAX_DECLARATIONS + MAX_FIELDS) {
        if raw_line.len() > MAX_LINE_BYTES {
            continue;
        }
        let indent = raw_line.len() - raw_line.trim_start().len();
        let Some(line) = source_line(raw_line, &mut in_block_comment) else {
            continue;
        };
        let line_no = index + 1;
        if line.starts_with("openapi:") || line.starts_with("asyncapi:") {
            api_kind_seen = true;
            api = state.declaration(None, "api", state.format.id(), line_no);
            continue;
        }
        if !api_kind_seen {
            continue;
        }
        if let Some(reference) = line.strip_prefix("$ref:") {
            let source = api.clone().unwrap_or_else(|| state.file_id.clone());
            state.reference(&source, reference.trim(), line_no);
            continue;
        }
        if line == "paths:" {
            endpoint = None;
            continue;
        }
        if line == "channels:" {
            channel = None;
            continue;
        }
        if line == "schemas:" {
            schemas_indent = Some(indent);
            continue;
        }
        if let Some(schema_indent) = schemas_indent {
            if indent <= schema_indent && line.ends_with(':') && !line.starts_with('-') {
                schemas_indent = None;
            } else if indent > schema_indent && line.ends_with(':') && !line.contains(' ') {
                let name = line.trim_end_matches(':');
                let _ = state.declaration(api.as_deref(), "schema", name, line_no);
                continue;
            }
        }
        if line.starts_with('/') && line.ends_with(':') {
            let id = state.declaration(
                api.as_deref(),
                "endpoint",
                line.trim_end_matches(':'),
                line_no,
            );
            endpoint = id.map(|id| (indent, id));
            continue;
        }
        if let Some((endpoint_indent, id)) = endpoint.as_ref()
            && indent > *endpoint_indent
            && let Some(method) = line.trim_end_matches(':').split_whitespace().next()
            && is_http_method(method)
        {
            state.operation(Some(id), method, line_no);
            continue;
        }
        if line.ends_with(':') && !line.contains(' ') && !is_http_method(line.trim_end_matches(':'))
        {
            let id = state.declaration(
                api.as_deref(),
                "channel",
                line.trim_end_matches(':'),
                line_no,
            );
            channel = id.map(|id| (indent, id));
            continue;
        }
        if let Some((channel_indent, id)) = channel.as_ref()
            && indent > *channel_indent
            && matches!(line.trim_end_matches(':'), "publish" | "subscribe")
        {
            state.operation(Some(id), line.trim_end_matches(':'), line_no);
        }
    }
}

fn is_http_method(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "get" | "put" | "post" | "delete" | "patch" | "head" | "options" | "trace"
    )
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/' | b'@')
        })
}

fn valid_protobuf_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(byte) if byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_reference(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1_024 && !value.bytes().any(|byte| byte.is_ascii_control())
}

fn is_builtin_type(value: &str) -> bool {
    matches!(
        value.trim_matches(|value| matches!(value, '[' | ']' | '!' | '?' | '<' | '>')),
        "bool"
            | "boolean"
            | "string"
            | "bytes"
            | "byte"
            | "int"
            | "int32"
            | "int64"
            | "uint32"
            | "uint64"
            | "sint32"
            | "sint64"
            | "fixed32"
            | "fixed64"
            | "float"
            | "double"
            | "number"
            | "integer"
            | "null"
            | "any"
            | "unknown"
            | "unit"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "s8"
            | "s16"
            | "s32"
            | "s64"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "f32"
            | "f64"
    )
}

#[cfg(test)]
mod tests {
    use super::{
        extract_binary_protocol_with_binding_or_inventory, extract_bound_binary_protocol_bytes,
        extract_protocol_bytes, looks_like_api_description, supports_extension, SchemaBindingError,
        VerifiedBinarySchemaBinding, MAX_COLUMNAR_METADATA_BYTES, MAX_NESTING,
    };
    use std::{collections::BTreeSet, path::Path};

    fn extract(name: &str, text: &str) -> graphoxide_core::Extraction {
        extract_protocol_bytes(Path::new(name), name, text.as_bytes()).expect("extract protocol")
    }

    fn node(result: &graphoxide_core::Extraction, label: &str, kind: &str) -> String {
        result
            .nodes
            .iter()
            .find(|node| {
                node.label == label
                    && node.extra.get("type").and_then(serde_json::Value::as_str) == Some(kind)
            })
            .map(|node| node.id.clone())
            .unwrap_or_else(|| panic!("missing {kind} {label}: {:#?}", result.nodes))
    }

    #[test]
    fn supports_all_protocol_suffixes_without_claiming_generic_json() {
        for extension in [
            "proto", "fbs", "bfbs", "thrift", "capnp", "avsc", "avdl", "graphql", "gql", "wit",
            "smithy", "cddl", "yang", "yin", "asn1", "pbf", "parquet",
        ] {
            assert!(supports_extension(extension), "{extension}");
        }
        assert!(!supports_extension("json"));
    }

    #[test]
    fn protobuf_emits_messages_fields_services_operations_and_imports() {
        let result = extract(
            "api.proto",
            "syntax = \"proto3\";\nimport \"common.proto\";\nmessage Request {\n  string name = 1;\n}\nservice Greeter {\n  rpc SayHello(Request) returns (Reply);\n}\n",
        );
        node(&result, "Request", "message");
        node(&result, "name", "field");
        node(&result, "Greeter", "service");
        node(&result, "SayHello", "operation");
        assert!(result.nodes.iter().any(|node| node.label == "common.proto"));
        assert!(result.edges.iter().any(|edge| edge.relation == "imports"));
    }

    #[test]
    fn representative_text_idls_emit_declarations() {
        for (name, source, label) in [
            (
                "types.fbs",
                "namespace demo;\ntable User {\n  id: ulong;\n}\n",
                "User",
            ),
            (
                "types.thrift",
                "namespace rs demo\nstruct User {\n  1: required string name\n}\n",
                "User",
            ),
            (
                "types.capnp",
                "struct User {\n  name @0 :Text;\n}\n",
                "User",
            ),
            (
                "types.avdl",
                "protocol Demo {\n  record User {\n    string name;\n  }\n}\n",
                "User",
            ),
            (
                "schema.graphql",
                "type User {\n  name: String!\n}\n",
                "User",
            ),
            (
                "world.wit",
                "package demo:api;\ninterface users {\n  get: func(id: u32) -> string;\n}\n",
                "users",
            ),
            (
                "model.smithy",
                "namespace demo\nstructure User {\n  name: String\n}\n",
                "User",
            ),
            ("types.cddl", "user = { name: tstr }", "user"),
            (
                "model.yang",
                "module demo {\n  container users {\n    leaf name {\n      type string;\n    }\n  }\n}\n",
                "demo",
            ),
            (
                "types.asn1",
                "User ::= SEQUENCE {\n  name UTF8String\n}\n",
                "User",
            ),
        ] {
            let result = extract(name, source);
            assert!(
                result.nodes.iter().any(|node| node.label == label),
                "{name}: {:#?}",
                result.nodes
            );
        }
    }

    #[test]
    fn openapi_json_and_yaml_capture_operations_schemas_and_references() {
        let json = extract(
            "openapi.json",
            r##"{"openapi":"3.1.0","info":{"title":"Pets"},"paths":{"/pets":{"get":{"operationId":"listPets","responses":{"200":{"$ref":"#/components/responses/Ok"}}}}},"components":{"schemas":{"Pet":{"type":"object","properties":{"name":{"type":"string"}}}}}}"##,
        );
        node(&json, "Pets", "api");
        node(&json, "/pets", "endpoint");
        node(&json, "listPets", "operation");
        node(&json, "Pet", "schema");
        assert!(json
            .nodes
            .iter()
            .any(|node| node.label == "#/components/responses/Ok"));

        let yaml = "openapi: 3.1.0\npaths:\n  /pets:\n    get:\n      operationId: listPets\ncomponents:\n  schemas:\n    Pet:\n      type: object\n";
        assert!(looks_like_api_description(
            Path::new("openapi.yaml"),
            yaml.as_bytes()
        ));
        let yaml = extract("openapi.yaml", yaml);
        node(&yaml, "/pets", "endpoint");
        assert!(yaml.nodes.iter().any(|node| node
            .extra
            .get("type")
            .and_then(serde_json::Value::as_str)
            == Some("operation")));
    }

    #[test]
    fn binary_wire_data_is_inventory_only_and_never_decoded() {
        let result = extract_protocol_bytes(Path::new("payload.pb"), "payload.pb", &[8, 150, 1])
            .expect("inventory binary protocol");
        assert_eq!(result.nodes.len(), 1);
        let node = &result.nodes[0];
        assert_eq!(
            node.extra
                .get("format_capability")
                .and_then(serde_json::Value::as_str),
            Some("inventory_only")
        );
        assert_eq!(
            node.extra
                .get("diagnostic")
                .and_then(serde_json::Value::as_str),
            Some("schema_required_binary_payload_not_decoded")
        );
        assert!(result.edges.is_empty());
    }

    #[test]
    fn every_schema_required_binary_protocol_extension_is_unbound_inventory_only() {
        let registry = crate::format_registry::format_registry();
        let extensions = registry
            .specs()
            .iter()
            .filter(|spec| {
                spec.adapter() == crate::format_registry::ByteAdapterKind::Protocol
                    && spec.schema_requirement
                        == crate::format_registry::SchemaRequirement::Required
            })
            .flat_map(|spec| spec.extensions.iter().copied())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            extensions,
            BTreeSet::from([
                "ber", "bfbs", "cer", "der", "fds", "pb", "pb3", "pbf", "pem", "protobuf",
                "protobin", "desc",
            ])
        );

        for extension in extensions {
            let name = format!("payload.{extension}");
            let result = extract_protocol_bytes(Path::new(&name), &name, &[0x08, 0x96, 0x01])
                .expect("unbound binary protocol inventory");
            let root = result.nodes.first().expect("inventory root");
            assert_eq!(
                root.extra
                    .get("format_capability")
                    .and_then(serde_json::Value::as_str),
                Some("inventory_only"),
                "{extension}"
            );
            assert_eq!(
                root.extra
                    .get("diagnostic")
                    .and_then(serde_json::Value::as_str),
                Some("schema_required_binary_payload_not_decoded"),
                "{extension}"
            );
            assert!(result.edges.is_empty(), "{extension}");
        }
    }

    #[test]
    fn optional_binding_falls_back_to_inventory_for_unbound_or_rejected_payloads() {
        let descriptor = descriptor_set_for_string_field("example", "Envelope", "body", 1);
        let protobuf = VerifiedBinarySchemaBinding::protobuf_descriptor(
            "example-envelope-v1",
            &descriptor,
            "example.Envelope",
        )
        .expect("binding");
        let schema = b"table Monster {\n  name:string;\n}\nfile_identifier \"MONS\";\n";
        let flatbuffers = VerifiedBinarySchemaBinding::flatbuffers_schema_metadata(
            "example-monster-v1",
            schema,
            "Monster",
            Some(*b"MONS"),
        )
        .expect("binding");

        for (name, bytes, binding) in [
            ("unbound.pb", &[0x0a, 0x02, b'o', b'k'][..], None),
            ("malformed.pb", &[0x0a, 0x80][..], Some(&protobuf)),
            // `.pb3` and `.protobin` are registered protobuf extensions, so
            // they must not be accepted as a FlatBuffers payload merely
            // because an attacker chooses that filename.
            (
                "mismatched.pb3",
                &flatbuffers_table(*b"MONS")[..],
                Some(&flatbuffers),
            ),
            (
                "mismatched.protobin",
                &flatbuffers_table(*b"MONS")[..],
                Some(&flatbuffers),
            ),
            (
                "bad-layout.bfbs",
                &[16, 0, 0, 0, b'M', b'O', b'N', b'S'][..],
                Some(&flatbuffers),
            ),
        ] {
            let result = extract_binary_protocol_with_binding_or_inventory(
                Path::new(name),
                name,
                bytes,
                binding,
            )
            .expect("inventory fallback");
            let root = result.nodes.first().expect("inventory root");
            assert_eq!(
                root.extra
                    .get("format_capability")
                    .and_then(serde_json::Value::as_str),
                Some("inventory_only"),
                "{name}"
            );
            let expected = if binding.is_some() {
                "schema_binding_rejected_payload_not_decoded"
            } else {
                "schema_required_binary_payload_not_decoded"
            };
            assert_eq!(
                root.extra
                    .get("diagnostic")
                    .and_then(serde_json::Value::as_str),
                Some(expected),
                "{name}"
            );
            assert!(result.edges.is_empty(), "{name}");
        }
    }

    #[test]
    fn protocol_byte_adapter_never_reads_its_logical_path() {
        let path = Path::new("/definitely-not-present-graphoxide-protocol-test/api.proto");
        let result = extract_protocol_bytes(
            path,
            "logical/api.proto",
            b"message Request { string name = 1; }",
        )
        .expect("borrowed protocol bytes must not open the logical path");
        node(&result, "Request", "message");
    }

    #[test]
    fn malformed_text_does_not_create_unbounded_or_comment_derived_facts() {
        let result = extract(
            "unsafe.proto",
            "// message Fake { string leaked = 1; }\nmessage Real { string name = 1; }",
        );
        assert!(result.nodes.iter().any(|node| node.label == "Real"));
        assert!(result.nodes.iter().all(|node| node.label != "Fake"));
        assert!(result.nodes.len() < 16);
    }

    #[test]
    fn verified_protobuf_descriptor_enables_schema_full_payload_facts() {
        let descriptor = descriptor_set_for_string_field("example", "Envelope", "body", 1);
        let binding = VerifiedBinarySchemaBinding::protobuf_descriptor(
            "example-envelope-v1",
            &descriptor,
            "example.Envelope",
        )
        .expect("validated FileDescriptorSet");
        let result = extract_bound_binary_protocol_bytes(
            Path::new("payload.pb"),
            "payload.pb",
            &[0x0a, 0x02, b'o', b'k'],
            &binding,
        )
        .expect("payload agrees with descriptor");
        let root = result.nodes.first().expect("protocol root");
        assert_eq!(
            root.extra
                .get("format_capability")
                .and_then(serde_json::Value::as_str),
            Some("schema_full")
        );
        assert_eq!(
            root.extra
                .get("schema_binding_id")
                .and_then(serde_json::Value::as_str),
            Some("example-envelope-v1")
        );
        assert!(result.nodes.iter().any(|node| {
            node.label == "example.Envelope"
                && node.extra.get("type").and_then(serde_json::Value::as_str)
                    == Some("schema_message")
        }));
        assert!(result.nodes.iter().any(|node| {
            node.label == "body"
                && node
                    .extra
                    .get("declared_type")
                    .and_then(serde_json::Value::as_str)
                    == Some("string")
        }));
    }

    #[test]
    fn protobuf_binding_rejects_malformed_descriptor_missing_type_and_wire_mismatch() {
        let malformed = VerifiedBinarySchemaBinding::protobuf_descriptor(
            "bad",
            &[0x0a, 0x80],
            "example.Envelope",
        );
        assert!(matches!(
            malformed,
            Err(SchemaBindingError::InvalidProtobufDescriptor(_))
        ));

        let descriptor = descriptor_set_for_string_field("example", "Envelope", "body", 1);
        let missing = VerifiedBinarySchemaBinding::protobuf_descriptor(
            "missing",
            &descriptor,
            "example.Missing",
        );
        assert!(matches!(
            missing,
            Err(SchemaBindingError::MissingProtobufMessage(_))
        ));

        let binding = VerifiedBinarySchemaBinding::protobuf_descriptor(
            "example-envelope-v1",
            &descriptor,
            "example.Envelope",
        )
        .expect("binding");
        let error = extract_bound_binary_protocol_bytes(
            Path::new("payload.pb"),
            "payload.pb",
            &[0x08, 0x01],
            &binding,
        )
        .expect_err("string field cannot use varint wire type");
        assert!(error.to_string().contains("incompatible"));
    }

    #[test]
    fn protobuf_binding_rejects_duplicate_reserved_and_excessively_nested_descriptors() {
        let field = message_field("body", 1);
        let duplicate = descriptor_set_for_message_body(
            "example",
            b"Envelope",
            &[field.as_slice(), field.as_slice()],
            &[],
        );
        assert!(matches!(
            VerifiedBinarySchemaBinding::protobuf_descriptor(
                "duplicate-field",
                &duplicate,
                "example.Envelope",
            ),
            Err(SchemaBindingError::InvalidProtobufDescriptor(
                "message declares duplicate field number"
            ))
        ));

        let reserved = descriptor_set_for_string_field("example", "Envelope", "body", 19_000);
        assert!(matches!(
            VerifiedBinarySchemaBinding::protobuf_descriptor(
                "reserved-field",
                &reserved,
                "example.Envelope",
            ),
            Err(SchemaBindingError::InvalidProtobufDescriptor(
                "field number is out of range"
            ))
        ));

        let mut nested = length_field(1, b"Leaf");
        for _ in 0..MAX_NESTING {
            nested = descriptor_set_message(b"Nested", &[], &[nested.as_slice()]);
        }
        let deeply_nested =
            descriptor_set_for_message_body("example", b"Root", &[], &[nested.as_slice()]);
        assert!(matches!(
            VerifiedBinarySchemaBinding::protobuf_descriptor(
                "deeply-nested",
                &deeply_nested,
                "example.Root",
            ),
            Err(SchemaBindingError::InvalidProtobufDescriptor(
                "message nesting exceeds bound"
            ))
        ));
    }

    #[test]
    fn verified_flatbuffers_metadata_enables_schema_full_layout_facts() {
        let schema =
            b"namespace example;\ntable Monster {\n  name:string;\n}\nfile_identifier \"MONS\";\n";
        let binding = VerifiedBinarySchemaBinding::flatbuffers_schema_metadata(
            "example-monster-v1",
            schema,
            "Monster",
            Some(*b"MONS"),
        )
        .expect("validated schema metadata");
        let result = extract_bound_binary_protocol_bytes(
            Path::new("monster.bfbs"),
            "monster.bfbs",
            &flatbuffers_table(*b"MONS"),
            &binding,
        )
        .expect("valid FlatBuffers table");
        let root = result.nodes.first().expect("protocol root");
        assert_eq!(
            root.extra
                .get("format_capability")
                .and_then(serde_json::Value::as_str),
            Some("schema_full")
        );
        assert_eq!(
            root.extra
                .get("payload_validation")
                .and_then(serde_json::Value::as_str),
            Some("flatbuffer_table_layout")
        );
        node(&result, "Monster", "table");
        node(&result, "name", "field");
    }

    #[test]
    fn flatbuffers_binding_rejects_schema_identifier_payload_and_layout_mismatches() {
        let schema = b"table Monster {\n  name:string;\n}\nfile_identifier \"MONS\";\n";
        let wrong_schema_identifier = VerifiedBinarySchemaBinding::flatbuffers_schema_metadata(
            "wrong-id",
            schema,
            "Monster",
            Some(*b"NOPE"),
        );
        assert!(matches!(
            wrong_schema_identifier,
            Err(SchemaBindingError::FlatbuffersIdentifierMismatch)
        ));
        let missing_root = VerifiedBinarySchemaBinding::flatbuffers_schema_metadata(
            "missing-root",
            schema,
            "Other",
            Some(*b"MONS"),
        );
        assert!(matches!(
            missing_root,
            Err(SchemaBindingError::InvalidFlatbuffersSchema(_))
        ));

        let binding = VerifiedBinarySchemaBinding::flatbuffers_schema_metadata(
            "example-monster-v1",
            schema,
            "Monster",
            Some(*b"MONS"),
        )
        .expect("binding");
        let identifier_error = extract_bound_binary_protocol_bytes(
            Path::new("monster.bfbs"),
            "monster.bfbs",
            &flatbuffers_table(*b"NOPE"),
            &binding,
        )
        .expect_err("mismatched identifier");
        assert!(identifier_error.to_string().contains("identifier"));

        let malformed = [16, 0, 0, 0, b'M', b'O', b'N', b'S'];
        let layout_error = extract_bound_binary_protocol_bytes(
            Path::new("monster.bfbs"),
            "monster.bfbs",
            &malformed,
            &binding,
        )
        .expect_err("truncated root table");
        assert!(layout_error.to_string().contains("out of bounds"));
    }

    #[test]
    fn arrow_ipc_file_stream_and_feather_v2_extract_partial_embedded_schema() {
        for (name, bytes, layout) in [
            ("events.arrow", arrow_ipc_file_fixture(), "ipc_file_footer"),
            (
                "events.arrows",
                arrow_ipc_stream_fixture(),
                "ipc_stream_message",
            ),
            (
                "events.feather",
                arrow_ipc_file_fixture(),
                "ipc_file_footer",
            ),
        ] {
            let result = extract_protocol_bytes(Path::new(name), name, &bytes)
                .expect("Arrow schema extraction must not need path I/O");
            let root = result.nodes.first().expect("Arrow schema root");
            assert_eq!(
                root.extra
                    .get("format_capability")
                    .and_then(serde_json::Value::as_str),
                Some("structural_partial"),
                "{name}"
            );
            assert_eq!(
                root.extra
                    .get("parse_status")
                    .and_then(serde_json::Value::as_str),
                Some("partial"),
                "{name}"
            );
            assert_eq!(
                root.extra
                    .get("schema_source")
                    .and_then(serde_json::Value::as_str),
                Some("arrow_ipc_schema_metadata"),
                "{name}"
            );
            assert_eq!(
                root.extra
                    .get("arrow_layout")
                    .and_then(serde_json::Value::as_str),
                Some(layout),
                "{name}"
            );
            assert_eq!(
                root.extra
                    .get("data_values_decoded")
                    .and_then(serde_json::Value::as_bool),
                Some(false),
                "{name}"
            );
            let field = result
                .nodes
                .iter()
                .find(|node| node.label == "name")
                .expect("Arrow field declaration");
            assert_eq!(
                field
                    .extra
                    .get("declared_type")
                    .and_then(serde_json::Value::as_str),
                Some("utf8")
            );
            assert_eq!(
                field
                    .extra
                    .get("nullable")
                    .and_then(serde_json::Value::as_bool),
                Some(true)
            );
        }
    }

    #[test]
    fn parquet_footer_extracts_partial_schema_hierarchy_and_metadata_keys_without_values() {
        let bytes = parquet_fixture();
        let result = extract_protocol_bytes(
            Path::new("/intentionally/missing/events.parquet"),
            "events.parquet",
            &bytes,
        )
        .expect("Parquet footer extraction must not need path I/O");
        let root = result.nodes.first().expect("Parquet schema root");
        assert_eq!(
            root.extra
                .get("format_capability")
                .and_then(serde_json::Value::as_str),
            Some("structural_partial")
        );
        assert_eq!(
            root.extra
                .get("parse_status")
                .and_then(serde_json::Value::as_str),
            Some("partial")
        );
        assert_eq!(
            root.extra
                .get("schema_source")
                .and_then(serde_json::Value::as_str),
            Some("parquet_file_metadata")
        );
        assert_eq!(
            root.extra
                .get("data_values_decoded")
                .and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            root.extra
                .get("schema_field_count")
                .and_then(serde_json::Value::as_u64),
            Some(2)
        );
        let field = result
            .nodes
            .iter()
            .find(|node| node.label == "name")
            .expect("Parquet field declaration");
        assert_eq!(
            field
                .extra
                .get("declared_type")
                .and_then(serde_json::Value::as_str),
            Some("utf8 (byte_array)")
        );
        assert_eq!(
            field
                .extra
                .get("nullable")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        let metadata = result
            .nodes
            .iter()
            .find(|node| node.label == "producer")
            .expect("Parquet custom metadata key");
        assert_eq!(
            metadata
                .extra
                .get("metadata_value_decoded")
                .and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert!(!result.nodes.iter().any(|node| node.label == "v1"));
    }

    #[test]
    fn malformed_or_limit_exceeding_columnar_metadata_is_inventory_only() {
        let mut corrupt = parquet_fixture();
        corrupt[0] = b'X';
        let corrupt_result =
            extract_protocol_bytes(Path::new("corrupt.parquet"), "corrupt.parquet", &corrupt)
                .expect("corrupt payload receives safe inventory");
        let corrupt_root = corrupt_result.nodes.first().expect("inventory root");
        assert_eq!(
            corrupt_root
                .extra
                .get("format_capability")
                .and_then(serde_json::Value::as_str),
            Some("inventory_only")
        );
        assert_eq!(
            corrupt_root
                .extra
                .get("diagnostic")
                .and_then(serde_json::Value::as_str),
            Some("columnar_schema_metadata_invalid_or_unavailable")
        );

        let feather_v1 = extract_protocol_bytes(
            Path::new("legacy.feather"),
            "legacy.feather",
            b"FEA1legacy-feather-v1-footer",
        )
        .expect("unsupported Feather v1 receives safe inventory");
        assert_eq!(
            feather_v1.nodes[0]
                .extra
                .get("diagnostic")
                .and_then(serde_json::Value::as_str),
            Some("feather_v1_schema_metadata_unsupported")
        );

        let mut oversized_footer = b"ARROW1\0\0".to_vec();
        oversized_footer
            .extend_from_slice(&((MAX_COLUMNAR_METADATA_BYTES + 1) as u32).to_le_bytes());
        oversized_footer.extend_from_slice(b"ARROW1");
        let limit_result =
            extract_protocol_bytes(Path::new("limit.arrow"), "limit.arrow", &oversized_footer)
                .expect("oversized metadata receives safe inventory");
        assert_eq!(
            limit_result.nodes[0]
                .extra
                .get("format_capability")
                .and_then(serde_json::Value::as_str),
            Some("inventory_only")
        );
    }

    fn arrow_ipc_file_fixture() -> Vec<u8> {
        let footer = arrow_footer_flatbuffer();
        let mut bytes = b"ARROW1\0\0".to_vec();
        bytes.extend_from_slice(&footer);
        bytes.extend_from_slice(&(footer.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"ARROW1");
        bytes
    }

    fn arrow_ipc_stream_fixture() -> Vec<u8> {
        let message = arrow_stream_message_flatbuffer();
        let mut bytes = 0xffff_ffff_u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&(message.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&message);
        bytes
    }

    // These fixtures are hand-written encodings following Arrow's published
    // FlatBuffer layout. They do not reuse the production reader, so a wrong
    // table/vtable offset, union discriminator, or footer framing is caught by
    // the tests rather than mirrored by a shared serializer.
    fn arrow_footer_flatbuffer() -> Vec<u8> {
        let mut bytes = vec![0; 92];
        put_u32(&mut bytes, 0, 16); // root -> Footer table
        put_u16(&mut bytes, 8, 8); // Footer vtable length
        put_u16(&mut bytes, 10, 8); // Footer object length
        put_u16(&mut bytes, 12, 0); // version absent
        put_u16(&mut bytes, 14, 4); // schema
        put_i32(&mut bytes, 16, 8); // Footer table -> vtable
        put_u32(&mut bytes, 20, 16); // schema table at 36

        put_u16(&mut bytes, 24, 10); // Schema vtable length
        put_u16(&mut bytes, 26, 12); // Schema object length
        put_u16(&mut bytes, 28, 0); // endianness absent
        put_u16(&mut bytes, 30, 4); // fields vector
        put_u16(&mut bytes, 32, 0); // custom metadata absent
        put_i32(&mut bytes, 36, 12); // Schema table -> vtable
        put_u32(&mut bytes, 40, 8); // fields vector at 48
        put_u32(&mut bytes, 48, 1); // one field
        put_u32(&mut bytes, 52, 20); // field table at 72

        put_u16(&mut bytes, 56, 16); // Field vtable length
        put_u16(&mut bytes, 58, 10); // Field object length
        put_u16(&mut bytes, 60, 4); // name
        put_u16(&mut bytes, 62, 8); // nullable
        put_u16(&mut bytes, 64, 9); // type_type union discriminator
        put_u16(&mut bytes, 66, 0); // type table not required for Utf8
        put_u16(&mut bytes, 68, 0); // dictionary absent
        put_u16(&mut bytes, 70, 0); // children absent
        put_i32(&mut bytes, 72, 16); // Field table -> vtable
        put_u32(&mut bytes, 76, 8); // string at 84
        bytes[80] = 1; // nullable
        bytes[81] = 5; // Arrow Type::Utf8
        put_u32(&mut bytes, 84, 4);
        bytes[88..92].copy_from_slice(b"name");
        bytes
    }

    fn arrow_stream_message_flatbuffer() -> Vec<u8> {
        let mut bytes = vec![0; 88];
        put_u32(&mut bytes, 0, 16); // root -> Message
        put_u16(&mut bytes, 6, 10); // Message vtable length
        put_u16(&mut bytes, 8, 12); // Message object length
        put_u16(&mut bytes, 10, 0); // version absent
        put_u16(&mut bytes, 12, 4); // header_type
        put_u16(&mut bytes, 14, 8); // header
        put_i32(&mut bytes, 16, 10); // Message table -> vtable
        bytes[20] = 1; // MessageHeader::Schema
        put_u32(&mut bytes, 24, 12); // Schema table at 36

        put_u16(&mut bytes, 28, 8); // Schema vtable length
        put_u16(&mut bytes, 30, 8); // Schema object length
        put_u16(&mut bytes, 32, 0); // endianness absent
        put_u16(&mut bytes, 34, 4); // fields
        put_i32(&mut bytes, 36, 8); // Schema table -> vtable
        put_u32(&mut bytes, 40, 4); // fields vector at 44
        put_u32(&mut bytes, 44, 1);
        put_u32(&mut bytes, 48, 20); // Field table at 68

        put_u16(&mut bytes, 52, 16); // Field vtable length
        put_u16(&mut bytes, 54, 10); // Field object length
        put_u16(&mut bytes, 56, 4);
        put_u16(&mut bytes, 58, 8);
        put_u16(&mut bytes, 60, 9);
        put_i32(&mut bytes, 68, 16);
        put_u32(&mut bytes, 72, 8); // string at 80
        bytes[76] = 1;
        bytes[77] = 5;
        put_u32(&mut bytes, 80, 4);
        bytes[84..88].copy_from_slice(b"name");
        bytes
    }

    fn parquet_fixture() -> Vec<u8> {
        let metadata = vec![
            0x15, 0x02, // version = 1
            0x19, 0x2c, // schema list: two struct entries
            0x48, 0x06, b's', b'c', b'h', b'e', b'm', b'a', // root name
            0x15, 0x02, // root has one child
            0x00, // root struct stop
            0x15, 0x0c, // BYTE_ARRAY physical type
            0x25, 0x02, // OPTIONAL repetition
            0x18, 0x04, b'n', b'a', b'm', b'e', // child name
            0x25, 0x00, // UTF8 converted type
            0x00, // child struct stop
            0x16, 0x00, // num_rows = 0
            0x19, 0x0c, // empty row_groups list
            0x19, 0x1c, // one key-value struct
            0x18, 0x08, b'p', b'r', b'o', b'd', b'u', b'c', b'e', b'r', 0x18, 0x02, b'v',
            b'1', // value intentionally must not surface
            0x00, // key-value struct stop
            0x00, // FileMetaData struct stop
        ];
        let mut bytes = b"PAR1".to_vec();
        bytes.extend_from_slice(&metadata);
        bytes.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"PAR1");
        bytes
    }

    fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_i32(bytes: &mut [u8], offset: usize, value: i32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn descriptor_set_for_string_field(
        package: &str,
        message: &str,
        field_name: &str,
        number: u32,
    ) -> Vec<u8> {
        let field = message_field(field_name, number);
        descriptor_set_for_message_body(package, message.as_bytes(), &[field.as_slice()], &[])
    }

    fn descriptor_set_for_message_body(
        package: &str,
        message: &[u8],
        fields: &[&[u8]],
        nested_messages: &[&[u8]],
    ) -> Vec<u8> {
        let descriptor = descriptor_set_message(message, fields, nested_messages);
        let file = length_field(2, package.as_bytes())
            .into_iter()
            .chain(length_field(4, &descriptor))
            .collect::<Vec<_>>();
        length_field(1, &file)
    }

    fn descriptor_set_message(name: &[u8], fields: &[&[u8]], nested_messages: &[&[u8]]) -> Vec<u8> {
        let mut descriptor = length_field(1, name);
        for field in fields {
            descriptor.extend(length_field(2, field));
        }
        for nested in nested_messages {
            descriptor.extend(length_field(3, nested));
        }
        descriptor
    }

    fn message_field(name: &str, number: u32) -> Vec<u8> {
        let mut field = length_field(1, name.as_bytes());
        field.extend(varint_field(3, number));
        field.extend(varint_field(4, 1));
        field.extend(varint_field(5, 9));
        field
    }

    fn length_field(number: u8, value: &[u8]) -> Vec<u8> {
        let mut field = vec![(number << 3) | 2];
        field.extend(varint(value.len() as u64));
        field.extend(value);
        field
    }

    fn varint_field(number: u8, value: u32) -> Vec<u8> {
        let mut field = vec![number << 3];
        field.extend(varint(u64::from(value)));
        field
    }

    fn varint(mut value: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            bytes.push(byte);
            if value == 0 {
                return bytes;
            }
        }
    }

    fn flatbuffers_table(identifier: [u8; 4]) -> Vec<u8> {
        let mut bytes = vec![0; 20];
        bytes[0..4].copy_from_slice(&16u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&identifier);
        bytes[8..10].copy_from_slice(&4u16.to_le_bytes());
        bytes[10..12].copy_from_slice(&4u16.to_le_bytes());
        bytes[16..20].copy_from_slice(&8i32.to_le_bytes());
        bytes
    }
}
