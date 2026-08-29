//! Explicit, consent-gated enrichment for offline graph inventory.
//!
//! This module intentionally owns the complete outbound boundary. Default
//! indexing never calls it and no provider configuration is inferred.

use anyhow::{bail, Context as _, Result};
use clap::Args;
use graphoxide_core::{CappedGraphRead, KnowledgeGraph, Node};
use graphoxide_extract::{
    detect::is_sensitive_path_only,
    format_registry::{format_registry, FileType},
};
use graphoxide_graph::{is_media_inventory_node, MediaTranscriptSummaryRecord};
use graphoxide_index_runtime::RuntimeCancellation;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    future::Future,
    io::Read,
    net::IpAddr,
    path::{Component, Path, PathBuf},
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

const PROFILE: &str = "media-transcript-summary-v1";
const PROVIDER: &str = "openai-compatible";
const CONSENT: &str = "send-redacted-transcript-text";
const DEFAULT_REQUESTS_PER_MINUTE: u32 = 60;
const DEFAULT_TIMEOUT_SECONDS: u64 = 30;
const MAX_REQUESTS_PER_MINUTE: u32 = 600;
const MAX_TIMEOUT_SECONDS: u64 = 300;
const MAX_API_KEY_BYTES: usize = 8 * 1024;
const MIN_API_KEY_BYTES: usize = 12;
const MAX_ENV_NAME_BYTES: usize = 128;
const MAX_CANDIDATES: usize = 32;
const MAX_TRANSCRIPT_BYTES: usize = 64 * 1024;
const MAX_AGGREGATE_TRANSCRIPT_BYTES: usize = 1024 * 1024;
const MAX_CACHE_BYTES: usize = 32 * 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_RETRY_AFTER_SECONDS: u64 = 30;
const MAX_RUN_SECONDS: u64 = 15 * 60;
const CACHE_SCHEMA_VERSION: u32 = 1;
const CACHE_DIRECTORY: &str = ".graphoxide/enrichment-cache/v1";
const INPUT_DIRECTORY: &str = ".graphoxide/enrichment-input";
const SYSTEM_PROMPT: &str = "Summarize the supplied redacted media transcript. Return exactly one JSON object with string field \"summary\" and array-of-string field \"topics\"; include no other fields.";

