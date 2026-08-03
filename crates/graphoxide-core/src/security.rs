//! Input validation helpers derived from upstream Graphify's `security.py`.

use serde_json::{Map, Value};
use std::{
    io::Read,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    time::Duration,
};

pub const MAX_FETCH_BYTES: usize = 50 * 1024 * 1024;
pub const MAX_TEXT_BYTES: usize = 10 * 1024 * 1024;
pub const METADATA_MAX_VALUE_LEN: usize = 512;
pub const METADATA_MAX_LIST_ITEMS: usize = 50;
const MAX_REDIRECTS: usize = 10;

/// Validate an external URL before fetching it. Only HTTP(S) is accepted, and
/// every resolved address must be public. DNS failures are rejected rather than
/// deferred to the HTTP client so callers get the same pre-flight guarantee as
/// upstream Graphify.
pub fn validate_url(url: &str) -> anyhow::Result<String> {
    let parsed = parse_external_url(url)?;
    resolve_and_validate(&parsed)?;
    Ok(url.to_owned())
}

fn parse_external_url(url: &str) -> anyhow::Result<reqwest::Url> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| anyhow::anyhow!("invalid URL {url:?}: {error}"))?;
    let scheme = parsed.scheme().to_ascii_lowercase();
    anyhow::ensure!(
        matches!(scheme.as_str(), "http" | "https"),
        "Blocked URL scheme '{scheme}' - only http and https are allowed. Got: {url:?}"
    );
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("URL has no host: {url:?}"))?;
    anyhow::ensure!(
        !matches!(
            host.to_ascii_lowercase().as_str(),
            "metadata.google.internal" | "metadata.google.com"
        ),
        "Blocked cloud metadata endpoint '{host}'. Got: {url:?}"
    );
    if let Ok(ip) = host.parse::<IpAddr>() {
        anyhow::ensure!(
            !ip_is_blocked(ip),
            "Blocked private/internal IP {ip}. Got: {url:?}"
        );
    }
    Ok(parsed)
}

fn resolve_and_validate(url: &reqwest::Url) -> anyhow::Result<SocketAddr> {
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("URL has no host: {url:?}"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("URL has no usable port: {url:?}"))?;

    let addresses: Vec<_> = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        (host, port)
            .to_socket_addrs()
            .map_err(|error| {
                anyhow::anyhow!("DNS resolution failed for '{host}': {error}. Got: {url:?}")
            })?
            .collect()
    };
    anyhow::ensure!(
        !addresses.is_empty(),
        "DNS resolution failed for '{host}': no addresses returned. Got: {url:?}"
    );
    for address in &addresses {
        anyhow::ensure!(
            !ip_is_blocked(address.ip()),
            "Blocked private/internal IP {} (resolved from '{host}'). Got: {url:?}",
            address.ip()
        );
    }
    Ok(addresses[0])
}

fn ip_is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ipv4_is_blocked(ip),
        IpAddr::V6(ip) => ipv6_is_blocked(ip),
    }
}

fn ipv4_is_blocked(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..=127).contains(&b))
        || (a == 198 && matches!(b, 18 | 19))
        || a >= 240
}

fn ipv6_is_blocked(ip: Ipv6Addr) -> bool {
    if let Some(embedded) = ip.to_ipv4_mapped() {
        return ipv4_is_blocked(embedded);
    }

    let octets = ip.octets();
    // RFC 6052 NAT64 well-known prefix: apply the policy to its embedded IPv4.
    if octets[..12] == [0x00, 0x64, 0xff, 0x9b, 0, 0, 0, 0, 0, 0, 0, 0] {
        return ipv4_is_blocked(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ));
    }

    let segments = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || octets[..12] == [0; 12] // deprecated IPv4-compatible form
        || segments[0] == 0x2002 // 6to4 transition space
        || (segments[0] == 0x2001 && segments[1] == 0) // Teredo
        || (segments[0] == 0x2001 && segments[1] == 2 && segments[2] == 0) // benchmarking
        || (segments[0] == 0x2001 && (segments[1] & 0xfff0) == 0x0010) // ORCHIDv1
        || (segments[0] == 0x2001 && segments[1] == 0x0db8) // documentation
        || (segments[0] == 0x3fff && (segments[1] & 0xf000) == 0) // documentation
        || segments[0] == 0x5f00 // segment-routing SIDs
        || (segments[0] & 0xffc0) == 0xfec0 // deprecated site-local
        || (segments[0] == 0x0100 && segments[1..4] == [0, 0, 0]) // discard-only
}

