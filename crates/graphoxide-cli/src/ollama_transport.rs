//! Pinned, proxy-free Ollama HTTP transport shared by local-source features.

use anyhow::{bail, Context as _, Result};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde_json::{json, Value};
use std::{
    io::Read,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    time::Duration,
};

pub const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434/v1";
pub const DEFAULT_OLLAMA_NATIVE_URL: &str = "http://localhost:11434";
const DEFAULT_MODEL: &str = "qwen2.5-coder:7b";
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_MODEL_PLAIN_TEXT_BYTES: usize = 64 * 1024;
pub(crate) const MARKDOWN_CONTEXT_TOKENS: usize = 131_072;
pub(crate) const MARKDOWN_COMPLETION_TOKENS: usize = 512;
pub(crate) const MARKDOWN_CHAT_TEMPLATE_RESERVE: usize = 1_024;
pub(crate) const MARKDOWN_SYSTEM_PROMPT: &str = "Write only a dense explanatory Markdown body of at most roughly 400 words for the requested wiki page. Do not add frontmatter, a title, a draft marker, or a Sources section.";
pub(crate) const MARKDOWN_RETRY_INSTRUCTION: &str = "Your immediately preceding answer was rejected because it violated the required Markdown body-only contract. Write only a dense explanatory Markdown body of at most roughly 400 words for the requested wiki page. Do not add frontmatter, a title, a draft marker, or a Sources section.";
pub(crate) const MARKDOWN_USER_PROMPT_BYTES: usize = MARKDOWN_CONTEXT_TOKENS
    - MARKDOWN_COMPLETION_TOKENS
    - MARKDOWN_CHAT_TEMPLATE_RESERVE
    - MARKDOWN_SYSTEM_PROMPT.len()
    - MARKDOWN_RETRY_INSTRUCTION.len();
const JSON_COMPLETION_TOKENS: usize = 1_024;

#[derive(Debug)]
pub struct OllamaTransport {
    client: reqwest::blocking::Client,
    endpoint: reqwest::Url,
    model: String,
    key: Option<String>,
    timeout: Duration,
    warning: Option<String>,
    labeling_compatible: bool,
    native: bool,
}

impl OllamaTransport {
    pub(crate) fn endpoint_key(&self) -> &str {
        self.endpoint.as_str()
    }

    pub fn local(base_url: &str, model: &str) -> Result<Self> {
        Self::local_with_resolver(base_url, model, |host, port| {
            (host, port)
                .to_socket_addrs()
                .map(|addresses| addresses.map(|address| address.ip()).collect())
                .unwrap_or_default()
        })
    }

    pub fn local_with_resolver<F>(base_url: &str, model: &str, resolver: F) -> Result<Self>
    where
        F: FnOnce(&str, u16) -> Vec<IpAddr>,
    {
        Self::build(
            base_url,
            model,
            None,
            Duration::from_secs(600),
            true,
            false,
            resolver,
        )
    }

    /// Use Ollama's native `/api/chat` endpoint for local wiki work.
    pub fn local_native(base_url: &str, model: &str) -> Result<Self> {
        Self::local_native_with_resolver(base_url, model, |host, port| {
            (host, port)
                .to_socket_addrs()
                .map(|addresses| addresses.map(|address| address.ip()).collect())
                .unwrap_or_default()
        })
    }

    fn local_native_with_resolver<F>(base_url: &str, model: &str, resolver: F) -> Result<Self>
    where
        F: FnOnce(&str, u16) -> Vec<IpAddr>,
    {
        Self::build(
            base_url,
            model,
            None,
            Duration::from_secs(600),
            true,
            true,
            resolver,
        )
    }

    /// Preserve the existing Ollama labeling environment and LAN behavior.
    pub fn for_labeling(
        requested_model: Option<&str>,
        timeout_seconds: Option<f64>,
    ) -> Result<Self> {
        let base = std::env::var("GRAPHOXIDE_LLM_BASE_URL")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                std::env::var("OLLAMA_BASE_URL")
                    .ok()
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_else(|| DEFAULT_OLLAMA_URL.into());
        let model = requested_model
            .map(str::to_owned)
            .or_else(|| std::env::var("OLLAMA_MODEL").ok())
            .or_else(|| std::env::var("GRAPHOXIDE_MODEL").ok())
            .unwrap_or_else(|| DEFAULT_MODEL.into());
        let key = std::env::var("OLLAMA_API_KEY")
            .ok()
            .filter(|value| !value.is_empty());
        let timeout = label_timeout(timeout_seconds)?;
        Self::build(&base, &model, key, timeout, false, false, |host, port| {
            (host, port)
                .to_socket_addrs()
                .map(|addresses| addresses.map(|address| address.ip()).collect())
                .unwrap_or_default()
        })
    }

