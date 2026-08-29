//! Versioned, secret-free provider profiles for consent-gated wiki model work.

use crate::ollama_transport::{
    normalize_wiki_markdown_body, validate_wiki_markdown_body, OllamaTransport,
    MARKDOWN_COMPLETION_TOKENS, MARKDOWN_RETRY_INSTRUCTION, MARKDOWN_SYSTEM_PROMPT,
    MARKDOWN_USER_PROMPT_BYTES,
};
use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use std::{
    fs,
    io::Read,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    path::Path,
    time::Duration,
};

pub const PROFILE_VERSION: u32 = 1;
const MAX_PROFILE_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const JSON_COMPLETION_TOKENS: usize = 1_024;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderProtocol {
    OpenaiCompatible,
    AnthropicMessages,
    OllamaNative,
    McpAgent,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderProfile {
    pub version: u32,
    pub id: String,
    pub protocol: ProviderProtocol,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub source_egress_consent: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
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
        Self::from_json(&fs::read(path)?)
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
                    self.api_key_env.is_none(),
                    "ollama-native profiles may not declare an API credential reference"
                );
            }
            ProviderProtocol::McpAgent => {
                anyhow::ensure!(
                    self.endpoint.is_none()
                        && self.model.is_none()
                        && self.api_key_env.is_none()
                        && self.source_egress_consent.is_none(),
                    "mcp-agent profiles may not declare an endpoint, model, credential reference, or source egress consent"
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
        validate_model(self.model.as_deref().unwrap_or_default())?;
        validate_consent(self.source_egress_consent.as_deref().unwrap_or_default())?;
        if requires_key {
            validate_env_name(self.api_key_env.as_deref().unwrap_or_default())?;
        } else if let Some(name) = &self.api_key_env {
            validate_env_name(name)?;
        }
        Ok(())
    }
}

/// One consent-gated direct-model transport. Agent profiles intentionally have
/// no direct transport because they operate through the read-only MCP surface.
pub enum WikiModelTransport {
    Ollama(OllamaTransport),
    Http(HttpTransport),
}

impl WikiModelTransport {
    pub fn from_profile(profile: &ProviderProfile) -> Result<Self> {
        profile.validate()?;
        match profile.protocol {
            ProviderProtocol::OllamaNative => Ok(Self::Ollama(OllamaTransport::local_native(
                profile.endpoint.as_deref().expect("validated endpoint"),
                profile.model.as_deref().expect("validated model"),
            )?)),
            ProviderProtocol::OpenaiCompatible | ProviderProtocol::AnthropicMessages => {
                Ok(Self::Http(HttpTransport::new(profile)?))
            }
            ProviderProtocol::McpAgent => {
                bail!("mcp-agent profiles submit artifacts through MCP and cannot make direct model requests")
            }
        }
    }

    pub fn complete_json_object(&self, system: &str, prompt: &str) -> Result<Value> {
        match self {
            Self::Ollama(transport) => transport.complete_json_object(system, prompt),
            Self::Http(transport) => transport.complete_json_object(system, prompt),
        }
    }

    pub fn complete_markdown(&self, prompt: &str) -> Result<String> {
        match self {
            Self::Ollama(transport) => transport.complete_markdown(prompt),
            Self::Http(transport) => transport.complete_markdown(prompt),
        }
    }
}

pub struct HttpTransport {
    client: reqwest::blocking::Client,
    endpoint: reqwest::Url,
    protocol: ProviderProtocol,
    model: String,
    api_key: String,
}

impl HttpTransport {
    fn new(profile: &ProviderProfile) -> Result<Self> {
        let endpoint = request_endpoint(profile)?;
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
        let api_key_env = profile
            .api_key_env
            .as_deref()
            .expect("validated API key env");
        let api_key = std::env::var_os(api_key_env)
            .with_context(|| format!("credential environment variable {api_key_env} is not set"))?
            .into_string()
            .map_err(|_| anyhow::anyhow!("credential environment variable is not valid UTF-8"))?;
        anyhow::ensure!(
            !api_key.is_empty() && api_key.len() <= 16 * 1024,
            "credential environment variable has an invalid length"
        );
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .resolve_to_addrs(&host, &addresses)
            .build()
            .context("build pinned wiki provider client")?;
        Ok(Self {
            client,
            endpoint,
            protocol: profile.protocol.clone(),
            model: profile.model.clone().expect("validated model"),
            api_key,
        })
    }