#[derive(Debug, Clone, Args)]
pub struct EnrichArgs {
    /// Project root containing graphoxide-out/graph.json.
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// List supported enrichment profiles without making provider requests.
    #[arg(long)]
    pub list_profiles: bool,
    /// Explicit enrichment profile.
    #[arg(long)]
    pub profile: Option<String>,
    /// Explicit provider protocol.
    #[arg(long)]
    pub provider: Option<String>,
    /// Explicit provider endpoint.
    #[arg(long)]
    pub endpoint: Option<String>,
    /// Explicit provider model name.
    #[arg(long)]
    pub model: Option<String>,
    /// Environment variable containing the provider credential.
    #[arg(long)]
    pub api_key_env: Option<String>,
    /// Exact acknowledgement of the data boundary.
    #[arg(long)]
    pub consent: Option<String>,
    /// Maximum provider requests per minute.
    #[arg(long, default_value_t = DEFAULT_REQUESTS_PER_MINUTE)]
    pub requests_per_minute: u32,
    /// Per-request timeout in seconds.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECONDS)]
    pub timeout_seconds: u64,
    /// Emit a single JSON report.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Serialize)]
struct RunReport {
    schema: &'static str,
    profile: &'static str,
    provider: &'static str,
    model: String,
    data_boundary: &'static str,
    redaction_version: &'static str,
    candidates: usize,
    cache_hits: usize,
    requests: usize,
    enrichments_written: usize,
}

#[derive(Clone)]
struct ValidatedArgs {
    root: PathBuf,
    endpoint: reqwest::Url,
    endpoint_sha256: String,
    model: String,
    api_key: String,
    requests_per_minute: u32,
    timeout_seconds: u64,
    json: bool,
}

struct Preflight {
    baseline: CappedGraphRead,
    graph_path: PathBuf,
    output_directory: PathBuf,
    candidates: Vec<Candidate>,
}

struct Candidate {
    source_node_id: String,
    source_file: String,
    redacted_transcript: String,
    redacted_input_sha256: String,
    input_redaction_count: u64,
    cache_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheRecord {
    schema_version: u32,
    source_file: String,
    profile: String,
    provider: String,
    model: String,
    redaction_version: String,
    data_boundary: String,
    redacted_input_sha256: String,
    endpoint_sha256: String,
    output_redaction_count: u64,
    summary: String,
    topics: Vec<String>,
    mac: String,
}

struct Redactor {
    secrets: Vec<String>,
}

struct ExecutionResult {
    records: Vec<MediaTranscriptSummaryRecord>,
    staged_cache: Vec<(PathBuf, CacheRecord)>,
    cache_hits: usize,
    requests: usize,
}

struct ProviderOutput {
    summary: String,
    topics: Vec<String>,
    output_redaction_count: u64,
}

#[derive(Deserialize)]
struct ProviderEnvelope {
    choices: Vec<ProviderChoice>,
}

#[derive(Deserialize)]
struct ProviderChoice {
    message: ProviderMessage,
}

#[derive(Deserialize)]
struct ProviderMessage {
    content: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SummaryPayload {
    summary: String,
    topics: Vec<String>,
}

pub fn run(args: EnrichArgs) -> Result<String> {
    if args.list_profiles {
        validate_list_mode(&args)?;
        return if args.json {
            Ok(serde_json::to_string(&profile_report())?)
        } else {
            Ok(format!(
                "{PROFILE}: redacted transcript text only ({PROVIDER}; consent: {CONSENT})"
            ))
        };
    }
    let cancellation = Arc::new(RuntimeCancellation::new());
    let signal_cancellation = Arc::clone(&cancellation);
    ctrlc::set_handler(move || signal_cancellation.cancel())
        .context("install enrichment cancellation handler")?;
    run_with_cancellation(args, &cancellation)
}

pub fn run_with_cancellation(
    args: EnrichArgs,
    cancellation: &RuntimeCancellation,
) -> Result<String> {
    let validated = validate_run_args(args)?;
    let deadline = Instant::now() + Duration::from_secs(MAX_RUN_SECONDS);
    if cancellation.is_cancelled() {
        bail!("enrichment cancelled");
    }
    let redactor = Redactor::from_environment(&validated.api_key)?;
    validate_configuration_redaction_boundary(&validated, &redactor)?;
    let preflight = preflight(&validated, &redactor, cancellation, deadline)?;
    let candidate_count = preflight.candidates.len();
    let execution = if candidate_count == 0 {
        ExecutionResult {
            records: Vec::new(),
            staged_cache: Vec::new(),
            cache_hits: 0,
            requests: 0,
        }
    } else {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("create isolated enrichment runtime")?;
        runtime.block_on(execute_candidates(
            &validated,
            &preflight,
            &redactor,
            cancellation,
            deadline,
        ))?
    };
    ensure_not_cancelled(cancellation)?;
    if !execution.records.is_empty() {
        commit_graph(&preflight, &execution.records, cancellation, deadline)?;
        for (path, cache) in &execution.staged_cache {
            // Cache durability is deliberately secondary to the committed
            // graph. A post-commit cache failure causes a safe future miss.
            let _ = publish_cache_best_effort(&validated.root, path, cache);
        }
    }
    let report = RunReport {
        schema: "graphoxide.enrichment-run.v1",
        profile: PROFILE,
        provider: PROVIDER,
        model: validated.model.clone(),
        data_boundary: graphoxide_graph::ENRICHMENT_DATA_BOUNDARY,
        redaction_version: graphoxide_graph::REDACTION_VERSION,
        candidates: candidate_count,
        cache_hits: execution.cache_hits,
        requests: execution.requests,
        enrichments_written: execution.records.len(),
    };
    if validated.json {
        Ok(serde_json::to_string(&report)?)
    } else {
        Ok(format!(
            "Enriched {} of {} transcript candidate(s) with {} provider request(s); {} cache hit(s).",
            report.enrichments_written, report.candidates, report.requests, report.cache_hits
        ))
    }
}

fn profile_report() -> Value {
    json!({
        "schema": "graphoxide.enrichment-profiles.v1",
        "profiles": [{
            "name": PROFILE,
            "provider": PROVIDER,
            "consent": CONSENT,
            "data_boundary": "redacted_transcript_text_only",
            "description": "Summarize an explicit transcript sidecar; only redacted transcript text crosses the provider boundary.",
            "input": ".graphoxide/enrichment-input/<media-path>.transcript.txt"
        }]
    })
}

fn validate_list_mode(args: &EnrichArgs) -> Result<()> {
    if args.profile.is_some()
        || args.provider.is_some()
        || args.endpoint.is_some()
        || args.model.is_some()
        || args.api_key_env.is_some()
        || args.consent.is_some()
    {
        bail!("--list-profiles cannot be combined with provider configuration");
    }
    Ok(())
}

fn validate_run_args(args: EnrichArgs) -> Result<ValidatedArgs> {
    let profile = required(args.profile, "--profile")?;
    let provider = required(args.provider, "--provider")?;
    let endpoint = required(args.endpoint, "--endpoint")?;
    let model = required(args.model, "--model")?;
    let api_key_env = required(args.api_key_env, "--api-key-env")?;
    let consent = required(args.consent, "--consent")?;
    if profile != PROFILE {
        bail!("unsupported enrichment profile");
    }
    if provider != PROVIDER {
        bail!("unsupported enrichment provider");
    }
    if consent != CONSENT {
        bail!("consent must exactly acknowledge send-redacted-transcript-text");
    }
    if !(1..=MAX_REQUESTS_PER_MINUTE).contains(&args.requests_per_minute) {
        bail!("--requests-per-minute must be between 1 and {MAX_REQUESTS_PER_MINUTE}");
    }
    if !(1..=MAX_TIMEOUT_SECONDS).contains(&args.timeout_seconds) {
        bail!("--timeout-seconds must be between 1 and {MAX_TIMEOUT_SECONDS}");
    }
    validate_model(&model)?;
    validate_env_name(&api_key_env)?;
    let api_key = std::env::var_os(&api_key_env)
        .with_context(|| format!("credential environment variable {api_key_env} is not set"))?
        .into_string()
        .map_err(|_| anyhow::anyhow!("credential environment variable is not valid UTF-8"))?;
    if !(MIN_API_KEY_BYTES..=MAX_API_KEY_BYTES).contains(&api_key.len())
        || api_key
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        || api_key.trim() != api_key
    {
        bail!("credential environment variable contains an invalid value");
    }
    let endpoint = validate_endpoint(&endpoint)?;
    let endpoint_sha256 = domain_sha256(b"graphoxide-enrichment-endpoint-v1\0", endpoint.as_str());
    let lexical_root = if args.path.is_absolute() {
        args.path.clone()
    } else {
        std::env::current_dir()?.join(&args.path)
    };
    let lexical_metadata =
        fs::symlink_metadata(&lexical_root).context("inspect project root argument")?;
    if !lexical_metadata.file_type().is_dir() || metadata_is_reparse_point(&lexical_metadata) {
        bail!("project root argument is not a safe directory");
    }
    let root = fs::canonicalize(&lexical_root).context("resolve project root argument")?;
    let root_metadata = fs::symlink_metadata(&root)?;
    if !root_metadata.file_type().is_dir() || metadata_is_reparse_point(&root_metadata) {
        bail!("project root is not a safe directory");
    }
    Ok(ValidatedArgs {
        root,
        endpoint,
        endpoint_sha256,
        model,
        api_key,
        requests_per_minute: args.requests_per_minute,
        timeout_seconds: args.timeout_seconds,
        json: args.json,
    })
}

fn required(value: Option<String>, flag: &str) -> Result<String> {
    value
        .filter(|value| !value.is_empty())
        .with_context(|| format!("{flag} is required for enrichment"))
}

fn validate_model(model: &str) -> Result<()> {
    if model.is_empty()
        || model.len() > graphoxide_graph::MAX_ENRICHMENT_MODEL_BYTES
        || model.trim() != model
        || model.chars().any(char::is_control)
    {
        bail!("--model contains an invalid value");
    }
    Ok(())
}

fn validate_env_name(name: &str) -> Result<()> {
    let mut bytes = name.bytes();
    let first = bytes.next();
    if name.len() > MAX_ENV_NAME_BYTES
        || !first.is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        || !bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
    {
        bail!("--api-key-env must be an ASCII environment variable name");
    }
    Ok(())
}

fn validate_endpoint(raw: &str) -> Result<reqwest::Url> {
    let mut endpoint = reqwest::Url::parse(raw).context("--endpoint must be an absolute URL")?;
    let authority = raw
        .split_once("://")
        .map(|(_, remainder)| remainder)
        .unwrap_or_default()
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if endpoint.cannot_be_a_base()
        || endpoint.host().is_none()
        || authority.contains('@')
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        bail!("--endpoint contains forbidden credentials, query, fragment, or path form");
    }
    match endpoint.scheme() {
        "https" => {}
        "http" => {
            let ip = endpoint
                .host_str()
                .map(|host| host.trim_start_matches('[').trim_end_matches(']'))
                .and_then(|host| host.parse::<IpAddr>().ok())
                .filter(IpAddr::is_loopback);
            if ip.is_none() {
                bail!("cleartext provider endpoints require a literal loopback IP address");
            }
        }
        _ => bail!("--endpoint must use HTTPS or literal-loopback HTTP"),
    }
    let normalized_path = endpoint.path().trim_end_matches('/').to_owned();
    endpoint.set_path(if normalized_path.is_empty() {
        "/"
    } else {
        &normalized_path
    });
    Ok(endpoint)
}

fn domain_sha256(domain: &[u8], value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(value.as_bytes());
    hex::encode(digest.finalize())
}

fn sha256_hex(value: &[u8]) -> String {
    hex::encode(Sha256::digest(value))
}

fn validate_configuration_redaction_boundary(
    args: &ValidatedArgs,
    redactor: &Redactor,
) -> Result<()> {
    if path_overlaps_redaction_boundary(&args.root, redactor) {
        bail!("project root overlaps a protected credential value or pattern");
    }
    let (model, model_redactions) = redactor.redact(&args.model);
    if model_redactions != 0 || model != args.model {
        bail!("--model overlaps a protected credential value or pattern");
    }
    let endpoint = args.endpoint.as_str();
    let (redacted_endpoint, endpoint_redactions) = redactor.redact(endpoint);
    if endpoint_redactions != 0 || redacted_endpoint != endpoint {
        bail!("--endpoint overlaps a protected credential value or pattern");
    }
    let decoded_path = percent_decode_path(args.endpoint.path())?;
    let (redacted_path, path_redactions) = redactor.redact(&decoded_path);
    if path_redactions != 0 || redacted_path != decoded_path {
        bail!("--endpoint path overlaps a protected credential value or pattern");
    }
    Ok(())
}

fn path_overlaps_redaction_boundary(path: &Path, redactor: &Redactor) -> bool {
    let path = path.to_string_lossy();
    let (redacted, count) = redactor.redact(&path);
    count != 0 || redacted != path
}

fn percent_decode_path(path: &str) -> Result<String> {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0_usize;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        if index + 2 >= bytes.len() {
            bail!("--endpoint contains malformed percent encoding");
        }
        let high = hex_nibble(bytes[index + 1])
            .context("--endpoint contains malformed percent encoding")?;
        let low = hex_nibble(bytes[index + 2])
            .context("--endpoint contains malformed percent encoding")?;
        decoded.push((high << 4) | low);
        index += 3;
    }
    String::from_utf8(decoded).context("--endpoint path must decode to UTF-8")
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn preflight(
    args: &ValidatedArgs,
    redactor: &Redactor,
    cancellation: &RuntimeCancellation,
    deadline: Instant,
) -> Result<Preflight> {
    ensure_deadline(cancellation, deadline)?;
    let output_directory = crate::watch::output_directory_from_env(&args.root)
        .unwrap_or_else(|| args.root.join(crate::watch::OUTPUT_DIRECTORY));
    if path_overlaps_redaction_boundary(&output_directory, redactor) {
        bail!("graph output directory overlaps a protected credential value or pattern");
    }
    let output_metadata =
        fs::symlink_metadata(&output_directory).context("inspect graph output directory")?;
    if !output_metadata.file_type().is_dir() || metadata_is_reparse_point(&output_metadata) {
        bail!("refusing unsafe graph output directory");
    }
    let output_directory =
        fs::canonicalize(&output_directory).context("resolve graph output directory")?;
    if path_overlaps_redaction_boundary(&output_directory, redactor) {
        bail!("graph output directory overlaps a protected credential value or pattern");
    }
    let graph_path = output_directory.join("graph.json");
    validate_regular_path(&output_directory, &graph_path, false)?;
    let baseline =
        graphoxide_core::read_graph_capped(&graph_path, graphoxide_core::max_graph_bytes())
            .context("read baseline graph for enrichment")?;
    validate_unique_graph_ids(&baseline.graph)?;

    let cache_directory = args.root.join(CACHE_DIRECTORY);
    let mut candidates = Vec::new();
    let mut seen_sources = BTreeSet::new();
    let mut aggregate_bytes = 0_usize;
    let mut eligible = baseline
        .graph
        .nodes
        .iter()
        .filter(|node| eligible_media_node(node))
        .collect::<Vec<_>>();
    eligible.sort_by(|left, right| {
        (left.source_file.as_str(), left.id.as_str())
            .cmp(&(right.source_file.as_str(), right.id.as_str()))
    });

    for node in eligible {
        ensure_deadline(cancellation, deadline)?;
        validate_source_path(&node.source_file)?;
        for value in [&node.source_file, &node.id] {
            let (redacted, count) = redactor.redact(value);
            if count != 0 || redacted != *value {
                bail!("eligible media identity overlaps a protected credential value or pattern");
            }
        }
        if !seen_sources.insert(node.source_file.clone()) {
            bail!("graph contains duplicate eligible media source");
        }
        let source_relative = Path::new(&node.source_file);
        if is_sensitive_path_only(source_relative) {
            bail!("eligible media source is in a sensitive path");
        }
        let media_path = args.root.join(source_relative);
        validate_media_path(&args.root, &media_path)?;

        let sidecar_relative = Path::new(INPUT_DIRECTORY)
            .join(source_relative)
            .with_extension(format!(
                "{}transcript.txt",
                source_relative
                    .extension()
                    .and_then(OsStr::to_str)
                    .map(|extension| format!("{extension}."))
                    .unwrap_or_default()
            ));
        let sidecar_path = args.root.join(&sidecar_relative);
        let Some(raw_transcript) =
            read_bounded_regular_nofollow(&args.root, &sidecar_path, MAX_TRANSCRIPT_BYTES, true)?
        else {
            continue;
        };
        if candidates.len() == MAX_CANDIDATES {
            bail!("enrichment candidate count exceeds {MAX_CANDIDATES}");
        }
        aggregate_bytes = aggregate_bytes
            .checked_add(raw_transcript.len())
            .context("aggregate transcript byte accounting overflow")?;
        if aggregate_bytes > MAX_AGGREGATE_TRANSCRIPT_BYTES {
            bail!("aggregate transcript input exceeds {MAX_AGGREGATE_TRANSCRIPT_BYTES} bytes");
        }
        let transcript = String::from_utf8(raw_transcript)
            .context("transcript sidecar must contain valid UTF-8")?;
        if transcript
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
        {
            bail!("transcript sidecar contains forbidden control characters");
        }
        let transcript = normalize_newlines(&transcript);
        let (redacted_transcript, input_redaction_count) = redactor.redact(&transcript);
        if redacted_transcript.trim().is_empty() {
            bail!("transcript sidecar is empty after normalization and redaction");
        }
        if redacted_transcript.len() > MAX_TRANSCRIPT_BYTES {
            bail!("redacted transcript exceeds the per-input byte cap");
        }
        let aggregate_redacted = candidates
            .iter()
            .try_fold(0_usize, |total, candidate: &Candidate| {
                total.checked_add(candidate.redacted_transcript.len())
            })
            .and_then(|total| total.checked_add(redacted_transcript.len()))
            .context("redacted transcript byte accounting overflow")?;
        if aggregate_redacted > MAX_AGGREGATE_TRANSCRIPT_BYTES {
            bail!("aggregate redacted transcript input exceeds the run byte cap");
        }
        let redacted_input_sha256 = sha256_hex(redacted_transcript.as_bytes());
        let cache_path = cache_directory.join(cache_file_name(&node.source_file));
        candidates.push(Candidate {
            source_node_id: node.id.clone(),
            source_file: node.source_file.clone(),
            redacted_transcript,
            redacted_input_sha256,
            input_redaction_count,
            cache_path,
        });
    }

    if !candidates.is_empty() {
        validate_cache_namespace_preflight(&args.root)?;
        for candidate in &candidates {
            validate_cache_entry_preflight(&args.root, &candidate.cache_path)?;
        }
    }

    Ok(Preflight {
        baseline,
        graph_path,
        output_directory,
        candidates,
    })
}

fn eligible_media_node(node: &Node) -> bool {
    is_media_inventory_node(node)
        && format_registry()
            .find_by_path(Path::new(&node.source_file))
            .and_then(|spec| spec.legacy_file_type)
            == Some(FileType::Video)
}

fn validate_unique_graph_ids(graph: &KnowledgeGraph) -> Result<()> {
    let mut ids = HashSet::with_capacity(graph.nodes.len());
    for node in &graph.nodes {
        if node.id.is_empty() || !ids.insert(node.id.as_str()) {
            bail!("graph contains an empty or duplicate node ID");
        }
    }
    Ok(())
}

fn validate_source_path(source: &str) -> Result<()> {
    if source.is_empty()
        || source.len() > graphoxide_graph::MAX_ENRICHMENT_SOURCE_BYTES
        || source.contains(['\\', '\0', ':'])
        || source.contains("!/")
    {
        bail!("eligible media source has an unsafe path");
    }
    if !source
        .split('/')
        .all(|component| !component.is_empty() && component != "." && component != "..")
    {
        bail!("eligible media source contains an alias path spelling");
    }
    let path = Path::new(source);
    if path.is_absolute()
        || !path.components().all(|component| {
            matches!(component, Component::Normal(value) if value != "." && value != "..")
        })
    {
        bail!("eligible media source must be a normalized project-relative path");
    }
    Ok(())
}

fn cache_file_name(source: &str) -> String {
    let digest = domain_sha256(
        b"graphoxide-enrichment-cache-path-v1\0",
        &format!("{source}\0{PROFILE}"),
    );
    format!("{digest}.json")
}

fn ensure_cache_directory(root: &Path) -> Result<PathBuf> {
    let relative = Path::new(CACHE_DIRECTORY);
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            bail!("internal cache path is not normalized");
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir() || metadata_is_reparse_point(&metadata) {
                    bail!("refusing unsafe enrichment cache directory");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).context("create enrichment cache directory")?;
                let metadata = fs::symlink_metadata(&current)?;
                if !metadata.file_type().is_dir() || metadata_is_reparse_point(&metadata) {
                    bail!("refusing unsafe enrichment cache directory");
                }
            }
            Err(error) => return Err(error).context("inspect enrichment cache directory"),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&current, fs::Permissions::from_mode(0o700))?;
    }
    Ok(current)
}

