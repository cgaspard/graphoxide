//! Central byte-only dispatch for registered structured formats.
//!
//! Language extractors keep precedence in `engine`; this module owns the
//! non-language formats and turns malformed or schema-required bytes into
//! deterministic inventory facts rather than attempting path I/O or aborting
//! a complete project extraction.

use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node, CONTAINER_SOURCE_ATTRIBUTE};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Immutable input handed from the I/O owner to one structured byte adapter.
/// It intentionally contains a logical path for classification/identity and
/// a borrowed source slice, never a filesystem handle or a path-read method.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReadyStructuredInput<'a> {
    pub path: &'a Path,
    pub source_file: &'a str,
    pub bytes: &'a [u8],
}

/// Stable internal contract for bounded structured byte extractors.
///
/// The trait is deliberately narrower than the project engine: implementations
/// receive ready bytes and parser limits only, so the type system makes it
/// impossible to smuggle a filesystem capability through this API.
pub(crate) trait ByteStructuredExtractor: Send + Sync {
    fn extract(
        &self,
        input: ReadyStructuredInput<'_>,
        limits: &crate::format_registry::FormatLimits,
    ) -> anyhow::Result<Extraction>;
}

type ByteStructuredExtractFn = for<'a> fn(
    ReadyStructuredInput<'a>,
    &crate::format_registry::FormatLimits,
) -> anyhow::Result<Extraction>;

struct FunctionByteStructuredExtractor(ByteStructuredExtractFn);

impl ByteStructuredExtractor for FunctionByteStructuredExtractor {
    fn extract(
        &self,
        input: ReadyStructuredInput<'_>,
        limits: &crate::format_registry::FormatLimits,
    ) -> anyhow::Result<Extraction> {
        if input.bytes.len() as u64 > limits.max_input_bytes {
            anyhow::bail!(
                "{} bytes exceeds registered {} byte limit",
                input.bytes.len(),
                limits.max_input_bytes
            );
        }
        let extraction = (self.0)(input, limits)?;
        let fact_count = extraction_fact_count(&extraction)
            .ok_or_else(|| anyhow::anyhow!("structured output fact count overflow"))?;
        anyhow::ensure!(
            fact_count <= limits.max_records,
            "{fact_count} output facts exceeds registered {} fact limit",
            limits.max_records
        );
        Ok(extraction)
    }
}

static ENGINEERING_EXTRACTOR: FunctionByteStructuredExtractor =
    FunctionByteStructuredExtractor(extract_engineering);
static SIMULATION_EXTRACTOR: FunctionByteStructuredExtractor =
    FunctionByteStructuredExtractor(extract_simulation);
static DIAGRAM_EXTRACTOR: FunctionByteStructuredExtractor =
    FunctionByteStructuredExtractor(extract_diagram);
static PROTOCOL_EXTRACTOR: FunctionByteStructuredExtractor =
    FunctionByteStructuredExtractor(extract_protocol);
static STRUCTURED_EXTRACTOR: FunctionByteStructuredExtractor =
    FunctionByteStructuredExtractor(extract_structured);

fn extract_engineering(
    input: ReadyStructuredInput<'_>,
    _: &crate::format_registry::FormatLimits,
) -> anyhow::Result<Extraction> {
    crate::engineering::extract_engineering_bytes(input.path, input.source_file, input.bytes)
}

fn extract_simulation(
    input: ReadyStructuredInput<'_>,
    _: &crate::format_registry::FormatLimits,
) -> anyhow::Result<Extraction> {
    crate::simulation::extract_simulation_bytes(input.path, input.source_file, input.bytes)
}

fn extract_diagram(
    input: ReadyStructuredInput<'_>,
    _: &crate::format_registry::FormatLimits,
) -> anyhow::Result<Extraction> {
    crate::diagrams::extract_diagram_bytes(input.path, input.source_file, input.bytes)
}

fn extract_protocol(
    input: ReadyStructuredInput<'_>,
    _: &crate::format_registry::FormatLimits,
) -> anyhow::Result<Extraction> {
    crate::protocols::extract_protocol_bytes(input.path, input.source_file, input.bytes)
}

fn extract_structured(
    input: ReadyStructuredInput<'_>,
    _: &crate::format_registry::FormatLimits,
) -> anyhow::Result<Extraction> {
    crate::structured::extract_structured_bytes(input.path, input.source_file, input.bytes)
        .map(|result| {
            let mut extraction = result.extraction;
            if !result.diagnostics.is_empty()
                && let Some(root) = extraction.nodes.first_mut()
            {
                root.extra.insert(
                    "structured_diagnostic_count".into(),
                    result.diagnostics.len().into(),
                );
            }
            extraction
        })
        .ok_or_else(|| anyhow::anyhow!("structured adapter does not own this path"))
}

pub(crate) fn extract_registered_format(
    path: &Path,
    source_file: &str,
    source: &[u8],
    extension: &str,
) -> Option<Extraction> {
    extract_registered_format_with_allowance(path, source_file, source, extension, None)
}

/// Dispatch a registered format under an optional isolated parser-arena
/// allowance. Legacy byte callers pass no allowance and retain fixed limits.
pub(crate) fn extract_registered_format_with_allowance(
    path: &Path,
    source_file: &str,
    source: &[u8],
    extension: &str,
    parser_allowance_bytes: Option<usize>,
) -> Option<Extraction> {
    let limits = parser_allowance_bytes.map_or_else(
        crate::containers::ContainerLimits::default,
        bounded_container_limits,
    );
    let mut dispatch_budget =
        RecursiveDispatchBudget::new_with_parser_allowance(limits, parser_allowance_bytes);
    extract_registered_format_at_depth(
        path,
        source_file,
        source,
        extension,
        0,
        &mut dispatch_budget,
    )
}

/// Byte-only format dispatch with an explicit archive depth and shared
/// recursive admission budget. The public entrypoint creates the root budget;
/// children can only reach this function through a validated archive member.
fn extract_registered_format_at_depth(
    path: &Path,
    source_file: &str,
    source: &[u8],
    extension: &str,
    recursion_depth: u16,
    dispatch_budget: &mut RecursiveDispatchBudget,
) -> Option<Extraction> {
    let registry = crate::format_registry::format_registry();
    let semantic_parser_allowance = dispatch_budget.parser_allowance_at_depth(recursion_depth);
    let package_manifest = crate::manifest_ingest::is_package_manifest_path(path);
    let path_spec = registry.find_by_path(path);
    let parser_plan_is_rejected = parser_plan_rejected(semantic_parser_allowance, source.len());
    // JSON configuration classification may deserialize the complete value.
    // A constrained runtime must reject before that classification allocation,
    // including for compatibility-sensitive named configuration files.
    if extension == "json" && parser_plan_is_rejected && path_spec.is_some() {
        return Some(rejected_inventory_extraction(
            path,
            source_file,
            "parser_arena_budget",
        ));
    }
    let use_json_config =
        extension == "json" && crate::json_config::should_use_json_config(path, source);
    // Valid package manifests keep their established configuration facts. An
    // invalid or oversized JSON package manifest instead enters the bounded
    // structured adapter so malformed untrusted input remains a diagnostic,
    // never an extraction failure.
    if package_manifest && (extension != "json" || use_json_config) {
        return None;
    }
    let malformed_or_oversized_named_json = extension == "json"
        && !use_json_config
        && path_spec.is_some_and(|spec| spec.id.as_str() == "package-manifest");
    let declared_spec =
        if (package_manifest && extension == "json") || malformed_or_oversized_named_json {
            registry.find_by_extension("json")
        } else {
            path_spec
        };
    // Markdown and MCP configuration have compatibility-sensitive fallbacks.
    // They take precedence over a representation probe exactly as before;
    // ordinary source-code, text, and Terraform suffixes do not.
    if declared_spec
        .is_some_and(|spec| matches!(spec.id.as_str(), "markdown" | "mcp-configuration"))
    {
        return None;
    }
    // JSON configuration has a compatibility-sensitive fallback extractor
    // with its established JSONC error contract. Keep that graph/error shape
    // until the byte adapter can preserve it exactly.
    if use_json_config {
        return None;
    }
    // Dream Maker icon stacks are PNG containers, but their Description
    // metadata has a dedicated byte parser in the compatibility fallback.
    // Do not let generic PNG inventory precedence mask those semantic facts.
    if extension == "dmi" {
        return None;
    }
    // Representation has precedence over a suffix family. A ZIP-backed
    // Draw.io, FMU, USDZ, IFCZIP, or Office document is a bounded container
    // inventory on the CPU plane; do not first hand compressed bytes to a
    // text diagram or model parser merely because its filename is familiar.
    if let Some(inventory) =
        container_inventory_extraction(path, source_file, source, recursion_depth, dispatch_budget)
    {
        return Some(inventory);
    }
    // Some protocol classifiers deserialize JSON to distinguish a generic
    // suffix. Apply the arena preflight before classification as well as
    // before the selected parser, so rejected inputs never allocate a DOM on
    // the way to the same deterministic inventory result.
    if parser_plan_is_rejected {
        let budgeted_spec = declared_spec.or_else(|| registry.find_by_magic(source).next());
        if budgeted_spec.is_some_and(|spec| {
            matches!(
                spec.adapter(),
                crate::format_registry::ByteAdapterKind::Engineering
                    | crate::format_registry::ByteAdapterKind::Simulation
                    | crate::format_registry::ByteAdapterKind::Diagram
                    | crate::format_registry::ByteAdapterKind::Protocol
                    | crate::format_registry::ByteAdapterKind::Structured
            )
        }) {
            return Some(rejected_inventory_extraction(
                path,
                source_file,
                "parser_arena_budget",
            ));
        }
    }
    // Generic JSON/YAML only becomes a protocol route with an explicit API
    // marker. A named API document is selected from the registry below.
    if crate::protocols::looks_like_api_description(path, source) {
        return Some(adapter_or_budget_rejected(
            &PROTOCOL_EXTRACTOR,
            path,
            source_file,
            source,
            semantic_parser_allowance,
            "protocol_parse_failed",
        ));
    }

    let spec = declared_spec.or_else(|| registry.find_by_magic(source).next())?;
    use crate::format_registry::ByteAdapterKind;
    match spec.adapter() {
        // Engine-owned families must fall through to the language/fallback
        // driver below this adapter. Returning an inventory root here would
        // mask their semantic extractor merely because they are registered.
        ByteAdapterKind::Engine => None,
        ByteAdapterKind::Engineering => Some(adapter_or_budget_rejected(
            &ENGINEERING_EXTRACTOR,
            path,
            source_file,
            source,
            semantic_parser_allowance,
            "engineering_parse_failed",
        )),
        ByteAdapterKind::Simulation => Some(
            if parser_plan_rejected(semantic_parser_allowance, source.len()) {
                rejected_inventory_extraction(path, source_file, "parser_arena_budget")
            } else {
                adapter_or_rejected(
                    extract_simulation_for_spec(
                        spec,
                        path,
                        source_file,
                        source,
                        semantic_parser_allowance,
                    ),
                    path,
                    source_file,
                    "simulation_parse_failed",
                )
            },
        ),
        ByteAdapterKind::Diagram => Some(adapter_or_budget_rejected(
            &DIAGRAM_EXTRACTOR,
            path,
            source_file,
            source,
            semantic_parser_allowance,
            "diagram_parse_failed",
        )),
        ByteAdapterKind::Protocol => Some(adapter_or_budget_rejected(
            &PROTOCOL_EXTRACTOR,
            path,
            source_file,
            source,
            semantic_parser_allowance,
            "protocol_parse_failed",
        )),
        ByteAdapterKind::Structured => Some(adapter_or_budget_rejected(
            &STRUCTURED_EXTRACTOR,
            path,
            source_file,
            source,
            semantic_parser_allowance,
            "structured_parse_failed",
        )),
        // `pdf-extract` owns compressed-stream decoding internally and cannot
        // consume the runtime's pre-allocation credits. Keep isolated PDF
        // extraction inventory-only until that decoder exposes an allocator or
        // decompressed-byte ceiling; the legacy facade retains fixed behavior.
        ByteAdapterKind::Pdf if dispatch_budget.parser_allowance_bytes.is_some() => {
            let mut extraction =
                rejected_inventory_extraction(path, source_file, "parser_arena_unenforceable");
            if let Some(root) = extraction.nodes.first_mut() {
                root.extra
                    .insert("format_capability".into(), "inventory_only".into());
            }
            Some(extraction)
        }
        ByteAdapterKind::Pdf => Some(extract_pdf_bytes(path, source_file, source)),
        // A suffix can identify a container/media representation even when a
        // malformed buffer lacks a recognizable signature. Claim it with an
        // explicit rejection instead of leaking it to a generic text parser.
        ByteAdapterKind::ContainerMedia => Some(rejected_inventory_extraction(
            path,
            source_file,
            "container_or_media_representation_unrecognized",
        )),
        ByteAdapterKind::Inventory => Some(registered_inventory_extraction(
            path,
            source_file,
            source.len(),
            spec,
        )),
    }
}

