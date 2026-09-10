//! Versioned, secret-free provider profiles for consent-gated wiki model work.

use crate::ollama_transport::{
    normalize_wiki_markdown_body, validate_wiki_markdown_body, OllamaTransport,
    MARKDOWN_COMPLETION_TOKENS, MARKDOWN_RETRY_INSTRUCTION, MARKDOWN_SYSTEM_PROMPT,
    MARKDOWN_USER_PROMPT_BYTES,
};
use anyhow::{bail, Context as _, Result};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use chrono::{SecondsFormat, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    path::Path,
    sync::{Arc, Condvar, Mutex, OnceLock},
    time::Duration,
};

pub const PROFILE_VERSION: u32 = 1;
const MAX_PROFILE_BYTES: usize = 256 * 1024;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_MODEL_TEXT_INPUT_BYTES: usize = 256 * 1024;
const MAX_PROVIDER_ERROR_RESPONSE_BYTES: usize = 4 * 1024;
/// Raw binary inputs are separately bounded from textual prompts. This matches
/// the source-admission limit while keeping one in-flight HTTP request bounded.
const MAX_HTTP_ENRICHMENT_INPUT_BYTES: usize = 128 * 1024 * 1024;
// Source-to-page authoring replies need enough room for evidence-bound
// technical detail, tables, and code excerpts. The 256 KiB response cap and
// per-run output budget remain the resource boundary.
const JSON_COMPLETION_TOKENS: usize = 1_024;
const MAX_CATALOG_MODELS: usize = 512;
const MAX_CATALOG_RESPONSE_BYTES: usize = 1024 * 1024;
/// Binary enrichment requests are base64-encoded and serialized by the
/// transport, so keep only two independent source partitions in flight.
const MAX_CONCURRENT_PROVIDER_REQUESTS: usize = 2;
const PROVIDER_SCHEDULER_CAPACITY: usize = MAX_CONCURRENT_PROVIDER_REQUESTS * 4;
const MAX_PROVIDER_RETRY_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug)]
struct ProviderScheduleState {
    in_flight_units: usize,
    retry_after: Option<std::time::Instant>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ProviderRequestWeight {
    Text = 1,
    Binary = 4,
}

impl ProviderRequestWeight {
    fn units(self) -> usize {
        self as usize
    }
}

#[derive(Debug)]
struct ProviderRequestScheduler {
    cap: usize,
    state: Mutex<ProviderScheduleState>,
    wake: Condvar,
}

#[derive(Debug)]
struct ProviderRequestPermit {
    scheduler: Arc<ProviderRequestScheduler>,
    units: usize,
}

impl ProviderRequestScheduler {
    fn new(cap: usize) -> Arc<Self> {
        debug_assert!(cap > 0, "provider scheduler cap must be nonzero");
        Arc::new(Self {
            cap,
            state: Mutex::new(ProviderScheduleState {
                in_flight_units: 0,
                retry_after: None,
            }),
            wake: Condvar::new(),
        })
    }

    #[cfg(test)]
    fn try_acquire(
        self: &Arc<Self>,
        weight: ProviderRequestWeight,
    ) -> Option<ProviderRequestPermit> {
        let units = weight.units();
        let mut state = self
            .state
            .lock()
            .expect("provider scheduler lock is not poisoned");
        if state.in_flight_units.saturating_add(units) > self.cap
            || state
                .retry_after
                .is_some_and(|retry_after| retry_after > std::time::Instant::now())
        {
            return None;
        }
        state.in_flight_units += units;
        Some(ProviderRequestPermit {
            scheduler: Arc::clone(self),
            units,
        })
    }

    fn acquire(self: &Arc<Self>, weight: ProviderRequestWeight) -> ProviderRequestPermit {
        let units = weight.units();
        let mut state = self
            .state
            .lock()
            .expect("provider scheduler lock is not poisoned");
        loop {
            let now = std::time::Instant::now();
            if state
                .retry_after
                .is_some_and(|retry_after| retry_after <= now)
            {
                state.retry_after = None;
            }
            if state.in_flight_units.saturating_add(units) <= self.cap
                && state.retry_after.is_none()
            {
                state.in_flight_units += units;
                return ProviderRequestPermit {
                    scheduler: Arc::clone(self),
                    units,
                };
            }
            if let Some(retry_after) = state.retry_after {
                let wait = retry_after.saturating_duration_since(now);
                let (next, _) = self
                    .wake
                    .wait_timeout(state, wait)
                    .expect("provider scheduler lock is not poisoned");
                state = next;
            } else {
                state = self
                    .wake
                    .wait(state)
                    .expect("provider scheduler lock is not poisoned");
            }
        }
    }

    fn defer(&self, delay: Duration) {
        let delay = delay.min(MAX_PROVIDER_RETRY_AFTER);
        let candidate = std::time::Instant::now() + delay;
        let mut state = self
            .state
            .lock()
            .expect("provider scheduler lock is not poisoned");
        if state.retry_after.is_none_or(|current| candidate > current) {
            state.retry_after = Some(candidate);
        }
        self.wake.notify_all();
    }
}

impl Drop for ProviderRequestPermit {
    fn drop(&mut self) {
        let mut state = self
            .scheduler
            .state
            .lock()
            .expect("provider scheduler lock is not poisoned");
        debug_assert!(
            state.in_flight_units >= self.units,
            "provider scheduler permit underflow"
        );
        state.in_flight_units = state.in_flight_units.saturating_sub(self.units);
        self.scheduler.wake.notify_all();
    }
}

fn provider_scheduler(provider_key: &str) -> Arc<ProviderRequestScheduler> {
    static SCHEDULERS: OnceLock<Mutex<BTreeMap<String, Arc<ProviderRequestScheduler>>>> =
        OnceLock::new();
    let mut schedulers = SCHEDULERS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .expect("provider scheduler registry lock is not poisoned");
    schedulers
        .entry(provider_key.to_owned())
        .or_insert_with(|| ProviderRequestScheduler::new(PROVIDER_SCHEDULER_CAPACITY))
        .clone()
}

pub type RequestOptions = BTreeMap<String, Value>;

fn default_profile_version() -> u32 {
    PROFILE_VERSION
}

fn default_model_enabled() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderProtocol {
    OpenaiCompatible,
    AnthropicMessages,
    OllamaNative,
    McpAgent,
}

/// Capabilities that can be selected by an enrichment or research route.
/// Labels remain available for human discovery, but only these controlled
/// values authorize a route to use a model.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize, JsonSchema, Ord, PartialOrd)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderCapability {
    TextGeneration,
    CodeGeneration,
    Reasoning,
    StructuredOutput,
    ToolUse,
    Vision,
    DocumentUnderstanding,
    Ocr,
    LayoutAnalysis,
    ChartUnderstanding,
    ImageGeneration,
    VideoUnderstanding,
    VideoGeneration,
    AudioTranscription,
    AudioGeneration,
    TextEmbeddings,
    MultimodalEmbeddings,
    Reranking,
}

impl ProviderCapability {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::TextGeneration => "text-generation",
            Self::CodeGeneration => "code-generation",
            Self::Reasoning => "reasoning",
            Self::StructuredOutput => "structured-output",
            Self::ToolUse => "tool-use",
            Self::Vision => "vision",
            Self::DocumentUnderstanding => "document-understanding",
            Self::Ocr => "ocr",
            Self::LayoutAnalysis => "layout-analysis",
            Self::ChartUnderstanding => "chart-understanding",
            Self::ImageGeneration => "image-generation",
            Self::VideoUnderstanding => "video-understanding",
            Self::VideoGeneration => "video-generation",
            Self::AudioTranscription => "audio-transcription",
            Self::AudioGeneration => "audio-generation",
            Self::TextEmbeddings => "text-embeddings",
            Self::MultimodalEmbeddings => "multimodal-embeddings",
            Self::Reranking => "reranking",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RequestOptionType {
    Boolean,
    Integer,
    Number,
    String,
}

/// A small provider-declared allowlist for safe model-specific request
/// options. It deliberately cannot describe headers, request bodies, or
/// resource limits.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestOptionRule {
    #[serde(rename = "type")]
    pub value_type: RequestOptionType,
    #[serde(default)]
    pub values: Vec<Value>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderModel {
    pub id: String,
    pub api_model: String,
    pub label: String,
    /// Disabled discovery entries make an exact API model visible without
    /// authorizing it for a stage until its capabilities are reviewed.
    #[serde(default = "default_model_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub capabilities: Vec<ProviderCapability>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub request_options: RequestOptions,
    /// Optional complete text-prompt byte budget for this selected model.
    /// Omission retains the conservative transport default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_text_input_bytes: Option<usize>,
}

/// Secret-free provider catalog. A catalog owns one connection and one or more
/// explicitly selectable models; credentials always remain environment-only.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderProfile {
    #[serde(default = "default_profile_version")]
    pub version: u32,
    pub id: String,
    pub protocol: ProviderProtocol,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub catalog_endpoint: Option<String>,
    #[serde(default)]
    pub credential_env: Option<String>,
    #[serde(default)]
    pub source_egress_consent: Option<String>,
    /// Optional bounded request deadline for providers that accept large
    /// document inputs. Credentials and request bodies remain out of config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_seconds: Option<u16>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub request_options: RequestOptions,
    #[serde(default)]
    pub allowed_request_options: BTreeMap<String, RequestOptionRule>,
    #[serde(default)]
    pub models: Vec<ProviderModel>,
}

/// A runtime-only, secret-free model-list observation. It deliberately records
/// API IDs rather than guessing from provider UI labels.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCatalogDiscovery {
    pub schema: String,
    pub profile_id: String,
    pub profile_sha256: String,
    pub protocol: ProviderProtocol,
    pub catalog_endpoint: String,
    pub discovered_at: String,
    pub api_models: Vec<String>,
}

/// A safe, non-persistent confirmation that one configured model accepts the
/// profile's direct request shape. Provider output, prompts, headers, and the
/// credential are deliberately excluded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProviderProbe {
    pub schema: String,
    pub profile_id: String,
    pub profile_sha256: String,
    pub model_id: String,
    pub protocol: ProviderProtocol,
}