fn validate_cache_namespace_preflight(root: &Path) -> Result<()> {
    let mut current = root.to_path_buf();
    let mut missing = false;
    for component in Path::new(CACHE_DIRECTORY).components() {
        let Component::Normal(name) = component else {
            bail!("internal cache path is not normalized");
        };
        current.push(name);
        if missing {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir() || metadata_is_reparse_point(&metadata) {
                    bail!("refusing unsafe enrichment cache namespace");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing = true,
            Err(error) => return Err(error).context("inspect enrichment cache namespace"),
        }
    }
    Ok(())
}

fn validate_cache_entry_preflight(root: &Path, path: &Path) -> Result<()> {
    validate_parent_chain(root, path)?;
    match fs::symlink_metadata(path) {
        // Unsafe final components are inert cache misses and may be replaced
        // only after the graph commits. Parent/root links remain fatal.
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect enrichment cache entry"),
    }
    Ok(())
}

#[cfg(not(windows))]
fn validate_media_path(root: &Path, path: &Path) -> Result<()> {
    validate_regular_path(root, path, false)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata_link_count_path(&metadata) != 1 {
        bail!("refusing multiply linked media inventory path");
    }
    Ok(())
}

#[cfg(windows)]
fn validate_media_path(root: &Path, path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        },
    };

    validate_regular_path(root, path, false)?;
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    // SAFETY: `wide` is NUL-terminated and remains live for the call. The
    // returned handle is closed on every path below.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error()).context("open media metadata handle");
    }
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `handle` is valid and `information` is initialized writable
    // storage. Closing does not invalidate the copied information structure.
    let succeeded = unsafe { GetFileInformationByHandle(handle, &mut information) };
    let close_result = unsafe { CloseHandle(handle) };
    if succeeded == 0 {
        return Err(std::io::Error::last_os_error()).context("inspect media metadata handle");
    }
    if close_result == 0 {
        return Err(std::io::Error::last_os_error()).context("close media metadata handle");
    }
    if information.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) != 0
        || information.nNumberOfLinks != 1
    {
        bail!("refusing unsafe, linked, or non-regular media inventory path");
    }
    Ok(())
}