    fn build<F>(
        base_url: &str,
        model: &str,
        key: Option<String>,
        timeout: Duration,
        loopback_only: bool,
        native: bool,
        resolver: F,
    ) -> Result<Self>
    where
        F: FnOnce(&str, u16) -> Vec<IpAddr>,
    {
        if loopback_only {
            validate_model(model)?;
        }
        let plan = graphoxide_extract::llm::plan_ollama_connection_with_resolver(
            base_url, false, resolver,
        )?;
        let loopback = plan.resolved_addresses.iter().all(IpAddr::is_loopback);
        if loopback_only && !loopback {
            bail!("wiki drafts require every resolved Ollama address to be loopback");
        }
        let mut base = reqwest::Url::parse(base_url).context("parse Ollama base URL")?;
        if let Ok(address) = plan.canonical_host.parse::<IpAddr>() {
            base.set_ip_host(address)
                .map_err(|()| anyhow::anyhow!("Ollama base URL has an invalid IP host"))?;
        } else {
            base.set_host(Some(&plan.canonical_host))
                .context("Ollama base URL has an invalid host")?;
        }
        let suffix = if native {
            "api/chat"
        } else {
            "chat/completions"
        };
        if !base.path().trim_end_matches('/').ends_with(suffix) {
            let path = format!("{}/{suffix}", base.path().trim_end_matches('/'));
            base.set_path(&path);
        }
        let addresses = plan
            .resolved_addresses
            .iter()
            .copied()
            .map(|address| SocketAddr::new(address, 0))
            .collect::<Vec<_>>();
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .resolve_to_addrs(&plan.canonical_host, &addresses)
            .build()
            .context("build pinned Ollama client")?;
        let warning = (!loopback_only && base.scheme() == "http" && !loopback).then(|| {
            let host = graphoxide_core::sanitize_label(&plan.canonical_host);
            format!(
                "Ollama labeling sends graph-derived labels and any configured API key to {:?} over plaintext HTTP",
                host
            )
        });
        Ok(Self {
            client,
            endpoint: base,
            model: model.into(),
            key,
            timeout,
            warning,
            labeling_compatible: !loopback_only,
            native,
        })
    }

    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }

    pub fn complete_markdown(&self, prompt: &str) -> Result<String> {
        anyhow::ensure!(
            prompt.len() <= MARKDOWN_USER_PROMPT_BYTES,
            "wiki model prompt exceeds its prompt byte cap"
        );
        let first = self.normalize_markdown_body(self.complete(
            json!([
                {"role": "system", "content": MARKDOWN_SYSTEM_PROMPT},
                {"role": "user", "content": prompt}
            ]),
            MARKDOWN_COMPLETION_TOKENS,
            false,
        )?)?;
        if Self::validate_markdown_body(&first).is_ok() {
            return Ok(first);
        }
        let retry = self.normalize_markdown_body(self.complete(
            json!([
                {"role": "system", "content": MARKDOWN_SYSTEM_PROMPT},
                {"role": "user", "content": prompt},
                {"role": "user", "content": MARKDOWN_RETRY_INSTRUCTION}
            ]),
            MARKDOWN_COMPLETION_TOKENS,
            false,
        )?)?;
        Self::validate_markdown_body(&retry)?;
        Ok(retry)
    }

    /// Complete a caller-defined, bounded JSON-object contract over the same
    /// loopback-only transport used for local wiki drafting.
    pub fn complete_json_object(&self, system: &str, prompt: &str) -> Result<Value> {
        anyhow::ensure!(
            !system.is_empty()
                && system.len().saturating_add(prompt.len()) <= MARKDOWN_USER_PROMPT_BYTES,
            "wiki model JSON prompt exceeds its prompt byte cap"
        );
        let body = self.complete(
            json!([
                {"role": "system", "content": system},
                {"role": "user", "content": prompt}
            ]),
            JSON_COMPLETION_TOKENS,
            true,
        )?;
        let value: Value = serde_json::from_str(&body).context("Ollama returned invalid JSON")?;
        anyhow::ensure!(
            value.is_object(),
            "Ollama returned a JSON value instead of an object"
        );
        Ok(value)
    }

    pub fn complete_json_object_with_image(
        &self,
        system: &str,
        prompt: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<Value> {
        anyhow::ensure!(
            matches!(media_type, "image/jpeg" | "image/png"),
            "Ollama enrichment image format is unsupported"
        );
        anyhow::ensure!(
            !system.is_empty()
                && system
                    .len()
                    .saturating_add(prompt.len())
                    .saturating_add(bytes.len())
                    <= MARKDOWN_USER_PROMPT_BYTES,
            "Ollama enrichment image exceeds its prompt byte cap"
        );
        let body = self.complete(
            json!([
                {"role": "system", "content": system},
                {"role": "user", "content": prompt, "images": [BASE64_STANDARD.encode(bytes)]},
            ]),
            JSON_COMPLETION_TOKENS,
            true,
        )?;
        let value: Value = serde_json::from_str(&body).context("Ollama returned invalid JSON")?;
        anyhow::ensure!(
            value.is_object(),
            "Ollama returned a JSON value instead of an object"
        );
        Ok(value)
    }

    pub fn call_label(
        &self,
        request: &graphoxide_graph::LabelRequest,
    ) -> Result<graphoxide_graph::LabelResponse> {
        anyhow::ensure!(
            !self.native,
            "native Ollama transport does not support graph labeling"
        );
        let model = request.model.as_deref().unwrap_or(&self.model);
        let response = self.request_value(
            json!({
                "model": model,
                "max_tokens": request.max_tokens,
                "temperature": 0,
                "messages": [{"role": "user", "content": request.prompt}],
            }),
            None,
            "label",
        )?;
        let content = response
            .pointer("/choices/0/message/content")
            .or_else(|| response.pointer("/content/0/text"))
            .and_then(Value::as_str)
            .context("label endpoint returned no message content")?;
        Ok(graphoxide_graph::LabelResponse {
            content: content.into(),
            usage: graphoxide_graph::LabelUsage {
                input: response
                    .pointer("/usage/prompt_tokens")
                    .or_else(|| response.pointer("/usage/input_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                output: response
                    .pointer("/usage/completion_tokens")
                    .or_else(|| response.pointer("/usage/output_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            },
        })
    }

    fn complete(&self, messages: Value, max_tokens: usize, json_object: bool) -> Result<String> {
        let body = if self.native {
            let mut body = json!({
                "model": self.model,
                "stream": false,
                "think": false,
                "options": {
                    "temperature": 0,
                    "num_ctx": MARKDOWN_CONTEXT_TOKENS,
                    "num_predict": max_tokens,
                },
                "messages": messages,
            });
            if json_object {
                body["format"] = Value::String("json".into());
            }
            body
        } else {
            json!({
                "model": self.model,
                "max_tokens": max_tokens,
                "temperature": 0,
                "think": false,
                "options": {"num_ctx": MARKDOWN_CONTEXT_TOKENS},
                "messages": messages,
            })
        };
        let response = self.request_value(body, Some(MAX_RESPONSE_BYTES), "Ollama")?;
        let body = response
            .pointer("/choices/0/message/content")
            .or_else(|| response.pointer("/message/content"))
            .and_then(Value::as_str)
            .context("Ollama returned no Markdown body")?;
        let body = body.replace("\r\n", "\n").replace('\r', "\n");
        Ok(body)
    }

    fn normalize_markdown_body(&self, body: String) -> Result<String> {
        Ok(normalize_wiki_markdown_body(body))
    }

    fn validate_markdown_body(body: &str) -> Result<()> {
        validate_wiki_markdown_body(body)
            .map_err(|error| anyhow::anyhow!("Ollama returned an invalid Markdown body: {error}"))
    }
}

pub(crate) fn normalize_wiki_markdown_body(body: String) -> String {
    let body = project_model_source_text(&body);
    project_model_markdown(&body)
}

pub(crate) fn validate_wiki_markdown_body(body: &str) -> Result<()> {
    if body.trim().is_empty()
        || body.trim_start().starts_with("---\n")
        || body.contains("<!-- graphoxide-draft -->")
        || body
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        bail!("body is empty or contains reserved markup");
    }
    if let Err(error) = validate_untrusted_model_markdown_body(body) {
        bail!("body violates the wiki Markdown contract: {error}");
    }
    Ok(())
}

fn project_model_source_text(input: &str) -> String {
    // ponytail: This bounded lexical projection is deliberately not a markup parser; add one only when measured corpora require structure-aware text preservation.
    let mut output = String::with_capacity(input.len());
    let mut offset = 0;
    while offset < input.len() {
        let remaining = &input[offset..];
        let url_prefix = ["https://", "http://"].into_iter().find(|prefix| {
            remaining
                .get(..prefix.len())
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
        });
        if url_prefix.is_some() {
            output.push_str("external reference");
            offset += remaining
                .char_indices()
                .skip(1)
                .find(|(_, character)| {
                    character.is_whitespace()
                        || matches!(character, ')' | ']' | '>' | '\"' | '\'' | '`')
                })
                .map_or(remaining.len(), |(index, _)| index);
            continue;
        }
        let character = remaining.chars().next().expect("non-empty remainder");
        if matches!(character, '[' | ']' | '<' | '>') {
            output.push(' ');
        } else {
            output.push(character);
        }
        offset += character.len_utf8();
    }
    if output.len() > MAX_MODEL_PLAIN_TEXT_BYTES {
        let mut end = MAX_MODEL_PLAIN_TEXT_BYTES;
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        output.truncate(end);
    }
    output
}

fn validate_untrusted_model_markdown_body(body: &str) -> Result<()> {
    let active = active_markdown(body);
    let unsafe_angle = active.char_indices().any(|(index, character)| {
        character == '<'
            && active[index + 1..]
                .chars()
                .next()
                .is_some_and(|next| next.is_ascii_alphabetic() || matches!(next, '/' | '!' | '?'))
    });
    anyhow::ensure!(
        inline_link_destinations(&active).is_empty()
            && !has_reference_link(&active)
            && !has_reference_definition(&active)
            && !unsafe_angle
            && !has_unsafe_model_heading(&active),
        "invalid model Markdown body: links and raw HTML are not allowed"
    );
    Ok(())
}

fn active_markdown(text: &str) -> String {
    let mut active = String::with_capacity(text.len());
    let mut fence = None;
    for chunk in text.split_inclusive('\n') {
        let newline = chunk.ends_with('\n');
        let line = chunk.strip_suffix('\n').unwrap_or(chunk);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some((marker, minimum)) = fence {
            if markdown_fence(line).is_some_and(|(current, count, suffix)| {
                current == marker && count >= minimum && suffix.trim().is_empty()
            }) {
                fence = None;
            }
        } else if let Some((marker, count, _)) = markdown_fence(line) {
            fence = Some((marker, count));
        } else {
            active.push_str(&without_inline_code(line));
        }
        if newline {
            active.push('\n');
        }
    }
    active
}

fn closing_delimiter(text: &str, opening: usize, open: u8, close: u8) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 1usize;
    let mut cursor = opening.checked_add(1)?;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor = cursor.saturating_add(2),
            byte if byte == open => {
                depth = depth.checked_add(1)?;
                cursor += 1;
            }
            byte if byte == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(cursor);
                }
                cursor += 1;
            }
            _ => cursor += 1,
        }
    }
    None
}