impl ProviderProfile {
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        anyhow::ensure!(
            bytes.len() <= MAX_PROFILE_BYTES,
            "provider profile exceeds the {MAX_PROFILE_BYTES}-byte limit"
        );
        let profile: Self = serde_json::from_slice(bytes).context("parse provider profile JSON")?;
        profile.validate()?;
        Ok(profile)
    }

    pub fn from_path(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("read provider profile {}", path.display()))?;
        anyhow::ensure!(
            metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
            "provider profile must be a regular file"
        );
        anyhow::ensure!(
            metadata.len() <= MAX_PROFILE_BYTES as u64,
            "provider profile exceeds the {MAX_PROFILE_BYTES}-byte limit"
        );
        let bytes = fs::read(path)?;
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("yaml" | "yml") => Self::from_yaml(&bytes),
            _ => Self::from_json(&bytes),
        }
    }

    pub fn from_yaml(bytes: &[u8]) -> Result<Self> {
        anyhow::ensure!(
            bytes.len() <= MAX_PROFILE_BYTES,
            "provider profile exceeds the {MAX_PROFILE_BYTES}-byte limit"
        );
        let profile: Self =
            serde_norway::from_slice(bytes).context("parse provider catalog YAML")?;
        profile.validate()?;
        Ok(profile)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.version == PROFILE_VERSION,
            "unsupported provider profile version"
        );
        validate_identifier(&self.id, "provider profile id")?;
        match self.protocol {
            ProviderProtocol::OpenaiCompatible | ProviderProtocol::AnthropicMessages => {
                self.validate_direct(true)?;
            }
            ProviderProtocol::OllamaNative => {
                self.validate_direct(false)?;
                anyhow::ensure!(
                    self.credential_env.is_none(),
                    "ollama-native profiles may not declare an API credential reference"
                );
            }
            ProviderProtocol::McpAgent => {
                anyhow::ensure!(
                    self.endpoint.is_none()
                        && self.catalog_endpoint.is_none()
                        && self.credential_env.is_none()
                        && self.source_egress_consent.is_none()
                        && self.request_timeout_seconds.is_none()
                        && self.request_options.is_empty()
                        && self.allowed_request_options.is_empty()
                        && self.models.is_empty(),
                    "mcp-agent profiles may not declare direct connection, model, or request configuration"
                );
                validate_identifier(
                    self.agent.as_deref().unwrap_or_default(),
                    "mcp-agent profile agent",
                )?;
            }
        }
        Ok(())
    }

    pub fn digest(&self) -> String {
        let encoded = serde_json::to_vec(self).expect("provider profile serializes");
        hex::encode(Sha256::digest(encoded))
    }

    fn validate_direct(&self, requires_key: bool) -> Result<()> {
        anyhow::ensure!(
            self.agent.is_none(),
            "direct provider profiles may not declare an agent"
        );
        let endpoint = self
            .endpoint
            .as_deref()
            .context("provider profile requires endpoint")?;
        validate_endpoint(endpoint)?;
        if let Some(catalog_endpoint) = &self.catalog_endpoint {
            validate_endpoint(catalog_endpoint)?;
        }
        validate_consent(self.source_egress_consent.as_deref().unwrap_or_default())?;
        if let Some(seconds) = self.request_timeout_seconds {
            anyhow::ensure!(
                (10..=600).contains(&seconds),
                "provider profile request timeout must be between 10 and 600 seconds"
            );
        }
        if requires_key {
            validate_env_name(self.credential_env.as_deref().unwrap_or_default())?;
        } else if let Some(name) = &self.credential_env {
            validate_env_name(name)?;
        }
        anyhow::ensure!(
            !self.models.is_empty() && self.models.len() <= MAX_CATALOG_MODELS,
            "direct provider catalogs must declare between one and {MAX_CATALOG_MODELS} models"
        );
        anyhow::ensure!(
            self.models.windows(2).all(|pair| pair[0].id < pair[1].id),
            "provider catalog models must be sorted and unique by ID"
        );
        validate_request_options(&self.request_options, None, "provider request options")?;
        for (name, rule) in &self.allowed_request_options {
            validate_identifier(name, "provider request option name")?;
            validate_request_option_rule(rule, name)?;
        }
        for model in &self.models {
            self.validate_model(model)?;
        }
        Ok(())
    }

    fn validate_model(&self, model: &ProviderModel) -> Result<()> {
        validate_identifier(&model.id, "provider catalog model id")?;
        validate_model(&model.api_model)?;
        validate_display_label(&model.label, "provider catalog model label")?;
        if model.enabled {
            anyhow::ensure!(
                !model.capabilities.is_empty()
                    && model
                        .capabilities
                        .windows(2)
                        .all(|pair| pair[0].as_str() < pair[1].as_str()),
                "enabled provider catalog model capabilities must be sorted, unique, and non-empty"
            );
        } else {
            anyhow::ensure!(
                model.capabilities.is_empty(),
                "disabled provider catalog models may not declare capabilities"
            );
        }
        anyhow::ensure!(
            model.labels.windows(2).all(|pair| pair[0] < pair[1]),
            "provider catalog model labels must be sorted and unique"
        );
        for label in &model.labels {
            validate_identifier(label, "provider catalog model label")?;
        }
        validate_request_options(
            &model.request_options,
            Some(&self.allowed_request_options),
            "provider catalog model request options",
        )?;
        if let Some(max_text_input_bytes) = model.max_text_input_bytes {
            anyhow::ensure!(
                (1..=MAX_MODEL_TEXT_INPUT_BYTES).contains(&max_text_input_bytes),
                "provider catalog model max_text_input_bytes must be between 1 and {MAX_MODEL_TEXT_INPUT_BYTES}"
            );
        }
        Ok(())
    }

    pub fn model(&self, model_id: &str) -> Result<&ProviderModel> {
        self.validate()?;
        let model = self
            .models
            .iter()
            .find(|model| model.id == model_id)
            .with_context(|| format!("provider catalog does not declare model {model_id}"))?;
        anyhow::ensure!(
            model.enabled,
            "provider catalog model {model_id} is discovered but not enabled for routing"
        );
        Ok(model)
    }

    pub fn effective_request_options(
        &self,
        model_id: &str,
        stage_options: &RequestOptions,
    ) -> Result<RequestOptions> {
        let model = self.model(model_id)?;
        validate_request_options(
            stage_options,
            Some(&self.allowed_request_options),
            "stage request options",
        )?;
        let mut options = self.request_options.clone();
        options.extend(model.request_options.clone());
        options.extend(stage_options.clone());
        Ok(options)
    }
}

/// Query an explicitly configured model-list endpoint with the runtime-only
/// credential. The result contains no credential, headers, response body, or
/// provider-generated label; callers review capability labels separately.
pub fn discover_catalog(profile: &ProviderProfile) -> Result<ProviderCatalogDiscovery> {
    profile.validate()?;
    anyhow::ensure!(
        matches!(
            profile.protocol,
            ProviderProtocol::OpenaiCompatible | ProviderProtocol::AnthropicMessages
        ),
        "provider catalog discovery requires an HTTP provider protocol"
    );
    let endpoint = reqwest::Url::parse(
        profile
            .catalog_endpoint
            .as_deref()
            .context("provider catalog discovery requires catalog_endpoint")?,
    )?;
    let (client, api_key) = direct_client_and_environment_key(profile, &endpoint)?;
    discover_catalog_with_client(profile, endpoint, &client, &api_key)
}

/// Send one tiny structured request to an explicitly enabled model. This is a
/// route/credential check only; it never reads or persists source material.
pub fn probe_model(
    profile: &ProviderProfile,
    model_id: &str,
    consent: &str,
) -> Result<ProviderProbe> {
    probe_model_inner(profile, model_id, consent, None)
}

#[cfg(test)]
fn probe_model_with_credential(
    profile: &ProviderProfile,
    model_id: &str,
    consent: &str,
    credential: &str,
) -> Result<ProviderProbe> {
    probe_model_inner(profile, model_id, consent, Some(credential))
}

fn probe_model_inner(
    profile: &ProviderProfile,
    model_id: &str,
    consent: &str,
    credential: Option<&str>,
) -> Result<ProviderProbe> {
    profile.validate()?;
    anyhow::ensure!(
        profile.source_egress_consent.as_deref() == Some(consent),
        "provider probe consent does not match the profile's source egress consent"
    );
    profile.model(model_id)?;
    let transport = match credential {
        Some(credential) => WikiModelTransport::from_profile_with_credential(
            profile,
            model_id,
            &RequestOptions::new(),
            Some(credential),
        )?,
        None => WikiModelTransport::from_profile(profile, model_id, &RequestOptions::new())?,
    };
    transport.complete_json_object(
        "Return one JSON object. This is a connectivity check and contains no source material.",
        "Return {\"ok\":true}.",
    )?;
    Ok(ProviderProbe {
        schema: "graphoxide.provider-probe/v1".into(),
        profile_id: profile.id.clone(),
        profile_sha256: profile.digest(),
        model_id: model_id.into(),
        protocol: profile.protocol.clone(),
    })
}