fn validate_regular_path(root: &Path, path: &Path, allow_missing: bool) -> Result<bool> {
    validate_parent_chain(root, path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || metadata_is_reparse_point(&metadata) {
                bail!("refusing unsafe non-regular path");
            }
            Ok(true)
        }
        Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("inspect required regular file"),
    }
}

fn validate_parent_chain(root: &Path, path: &Path) -> Result<()> {
    let relative = path
        .strip_prefix(root)
        .context("managed path escapes the project root")?;
    let mut current = root.to_path_buf();
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    for component in parent.components() {
        let Component::Normal(name) = component else {
            bail!("managed path is not normalized");
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir() || metadata_is_reparse_point(&metadata) {
                    bail!("managed path has an unsafe parent directory");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("inspect managed path parent"),
        }
    }
    Ok(())
}

fn read_bounded_regular_nofollow(
    root: &Path,
    path: &Path,
    cap: usize,
    allow_missing: bool,
) -> Result<Option<Vec<u8>>> {
    validate_parent_chain(root, path)?;
    let initial_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || metadata_is_reparse_point(&metadata) {
                bail!("refusing unsafe non-regular enrichment input");
            }
            if metadata.len() > cap as u64 {
                bail!("enrichment input exceeds its byte cap");
            }
            if metadata_link_count_path(&metadata) != 1 {
                bail!("refusing multiply linked enrichment input");
            }
            metadata
        }
        Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None)
        }
        Err(error) => return Err(error).context("inspect enrichment input"),
    };
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options.open(path).context("open enrichment input")?;
    validate_open_regular(&file, cap)?;
    if !same_metadata_identity(&initial_metadata, &file.metadata()?) {
        bail!("enrichment input changed during admission");
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take((cap as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .context("read enrichment input")?;
    if bytes.len() > cap {
        bail!("enrichment input exceeds its byte cap");
    }
    validate_open_regular(&file, cap)?;
    validate_parent_chain(root, path)?;
    let final_metadata = fs::symlink_metadata(path).context("revalidate enrichment input")?;
    if metadata_is_reparse_point(&final_metadata) {
        bail!("enrichment input changed during bounded read");
    }
    let final_file = options
        .open(path)
        .context("reopen enrichment input for identity check")?;
    validate_open_regular(&final_file, cap)?;
    if !same_open_file_identity(&file, &final_file)? {
        bail!("enrichment input changed during bounded read");
    }
    Ok(Some(bytes))
}

/// Read a regular file beneath `root` through the enrichment admission path.
///
/// This keeps callers on the same bounded, no-follow, identity-rechecked path
/// used for enrichment inputs.
pub fn safe_read_bounded(root: &Path, path: &Path, cap: usize) -> Result<Vec<u8>> {
    read_bounded_regular_nofollow(root, path, cap, false)?
        .context("required safe input disappeared")
}

#[cfg(unix)]
fn same_metadata_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_metadata_identity(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    // On Windows the robust check is performed between two open handles below.
    true
}

#[cfg(unix)]
fn same_open_file_identity(left: &File, right: &File) -> std::io::Result<bool> {
    Ok(same_metadata_identity(
        &left.metadata()?,
        &right.metadata()?,
    ))
}

#[cfg(windows)]
fn same_open_file_identity(left: &File, right: &File) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    fn information(file: &File) -> std::io::Result<BY_HANDLE_FILE_INFORMATION> {
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: the borrowed handle stays valid for the call and the output
        // pointer refers to initialized writable storage.
        let succeeded =
            unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut information) };
        if succeeded == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(information)
        }
    }
    let left = information(left)?;
    let right = information(right)?;
    Ok(left.dwVolumeSerialNumber == right.dwVolumeSerialNumber
        && left.nFileIndexHigh == right.nFileIndexHigh
        && left.nFileIndexLow == right.nFileIndexLow)
}