fn inline_link_destinations(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut destinations = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' {
            cursor = cursor.saturating_add(2);
            continue;
        }
        if bytes[cursor] != b'[' {
            cursor += 1;
            continue;
        }
        let Some(label_end) = closing_delimiter(text, cursor, b'[', b']') else {
            cursor += 1;
            continue;
        };
        let mut destination_start = label_end + 1;
        while destination_start < bytes.len() && bytes[destination_start].is_ascii_whitespace() {
            destination_start += 1;
        }
        if bytes.get(destination_start) != Some(&b'(') {
            cursor += 1;
            continue;
        }
        let Some(destination_end) = closing_delimiter(text, destination_start, b'(', b')') else {
            cursor += 1;
            continue;
        };
        let contents = text[destination_start + 1..destination_end].trim();
        let destination = if let Some(angle) = contents.strip_prefix('<') {
            angle.split_once('>').map_or(angle, |(value, _)| value)
        } else {
            contents.split_ascii_whitespace().next().unwrap_or("")
        };
        destinations.push(destination.to_owned());
        cursor = destination_end + 1;
    }
    destinations
}

fn has_reference_link(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' {
            cursor = cursor.saturating_add(2);
            continue;
        }
        if bytes[cursor] != b'[' {
            cursor += 1;
            continue;
        }
        let Some(label_end) = closing_delimiter(text, cursor, b'[', b']') else {
            cursor += 1;
            continue;
        };
        let mut reference_start = label_end + 1;
        while reference_start < bytes.len() && bytes[reference_start].is_ascii_whitespace() {
            reference_start += 1;
        }
        if bytes.get(reference_start) == Some(&b'[')
            && closing_delimiter(text, reference_start, b'[', b']').is_some()
        {
            return true;
        }
        cursor += 1;
    }
    false
}