    fn complete_json_object(&self, system: &str, prompt: &str) -> Result<Value> {
        anyhow::ensure!(
            !system.is_empty()
                && system.len().saturating_add(prompt.len()) <= MARKDOWN_USER_PROMPT_BYTES,
            "wiki model JSON prompt exceeds its prompt byte cap"
        );
        let body = self.complete(system, prompt, JSON_COMPLETION_TOKENS, true)?;
        let value: Value = serde_json::from_str(&body).context("provider returned invalid JSON")?;
        anyhow::ensure!(
            value.is_object(),
            "provider returned a JSON value instead of an object"
        );
        Ok(value)
    }

    fn complete_markdown(&self, prompt: &str) -> Result<String> {
        anyhow::ensure!(
            prompt.len() <= MARKDOWN_USER_PROMPT_BYTES,
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
            if matches!(status.as_u16(), 429 | 500..=599) && attempt == 0 {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            anyhow::ensure!(
                status.is_success(),
                "provider endpoint returned HTTP {status}"
            );
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
            return serde_json::from_slice(&bytes).context("provider response is not valid JSON");
        }
        unreachable!("bounded provider retry loop returns or errors")
    }
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

fn validate_env_name(value: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value.bytes().enumerate().all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_uppercase() || (index != 0 && byte.is_ascii_digit())
            }),
        "provider profile api_key_env must be an environment variable name"
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
    use super::*;

    fn direct_profile(protocol: &str) -> String {
        format!(
            r#"{{"version":1,"id":"internal-wiki","protocol":"{protocol}","endpoint":"https://wiki.example.test/v1","model":"model-v1","api_key_env":"INTERNAL_WIKI_KEY","source_egress_consent":"send-source-text-to-internal-wiki"}}"#
        )
    }

    #[test]
    fn direct_profiles_are_versioned_secret_free_and_digest_stable() {
        let profile = ProviderProfile::from_json(direct_profile("openai-compatible").as_bytes())
            .expect("valid profile");
        assert_eq!(profile.digest(), profile.digest());
        assert!(ProviderProfile::from_json(
            br#"{"version":1,"id":"internal-wiki","protocol":"openai-compatible","endpoint":"https://wiki.example.test/v1","model":"model-v1","api_key":"not-allowed","api_key_env":"INTERNAL_WIKI_KEY","source_egress_consent":"send-source-text-to-internal-wiki"}"#
        )
        .is_err());
    }

    #[test]
    fn direct_profiles_require_https_except_literal_loopback() {
        let http = direct_profile("anthropic-messages")
            .replace("https://wiki.example.test", "http://wiki.example.test");
        assert!(ProviderProfile::from_json(http.as_bytes()).is_err());
        assert!(ProviderProfile::from_json(
            br#"{"version":1,"id":"local-wiki","protocol":"ollama-native","endpoint":"http://127.0.0.1:11434","model":"local-model","source_egress_consent":"send-source-text-to-local-wiki"}"#
        )
        .is_ok());
    }

    #[test]
    fn mcp_agent_profile_has_no_direct_egress_configuration() {
        let profile = ProviderProfile::from_json(
            br#"{"version":1,"id":"codex-wiki","protocol":"mcp-agent","agent":"codex"}"#,
        )
        .expect("valid MCP agent profile");
        assert_eq!(profile.protocol, ProviderProtocol::McpAgent);
        assert!(WikiModelTransport::from_profile(&profile).is_err());
        assert!(ProviderProfile::from_json(
            br#"{"version":1,"id":"codex-wiki","protocol":"mcp-agent","agent":"codex","endpoint":"https://example.test"}"#
        )
        .is_err());
    }
}
