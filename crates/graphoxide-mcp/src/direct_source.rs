use anyhow::{bail, ensure, Context as _, Result};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};

/// An explicit caller acknowledgement that this one request may transfer data over HTTPS.
#[derive(Debug, Clone, Copy)]
pub struct HttpsFetchConsent(());

/// Make one HTTPS fetch explicit at the call site.
pub const fn allow_https_fetch() -> HttpsFetchConsent {
    HttpsFetchConsent(())
}

/// A server-issued authorization to send one transient source to an AI model.
///
/// This token is deliberately opaque: a tool caller can acknowledge egress in a
/// request, but only the MCP boundary can authorize the adapter to perform it.
#[derive(Debug, Clone, Copy)]
pub struct ModelEgressConsent(());

/// Authorize one AI authoring or review egress at the MCP boundary.
pub const fn allow_model_egress() -> ModelEgressConsent {
    ModelEgressConsent(())
}

/// The bounded, logical source locator accepted by the direct-source MCP surface.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DirectSourceInput {
    BoundPath {
        binding: String,
        relative_path: String,
    },
    Https {
        url: String,
    },
}

/// Batched direct-source admission request. It intentionally contains no physical path or body.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DirectSourceAddRequest {
    pub inputs: Vec<DirectSourceInput>,
    /// Explicit acknowledgement that admitted source text may leave the host for AI authoring.
    pub allow_model_egress: bool,
}

impl DirectSourceAddRequest {
    pub fn new(inputs: Vec<DirectSourceInput>, allow_model_egress: bool) -> Self {
        Self {
            inputs,
            allow_model_egress,
        }
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.inputs.is_empty(),
            "direct source add requires at least one input"
        );
        for input in &self.inputs {
            match input {
                DirectSourceInput::BoundPath {
                    binding,
                    relative_path,
                } => {
                    ensure!(
                        !binding.is_empty()
                            && binding.bytes().all(|byte| byte.is_ascii_alphanumeric()
                                || byte == b'-'
                                || byte == b'_'),
                        "bound source binding must be a non-empty logical alias"
                    );
                    validate_relative_path(relative_path)?;
                }
                DirectSourceInput::Https { url } => validate_https_url(url)?,
            }
        }
        Ok(())
    }

    fn has_https(&self) -> bool {
        self.inputs
            .iter()
            .any(|input| matches!(input, DirectSourceInput::Https { .. }))
    }
}

/// A persisted, storage-free source locator returned by direct-source add and status operations.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DirectSourceLocation {
    Git {
        remote: String,
        commit: String,
        path: String,
    },
    BoundPath {
        binding: String,
        relative_path: String,
    },
    Https {
        url: String,
    },
}

/// Source metadata only; raw bodies and capture identifiers are deliberately absent.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DirectSourceStatus {
    pub source_id: String,
    pub location: DirectSourceLocation,
    pub content_sha256: String,
    pub bytes: u64,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DirectSourceAddResult {
    pub sources: Vec<DirectSourceStatus>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DirectSourceStatusResult {
    pub sources: Vec<DirectSourceStatus>,
}

/// One source refresh request. The adapter resolves the logical source ID; no
/// locator or source bytes cross this MCP contract.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DirectSourceRefreshRequest {
    pub source_id: String,
    /// Explicit acknowledgement for a refresh that resolves to a remote source.
    pub allow_remote_fetch: bool,
    /// Explicit acknowledgement that a changed source may be re-authored by an AI model.
    pub allow_model_egress: bool,
}

/// Metadata returned by a successful direct-source lifecycle operation.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DirectSourceLifecycleResult {
    pub source_id: String,
    pub content_sha256: String,
    pub bytes: u64,
    pub status: String,
}

/// Request an AI quality pass or apply a local human confirmation.
///
/// The tagged shape intentionally prevents HumanConfirm from carrying any
/// egress acknowledgement. Its only wire input is a logical source ID.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DirectSourceReviewRequest {
    Ai {
        source_id: String,
        /// Explicit acknowledgement for an AI review that resolves to a remote source.
        allow_remote_fetch: bool,
        /// Explicit acknowledgement that the transient source may leave the host for AI review.
        allow_model_egress: bool,
    },
    HumanConfirm {
        source_id: String,
    },
}