fn extract_simulation_for_spec(
    spec: &crate::format_registry::FormatSpec,
    path: &Path,
    source_file: &str,
    source: &[u8],
    parser_allowance_bytes: Option<usize>,
) -> anyhow::Result<Extraction> {
    if crate::simulation::format_for_path(path).is_some() {
        return extract_through(
            &SIMULATION_EXTRACTOR,
            path,
            source_file,
            source,
            parser_allowance_bytes,
        );
    }

    // Magic-only classification has no filename from which the simulation
    // adapter can infer an inventory format. Preserve the registry's magic
    // contract without treating the bytes as a semantic text asset.
    let format = match spec.id.as_str() {
        "openusd-inventory" => crate::simulation::SimulationFormat::UsdcInventory,
        "simulation-container" => crate::simulation::SimulationFormat::GlbInventory,
        _ => anyhow::bail!(
            "simulation registry entry {} cannot classify a magic-only input",
            spec.id.as_str()
        ),
    };
    extract_with_parser_plan(parser_allowance_bytes, source.len(), || {
        crate::simulation::extract_simulation_format_bytes(format, path, source_file, source)
    })
}

fn extract_through(
    extractor: &dyn ByteStructuredExtractor,
    path: &Path,
    source_file: &str,
    source: &[u8],
    parser_allowance_bytes: Option<usize>,
) -> anyhow::Result<Extraction> {
    let limits = crate::format_registry::format_registry()
        .find_by_path(path)
        .or_else(|| {
            crate::format_registry::format_registry()
                .find_by_magic(source)
                .next()
        })
        .map(|spec| spec.limits)
        .unwrap_or(crate::format_registry::TEXT_LIMITS);
    extract_with_parser_plan(parser_allowance_bytes, source.len(), || {
        extractor.extract(
            ReadyStructuredInput {
                path,
                source_file,
                bytes: source,
            },
            &limits,
        )
    })
}

fn extract_with_parser_plan(
    parser_allowance_bytes: Option<usize>,
    source_bytes: usize,
    operation: impl FnOnce() -> anyhow::Result<Extraction>,
) -> anyhow::Result<Extraction> {
    let Some(allowance_bytes) = parser_allowance_bytes else {
        return operation();
    };
    let plan = crate::parser_budget::ParserPlan::for_source(allowance_bytes, source_bytes)
        .ok_or_else(|| anyhow::anyhow!("parser arena budget rejected semantic extraction"))?;
    let max_facts = plan.max_facts();
    let (result, exhausted) = crate::parser_budget::with_plan(plan, operation);
    let mut extraction = result?;
    let fact_count = extraction_fact_count(&extraction)
        .ok_or_else(|| anyhow::anyhow!("parser output fact count overflow"))?;
    anyhow::ensure!(
        fact_count <= max_facts,
        "parser output exceeded its dynamic fact allowance"
    );
    if exhausted && let Some(root) = extraction.nodes.first_mut() {
        root.extra.insert("parse_status".into(), "partial".into());
        root.extra
            .insert("parser_diagnostic".into(), "parser_arena_fact_limit".into());
    }
    Ok(extraction)
}

fn parser_plan_rejected(parser_allowance_bytes: Option<usize>, source_bytes: usize) -> bool {
    parser_allowance_bytes.is_some_and(|allowance_bytes| {
        crate::parser_budget::ParserPlan::for_source(allowance_bytes, source_bytes).is_none()
    })
}

fn adapter_or_budget_rejected(
    extractor: &dyn ByteStructuredExtractor,
    path: &Path,
    source_file: &str,
    source: &[u8],
    parser_allowance_bytes: Option<usize>,
    diagnostic: &'static str,
) -> Extraction {
    if parser_plan_rejected(parser_allowance_bytes, source.len()) {
        return rejected_inventory_extraction(path, source_file, "parser_arena_budget");
    }
    adapter_or_rejected(
        extract_through(extractor, path, source_file, source, parser_allowance_bytes),
        path,
        source_file,
        diagnostic,
    )
}

fn adapter_or_rejected(
    result: anyhow::Result<Extraction>,
    path: &Path,
    source_file: &str,
    diagnostic: &'static str,
) -> Extraction {
    result
        .ok()
        .filter(|extraction| !extraction.nodes.is_empty())
        .unwrap_or_else(|| rejected_inventory_extraction(path, source_file, diagnostic))
}

fn extract_pdf_bytes(path: &Path, source_file: &str, source: &[u8]) -> Extraction {
    let Ok(text) = pdf_extract::extract_text_from_mem(source) else {
        return rejected_inventory_extraction(path, source_file, "pdf_parse_failed");
    };
    let mut virtual_markdown = path.to_path_buf();
    virtual_markdown.set_extension("md");
    crate::fallback::extract_text_bytes(&virtual_markdown, source_file, text.as_bytes())
        .unwrap_or_else(|_| rejected_inventory_extraction(path, source_file, "pdf_text_rejected"))
}

/// Aggregate admission guard for one root archive tree.
///
/// Per-container validation protects decompression and metadata allocations.
/// This additional guard bounds the number and declared bytes of members that
/// may reach semantic adapters, plus every retained node, edge, hyperedge, and
/// member-inventory fact across the complete nested tree. Isolated ZIP input is
/// metadata-only because compressed members cannot borrow the source buffer;
/// legacy ZIP dispatch and zero-copy TAR dispatch retain their fixed bounds.
#[derive(Debug)]
struct RecursiveDispatchBudget {
    remaining_dispatch_members: usize,
    remaining_declared_bytes: u64,
    remaining_output_facts: usize,
    output_fact_limit: usize,
    container_limits: crate::containers::ContainerLimits,
    parser_allowance_bytes: Option<usize>,
}

impl RecursiveDispatchBudget {
    #[cfg(test)]
    fn new(limits: crate::containers::ContainerLimits) -> Self {
        // This test helper constrains aggregate child dispatch while retaining
        // the ordinary archive-inspection contract. Production allowance
        // construction uses `new_with_parser_allowance` directly.
        let mut budget =
            Self::new_with_parser_allowance(crate::containers::ContainerLimits::default(), None);
        budget.remaining_dispatch_members = limits.max_members;
        budget.remaining_declared_bytes = limits.max_total_uncompressed_bytes;
        budget
    }

    fn new_with_parser_allowance(
        limits: crate::containers::ContainerLimits,
        parser_allowance_bytes: Option<usize>,
    ) -> Self {
        let output_fact_limit = parser_allowance_bytes.map_or(
            crate::format_registry::CONTAINER_LIMITS.max_records,
            |allowance| {
                crate::format_registry::CONTAINER_LIMITS
                    .max_records
                    .min((allowance / (4 * 1024)).max(1))
            },
        );
        Self::with_output_fact_limit_and_parser_allowance(
            limits,
            output_fact_limit,
            parser_allowance_bytes,
        )
    }

    #[cfg(test)]
    fn with_output_fact_limit(
        limits: crate::containers::ContainerLimits,
        output_fact_limit: usize,
    ) -> Self {
        Self::with_output_fact_limit_and_parser_allowance(limits, output_fact_limit, None)
    }

    fn with_output_fact_limit_and_parser_allowance(
        limits: crate::containers::ContainerLimits,
        output_fact_limit: usize,
        parser_allowance_bytes: Option<usize>,
    ) -> Self {
        Self {
            remaining_dispatch_members: limits.max_members,
            remaining_declared_bytes: limits.max_total_uncompressed_bytes,
            remaining_output_facts: output_fact_limit,
            output_fact_limit,
            container_limits: limits,
            parser_allowance_bytes,
        }
    }

    fn admit_dispatch(&mut self, member: &crate::containers::ContainerMember) -> bool {
        let Some(remaining_bytes) = self
            .remaining_declared_bytes
            .checked_sub(member.declared_uncompressed_bytes)
        else {
            return false;
        };
        let Some(remaining_members) = self.remaining_dispatch_members.checked_sub(1) else {
            return false;
        };
        self.remaining_declared_bytes = remaining_bytes;
        self.remaining_dispatch_members = remaining_members;
        true
    }