fn has_reference_definition(text: &str) -> bool {
    text.lines().any(|line| {
        let indent = line.bytes().take_while(|byte| *byte == b' ').count();
        if indent > 3 || line.as_bytes().get(indent) != Some(&b'[') {
            return false;
        }
        closing_delimiter(line, indent, b'[', b']')
            .and_then(|closing| line.as_bytes().get(closing + 1))
            == Some(&b':')
    })
}

fn has_unsafe_model_heading(active: &str) -> bool {
    let mut previous = None;
    for line in active.lines() {
        if atx_heading(line)
            .is_some_and(|(level, title)| level == 1 || title.eq_ignore_ascii_case("sources"))
            || previous.is_some_and(|title| {
                setext_heading(title, line).is_some_and(|(level, title)| {
                    level == 1 || title.eq_ignore_ascii_case("sources")
                })
            })
        {
            return true;
        }
        previous = Some(line);
    }
    false
}

impl OllamaTransport {
    fn request_value(&self, body: Value, cap: Option<usize>, label: &str) -> Result<Value> {
        let mut request = self.client.post(self.endpoint.clone()).json(&body);
        if let Some(key) = &self.key {
            request = request.bearer_auth(key);
        }
        let mut response = request.send().map_err(|error| {
            if error.is_timeout() {
                if self.labeling_compatible {
                    anyhow::anyhow!(
                        "label request to {} timed out after {}s; local models may need more time, so increase --timeout-seconds (or GRAPHOXIDE_LLM_TIMEOUT_SECONDS)",
                        self.endpoint,
                        self.timeout.as_secs_f64()
                    )
                } else {
                    anyhow::anyhow!(
                        "Ollama request to {} timed out after {}s",
                        self.endpoint,
                        self.timeout.as_secs_f64()
                    )
                }
            } else if self.labeling_compatible {
                error.into()
            } else {
                anyhow::anyhow!("Ollama request failed: {error}")
            }
        })?;
        if !response.status().is_success() {
            bail!("{label} endpoint returned HTTP {}", response.status());
        }
        let Some(cap) = cap else {
            return response
                .json()
                .context("label endpoint returned invalid JSON");
        };
        if response
            .content_length()
            .is_some_and(|length| length > cap as u64)
        {
            bail!("Ollama response exceeds the byte cap");
        }
        let mut bytes = Vec::new();
        response
            .by_ref()
            .take((cap as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .context("read Ollama response")?;
        if bytes.len() > cap {
            bail!("Ollama response exceeds the byte cap");
        }
        serde_json::from_slice(&bytes).context("Ollama response is not valid JSON")
    }
}

fn project_model_markdown(input: &str) -> String {
    input
        .split_inclusive('\n')
        .map(project_model_line)
        .collect()
}

fn project_model_line(line: &str) -> String {
    let (line, ending) = line
        .strip_suffix('\n')
        .map_or((line, ""), |line| (line, "\n"));
    let line = line.trim_end_matches([' ', '\t']);
    if is_setext_marker(line) {
        return ending.into();
    }
    let line = strip_atx_markers(line);
    let mut output = String::with_capacity(line.len() + ending.len());
    let mut offset = 0;
    while offset < line.len() {
        let remaining = &line[offset..];
        if let Some(length) = model_url_length(remaining) {
            output.push_str("external reference");
            offset += length;
            continue;
        }
        let character = remaining.chars().next().expect("non-empty remainder");
        output.push(character);
        offset += character.len_utf8();
    }
    output.push_str(ending);
    output
}

fn strip_atx_markers(line: &str) -> &str {
    let indentation = line
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == b' ')
        .count();
    if indentation > 3 {
        return line;
    }
    let hashes = line.as_bytes()[indentation..]
        .iter()
        .take_while(|byte| **byte == b'#')
        .count();
    if !(1..=6).contains(&hashes) {
        return line;
    }
    let rest = &line[indentation + hashes..];
    if !rest.chars().next().is_none_or(char::is_whitespace) {
        return line;
    }
    let rest = rest.trim();
    let closing = rest
        .as_bytes()
        .iter()
        .rev()
        .take_while(|byte| **byte == b'#')
        .count();
    if closing != 0 {
        let before = &rest[..rest.len() - closing];
        if before.ends_with(char::is_whitespace) {
            before.trim_end()
        } else {
            rest
        }
    } else {
        rest
    }
}

fn is_setext_marker(line: &str) -> bool {
    let indentation = line
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == b' ')
        .count();
    if indentation > 3 {
        return false;
    }
    let Some(marker) = line.as_bytes()[indentation..].first().copied() else {
        return false;
    };
    matches!(marker, b'=' | b'-')
        && line[indentation..]
            .bytes()
            .all(|byte| byte == marker || byte == b' ')
}