#[cfg(not(any(unix, windows)))]
fn same_open_file_identity(_left: &File, _right: &File) -> std::io::Result<bool> {
    Ok(false)
}

fn validate_open_regular(file: &File, cap: usize) -> Result<()> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata_is_reparse_point(&metadata)
        || metadata.len() > cap as u64
        || open_file_link_count(file)? != 1
    {
        bail!("refusing unsafe or oversized enrichment input");
    }
    Ok(())
}

#[cfg(unix)]
fn metadata_link_count_path(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    metadata.nlink()
}

#[cfg(not(unix))]
fn metadata_link_count_path(_metadata: &fs::Metadata) -> u64 {
    1
}

#[cfg(unix)]
fn open_file_link_count(file: &File) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt as _;
    Ok(file.metadata()?.nlink())
}

#[cfg(windows)]
fn open_file_link_count(file: &File) -> std::io::Result<u64> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the borrowed handle stays valid for the duration of this call.
    let succeeded =
        unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut information) };
    if succeeded == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(u64::from(information.nNumberOfLinks))
    }
}

#[cfg(not(any(unix, windows)))]
fn open_file_link_count(_file: &File) -> std::io::Result<u64> {
    Ok(1)
}

fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(windows))]
    false
}

fn ensure_not_cancelled(cancellation: &RuntimeCancellation) -> Result<()> {
    if cancellation.is_cancelled() {
        bail!("enrichment cancelled");
    }
    Ok(())
}

impl Redactor {
    fn from_environment(api_key: &str) -> Result<Self> {
        Self::from_environment_and_secret(Some(api_key))
    }