fn discover_catalog_with_client(
    profile: &ProviderProfile,
    endpoint: reqwest::Url,
    client: &reqwest::blocking::Client,
    api_key: &str,
) -> Result<ProviderCatalogDiscovery> {
    let mut request = client.get(endpoint.clone());
    request = match profile.protocol {
        ProviderProtocol::AnthropicMessages => request
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", api_key),
        ProviderProtocol::OpenaiCompatible => request.bearer_auth(api_key),
        _ => unreachable!("catalog discovery rejects non-HTTP protocols"),
    };
    let mut response = request.send().context("send provider catalog request")?;
    let status = response.status();
    anyhow::ensure!(
        status.is_success(),
        "provider catalog endpoint returned HTTP {status}"
    );
    anyhow::ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= MAX_CATALOG_RESPONSE_BYTES as u64),
        "provider catalog response exceeds the byte cap"
    );
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take((MAX_CATALOG_RESPONSE_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .context("read provider catalog response")?;
    anyhow::ensure!(
        bytes.len() <= MAX_CATALOG_RESPONSE_BYTES,
        "provider catalog response exceeds the byte cap"
    );
    let value: Value =
        serde_json::from_slice(&bytes).context("provider catalog response is not valid JSON")?;
    let records = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(Value::as_array)
        .context("provider catalog response has no data or models array")?;
    let mut api_models = records
        .iter()
        .filter_map(|record| {
            record
                .as_str()
                .or_else(|| record.get("id").and_then(Value::as_str))
        })
        .map(str::to_owned)
        .collect::<Vec<_>>();
    api_models.sort();
    api_models.dedup();
    anyhow::ensure!(
        !api_models.is_empty() && api_models.len() <= MAX_CATALOG_MODELS,
        "provider catalog must return between one and {MAX_CATALOG_MODELS} model IDs"
    );
    for model in &api_models {
        validate_model(model)?;
    }
    Ok(ProviderCatalogDiscovery {
        schema: "graphoxide.provider-catalog-discovery/v1".into(),
        profile_id: profile.id.clone(),
        profile_sha256: profile.digest(),
        protocol: profile.protocol.clone(),
        catalog_endpoint: endpoint.to_string(),
        discovered_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        api_models,
    })
}

/// One consent-gated direct-model transport. Agent profiles intentionally have
/// no direct transport because they operate through the read-only MCP surface.
pub enum WikiModelTransport {
    Ollama(OllamaTransport),
    Http(HttpTransport),
}

pub(crate) struct WikiEnrichmentPermit {
    scheduler: Arc<ProviderRequestScheduler>,
    weight: ProviderRequestWeight,
    permit: Option<ProviderRequestPermit>,
}

impl WikiEnrichmentPermit {
    fn new(scheduler: Arc<ProviderRequestScheduler>, weight: ProviderRequestWeight) -> Self {
        Self {
            permit: Some(scheduler.acquire(weight)),
            scheduler,
            weight,
        }
    }

    fn reacquire(&mut self) {
        self.permit = Some(self.scheduler.acquire(self.weight));
    }
}

/// One bounded source representation accepted by direct enrichment requests.
pub enum EnrichmentModelInput<'a> {
    ExtractedText(&'a str),
    Document {
        media_type: &'a str,
        bytes: &'a [u8],
    },
    Image {
        media_type: &'a str,
        bytes: &'a [u8],
    },
}

fn enrichment_input_weight(input: &EnrichmentModelInput<'_>) -> ProviderRequestWeight {
    match input {
        EnrichmentModelInput::ExtractedText(_) => ProviderRequestWeight::Text,
        EnrichmentModelInput::Document { .. } | EnrichmentModelInput::Image { .. } => {
            ProviderRequestWeight::Binary
        }
    }
}

impl WikiModelTransport {
    pub fn from_profile(
        profile: &ProviderProfile,
        model_id: &str,
        stage_options: &RequestOptions,
    ) -> Result<Self> {
        profile.validate()?;
        if profile.protocol == ProviderProtocol::McpAgent {
            bail!("mcp-agent profiles submit artifacts through MCP and cannot make direct model requests");
        }
        let model = profile.model(model_id)?;
        let options = profile.effective_request_options(model_id, stage_options)?;
        match profile.protocol {
            ProviderProtocol::OllamaNative => {
                let endpoint = profile.endpoint.as_deref().expect("validated endpoint");
                Ok(Self::Ollama(OllamaTransport::local_native(
                    endpoint,
                    &model.api_model,
                )?))
            }
            ProviderProtocol::OpenaiCompatible | ProviderProtocol::AnthropicMessages => {
                Ok(Self::Http(HttpTransport::new(profile, model, options)?))
            }
            ProviderProtocol::McpAgent => {
                unreachable!("MCP agents were rejected before model selection")
            }
        }
    }

    pub(crate) fn from_profile_with_credential(
        profile: &ProviderProfile,
        model_id: &str,
        stage_options: &RequestOptions,
        credential: Option<&str>,
    ) -> Result<Self> {
        profile.validate()?;
        if profile.protocol == ProviderProtocol::McpAgent {
            bail!("mcp-agent profiles submit artifacts through MCP and cannot make direct model requests");
        }
        let model = profile.model(model_id)?;
        let options = profile.effective_request_options(model_id, stage_options)?;
        match profile.protocol {
            ProviderProtocol::OllamaNative => {
                let endpoint = profile.endpoint.as_deref().expect("validated endpoint");
                Ok(Self::Ollama(OllamaTransport::local_native(
                    endpoint,
                    &model.api_model,
                )?))
            }
            ProviderProtocol::OpenaiCompatible | ProviderProtocol::AnthropicMessages => {
                let credential = credential.context("configured provider credential is unset")?;
                Ok(Self::Http(HttpTransport::new_with_credential(
                    profile, model, options, credential,
                )?))
            }
            ProviderProtocol::McpAgent => {
                unreachable!("MCP agents were rejected before model selection")
            }
        }
    }

    pub fn complete_json_object(&self, system: &str, prompt: &str) -> Result<Value> {
        match self {
            Self::Ollama(transport) => {
                let _permit = self.acquire_enrichment_permit(ProviderRequestWeight::Text);
                transport.complete_json_object(system, prompt)
            }
            Self::Http(transport) => transport.complete_json_object(system, prompt),
        }
    }

    pub(crate) fn acquire_enrichment_permit(
        &self,
        weight: ProviderRequestWeight,
    ) -> WikiEnrichmentPermit {
        let scheduler = match self {
            Self::Ollama(transport) => provider_scheduler(transport.endpoint_key()),
            Self::Http(transport) => provider_scheduler(transport.endpoint.as_str()),
        };
        WikiEnrichmentPermit::new(scheduler, weight)
    }

    pub fn complete_enrichment_json(
        &self,
        system: &str,
        prompt: &str,
        input: EnrichmentModelInput<'_>,
    ) -> Result<Value> {
        self.complete_enrichment_json_with_permit(system, prompt, input, None)
    }

    pub(crate) fn complete_enrichment_json_with_permit(
        &self,
        system: &str,
        prompt: &str,
        input: EnrichmentModelInput<'_>,
        permit: Option<&mut WikiEnrichmentPermit>,
    ) -> Result<Value> {
        match self {
            Self::Ollama(transport) => {
                let mut owned_permit = permit
                    .is_none()
                    .then(|| self.acquire_enrichment_permit(enrichment_input_weight(&input)));
                let _permit = permit.or(owned_permit.as_mut());
                match input {
                    EnrichmentModelInput::ExtractedText(text) => {
                        transport.complete_json_object(system, &format!("{prompt}\n\n{text}"))
                    }
                    EnrichmentModelInput::Document { .. } => {
                        bail!("ollama-native enrichment does not support raw document input")
                    }
                    EnrichmentModelInput::Image { media_type, bytes } => {
                        transport.complete_json_object_with_image(system, prompt, media_type, bytes)
                    }
                }
            }
            Self::Http(transport) => {
                transport.complete_enrichment_json(system, prompt, input, permit)
            }
        }
    }

    pub fn complete_markdown(&self, prompt: &str) -> Result<String> {
        match self {
            Self::Ollama(transport) => {
                let _permit = self.acquire_enrichment_permit(ProviderRequestWeight::Text);
                transport.complete_markdown(prompt)
            }
            Self::Http(transport) => transport.complete_markdown(prompt),
        }
    }
}

pub struct HttpTransport {
    client: reqwest::blocking::Client,
    endpoint: reqwest::Url,
    protocol: ProviderProtocol,
    model: String,
    request_options: RequestOptions,
    max_text_input_bytes: usize,
    api_key: String,
}

impl HttpTransport {
    fn new(
        profile: &ProviderProfile,
        model: &ProviderModel,
        request_options: RequestOptions,
    ) -> Result<Self> {
        let api_key = environment_api_key(profile)?;
        Self::new_with_credential(profile, model, request_options, &api_key)
    }

    fn new_with_credential(
        profile: &ProviderProfile,
        model: &ProviderModel,
        request_options: RequestOptions,
        api_key: &str,
    ) -> Result<Self> {
        let endpoint = request_endpoint(profile)?;
        let client = direct_client_and_key(profile, &endpoint, api_key)?;
        Ok(Self {
            client,
            endpoint,
            protocol: profile.protocol.clone(),
            model: model.api_model.clone(),
            request_options,
            max_text_input_bytes: model
                .max_text_input_bytes
                .unwrap_or(MARKDOWN_USER_PROMPT_BYTES),
            api_key: api_key.into(),
        })
    }

    fn complete_json_object(&self, system: &str, prompt: &str) -> Result<Value> {
        anyhow::ensure!(
            !system.is_empty()
                && system.len().saturating_add(prompt.len()) <= self.max_text_input_bytes,
            "wiki model JSON prompt exceeds its prompt byte cap"
        );
        let body = self.complete(system, prompt, JSON_COMPLETION_TOKENS, true)?;
        parse_json_object_response(&body)
    }

    fn complete_enrichment_json(
        &self,
        system: &str,
        prompt: &str,
        input: EnrichmentModelInput<'_>,
        permit: Option<&mut WikiEnrichmentPermit>,
    ) -> Result<Value> {
        let weight = enrichment_input_weight(&input);
        let mut local_permit = permit
            .is_none()
            .then(|| WikiEnrichmentPermit::new(provider_scheduler(self.endpoint.as_str()), weight));
        let permit = permit
            .or(local_permit.as_mut())
            .expect("enrichment request always has a provider permit");
        let body = match input {
            EnrichmentModelInput::ExtractedText(text) => {
                anyhow::ensure!(
                    system
                        .len()
                        .saturating_add(prompt.len())
                        .saturating_add(text.len())
                        <= self.max_text_input_bytes,
                    "wiki model enrichment prompt exceeds its prompt byte cap"
                );
                self.enrichment_body(system, prompt, json!({"type": "text", "text": text}))
            }
            EnrichmentModelInput::Document { media_type, bytes } => {
                anyhow::ensure!(
                    self.protocol == ProviderProtocol::AnthropicMessages
                        && media_type == "application/pdf",
                    "raw document enrichment requires an Anthropic-compatible PDF route"
                );
                validate_http_enrichment_input_bytes(bytes.len(), "document")?;
                let encoded = BASE64_STANDARD.encode(bytes);
                self.enrichment_body(
                    system,
                    prompt,
                    json!({
                        "type": "document",
                        "source": {"type": "base64", "media_type": media_type, "data": encoded},
                    }),
                )
            }
            EnrichmentModelInput::Image { media_type, bytes } => {
                anyhow::ensure!(
                    matches!(media_type, "image/jpeg" | "image/png"),
                    "wiki model enrichment image format is unsupported"
                );
                validate_http_enrichment_input_bytes(bytes.len(), "image")?;
                let encoded = BASE64_STANDARD.encode(bytes);
                let content = match self.protocol {
                    ProviderProtocol::OpenaiCompatible => json!({
                        "type": "image_url",
                        "image_url": {"url": format!("data:{media_type};base64,{encoded}")},
                    }),
                    ProviderProtocol::AnthropicMessages => json!({
                        "type": "image",
                        "source": {"type": "base64", "media_type": media_type, "data": encoded},
                    }),
                    _ => unreachable!("only HTTP provider protocols build HTTP requests"),
                };
                self.enrichment_body(system, prompt, content)
            }
        };
        let body = self.request_value_with_permit(body, permit)?;
        let content = match self.protocol {
            ProviderProtocol::OpenaiCompatible => body.pointer("/choices/0/message/content"),
            ProviderProtocol::AnthropicMessages => body.pointer("/content/0/text"),
            _ => None,
        }
        .and_then(Value::as_str)
        .context("provider returned no enrichment content")?;
        parse_json_object_response(content)
    }

    fn enrichment_body(&self, system: &str, prompt: &str, input: Value) -> Value {
        let body = match self.protocol {
            ProviderProtocol::OpenaiCompatible => {
                let content = input
                    .get("type")
                    .filter(|kind| *kind == "text")
                    .and_then(|_| input.get("text"))
                    .and_then(Value::as_str)
                    .map(|text| Value::String(format!("{prompt}\n\nSource material:\n{text}")))
                    .unwrap_or_else(|| json!([{"type": "text", "text": prompt}, input]));
                json!({
                    "model": self.model,
                    "max_tokens": JSON_COMPLETION_TOKENS,
                    "temperature": 0,
                    "response_format": {"type": "json_object"},
                    "messages": [
                        {"role": "system", "content": system},
                        {"role": "user", "content": content},
                    ],
                })
            }
            ProviderProtocol::AnthropicMessages => json!({
                "model": self.model,
                "max_tokens": JSON_COMPLETION_TOKENS,
                "temperature": 0,
                "system": system,
                "messages": [{"role": "user", "content": [{"type": "text", "text": prompt}, input]}],
            }),
            _ => unreachable!("only HTTP provider protocols build HTTP requests"),
        };
        self.apply_request_options(body)
    }

    fn complete_markdown(&self, prompt: &str) -> Result<String> {
        anyhow::ensure!(
            MARKDOWN_SYSTEM_PROMPT.len().saturating_add(prompt.len()) <= self.max_text_input_bytes,
            "wiki model prompt exceeds its prompt byte cap"
        );
        let first = normalize_wiki_markdown_body(self.complete(
            MARKDOWN_SYSTEM_PROMPT,
            prompt,
            MARKDOWN_COMPLETION_TOKENS,
            false,
        )?);
        if validate_wiki_markdown_body(&first).is_ok() {
            return Ok(first);
        }
        let retry = normalize_wiki_markdown_body(self.complete(
            MARKDOWN_SYSTEM_PROMPT,
            &format!("{prompt}\n\n{MARKDOWN_RETRY_INSTRUCTION}"),
            MARKDOWN_COMPLETION_TOKENS,
            false,
        )?);
        validate_wiki_markdown_body(&retry)?;
        Ok(retry)
    }

    fn complete(
        &self,
        system: &str,
        prompt: &str,
        max_tokens: usize,
        json_object: bool,
    ) -> Result<String> {
        let body = match self.protocol {
            ProviderProtocol::OpenaiCompatible => {
                let mut body = json!({
                    "model": self.model,
                    "max_tokens": max_tokens,
                    "temperature": 0,
                    "messages": [
                        {"role": "system", "content": system},
                        {"role": "user", "content": prompt},
                    ],
                });
                if json_object {
                    body["response_format"] = json!({"type": "json_object"});
                }
                body
            }
            ProviderProtocol::AnthropicMessages => json!({
                "model": self.model,
                "max_tokens": max_tokens,
                "temperature": 0,
                "system": system,
                "messages": [{"role": "user", "content": prompt}],
            }),
            _ => unreachable!("only HTTP provider protocols build HTTP requests"),
        };
        let body = self.apply_request_options(body);
        let response = self.request_value(body)?;
        let content = match self.protocol {
            ProviderProtocol::OpenaiCompatible => response.pointer("/choices/0/message/content"),
            ProviderProtocol::AnthropicMessages => response.pointer("/content/0/text"),
            _ => None,
        }
        .and_then(Value::as_str)
        .context("provider returned no message content")?;
        Ok(content.replace("\r\n", "\n").replace('\r', "\n"))
    }

    fn request_value(&self, body: Value) -> Result<Value> {
        self.request_value_with_weight(body, ProviderRequestWeight::Text)
    }

    fn request_value_with_weight(
        &self,
        body: Value,
        weight: ProviderRequestWeight,
    ) -> Result<Value> {
        let mut permit =
            WikiEnrichmentPermit::new(provider_scheduler(self.endpoint.as_str()), weight);
        self.request_value_with_permit(body, &mut permit)
    }

    fn request_value_with_permit(
        &self,
        body: Value,
        permit: &mut WikiEnrichmentPermit,
    ) -> Result<Value> {
        for attempt in 0..=1 {
            let mut request = self.client.post(self.endpoint.clone()).json(&body);
            request = match self.protocol {
                ProviderProtocol::AnthropicMessages => request
                    .header("anthropic-version", "2023-06-01")
                    .header("x-api-key", &self.api_key),
                ProviderProtocol::OpenaiCompatible => request.bearer_auth(&self.api_key),
                _ => unreachable!("only HTTP provider protocols make HTTP requests"),
            };
            let mut response = request.send().context("send wiki provider request")?;
            let status = response.status();
            if status.as_u16() == 429 {
                if attempt == 0 {
                    // A rate limit applies to this provider endpoint, not merely
                    // this one request. Release the slot before retrying so every
                    // concurrent worker observes the same Retry-After gate.
                    let retry_after = provider_retry_after(response.headers())
                        .map_err(|_| provider_rate_limited_error())?;
                    permit.scheduler.defer(retry_after);
                    drop(response);
                    drop(permit.permit.take());
                    permit.reacquire();
                    continue;
                }
                return Err(provider_rate_limited_error());
            }
            if matches!(status.as_u16(), 500..=599) && attempt == 0 {
                drop(response);
                drop(permit.permit.take());
                std::thread::sleep(Duration::from_millis(100));
                permit.reacquire();
                continue;
            }
            if !status.is_success() {
                if status.as_u16() == 400 && provider_explicitly_rejected_input(&mut response) {
                    bail!("provider endpoint returned HTTP {status}: provider-input-unsupported");
                }
                bail!("provider endpoint returned HTTP {status}");
            }
            anyhow::ensure!(
                response
                    .content_length()
                    .is_none_or(|length| length <= MAX_RESPONSE_BYTES as u64),
                "provider response exceeds the byte cap"
            );
            let mut bytes = Vec::new();
            response
                .by_ref()
                .take((MAX_RESPONSE_BYTES as u64).saturating_add(1))
                .read_to_end(&mut bytes)
                .context("read wiki provider response")?;
            anyhow::ensure!(
                bytes.len() <= MAX_RESPONSE_BYTES,
                "provider response exceeds the byte cap"
            );
            match serde_json::from_slice(&bytes) {
                Ok(value) => return Ok(value),
                Err(error) if attempt == 0 && error.is_eof() => {
                    drop(permit.permit.take());
                    std::thread::sleep(Duration::from_millis(100));
                    permit.reacquire();
                }
                Err(error) => return Err(error).context("provider response is not valid JSON"),
            }
        }
        unreachable!("bounded provider retry loop returns or errors")
    }

    fn apply_request_options(&self, mut body: Value) -> Value {
        let object = body
            .as_object_mut()
            .expect("provider request bodies are always JSON objects");
        for (name, value) in &self.request_options {
            object.insert(name.clone(), value.clone());
        }
        body
    }
}

fn provider_retry_after(headers: &reqwest::header::HeaderMap) -> Result<Duration> {
    let Some(value) = headers.get(reqwest::header::RETRY_AFTER) else {
        return Ok(Duration::from_millis(100));
    };
    let seconds = value
        .to_str()
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .context("provider Retry-After must be an integer number of seconds")?;
    anyhow::ensure!(
        seconds <= MAX_PROVIDER_RETRY_AFTER.as_secs(),
        "provider Retry-After exceeds the 30 second cap"
    );
    Ok(Duration::from_secs(seconds).max(Duration::from_millis(100)))
}

fn provider_rate_limited_error() -> anyhow::Error {
    anyhow::anyhow!("provider-rate-limited")
}

/// Only retain a fixed controller marker from an error response. Provider error
/// text is untrusted and may contain source or request details.
fn provider_explicitly_rejected_input(response: &mut reqwest::blocking::Response) -> bool {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_ERROR_RESPONSE_BYTES as u64)
    {
        return false;
    }
    let mut bytes = Vec::new();
    if response
        .by_ref()
        .take((MAX_PROVIDER_ERROR_RESPONSE_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() > MAX_PROVIDER_ERROR_RESPONSE_BYTES
    {
        return false;
    }
    let code = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|body| {
            body.pointer("/error/code")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    matches!(
        code.as_deref(),
        Some("unsupported_document_input" | "unsupported_input_format")
    )
}

fn validate_http_enrichment_input_bytes(bytes: usize, kind: &str) -> Result<()> {
    anyhow::ensure!(
        bytes <= MAX_HTTP_ENRICHMENT_INPUT_BYTES,
        "wiki model enrichment {kind} exceeds its byte cap"
    );
    Ok(())
}

/// Some otherwise compliant model routes wrap the requested JSON in one
/// Markdown fence. Accept exactly that presentation wrapper, never prose or
/// mixed content, before applying the existing strict object validation.
fn parse_json_object_response(content: &str) -> Result<Value> {
    let content = content.trim();
    let candidate = if let Some(fenced) = content.strip_prefix("```") {
        let (_, body) = fenced
            .split_once('\n')
            .context("provider returned invalid JSON")?;
        body.trim_end()
            .strip_suffix("```")
            .context("provider returned invalid JSON")?
            .trim()
    } else {
        content
    };
    let value: Value = serde_json::from_str(candidate).context("provider returned invalid JSON")?;
    anyhow::ensure!(
        value.is_object(),
        "provider returned a JSON value instead of an object"
    );
    Ok(value)
}

fn direct_client_and_environment_key(
    profile: &ProviderProfile,
    endpoint: &reqwest::Url,
) -> Result<(reqwest::blocking::Client, String)> {
    let api_key = environment_api_key(profile)?;
    let client = direct_client_and_key(profile, endpoint, &api_key)?;
    Ok((client, api_key))
}

fn environment_api_key(profile: &ProviderProfile) -> Result<String> {
    let credential_env = profile
        .credential_env
        .as_deref()
        .expect("validated API key env");
    let api_key = std::env::var_os(credential_env)
        .with_context(|| format!("credential environment variable {credential_env} is not set"))?
        .into_string()
        .map_err(|_| anyhow::anyhow!("credential environment variable is not valid UTF-8"))?;
    validate_api_key(&api_key)?;
    Ok(api_key)
}

fn direct_client_and_key(
    profile: &ProviderProfile,
    endpoint: &reqwest::Url,
    api_key: &str,
) -> Result<reqwest::blocking::Client> {
    let host = endpoint
        .host_str()
        .context("provider profile endpoint has no host")?
        .to_owned();
    let port = endpoint
        .port_or_known_default()
        .context("provider profile endpoint has no port")?;
    let addresses = (host.as_str(), port)
        .to_socket_addrs()
        .context("resolve provider profile endpoint")?
        .map(|address| SocketAddr::new(address.ip(), 0))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        !addresses.is_empty(),
        "provider profile endpoint resolved no addresses"
    );
    if endpoint.scheme() == "http" {
        anyhow::ensure!(
            addresses.iter().all(|address| address.ip().is_loopback()),
            "provider profile HTTP endpoint did not resolve entirely to loopback"
        );
    }
    validate_api_key(api_key)?;
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(u64::from(
            profile.request_timeout_seconds.unwrap_or(60),
        )))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .resolve_to_addrs(&host, &addresses)
        .build()
        .context("build pinned wiki provider client")?;
    Ok(client)
}

fn validate_api_key(api_key: &str) -> Result<()> {
    anyhow::ensure!(
        !api_key.is_empty() && api_key.len() <= 16 * 1024,
        "provider credential has an invalid length"
    );
    Ok(())
}

fn request_endpoint(profile: &ProviderProfile) -> Result<reqwest::Url> {
    let mut endpoint = reqwest::Url::parse(
        profile
            .endpoint
            .as_deref()
            .expect("validated provider endpoint"),
    )?;
    let suffix = match profile.protocol {
        ProviderProtocol::OpenaiCompatible => "chat/completions",
        ProviderProtocol::AnthropicMessages => "messages",
        _ => unreachable!("only HTTP protocols have a request endpoint"),
    };
    if !endpoint.path().trim_end_matches('/').ends_with(suffix) {
        endpoint.set_path(&format!(
            "{}/{suffix}",
            endpoint.path().trim_end_matches('/')
        ));
    }
    Ok(endpoint)
}

fn validate_identifier(value: &str, label: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .enumerate()
                .all(|(index, byte)| byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || (index != 0 && matches!(byte, b'-' | b'_'))),
        "{label} must be a lowercase identifier"
    );
    Ok(())
}