    fn can_reserve_output_facts(&self, facts: usize) -> bool {
        facts <= self.remaining_output_facts
    }

    fn reserve_output_facts(&mut self, facts: usize) -> bool {
        let Some(remaining) = self.remaining_output_facts.checked_sub(facts) else {
            return false;
        };
        self.remaining_output_facts = remaining;
        true
    }

    fn release_output_facts(&mut self, facts: usize) {
        self.remaining_output_facts = self
            .remaining_output_facts
            .checked_add(facts)
            .expect("recursive output fact accounting must not overflow");
        debug_assert!(self.remaining_output_facts <= self.output_fact_limit);
    }

    fn remaining_output_facts(&self) -> usize {
        self.remaining_output_facts
    }

    fn parser_allowance_at_depth(&self, recursion_depth: u16) -> Option<usize> {
        self.parser_allowance_bytes.map(|allowance| {
            allowance
                .checked_shr(u32::from(recursion_depth).min(usize::BITS - 1))
                .unwrap_or(0)
        })
    }
}

fn bounded_container_limits(allowance_bytes: usize) -> crate::containers::ContainerLimits {
    let defaults = crate::containers::ContainerLimits::default();
    // Metadata, decoded SVG bytes, parser events, and retained inventory all
    // share this worker-local allowance. Keep each independent ceiling below a
    // disjoint conservative fraction rather than treating the static limits as
    // safe allocations for every runtime configuration.
    crate::containers::ContainerLimits {
        max_input_bytes: defaults
            .max_input_bytes
            .min(allowance_bytes.saturating_mul(16).max(1)),
        max_members: defaults.max_members.min((allowance_bytes / 1024).max(1)),
        max_central_directory_bytes: defaults
            .max_central_directory_bytes
            .min((allowance_bytes / 4).max(1)),
        max_member_uncompressed_bytes: defaults
            .max_member_uncompressed_bytes
            .min((allowance_bytes / 4).max(1) as u64),
        max_total_uncompressed_bytes: defaults
            .max_total_uncompressed_bytes
            .min((allowance_bytes / 4).max(1) as u64),
        max_member_name_bytes: defaults
            .max_member_name_bytes
            .min((allowance_bytes / 64).max(1)),
        max_svg_bytes: defaults.max_svg_bytes.min((allowance_bytes / 4).max(1)),
        max_svg_event_bytes: defaults
            .max_svg_event_bytes
            .min((allowance_bytes / 8).max(1)),
        max_svg_depth: defaults.max_svg_depth.min((allowance_bytes / 256).max(1)),
        max_svg_elements: defaults
            .max_svg_elements
            .min((allowance_bytes / (4 * 1024)).max(1)),
        max_svg_references: defaults
            .max_svg_references
            .min((allowance_bytes / (4 * 1024)).max(1)),
        max_svg_string_bytes: defaults
            .max_svg_string_bytes
            .min((allowance_bytes / 256).max(1)),
        max_media_probe_bytes: defaults
            .max_media_probe_bytes
            .min((allowance_bytes / 4).max(1)),
        ..defaults
    }
}

struct ExtractedContainerMember {
    path: String,
    extraction: Extraction,
    accounted_output_facts: usize,
}

fn extraction_fact_count(extraction: &Extraction) -> Option<usize> {
    extraction
        .nodes
        .len()
        .checked_add(extraction.edges.len())?
        .checked_add(extraction.hyperedges.len())
}

fn child_output_fact_count(extraction: &Extraction) -> Option<usize> {
    // Every child node also receives a deterministic parent-member `contains`
    // edge when its extraction is attached to the container inventory.
    extraction_fact_count(extraction)?.checked_add(extraction.nodes.len())
}

fn discard_extracted_members(
    extracted_members: &mut Vec<ExtractedContainerMember>,
    dispatch_budget: &mut RecursiveDispatchBudget,
) {
    for extracted in extracted_members.drain(..) {
        dispatch_budget.release_output_facts(extracted.accounted_output_facts);
    }
}

fn mark_container_source(extraction: &mut Extraction, source_file: &str) {
    let value = serde_json::Value::String(source_file.to_owned());
    for node in &mut extraction.nodes {
        node.extra
            .insert(CONTAINER_SOURCE_ATTRIBUTE.into(), value.clone());
    }
    for edge in &mut extraction.edges {
        edge.extra
            .insert(CONTAINER_SOURCE_ATTRIBUTE.into(), value.clone());
    }
    for hyperedge in &mut extraction.hyperedges {
        if let Some(object) = hyperedge.as_object_mut() {
            object.insert(CONTAINER_SOURCE_ATTRIBUTE.into(), value.clone());
        }
    }
}

fn virtual_member_path(container_path: &Path, member_path: &str) -> PathBuf {
    // `!/` is the reserved logical boundary documented by source discovery;
    // physical directories that could produce the same spelling are skipped.
    PathBuf::from(format!(
        "{}!/{member_path}",
        container_path.to_string_lossy()
    ))
}

fn virtual_member_source_file(source_file: &str, member_path: &str) -> String {
    format!("{source_file}!/{member_path}")
}