    fn from_environment_and_secret(extra_secret: Option<&str>) -> Result<Self> {
        const MAX_ENV_SECRETS: usize = 128;
        const MAX_ENV_SECRET_BYTES: usize = 64 * 1024;
        let mut environment_secrets = BTreeMap::new();
        let mut retained_bytes = 0_usize;
        for (name, value) in std::env::vars_os() {
            let (Some(name), Some(value)) = (name.to_str(), value.to_str()) else {
                continue;
            };
            let upper = name.to_ascii_uppercase();
            if ![
                "SECRET",
                "TOKEN",
                "PASSWORD",
                "PASSWD",
                "API_KEY",
                "CREDENTIAL",
                "AUTH",
            ]
            .iter()
            .any(|marker| upper.contains(marker))
            {
                continue;
            }
            if value.is_empty() || value == "[REDACTED]" {
                continue;
            }
            if value.chars().any(char::is_control) {
                bail!("a secret-like environment value contains forbidden control characters");
            }
            if !(12..=MAX_API_KEY_BYTES).contains(&value.len()) {
                bail!("a secret-like environment value is outside the safe redaction bounds");
            }
            let value_len = value.len();
            if environment_secrets.contains_key(name) {
                continue;
            }
            let next_bytes = retained_bytes
                .checked_add(value_len)
                .context("secret-like environment byte accounting overflow")?;
            if environment_secrets.len() == MAX_ENV_SECRETS || next_bytes > MAX_ENV_SECRET_BYTES {
                bail!("too many secret-like environment values to redact safely");
            }
            environment_secrets.insert(name.to_owned(), value.to_owned());
            retained_bytes = next_bytes;
        }
        let mut secrets = extra_secret
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        for (_, value) in environment_secrets {
            secrets.push(value);
        }
        secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
        secrets.dedup();
        Ok(Self { secrets })
    }

    fn redact(&self, input: &str) -> (String, u64) {
        static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?i)\b(?:gh[pousr]_|sk-)[a-z0-9_-]{16,}\b")
                .expect("static token redaction regex")
        });
        static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"(?i)[\"']?[a-z0-9_.-]*(?:authorization|credential|secret|token|password|passwd|api[_-]?key|access[_-]?key)[a-z0-9_.-]*[\"']?\s*[:=]\s*(?:\"[^\"]*\"|'[^']*'|(?:bearer\s+)?[^\s,;}]+)"#)
                .expect("static assignment redaction regex")
        });
        static BEARER: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"(?i)\bbearer\s+[^\s,;}"']+"#).expect("static bearer redaction regex")
        });
        static JWT: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"\beyJ[a-zA-Z0-9_-]{5,}\.[a-zA-Z0-9_-]{5,}\.[a-zA-Z0-9_-]{5,}\b")
                .expect("static JWT redaction regex")
        });
        static PRIVATE_KEY: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?s)-----BEGIN [A-Z0-9 ]{0,32}PRIVATE KEY-----.*?-----END [A-Z0-9 ]{0,32}PRIVATE KEY-----")
                .expect("static private-key redaction regex")
        });
        let mut output = input.to_owned();
        let mut count = 0_u64;
        for secret in &self.secrets {
            let matches = output.matches(secret).count() as u64;
            if matches > 0 {
                output = output.replace(secret, "[REDACTED]");
                count = count.saturating_add(matches);
            }
        }
        let token_matches = TOKEN.find_iter(&output).count() as u64;
        if token_matches > 0 {
            output = TOKEN.replace_all(&output, "[REDACTED]").into_owned();
            count = count.saturating_add(token_matches);
        }
        let bearer_matches = BEARER.find_iter(&output).count() as u64;
        if bearer_matches > 0 {
            output = BEARER.replace_all(&output, "[REDACTED]").into_owned();
            count = count.saturating_add(bearer_matches);
        }
        let jwt_matches = JWT.find_iter(&output).count() as u64;
        if jwt_matches > 0 {
            output = JWT.replace_all(&output, "[REDACTED]").into_owned();
            count = count.saturating_add(jwt_matches);
        }
        let private_key_matches = PRIVATE_KEY.find_iter(&output).count() as u64;
        if private_key_matches > 0 {
            output = PRIVATE_KEY.replace_all(&output, "[REDACTED]").into_owned();
            count = count.saturating_add(private_key_matches);
        }
        let assignment_matches = ASSIGNMENT.find_iter(&output).count() as u64;
        if assignment_matches > 0 {
            output = ASSIGNMENT.replace_all(&output, "[REDACTED]").into_owned();
            count = count.saturating_add(assignment_matches);
        }
        (output, count)
    }
}

/// Apply the enrichment redaction policy without adding a provider credential.
pub fn redact_local_text(input: &str) -> Result<(String, u64)> {
    Ok(Redactor::from_environment_and_secret(None)?.redact(input))
}

fn normalize_newlines(input: &str) -> String {
    input.replace("\r\n", "\n").replace('\r', "\n")
}

async fn execute_candidates(
    args: &ValidatedArgs,
    preflight: &Preflight,
    redactor: &Redactor,
    cancellation: &RuntimeCancellation,
    deadline: Instant,
) -> Result<ExecutionResult> {
    let mac_key = blake3::derive_key(
        "graphoxide explicit enrichment cache authentication v1",
        args.api_key.as_bytes(),
    );

    // Probe every cache entry before starting the first provider request. A
    // parent swap or unsafe cache root therefore cannot create a partial run.
    let mut cached = Vec::with_capacity(preflight.candidates.len());
    for candidate in &preflight.candidates {
        ensure_not_cancelled(cancellation)?;
        cached.push(read_cache(candidate, args, redactor, &mac_key)?);
    }

    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(args.timeout_seconds))
        .timeout(Duration::from_secs(args.timeout_seconds))
        .pool_max_idle_per_host(0)
        .build()
        .context("construct isolated enrichment provider client")?;
    let provider_url = provider_request_url(&args.endpoint)?;
    let interval = Duration::from_secs_f64(60.0 / f64::from(args.requests_per_minute));
    let mut last_request = None;
    let mut requests = 0_usize;
    let mut cache_hits = 0_usize;
    let mut records = Vec::with_capacity(preflight.candidates.len());
    let mut staged_cache = Vec::new();

    for (candidate, cache) in preflight.candidates.iter().zip(cached) {
        ensure_deadline(cancellation, deadline)?;
        let output = if let Some(cache) = cache {
            cache_hits += 1;
            ProviderOutput {
                summary: cache.summary,
                topics: cache.topics,
                output_redaction_count: cache.output_redaction_count,
            }
        } else {
            let output = request_provider(
                &client,
                &provider_url,
                &args.api_key,
                &args.model,
                &candidate.redacted_transcript,
                redactor,
                interval,
                &mut last_request,
                &mut requests,
                cancellation,
                deadline,
            )
            .await?;
            let mut cache = CacheRecord {
                schema_version: CACHE_SCHEMA_VERSION,
                source_file: candidate.source_file.clone(),
                profile: PROFILE.into(),
                provider: PROVIDER.into(),
                model: args.model.clone(),
                redaction_version: graphoxide_graph::REDACTION_VERSION.into(),
                data_boundary: graphoxide_graph::ENRICHMENT_DATA_BOUNDARY.into(),
                redacted_input_sha256: candidate.redacted_input_sha256.clone(),
                endpoint_sha256: args.endpoint_sha256.clone(),
                output_redaction_count: output.output_redaction_count,
                summary: output.summary.clone(),
                topics: output.topics.clone(),
                mac: String::new(),
            };
            cache.mac = cache_mac(&cache, &mac_key)?;
            staged_cache.push((candidate.cache_path.clone(), cache));
            output
        };
        let redaction_count = candidate
            .input_redaction_count
            .checked_add(output.output_redaction_count)
            .context("redaction count overflow")?;
        records.push(MediaTranscriptSummaryRecord {
            source_node_id: candidate.source_node_id.clone(),
            source_file: candidate.source_file.clone(),
            profile: PROFILE.into(),
            provider: PROVIDER.into(),
            model: args.model.clone(),
            redacted_input_sha256: candidate.redacted_input_sha256.clone(),
            redaction_count,
            summary: output.summary,
            topics: output.topics,
        });
    }

    Ok(ExecutionResult {
        records,
        staged_cache,
        cache_hits,
        requests,
    })
}