fn validate_model(value: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= 256
            && value.trim() == value
            && !value.chars().any(char::is_control),
        "provider profile model is invalid"
    );
    Ok(())
}

fn validate_display_label(value: &str, label: &str) -> Result<()> {
    anyhow::ensure!(
        !value.trim().is_empty()
            && value.len() <= 160
            && value.trim() == value
            && !value.chars().any(char::is_control),
        "{label} is invalid"
    );
    Ok(())
}

fn validate_request_option_rule(rule: &RequestOptionRule, name: &str) -> Result<()> {
    anyhow::ensure!(
        !reserved_request_option(name),
        "provider request option {name} is reserved"
    );
    anyhow::ensure!(
        rule.values.len() <= 64,
        "provider request option {name} declares too many allowed values"
    );
    anyhow::ensure!(
        rule.values
            .iter()
            .all(|value| request_option_type_matches(value, &rule.value_type)),
        "provider request option {name} allowed values do not match its type"
    );
    let encoded = rule
        .values
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(
        encoded
            .iter()
            .enumerate()
            .all(|(index, value)| !encoded[..index].contains(value)),
        "provider request option {name} allowed values must be unique"
    );
    Ok(())
}

fn validate_request_options(
    options: &RequestOptions,
    rules: Option<&BTreeMap<String, RequestOptionRule>>,
    label: &str,
) -> Result<()> {
    anyhow::ensure!(
        options.len() <= 32,
        "{label} declares too many request options"
    );
    for (name, value) in options {
        validate_identifier(name, "provider request option name")?;
        anyhow::ensure!(
            !reserved_request_option(name),
            "{label} may not override reserved option {name}"
        );
        anyhow::ensure!(
            value.is_boolean() || value.is_number() || value.is_string(),
            "{label} option {name} must be a scalar JSON value"
        );
        if let Some(rules) = rules {
            let rule = rules
                .get(name)
                .with_context(|| format!("{label} option {name} is not allowed by the provider"))?;
            anyhow::ensure!(
                request_option_type_matches(value, &rule.value_type),
                "{label} option {name} does not match its declared type"
            );
            anyhow::ensure!(
                rule.values.is_empty() || rule.values.iter().any(|allowed| allowed == value),
                "{label} option {name} is not one of the provider's allowed values"
            );
        }
    }
    Ok(())
}