impl DirectSourceReviewRequest {
    fn source_id(&self) -> &str {
        match self {
            Self::Ai { source_id, .. } | Self::HumanConfirm { source_id } => source_id,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DirectSourceReviewDecision {
    Approve,
    Reject,
}

/// Metadata-only direct review result.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DirectSourceReviewResult {
    pub source: DirectSourceLifecycleResult,
    pub decision: DirectSourceReviewDecision,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DirectSourceRetireRequest {
    pub source_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DirectSourceRetireResult {
    pub source_id: String,
    pub retired: bool,
}

/// Adapter boundary for the direct-source implementation owned outside this crate.
pub trait DirectSourceService: Send + Sync {
    /// Implementations must reject source authoring when `model_egress` is
    /// absent. The token is issued only after server authorization and a
    /// per-request acknowledgement at the MCP boundary.
    fn add_sources(
        &self,
        request: DirectSourceAddRequest,
        model_egress: Option<ModelEgressConsent>,
    ) -> Result<DirectSourceAddResult>;
    fn source_status(&self) -> Result<DirectSourceStatusResult>;

    /// Implementations must reject a remote-resolved source when `remote_fetch`
    /// is `None` and re-authoring when `model_egress` is `None`; tokens are
    /// issued only after server authorization and per-request acknowledgement.
    fn refresh_source(
        &self,
        _request: DirectSourceRefreshRequest,
        _remote_fetch: Option<HttpsFetchConsent>,
        _model_egress: Option<ModelEgressConsent>,
    ) -> Result<DirectSourceLifecycleResult> {
        bail!("direct source refresh is not bound")
    }

    /// Implementations must reject remote source resolution without
    /// `remote_fetch`, and AI review egress without `model_egress`.
    /// HumanConfirm is a source-ID-only, persisted local-status operation and
    /// always receives neither token.
    fn review_source(
        &self,
        _request: DirectSourceReviewRequest,
        _remote_fetch: Option<HttpsFetchConsent>,
        _model_egress: Option<ModelEgressConsent>,
    ) -> Result<DirectSourceReviewResult> {
        bail!("direct source review is not bound")
    }

    fn retire_source(
        &self,
        _request: DirectSourceRetireRequest,
    ) -> Result<DirectSourceRetireResult> {
        bail!("direct source retirement is not bound")
    }
}

impl<T> DirectSourceService for std::sync::Arc<T>
where
    T: DirectSourceService + ?Sized,
{
    fn add_sources(
        &self,
        request: DirectSourceAddRequest,
        model_egress: Option<ModelEgressConsent>,
    ) -> Result<DirectSourceAddResult> {
        self.as_ref().add_sources(request, model_egress)
    }

    fn source_status(&self) -> Result<DirectSourceStatusResult> {
        self.as_ref().source_status()
    }

    fn refresh_source(
        &self,
        request: DirectSourceRefreshRequest,
        remote_fetch: Option<HttpsFetchConsent>,
        model_egress: Option<ModelEgressConsent>,
    ) -> Result<DirectSourceLifecycleResult> {
        self.as_ref()
            .refresh_source(request, remote_fetch, model_egress)
    }

    fn review_source(
        &self,
        request: DirectSourceReviewRequest,
        remote_fetch: Option<HttpsFetchConsent>,
        model_egress: Option<ModelEgressConsent>,
    ) -> Result<DirectSourceReviewResult> {
        self.as_ref()
            .review_source(request, remote_fetch, model_egress)
    }

    fn retire_source(
        &self,
        request: DirectSourceRetireRequest,
    ) -> Result<DirectSourceRetireResult> {
        self.as_ref().retire_source(request)
    }
}

/// MCP-facing authorization boundary. HTTPS needs a server opt-in and per-call consent.
#[derive(Clone)]
pub struct DirectSourceMcp<S> {
    service: S,
    https_fetch_enabled: bool,
    model_egress_enabled: bool,
}

impl<S> DirectSourceMcp<S>
where
    S: DirectSourceService,
{
    pub const fn new(service: S, https_fetch_enabled: bool) -> Self {
        Self::with_capabilities(service, https_fetch_enabled, false)
    }

    /// Construct an MCP boundary with independently granted network and model
    /// egress capabilities. Model egress is intentionally disabled by `new`.
    pub const fn with_capabilities(
        service: S,
        https_fetch_enabled: bool,
        model_egress_enabled: bool,
    ) -> Self {
        Self {
            service,
            https_fetch_enabled,
            model_egress_enabled,
        }
    }

    pub fn add_sources(
        &self,
        request: DirectSourceAddRequest,
        consent: Option<HttpsFetchConsent>,
    ) -> Result<DirectSourceAddResult> {
        request.validate()?;
        ensure!(
            request.allow_model_egress,
            "direct source add requires explicit model egress acknowledgement"
        );
        ensure!(
            self.model_egress_enabled,
            "server is not authorized for direct source model egress"
        );
        if request.has_https() {
            ensure!(
                self.https_fetch_enabled,
                "server is not authorized to fetch HTTPS direct sources"
            );
            ensure!(
                consent.is_some(),
                "HTTPS direct sources require explicit per-call fetch consent"
            );
        }
        self.service
            .add_sources(request, Some(allow_model_egress()))
    }

    pub fn source_status(&self) -> Result<DirectSourceStatusResult> {
        self.service.source_status()
    }

    pub fn refresh_source(
        &self,
        request: DirectSourceRefreshRequest,
    ) -> Result<DirectSourceLifecycleResult> {
        validate_refresh_request(&request)?;
        ensure!(
            self.model_egress_enabled,
            "server is not authorized for direct source model egress"
        );
        let remote_fetch = if request.allow_remote_fetch {
            ensure!(
                self.https_fetch_enabled,
                "server is not authorized to fetch remote direct sources"
            );
            Some(allow_https_fetch())
        } else {
            None
        };
        self.service
            .refresh_source(request, remote_fetch, Some(allow_model_egress()))
    }

    pub fn review_source(
        &self,
        request: DirectSourceReviewRequest,
    ) -> Result<DirectSourceReviewResult> {
        validate_review_request(&request)?;
        let (remote_fetch, model_egress) = match &request {
            DirectSourceReviewRequest::HumanConfirm { .. } => (None, None),
            DirectSourceReviewRequest::Ai {
                allow_remote_fetch, ..
            } => {
                let remote_fetch = if *allow_remote_fetch {
                    ensure!(
                        self.https_fetch_enabled,
                        "server is not authorized to fetch remote direct sources"
                    );
                    Some(allow_https_fetch())
                } else {
                    None
                };
                ensure!(
                    self.model_egress_enabled,
                    "server is not authorized for direct source model egress"
                );
                (remote_fetch, Some(allow_model_egress()))
            }
        };
        self.service
            .review_source(request, remote_fetch, model_egress)
    }

    pub fn retire_source(
        &self,
        request: DirectSourceRetireRequest,
    ) -> Result<DirectSourceRetireResult> {
        validate_source_id(&request.source_id)?;
        self.service.retire_source(request)
    }
}

fn validate_refresh_request(request: &DirectSourceRefreshRequest) -> Result<()> {
    validate_source_id(&request.source_id)?;
    ensure!(
        request.allow_model_egress,
        "direct source refresh requires explicit model egress acknowledgement"
    );
    Ok(())
}

fn validate_review_request(request: &DirectSourceReviewRequest) -> Result<()> {
    validate_source_id(request.source_id())?;
    match request {
        DirectSourceReviewRequest::Ai {
            allow_model_egress, ..
        } => {
            ensure!(
                *allow_model_egress,
                "AI direct source review requires explicit model egress acknowledgement"
            );
        }
        DirectSourceReviewRequest::HumanConfirm { .. } => {}
    }
    Ok(())
}

fn validate_source_id(source_id: &str) -> Result<()> {
    ensure!(
        source_id.starts_with("src:")
            && source_id.len() <= 128
            && !source_id[4..].is_empty()
            && source_id[4..]
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "direct source ID is invalid"
    );
    Ok(())
}

fn validate_relative_path(relative_path: &str) -> Result<()> {
    ensure!(
        !relative_path.is_empty(),
        "bound source relative path must not be empty"
    );
    let path = Path::new(relative_path);
    ensure!(path.is_relative(), "bound source path must be relative");
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                bail!("bound source path must not contain traversal components")
            }
        }
    }
    Ok(())
}

fn validate_https_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).context("direct source URL must be a valid HTTPS URL")?;
    ensure!(
        parsed.scheme() == "https" && parsed.host_str().is_some(),
        "direct source URL must be a valid HTTPS URL"
    );
    ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "direct source URL must not contain credentials"
    );
    ensure!(
        parsed.query().is_none(),
        "direct source URL must not contain a query string"
    );
    ensure!(
        parsed.fragment().is_none(),
        "direct source URL must not contain a fragment"
    );
    ensure!(
        parsed.as_str() == url,
        "direct source URL must use its normalized HTTPS form"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_add_requires_server_authorization_and_call_consent() {
        let request = DirectSourceAddRequest::new(
            vec![DirectSourceInput::Https {
                url: "https://docs.example.test/reference.md".into(),
            }],
            true,
        );
        let disabled = DirectSourceMcp::with_capabilities(TestService, false, true);
        assert!(disabled
            .add_sources(request.clone(), Some(allow_https_fetch()))
            .is_err());

        let enabled = DirectSourceMcp::with_capabilities(TestService, true, true);
        assert!(enabled.add_sources(request.clone(), None).is_err());
        assert_eq!(
            enabled
                .add_sources(request, Some(allow_https_fetch()))
                .expect("explicitly authorized HTTPS source")
                .sources,
            vec![status("src:docs")]
        );
    }

    #[test]
    fn direct_add_requires_server_and_per_call_model_egress_consent() {
        let request = DirectSourceAddRequest {
            inputs: vec![DirectSourceInput::BoundPath {
                binding: "raw-root-01".into(),
                relative_path: "protocols/redfish.md".into(),
            }],
            allow_model_egress: false,
        };
        let error = DirectSourceMcp::new(TestService, false)
            .add_sources(request.clone(), None)
            .expect_err("source authoring must require server model-egress authorization");
        assert!(error.to_string().contains("model egress"));

        let enabled = DirectSourceMcp::with_capabilities(TestService, false, true);
        let error = enabled
            .add_sources(request, None)
            .expect_err("source authoring must require explicit model-egress acknowledgement");
        assert!(error.to_string().contains("model egress"));
    }

    #[test]
    fn bound_paths_are_logical_and_https_schema_has_no_storage_fields() {
        let request = DirectSourceAddRequest::new(
            vec![DirectSourceInput::BoundPath {
                binding: "raw-root-01".into(),
                relative_path: "protocols/redfish.md".into(),
            }],
            true,
        );
        let result = DirectSourceMcp::with_capabilities(TestService, false, true)
            .add_sources(request, None)
            .expect("bound source needs no network authorization");
        assert_eq!(result.sources, vec![status("src:docs")]);

        let schema =
            serde_json::to_value(schemars::schema_for!(DirectSourceAddRequest)).expect("schema");
        let encoded = schema.to_string();
        for forbidden in [
            "physical_path",
            "raw_body",
            "capture_id",
            "source_store",
            "page_ids",
        ] {
            assert!(!encoded.contains(forbidden), "schema leaked {forbidden}");
        }
        assert!(encoded.contains("binding"));
        assert!(encoded.contains("relative_path"));
        assert!(encoded.contains("url"));
    }

    #[test]
    fn noncanonical_https_url_is_rejected_before_service_dispatch() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let service = CountingService(calls.clone());
        let error = DirectSourceMcp::new(service, true)
            .add_sources(
                DirectSourceAddRequest::new(
                    vec![DirectSourceInput::Https {
                        url: "https://DOCS.example.test/reference.md".into(),
                    }],
                    true,
                ),
                Some(allow_https_fetch()),
            )
            .expect_err("noncanonical host must be rejected");
        assert!(error.to_string().contains("normalized HTTPS form"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn lifecycle_operations_expose_only_logical_result_metadata() {
        let service = LifecycleService;
        let mcp = DirectSourceMcp::with_capabilities(service, true, true);

        assert_eq!(
            mcp.refresh_source(DirectSourceRefreshRequest {
                source_id: "src:docs".into(),
                allow_remote_fetch: true,
                allow_model_egress: true,
            })
            .expect("refresh logical source"),
            lifecycle_result("ai-reviewed")
        );
        assert_eq!(
            mcp.review_source(DirectSourceReviewRequest::Ai {
                source_id: "src:docs".into(),
                allow_remote_fetch: false,
                allow_model_egress: true,
            })
            .expect("review logical source"),
            DirectSourceReviewResult {
                source: lifecycle_result("ai-reviewed"),
                decision: DirectSourceReviewDecision::Approve,
            }
        );
        assert_eq!(
            mcp.retire_source(DirectSourceRetireRequest {
                source_id: "src:docs".into(),
            })
            .expect("retire logical source"),
            DirectSourceRetireResult {
                source_id: "src:docs".into(),
                retired: true,
            }
        );

        let schema =
            serde_json::to_value(schemars::schema_for!(DirectSourceReviewRequest)).expect("schema");
        let encoded = schema.to_string();
        for forbidden in [
            "physical_path",
            "raw_body",
            "locator",
            "provider_profile",
            "credential",
            "prompt",
            "attestation",
        ] {
            assert!(!encoded.contains(forbidden), "schema leaked {forbidden}");
        }
    }

    #[test]
    fn refresh_wire_request_requires_model_egress_acknowledgement() {
        let schema = serde_json::to_value(schemars::schema_for!(DirectSourceRefreshRequest))
            .expect("schema");
        let properties = schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("refresh request properties");
        assert!(
            properties.contains_key("allow_model_egress"),
            "a refresh can re-author a changed source"
        );
    }

    #[test]
    fn human_confirmation_wire_request_is_source_id_only_and_local() {
        let extra_fields = serde_json::json!({
            "mode": "human-confirm",
            "source_id": "src:docs",
            "allow_remote_fetch": false,
            "allow_model_egress": false,
        });
        let error = serde_json::from_value::<DirectSourceReviewRequest>(extra_fields)
            .expect_err("human confirmation must reject every egress field");
        assert!(error.to_string().contains("unknown field"));

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mcp = DirectSourceMcp::new(CountingLifecycleService(calls.clone()), false);
        let result = mcp
            .review_source(DirectSourceReviewRequest::HumanConfirm {
                source_id: "src:docs".into(),
            })
            .expect("local human confirmation needs neither server capability");
        assert_eq!(result.decision, DirectSourceReviewDecision::Reject);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn ai_review_requires_server_model_egress_capability_and_call_acknowledgement() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let disabled = DirectSourceMcp::new(CountingLifecycleService(calls.clone()), true);
        let error = disabled
            .review_source(DirectSourceReviewRequest::Ai {
                source_id: "src:docs".into(),
                allow_remote_fetch: false,
                allow_model_egress: true,
            })
            .expect_err("server model-egress authorization is required");
        assert!(error.to_string().contains("model egress"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);

        let network_disabled = DirectSourceMcp::with_capabilities(
            CountingLifecycleService(calls.clone()),
            false,
            true,
        );
        let error = network_disabled
            .review_source(DirectSourceReviewRequest::Ai {
                source_id: "src:docs".into(),
                allow_remote_fetch: true,
                allow_model_egress: true,
            })
            .expect_err("remote AI review requires server network authorization");
        assert!(error.to_string().contains("not authorized"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);

        let enabled =
            DirectSourceMcp::with_capabilities(CountingLifecycleService(calls.clone()), true, true);
        enabled
            .review_source(DirectSourceReviewRequest::Ai {
                source_id: "src:docs".into(),
                allow_remote_fetch: false,
                allow_model_egress: true,
            })
            .expect("AI review receives both egress authorizations");
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn lifecycle_authorization_and_invalid_ai_requests_fail_before_service_dispatch() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let disabled = DirectSourceMcp::new(CountingLifecycleService(calls.clone()), false);
        let refresh = disabled
            .refresh_source(DirectSourceRefreshRequest {
                source_id: "src:docs".into(),
                allow_remote_fetch: true,
                allow_model_egress: true,
            })
            .expect_err("server model-egress authorization is required");
        assert!(refresh.to_string().contains("model egress"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);

        let missing_ack =
            DirectSourceMcp::with_capabilities(CountingLifecycleService(calls.clone()), true, true)
                .refresh_source(DirectSourceRefreshRequest {
                    source_id: "src:docs".into(),
                    allow_remote_fetch: false,
                    allow_model_egress: false,
                })
                .expect_err("refresh re-authoring needs explicit model-egress acknowledgement");
        assert!(missing_ack.to_string().contains("model egress"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);

        let enabled = DirectSourceMcp::new(CountingLifecycleService(calls.clone()), true);

        let ai = enabled
            .review_source(DirectSourceReviewRequest::Ai {
                source_id: "src:docs".into(),
                allow_remote_fetch: false,
                allow_model_egress: false,
            })
            .expect_err("AI review needs explicit provider egress acknowledgement");
        assert!(ai.to_string().contains("model egress"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);

        let empty = enabled
            .retire_source(DirectSourceRetireRequest {
                source_id: "src:".into(),
            })
            .expect_err("empty logical source ID is invalid");
        assert!(empty.to_string().contains("source ID"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn lifecycle_adapters_receive_nonforgeable_transfer_and_egress_consents() {
        let mcp = DirectSourceMcp::with_capabilities(TokenRequiredLifecycleService, true, true);

        mcp.refresh_source(DirectSourceRefreshRequest {
            source_id: "src:docs".into(),
            allow_remote_fetch: true,
            allow_model_egress: true,
        })
        .expect("remote refresh carries the server-issued transfer consent");

        mcp.review_source(DirectSourceReviewRequest::Ai {
            source_id: "src:docs".into(),
            allow_remote_fetch: false,
            allow_model_egress: true,
        })
        .expect("AI review carries the server-issued model-egress consent");

        mcp.review_source(DirectSourceReviewRequest::HumanConfirm {
            source_id: "src:docs".into(),
        })
        .expect("human confirmation carries neither transfer nor model-egress consent");
    }

    #[test]
    fn human_confirmation_wire_contract_rejects_nonlocal_fields() {
        let request = serde_json::json!({
            "source_id": "src:docs",
            "mode": "human-confirm",
            "allow_remote_fetch": false,
        });

        let error = serde_json::from_value::<DirectSourceReviewRequest>(request)
            .expect_err("human confirmation accepts no remote or model field");
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn default_lifecycle_service_methods_fail_closed() {
        let error = DirectSourceMcp::new(TestService, true)
            .retire_source(DirectSourceRetireRequest {
                source_id: "src:docs".into(),
            })
            .expect_err("unbound lifecycle service must fail closed");
        assert!(error.to_string().contains("not bound"));
    }

    #[derive(Clone)]
    struct TestService;

    struct CountingService(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    struct LifecycleService;

    struct CountingLifecycleService(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    struct TokenRequiredLifecycleService;

    impl DirectSourceService for TestService {
        fn add_sources(
            &self,
            _request: DirectSourceAddRequest,
            _model_egress: Option<ModelEgressConsent>,
        ) -> anyhow::Result<DirectSourceAddResult> {
            Ok(DirectSourceAddResult {
                sources: vec![status("src:docs")],
            })
        }

        fn source_status(&self) -> anyhow::Result<DirectSourceStatusResult> {
            Ok(DirectSourceStatusResult {
                sources: vec![status("src:docs")],
            })
        }
    }

    impl DirectSourceService for CountingService {
        fn add_sources(
            &self,
            _request: DirectSourceAddRequest,
            _model_egress: Option<ModelEgressConsent>,
        ) -> anyhow::Result<DirectSourceAddResult> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(DirectSourceAddResult { sources: vec![] })
        }

        fn source_status(&self) -> anyhow::Result<DirectSourceStatusResult> {
            Ok(DirectSourceStatusResult { sources: vec![] })
        }
    }

    impl DirectSourceService for LifecycleService {
        fn add_sources(
            &self,
            _request: DirectSourceAddRequest,
            _model_egress: Option<ModelEgressConsent>,
        ) -> anyhow::Result<DirectSourceAddResult> {
            Ok(DirectSourceAddResult { sources: vec![] })
        }

        fn source_status(&self) -> anyhow::Result<DirectSourceStatusResult> {
            Ok(DirectSourceStatusResult { sources: vec![] })
        }

        fn refresh_source(
            &self,
            _request: DirectSourceRefreshRequest,
            _remote_fetch: Option<HttpsFetchConsent>,
            _model_egress: Option<ModelEgressConsent>,
        ) -> anyhow::Result<DirectSourceLifecycleResult> {
            Ok(lifecycle_result("ai-reviewed"))
        }

        fn review_source(
            &self,
            _request: DirectSourceReviewRequest,
            _remote_fetch: Option<HttpsFetchConsent>,
            _model_egress: Option<ModelEgressConsent>,
        ) -> anyhow::Result<DirectSourceReviewResult> {
            Ok(DirectSourceReviewResult {
                source: lifecycle_result("ai-reviewed"),
                decision: DirectSourceReviewDecision::Approve,
            })
        }

        fn retire_source(
            &self,
            _request: DirectSourceRetireRequest,
        ) -> anyhow::Result<DirectSourceRetireResult> {
            Ok(DirectSourceRetireResult {
                source_id: "src:docs".into(),
                retired: true,
            })
        }
    }

    impl DirectSourceService for CountingLifecycleService {
        fn add_sources(
            &self,
            _request: DirectSourceAddRequest,
            _model_egress: Option<ModelEgressConsent>,
        ) -> anyhow::Result<DirectSourceAddResult> {
            Ok(DirectSourceAddResult { sources: vec![] })
        }

        fn source_status(&self) -> anyhow::Result<DirectSourceStatusResult> {
            Ok(DirectSourceStatusResult { sources: vec![] })
        }

        fn refresh_source(
            &self,
            _request: DirectSourceRefreshRequest,
            _remote_fetch: Option<HttpsFetchConsent>,
            _model_egress: Option<ModelEgressConsent>,
        ) -> anyhow::Result<DirectSourceLifecycleResult> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(lifecycle_result("provisional"))
        }

        fn review_source(
            &self,
            _request: DirectSourceReviewRequest,
            _remote_fetch: Option<HttpsFetchConsent>,
            _model_egress: Option<ModelEgressConsent>,
        ) -> anyhow::Result<DirectSourceReviewResult> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(DirectSourceReviewResult {
                source: lifecycle_result("provisional"),
                decision: DirectSourceReviewDecision::Reject,
            })
        }
    }

    impl DirectSourceService for TokenRequiredLifecycleService {
        fn add_sources(
            &self,
            _request: DirectSourceAddRequest,
            _model_egress: Option<ModelEgressConsent>,
        ) -> anyhow::Result<DirectSourceAddResult> {
            Ok(DirectSourceAddResult { sources: vec![] })
        }

        fn source_status(&self) -> anyhow::Result<DirectSourceStatusResult> {
            Ok(DirectSourceStatusResult { sources: vec![] })
        }

        fn refresh_source(
            &self,
            _request: DirectSourceRefreshRequest,
            remote_fetch: Option<HttpsFetchConsent>,
            model_egress: Option<ModelEgressConsent>,
        ) -> anyhow::Result<DirectSourceLifecycleResult> {
            ensure!(remote_fetch.is_some(), "missing transfer consent");
            ensure!(model_egress.is_some(), "missing model egress consent");
            Ok(lifecycle_result("refreshed"))
        }

        fn review_source(
            &self,
            request: DirectSourceReviewRequest,
            remote_fetch: Option<HttpsFetchConsent>,
            model_egress: Option<ModelEgressConsent>,
        ) -> anyhow::Result<DirectSourceReviewResult> {
            match request {
                DirectSourceReviewRequest::Ai { .. } => {
                    ensure!(remote_fetch.is_none(), "unexpected transfer consent");
                    ensure!(model_egress.is_some(), "missing model egress consent");
                }
                DirectSourceReviewRequest::HumanConfirm { .. } => {
                    ensure!(remote_fetch.is_none(), "unexpected transfer consent");
                    ensure!(model_egress.is_none(), "unexpected model egress consent");
                }
            }
            Ok(DirectSourceReviewResult {
                source: lifecycle_result("reviewed"),
                decision: DirectSourceReviewDecision::Approve,
            })
        }
    }

    fn status(source_id: &str) -> DirectSourceStatus {
        DirectSourceStatus {
            source_id: source_id.into(),
            location: DirectSourceLocation::Https {
                url: "https://docs.example.test/reference.md".into(),
            },
            content_sha256: "a".repeat(64),
            bytes: 3,
            status: "human-confirmed".into(),
        }
    }

    fn lifecycle_result(status: &str) -> DirectSourceLifecycleResult {
        DirectSourceLifecycleResult {
            source_id: "src:docs".into(),
            content_sha256: "a".repeat(64),
            bytes: 3,
            status: status.into(),
        }
    }
}