fn provider_request_url(endpoint: &reqwest::Url) -> Result<reqwest::Url> {
    let mut url = endpoint.clone();
    let base = endpoint.path().trim_end_matches('/');
    url.set_path(&format!("{base}/chat/completions"));
    if url.host().is_none() {
        bail!("provider endpoint has no host");
    }
    Ok(url)
}

#[allow(clippy::too_many_arguments)]
async fn request_provider(
    client: &reqwest::Client,
    url: &reqwest::Url,
    api_key: &str,
    model: &str,
    transcript: &str,
    redactor: &Redactor,
    interval: Duration,
    last_request: &mut Option<Instant>,
    requests: &mut usize,
    cancellation: &RuntimeCancellation,
    deadline: Instant,
) -> Result<ProviderOutput> {
    let body = json!({
        "model": model,
        "max_tokens": 512,
        "temperature": 0,
        "response_format": {"type": "json_object"},
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": transcript}
        ]
    });
    let mut retry_after = Duration::ZERO;
    for attempt in 0..2 {
        pace_request(
            *last_request,
            interval.max(retry_after),
            cancellation,
            deadline,
        )
        .await?;
        ensure_deadline(cancellation, deadline)?;
        *last_request = Some(Instant::now());
        *requests = requests.checked_add(1).context("request count overflow")?;
        let send = client
            .post(url.clone())
            .bearer_auth(api_key)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .json(&body)
            .send();
        let response = await_cancellable(send, cancellation, deadline)
            .await?
            .map_err(|_| anyhow::anyhow!("enrichment provider request failed"))?;
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            if attempt == 1 {
                bail!("enrichment provider rate-limited the retry");
            }
            retry_after = parse_retry_after(response.headers())?;
            continue;
        }
        if !response.status().is_success() {
            bail!("enrichment provider returned a non-success status");
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            bail!("enrichment provider response exceeds the byte cap");
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type
            .split(';')
            .next()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
        {
            bail!("enrichment provider response is not JSON");
        }
        let bytes = read_response_bounded(response, cancellation, deadline).await?;
        return parse_provider_output(&bytes, redactor);
    }
    unreachable!("bounded provider attempts return or fail")
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Result<Duration> {
    let Some(value) = headers.get(reqwest::header::RETRY_AFTER) else {
        return Ok(Duration::ZERO);
    };
    let seconds = value
        .to_str()
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .context("provider Retry-After must be an integer number of seconds")?;
    if seconds > MAX_RETRY_AFTER_SECONDS {
        bail!("provider Retry-After exceeds the 30 second cap");
    }
    Ok(Duration::from_secs(seconds))
}

async fn read_response_bounded(
    mut response: reqwest::Response,
    cancellation: &RuntimeCancellation,
    deadline: Instant,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let chunk = await_cancellable(response.chunk(), cancellation, deadline)
            .await?
            .map_err(|_| anyhow::anyhow!("enrichment provider response read failed"))?;
        let Some(chunk) = chunk else { break };
        let next = bytes
            .len()
            .checked_add(chunk.len())
            .context("provider response byte accounting overflow")?;
        if next > MAX_RESPONSE_BYTES {
            bail!("enrichment provider response exceeds the byte cap");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn parse_provider_output(bytes: &[u8], redactor: &Redactor) -> Result<ProviderOutput> {
    let mut envelope: ProviderEnvelope = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("provider response does not match the strict schema"))?;
    if envelope.choices.len() != 1 {
        bail!("provider response must contain exactly one choice");
    }
    let choice = envelope.choices.pop().expect("length checked");
    let payload: SummaryPayload = serde_json::from_str(&choice.message.content).map_err(|_| {
        anyhow::anyhow!("provider message content does not match the strict summary schema")
    })?;
    let normalized_summary = normalize_newlines(&payload.summary);
    let (summary, mut output_redaction_count) = redactor.redact(&normalized_summary);
    let mut topics = Vec::with_capacity(payload.topics.len());
    for topic in payload.topics {
        let normalized_topic = normalize_newlines(&topic);
        let (topic, count) = redactor.redact(&normalized_topic);
        output_redaction_count = output_redaction_count.saturating_add(count);
        topics.push(topic);
    }
    topics.sort();
    topics.dedup();
    validate_summary_fields(&summary, &topics)?;
    Ok(ProviderOutput {
        summary,
        topics,
        output_redaction_count,
    })
}

fn validate_summary_fields(summary: &str, topics: &[String]) -> Result<()> {
    if summary.is_empty()
        || summary.trim().is_empty()
        || summary.len() > graphoxide_graph::MAX_ENRICHMENT_SUMMARY_BYTES
        || summary
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        bail!("provider summary is empty, oversized, or contains forbidden controls");
    }
    if topics.is_empty() || topics.len() > graphoxide_graph::MAX_ENRICHMENT_TOPICS {
        bail!("provider topics are empty or exceed the item cap");
    }
    for topic in topics {
        if topic.is_empty()
            || topic.trim().is_empty()
            || topic.len() > graphoxide_graph::MAX_ENRICHMENT_TOPIC_BYTES
            || topic.chars().any(char::is_control)
        {
            bail!("provider topic is empty, oversized, or contains forbidden controls");
        }
    }
    Ok(())
}