fn model_url_length(input: &str) -> Option<usize> {
    let token_end = input
        .char_indices()
        .find(|(_, character)| {
            character.is_whitespace() || matches!(character, ')' | ']' | '>' | '"' | '\'' | '`')
        })
        .map_or(input.len(), |(index, _)| index);
    let token = input[..token_end].trim_end_matches(['.', ',', ';', ':', '!', '?']);
    if token.is_empty() {
        return None;
    }
    if [
        "https://",
        "http://",
        "ftp://",
        "smb://",
        "file://",
        "mailto:",
        "javascript:",
    ]
    .into_iter()
    .any(|prefix| {
        token
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    }) || is_bare_url(token)
    {
        Some(token.len())
    } else {
        None
    }
}

fn is_bare_url(token: &str) -> bool {
    let locator = token.find(['/', '?', '#']);
    if locator.is_none()
        && !token
            .get(..4)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("www."))
    {
        return false;
    }
    let host = locator.map_or(token, |index| &token[..index]);
    let mut labels = host.split('.');
    let Some(first) = labels.next() else {
        return false;
    };
    if first.is_empty()
        || !first
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return false;
    }
    let labels = labels.collect::<Vec<_>>();
    let Some(tld) = labels.last() else {
        return false;
    };
    labels.iter().all(|label| {
        !label.is_empty()
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }) && tld.len() >= 2
        && tld.bytes().all(|byte| byte.is_ascii_alphabetic())
}

