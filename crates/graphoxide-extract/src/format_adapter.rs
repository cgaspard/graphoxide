//! Central byte-only dispatch for registered structured formats.
//!
//! Language extractors keep precedence in `engine`; this module owns the
//! non-language formats and turns malformed or schema-required bytes into
//! deterministic inventory facts rather than attempting path I/O or aborting
//! a complete project extraction.

use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node, CONTAINER_SOURCE_ATTRIBUTE};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    rc::Rc,
};

use crate::containers::{
    ArchiveKind, CompressedMemberAdmission, ContainerInspection, ContainerLimits,
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
    extract_registered_format_with_allowance_and_cancellation(
        path,
        source_file,
        source,
        extension,
        parser_allowance_bytes,
        None,
    )
}

/// Dispatch a registered format under an optional isolated parser allowance
/// and cooperative cancellation handle.
pub(crate) fn extract_registered_format_with_allowance_and_cancellation(
    path: &Path,
    source_file: &str,
    source: &[u8],
    extension: &str,
    parser_allowance_bytes: Option<usize>,
    cancellation: Option<&graphoxide_index_runtime::RuntimeCancellation>,
) -> Option<Extraction> {
    let limits = parser_allowance_bytes.map_or_else(
        crate::containers::ContainerLimits::default,
        bounded_container_limits,
    );
    let mut dispatch_budget = RecursiveDispatchBudget::new_with_parser_allowance_and_cancellation(
        limits,
        parser_allowance_bytes,
        cancellation.cloned(),
    );
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
    // Known OOXML, ODF, and EPUB suffixes have a dedicated selective package
    // parser. Claim them before the generic ZIP route so package XML is parsed
    // under one format-specific allowance and never recursively dispatched as
    // arbitrary archive payloads.
    if let Some(kind) = crate::office::OfficeKind::from_extension(extension) {
        return Some(extract_office_for_spec(
            OfficeExtractionRequest {
                path,
                source_file,
                source,
                extension,
                kind,
                parser_allowance_bytes: semantic_parser_allowance,
                recursion_depth,
            },
            dispatch_budget,
        ));
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
        ByteAdapterKind::Pdf => Some(extract_pdf_for_spec(
            path,
            source_file,
            source,
            semantic_parser_allowance,
            dispatch_budget.cancellation(),
        )),
        // Extension-owned package formats are consumed before generic ZIP
        // inspection above. Keep a fail-closed branch for future magic-only
        // registry entries instead of allowing compressed bytes to fall
        // through to a text parser.
        ByteAdapterKind::Office => Some(rejected_office_extraction(
            path,
            source_file,
            extension,
            "office_format_unrecognized",
        )),
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

fn extract_pdf_for_spec(
    path: &Path,
    source_file: &str,
    source: &[u8],
    parser_allowance_bytes: Option<usize>,
    cancellation: Option<&graphoxide_index_runtime::RuntimeCancellation>,
) -> Extraction {
    let result = extract_with_parser_plan(parser_allowance_bytes, source.len(), || {
        let limits = match parser_allowance_bytes {
            Some(allowance_bytes) => {
                let Some(limits) =
                    crate::pdf::PdfLimits::for_parser_allowance(allowance_bytes, source.len())
                else {
                    anyhow::ensure!(
                        crate::parser_budget::try_reserve_facts(1),
                        "PDF rejection root exceeded its dynamic fact allowance"
                    );
                    return Ok(rejected_pdf_extraction(
                        path,
                        source_file,
                        "parser_arena_budget",
                    ));
                };
                limits
            }
            None => crate::pdf::PdfLimits::default(),
        };
        let is_cancelled = || {
            cancellation.is_some_and(graphoxide_index_runtime::RuntimeCancellation::is_cancelled)
        };
        match crate::pdf::extract_pdf_bytes(path, source_file, source, limits, Some(&is_cancelled))
        {
            Ok(extraction) if !extraction.nodes.is_empty() => Ok(extraction),
            Ok(_) => anyhow::bail!("PDF parser returned an empty extraction"),
            Err(error) => {
                anyhow::ensure!(
                    crate::parser_budget::try_reserve_facts(1),
                    "PDF rejection root exceeded its dynamic fact allowance"
                );
                Ok(rejected_pdf_extraction(path, source_file, error.code()))
            }
        }
    });
    result.unwrap_or_else(|_| rejected_pdf_extraction(path, source_file, "parser_arena_budget"))
}

struct OfficeExtractionRequest<'a> {
    path: &'a Path,
    source_file: &'a str,
    source: &'a [u8],
    extension: &'a str,
    kind: crate::office::OfficeKind,
    parser_allowance_bytes: Option<usize>,
    recursion_depth: u16,
}

fn extract_office_for_spec(
    request: OfficeExtractionRequest<'_>,
    dispatch_budget: &mut RecursiveDispatchBudget,
) -> Extraction {
    let OfficeExtractionRequest {
        path,
        source_file,
        source,
        extension,
        kind,
        parser_allowance_bytes,
        recursion_depth,
    } = request;
    let mut limits = match parser_allowance_bytes {
        Some(allowance_bytes) => {
            let Some(limits) =
                crate::office::OfficeLimits::for_parser_allowance(allowance_bytes, source.len())
            else {
                return rejected_office_extraction(
                    path,
                    source_file,
                    extension,
                    "parser_arena_budget",
                );
            };
            limits
        }
        None => crate::office::OfficeLimits::default(),
    };
    let tree_fact_limit = if recursion_depth == 0 {
        dispatch_budget.remaining_output_facts()
    } else {
        // A nested child node also receives one parent-member `contains`
        // edge. Reserving at most half the remaining tree facts before parse
        // ensures the post-parse attachment cannot exceed the shared budget.
        dispatch_budget.remaining_output_facts() / 2
    };
    limits.max_facts = limits.max_facts.min(tree_fact_limit);
    limits.max_units = limits.max_units.min(limits.max_facts.saturating_sub(1) / 2);
    if limits.max_facts < 3 || limits.max_units == 0 {
        return rejected_office_extraction(path, source_file, extension, "parser_arena_budget");
    }
    let Some(plan) = crate::parser_budget::ParserPlan::for_fact_limit(limits.max_facts) else {
        return rejected_office_extraction(path, source_file, extension, "parser_arena_budget");
    };
    let shared_budget = RefCell::new(dispatch_budget);
    let is_cancelled = || shared_budget.borrow().is_cancelled();
    let (result, exhausted) = crate::parser_budget::with_plan(plan, || {
        crate::office::extract_office_bytes_with_admission(
            path,
            source_file,
            source,
            kind,
            limits,
            Some(&is_cancelled),
            |_| shared_budget.borrow_mut().admit_encounter(),
            |member| {
                let mut budget = shared_budget.borrow_mut();
                if !budget.admit_dispatch(member) {
                    return None;
                }
                let bytes = usize::try_from(member.declared_uncompressed_bytes).ok()?;
                budget.try_reserve_scratch(bytes)
            },
        )
    });
    if exhausted {
        return rejected_office_extraction(path, source_file, extension, "office_fact_limit");
    }
    match result {
        Ok(extraction)
            if !extraction.nodes.is_empty()
                && extraction_fact_count(&extraction)
                    .is_some_and(|fact_count| fact_count <= limits.max_facts) =>
        {
            extraction
        }
        Ok(_) => rejected_office_extraction(path, source_file, extension, "office_fact_limit"),
        Err(error) => rejected_office_extraction(path, source_file, extension, error.code()),
    }
}

/// Aggregate admission guard for one root archive tree.
///
/// Per-container validation protects decompression and metadata allocations.
/// This additional guard bounds the number and declared bytes of members that
/// may reach semantic adapters, plus every retained node, edge, hyperedge, and
/// member-inventory fact across the complete nested tree. Compressed members
/// additionally hold a tree-scoped scratch permit from before allocation until
/// their child parser returns; TAR payloads remain zero-copy source slices.
#[derive(Debug)]
struct RecursiveDispatchBudget {
    remaining_encountered_members: usize,
    remaining_dispatch_members: usize,
    remaining_declared_bytes: u64,
    remaining_output_facts: usize,
    output_fact_limit: usize,
    container_limits: crate::containers::ContainerLimits,
    parser_allowance_bytes: Option<usize>,
    cancellation: Option<graphoxide_index_runtime::RuntimeCancellation>,
    scratch: RecursiveScratchBudget,
}

#[derive(Debug, Clone)]
struct RecursiveScratchBudget {
    state: Rc<RecursiveScratchState>,
}

#[derive(Debug)]
struct RecursiveScratchState {
    used: Cell<usize>,
    limit: usize,
}

#[derive(Debug)]
struct RecursiveScratchPermit {
    state: Rc<RecursiveScratchState>,
    bytes: usize,
}

impl RecursiveScratchBudget {
    fn new(limit: usize) -> Self {
        Self {
            state: Rc::new(RecursiveScratchState {
                used: Cell::new(0),
                limit,
            }),
        }
    }

    fn try_reserve(&self, bytes: usize) -> Option<RecursiveScratchPermit> {
        let next = self.state.used.get().checked_add(bytes)?;
        if next > self.state.limit {
            return None;
        }
        self.state.used.set(next);
        Some(RecursiveScratchPermit {
            state: Rc::clone(&self.state),
            bytes,
        })
    }
}

impl Drop for RecursiveScratchPermit {
    fn drop(&mut self) {
        self.state.used.set(
            self.state
                .used
                .get()
                .checked_sub(self.bytes)
                .expect("recursive scratch accounting must not underflow"),
        );
    }
}

impl RecursiveDispatchBudget {
    #[cfg(test)]
    fn new(limits: crate::containers::ContainerLimits) -> Self {
        // This test helper constrains aggregate child dispatch while retaining
        // the ordinary archive-inspection contract. Production allowance
        // construction uses `new_with_parser_allowance` directly.
        let mut budget =
            Self::new_with_parser_allowance(crate::containers::ContainerLimits::default(), None);
        budget.remaining_encountered_members = limits.max_members;
        budget.remaining_dispatch_members = limits.max_members;
        budget.remaining_declared_bytes = limits.max_total_uncompressed_bytes;
        budget
    }

    #[cfg(test)]
    fn new_with_parser_allowance(
        limits: crate::containers::ContainerLimits,
        parser_allowance_bytes: Option<usize>,
    ) -> Self {
        Self::new_with_parser_allowance_and_cancellation(limits, parser_allowance_bytes, None)
    }

    fn new_with_parser_allowance_and_cancellation(
        limits: crate::containers::ContainerLimits,
        parser_allowance_bytes: Option<usize>,
        cancellation: Option<graphoxide_index_runtime::RuntimeCancellation>,
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
            cancellation,
        )
    }

    #[cfg(test)]
    fn with_output_fact_limit(
        limits: crate::containers::ContainerLimits,
        output_fact_limit: usize,
    ) -> Self {
        Self::with_output_fact_limit_and_parser_allowance(limits, output_fact_limit, None, None)
    }

    fn with_output_fact_limit_and_parser_allowance(
        limits: crate::containers::ContainerLimits,
        output_fact_limit: usize,
        parser_allowance_bytes: Option<usize>,
        cancellation: Option<graphoxide_index_runtime::RuntimeCancellation>,
    ) -> Self {
        let scratch_limit =
            usize::try_from(limits.max_total_uncompressed_bytes).unwrap_or(usize::MAX);
        Self {
            remaining_encountered_members: limits.max_members,
            remaining_dispatch_members: limits.max_members,
            remaining_declared_bytes: limits.max_total_uncompressed_bytes,
            remaining_output_facts: output_fact_limit,
            output_fact_limit,
            container_limits: limits,
            parser_allowance_bytes,
            cancellation,
            scratch: RecursiveScratchBudget::new(scratch_limit),
        }
    }

    fn admit_encounter(&mut self) -> bool {
        let Some(remaining) = self.remaining_encountered_members.checked_sub(1) else {
            return false;
        };
        self.remaining_encountered_members = remaining;
        true
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

    fn admit_decoded_member(&mut self, decoded_bytes: usize) -> bool {
        let Ok(decoded_bytes) = u64::try_from(decoded_bytes) else {
            return false;
        };
        let Some(remaining_bytes) = self.remaining_declared_bytes.checked_sub(decoded_bytes) else {
            return false;
        };
        let Some(remaining_dispatch) = self.remaining_dispatch_members.checked_sub(1) else {
            return false;
        };
        let Some(remaining_encountered) = self.remaining_encountered_members.checked_sub(1) else {
            return false;
        };
        self.remaining_declared_bytes = remaining_bytes;
        self.remaining_dispatch_members = remaining_dispatch;
        self.remaining_encountered_members = remaining_encountered;
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

    fn is_cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(graphoxide_index_runtime::RuntimeCancellation::is_cancelled)
    }

    fn cancellation(&self) -> Option<&graphoxide_index_runtime::RuntimeCancellation> {
        self.cancellation.as_ref()
    }

    fn try_reserve_scratch(&self, bytes: usize) -> Option<RecursiveScratchPermit> {
        self.scratch.try_reserve(bytes)
    }
}

pub(crate) fn bounded_container_limits(
    allowance_bytes: usize,
) -> crate::containers::ContainerLimits {
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

fn mark_container_source(
    extraction: &mut Extraction,
    container_source: &str,
    default_fact_source: &str,
) {
    let value = serde_json::Value::String(container_source.to_owned());
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
            object
                .entry("source_file")
                .or_insert_with(|| default_fact_source.into());
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

fn generated_container_member_id(stem: &str, member_path: &str, attempt: u64) -> String {
    let legacy = make_id(&[stem, "member", member_path]);
    let mut owner = Vec::with_capacity(stem.len() + member_path.len() + 10);
    owner.extend_from_slice(stem.as_bytes());
    owner.push(0);
    owner.extend_from_slice(member_path.as_bytes());
    owner.push(0);
    owner.extend_from_slice(&attempt.to_le_bytes());
    format!("{}_{}", legacy, blake3::hash(&owner).to_hex())
}

fn generated_container_fact_id(
    domain: &[u8],
    member_source: &str,
    id: &str,
    attempt: u64,
) -> String {
    let mut owner = Vec::with_capacity(domain.len() + member_source.len() + id.len() + 11);
    owner.extend_from_slice(domain);
    owner.push(0);
    owner.extend_from_slice(member_source.as_bytes());
    owner.push(0);
    owner.extend_from_slice(id.as_bytes());
    owner.push(0);
    owner.extend_from_slice(&attempt.to_le_bytes());
    format!("{}_{}", id, blake3::hash(&owner).to_hex())
}

fn collision_safe_owned_id_plan(
    domain: &[u8],
    owners: &BTreeMap<String, BTreeSet<usize>>,
    owner_names: &[String],
    reserved: &BTreeSet<String>,
) -> Vec<BTreeMap<String, String>> {
    let mut all_reserved = reserved.clone();
    all_reserved.extend(owners.keys().cloned());
    let mut used = reserved.clone();
    let mut plan = vec![BTreeMap::new(); owner_names.len()];
    for (id, id_owners) in owners {
        let preserve_legacy = id_owners.len() == 1 && !reserved.contains(id);
        for owner in id_owners {
            let owner_name = owner_names
                .get(*owner)
                .expect("container fact owner index must be valid");
            let assigned = if preserve_legacy {
                id.clone()
            } else {
                let mut attempt = 0_u64;
                loop {
                    let candidate = generated_container_fact_id(domain, owner_name, id, attempt);
                    if !all_reserved.contains(&candidate) && !used.contains(&candidate) {
                        break candidate;
                    }
                    attempt = attempt
                        .checked_add(1)
                        .expect("container fact ID attempts must not overflow");
                }
            };
            used.insert(assigned.clone());
            plan[*owner].insert(id.clone(), assigned);
        }
    }
    plan
}

fn remap_container_extraction_ids(
    extraction: &mut Extraction,
    node_remap: &BTreeMap<String, String>,
    hyperedge_remap: &BTreeMap<String, String>,
) {
    for node in &mut extraction.nodes {
        if let Some(id) = node_remap.get(&node.id) {
            node.id.clone_from(id);
        }
    }
    for edge in &mut extraction.edges {
        if let Some(id) = node_remap.get(&edge.source) {
            edge.source.clone_from(id);
        }
        if let Some(id) = node_remap.get(&edge.target) {
            edge.target.clone_from(id);
        }
        for field in ["_src", "_tgt"] {
            if let Some(id) = edge
                .extra
                .get(field)
                .and_then(serde_json::Value::as_str)
                .and_then(|id| node_remap.get(id))
            {
                edge.extra.insert(field.into(), id.clone().into());
            }
        }
    }
    for hyperedge in &mut extraction.hyperedges {
        let Some(object) = hyperedge.as_object_mut() else {
            continue;
        };
        if let Some(id) = object
            .get("id")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| hyperedge_remap.get(id))
        {
            object.insert("id".into(), id.clone().into());
        }
        for field in ["source", "target", "from", "to"] {
            if let Some(id) = object
                .get(field)
                .and_then(serde_json::Value::as_str)
                .and_then(|id| node_remap.get(id))
            {
                object.insert(field.into(), id.clone().into());
            }
        }
        for field in ["nodes", "members", "node_ids"] {
            let Some(members) = object
                .get_mut(field)
                .and_then(serde_json::Value::as_array_mut)
            else {
                continue;
            };
            for member in members {
                let Some(id) = member.as_str().and_then(|id| node_remap.get(id)) else {
                    continue;
                };
                *member = id.clone().into();
            }
        }
    }
}

/// Apply the repository's sensitive-path policy to a normalized archive path
/// without ever probing that logical path on disk. Extensionless names need a
/// lexical fast path because the compatibility detector may inspect a shebang
/// when classifying an ordinary extensionless source file.
pub(crate) fn is_sensitive_container_member_path(member_path: &str) -> bool {
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
        return crate::detect::is_sensitive_path_only(path);
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
    let parser_allowance = dispatch_budget.parser_allowance_at_depth(recursion_depth);
    if dispatch_budget.is_cancelled() {
        return rejected_inventory_extraction(&path, &member_source_file, "cancelled");
    }
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
    // Recursive generic AST and compatibility extractors do not pass through
    // the registered adapter preflight. Apply it only after container and
    // structured ownership has declined, so nested archives retain their
    // independently bounded member policy.
    if parser_plan_rejected(parser_allowance, bytes.len()) {
        return rejected_inventory_extraction(&path, &member_source_file, "parser_arena_budget");
    }
    let result = parser_allowance.map_or_else(
        || crate::engine::extract_as_bytes(&path, &member_source_file, bytes),
        |allowance| {
            if let Some(cancellation) = dispatch_budget.cancellation() {
                crate::engine::extract_as_bytes_with_parser_allowance_and_cancellation(
                    &path,
                    &member_source_file,
                    bytes,
                    allowance,
                    cancellation,
                )
            } else {
                crate::engine::extract_as_bytes_with_parser_allowance(
                    &path,
                    &member_source_file,
                    bytes,
                    allowance,
                )
            }
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
    if dispatch_budget.is_cancelled() {
        return Err("cancelled");
    }
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

    dispatch_admitted_container_member(
        container_path,
        source_file,
        member,
        bytes,
        recursion_depth,
        dispatch_budget,
    )
}

fn dispatch_admitted_container_member(
    container_path: &Path,
    source_file: &str,
    member: &crate::containers::ContainerMember,
    bytes: &[u8],
    recursion_depth: u16,
    dispatch_budget: &mut RecursiveDispatchBudget,
) -> Result<Option<ExtractedContainerMember>, &'static str> {
    if dispatch_budget.is_cancelled() {
        return Err("cancelled");
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
    if dispatch_budget.is_cancelled() {
        return Err("cancelled");
    }
    let member_source_file = virtual_member_source_file(source_file, &member.path);
    mark_container_source(&mut extraction, source_file, &member_source_file);
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
        inspect_svgz_bounded, recursive_archive_kind, visit_tar_members_bounded_with_encounter,
        visit_zip_members_bounded_with_encounter, ByteInventory, InspectionStatus,
        SvgReferenceRelation,
    };

    let source_name = path.file_name()?.to_string_lossy();
    let limits = dispatch_budget.container_limits;
    let mut extracted_members = Vec::new();
    let mut member_dispatch_statuses = BTreeMap::new();
    let mut dispatch_stop_reason = None;
    let mut admitted_encountered_members = None;
    let recursive_kind = recursive_archive_kind(&source_name, source);
    let svgz =
        source_name.to_ascii_lowercase().ends_with(".svgz") && source.starts_with(b"\x1f\x8b");
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
        Some(ArchiveKind::Tar) => {
            let branch_reason = Cell::new(None);
            let encountered = Cell::new(0_usize);
            let inspection = {
                let budget = RefCell::new(&mut *dispatch_budget);
                visit_tar_members_bounded_with_encounter(
                    source,
                    recursion_depth,
                    limits,
                    || budget.borrow().is_cancelled(),
                    |_| {
                        if budget.borrow_mut().admit_encounter() {
                            encountered.set(encountered.get() + 1);
                            true
                        } else {
                            branch_reason.set(Some("aggregate_encountered_member_limit"));
                            false
                        }
                    },
                    |child| {
                        match dispatch_container_member(
                            path,
                            source_file,
                            child.member,
                            child.bytes,
                            recursion_depth + 1,
                            &mut budget.borrow_mut(),
                            &mut member_dispatch_statuses,
                        ) {
                            Ok(Some(extracted)) => extracted_members.push(extracted),
                            Ok(None) => {}
                            Err(reason) => {
                                branch_reason.set(Some(reason));
                                return false;
                            }
                        }
                        true
                    },
                )
            };
            admitted_encountered_members = Some(encountered.get());
            dispatch_stop_reason = branch_reason.get();
            ByteInventory::Container(inspection)
        }
        Some(ArchiveKind::Zip) => {
            let branch_reason = Cell::new(None);
            let encountered = Cell::new(0_usize);
            let inspection = {
                let budget = RefCell::new(&mut *dispatch_budget);
                visit_zip_members_bounded_with_encounter(
                    source,
                    recursion_depth,
                    limits,
                    || budget.borrow().is_cancelled(),
                    |_| {
                        if budget.borrow_mut().admit_encounter() {
                            encountered.set(encountered.get() + 1);
                            true
                        } else {
                            branch_reason.set(Some("aggregate_encountered_member_limit"));
                            false
                        }
                    },
                    |member| {
                        if is_sensitive_container_member_path(&member.path) {
                            member_dispatch_statuses
                                .insert(member.path.clone(), "sensitive_path_skipped");
                            return CompressedMemberAdmission::Skip;
                        }
                        let mut budget = budget.borrow_mut();
                        if !budget.can_reserve_output_facts(2) {
                            branch_reason.set(Some("aggregate_fact_limit"));
                            return CompressedMemberAdmission::Stop;
                        }
                        if !budget.admit_dispatch(member) {
                            branch_reason.set(Some("aggregate_member_or_byte_limit"));
                            return CompressedMemberAdmission::Stop;
                        }
                        let Ok(bytes) = usize::try_from(member.declared_uncompressed_bytes) else {
                            branch_reason.set(Some("aggregate_member_or_byte_limit"));
                            return CompressedMemberAdmission::Stop;
                        };
                        match budget.try_reserve_scratch(bytes) {
                            Some(permit) => CompressedMemberAdmission::Dispatch(permit),
                            None => {
                                branch_reason.set(Some("aggregate_scratch_limit"));
                                CompressedMemberAdmission::Stop
                            }
                        }
                    },
                    |child| {
                        match dispatch_admitted_container_member(
                            path,
                            source_file,
                            child.member,
                            child.bytes,
                            recursion_depth + 1,
                            &mut budget.borrow_mut(),
                        ) {
                            Ok(Some(extracted)) => extracted_members.push(extracted),
                            Ok(None) => {}
                            Err(reason) => {
                                branch_reason.set(Some(reason));
                                return false;
                            }
                        }
                        true
                    },
                )
            };
            admitted_encountered_members = Some(encountered.get());
            dispatch_stop_reason = branch_reason.get();
            ByteInventory::Container(inspection)
        }
        Some(
            ArchiveKind::Gzip
            | ArchiveKind::Bzip2
            | ArchiveKind::Xz
            | ArchiveKind::Zstd
            | ArchiveKind::Lz4,
        ) => {
            let (inspection, admitted, stop_reason) = dispatch_single_stream_container(
                recursive_kind,
                &source_name,
                path,
                source_file,
                source,
                recursion_depth,
                limits,
                dispatch_budget,
                &mut extracted_members,
                &mut member_dispatch_statuses,
            )
            .expect("single-stream arms only match a `Some` recursive kind");
            admitted_encountered_members = Some(admitted);
            dispatch_stop_reason = stop_reason;
            ByteInventory::Container(inspection)
        }
        _ if svgz => {
            let budget = RefCell::new(&mut *dispatch_budget);
            ByteInventory::Media(inspect_svgz_bounded(
                source,
                recursion_depth,
                limits,
                || budget.borrow().is_cancelled(),
                |bytes| {
                    let mut budget = budget.borrow_mut();
                    let permit = budget.try_reserve_scratch(bytes)?;
                    budget.admit_decoded_member(bytes).then_some(permit)
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
            let requested_member_count = members.len();
            let encountered_limit = if let Some(admitted) = admitted_encountered_members {
                admitted
            } else if recursive_output_accounting {
                let mut admitted = 0_usize;
                while admitted < members.len() && dispatch_budget.admit_encounter() {
                    admitted += 1;
                }
                admitted
            } else {
                members.len()
            };
            if encountered_limit < members.len() {
                members.truncate(encountered_limit);
                container.status = InspectionStatus::InventoryOnly;
                dispatch_stop_reason.get_or_insert("aggregate_encountered_member_limit");
                discard_extracted_members(&mut extracted_members, dispatch_budget);
            }
            for member in &members {
                if is_sensitive_container_member_path(&member.path) {
                    member_dispatch_statuses
                        .entry(member.path.clone())
                        .or_insert("sensitive_path_skipped");
                }
            }
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
            let mut member_id_owners = BTreeMap::<String, BTreeSet<String>>::new();
            for member in &members {
                member_id_owners
                    .entry(make_id(&[&stem, "member", &member.path]))
                    .or_default()
                    .insert(member.path.clone());
            }
            let reserved_legacy_member_ids =
                member_id_owners.keys().cloned().collect::<BTreeSet<_>>();
            let mut used_inventory_ids = BTreeSet::from([file_id.clone()]);
            let mut member_ids = BTreeMap::new();
            for member in &members {
                let legacy = make_id(&[&stem, "member", &member.path]);
                let unique_legacy = member_id_owners
                    .get(&legacy)
                    .is_some_and(|owners| owners.len() == 1)
                    && !used_inventory_ids.contains(&legacy);
                let id = if unique_legacy {
                    legacy
                } else {
                    let mut attempt = 0_u64;
                    loop {
                        let candidate = generated_container_member_id(&stem, &member.path, attempt);
                        if !reserved_legacy_member_ids.contains(&candidate)
                            && !used_inventory_ids.contains(&candidate)
                        {
                            break candidate;
                        }
                        attempt = attempt
                            .checked_add(1)
                            .expect("container member ID attempts must not overflow");
                    }
                };
                used_inventory_ids.insert(id.clone());
                member_ids.insert(member.path.clone(), id);
            }

            let member_sources = extracted_members
                .iter()
                .map(|extracted| virtual_member_source_file(source_file, &extracted.path))
                .collect::<Vec<_>>();
            let mut fact_id_owners = BTreeMap::<String, BTreeSet<usize>>::new();
            let mut hyperedge_id_owners = BTreeMap::<String, BTreeSet<usize>>::new();
            for (owner, extracted) in extracted_members.iter().enumerate() {
                for node in &extracted.extraction.nodes {
                    fact_id_owners
                        .entry(node.id.clone())
                        .or_default()
                        .insert(owner);
                }
                for id in extracted
                    .extraction
                    .hyperedges
                    .iter()
                    .filter_map(|hyperedge| hyperedge.get("id"))
                    .filter_map(serde_json::Value::as_str)
                {
                    hyperedge_id_owners
                        .entry(id.to_owned())
                        .or_default()
                        .insert(owner);
                }
            }
            let node_plan = collision_safe_owned_id_plan(
                b"node",
                &fact_id_owners,
                &member_sources,
                &used_inventory_ids,
            );
            let hyperedge_plan = collision_safe_owned_id_plan(
                b"hyperedge",
                &hyperedge_id_owners,
                &member_sources,
                &BTreeSet::new(),
            );
            let empty_remap = BTreeMap::new();
            for (owner, extracted) in extracted_members.iter_mut().enumerate() {
                remap_container_extraction_ids(
                    &mut extracted.extraction,
                    node_plan.get(owner).unwrap_or(&empty_remap),
                    hyperedge_plan.get(owner).unwrap_or(&empty_remap),
                );
            }

            let mut nodes = vec![root];
            let mut edges = Vec::new();
            let mut hyperedges = Vec::new();
            for member in members {
                let member_id = member_ids
                    .get(&member.path)
                    .cloned()
                    .expect("every admitted member must have an ID");
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
                    hyperedges.extend(extracted.extraction.hyperedges);
                    for child_id in child_ids {
                        edges.push(contains_edge(member_id, &child_id, source_file));
                    }
                }
            }
            Some(Extraction {
                nodes,
                edges,
                hyperedges,
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

/// Dispatch one single-stream compressed member (GZIP, BZIP2, XZ, Zstandard,
/// or LZ4) under caller-owned scratch admission, sharing the recursive member
/// dispatch, encounter accounting, and stop-reason policy of the other
/// container kinds.
#[allow(clippy::too_many_arguments)]
fn dispatch_single_stream_container(
    kind: Option<ArchiveKind>,
    source_name: &str,
    path: &Path,
    source_file: &str,
    source: &[u8],
    recursion_depth: u16,
    limits: ContainerLimits,
    dispatch_budget: &mut RecursiveDispatchBudget,
    extracted_members: &mut Vec<ExtractedContainerMember>,
    member_dispatch_statuses: &mut BTreeMap<String, &'static str>,
) -> Option<(ContainerInspection, usize, Option<&'static str>)> {
    use crate::containers::{
        visit_bzip2_member_bounded, visit_gzip_member_bounded_with_encounter,
        visit_lz4_member_bounded, visit_xz_member_bounded, visit_zstd_member_bounded,
    };
    let kind = kind?;
    let branch_reason = Cell::new(None);
    let encountered = Cell::new(0_usize);
    let budget = RefCell::new(dispatch_budget);
    let inspection = match kind {
        ArchiveKind::Gzip => visit_gzip_member_bounded_with_encounter(
            source_name,
            source,
            recursion_depth,
            limits,
            || budget.borrow().is_cancelled(),
            |member| single_stream_encounter(&budget, &branch_reason, &encountered, member),
            |member| {
                single_stream_admission(
                    &mut budget.borrow_mut(),
                    &branch_reason,
                    member,
                    member_dispatch_statuses,
                )
            },
            |child| {
                dispatch_container_child(
                    child,
                    path,
                    source_file,
                    recursion_depth,
                    &mut budget.borrow_mut(),
                    &branch_reason,
                    extracted_members,
                )
            },
        ),
        ArchiveKind::Bzip2 => visit_bzip2_member_bounded(
            source_name,
            source,
            recursion_depth,
            limits,
            || budget.borrow().is_cancelled(),
            |member| single_stream_encounter(&budget, &branch_reason, &encountered, member),
            |member| {
                single_stream_admission(
                    &mut budget.borrow_mut(),
                    &branch_reason,
                    member,
                    member_dispatch_statuses,
                )
            },
            |child| {
                dispatch_container_child(
                    child,
                    path,
                    source_file,
                    recursion_depth,
                    &mut budget.borrow_mut(),
                    &branch_reason,
                    extracted_members,
                )
            },
        ),
        ArchiveKind::Xz => visit_xz_member_bounded(
            source_name,
            source,
            recursion_depth,
            limits,
            || budget.borrow().is_cancelled(),
            |member| single_stream_encounter(&budget, &branch_reason, &encountered, member),
            |member| {
                single_stream_admission(
                    &mut budget.borrow_mut(),
                    &branch_reason,
                    member,
                    member_dispatch_statuses,
                )
            },
            |child| {
                dispatch_container_child(
                    child,
                    path,
                    source_file,
                    recursion_depth,
                    &mut budget.borrow_mut(),
                    &branch_reason,
                    extracted_members,
                )
            },
        ),
        ArchiveKind::Zstd => visit_zstd_member_bounded(
            source_name,
            source,
            recursion_depth,
            limits,
            || budget.borrow().is_cancelled(),
            |member| single_stream_encounter(&budget, &branch_reason, &encountered, member),
            |member| {
                single_stream_admission(
                    &mut budget.borrow_mut(),
                    &branch_reason,
                    member,
                    member_dispatch_statuses,
                )
            },
            |child| {
                dispatch_container_child(
                    child,
                    path,
                    source_file,
                    recursion_depth,
                    &mut budget.borrow_mut(),
                    &branch_reason,
                    extracted_members,
                )
            },
        ),
        ArchiveKind::Lz4 => visit_lz4_member_bounded(
            source_name,
            source,
            recursion_depth,
            limits,
            || budget.borrow().is_cancelled(),
            |member| single_stream_encounter(&budget, &branch_reason, &encountered, member),
            |member| {
                single_stream_admission(
                    &mut budget.borrow_mut(),
                    &branch_reason,
                    member,
                    member_dispatch_statuses,
                )
            },
            |child| {
                dispatch_container_child(
                    child,
                    path,
                    source_file,
                    recursion_depth,
                    &mut budget.borrow_mut(),
                    &branch_reason,
                    extracted_members,
                )
            },
        ),
        ArchiveKind::SevenZip | ArchiveKind::Rar => {
            unreachable!("7z and RAR are not single-stream dispatchable")
        }
        ArchiveKind::Tar => unreachable!("tar dispatch is handled separately"),
        ArchiveKind::Zip => unreachable!("zip dispatch is handled separately"),
    };
    Some((inspection, encountered.get(), branch_reason.get()))
}

/// Shared encounter accounting for single-stream dispatch: admit one member
/// until the aggregate encountered-member budget is exhausted.
fn single_stream_encounter(
    budget: &RefCell<&mut RecursiveDispatchBudget>,
    branch_reason: &Cell<Option<&'static str>>,
    encountered: &Cell<usize>,
    _member: &crate::containers::ContainerMember,
) -> bool {
    if budget.borrow_mut().admit_encounter() {
        encountered.set(encountered.get() + 1);
        true
    } else {
        branch_reason.set(Some("aggregate_encountered_member_limit"));
        false
    }
}

/// Shared admission policy for single-stream dispatch: skip sensitive paths,
/// then reserve output facts, the member/byte budget, and a bounded scratch
/// permit.
fn single_stream_admission(
    budget: &mut RecursiveDispatchBudget,
    branch_reason: &Cell<Option<&'static str>>,
    member: &crate::containers::ContainerMember,
    member_dispatch_statuses: &mut BTreeMap<String, &'static str>,
) -> CompressedMemberAdmission<RecursiveScratchPermit> {
    if is_sensitive_container_member_path(&member.path) {
        member_dispatch_statuses.insert(member.path.clone(), "sensitive_path_skipped");
        return CompressedMemberAdmission::Skip;
    }
    if !budget.can_reserve_output_facts(2) {
        branch_reason.set(Some("aggregate_fact_limit"));
        return CompressedMemberAdmission::Stop;
    }
    if !budget.admit_dispatch(member) {
        branch_reason.set(Some("aggregate_member_or_byte_limit"));
        return CompressedMemberAdmission::Stop;
    }
    let Ok(bytes) = usize::try_from(member.declared_uncompressed_bytes) else {
        branch_reason.set(Some("aggregate_member_or_byte_limit"));
        return CompressedMemberAdmission::Stop;
    };
    match budget.try_reserve_scratch(bytes) {
        Some(permit) => CompressedMemberAdmission::Dispatch(permit),
        None => {
            branch_reason.set(Some("aggregate_scratch_limit"));
            CompressedMemberAdmission::Stop
        }
    }
}

/// Shared body for dispatching one decoded single-stream child member. Each
/// codec's `Dispatchable*Member` is destructured into its `member` and
/// `bytes` before the uniform recursive dispatch runs.
#[allow(clippy::too_many_arguments)]
fn dispatch_container_child<'member, 'bytes, Child>(
    child: Child,
    path: &Path,
    source_file: &str,
    recursion_depth: u16,
    budget: &mut RecursiveDispatchBudget,
    branch_reason: &Cell<Option<&'static str>>,
    extracted_members: &mut Vec<ExtractedContainerMember>,
) -> bool
where
    Child: SingleStreamChild<'member, 'bytes>,
{
    let (member, bytes) = child.into_parts();
    match dispatch_admitted_container_member(
        path,
        source_file,
        member,
        bytes,
        recursion_depth + 1,
        budget,
    ) {
        Ok(Some(extracted)) => extracted_members.push(extracted),
        Ok(None) => {}
        Err(reason) => {
            branch_reason.set(Some(reason));
            return false;
        }
    }
    true
}

/// Implemented by the five single-stream `Dispatchable*Member` types so a
/// single recursive dispatch body can be shared across codecs.
trait SingleStreamChild<'member, 'bytes> {
    fn into_parts(self) -> (&'member crate::containers::ContainerMember, &'bytes [u8]);
}

impl<'member, 'bytes> SingleStreamChild<'member, 'bytes>
    for crate::containers::DispatchableGzipMember<'member, 'bytes>
{
    fn into_parts(self) -> (&'member crate::containers::ContainerMember, &'bytes [u8]) {
        (self.member, self.bytes)
    }
}

impl<'member, 'bytes> SingleStreamChild<'member, 'bytes>
    for crate::containers::DispatchableBzip2Member<'member, 'bytes>
{
    fn into_parts(self) -> (&'member crate::containers::ContainerMember, &'bytes [u8]) {
        (self.member, self.bytes)
    }
}

impl<'member, 'bytes> SingleStreamChild<'member, 'bytes>
    for crate::containers::DispatchableXzMember<'member, 'bytes>
{
    fn into_parts(self) -> (&'member crate::containers::ContainerMember, &'bytes [u8]) {
        (self.member, self.bytes)
    }
}

impl<'member, 'bytes> SingleStreamChild<'member, 'bytes>
    for crate::containers::DispatchableZstdMember<'member, 'bytes>
{
    fn into_parts(self) -> (&'member crate::containers::ContainerMember, &'bytes [u8]) {
        (self.member, self.bytes)
    }
}

impl<'member, 'bytes> SingleStreamChild<'member, 'bytes>
    for crate::containers::DispatchableLz4Member<'member, 'bytes>
{
    fn into_parts(self) -> (&'member crate::containers::ContainerMember, &'bytes [u8]) {
        (self.member, self.bytes)
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

fn rejected_pdf_extraction(path: &Path, source_file: &str, diagnostic: &'static str) -> Extraction {
    let file_id = make_id(&[&source_stem(source_file)]);
    let mut node = inventory_file_node(path, source_file, &file_id, "pdf_document");
    node.file_type = "paper".into();
    node.extra.insert("format".into(), "pdf".into());
    node.extra.insert("_origin".into(), "pdf".into());
    node.extra
        .insert("format_capability".into(), "structural_partial".into());
    node.extra.insert("parse_status".into(), "rejected".into());
    node.extra.insert("diagnostic".into(), diagnostic.into());
    Extraction {
        nodes: vec![node],
        edges: Vec::new(),
        hyperedges: Vec::new(),
    }
}

fn rejected_office_extraction(
    path: &Path,
    source_file: &str,
    extension: &str,
    diagnostic: &'static str,
) -> Extraction {
    let file_id = make_id(&[&source_stem(source_file)]);
    let mut node = inventory_file_node(path, source_file, &file_id, "document_package");
    node.extra
        .insert("format".into(), extension.to_ascii_lowercase().into());
    node.extra
        .insert("_origin".into(), "document_package".into());
    node.extra
        .insert("format_capability".into(), "structural_partial".into());
    node.extra.insert("parse_status".into(), "rejected".into());
    node.extra.insert("diagnostic".into(), diagnostic.into());
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

    fn one_page_pdf(text: &str) -> Vec<u8> {
        assert!(text.is_ascii());
        let escaped = text
            .replace('\\', "\\\\")
            .replace('(', "\\(")
            .replace(')', "\\)");
        let content = format!("BT /F1 12 Tf 72 720 Td ({escaped}) Tj ET\n");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_owned(),
            format!("<< /Length {} >>\nstream\n{content}endstream", content.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
                .to_owned(),
        ];
        let mut pdf = b"%PDF-1.4\n%\x80\x80\x80\x80\n".to_vec();
        let mut offsets = Vec::with_capacity(objects.len());
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
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
        // A valid BZIP2 stream is now decoded and dispatched (parsed), no
        // longer inventory-only.
        let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::new(6));
        use std::io::Write as _;
        encoder.write_all(b"payload").expect("write bzip");
        let valid_bzip = encoder.finish().expect("finish bzip");
        let bzip = extract("fixtures/asset.tar.bz2", &valid_bzip);
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
            Some("parsed")
        );
        // A corrupt (truncated) BZIP2 stream fails closed and is rejected.
        let corrupt = extract("fixtures/asset.bz2", b"BZh9");
        let corrupt_root = corrupt.nodes.first().expect("container root");
        assert_eq!(
            corrupt_root
                .extra
                .get("inspection_status")
                .and_then(serde_json::Value::as_str),
            Some("rejected")
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
        mark_container_source(
            &mut extraction,
            "archive.tar",
            "archive.tar!/nested/config.toml",
        );

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
    fn member_namespace_remaps_nodes_edges_and_hyperedges_together() {
        let mut edge = contains_edge("a", "b", "archive.zip!/member.dot");
        edge.extra.insert("_src".into(), "a".into());
        edge.extra.insert("_tgt".into(), "b".into());
        let mut extraction = Extraction {
            nodes: vec![
                inventory_file_node(Path::new("a"), "a", "a", "document"),
                inventory_file_node(Path::new("b"), "b", "b", "document"),
            ],
            edges: vec![edge],
            hyperedges: vec![serde_json::json!({
                "id": "group",
                "nodes": ["a", "b"],
                "source": "a",
                "target": "b"
            })],
        };
        let first_owner = "archive.zip!/member.dot".to_owned();
        let sibling_owner = "archive.zip!/sibling.dot".to_owned();
        let owner_names = vec![first_owner, sibling_owner];
        let duplicate_owners = BTreeSet::from([0_usize, 1_usize]);
        let node_plan = collision_safe_owned_id_plan(
            b"node",
            &BTreeMap::from([
                ("a".into(), duplicate_owners.clone()),
                ("b".into(), duplicate_owners.clone()),
            ]),
            &owner_names,
            &BTreeSet::new(),
        );
        let hyperedge_plan = collision_safe_owned_id_plan(
            b"hyperedge",
            &BTreeMap::from([("group".into(), duplicate_owners)]),
            &owner_names,
            &BTreeSet::new(),
        );
        remap_container_extraction_ids(
            &mut extraction,
            node_plan.first().expect("first node remap"),
            hyperedge_plan.first().expect("first hyperedge remap"),
        );
        mark_container_source(&mut extraction, "archive.zip", "archive.zip!/member.dot");

        let ids = extraction
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(extraction.edges[0].source.as_str()));
        assert!(ids.contains(extraction.edges[0].target.as_str()));
        assert!(ids.contains(extraction.edges[0].true_source()));
        assert!(ids.contains(extraction.edges[0].true_target()));
        assert!(extraction.hyperedges[0]["nodes"]
            .as_array()
            .is_some_and(|members| members
                .iter()
                .all(|member| member.as_str().is_some_and(|id| ids.contains(id)))));
        assert_eq!(
            extraction.hyperedges[0]["source_file"],
            "archive.zip!/member.dot"
        );
        assert_eq!(
            extraction.hyperedges[0][CONTAINER_SOURCE_ATTRIBUTE],
            "archive.zip"
        );
        let first_group = extraction.hyperedges[0]["id"]
            .as_str()
            .expect("remapped hyperedge ID")
            .to_owned();
        let mut sibling = Extraction {
            nodes: Vec::new(),
            edges: Vec::new(),
            hyperedges: vec![serde_json::json!({ "id": "group" })],
        };
        remap_container_extraction_ids(
            &mut sibling,
            &BTreeMap::new(),
            hyperedge_plan.get(1).expect("sibling hyperedge remap"),
        );
        assert_ne!(sibling.hyperedges[0]["id"], first_group);
    }

    #[test]
    fn owned_id_plan_avoids_generated_candidates_copied_by_a_third_member() {
        let first = "archive.zip!/a.dot";
        let second = "archive.zip!/b.dot";
        let third = "archive.zip!/c.dot";
        let owner_names = vec![first.into(), second.into(), third.into()];
        let copied = generated_container_fact_id(b"node", first, "x", 0);
        let owners = BTreeMap::from([
            ("x".into(), BTreeSet::from([0_usize, 1_usize])),
            (copied.clone(), BTreeSet::from([2_usize])),
        ]);
        let plan = collision_safe_owned_id_plan(b"node", &owners, &owner_names, &BTreeSet::new());
        let assigned = [
            plan[0]["x"].as_str(),
            plan[1]["x"].as_str(),
            plan[2][&copied].as_str(),
        ]
        .into_iter()
        .collect::<BTreeSet<_>>();
        assert_eq!(assigned.len(), 3);
        assert_eq!(plan[2][&copied], copied);
        assert_ne!(plan[0]["x"], copied);
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
    fn isolated_zip_dispatches_bounded_member_semantics() {
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
            Some("parsed")
        );
        assert_eq!(
            root.extra
                .get("decompressed_bytes")
                .and_then(serde_json::Value::as_u64),
            Some(b"digraph platform { gateway -> database; }".len() as u64)
        );
        assert_eq!(root.extra["format_capability"], "structural_partial");
        assert_eq!(root.extra["parse_status"], "partial");
        assert!(!root.extra.contains_key("recursive_dispatch_status"));
        assert!(extraction
            .nodes
            .iter()
            .any(|node| node.label == "design/architecture.dot"));
        assert!(extraction
            .nodes
            .iter()
            .any(|node| matches!(node.label.as_str(), "gateway" | "database")));
        assert!(has_relation(&extraction, "flows_to"));
    }

    #[test]
    fn isolated_gzip_and_tgz_dispatch_one_bounded_child() {
        let dot = b"digraph platform { gateway -> database; }";
        let gzip = gzip_bytes(dot);
        let gzip_extraction =
            extract_with_allowance("design/architecture.dot.gz", &gzip, 2 * 1024 * 1024);
        assert!(has_relation(&gzip_extraction, "flows_to"));
        assert!(gzip_extraction.nodes.iter().any(|node| {
            node.source_file == "design/architecture.dot.gz!/architecture.dot"
                && node.label == "gateway"
        }));

        let tar = tar_bytes(&[("design/architecture.dot", dot)]);
        let tgz = gzip_bytes(&tar);
        let tgz_extraction = extract_with_allowance("bundle.tgz", &tgz, 4 * 1024 * 1024);
        assert!(has_relation(&tgz_extraction, "flows_to"));
        assert!(tgz_extraction.nodes.iter().any(|node| {
            node.source_file == "bundle.tgz!/bundle.tar!/design/architecture.dot"
                && node.label == "database"
        }));
    }

    #[test]
    fn nested_svgz_decode_shares_the_tree_compressed_scratch_limit() {
        let svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><path id=\"{}\" d=\"M0 0L1 1\"/></svg>",
            (0..96)
                .map(|index| char::from(b'a' + (index % 26) as u8))
                .collect::<String>()
        );
        let svgz = gzip_bytes(svg.as_bytes());
        let scratch_limit = svg.len().max(svgz.len());
        assert!(svg.len() + svgz.len() > scratch_limit);
        let archive = zip_bytes(&[("media/diagram.svgz", &svgz)]);
        let limits = crate::containers::ContainerLimits {
            max_member_uncompressed_bytes: scratch_limit as u64,
            max_total_uncompressed_bytes: scratch_limit as u64,
            max_svg_bytes: scratch_limit,
            ..crate::containers::ContainerLimits::default()
        };
        let mut budget = RecursiveDispatchBudget::with_output_fact_limit(limits, 128);
        let extraction = container_inventory_extraction(
            Path::new("media.zip"),
            "media.zip",
            &archive,
            0,
            &mut budget,
        )
        .expect("bounded SVGZ archive inventory");

        let svgz_root = extraction
            .nodes
            .iter()
            .find(|node| node.source_file == "media.zip!/media/diagram.svgz")
            .expect("SVGZ inventory root");
        assert_eq!(svgz_root.extra["inspection_status"], "inventoryonly");
        assert!(svgz_root.extra["diagnostics"]
            .as_array()
            .is_some_and(|diagnostics| diagnostics
                .iter()
                .any(|diagnostic| diagnostic == "nesteddispatchstopped")));
        assert!(!extraction.nodes.iter().any(|node| {
            node.source_file == "media.zip!/media/diagram.svgz"
                && node
                    .extra
                    .get("type")
                    .is_some_and(|kind| kind == "svg_element")
        }));
    }

    #[test]
    fn repeated_svgz_members_share_the_tree_decoded_byte_limit() {
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect id=\"item\"/></svg>";
        let svgz = gzip_bytes(svg);
        let archive = tar_bytes(&[("a.svgz", &svgz), ("b.svgz", &svgz)]);
        let decoded_limit = svgz
            .len()
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(svg.len()))
            .expect("fixture limit");
        let limits = crate::containers::ContainerLimits {
            max_member_uncompressed_bytes: decoded_limit as u64,
            max_total_uncompressed_bytes: decoded_limit as u64,
            max_svg_bytes: svg.len(),
            ..crate::containers::ContainerLimits::default()
        };
        let mut budget = RecursiveDispatchBudget::with_output_fact_limit(limits, 128);
        let extraction = container_inventory_extraction(
            Path::new("many.tar"),
            "many.tar",
            &archive,
            0,
            &mut budget,
        )
        .expect("bounded repeated SVGZ inventory");

        assert!(extraction.nodes.iter().any(|node| {
            node.source_file == "many.tar!/a.svgz"
                && node
                    .extra
                    .get("type")
                    .is_some_and(|kind| kind == "svg_element")
        }));
        let second = extraction
            .nodes
            .iter()
            .find(|node| {
                node.source_file == "many.tar!/b.svgz" && node.extra.contains_key("diagnostics")
            })
            .expect("second SVGZ inventory root");
        assert_eq!(second.extra["inspection_status"], "inventoryonly");
        assert!(second.extra["diagnostics"]
            .as_array()
            .is_some_and(|diagnostics| diagnostics
                .iter()
                .any(|diagnostic| diagnostic == "nesteddispatchstopped")));
        assert!(!extraction.nodes.iter().any(|node| {
            node.source_file == "many.tar!/b.svgz"
                && node
                    .extra
                    .get("type")
                    .is_some_and(|kind| kind == "svg_element")
        }));
    }

    #[test]
    fn normalization_colliding_member_paths_keep_disjoint_semantic_ids() {
        let archive = zip_bytes(&[
            ("a-b.dot", b"digraph first { source_a -> target_a; }"),
            ("a_b.dot", b"digraph second { source_b -> target_b; }"),
        ]);
        let extraction = extract_with_allowance("collisions.zip", &archive, 4 * 1024 * 1024);
        let first = extraction
            .nodes
            .iter()
            .filter(|node| node.source_file == "collisions.zip!/a-b.dot")
            .map(|node| node.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let second = extraction
            .nodes
            .iter()
            .filter(|node| node.source_file == "collisions.zip!/a_b.dot")
            .map(|node| node.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert!(!first.is_empty());
        assert!(!second.is_empty());
        assert!(first.is_disjoint(&second));
        let member_ids = extraction
            .nodes
            .iter()
            .filter(|node| {
                node.source_file == "collisions.zip"
                    && matches!(node.label.as_str(), "a-b.dot" | "a_b.dot")
            })
            .map(|node| node.id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(member_ids.len(), 2);
        assert_eq!(
            extraction
                .edges
                .iter()
                .filter(|edge| edge.relation == "flows_to")
                .count(),
            2
        );
    }

    #[test]
    fn collision_free_tar_members_preserve_legacy_fact_and_anchor_ids() {
        let dot = b"digraph stable { gateway -> database; }";
        let member_source = "single.tar!/design/stable.dot";
        let raw = crate::engine::extract_as_bytes(Path::new(member_source), member_source, dot)
            .expect("legacy child extraction");
        let archive = extract("single.tar", &tar_bytes(&[("design/stable.dot", dot)]));

        for label in ["gateway", "database"] {
            let raw_id = &raw
                .nodes
                .iter()
                .find(|node| node.label == label)
                .expect("raw semantic node")
                .id;
            let archive_id = &archive
                .nodes
                .iter()
                .find(|node| node.source_file == member_source && node.label == label)
                .expect("archived semantic node")
                .id;
            assert_eq!(archive_id, raw_id, "{label}");
        }
        let member = archive
            .nodes
            .iter()
            .find(|node| node.label == "design/stable.dot")
            .expect("member inventory node");
        assert_eq!(
            member.id,
            make_id(&["single", "member", "design/stable.dot"])
        );
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
                    Some(
                        "membersizelimit"
                            | "totalsizelimit"
                            | "inputtoolarge"
                            | "compressionratiolimit"
                    )
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
        assert!(root.extra["diagnostics"]
            .as_array()
            .is_some_and(|diagnostics| diagnostics.iter().any(|diagnostic| matches!(
                diagnostic.as_str(),
                Some("membersizelimit" | "totalsizelimit" | "compressionratiolimit")
            ))));
    }

    #[test]
    fn isolated_pdf_budget_rejection_preserves_structural_document_identity() {
        let extraction = extract_with_allowance("document.pdf", b"%PDF-1.7\ninvalid", 16 * 1024);
        assert_eq!(extraction.nodes.len(), 1);
        assert!(extraction.edges.is_empty());
        assert!(extraction.hyperedges.is_empty());
        let root = extraction.nodes.first().expect("PDF rejected root");
        assert_eq!(root.file_type, "paper");
        assert_eq!(root.source_location, None);
        assert_eq!(
            root.extra
                .get("diagnostic")
                .and_then(serde_json::Value::as_str),
            Some("parser_arena_budget")
        );
        assert_eq!(
            root.extra
                .get("format_capability")
                .and_then(serde_json::Value::as_str),
            Some("structural_partial")
        );
        assert_eq!(
            root.extra.get("type").and_then(serde_json::Value::as_str),
            Some("pdf_document")
        );
        assert_eq!(
            root.extra.get("format").and_then(serde_json::Value::as_str),
            Some("pdf")
        );
        assert_eq!(
            root.extra
                .get("_origin")
                .and_then(serde_json::Value::as_str),
            Some("pdf")
        );
        assert_eq!(
            root.extra
                .get("parse_status")
                .and_then(serde_json::Value::as_str),
            Some("rejected")
        );
    }

    #[test]
    fn isolated_pdf_routes_to_bounded_page_facts_without_source_locations() {
        let source = one_page_pdf("Bounded adapter text");
        let extraction = extract_with_allowance("document.pdf", &source, 16 * 1024 * 1024);
        assert_eq!(extraction.nodes.len(), 2);
        assert_eq!(extraction.edges.len(), 1);
        assert!(extraction.hyperedges.is_empty());

        let root = extraction
            .nodes
            .iter()
            .find(|node| {
                node.extra.get("type").and_then(serde_json::Value::as_str) == Some("pdf_document")
            })
            .expect("PDF document root");
        let page = extraction
            .nodes
            .iter()
            .find(|node| {
                node.extra.get("type").and_then(serde_json::Value::as_str) == Some("pdf_page")
            })
            .expect("PDF page node");
        assert_eq!(root.file_type, "paper");
        assert_eq!(page.file_type, "paper");
        assert_eq!(root.source_location, None);
        assert_eq!(page.source_location, None);
        assert_eq!(
            page.extra
                .get("page_number")
                .and_then(serde_json::Value::as_u64),
            Some(1)
        );
        assert!(page
            .extra
            .get("text")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| text.contains("Bounded adapter text")));
        assert!(extraction.edges.iter().all(|edge| {
            edge.relation == "contains"
                && edge.source_file == "document.pdf"
                && edge
                    .extra
                    .get("_origin")
                    .and_then(serde_json::Value::as_str)
                    == Some("pdf")
        }));
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
    fn nested_directories_share_one_tree_wide_encounter_limit() {
        let directories = ["a/", "b/", "c/", "d/"];
        let nested = zip_directory_bytes(&directories);
        let outer = zip_bytes(&[("nested/archive.zip", &nested)]);
        let mut budget = RecursiveDispatchBudget::new(crate::containers::ContainerLimits {
            max_members: 4,
            ..crate::containers::ContainerLimits::default()
        });
        let extraction = container_inventory_extraction(
            Path::new("directories.zip"),
            "directories.zip",
            &outer,
            0,
            &mut budget,
        )
        .expect("bounded nested directory inventory");

        let nested_root = extraction
            .nodes
            .iter()
            .find(|node| node.source_file == "directories.zip!/nested/archive.zip")
            .expect("nested archive root");
        assert_eq!(
            nested_root
                .extra
                .get("recursive_dispatch_status")
                .and_then(serde_json::Value::as_str),
            Some("aggregate_encountered_member_limit")
        );
        assert_eq!(
            nested_root
                .extra
                .get("omitted_member_count")
                .and_then(serde_json::Value::as_u64),
            Some(1)
        );
        let labels = extraction
            .nodes
            .iter()
            .map(|node| node.label.as_str())
            .collect::<Vec<_>>();
        assert!(labels.contains(&"c"), "retained labels: {labels:?}");
        assert!(!labels.contains(&"d"));
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
                crate::format_registry::ByteAdapterKind::Office => b"PK\x03\x04".as_slice(),
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
                        | ByteAdapterKind::Pdf
                        | ByteAdapterKind::Office
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
            FormatCapability::InventoryOnly => {}
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
                crate::format_registry::FormatCapability::SemanticFull => {
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