/// Fetch a bounded HTTP(S) response body.
pub fn safe_fetch(url: &str, max_bytes: usize, timeout: Duration) -> anyhow::Result<Vec<u8>> {
    let mut current = parse_external_url(url)?;
    for redirects in 0..=MAX_REDIRECTS {
        let address = resolve_and_validate(&current)?;
        let host = current
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("URL has no host: {current:?}"))?;
        // Disable automatic redirects so every hop gets a freshly validated,
        // DNS-pinned client. `resolve` keeps the original hostname for HTTP Host
        // and TLS SNI while connecting to exactly the address checked above.
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .resolve(host, address)
            .user_agent("Mozilla/5.0 graphoxide/1.0")
            .build()?;
        let mut response = client.get(current.clone()).send()?;
        if is_redirect(response.status().as_u16()) {
            anyhow::ensure!(
                redirects < MAX_REDIRECTS,
                "too many redirects while fetching {url:?}"
            );
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "HTTP {} redirect from {} has no Location header",
                        response.status(),
                        current
                    )
                })?
                .to_str()
                .map_err(|error| anyhow::anyhow!("invalid redirect Location: {error}"))?;
            current = current.join(location).map_err(|error| {
                anyhow::anyhow!("invalid redirect target {location:?} from {current}: {error}")
            })?;
            parse_external_url(current.as_str())?;
            continue;
        }
        ensure_success_status(response.status().as_u16(), current.as_str())?;
        return read_limited(&mut response, max_bytes, current.as_str());
    }
    unreachable!("redirect loop either returns or errors at the configured cap")
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Validate the HTTP status independently of response streaming.
pub fn ensure_success_status(status: u16, url: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        (200..300).contains(&status),
        "HTTP {status} returned by {url}"
    );
    Ok(())
}

/// Fetch bounded text and decode UTF-8 with replacement for malformed bytes.
pub fn safe_fetch_text(url: &str, max_bytes: usize, timeout: Duration) -> anyhow::Result<String> {
    Ok(decode_utf8_lossy(&safe_fetch(url, max_bytes, timeout)?))
}

pub fn decode_utf8_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Read a streaming response without ever retaining more than `max_bytes`.
pub fn read_limited(
    reader: &mut impl Read,
    max_bytes: usize,
    source: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut result = Vec::new();
    let mut chunk = [0_u8; 65_536];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        anyhow::ensure!(
            result.len().saturating_add(read) <= max_bytes,
            "Response from {source:?} exceeds size limit ({max_bytes} bytes)"
        );
        result.extend_from_slice(&chunk[..read]);
    }
    Ok(result)
}

/// Resolve a graph path and ensure it remains inside its allowed output base.
pub fn validate_graph_path(path: impl AsRef<Path>, base: Option<&Path>) -> anyhow::Result<PathBuf> {
    validate_graph_path_with_output_name(path.as_ref(), base, &configured_output_name())
}

fn configured_output_name() -> String {
    std::env::var("GRAPHOXIDE_OUT")
        .ok()
        .or_else(|| std::env::var("GRAPHIFY_OUT").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "graphoxide-out".into())
}

/// Explicit output-name form, useful to embedders with a non-default directory.
pub fn validate_graph_path_with_output_name(
    path: &Path,
    base: Option<&Path>,
    output_name: &str,
) -> anyhow::Result<PathBuf> {
    let base = match base {
        Some(base) => base.to_path_buf(),
        None => {
            let absolute_hint = absolute_path(path)?;
            let expected_name = Path::new(output_name)
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new(output_name));
            absolute_hint
                .ancestors()
                .find(|candidate| candidate.file_name() == Some(expected_name))
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(output_name))
        }
    };
    anyhow::ensure!(
        base.exists(),
        "Graph base directory does not exist: {}. Run graphoxide first to build the graph",
        base.display()
    );
    let base = base.canonicalize()?;
    let lexical = canonicalize_with_missing_tail(&normalize_lexical(&absolute_path(path)?))?;
    anyhow::ensure!(
        lexical.starts_with(&base),
        "Path {:?} escapes the allowed directory {}. Only paths inside the output directory are permitted",
        path,
        base.display()
    );
    let resolved = path.canonicalize().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!("Graph file not found: {}", path.display())
        } else {
            error.into()
        }
    })?;
    anyhow::ensure!(
        resolved.starts_with(&base),
        "Path {:?} escapes the allowed directory {}. Only paths inside the output directory are permitted",
        path,
        base.display()
    );
    Ok(resolved)
}