fn without_inline_code(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut visible = String::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let Some(relative) = line[cursor..].find('`') else {
            visible.push_str(&line[cursor..]);
            break;
        };
        let opening = cursor + relative;
        visible.push_str(&line[cursor..opening]);
        let count = bytes[opening..]
            .iter()
            .take_while(|byte| **byte == b'`')
            .count();
        let mut search = opening + count;
        let mut closing = None;
        while search < bytes.len() {
            let Some(relative) = line[search..].find('`') else {
                break;
            };
            let candidate = search + relative;
            let candidate_count = bytes[candidate..]
                .iter()
                .take_while(|byte| **byte == b'`')
                .count();
            if candidate_count == count {
                closing = Some(candidate + count);
                break;
            }
            search = candidate + candidate_count;
        }
        if let Some(closing) = closing {
            cursor = closing;
        } else {
            visible.push_str(&line[opening..]);
            break;
        }
    }
    visible
}

fn markdown_fence(line: &str) -> Option<(u8, usize, &str)> {
    let indent = line
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == b' ')
        .count();
    if indent > 3 {
        return None;
    }
    let line = &line[indent..];
    let marker = *line.as_bytes().first()?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let count = line
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == marker)
        .count();
    (count >= 3).then_some((marker, count, &line[count..]))
}

fn atx_heading(line: &str) -> Option<(usize, &str)> {
    let indent = line
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == b' ')
        .count();
    if indent > 3 {
        return None;
    }
    let line = &line[indent..];
    let level = line
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == b'#')
        .count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &line[level..];
    if rest
        .chars()
        .next()
        .is_some_and(|character| !character.is_whitespace())
    {
        return None;
    }
    Some((level, rest.trim().trim_end_matches('#').trim_end()))
}

