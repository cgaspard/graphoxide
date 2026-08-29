//! Strict, manifest-driven read-only OpenAPI service contracts.

use anyhow::{bail, ensure, Context as _, Result};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::Read as _,
    net::{SocketAddr, ToSocketAddrs},
    path::Path,
    time::Duration,
};

const VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_SPEC_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenApiServiceManifest {
    version: u32,
    id: String,
    base_url: String,
    allowed_host: String,
    spec_sha256: String,
    credential_env: String,
    credential_header: String,
    #[serde(default)]
    credential_prefix: String,
    request_identity: String,
    allowed_operations: Vec<String>,
    max_response_bytes: u64,
    requests_per_minute: u32,
    max_pages: u32,
    pagination: Pagination,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum Pagination {
    None,
}

#[derive(Debug, Eq, PartialEq)]
pub struct OpenApiReadRequest {
    pub service_id: String,
    pub operation_id: String,
    pub method: &'static str,
    pub url: String,
    pub credential_env: String,
    pub credential_header: String,
    pub credential_prefix: String,
    pub request_identity: String,
    pub max_response_bytes: u64,
}

#[derive(Debug, Eq, PartialEq, serde::Serialize)]
pub struct OpenApiReadResponse {
    pub service_id: String,
    pub operation_id: String,
    pub status: u16,
    pub response_sha256: String,
    pub response_bytes: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl OpenApiReadRequest {
    /// Perform exactly one pinned, read-only request. Response bytes are
    /// hashed then discarded; callers never receive raw service content.
    pub fn fetch_metadata(&self) -> Result<OpenApiReadResponse> {
        let url = reqwest::Url::parse(&self.url).context("parse resolved OpenAPI request URL")?;
        let host = url
            .host_str()
            .context("resolved OpenAPI request has no host")?;
        let port = url
            .port_or_known_default()
            .context("resolved OpenAPI request has no port")?;
        let addresses = (host, port)
            .to_socket_addrs()
            .context("resolve OpenAPI service host")?
            .map(|address| SocketAddr::new(address.ip(), 0))
            .collect::<Vec<_>>();
        ensure!(
            !addresses.is_empty(),
            "OpenAPI service host resolved no addresses"
        );
        let credential = std::env::var(&self.credential_env).with_context(|| {
            format!(
                "OpenAPI credential environment variable {} is not set",
                self.credential_env
            )
        })?;
        ensure!(
            !credential.is_empty() && credential.len() <= 16 * 1024,
            "OpenAPI credential environment variable has an invalid length"
        );
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .resolve_to_addrs(host, &addresses)
            .build()
            .context("build pinned OpenAPI service client")?;
        let credential_header =
            reqwest::header::HeaderName::from_bytes(self.credential_header.as_bytes())
                .context("parse OpenAPI credential header")?;
        let credential_value = reqwest::header::HeaderValue::from_str(&format!(
            "{}{}",
            self.credential_prefix, credential
        ))
        .context("construct OpenAPI credential header")?;
        let request = match self.method {
            "GET" => client.get(url),
            "HEAD" => client.head(url),
            _ => bail!("resolved OpenAPI operation is not read-only"),
        }
        .header(credential_header, credential_value)
        .header(reqwest::header::USER_AGENT, &self.request_identity);
        let mut response = request.send().context("send OpenAPI service request")?;
        let status = response.status();
        ensure!(
            status.is_success(),
            "OpenAPI service returned HTTP {status}"
        );
        ensure!(
            response
                .content_length()
                .is_none_or(|length| length <= self.max_response_bytes),
            "OpenAPI service response exceeds byte cap"
        );
        let etag = header_value(response.headers(), reqwest::header::ETAG);
        let last_modified = header_value(response.headers(), reqwest::header::LAST_MODIFIED);
        let mut body = Vec::new();
        response
            .by_ref()
            .take(self.max_response_bytes.saturating_add(1))
            .read_to_end(&mut body)
            .context("read OpenAPI service response")?;
        ensure!(
            body.len() as u64 <= self.max_response_bytes,
            "OpenAPI service response exceeds byte cap"
        );
        Ok(OpenApiReadResponse {
            service_id: self.service_id.clone(),
            operation_id: self.operation_id.clone(),
            status: status.as_u16(),
            response_sha256: hex::encode(Sha256::digest(&body)),
            response_bytes: body.len() as u64,
            etag,
            last_modified,
        })
    }
}

fn header_value(
    headers: &reqwest::header::HeaderMap,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 512 && !value.chars().any(char::is_control))
        .map(str::to_owned)
}

impl OpenApiServiceManifest {
    pub fn from_path(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("inspect OpenAPI service manifest {}", path.display()))?;
        ensure!(
            metadata.file_type().is_file()
                && !metadata.file_type().is_symlink()
                && metadata.len() <= MAX_MANIFEST_BYTES,
            "OpenAPI service manifest must be a bounded regular file"
        );
        let bytes = fs::read(path)?;
        let manifest =
            serde_json::from_slice::<Self>(&bytes).context("parse OpenAPI service manifest")?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn resolve_read(&self, spec: &[u8], operation_id: &str) -> Result<OpenApiReadRequest> {
        self.validate()?;
        ensure!(
            spec.len() <= MAX_SPEC_BYTES,
            "OpenAPI specification exceeds byte limit"
        );
        ensure!(
            hex::encode(Sha256::digest(spec)) == self.spec_sha256,
            "OpenAPI specification digest does not match the manifest"
        );
        ensure!(
            self.allowed_operations
                .iter()
                .any(|allowed| allowed == operation_id),
            "OpenAPI operation is not allowlisted by the manifest"
        );
        let document: serde_json::Value =
            serde_json::from_slice(spec).context("parse OpenAPI JSON specification")?;
        ensure!(
            document
                .get("openapi")
                .and_then(serde_json::Value::as_str)
                .is_some(),
            "OpenAPI specification has no openapi version"
        );
        let paths = document
            .get("paths")
            .and_then(serde_json::Value::as_object)
            .context("OpenAPI specification has no paths object")?;
        let mut selected = None;
        for (path, operations) in paths {
            let Some(operations) = operations.as_object() else {
                continue;
            };
            for (method, operation) in operations {
                if operation
                    .get("operationId")
                    .and_then(serde_json::Value::as_str)
                    != Some(operation_id)
                {
                    continue;
                }
                let method = match method.as_str() {
                    "get" => "GET",
                    "head" => "HEAD",
                    _ => bail!("OpenAPI allowlisted operation must use GET or HEAD"),
                };
                ensure!(
                    path.starts_with('/')
                        && !path.contains(['?', '#', '{', '}'])
                        && path.len() <= 2_048,
                    "OpenAPI operation path must be a bounded literal absolute path"
                );
                ensure!(
                    selected.replace((method, path.as_str())).is_none(),
                    "OpenAPI operation ID is ambiguous"
                );
            }
        }
        let (method, path) =
            selected.context("OpenAPI operation ID was not found in the pinned specification")?;
        let mut url = reqwest::Url::parse(&self.base_url).expect("validated manifest endpoint");
        let base = url.path().trim_end_matches('/');
        url.set_path(&format!("{base}{path}"));
        url.set_query(None);
        url.set_fragment(None);
        Ok(OpenApiReadRequest {
            service_id: self.id.clone(),
            operation_id: operation_id.to_owned(),
            method,
            url: url.to_string(),
            credential_env: self.credential_env.clone(),
            credential_header: self.credential_header.clone(),
            credential_prefix: self.credential_prefix.clone(),
            request_identity: self.request_identity.clone(),
            max_response_bytes: self.max_response_bytes,
        })
    }

    pub fn validate_spec(&self, spec: &[u8]) -> Result<()> {
        for operation in &self.allowed_operations {
            self.resolve_read(spec, operation)?;
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == VERSION,
            "unsupported OpenAPI service manifest version"
        );
        validate_id(&self.id, "service id")?;
        validate_host(&self.allowed_host)?;
        let endpoint =
            reqwest::Url::parse(&self.base_url).context("parse OpenAPI service base_url")?;
        ensure!(
            endpoint.scheme() == "https"
                && endpoint.username().is_empty()
                && endpoint.password().is_none()
                && endpoint.query().is_none()
                && endpoint.fragment().is_none()
                && endpoint.host_str() == Some(self.allowed_host.as_str()),
            "OpenAPI service base_url must be HTTPS, credential-free, and match allowed_host"
        );
        ensure!(
            is_sha256(&self.spec_sha256),
            "OpenAPI service spec_sha256 is invalid"
        );
        validate_env_name(&self.credential_env)?;
        ensure!(
            reqwest::header::HeaderName::from_bytes(self.credential_header.as_bytes()).is_ok(),
            "OpenAPI service credential_header is invalid"
        );
        validate_metadata(&self.credential_prefix, "credential_prefix", 128)?;
        validate_metadata(&self.request_identity, "request_identity", 128)?;
        ensure!(
            !self.request_identity.is_empty(),
            "OpenAPI service request_identity is invalid"
        );
        ensure!(
            !self.allowed_operations.is_empty() && self.allowed_operations.len() <= 128,
            "OpenAPI service must allowlist one to 128 operations"
        );
        let mut operations = BTreeSet::new();
        for operation in &self.allowed_operations {
            validate_id(operation, "allowed operation")?;
            ensure!(
                operations.insert(operation),
                "OpenAPI service allowlisted operation is duplicated"
            );
        }
        ensure!(
            (1..=16 * 1024 * 1024).contains(&self.max_response_bytes)
                && self.requests_per_minute > 0
                && self.requests_per_minute <= 10_000
                && (1..=1_024).contains(&self.max_pages)
                && self.pagination == Pagination::None,
            "OpenAPI service limits or pagination policy are invalid"
        );
        Ok(())
    }
}

fn validate_id(value: &str, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .enumerate()
                .all(|(index, byte)| byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || (index != 0 && matches!(byte, b'-' | b'_'))),
        "OpenAPI service {label} must be a lowercase identifier"
    );
    Ok(())
}