fn request_option_type_matches(value: &Value, value_type: &RequestOptionType) -> bool {
    match value_type {
        RequestOptionType::Boolean => value.is_boolean(),
        RequestOptionType::Integer => value.as_i64().is_some() || value.as_u64().is_some(),
        RequestOptionType::Number => value.is_number(),
        RequestOptionType::String => value.is_string(),
    }
}

fn reserved_request_option(name: &str) -> bool {
    matches!(
        name,
        "agent"
            | "api_model"
            | "api_key"
            | "api_key_env"
            | "authorization"
            | "catalog_endpoint"
            | "credential_env"
            | "egress_consent"
            | "endpoint"
            | "headers"
            | "input"
            | "max_completion_tokens"
            | "max_output_tokens"
            | "max_tokens"
            | "messages"
            | "model"
            | "prompt"
            | "response_format"
            | "response_schema"
            | "source_egress_consent"
            | "system"
            | "timeout"
            | "token_limit"
    )
}

fn validate_env_name(value: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value.bytes().enumerate().all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_uppercase() || (index != 0 && byte.is_ascii_digit())
            }),
        "provider catalog credential_env must be an environment variable name"
    );
    Ok(())
}

fn validate_consent(value: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= 256
            && value.trim() == value
            && !value.chars().any(char::is_control),
        "provider profile source_egress_consent is invalid"
    );
    Ok(())
}

fn validate_endpoint(value: &str) -> Result<()> {
    let endpoint = reqwest::Url::parse(value).context("parse provider profile endpoint")?;
    anyhow::ensure!(
        endpoint.username().is_empty()
            && endpoint.password().is_none()
            && endpoint.query().is_none()
            && endpoint.fragment().is_none()
            && endpoint.host_str().is_some(),
        "provider profile endpoint may not contain credentials, a query string, or a fragment"
    );
    match endpoint.scheme() {
        "https" => Ok(()),
        "http" if endpoint_host_is_loopback(&endpoint) => Ok(()),
        "http" => bail!("provider profile endpoint must use HTTPS unless it names loopback"),
        _ => bail!("provider profile endpoint must use HTTP(S)"),
    }
}