fn setext_heading<'a>(title: &'a str, underline: &str) -> Option<(usize, &'a str)> {
    let title = title.trim();
    if title.is_empty() {
        return None;
    }
    let indent = underline
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == b' ')
        .count();
    if indent > 3 {
        return None;
    }
    let underline = &underline[indent..];
    let marker = *underline.as_bytes().first()?;
    if !matches!(marker, b'=' | b'-') || underline.is_empty() {
        return None;
    }
    let marker_count = underline
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == marker)
        .count();
    underline[marker_count..]
        .trim()
        .is_empty()
        .then_some((usize::from(marker == b'-') + 1, title))
}

fn validate_model(model: &str) -> Result<()> {
    if model.is_empty()
        || model.len() > 256
        || model.trim() != model
        || model.chars().any(char::is_control)
    {
        bail!("--model contains an invalid value");
    }
    Ok(())
}

pub fn label_timeout(explicit: Option<f64>) -> Result<Duration> {
    let (source, seconds) = if let Some(seconds) = explicit {
        ("--timeout-seconds", seconds)
    } else if let Ok(value) = std::env::var("GRAPHOXIDE_LLM_TIMEOUT_SECONDS") {
        (
            "GRAPHOXIDE_LLM_TIMEOUT_SECONDS",
            value.parse::<f64>().map_err(|error| {
                anyhow::anyhow!("GRAPHOXIDE_LLM_TIMEOUT_SECONDS must be a number: {error}")
            })?,
        )
    } else if let Ok(value) = std::env::var("GRAPHIFY_API_TIMEOUT") {
        (
            "GRAPHIFY_API_TIMEOUT",
            value.parse::<f64>().map_err(|error| {
                anyhow::anyhow!("GRAPHIFY_API_TIMEOUT must be a number: {error}")
            })?,
        )
    } else {
        ("default", 600.0)
    };
    if !seconds.is_finite() || seconds <= 0.0 {
        bail!("{source} must be a finite number greater than zero");
    }
    Duration::try_from_secs_f64(seconds)
        .map_err(|error| anyhow::anyhow!("{source} is not a valid timeout: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead as _, BufReader, Write as _},
        net::TcpListener,
        thread,
    };

    #[test]
    fn normalized_model_markdown_removes_links_and_raw_markup() {
        let normalized = normalize_wiki_markdown_body(
            "# Heading\nSee [the <b>reference</b>](https://example.invalid/path).\n".into(),
        );

        assert!(normalized.contains("external reference"));
        assert!(!normalized.contains(['[', ']', '<', '>']));
        validate_wiki_markdown_body(&normalized).expect("normalized Markdown");
    }

    #[test]
    fn model_markdown_validation_rejects_executable_markup() {
        for body in [
            "[reference](https://example.invalid)",
            "<script>alert(1)</script>",
            "# Title",
            "Sources\n---",
        ] {
            assert!(
                validate_untrusted_model_markdown_body(body).is_err(),
                "{body}"
            );
        }
    }

    #[test]
    fn native_image_enrichment_uses_the_ollama_chat_contract() {
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
            let response = r#"{"message":{"content":"{\"summary\":\"image summary\"}"}}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
                        response.len()
                    )
                    .as_bytes(),
                )
                .expect("write provider response");
            (
                headers,
                String::from_utf8(body).expect("UTF-8 provider body"),
            )
        });

        let transport = OllamaTransport::local_native(&format!("http://{address}"), "test-model")
            .expect("build loopback native transport");
        let response = transport
            .complete_json_object_with_image(
                "Return JSON only.",
                "Summarize this image.",
                "image/png",
                b"\x89PNG\r\n\x1a\nfixture",
            )
            .expect("complete image enrichment");

        assert_eq!(response["summary"], "image summary");
        let (headers, body) = request.join().expect("join provider server");
        assert!(headers.starts_with("POST /api/chat HTTP/1.1\r\n"));
        let body: Value = serde_json::from_str(&body).expect("parse native provider request");
        assert_eq!(body["format"], "json");
        assert_eq!(body["messages"][1]["images"][0], "iVBORw0KGgpmaXh0dXJl");
    }
}