fn validate_host(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 253
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')),
        "OpenAPI service allowed_host is invalid"
    );
    Ok(())
}

fn validate_env_name(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value.bytes().enumerate().all(|(index, byte)| byte == b'_'
                || byte.is_ascii_uppercase()
                || (index != 0 && byte.is_ascii_digit())),
        "OpenAPI service credential_env is invalid"
    );
    Ok(())
}

fn validate_metadata(value: &str, label: &str, maximum: usize) -> Result<()> {
    ensure!(
        value.len() <= maximum && !value.chars().any(char::is_control),
        "OpenAPI service {label} is invalid"
    );
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Vec<u8> {
        br#"{"openapi":"3.1.0","paths":{"/v1/inventory":{"get":{"operationId":"list-inventory"},"post":{"operationId":"mutate-inventory"}},"/v1/health":{"head":{"operationId":"health"}}}}"#.to_vec()
    }

    fn manifest(spec: &[u8]) -> OpenApiServiceManifest {
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "id": "internal-inventory",
            "base_url": "https://inventory.example.test/api",
            "allowed_host": "inventory.example.test",
            "spec_sha256": hex::encode(Sha256::digest(spec)),
            "credential_env": "INTERNAL_INVENTORY_TOKEN",
            "credential_header": "Authorization",
            "credential_prefix": "Bearer ",
            "request_identity": "graphoxide-catalog-wiki",
            "allowed_operations": ["list-inventory", "health"],
            "max_response_bytes": 1048576,
            "requests_per_minute": 60,
            "max_pages": 1,
            "pagination": "none"
        }))
        .expect("manifest")
    }

    #[test]
    fn pinned_spec_resolves_only_allowlisted_read_operations() {
        let spec = spec();
        let mut manifest = manifest(&spec);
        let request = manifest
            .resolve_read(&spec, "list-inventory")
            .expect("GET request");
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.url,
            "https://inventory.example.test/api/v1/inventory"
        );
        assert!(manifest.resolve_read(&spec, "mutate-inventory").is_err());
        manifest.allowed_operations = vec!["mutate-inventory".into()];
        assert!(manifest.resolve_read(&spec, "mutate-inventory").is_err());
    }

    #[test]
    fn manifest_rejects_unpinned_or_non_https_contracts() {
        let spec = spec();
        let mut manifest = manifest(&spec);
        manifest.spec_sha256 = "a".repeat(64);
        assert!(manifest.resolve_read(&spec, "health").is_err());
        let invalid = serde_json::json!({
            "version": 1, "id": "unsafe", "base_url": "http://inventory.example.test",
            "allowed_host": "inventory.example.test", "spec_sha256": hex::encode(Sha256::digest(&spec)),
            "credential_env": "TOKEN", "credential_header": "Authorization", "request_identity": "graphoxide",
            "allowed_operations": ["health"], "max_response_bytes": 1, "requests_per_minute": 1,
            "max_pages": 1, "pagination": "none"
        });
        let invalid: OpenApiServiceManifest =
            serde_json::from_value(invalid).expect("parse manifest");
        assert!(invalid.validate().is_err());
    }
}