/// Apply the repository's sensitive-path policy to a normalized archive path
/// without ever probing that logical path on disk. Extensionless names need a
/// lexical fast path because the compatibility detector may inspect a shebang
/// when classifying an ordinary extensionless source file.
fn is_sensitive_container_member_path(member_path: &str) -> bool {
    let path = Path::new(member_path);
    let components = path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => {
                Some(value.to_string_lossy().to_ascii_lowercase())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if components.iter().any(|component| {
        matches!(
            component.as_str(),
            ".ssh" | ".gnupg" | ".aws" | ".gcloud" | "secrets" | ".secrets" | "credentials"
        )
    }) {
        return true;
    }

    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let env_template = [".example", ".sample", ".template", ".dist"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
        && (name.starts_with(".env.") || name.starts_with(".envrc."));
    if (name.starts_with(".env") || name.starts_with(".envrc")) && !env_template {
        return true;
    }
    let private_key_name = ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"]
        .iter()
        .any(|key| {
            name.strip_suffix(key).is_some_and(|prefix| {
                prefix
                    .chars()
                    .next_back()
                    .is_none_or(|character| !character.is_ascii_alphanumeric())
            })
        });
    if [
        ".netrc",
        ".pgpass",
        ".htpasswd",
        ".npmrc",
        ".pypirc",
        ".git-credentials",
        ".boto",
        "secring",
        "secring.gpg",
        "secring.pgp",
    ]
    .contains(&name.as_str())
        || private_key_name
        || [
            ".pem", ".key", ".p12", ".pfx", ".cert", ".crt", ".der", ".p8",
        ]
        .iter()
        .any(|suffix| name.ends_with(suffix))
    {
        return true;
    }

    if path.extension().is_some() {
        return crate::detect::is_sensitive(path);
    }
    let stem = name.trim_start_matches('.');
    if ["service_account", "service-account", "service.account"]
        .iter()
        .any(|marker| stem.contains(marker))
    {
        return true;
    }
    stem.split(['-', '_', '.', ' ', '\t']).any(|part| {
        matches!(
            part,
            "credential"
                | "credentials"
                | "secret"
                | "secrets"
                | "passwd"
                | "passwds"
                | "password"
                | "passwords"
                | "token"
                | "tokens"
                | "serviceaccount"
        )
    })
}

fn extract_container_member(
    container_path: &Path,
    source_file: &str,
    member_path: &str,
    bytes: &[u8],
    recursion_depth: u16,
    dispatch_budget: &mut RecursiveDispatchBudget,
) -> Extraction {
    let path = virtual_member_path(container_path, member_path);
    let member_source_file = virtual_member_source_file(source_file, member_path);
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if let Some(extraction) = extract_registered_format_at_depth(
        &path,
        &member_source_file,
        bytes,
        &extension,
        recursion_depth,
        dispatch_budget,
    ) {
        return extraction;
    }
    let result = dispatch_budget
        .parser_allowance_at_depth(recursion_depth)
        .map_or_else(
            || crate::engine::extract_as_bytes(&path, &member_source_file, bytes),
            |allowance| {
                crate::engine::extract_as_bytes_with_parser_allowance(
                    &path,
                    &member_source_file,
                    bytes,
                    allowance,
                )
            },
        );
    result.unwrap_or_else(|_| {
        rejected_inventory_extraction(&path, &member_source_file, "member_parse_failed")
    })
}

fn dispatch_container_member(
    container_path: &Path,
    source_file: &str,
    member: &crate::containers::ContainerMember,
    bytes: &[u8],
    recursion_depth: u16,
    dispatch_budget: &mut RecursiveDispatchBudget,
    member_dispatch_statuses: &mut BTreeMap<String, &'static str>,
) -> Result<Option<ExtractedContainerMember>, &'static str> {
    if is_sensitive_container_member_path(&member.path) {
        member_dispatch_statuses.insert(member.path.clone(), "sensitive_path_skipped");
        return Ok(None);
    }
    // One child root and its parent-member edge are the smallest useful
    // attachment. Stop before parsing when even that result cannot be retained.
    if !dispatch_budget.can_reserve_output_facts(2) {
        return Err("aggregate_fact_limit");
    }
    if !dispatch_budget.admit_dispatch(member) {
        return Err("aggregate_member_or_byte_limit");
    }

    let before = dispatch_budget.remaining_output_facts();
    let mut extraction = extract_container_member(
        container_path,
        source_file,
        &member.path,
        bytes,
        recursion_depth,
        dispatch_budget,
    );
    mark_container_source(&mut extraction, source_file);
    let already_accounted = before
        .checked_sub(dispatch_budget.remaining_output_facts())
        .expect("recursive child extraction cannot increase its fact budget");
    let Some(required) = child_output_fact_count(&extraction) else {
        dispatch_budget.release_output_facts(already_accounted);
        return Err("aggregate_fact_limit");
    };
    let additional = required.saturating_sub(already_accounted);
    if !dispatch_budget.reserve_output_facts(additional) {
        dispatch_budget.release_output_facts(already_accounted);
        return Err("aggregate_fact_limit");
    }
    let accounted_output_facts = before
        .checked_sub(dispatch_budget.remaining_output_facts())
        .expect("recursive child extraction cannot increase its fact budget");
    Ok(Some(ExtractedContainerMember {
        path: member.path.clone(),
        extraction,
        accounted_output_facts,
    }))
}

fn container_inventory_extraction(
    path: &Path,
    source_file: &str,
    source: &[u8],
    recursion_depth: u16,
    dispatch_budget: &mut RecursiveDispatchBudget,
) -> Option<Extraction> {
    use crate::containers::{
        recursive_archive_kind, visit_tar_members, visit_zip_members, ArchiveKind, ByteInventory,
        InspectionStatus, SvgReferenceRelation,
    };

    let source_name = path.file_name()?.to_string_lossy();
    let limits = dispatch_budget.container_limits;
    if dispatch_budget
        .parser_allowance_bytes
        .is_some_and(|allowance| allowance < 128 * 1024)
        && source.starts_with(b"\x1f\x8b")
    {
        return Some(rejected_inventory_extraction(
            path,
            source_file,
            "parser_arena_budget",
        ));
    }
    let mut extracted_members = Vec::new();
    let mut member_dispatch_statuses = BTreeMap::new();
    let mut dispatch_stop_reason = None;
    let recursive_kind = recursive_archive_kind(&source_name, source);
    let recursive_output_accounting = recursive_kind.is_some();
    if recursive_output_accounting && !dispatch_budget.reserve_output_facts(1) {
        // A parent dispatcher reserves room for a child root before calling
        // this function, and a root invocation starts with a non-zero budget.
        // Keep the fallback bounded if either invariant is broken.
        return Some(rejected_inventory_extraction(
            path,
            source_file,
            "aggregate_fact_limit",
        ));
    }
    let inventory = match recursive_kind {
        Some(ArchiveKind::Tar) if recursion_depth < limits.max_recursion_depth => {
            ByteInventory::Container(visit_tar_members(
                source,
                recursion_depth,
                limits,
                |child| {
                    match dispatch_container_member(
                        path,
                        source_file,
                        child.member,
                        child.bytes,
                        recursion_depth + 1,
                        dispatch_budget,
                        &mut member_dispatch_statuses,
                    ) {
                        Ok(Some(extracted)) => extracted_members.push(extracted),
                        Ok(None) => {}
                        Err(reason) => {
                            dispatch_stop_reason.get_or_insert(reason);
                            return false;
                        }
                    }
                    true
                },
            ))
        }
        Some(ArchiveKind::Zip) if dispatch_budget.parser_allowance_bytes.is_some() => {
            // ZIP payloads require an owned decompression buffer before a child
            // parser can run. Isolated extraction keeps ZIP metadata only, so
            // no member payload allocation can compete with the parser arena.
            dispatch_stop_reason = Some("compressed_member_dispatch_disabled");
            ByteInventory::Container(crate::containers::inspect_zip_inventory_bytes(
                source,
                recursion_depth,
                limits,
            ))
        }
        Some(ArchiveKind::Zip) if recursion_depth < limits.max_recursion_depth => {
            ByteInventory::Container(visit_zip_members(
                source,
                recursion_depth,
                limits,
                |child| {
                    match dispatch_container_member(
                        path,
                        source_file,
                        child.member,
                        child.bytes,
                        recursion_depth + 1,
                        dispatch_budget,
                        &mut member_dispatch_statuses,
                    ) {
                        Ok(Some(extracted)) => extracted_members.push(extracted),
                        Ok(None) => {}
                        Err(reason) => {
                            dispatch_stop_reason.get_or_insert(reason);
                            return false;
                        }
                    }
                    true
                },
            ))
        }
        _ => crate::containers::inspect_bytes(&source_name, source, recursion_depth, limits),
    };
    let stem = source_stem(source_file);
    let file_id = make_id(&[&stem]);
    match inventory {
        ByteInventory::Unrecognized => {
            if recursive_output_accounting {
                dispatch_budget.release_output_facts(1);
            }
            None
        }
        ByteInventory::Container(mut container) => {
            let mut members = container.members;
            members.sort_by(|left, right| left.path.cmp(&right.path));
            for member in &members {
                if is_sensitive_container_member_path(&member.path) {
                    member_dispatch_statuses
                        .entry(member.path.clone())
                        .or_insert("sensitive_path_skipped");
                }
            }
            let requested_member_count = members.len();
            let mut semantic_output_allowed = container.status == InspectionStatus::Parsed;
            if !semantic_output_allowed {
                discard_extracted_members(&mut extracted_members, dispatch_budget);
            }

            if recursive_output_accounting {
                let member_inventory_facts = members.len().checked_mul(2);
                if member_inventory_facts
                    .is_none_or(|facts| !dispatch_budget.can_reserve_output_facts(facts))
                {
                    // Inventory has priority over semantic children. If both do
                    // not fit, release every retained child result before
                    // deterministically admitting the sorted member prefix.
                    discard_extracted_members(&mut extracted_members, dispatch_budget);
                    semantic_output_allowed = false;
                    container.status = InspectionStatus::InventoryOnly;
                    dispatch_stop_reason.get_or_insert("aggregate_fact_limit");
                }
                let mut admitted_members = 0usize;
                while admitted_members < members.len() && dispatch_budget.reserve_output_facts(2) {
                    admitted_members += 1;
                }
                if admitted_members < members.len() {
                    discard_extracted_members(&mut extracted_members, dispatch_budget);
                    semantic_output_allowed = false;
                    container.status = InspectionStatus::InventoryOnly;
                    dispatch_stop_reason.get_or_insert("aggregate_fact_limit");
                    members.truncate(admitted_members);
                }
            }

            let mut root = inventory_file_node(path, source_file, &file_id, "container");
            root.extra.insert(
                "format".into(),
                format!("{:?}", container.kind).to_ascii_lowercase().into(),
            );
            root.extra.insert(
                "inspection_status".into(),
                format!("{:?}", container.status)
                    .to_ascii_lowercase()
                    .into(),
            );
            root.extra.insert(
                "decompressed_bytes".into(),
                container.decompressed_bytes.into(),
            );
            root.extra.insert(
                "diagnostics".into(),
                diagnostics_value(&container.diagnostics),
            );
            if let Some(spec) = crate::format_registry::format_registry().find_by_path(path) {
                let capability = if container.status == InspectionStatus::Parsed {
                    spec.capability.as_str()
                } else {
                    "inventory_only"
                };
                root.extra
                    .insert("format_capability".into(), capability.into());
                root.extra.insert(
                    "parse_status".into(),
                    if capability == "structural_partial" {
                        "partial"
                    } else if capability == "inventory_only" {
                        "inventory_only"
                    } else {
                        "parsed"
                    }
                    .into(),
                );
            }
            if let Some(reason) = dispatch_stop_reason {
                root.extra
                    .insert("recursive_dispatch_status".into(), reason.into());
            } else if recursive_kind.is_some() && recursion_depth >= limits.max_recursion_depth {
                root.extra
                    .insert("recursive_dispatch_status".into(), "recursion_limit".into());
            }
            if !member_dispatch_statuses.is_empty() {
                root.extra.insert(
                    "sensitive_member_count".into(),
                    member_dispatch_statuses.len().into(),
                );
            }
            if members.len() < requested_member_count {
                root.extra.insert(
                    "omitted_member_count".into(),
                    (requested_member_count - members.len()).into(),
                );
            }
            let mut nodes = vec![root];
            let mut edges = Vec::new();
            let mut member_ids = BTreeMap::new();
            for member in members {
                let member_id = make_id(&[&stem, "member", &member.path]);
                member_ids.insert(member.path.clone(), member_id.clone());
                let mut extra = BTreeMap::from([
                    ("type".into(), "container_member".into()),
                    (
                        "member_kind".into(),
                        format!("{:?}", member.kind).to_ascii_lowercase().into(),
                    ),
                    ("compressed_bytes".into(), member.compressed_bytes.into()),
                    (
                        "declared_uncompressed_bytes".into(),
                        member.declared_uncompressed_bytes.into(),
                    ),
                ]);
                if let Some(status) = member_dispatch_statuses.get(&member.path) {
                    extra.insert("dispatch_status".into(), (*status).into());
                }
                nodes.push(Node {
                    id: member_id.clone(),
                    label: member.path,
                    file_type: "document".into(),
                    source_file: source_file.into(),
                    source_location: None,
                    community: None,
                    extra,
                });
                edges.push(contains_edge(&file_id, &member_id, source_file));
            }
            // Partial dispatch is intentionally discarded: inventory is still
            // useful, but attaching semantic facts after an admission stop
            // would make a bounded incomplete archive look complete.
            if semantic_output_allowed {
                for extracted in extracted_members {
                    let Some(member_id) = member_ids.get(&extracted.path) else {
                        continue;
                    };
                    let child_ids = extracted
                        .extraction
                        .nodes
                        .iter()
                        .map(|node| node.id.clone())
                        .collect::<Vec<_>>();
                    nodes.extend(extracted.extraction.nodes);
                    edges.extend(extracted.extraction.edges);
                    for child_id in child_ids {
                        edges.push(contains_edge(member_id, &child_id, source_file));
                    }
                }
            }
            Some(Extraction {
                nodes,
                edges,
                hyperedges: Vec::new(),
            })
        }
        ByteInventory::Media(media) => {
            let mut root = inventory_file_node(path, source_file, &file_id, "media");
            root.file_type = "image".into();
            root.extra.insert(
                "format".into(),
                format!("{:?}", media.kind).to_ascii_lowercase().into(),
            );
            root.extra.insert(
                "inspection_status".into(),
                format!("{:?}", media.status).to_ascii_lowercase().into(),
            );
            root.extra
                .insert("diagnostics".into(), diagnostics_value(&media.diagnostics));
            if let Some(spec) = crate::format_registry::format_registry().find_by_path(path) {
                let capability = if media.status == InspectionStatus::Parsed {
                    spec.capability.as_str()
                } else {
                    "inventory_only"
                };
                root.extra
                    .insert("format_capability".into(), capability.into());
                root.extra.insert(
                    "parse_status".into(),
                    if capability == "structural_partial" {
                        "partial"
                    } else if capability == "inventory_only" {
                        "inventory_only"
                    } else {
                        "parsed"
                    }
                    .into(),
                );
            }
            if let Some(metadata) = media.metadata {
                root.extra.insert("width".into(), metadata.width.into());
                root.extra.insert("height".into(), metadata.height.into());
                root.extra
                    .insert("animated".into(), metadata.animated.into());
            }
            let mut nodes = vec![root];
            let mut edges = Vec::new();
            if let Some(svg) = media.svg {
                if let Some(root) = nodes.first_mut() {
                    root.extra.insert("title".into(), svg.title.into());
                }
                let mut element_ids = BTreeMap::new();
                for element in svg.elements {
                    let label = element.label.or(element.id).unwrap_or(element.name);
                    let element_id = make_id(&[&stem, "svg", &element.ordinal.to_string(), &label]);
                    element_ids.insert(element.ordinal, element_id.clone());
                    nodes.push(Node {
                        id: element_id.clone(),
                        label,
                        file_type: "image".into(),
                        source_file: source_file.into(),
                        source_location: None,
                        community: None,
                        extra: BTreeMap::from([
                            ("type".into(), "svg_element".into()),
                            ("ordinal".into(), element.ordinal.into()),
                        ]),
                    });
                    edges.push(contains_edge(&file_id, &element_id, source_file));
                }
                for reference in svg.references {
                    let Some(source_id) = element_ids.get(&reference.source_ordinal) else {
                        continue;
                    };
                    edges.push(Edge {
                        source: source_id.clone(),
                        target: make_id(&["svg_reference", &reference.target]),
                        relation: match reference.relation {
                            SvgReferenceRelation::Fragment => "references_fragment",
                            SvgReferenceRelation::External => "references_external",
                        }
                        .into(),
                        confidence: Confidence::Extracted,
                        source_file: source_file.into(),
                        extra: BTreeMap::new(),
                    });
                }
            }
            Some(Extraction {
                nodes,
                edges,
                hyperedges: Vec::new(),
            })
        }
    }
}

fn source_stem(source_file: &str) -> String {
    Path::new(source_file)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/")
}

fn inventory_file_node(path: &Path, source_file: &str, file_id: &str, kind: &'static str) -> Node {
    Node {
        id: file_id.into(),
        label: path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(source_file)
            .into(),
        file_type: "document".into(),
        source_file: source_file.into(),
        source_location: None,
        community: None,
        extra: BTreeMap::from([("type".into(), kind.into())]),
    }
}

fn contains_edge(source: &str, target: &str, source_file: &str) -> Edge {
    Edge {
        source: source.into(),
        target: target.into(),
        relation: "contains".into(),
        confidence: Confidence::Extracted,
        source_file: source_file.into(),
        extra: BTreeMap::new(),
    }
}

fn diagnostics_value<T: std::fmt::Debug>(diagnostics: &[T]) -> serde_json::Value {
    serde_json::Value::Array(
        diagnostics
            .iter()
            .map(|item| format!("{item:?}").to_ascii_lowercase().into())
            .collect(),
    )
}

fn rejected_inventory_extraction(
    path: &Path,
    source_file: &str,
    diagnostic: &'static str,
) -> Extraction {
    let file_id = make_id(&[&source_stem(source_file)]);
    let mut node = inventory_file_node(path, source_file, &file_id, "format_inventory");
    node.extra.insert("parse_status".into(), "rejected".into());
    node.extra.insert("diagnostic".into(), diagnostic.into());
    node.extra
        .insert("format_capability".into(), "inventory_only".into());
    Extraction {
        nodes: vec![node],
        edges: Vec::new(),
        hyperedges: Vec::new(),
    }
}

fn registered_inventory_extraction(
    path: &Path,
    source_file: &str,
    byte_length: usize,
    spec: &crate::format_registry::FormatSpec,
) -> Extraction {
    let file_id = make_id(&[&source_stem(source_file)]);
    let mut node = inventory_file_node(path, source_file, &file_id, "format_inventory");
    node.extra.insert("format".into(), spec.id.as_str().into());
    node.extra
        .insert("format_capability".into(), spec.capability.as_str().into());
    node.extra.insert(
        "schema_requirement".into(),
        spec.schema_requirement.as_str().into(),
    );
    node.extra.insert("byte_length".into(), byte_length.into());
    node.extra
        .insert("parse_status".into(), "inventory_only".into());
    Extraction {
        nodes: vec![node],
        edges: Vec::new(),
        hyperedges: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write;
    use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

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

    fn zip_directory_bytes(directories: &[&str]) -> Vec<u8> {
        let cursor = std::io::Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default();
        for directory in directories {
            writer
                .add_directory(*directory, options)
                .expect("add ZIP directory");
        }
        writer.finish().expect("finish ZIP").into_inner()
    }

    fn tar_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        const BLOCK_BYTES: usize = 512;
        let mut archive = Vec::new();
        for (name, value) in entries {
            assert!(name.len() <= 100, "fixture name fits direct TAR header");
            let mut header = [0_u8; BLOCK_BYTES];
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
            header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
            archive.extend_from_slice(&header);
            archive.extend_from_slice(value);
            archive.resize(archive.len().div_ceil(BLOCK_BYTES) * BLOCK_BYTES, 0);
        }
        archive.resize(archive.len() + (2 * BLOCK_BYTES), 0);
        archive
    }

    fn extract(path: &str, bytes: &[u8]) -> Extraction {
        let extension = Path::new(path)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        extract_registered_format(Path::new(path), path, bytes, extension)
            .unwrap_or_else(|| panic!("registered adapter did not claim {path}"))
    }

    fn extract_with_allowance(path: &str, bytes: &[u8], allowance_bytes: usize) -> Extraction {
        let extension = Path::new(path)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        extract_registered_format_with_allowance(
            Path::new(path),
            path,
            bytes,
            extension,
            Some(allowance_bytes),
        )
        .unwrap_or_else(|| panic!("registered adapter did not claim {path}"))
    }

    fn gzip_bytes(value: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(value).expect("write gzip fixture");
        encoder.finish().expect("finish gzip fixture")
    }

    fn has_relation(extraction: &Extraction, relation: &str) -> bool {
        extraction
            .edges
            .iter()
            .any(|edge| edge.relation == relation)
    }

    #[test]
    fn dispatches_new_structured_families_without_path_io() {
        let csv = extract("data/services.csv", b"service,owner\napi,platform\n");
        assert!(csv.nodes.iter().any(|node| {
            node.extra
                .get("structured_value")
                .and_then(serde_json::Value::as_str)
                == Some("api")
        }));

        let ccsv = extract("data/services.ccsv", b"service,owner\napi,platform\n");
        assert!(ccsv.nodes.iter().any(|node| {
            node.extra
                .get("structured_value")
                .and_then(serde_json::Value::as_str)
                == Some("api")
        }));

        let proto = extract(
            "schema/service.proto",
            b"syntax = \"proto3\";\nmessage Service {\n  string name = 1;\n}\n",
        );
        assert!(proto.nodes.iter().any(|node| node.label == "Service"));

        let diagram = extract("architecture.dot", b"digraph system { api -> database; }");
        assert!(has_relation(&diagram, "flows_to") || !diagram.edges.is_empty());

        let simulation = extract(
            "robot.usda",
            b"#usda 1.0\ndef Xform \"Robot\" { rel references = </Library/Robot> }",
        );
        assert!(simulation.nodes.iter().any(|node| node.label == "Robot"));
    }

    #[test]
    fn schema_required_binary_payloads_stay_inventory_only() {
        let extraction = extract("wire.pb", &[0x08, 0x96, 0x01]);
        let root = extraction.nodes.first().expect("inventory root");
        assert_eq!(
            root.extra
                .get("diagnostic")
                .and_then(serde_json::Value::as_str),
            Some("schema_required_binary_payload_not_decoded")
        );
        assert!(extraction.edges.is_empty());
    }

    #[test]
    fn columnar_magic_routes_through_the_byte_only_protocol_adapter() {
        let extraction = extract_registered_format(
            Path::new("extensionless-payload"),
            "extensionless-payload",
            b"PAR1truncated",
            "",
        )
        .expect("Parquet magic must select the protocol byte adapter");
        let root = extraction.nodes.first().expect("inventory root");
        assert_eq!(
            root.extra
                .get("protocol_format")
                .and_then(serde_json::Value::as_str),
            Some("parquet")
        );
        assert_eq!(
            root.extra
                .get("format_capability")
                .and_then(serde_json::Value::as_str),
            Some("inventory_only")
        );
    }

    #[test]
    fn archive_routing_reports_actual_inventory_capability() {
        let bzip = extract("fixtures/asset.bz2", b"BZh9");
        let bzip_root = bzip.nodes.first().expect("container root");
        assert_eq!(
            bzip_root
                .extra
                .get("type")
                .and_then(serde_json::Value::as_str),
            Some("container")
        );
        assert_eq!(
            bzip_root
                .extra
                .get("inspection_status")
                .and_then(serde_json::Value::as_str),
            Some("inventoryonly")
        );
        assert_eq!(
            bzip_root
                .extra
                .get("diagnostics")
                .and_then(serde_json::Value::as_array)
                .and_then(|diagnostics| diagnostics.iter().find_map(serde_json::Value::as_str)),
            Some("declaredsizeunavailable")
        );

        let seven_zip = extract("fixtures/asset.7z", b"7z\xbc\xaf\x27\x1c");
        let seven_zip_root = seven_zip.nodes.first().expect("container root");
        assert_eq!(
            seven_zip_root
                .extra
                .get("inspection_status")
                .and_then(serde_json::Value::as_str),
            Some("inventoryonly")
        );
        assert_eq!(
            seven_zip_root
                .extra
                .get("format")
                .and_then(serde_json::Value::as_str),
            Some("sevenzip")
        );
    }

    #[test]
    fn container_source_provenance_marks_and_round_trips_every_fact_kind() {
        let mut extraction = Extraction {
            nodes: vec![inventory_file_node(
                Path::new("archive.tar!/nested/config.toml"),
                "archive.tar!/nested/config.toml",
                "config",
                "structured_file",
            )],
            edges: vec![contains_edge(
                "member",
                "config",
                "archive.tar!/nested/config.toml",
            )],
            hyperedges: vec![serde_json::json!({
                "id": "config_group",
                "nodes": ["member", "config"],
                "source_file": "archive.tar!/nested/config.toml",
                "_container_source": "nested-owner.tar"
            })],
        };
        mark_container_source(&mut extraction, "archive.tar");

        let round_trip: Extraction =
            serde_json::from_value(serde_json::to_value(extraction).expect("serialize extraction"))
                .expect("deserialize extraction");
        assert!(round_trip.nodes.iter().all(|node| {
            node.extra
                .get(CONTAINER_SOURCE_ATTRIBUTE)
                .and_then(serde_json::Value::as_str)
                == Some("archive.tar")
        }));
        assert!(round_trip.edges.iter().all(|edge| {
            edge.extra
                .get(CONTAINER_SOURCE_ATTRIBUTE)
                .and_then(serde_json::Value::as_str)
                == Some("archive.tar")
        }));
        assert!(round_trip.hyperedges.iter().all(|hyperedge| {
            hyperedge
                .get(CONTAINER_SOURCE_ATTRIBUTE)
                .and_then(serde_json::Value::as_str)
                == Some("archive.tar")
        }));
    }

    #[test]
    fn recursive_archives_dispatch_nested_semantics_without_path_io() {
        let nested = zip_bytes(&[(
            "design/architecture.dot",
            b"digraph platform { gateway -> database; }",
        )]);
        let archive = zip_bytes(&[("nested/architecture.zip", &nested)]);
        let extraction = extract("/definitely/not/present/architecture.zip", &archive);

        assert!(extraction.nodes.iter().any(|node| node.label == "gateway"));
        assert!(extraction.nodes.iter().any(|node| node.label == "database"));
        assert!(has_relation(&extraction, "flows_to"));
        assert!(extraction.edges.iter().any(|edge| {
            edge.relation == "contains"
                && edge.source_file == "/definitely/not/present/architecture.zip"
        }));
        assert!(extraction.nodes.iter().any(|node| {
            node.source_file
                == "/definitely/not/present/architecture.zip!/nested/architecture.zip!/design/architecture.dot"
        }));
    }

    #[test]
    fn isolated_zip_is_metadata_only_and_never_dispatches_member_payloads() {
        let archive = zip_bytes(&[(
            "design/architecture.dot",
            b"digraph platform { gateway -> database; }",
        )]);
        let extraction = extract_with_allowance("architecture.zip", &archive, 2 * 1024 * 1024);
        let root = extraction.nodes.first().expect("ZIP inventory root");
        assert_eq!(
            root.extra
                .get("inspection_status")
                .and_then(serde_json::Value::as_str),
            Some("inventoryonly")
        );
        assert_eq!(
            root.extra
                .get("decompressed_bytes")
                .and_then(serde_json::Value::as_u64),
            Some(0)
        );
        assert_eq!(
            root.extra
                .get("recursive_dispatch_status")
                .and_then(serde_json::Value::as_str),
            Some("compressed_member_dispatch_disabled")
        );
        assert!(extraction
            .nodes
            .iter()
            .any(|node| node.label == "design/architecture.dot"));
        assert!(!extraction
            .nodes
            .iter()
            .any(|node| matches!(node.label.as_str(), "gateway" | "database")));
        assert!(!has_relation(&extraction, "flows_to"));
    }

    #[test]
    fn isolated_compressed_inspection_derives_decode_limits_from_the_arena() {
        let mut svg = String::from("<svg xmlns=\"http://www.w3.org/2000/svg\">");
        for index in 0..5_000 {
            svg.push_str(&format!("<path id=\"path-{index:04}\"/>"));
        }
        svg.push_str("</svg>");
        let compressed = gzip_bytes(svg.as_bytes());

        for path in ["oversized.svgz", "oversized.gz"] {
            let extraction = extract_with_allowance(path, &compressed, 256 * 1024);
            let root = extraction.nodes.first().expect("compressed inventory root");
            let diagnostics = root
                .extra
                .get("diagnostics")
                .and_then(serde_json::Value::as_array)
                .expect("stable compressed diagnostics");
            assert!(diagnostics.iter().any(|diagnostic| {
                matches!(
                    diagnostic.as_str(),
                    Some("totalsizelimit" | "inputtoolarge" | "compressionratiolimit")
                )
            }));
            if path.ends_with(".gz") {
                assert_eq!(
                    root.extra
                        .get("decompressed_bytes")
                        .and_then(serde_json::Value::as_u64),
                    Some(0)
                );
            }
        }

        let below_stream_scratch = extract_with_allowance("tiny-arena.gz", &compressed, 64 * 1024);
        let root = below_stream_scratch
            .nodes
            .first()
            .expect("tiny-arena inventory root");
        assert_eq!(
            root.extra
                .get("diagnostic")
                .and_then(serde_json::Value::as_str),
            Some("parser_arena_budget")
        );
    }

    #[test]
    fn isolated_pdf_is_inventory_only_before_unbounded_stream_decoding() {
        let extraction = extract_with_allowance("document.pdf", b"%PDF-1.7\ninvalid", 1024 * 1024);
        let root = extraction.nodes.first().expect("PDF inventory root");
        assert_eq!(
            root.extra
                .get("diagnostic")
                .and_then(serde_json::Value::as_str),
            Some("parser_arena_unenforceable")
        );
        assert_eq!(
            root.extra
                .get("format_capability")
                .and_then(serde_json::Value::as_str),
            Some("inventory_only")
        );
    }

    #[test]
    fn isolated_semantic_builders_consume_fact_credits_before_retention() {
        let fixtures = [
            (
                "bounded.dot",
                "digraph { a -> b; b -> c; c -> d; d -> e; e -> f; }",
            ),
            (
                "bounded.proto",
                "message Item {\n string a = 1;\n string b = 2;\n string c = 3;\n string d = 4;\n}",
            ),
            (
                "bounded.usda",
                "#usda 1.0\ndef Xform \"A\" {}\ndef Xform \"B\" {}\ndef Xform \"C\" {}\n",
            ),
        ];
        for (path, source) in fixtures {
            let allowance = (16 * 1024) + (source.len() * 16) + (4 * 2 * 1024);
            let extraction = extract_with_allowance(path, source.as_bytes(), allowance);
            assert!(
                extraction_fact_count(&extraction).is_some_and(|facts| facts <= 4),
                "{path} retained more facts than its pre-allocation credits: {extraction:?}"
            );
            let root = extraction.nodes.first().expect("bounded semantic root");
            assert_eq!(
                root.extra
                    .get("parser_diagnostic")
                    .and_then(serde_json::Value::as_str),
                Some("parser_arena_fact_limit"),
                "{path} did not expose deterministic arena truncation"
            );
        }
    }

    #[test]
    fn recursive_archive_depth_limit_keeps_only_bounded_inventory() {
        let mut archive = zip_bytes(&[("leaf.dot", b"digraph depth { api -> db; }")]);
        for _ in 0..=crate::containers::ContainerLimits::default().max_recursion_depth {
            archive = zip_bytes(&[("nested.zip", &archive)]);
        }
        let extraction = extract("deep.zip", &archive);
        assert!(!has_relation(&extraction, "flows_to"));
        assert!(extraction.nodes.iter().any(|node| {
            node.extra
                .get("recursive_dispatch_status")
                .and_then(serde_json::Value::as_str)
                == Some("recursion_limit")
        }));
    }

    #[test]
    fn recursive_archive_member_budget_discards_partial_semantics() {
        let archive = zip_bytes(&[
            ("a.dot", b"digraph a { api -> db; }"),
            ("b.dot", b"digraph b { worker -> queue; }"),
        ]);
        let limits = crate::containers::ContainerLimits {
            max_members: 1,
            ..crate::containers::ContainerLimits::default()
        };
        let mut budget = RecursiveDispatchBudget::new(limits);
        let extraction = container_inventory_extraction(
            Path::new("bounded.zip"),
            "bounded.zip",
            &archive,
            0,
            &mut budget,
        )
        .expect("ZIP is a registered container");
        assert!(!has_relation(&extraction, "flows_to"));
        let root = extraction.nodes.first().expect("container root");
        assert_eq!(
            root.extra
                .get("inspection_status")
                .and_then(serde_json::Value::as_str),
            Some("inventoryonly")
        );
        assert_eq!(
            root.extra
                .get("diagnostics")
                .and_then(serde_json::Value::as_array)
                .and_then(|items| items.first())
                .and_then(serde_json::Value::as_str),
            Some("nesteddispatchstopped")
        );
    }

    #[test]
    fn recursive_archives_screen_sensitive_member_paths_without_persisting_values() {
        const ZIP_SECRET: &str = "zip-sensitive-sentinel";
        const TAR_SECRET: &str = "tar-sensitive-sentinel";
        let tar = tar_bytes(&[
            (".env", format!("TOKEN={TAR_SECRET}\n").as_bytes()),
            ("safe.dot", b"digraph safe { worker -> queue; }"),
        ]);
        let zip = zip_bytes(&[
            (".env", format!("TOKEN={ZIP_SECRET}\n").as_bytes()),
            (
                "credentials.json",
                format!(r#"{{"password":"{ZIP_SECRET}"}}"#).as_bytes(),
            ),
            ("nested/archive.tar", &tar),
            ("safe.dot", b"digraph safe { gateway -> database; }"),
        ]);

        let zip_extraction = extract("sensitive.zip", &zip);
        let rendered = serde_json::to_string(&zip_extraction).expect("serialize ZIP extraction");
        assert!(!rendered.contains(ZIP_SECRET));
        assert!(!rendered.contains(TAR_SECRET));
        assert!(zip_extraction
            .edges
            .iter()
            .any(|edge| edge.relation == "flows_to"));
        let skipped = zip_extraction
            .nodes
            .iter()
            .filter(|node| {
                node.extra
                    .get("dispatch_status")
                    .and_then(serde_json::Value::as_str)
                    == Some("sensitive_path_skipped")
            })
            .map(|node| node.label.as_str())
            .collect::<Vec<_>>();
        assert!(skipped.contains(&".env"));
        assert!(skipped.contains(&"credentials.json"));
        assert!(zip_extraction.nodes.iter().any(|node| {
            node.label == ".env"
                && node.source_file == "sensitive.zip!/nested/archive.tar"
                && node
                    .extra
                    .get("dispatch_status")
                    .and_then(serde_json::Value::as_str)
                    == Some("sensitive_path_skipped")
        }));

        let tar_extraction = extract("sensitive.tar", &tar);
        let rendered = serde_json::to_string(&tar_extraction).expect("serialize TAR extraction");
        assert!(!rendered.contains(TAR_SECRET));
        let tar_root = tar_extraction.nodes.first().expect("TAR root");
        assert_eq!(
            tar_root
                .extra
                .get("format_capability")
                .and_then(serde_json::Value::as_str),
            Some("structural_partial")
        );
        assert_eq!(
            tar_root
                .extra
                .get("parse_status")
                .and_then(serde_json::Value::as_str),
            Some("partial")
        );
        assert!(tar_extraction.nodes.iter().any(|node| {
            node.label == ".env"
                && node
                    .extra
                    .get("dispatch_status")
                    .and_then(serde_json::Value::as_str)
                    == Some("sensitive_path_skipped")
        }));
        assert!(tar_extraction
            .edges
            .iter()
            .any(|edge| edge.relation == "flows_to"));
    }

    #[test]
    fn aggregate_fact_budget_discards_semantics_and_caps_sorted_member_inventory() {
        let entries = (b'a'..=b'h')
            .map(|letter| {
                (
                    format!("{}.dot", char::from(letter)),
                    format!(
                        "digraph {} {{ source_{} -> target_{}; }}",
                        char::from(letter),
                        char::from(letter),
                        char::from(letter)
                    ),
                )
            })
            .collect::<Vec<_>>();
        let forward = entries
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_bytes()))
            .collect::<Vec<_>>();
        let reverse = entries
            .iter()
            .rev()
            .map(|(name, value)| (name.as_str(), value.as_bytes()))
            .collect::<Vec<_>>();

        let extract_bounded = |bytes: &[u8]| {
            let mut budget = RecursiveDispatchBudget::with_output_fact_limit(
                crate::containers::ContainerLimits::default(),
                15,
            );
            container_inventory_extraction(
                Path::new("bounded.zip"),
                "bounded.zip",
                bytes,
                0,
                &mut budget,
            )
            .expect("bounded ZIP inventory")
        };
        let forward = extract_bounded(&zip_bytes(&forward));
        let reverse = extract_bounded(&zip_bytes(&reverse));
        assert_eq!(
            serde_json::to_vec(&forward).expect("serialize forward inventory"),
            serde_json::to_vec(&reverse).expect("serialize reverse inventory"),
            "central-directory order must not change the capped result"
        );
        assert!(extraction_fact_count(&forward).expect("bounded fact count") <= 15);
        let root = forward.nodes.first().expect("container root");
        assert_eq!(
            root.extra
                .get("recursive_dispatch_status")
                .and_then(serde_json::Value::as_str),
            Some("aggregate_fact_limit")
        );
        assert!(root.extra["omitted_member_count"]
            .as_u64()
            .is_some_and(|count| count > 0));
        assert!(!has_relation(&forward, "flows_to"));
    }

    #[test]
    fn default_archive_budget_uses_the_published_aggregate_fact_ceiling() {
        let directory_names = (0..2_050)
            .map(|index| format!("d{index:04}/"))
            .collect::<Vec<_>>();
        let directory_refs = directory_names
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let extraction = extract("default-budget.zip", &zip_directory_bytes(&directory_refs));

        let fact_count = extraction_fact_count(&extraction).expect("bounded archive fact count");
        assert!(fact_count <= crate::format_registry::CONTAINER_LIMITS.max_records);
        assert_eq!(crate::format_registry::CONTAINER_LIMITS.max_records, 4_096);
        let root = extraction.nodes.first().expect("container root");
        assert_eq!(
            root.extra
                .get("recursive_dispatch_status")
                .and_then(serde_json::Value::as_str),
            Some("aggregate_fact_limit")
        );
        assert!(root.extra["omitted_member_count"]
            .as_u64()
            .is_some_and(|count| count > 0));
    }

    #[test]
    fn nested_directory_inventory_and_parent_links_share_the_fact_budget() {
        let directories = ["a/", "b/", "c/", "d/", "e/", "f/", "g/", "h/"];
        let nested = zip_directory_bytes(&directories);
        let outer = zip_bytes(&[("nested/archive.zip", &nested)]);
        let mut budget = RecursiveDispatchBudget::with_output_fact_limit(
            crate::containers::ContainerLimits::default(),
            18,
        );
        let extraction = container_inventory_extraction(
            Path::new("directories.zip"),
            "directories.zip",
            &outer,
            0,
            &mut budget,
        )
        .expect("bounded nested directory inventory");

        assert!(extraction_fact_count(&extraction).expect("bounded fact count") <= 18);
        let root = extraction.nodes.first().expect("container root");
        assert_eq!(
            root.extra
                .get("recursive_dispatch_status")
                .and_then(serde_json::Value::as_str),
            Some("aggregate_fact_limit")
        );
        assert_eq!(
            root.extra
                .get("inspection_status")
                .and_then(serde_json::Value::as_str),
            Some("inventoryonly")
        );
        assert!(
            !extraction
                .nodes
                .iter()
                .any(|node| directories.contains(&node.label.as_str())),
            "an over-budget nested extraction must be discarded rather than attached partially"
        );
    }

    #[test]
    fn compatibility_markdown_and_json_remain_with_existing_fallbacks() {
        assert!(extract_registered_format(
            Path::new("guide.md"),
            "guide.md",
            b"# Guide\n[Reference](reference.md)",
            "md",
        )
        .is_none());
        assert!(extract_registered_format(
            Path::new(".vscode/tasks.json"),
            ".vscode/tasks.json",
            b"{not JSONC}",
            "json",
        )
        .is_none());
    }

    #[test]
    fn representation_magic_precedes_an_ordinary_engine_owned_suffix() {
        let extraction = extract("src/not_really_rust.rs", b"PK\x03\x04");
        assert_eq!(
            extraction
                .nodes
                .first()
                .and_then(|node| { node.extra.get("type").and_then(serde_json::Value::as_str) }),
            Some("container")
        );
    }

    fn registry_seed(
        spec: &crate::format_registry::FormatSpec,
    ) -> (std::path::PathBuf, &'static [u8]) {
        let path = spec
            .file_names
            .first()
            .map(|name| (*name).into())
            .or_else(|| {
                spec.extensions
                    .first()
                    .map(|extension| format!("fixture.{extension}").into())
            })
            .expect("every registered format has a path discriminator");
        let bytes = match spec.id.as_str() {
            "source-code" => b"fn fixture() {}\n".as_slice(),
            "json" | "package-manifest" | "mcp-configuration" => {
                br#"{"name":"fixture","mcpServers":{}}"#.as_slice()
            }
            "json5" => b"{name: 'fixture'}\n".as_slice(),
            "json-lines" => b"{\"name\":\"fixture\"}\n".as_slice(),
            "terraform-hcl" => b"resource \"fixture\" \"main\" {}\n".as_slice(),
            "api-description" => b"{\"openapi\":\"3.1.0\",\"paths\":{}}\n".as_slice(),
            "markup-documents" => b"Fixture\n=======\n".as_slice(),
            "yaml" | "named-yaml-configuration" => b"name: fixture\n".as_slice(),
            "toml" => b"name = \"fixture\"\n".as_slice(),
            "xml" | "xml-schema-languages" => b"<fixture/>\n".as_slice(),
            "svg" => {
                b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect id=\"fixture\"/></svg>".as_slice()
            }
            "named-json-configuration" => br#"{"name":"fixture"}"#.as_slice(),
            "avro-idl" => b"protocol Fixture {}\n".as_slice(),
            "avro-schema" => br#"{"type":"record","name":"Fixture","fields":[]}"#.as_slice(),
            "markdown" => b"# Fixture\n".as_slice(),
            "plain-text" => b"fixture\n".as_slice(),
            "api-idl" => b"type Query { fixture: String }\n".as_slice(),
            "bpmn-uml-sysml" | "drawio" => b"<definitions id=\"fixture\"/>\n".as_slice(),
            "whiteboard" => br#"{"type":"excalidraw","elements":[]}"#.as_slice(),
            "facility-metadata" => br#"{"Name":"fixture"}"#.as_slice(),
            "simulation-assets" | "simulation-fmi-model-description" | "simulation-scenarios" => {
                b"<fixture/>\n".as_slice()
            }
            _ => match spec.adapter() {
                crate::format_registry::ByteAdapterKind::Engine => b"fixture\n".as_slice(),
                crate::format_registry::ByteAdapterKind::Structured => {
                    b"name: fixture\n".as_slice()
                }
                crate::format_registry::ByteAdapterKind::Protocol => {
                    b"message Fixture { optional string name = 1; }\n".as_slice()
                }
                crate::format_registry::ByteAdapterKind::Diagram => {
                    b"digraph { api -> db; }\n".as_slice()
                }
                crate::format_registry::ByteAdapterKind::Engineering => {
                    b"#1=IFCWALL();\n".as_slice()
                }
                crate::format_registry::ByteAdapterKind::Simulation => {
                    b"#usda 1.0\ndef Xform \"Fixture\" {}\n".as_slice()
                }
                crate::format_registry::ByteAdapterKind::ContainerMedia => b"PK\x03\x04".as_slice(),
                crate::format_registry::ByteAdapterKind::Pdf => b"%PDF-1.7\n".as_slice(),
                crate::format_registry::ByteAdapterKind::Inventory => b"fixture\n".as_slice(),
            },
        };
        (path, bytes)
    }

    fn every_discriminator_path(
        spec: &crate::format_registry::FormatSpec,
    ) -> Vec<std::path::PathBuf> {
        spec.extensions
            .iter()
            .map(|extension| format!("fixture.{extension}").into())
            .chain(spec.file_names.iter().map(|name| (*name).into()))
            .collect()
    }

    fn extension(path: &Path) -> &str {
        path.extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
    }

    fn assert_bounded(spec: &crate::format_registry::FormatSpec, extraction: &Extraction) {
        assert!(
            extraction_fact_count(extraction).is_some_and(|facts| facts <= spec.limits.max_records),
            "{} exceeded its registered aggregate fact limit",
            spec.id.as_str()
        );
    }

    fn assert_capability_owner(spec: &crate::format_registry::FormatSpec) {
        use crate::format_registry::{ByteAdapterKind, FormatCapability};

        match spec.capability {
            FormatCapability::SemanticFull => assert!(
                matches!(
                    spec.adapter(),
                    ByteAdapterKind::Engine
                        | ByteAdapterKind::Structured
                        | ByteAdapterKind::Protocol
                        | ByteAdapterKind::Diagram
                        | ByteAdapterKind::Engineering
                        | ByteAdapterKind::Simulation
                        | ByteAdapterKind::Pdf
                        // SVG is semantically parsed by the media adapter.
                        | ByteAdapterKind::ContainerMedia
                ),
                "{} claims semantic extraction through a non-semantic adapter",
                spec.id.as_str()
            ),
            FormatCapability::SchemaFull => assert_eq!(
                spec.adapter(),
                ByteAdapterKind::Protocol,
                "{} claims schema extraction outside the schema-aware protocol adapter",
                spec.id.as_str()
            ),
            FormatCapability::StructuralPartial => assert!(
                matches!(
                    spec.adapter(),
                    ByteAdapterKind::Engine
                        | ByteAdapterKind::Structured
                        | ByteAdapterKind::Protocol
                        | ByteAdapterKind::Diagram
                        | ByteAdapterKind::Engineering
                        | ByteAdapterKind::Simulation
                        | ByteAdapterKind::ContainerMedia
                ),
                "{} claims structural extraction outside a partial-structure adapter",
                spec.id.as_str()
            ),
            FormatCapability::ContainerFull => assert_eq!(
                spec.adapter(),
                ByteAdapterKind::ContainerMedia,
                "{} claims container extraction outside the container adapter",
                spec.id.as_str()
            ),
            FormatCapability::InventoryOnly => {
                if spec.adapter() == ByteAdapterKind::Pdf {
                    assert_eq!(
                        spec.id.as_str(),
                        "pdf",
                        "only the bounded PDF inventory route may own the PDF adapter"
                    );
                }
            }
        }
    }

    fn assert_rejected_or_inventory(
        extraction: &Extraction,
        spec: &crate::format_registry::FormatSpec,
    ) {
        assert!(
            !extraction.nodes.is_empty(),
            "{} dropped malformed registered input instead of retaining a bounded result",
            spec.id.as_str()
        );
        assert_bounded(spec, extraction);
    }

    fn has_inventory_only_status(extraction: &Extraction) -> bool {
        extraction.nodes.iter().any(|node| {
            ["format_capability", "parse_status"].iter().any(|key| {
                matches!(
                    node.extra.get(*key).and_then(serde_json::Value::as_str),
                    Some("inventory_only" | "rejected")
                )
            }) || matches!(
                node.extra
                    .get("inspection_status")
                    .and_then(serde_json::Value::as_str),
                Some("inventoryonly" | "rejected")
            )
        })
    }

    #[test]
    fn registry_driven_matrix_covers_each_extension_filename_and_default_byte_route() {
        let registry = crate::format_registry::format_registry();
        let mut checked_discriminators = 0usize;
        for spec in registry.specs() {
            assert_capability_owner(spec);
            let (path, bytes) = registry_seed(spec);
            let source_file = path.to_string_lossy();
            let extraction = match spec.adapter() {
                crate::format_registry::ByteAdapterKind::Engine => {
                    crate::engine::extract_as_bytes(&path, &source_file, bytes).unwrap_or_else(
                        |error| panic!("{} engine route failed: {error:#}", spec.id.as_str()),
                    )
                }
                _ => extract_registered_format(
                    &path,
                    &source_file,
                    bytes,
                    path.extension()
                        .and_then(|value| value.to_str())
                        .unwrap_or_default(),
                )
                .unwrap_or_else(|| {
                    panic!(
                        "{} did not route through its byte adapter",
                        spec.id.as_str()
                    )
                }),
            };
            assert_bounded(spec, &extraction);
            assert!(
                !extraction.nodes.is_empty(),
                "{} did not produce a semantic, container, or inventory root",
                spec.id.as_str()
            );
            match spec.capability {
                crate::format_registry::FormatCapability::SemanticFull
                    if spec.id.as_str() != "pdf" =>
                {
                    assert!(
                        !has_inventory_only_status(&extraction),
                        "{} advertised semantic extraction but its valid routing seed is inventory-only",
                        spec.id.as_str()
                    );
                }
                crate::format_registry::FormatCapability::SchemaFull => assert!(
                    extraction.nodes.iter().any(|node| {
                        node.extra
                            .get("format_capability")
                            .and_then(serde_json::Value::as_str)
                            == Some("schema_full")
                    }) || has_inventory_only_status(&extraction),
                    "{} did not emit schema facts or a bounded inventory result",
                    spec.id.as_str()
                ),
                _ => {}
            }

            for discriminator in every_discriminator_path(spec) {
                checked_discriminators += 1;
                let owner = registry.find_by_path(&discriminator).unwrap_or_else(|| {
                    panic!("{} path was not registered", discriminator.display())
                });
                assert_eq!(
                    owner.id,
                    spec.id,
                    "{} path ownership drifted",
                    discriminator.display()
                );
                if let Some(extension) = discriminator.extension().and_then(|value| value.to_str())
                {
                    if let Some(extension_owner) = registry.find_by_extension(extension) {
                        // Named files can intentionally override an extension family.
                        assert!(
                            extension_owner.id == spec.id || !spec.file_names.is_empty(),
                            "{extension} extension does not resolve to {}",
                            spec.id.as_str()
                        );
                    } else {
                        assert!(
                            spec.file_names
                                .iter()
                                .any(|name| *name == discriminator.to_string_lossy()),
                            "{extension} has no extension owner and {} is not a named format",
                            discriminator.display()
                        );
                    }
                }

                let source_file = discriminator.to_string_lossy();
                let routed = extract_registered_format(
                    &discriminator,
                    &source_file,
                    bytes,
                    extension(&discriminator),
                );
                if spec.adapter() == crate::format_registry::ByteAdapterKind::Engine {
                    // `package-manifest` groups both semantic package roots
                    // (such as `package.json`) and lock/config representations
                    // that deliberately retain generic bounded structure when
                    // no manifest/config parser accepts them.  The latter are
                    // the one engine-owned family that can truthfully enter a
                    // byte adapter; otherwise an Engine owner must fall
                    // through unchanged to the compatibility extractor.
                    let allows_bounded_json_fallback = spec.id.as_str() == "package-manifest"
                        && extension(&discriminator) == "json"
                        && !crate::json_config::should_use_json_config(&discriminator, bytes);
                    assert_eq!(
                        routed.is_some(),
                        allows_bounded_json_fallback,
                        "{} did not use its declared engine or bounded JSON fallback route",
                        discriminator.display()
                    );
                    if let Some(routed) = routed {
                        assert_bounded(spec, &routed);
                        assert!(
                            !routed.nodes.is_empty(),
                            "{} produced no bounded JSON fallback root",
                            discriminator.display()
                        );
                    }
                } else {
                    let routed = routed.unwrap_or_else(|| {
                        panic!(
                            "{} did not reach {}",
                            discriminator.display(),
                            spec.adapter().as_str()
                        )
                    });
                    assert_bounded(spec, &routed);
                    assert!(
                        !routed.nodes.is_empty(),
                        "{} produced no bounded extraction or diagnostic root",
                        discriminator.display()
                    );

                    let malformed = extract_registered_format(
                        &discriminator,
                        &source_file,
                        b"\xff\x00 malformed registered input",
                        extension(&discriminator),
                    )
                    .unwrap_or_else(|| {
                        panic!(
                            "{} dropped malformed registered input",
                            discriminator.display()
                        )
                    });
                    assert_rejected_or_inventory(&malformed, spec);
                }
            }
        }
        assert!(
            checked_discriminators > 0,
            "the registry has no testable discriminators"
        );
    }

    #[test]
    fn all_byte_adapters_reject_before_invoking_a_parser_when_the_registered_limit_is_exceeded() {
        let limits = crate::format_registry::FormatLimits {
            max_input_bytes: 1,
            max_nesting: 1,
            max_records: 1,
            max_container_members: 0,
            max_recursion_depth: 0,
            max_expansion_ratio: 1,
        };
        let error = STRUCTURED_EXTRACTOR
            .extract(
                ReadyStructuredInput {
                    path: Path::new("fixture.json"),
                    source_file: "fixture.json",
                    bytes: b"{}",
                },
                &limits,
            )
            .expect_err("the byte adapter must enforce the registered ceiling first");
        assert!(error
            .to_string()
            .contains("exceeds registered 1 byte limit"));

        let families: [(
            &dyn ByteStructuredExtractor,
            &str,
            crate::format_registry::FormatLimits,
        ); 4] = [
            (
                &DIAGRAM_EXTRACTOR,
                "fixture.dot",
                crate::format_registry::DIAGRAM_LIMITS,
            ),
            (
                &ENGINEERING_EXTRACTOR,
                "fixture.ifc",
                crate::format_registry::ENGINEERING_LIMITS,
            ),
            (
                &PROTOCOL_EXTRACTOR,
                "fixture.proto",
                crate::format_registry::PROTOCOL_LIMITS,
            ),
            (
                &SIMULATION_EXTRACTOR,
                "fixture.usda",
                crate::format_registry::SIMULATION_LIMITS,
            ),
        ];
        for (extractor, source_file, limits) in families {
            let bytes = vec![b' '; limits.max_input_bytes as usize + 1];
            let error = extractor
                .extract(
                    ReadyStructuredInput {
                        path: Path::new(source_file),
                        source_file,
                        bytes: &bytes,
                    },
                    &limits,
                )
                .expect_err("the family adapter must enforce its published byte ceiling first");
            assert!(
                error
                    .to_string()
                    .contains(&format!("registered {} byte limit", limits.max_input_bytes)),
                "{source_file}: {error:#}"
            );
        }
    }

    #[test]
    fn byte_adapter_wrapper_enforces_the_published_aggregate_fact_ceiling() {
        fn two_fact_extraction(
            input: ReadyStructuredInput<'_>,
            _: &crate::format_registry::FormatLimits,
        ) -> anyhow::Result<Extraction> {
            let mut extraction =
                rejected_inventory_extraction(input.path, input.source_file, "test_inventory");
            extraction
                .nodes
                .push(extraction.nodes.first().expect("inventory root").clone());
            Ok(extraction)
        }

        let limits = crate::format_registry::FormatLimits {
            max_input_bytes: 1,
            max_nesting: 1,
            max_records: 1,
            max_container_members: 0,
            max_recursion_depth: 0,
            max_expansion_ratio: 1,
        };
        let extractor = FunctionByteStructuredExtractor(two_fact_extraction);
        let error = extractor
            .extract(
                ReadyStructuredInput {
                    path: Path::new("fixture.test"),
                    source_file: "fixture.test",
                    bytes: b"x",
                },
                &limits,
            )
            .expect_err("aggregate fact output above the published ceiling must be rejected");
        assert!(error
            .to_string()
            .contains("2 output facts exceeds registered 1 fact limit"));
    }

    #[test]
    fn every_magic_only_representation_reaches_a_bounded_byte_route() {
        let registry = crate::format_registry::format_registry();
        let mut checked_rules = 0usize;
        for spec in registry.specs() {
            for rule in spec.magic {
                checked_rules += 1;
                let mut bytes = vec![0; rule.offset + rule.bytes.len()];
                bytes[rule.offset..].copy_from_slice(rule.bytes);
                let path = Path::new("magic-only.bin");
                let extraction = extract_registered_format(path, "magic-only.bin", &bytes, "bin")
                    .unwrap_or_else(|| {
                        panic!(
                            "{} magic rule did not reach a byte adapter",
                            spec.id.as_str()
                        )
                    });
                assert_rejected_or_inventory(&extraction, spec);
            }
        }
        assert!(
            checked_rules > 0,
            "the registry has no magic rules to validate"
        );
    }
}