fn normalize_lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

fn canonicalize_with_missing_tail(path: &Path) -> std::io::Result<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut tail = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(|name| name.to_owned()) else {
            return Ok(path.to_path_buf());
        };
        tail.push(name);
        if !existing.pop() {
            return Ok(path.to_path_buf());
        }
    }
    let mut canonical = existing.canonicalize()?;
    for component in tail.into_iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

fn absolute_path(path: &Path) -> std::io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// Strip ASCII control characters and cap at 256 Unicode scalar values.
/// HTML escaping belongs at the point of direct HTML injection.
pub fn sanitize_label(label: &str) -> String {
    sanitize_optional_label(Some(label))
}

/// Typed equivalent of upstream's `sanitize_label(None) == ""` behavior.
pub fn sanitize_optional_label(label: Option<&str>) -> String {
    label
        .unwrap_or_default()
        .chars()
        .filter(|ch| !matches!(*ch as u32, 0x00..=0x1f | 0x7f))
        .take(256)
        .collect()
}

/// Convert a value to a bounded, control-free, HTML-safe string.
pub fn sanitize_metadata_string(value: impl ToString) -> String {
    let clean: String = value
        .to_string()
        .chars()
        .filter(|ch| !matches!(*ch as u32, 0x00..=0x1f | 0x7f))
        .collect();
    let escaped = html_escape(&clean);
    escaped.chars().take(METADATA_MAX_VALUE_LEN).collect()
}

fn html_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#x27;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// Recursively sanitize a JSON-compatible metadata value.
pub fn sanitize_metadata_value(value: &Value) -> Value {
    match value {
        Value::String(value) => sanitize_metadata_string(value).into(),
        Value::Object(value) => Value::Object(sanitize_metadata(Some(value))),
        Value::Array(value) => Value::Array(
            value
                .iter()
                .take(METADATA_MAX_LIST_ITEMS)
                .map(sanitize_metadata_value)
                .collect(),
        ),
        Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
    }
}

/// Sanitize metadata keys and values before graph export.
pub fn sanitize_metadata(metadata: Option<&Map<String, Value>>) -> Map<String, Value> {
    let mut result = Map::new();
    for (key, value) in metadata.into_iter().flatten() {
        let key = sanitize_metadata_string(key);
        if !key.is_empty() {
            result.insert(key, sanitize_metadata_value(value));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{ip_is_blocked, sanitize_label, validate_url};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn strips_only_upstream_control_range_and_caps() {
        assert_eq!(sanitize_label("a\n\tb\u{0085}c\u{007f}"), "ab\u{0085}c");
        assert_eq!(sanitize_label(&"界".repeat(300)).chars().count(), 256);
    }

    #[test]
    fn dns_hostnames_resolving_to_loopback_are_blocked() {
        let error = validate_url("http://localhost/").expect_err("loopback DNS must be blocked");
        assert!(error.to_string().contains("private/internal IP"));
    }

    #[test]
    fn private_transition_and_special_purpose_addresses_are_blocked() {
        for ip in [
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(198, 18, 0, 1)),
            IpAddr::V6("::ffff:127.0.0.1".parse::<Ipv6Addr>().unwrap()),
            IpAddr::V6("64:ff9b::10.0.0.1".parse::<Ipv6Addr>().unwrap()),
            IpAddr::V6("::8.8.8.8".parse::<Ipv6Addr>().unwrap()),
            IpAddr::V6("2001:2::1".parse::<Ipv6Addr>().unwrap()),
            IpAddr::V6("2001:db8::1".parse::<Ipv6Addr>().unwrap()),
            IpAddr::V6("2002:0808:0808::1".parse::<Ipv6Addr>().unwrap()),
            IpAddr::V6("3fff::1".parse::<Ipv6Addr>().unwrap()),
            IpAddr::V6("5f00::1".parse::<Ipv6Addr>().unwrap()),
        ] {
            assert!(ip_is_blocked(ip), "{ip} should be blocked");
        }
    }

    #[test]
    fn public_nat64_address_is_not_mistaken_for_private_ipv6() {
        assert!(!ip_is_blocked(IpAddr::V6(
            "64:ff9b::8.8.8.8".parse().unwrap()
        )));
    }
}