async fn pace_request(
    last_request: Option<Instant>,
    minimum_delay: Duration,
    cancellation: &RuntimeCancellation,
    deadline: Instant,
) -> Result<()> {
    let Some(last_request) = last_request else {
        return Ok(());
    };
    let target = last_request + minimum_delay;
    while Instant::now() < target {
        ensure_deadline(cancellation, deadline)?;
        let wait = target
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(25));
        tokio::time::sleep(wait).await;
    }
    Ok(())
}

async fn await_cancellable<F>(
    future: F,
    cancellation: &RuntimeCancellation,
    deadline: Instant,
) -> Result<F::Output>
where
    F: Future,
{
    tokio::pin!(future);
    loop {
        ensure_deadline(cancellation, deadline)?;
        tokio::select! {
            output = &mut future => return Ok(output),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    }
}

fn ensure_deadline(cancellation: &RuntimeCancellation, deadline: Instant) -> Result<()> {
    ensure_not_cancelled(cancellation)?;
    if Instant::now() >= deadline {
        bail!("enrichment run exceeded its 15 minute deadline");
    }
    Ok(())
}

fn read_cache(
    candidate: &Candidate,
    args: &ValidatedArgs,
    redactor: &Redactor,
    mac_key: &[u8; 32],
) -> Result<Option<CacheRecord>> {
    validate_parent_chain(&args.root, &candidate.cache_path)?;
    let metadata = match fs::symlink_metadata(&candidate.cache_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect enrichment cache entry"),
    };
    if !metadata.file_type().is_file()
        || metadata_is_reparse_point(&metadata)
        || metadata.len() > MAX_CACHE_BYTES as u64
        || metadata_link_count_path(&metadata) != 1
    {
        return Ok(None);
    }
    let Some(bytes) =
        read_bounded_regular_nofollow(&args.root, &candidate.cache_path, MAX_CACHE_BYTES, true)?
    else {
        return Ok(None);
    };
    let Ok(record) = serde_json::from_slice::<CacheRecord>(&bytes) else {
        return Ok(None);
    };
    if !cache_record_matches(&record, candidate, args, redactor, mac_key)? {
        return Ok(None);
    }
    Ok(Some(record))
}

fn cache_record_matches(
    record: &CacheRecord,
    candidate: &Candidate,
    args: &ValidatedArgs,
    redactor: &Redactor,
    mac_key: &[u8; 32],
) -> Result<bool> {
    if record.schema_version != CACHE_SCHEMA_VERSION
        || record.source_file != candidate.source_file
        || record.profile != PROFILE
        || record.provider != PROVIDER
        || record.model != args.model
        || record.redaction_version != graphoxide_graph::REDACTION_VERSION
        || record.data_boundary != graphoxide_graph::ENRICHMENT_DATA_BOUNDARY
        || record.redacted_input_sha256 != candidate.redacted_input_sha256
        || record.endpoint_sha256 != args.endpoint_sha256
        || record.endpoint_sha256.len() != 64
        || record.redacted_input_sha256.len() != 64
    {
        return Ok(false);
    }
    let expected = cache_mac(record, mac_key)?;
    if !constant_time_hex_eq(&expected, &record.mac) {
        return Ok(false);
    }
    let (summary, summary_redactions) = redactor.redact(&record.summary);
    let mut topics = record.topics.clone();
    let mut newly_redacted = summary_redactions;
    for topic in &mut topics {
        let (redacted, count) = redactor.redact(topic);
        *topic = redacted;
        newly_redacted = newly_redacted.saturating_add(count);
    }
    topics.sort();
    topics.dedup();
    if newly_redacted != 0 || summary != record.summary || topics != record.topics {
        return Ok(false);
    }
    Ok(validate_summary_fields(&record.summary, &record.topics).is_ok())
}

fn cache_mac(record: &CacheRecord, key: &[u8; 32]) -> Result<String> {
    let mut unsigned = record.clone();
    unsigned.mac.clear();
    let payload = serde_json::to_vec(&unsigned).context("serialize authenticated cache payload")?;
    Ok(blake3::keyed_hash(key, &payload).to_hex().to_string())
}

fn constant_time_hex_eq(left: &str, right: &str) -> bool {
    if left.len() != 64 || right.len() != 64 {
        return false;
    }
    left.as_bytes()
        .iter()
        .zip(right.as_bytes())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn commit_graph(
    preflight: &Preflight,
    records: &[MediaTranscriptSummaryRecord],
    cancellation: &RuntimeCancellation,
    deadline: Instant,
) -> Result<()> {
    let _lock = loop {
        ensure_deadline(cancellation, deadline)?;
        if let Some(lock) =
            crate::watch::RebuildLockGuard::acquire(&preflight.output_directory, false)?
        {
            break lock;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    ensure_deadline(cancellation, deadline)?;
    validate_regular_path(&preflight.output_directory, &preflight.graph_path, false)?;
    let current = graphoxide_core::read_graph_capped(
        &preflight.graph_path,
        graphoxide_core::max_graph_bytes(),
    )
    .context("reread graph under rebuild lock")?;
    if current.sha256 != preflight.baseline.sha256 {
        bail!("graph changed during enrichment; refusing to overwrite the concurrent generation");
    }
    let mut graph = current.graph;
    graphoxide_graph::apply_media_transcript_summaries(&mut graph, records)
        .map_err(|_| anyhow::anyhow!("failed to apply validated enrichment facts"))?;
    // The strict atomic replace is the commit point. Check cancellation and
    // the run deadline immediately before it, then let that one operation
    // finish without interruption so the destination is never half-written.
    ensure_deadline(cancellation, deadline)?;
    let wrote = graphoxide_core::write_graph_atomic_strict(&preflight.graph_path, &graph, true)
        .context("publish enriched graph atomically")?;
    if !wrote {
        bail!("strict graph writer declined enrichment publication");
    }
    Ok(())
}

fn publish_cache_best_effort(root: &Path, path: &Path, record: &CacheRecord) -> Result<()> {
    let cache_directory = ensure_cache_directory(root)?;
    if path.parent() != Some(cache_directory.as_path()) {
        bail!("cache publication target escaped its namespace");
    }
    validate_parent_chain(root, path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata_is_reparse_point(&metadata) || !metadata.file_type().is_file() => {
            if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
                bail!("refusing to replace a cache directory entry");
            }
            fs::remove_file(path).context("remove unsafe cache final component")?;
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect cache publication target"),
    }
    graphoxide_core::write_json_atomic_strict(path, record, true)
        .context("publish authenticated enrichment cache")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