fn endpoint_host_is_loopback(endpoint: &reqwest::Url) -> bool {
    endpoint.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

#[cfg(test)]
mod tests {
    #![deny(unsafe_code)]

    use super::*;

    #[test]
    fn provider_scheduler_allows_eight_text_requests_but_only_two_binary_requests() {
        let scheduler = ProviderRequestScheduler::new(8);
        let first = scheduler
            .try_acquire(ProviderRequestWeight::Text)
            .expect("first text request is admitted");
        let second = scheduler
            .try_acquire(ProviderRequestWeight::Text)
            .expect("second text request is admitted");
        let third = scheduler
            .try_acquire(ProviderRequestWeight::Text)
            .expect("third text request is admitted");
        let fourth = scheduler
            .try_acquire(ProviderRequestWeight::Text)
            .expect("fourth text request is admitted");
        let fifth = scheduler
            .try_acquire(ProviderRequestWeight::Text)
            .expect("fifth text request is admitted");
        let sixth = scheduler
            .try_acquire(ProviderRequestWeight::Text)
            .expect("sixth text request is admitted");
        let seventh = scheduler
            .try_acquire(ProviderRequestWeight::Text)
            .expect("seventh text request is admitted");
        let eighth = scheduler
            .try_acquire(ProviderRequestWeight::Text)
            .expect("eighth text request is admitted");
        assert!(
            scheduler.try_acquire(ProviderRequestWeight::Text).is_none(),
            "a ninth text request must wait for bounded provider capacity"
        );
        drop(first);
        assert!(
            scheduler.try_acquire(ProviderRequestWeight::Text).is_some(),
            "releasing one text request makes one unit available"
        );
        drop(second);
        drop(third);
        drop(fourth);
        drop(fifth);
        drop(sixth);
        drop(seventh);
        drop(eighth);

        let first = scheduler
            .try_acquire(ProviderRequestWeight::Binary)
            .expect("first binary request is admitted");
        let second = scheduler
            .try_acquire(ProviderRequestWeight::Binary)
            .expect("second binary request is admitted");
        assert!(
            scheduler
                .try_acquire(ProviderRequestWeight::Binary)
                .is_none(),
            "a third binary request must never be admitted"
        );
        drop(first);
        drop(second);
    }

    #[test]
    fn model_transports_share_weighted_endpoint_admission() {
        let ollama_profile = ProviderProfile::from_json(
            br#"{"version":1,"id":"local-wiki","protocol":"ollama-native","endpoint":"http://127.0.0.1:11434","source_egress_consent":"fixture-consent","models":[{"id":"writer","api_model":"local-model","label":"Writer","capabilities":["vision"]}]}"#,
        )
        .expect("valid Ollama profile");
        let ollama =
            WikiModelTransport::from_profile(&ollama_profile, "writer", &RequestOptions::new())
                .expect("build Ollama transport");
        let key = match &ollama {
            WikiModelTransport::Ollama(transport) => transport.endpoint_key().to_owned(),
            WikiModelTransport::Http(_) => unreachable!("expected Ollama transport"),
        };
        let first = ollama.acquire_enrichment_permit(ProviderRequestWeight::Binary);
        let second = ollama.acquire_enrichment_permit(ProviderRequestWeight::Binary);
        assert!(
            provider_scheduler(&key)
                .try_acquire(ProviderRequestWeight::Binary)
                .is_none(),
            "Ollama must not admit a third binary request"
        );
        drop(first);
        drop(second);

        let http_profile = ProviderProfile::from_json(
            br#"{"version":1,"id":"http-wiki","protocol":"openai-compatible","endpoint":"http://127.0.0.1:11435/v1","credential_env":"GRAPHOXIDE_WEIGHTED_TEST_KEY","source_egress_consent":"fixture-consent","models":[{"id":"writer","api_model":"model","label":"Writer","capabilities":["document-understanding"]}]}"#,
        )
        .expect("valid HTTP profile");
        let http = WikiModelTransport::from_profile_with_credential(
            &http_profile,
            "writer",
            &RequestOptions::new(),
            Some("test-key"),
        )
        .expect("build HTTP transport");
        let key = match &http {
            WikiModelTransport::Http(transport) => transport.endpoint.as_str().to_owned(),
            WikiModelTransport::Ollama(_) => unreachable!("expected HTTP transport"),
        };
        let permits = (0..PROVIDER_SCHEDULER_CAPACITY)
            .map(|_| http.acquire_enrichment_permit(ProviderRequestWeight::Text))
            .collect::<Vec<_>>();
        assert!(
            provider_scheduler(&key)
                .try_acquire(ProviderRequestWeight::Text)
                .is_none(),
            "HTTP must reject the next text request after its configured capacity"
        );
        drop(permits);
    }

    #[test]
    fn provider_retry_after_is_bounded_and_has_a_safe_default() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(
            provider_retry_after(&headers).expect("default delay"),
            Duration::from_millis(100)
        );
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "2".parse().expect("header value"),
        );
        assert_eq!(
            provider_retry_after(&headers).expect("Retry-After delay"),
            Duration::from_secs(2)
        );
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "31".parse().expect("header value"),
        );
        assert!(provider_retry_after(&headers).is_err());
    }

    #[test]
    fn provider_rate_limit_terminal_error_is_stable_after_retry_or_cap_rejection() {
        assert_eq!(
            provider_rate_limited_error().to_string(),
            "provider-rate-limited"
        );
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "31".parse().expect("header value"),
        );
        let error = provider_retry_after(&headers)
            .map_err(|_| provider_rate_limited_error())
            .expect_err("over-cap retry must be terminal");
        assert_eq!(error.to_string(), "provider-rate-limited");
    }
    use std::{
        io::{BufRead, BufReader, ErrorKind, Read, Write},
        net::TcpListener,
        thread,
        time::Instant,
    };

    fn direct_profile(protocol: &str) -> String {
        format!(
            r#"{{"version":1,"id":"internal-wiki","protocol":"{protocol}","endpoint":"https://wiki.example.test/v1","credential_env":"INTERNAL_WIKI_KEY","source_egress_consent":"send-source-text-to-internal-wiki","models":[{{"id":"writer","api_model":"model-v1","label":"Writer","capabilities":["text-generation"]}}]}}"#
        )
    }

    #[test]
    fn direct_profiles_are_versioned_secret_free_and_digest_stable() {
        let profile = ProviderProfile::from_json(direct_profile("openai-compatible").as_bytes())
            .expect("valid profile");
        assert_eq!(profile.digest(), profile.digest());
        assert!(ProviderProfile::from_json(
            br#"{"version":1,"id":"internal-wiki","protocol":"openai-compatible","endpoint":"https://wiki.example.test/v1","api_key":"not-allowed","credential_env":"INTERNAL_WIKI_KEY","source_egress_consent":"send-source-text-to-internal-wiki","models":[{"id":"writer","api_model":"model-v1","label":"Writer","capabilities":["text-generation"]}]}"#
        )
        .is_err());
    }

    #[test]
    fn yaml_provider_catalog_matches_json_without_scalar_coercion() {
        let yaml = br#"---
version: 1
id: internal-wiki
protocol: openai-compatible
endpoint: https://wiki.example.test/v1
credential_env: INTERNAL_WIKI_KEY
source_egress_consent: 'yes'
models:
  - id: writer
    api_model: '2024-12-21'
    label: 'NO'
    capabilities: [structured-output, text-generation]
"#;
        let expected = ProviderProfile::from_json(&serde_json::to_vec(&serde_json::json!({
            "version": 1, "id": "internal-wiki", "protocol": "openai-compatible",
            "endpoint": "https://wiki.example.test/v1", "credential_env": "INTERNAL_WIKI_KEY",
            "source_egress_consent": "yes", "models": [{"id": "writer", "api_model": "2024-12-21",
                "label": "NO", "capabilities": ["structured-output", "text-generation"]}]
        })).expect("equivalent JSON")).expect("JSON profile");
        let parsed = ProviderProfile::from_yaml(yaml).expect("YAML profile");
        assert_eq!(parsed, expected);
        assert_eq!(parsed.digest(), expected.digest());
        let fixture = tempfile::tempdir().expect("provider directory");
        let path = fixture.path().join("provider.yaml");
        fs::write(&path, yaml).expect("write YAML profile");
        assert_eq!(
            ProviderProfile::from_path(&path).expect("YAML file dispatch"),
            expected
        );
    }

    #[test]
    fn yaml_provider_catalog_rejects_duplicate_fields_secrets_and_extra_documents() {
        let yaml = "version: 1\nid: local\nprotocol: mcp-agent\nagent: reviewer\n";
        ProviderProfile::from_yaml(yaml.as_bytes()).expect("valid agent profile");
        for invalid in [
            format!("{yaml}id: replacement\n"),
            format!("{yaml}api_key: untrusted-secret-fixture\n"),
            format!("{yaml}---\n{yaml}"),
        ] {
            assert!(
                ProviderProfile::from_yaml(invalid.as_bytes()).is_err(),
                "ambiguous or forbidden YAML must not be accepted"
            );
        }
        let oversized = vec![b' '; MAX_PROFILE_BYTES + 1];
        assert!(ProviderProfile::from_yaml(&oversized)
            .expect_err("bounded YAML")
            .to_string()
            .contains("byte limit"));
    }

    #[test]
    fn direct_provider_request_timeout_is_bounded_and_optional() {
        let profile = ProviderProfile::from_json(
            direct_profile("anthropic-messages")
                .replace(
                    "\"source_egress_consent\":\"send-source-text-to-internal-wiki\",",
                    "\"source_egress_consent\":\"send-source-text-to-internal-wiki\",\"request_timeout_seconds\":300,",
                )
                .as_bytes(),
        )
        .expect("valid bounded timeout");
        assert_eq!(profile.request_timeout_seconds, Some(300));
        assert!(ProviderProfile::from_json(
            direct_profile("anthropic-messages")
                .replace(
                    "\"source_egress_consent\":\"send-source-text-to-internal-wiki\",",
                    "\"source_egress_consent\":\"send-source-text-to-internal-wiki\",\"request_timeout_seconds\":9,",
                )
                .as_bytes(),
        )
        .is_err());
    }

    #[test]
    fn selected_model_text_input_budget_defaults_and_is_bounded() {
        let default_profile = ProviderProfile::from_json(
            direct_profile("openai-compatible")
                .replace("https://wiki.example.test/v1", "http://127.0.0.1:1/v1")
                .as_bytes(),
        )
        .expect("parse default profile");
        let default_model = default_profile.model("writer").expect("default model");
        let default_transport = HttpTransport::new_with_credential(
            &default_profile,
            default_model,
            RequestOptions::new(),
            "test-key",
        )
        .expect("default transport");
        assert_eq!(
            default_transport.max_text_input_bytes,
            MARKDOWN_USER_PROMPT_BYTES
        );

        let budgeted = direct_profile("openai-compatible").replace(
            "\"label\":\"Writer\"",
            "\"label\":\"Writer\",\"max_text_input_bytes\":262144",
        );
        let budgeted_profile =
            ProviderProfile::from_json(budgeted.as_bytes()).expect("parse budgeted profile");
        assert_eq!(
            serde_json::to_value(budgeted_profile.model("writer").expect("budgeted model"))
                .expect("serialize selected model")
                .pointer("/max_text_input_bytes")
                .and_then(serde_json::Value::as_u64),
            Some(262_144)
        );

        for invalid in [0, 262_145] {
            let invalid = direct_profile("openai-compatible").replace(
                "\"label\":\"Writer\"",
                &format!("\"label\":\"Writer\",\"max_text_input_bytes\":{invalid}"),
            );
            assert!(ProviderProfile::from_json(invalid.as_bytes()).is_err());
        }
    }

    #[test]
    fn selected_model_text_input_budget_rejects_an_over_budget_complete_prompt() {
        let profile = direct_profile("openai-compatible")
            .replace(
                "\"label\":\"Writer\"",
                "\"label\":\"Writer\",\"max_text_input_bytes\":20",
            )
            .replace("https://wiki.example.test/v1", "http://127.0.0.1:1/v1");
        let profile = ProviderProfile::from_json(profile.as_bytes()).expect("parse profile");
        let transport = WikiModelTransport::from_profile_with_credential(
            &profile,
            "writer",
            &RequestOptions::new(),
            Some("test-key"),
        )
        .expect("build budgeted transport");

        let error = transport
            .complete_json_object("Return JSON.", "Use the evidence.")
            .expect_err("complete prompt must respect selected-model budget");
        assert!(error.to_string().contains("prompt byte cap"));
    }

    #[test]
    fn provider_catalog_accepts_multiple_named_models_and_option_rules() {
        let profile = ProviderProfile::from_json(
            br#"{
                "version": 1,
                "id": "research-provider",
                "protocol": "openai-compatible",
                "endpoint": "https://provider.example.test/v1",
                "credential_env": "RESEARCH_PROVIDER_KEY",
                "source_egress_consent": "approved-source-class",
                "request_options": {"temperature": 0},
                "allowed_request_options": {
                    "reasoning_effort": {"type": "string", "values": ["low", "high"]}
                },
                "models": [
                    {
                        "id": "document-reasoner",
                        "api_model": "publisher/document-reasoner",
                        "label": "Document Reasoner",
                        "capabilities": ["document-understanding", "structured-output"],
                        "labels": ["long-context", "technical-research"],
                        "request_options": {"reasoning_effort": "high"}
                    },
                    {
                        "id": "reviewer",
                        "api_model": "publisher/reviewer",
                        "label": "Reviewer",
                        "capabilities": ["reasoning", "structured-output"],
                        "labels": ["independent-review"]
                    }
                ]
            }"#,
        )
        .expect("valid multi-model provider catalog");

        assert_eq!(profile.models.len(), 2);
        assert_eq!(
            profile
                .effective_request_options("document-reasoner", &RequestOptions::new())
                .expect("resolve model options"),
            serde_json::from_value(json!({"reasoning_effort": "high", "temperature": 0}))
                .expect("request option map")
        );
        assert!(profile
            .effective_request_options(
                "document-reasoner",
                &serde_json::from_value(json!({"model": "not-allowed"}))
                    .expect("request option map"),
            )
            .is_err());
    }

    #[test]
    fn discovered_disabled_models_are_visible_but_cannot_be_routed() {
        let profile = ProviderProfile::from_json(
            br#"{
                "version": 1,
                "id": "research-provider",
                "protocol": "openai-compatible",
                "endpoint": "https://provider.example.test/v1",
                "credential_env": "RESEARCH_PROVIDER_KEY",
                "source_egress_consent": "approved-source-class",
                "models": [
                    {
                        "id": "discovered-only",
                        "api_model": "publisher/discovered-only",
                        "label": "Discovered only",
                        "enabled": false,
                        "labels": ["discovered"]
                    }
                ]
            }"#,
        )
        .expect("valid disabled discovery entry");
        assert!(profile.model("discovered-only").is_err());
        assert!(ProviderProfile::from_json(
            br#"{"version":1,"id":"research-provider","protocol":"openai-compatible","endpoint":"https://provider.example.test/v1","credential_env":"RESEARCH_PROVIDER_KEY","source_egress_consent":"approved-source-class","models":[{"id":"invalid-disabled","api_model":"publisher/invalid-disabled","label":"Invalid disabled","enabled":false,"capabilities":["text-generation"]}]}"#
        )
        .is_err());
    }

    #[test]
    fn catalog_discovery_uses_the_declared_protocol_and_returns_sorted_api_ids() {
        for (protocol, expected_header) in [
            ("openai-compatible", "authorization: Bearer test-key"),
            ("anthropic-messages", "x-api-key: test-key"),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind catalog listener");
            let address = listener.local_addr().expect("catalog listener address");
            let request = thread::spawn(move || {
                let (socket, _) = listener.accept().expect("accept catalog request");
                let mut reader = BufReader::new(socket);
                let mut request = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("read catalog header");
                    if line == "\r\n" {
                        break;
                    }
                    request.push_str(&line);
                }
                let mut socket = reader.into_inner();
                let response = r#"{"data":[{"id":"publisher/zeta"},{"id":"publisher/alpha"},{"id":"publisher/zeta"}]}"#;
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
                            response.len()
                        )
                        .as_bytes(),
                    )
                    .expect("write catalog response");
                request
            });
            let profile = ProviderProfile::from_json(
                format!(
                    r#"{{"version":1,"id":"catalog-worker","protocol":"{protocol}","endpoint":"http://{address}/v1","catalog_endpoint":"http://{address}/v1/models","credential_env":"CATALOG_TEST_KEY","source_egress_consent":"approved-source-class","models":[{{"id":"writer","api_model":"publisher/alpha","label":"Writer","capabilities":["text-generation"]}}]}}"#,
                )
                .as_bytes(),
            )
            .expect("parse catalog profile");
            let endpoint = reqwest::Url::parse(&format!("http://{address}/v1/models"))
                .expect("parse catalog endpoint");
            let client = reqwest::blocking::Client::builder()
                .no_proxy()
                .build()
                .expect("build catalog client");
            let discovery = discover_catalog_with_client(&profile, endpoint, &client, "test-key")
                .expect("discover catalog");
            assert_eq!(discovery.api_models, ["publisher/alpha", "publisher/zeta"]);
            let request = request.join().expect("join catalog listener");
            assert!(request.starts_with("GET /v1/models HTTP/1.1\r\n"));
            assert!(request
                .to_ascii_lowercase()
                .contains(&expected_header.to_ascii_lowercase()));
        }
    }

    #[test]
    fn direct_profiles_require_https_except_literal_loopback() {
        let http = direct_profile("anthropic-messages")
            .replace("https://wiki.example.test", "http://wiki.example.test");
        assert!(ProviderProfile::from_json(http.as_bytes()).is_err());
        assert!(ProviderProfile::from_json(
            br#"{"version":1,"id":"local-wiki","protocol":"ollama-native","endpoint":"http://127.0.0.1:11434","source_egress_consent":"send-source-text-to-local-wiki","models":[{"id":"writer","api_model":"local-model","label":"Writer","capabilities":["text-generation"]}]}"#
        )
        .is_ok());
    }

    #[test]
    fn explicit_direct_credential_works_without_an_environment_value() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback provider");
        let address = listener
            .local_addr()
            .expect("read loopback provider address");
        let request = thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept provider request");
            let mut reader = BufReader::new(socket.try_clone().expect("clone provider socket"));
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read provider header");
                if line == "\r\n" {
                    break;
                }
                headers.push_str(&line);
            }
            let length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then_some(value.trim())
                    })
                })
                .expect("content length")
                .parse::<usize>()
                .expect("numeric content length");
            let mut body = vec![0; length];
            reader.read_exact(&mut body).expect("read provider body");
            let response = r#"{"choices":[{"message":{"content":"{\"ok\":true}"}}]}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
                        response.len()
                    )
                    .as_bytes(),
                )
                .expect("write provider response");
            (headers, body)
        });
        let profile = ProviderProfile::from_json(
            format!(
                r#"{{"version":1,"id":"loopback-provider","protocol":"openai-compatible","endpoint":"http://{address}/v1","credential_env":"GX_PROVIDER_DIRECT_LOOPBACK_KEY","source_egress_consent":"fixture-consent","models":[{{"id":"writer","api_model":"fixture-model","label":"Writer","capabilities":["structured-output","text-generation"]}}]}}"#,
            )
            .as_bytes(),
        )
        .expect("parse loopback provider profile");
        assert!(std::env::var_os("GX_PROVIDER_DIRECT_LOOPBACK_KEY").is_none());
        let transport = WikiModelTransport::from_profile_with_credential(
            &profile,
            "writer",
            &RequestOptions::new(),
            Some("test-key"),
        )
        .expect("construct direct profile transport");
        let response = transport.complete_json_object("Return JSON.", "Use the evidence.");
        assert_eq!(
            response.expect("complete direct profile request"),
            json!({"ok": true})
        );
        let (headers, body) = request.join().expect("join provider server");
        assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
        assert!(headers
            .to_ascii_lowercase()
            .contains("authorization: bearer test-key"));
        assert!(String::from_utf8(body)
            .expect("UTF-8 provider body")
            .contains("fixture-model"));
    }

    #[test]
    fn mcp_agent_profile_has_no_direct_egress_configuration() {
        let profile = ProviderProfile::from_json(
            br#"{"version":1,"id":"codex-wiki","protocol":"mcp-agent","agent":"codex"}"#,
        )
        .expect("valid MCP agent profile");
        assert_eq!(profile.protocol, ProviderProtocol::McpAgent);
        assert!(
            WikiModelTransport::from_profile(&profile, "writer", &RequestOptions::new()).is_err()
        );
        assert!(ProviderProfile::from_json(
            br#"{"version":1,"id":"codex-wiki","protocol":"mcp-agent","agent":"codex","endpoint":"https://example.test"}"#
        )
        .is_err());
    }

    #[test]
    fn http_transports_send_the_expected_authenticated_json_contract() {
        for (protocol, response, expected_header, expected_path) in [
            (
                ProviderProtocol::OpenaiCompatible,
                r#"{"choices":[{"message":{"content":"{\"accepted\":true}"}}]}"#,
                "authorization: Bearer test-key",
                "/v1/chat/completions",
            ),
            (
                ProviderProtocol::AnthropicMessages,
                r#"{"content":[{"text":"{\"accepted\":true}"}]}"#,
                "x-api-key: test-key",
                "/v1/messages",
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback provider");
            let address = listener.local_addr().expect("read loopback address");
            let response = response.to_owned();
            let request = thread::spawn(move || {
                let (socket, _) = listener.accept().expect("accept provider request");
                let mut reader = BufReader::new(socket.try_clone().expect("clone provider socket"));
                let mut request = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("read provider header");
                    if line == "\r\n" {
                        break;
                    }
                    request.push_str(&line);
                }
                let content_length = request
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then_some(value.trim())
                    })
                    .expect("content length")
                    .parse::<usize>()
                    .expect("numeric content length");
                let mut body = vec![0; content_length];
                reader.read_exact(&mut body).expect("read provider body");
                request.push_str(&String::from_utf8(body).expect("UTF-8 provider body"));
                let mut socket = reader.into_inner();
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
                            response.len()
                        )
                        .as_bytes(),
                    )
                    .expect("write provider response");
                request
            });
            let suffix = match protocol {
                ProviderProtocol::OpenaiCompatible => "chat/completions",
                ProviderProtocol::AnthropicMessages => "messages",
                _ => unreachable!("test covers HTTP provider protocols"),
            };
            let transport = HttpTransport {
                client: reqwest::blocking::Client::builder()
                    .no_proxy()
                    .build()
                    .expect("build loopback client"),
                endpoint: reqwest::Url::parse(&format!("http://{address}/v1/{suffix}"))
                    .expect("parse loopback endpoint"),
                protocol: protocol.clone(),
                model: "test-model".into(),
                request_options: serde_json::from_value(json!({"reasoning_effort": "high"}))
                    .expect("request option map"),
                max_text_input_bytes: MARKDOWN_USER_PROMPT_BYTES,
                api_key: "test-key".into(),
            };

            assert_eq!(
                transport
                    .complete_json_object("Return JSON.", "Use the evidence.")
                    .expect("complete JSON"),
                json!({"accepted": true})
            );
            let request = request.join().expect("join provider server");
            assert!(request.starts_with(&format!("POST {expected_path} HTTP/1.1\r\n")));
            assert!(request
                .to_ascii_lowercase()
                .contains(&expected_header.to_ascii_lowercase()));
            assert!(request.contains("\"model\":\"test-model\""));
            assert!(request.contains("\"max_tokens\":1024"));
            assert!(request.contains("\"reasoning_effort\":\"high\""));
            assert!(request.contains("\"Use the evidence.\""));
        }
    }

    #[test]
    fn http_transport_retries_one_successful_truncated_outer_json_response() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback provider");
        listener
            .set_nonblocking(true)
            .expect("make loopback provider nonblocking");
        let address = listener.local_addr().expect("read loopback address");
        let requests = thread::spawn(move || {
            let responses = [
                r#"{"choices":[{"message":{"content":"{\"accepted\":true}"}}]"#,
                r#"{"choices":[{"message":{"content":"{\"accepted\":true}"}}]}"#,
            ];
            let mut served = 0;
            let deadline = Instant::now() + Duration::from_secs(1);
            while served < responses.len() && Instant::now() < deadline {
                let (socket, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("accept provider request: {error}"),
                };
                let mut reader = BufReader::new(socket.try_clone().expect("clone provider socket"));
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("read provider header");
                    if line == "\r\n" {
                        break;
                    }
                    headers.push_str(&line);
                }
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then_some(value.trim())
                        })
                    })
                    .expect("content length")
                    .parse::<usize>()
                    .expect("numeric content length");
                let mut body = vec![0; length];
                reader.read_exact(&mut body).expect("read provider body");
                let response = responses[served];
                let mut socket = reader.into_inner();
                socket
                    .write_all(format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
                        response.len()
                    ).as_bytes())
                    .expect("write provider response");
                served += 1;
            }
            served
        });
        let transport = HttpTransport {
            client: reqwest::blocking::Client::builder()
                .no_proxy()
                .build()
                .expect("build loopback client"),
            endpoint: reqwest::Url::parse(&format!("http://{address}/v1/chat/completions"))
                .expect("parse loopback endpoint"),
            protocol: ProviderProtocol::OpenaiCompatible,
            model: "test-model".into(),
            request_options: RequestOptions::new(),
            max_text_input_bytes: MARKDOWN_USER_PROMPT_BYTES,
            api_key: "test-key".into(),
        };

        let response = transport.complete_json_object("Return JSON.", "Use the evidence.");
        assert_eq!(requests.join().expect("join provider server"), 2);
        assert_eq!(
            response.expect("retry must return the strict valid completion"),
            json!({"accepted": true})
        );
    }

    #[test]
    fn http_enrichment_images_use_protocol_content_blocks() {
        for (protocol, response, expected_marker) in [
            (
                ProviderProtocol::OpenaiCompatible,
                r#"{"choices":[{"message":{"content":"{\"accepted\":true}"}}]}"#,
                "image_url",
            ),
            (
                ProviderProtocol::AnthropicMessages,
                r#"{"content":[{"text":"{\"accepted\":true}"}]}"#,
                "\"type\":\"image\"",
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback provider");
            let address = listener.local_addr().expect("read loopback address");
            let response = response.to_owned();
            let request = thread::spawn(move || {
                let (socket, _) = listener.accept().expect("accept provider request");
                let mut reader = BufReader::new(socket.try_clone().expect("clone provider socket"));
                let mut request = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("read provider header");
                    if line == "\r\n" {
                        break;
                    }
                    request.push_str(&line);
                }
                let length = request
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then_some(value.trim())
                        })
                    })
                    .expect("content length")
                    .parse::<usize>()
                    .expect("numeric content length");
                let mut body = vec![0; length];
                reader.read_exact(&mut body).expect("read provider body");
                request.push_str(&String::from_utf8(body).expect("UTF-8 provider body"));
                let mut socket = reader.into_inner();
                socket
                    .write_all(format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
                        response.len()
                    ).as_bytes())
                    .expect("write provider response");
                request
            });
            let suffix = match protocol {
                ProviderProtocol::OpenaiCompatible => "chat/completions",
                ProviderProtocol::AnthropicMessages => "messages",
                _ => unreachable!("test covers HTTP provider protocols"),
            };
            let transport = HttpTransport {
                client: reqwest::blocking::Client::builder()
                    .no_proxy()
                    .build()
                    .expect("build loopback client"),
                endpoint: reqwest::Url::parse(&format!("http://{address}/v1/{suffix}"))
                    .expect("parse loopback endpoint"),
                protocol,
                model: "test-model".into(),
                request_options: RequestOptions::new(),
                max_text_input_bytes: MARKDOWN_USER_PROMPT_BYTES,
                api_key: "test-key".into(),
            };

            assert_eq!(
                transport
                    .complete_enrichment_json(
                        "Return JSON.",
                        "Describe the image.",
                        EnrichmentModelInput::Image {
                            media_type: "image/png",
                            bytes: b"fixture",
                        },
                        None,
                    )
                    .expect("complete image JSON"),
                json!({"accepted": true})
            );
            let request = request.join().expect("join provider server");
            assert!(request.contains(expected_marker));
            assert!(request.contains("Zml4dHVyZQ"));
        }
    }

    #[test]
    fn openai_text_enrichment_uses_a_string_content_message() {
        let transport = HttpTransport {
            client: reqwest::blocking::Client::builder()
                .no_proxy()
                .build()
                .expect("build loopback client"),
            endpoint: reqwest::Url::parse("http://127.0.0.1:1/v1/chat/completions")
                .expect("parse loopback endpoint"),
            protocol: ProviderProtocol::OpenaiCompatible,
            model: "test-model".into(),
            request_options: RequestOptions::new(),
            max_text_input_bytes: MARKDOWN_USER_PROMPT_BYTES,
            api_key: "test-key".into(),
        };

        let body = transport.enrichment_body(
            "Return JSON.",
            "Recognize the supplied text.",
            json!({"type": "text", "text": "source material"}),
        );

        assert_eq!(
            body.pointer("/messages/1/content").and_then(Value::as_str),
            Some("Recognize the supplied text.\n\nSource material:\nsource material"),
            "OpenAI-compatible text enrichment must use the broadly supported string form"
        );
    }

    #[test]
    fn anthropic_pdf_enrichment_uses_a_document_content_block() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback provider");
        let address = listener.local_addr().expect("read loopback address");
        let request = thread::spawn(move || {
            let (socket, _) = listener.accept().expect("accept provider request");
            let mut reader = BufReader::new(socket.try_clone().expect("clone provider socket"));
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read provider header");
                if line == "\r\n" {
                    break;
                }
                headers.push_str(&line);
            }
            let length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then_some(value.trim())
                    })
                })
                .expect("content length")
                .parse::<usize>()
                .expect("numeric content length");
            let mut body = vec![0; length];
            reader.read_exact(&mut body).expect("read provider body");
            let mut socket = reader.into_inner();
            let response = r#"{"content":[{"text":"{\"accepted\":true}"}]}"#;
            socket
                .write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}", response.len()).as_bytes())
                .expect("write provider response");
            String::from_utf8(body).expect("UTF-8 provider body")
        });
        let transport = HttpTransport {
            client: reqwest::blocking::Client::builder()
                .no_proxy()
                .build()
                .expect("build loopback client"),
            endpoint: reqwest::Url::parse(&format!("http://{address}/v1/messages"))
                .expect("parse loopback endpoint"),
            protocol: ProviderProtocol::AnthropicMessages,
            model: "test-model".into(),
            request_options: RequestOptions::new(),
            max_text_input_bytes: MARKDOWN_USER_PROMPT_BYTES,
            api_key: "test-key".into(),
        };
        assert_eq!(
            transport
                .complete_enrichment_json(
                    "Return JSON.",
                    "Read the PDF.",
                    EnrichmentModelInput::Document {
                        media_type: "application/pdf",
                        bytes: b"%PDF-fixture",
                    },
                    None,
                )
                .expect("complete document JSON"),
            json!({"accepted": true})
        );
        let request = request.join().expect("join provider server");
        assert!(request.contains("\"type\":\"document\""));
        assert!(request.contains("\"media_type\":\"application/pdf\""));
        assert!(request.contains("JVBERi1maXh0dXJl"));
    }

    #[test]
    fn document_enrichment_rejects_oversized_bytes_before_encoding_or_egress() {
        validate_http_enrichment_input_bytes(MARKDOWN_USER_PROMPT_BYTES + 1, "document")
            .expect("binary inputs may exceed the text prompt limit");
        let error =
            validate_http_enrichment_input_bytes(MAX_HTTP_ENRICHMENT_INPUT_BYTES + 1, "document")
                .expect_err("oversized documents must be rejected before provider egress");
        assert!(error.to_string().contains("document exceeds its byte cap"));
    }

    #[test]
    fn structured_provider_output_accepts_one_json_fence_only() {
        assert_eq!(
            parse_json_object_response("```json\n{\"accepted\":true}\n```")
                .expect("parse fenced JSON"),
            json!({"accepted": true})
        );
        assert!(parse_json_object_response("Here is the result: {\"accepted\":true}").is_err());
    }

    #[test]
    fn explicit_probe_credential_exercises_the_success_path_without_process_environment() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe listener");
        let address = listener.local_addr().expect("read probe address");
        let request = thread::spawn(move || {
            let (socket, _) = listener.accept().expect("accept probe request");
            let mut reader = BufReader::new(socket);
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read probe header");
                if line == "\r\n" {
                    break;
                }
                headers.push_str(&line);
            }
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .or_else(|| {
                    headers
                        .lines()
                        .find_map(|line| line.strip_prefix("Content-Length: "))
                })
                .expect("probe content length")
                .parse::<usize>()
                .expect("numeric probe content length");
            let mut body = vec![0; length];
            reader.read_exact(&mut body).expect("read probe body");
            let mut socket = reader.into_inner();
            let response = r#"{"choices":[{"message":{"content":"{\"ok\":true}"}}]}"#;
            socket
                .write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}", response.len()).as_bytes())
                .expect("write probe response");
            (headers, String::from_utf8(body).expect("UTF-8 probe body"))
        });
        let key = format!("GRAPHOXIDE_PROBE_TEST_{}", address.port());
        let profile = ProviderProfile::from_json(
            format!(r#"{{"version":1,"id":"probe-worker","protocol":"openai-compatible","endpoint":"http://{address}/v1","credential_env":"{key}","source_egress_consent":"probe-consent","models":[{{"id":"writer","api_model":"probe-model","label":"Writer","capabilities":["structured-output"]}}]}}"#).as_bytes(),
        )
        .expect("parse probe profile");
        assert!(std::env::var_os(&key).is_none());
        assert!(
            probe_model_with_credential(&profile, "writer", "wrong-consent", "test-key").is_err()
        );
        let report = probe_model_with_credential(&profile, "writer", "probe-consent", "test-key")
            .expect("probe model");
        let (headers, body) = request.join().expect("join probe server");
        assert_eq!(report.schema, "graphoxide.provider-probe/v1");
        assert_eq!(report.profile_id, "probe-worker");
        assert_eq!(report.model_id, "writer");
        assert_eq!(report.protocol, ProviderProtocol::OpenaiCompatible);
        assert!(!serde_json::to_string(&report)
            .expect("serialize report")
            .contains("test-key"));
        assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
        assert!(headers
            .to_ascii_lowercase()
            .contains("authorization: bearer test-key"));
        assert!(body.contains("probe-model"));
        assert!(body.contains("This is a connectivity check"));
    }
}
