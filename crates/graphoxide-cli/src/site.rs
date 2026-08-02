//! Minimal, dependency-free static website preview server.

use std::{
    fs,
    io::{self, Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    path::{Component, Path, PathBuf},
    time::Duration,
};

const MAX_REQUEST_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Method {
    Get,
    Head,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Status {
    BadRequest,
    Forbidden,
    NotFound,
    MethodNotAllowed,
}

impl Status {
    fn line(self) -> &'static str {
        match self {
            Self::BadRequest => "400 Bad Request",
            Self::Forbidden => "403 Forbidden",
            Self::NotFound => "404 Not Found",
            Self::MethodNotAllowed => "405 Method Not Allowed",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::BadRequest => "Bad request\n",
            Self::Forbidden => "Forbidden\n",
            Self::NotFound => "Not found\n",
            Self::MethodNotAllowed => "Method not allowed\n",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct Request {
    method: Method,
    target: String,
}

pub fn serve(site_root: &Path, port: u16) -> anyhow::Result<()> {
    let root = site_root.canonicalize().map_err(|error| {
        anyhow::anyhow!(
            "cannot open website directory {}: {error}",
            site_root.display()
        )
    })?;
    if !root.is_dir() {
        anyhow::bail!("website path is not a directory: {}", root.display());
    }

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
    let address = listener.local_addr()?;
    println!("Serving {} at http://{address}", root.display());
    println!("Press Ctrl-C to stop.");
    io::stdout().flush()?;

    for connection in listener.incoming() {
        match connection {
            Ok(mut stream) => {
                if let Err(error) = handle_connection(&mut stream, &root) {
                    eprintln!("[graphoxide site] connection error: {error}");
                }
            }
            Err(error) => eprintln!("[graphoxide site] accept error: {error}"),
        }
    }
    Ok(())
}

fn handle_connection(stream: &mut TcpStream, root: &Path) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    let request = match read_request(stream).and_then(|raw| parse_request(&raw)) {
        Ok(request) => request,
        Err(status) => return write_error(stream, status, false),
    };
    let head_only = request.method == Method::Head;
    let path = match resolve_path(root, &request.target) {
        Ok(path) => path,
        Err(status) => return write_error(stream, status, head_only),
    };
    let body = match fs::read(&path) {
        Ok(body) => body,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return write_error(stream, Status::NotFound, head_only);
        }
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            return write_error(stream, Status::Forbidden, head_only);
        }
        Err(error) => return Err(error),
    };

    write_response(stream, "200 OK", mime_type(&path), &body, head_only, None)
}

fn read_request(stream: &mut TcpStream) -> Result<Vec<u8>, Status> {
    let mut request = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        let count = stream.read(&mut chunk).map_err(|_| Status::BadRequest)?;
        if count == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..count]);
        if request.len() > MAX_REQUEST_BYTES {
            return Err(Status::BadRequest);
        }
        if request.windows(4).any(|bytes| bytes == b"\r\n\r\n")
            || request.windows(2).any(|bytes| bytes == b"\n\n")
        {
            break;
        }
    }
    if request.is_empty()
        || (!request.windows(4).any(|bytes| bytes == b"\r\n\r\n")
            && !request.windows(2).any(|bytes| bytes == b"\n\n"))
    {
        return Err(Status::BadRequest);
    }
    Ok(request)
}

fn parse_request(raw: &[u8]) -> Result<Request, Status> {
    let text = std::str::from_utf8(raw).map_err(|_| Status::BadRequest)?;
    let header = text
        .split_once("\r\n\r\n")
        .map(|(header, _)| header)
        .or_else(|| text.split_once("\n\n").map(|(header, _)| header))
        .ok_or(Status::BadRequest)?;
    let mut lines = header.lines();
    let mut parts = lines.next().ok_or(Status::BadRequest)?.split_whitespace();
    let method = match parts.next().ok_or(Status::BadRequest)? {
        "GET" => Method::Get,
        "HEAD" => Method::Head,
        value
            if !value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte == b'-') =>
        {
            return Err(Status::MethodNotAllowed);
        }
        _ => return Err(Status::BadRequest),
    };
    let target = parts.next().ok_or(Status::BadRequest)?;
    let version = parts.next().ok_or(Status::BadRequest)?;
    if parts.next().is_some()
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || !target.starts_with('/')
        || target.starts_with("//")
        || target.contains('#')
    {
        return Err(Status::BadRequest);
    }
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line.starts_with([' ', '\t']) || !line.contains(':') {
            return Err(Status::BadRequest);
        }
    }
    Ok(Request {
        method,
        target: target.to_owned(),
    })
}

fn resolve_path(root: &Path, request_target: &str) -> Result<PathBuf, Status> {
    let encoded_path = request_target
        .split_once('?')
        .map_or(request_target, |v| v.0);
    let decoded = percent_decode(encoded_path)?;
    if decoded.contains(['\\', '\0', '\r', '\n']) {
        return Err(Status::Forbidden);
    }

    let relative = decoded.strip_prefix('/').ok_or(Status::BadRequest)?;
    let relative_path = Path::new(relative);
    for component in relative_path.components() {
        match component {
            Component::Normal(segment) => {
                let segment = segment.to_str().ok_or(Status::BadRequest)?;
                if segment.starts_with('.') && segment != ".nojekyll" {
                    return Err(Status::Forbidden);
                }
            }
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(Status::Forbidden);
            }
        }
    }

    let mut candidate = if relative.is_empty() {
        root.join("index.html")
    } else {
        root.join(relative_path)
    };
    if candidate.is_dir() {
        candidate = candidate.join("index.html");
    }
    let canonical = candidate
        .canonicalize()
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => Status::NotFound,
            io::ErrorKind::PermissionDenied => Status::Forbidden,
            _ => Status::BadRequest,
        })?;
    if !canonical.starts_with(root) {
        return Err(Status::Forbidden);
    }
    if !canonical.is_file() {
        return Err(Status::NotFound);
    }
    Ok(canonical)
}

fn percent_decode(value: &str) -> Result<String, Status> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(Status::BadRequest);
            }
            let high = hex_value(bytes[index + 1]).ok_or(Status::BadRequest)?;
            let low = hex_value(bytes[index + 2]).ok_or(Status::BadRequest)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| Status::BadRequest)
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn mime_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn write_error(stream: &mut TcpStream, status: Status, head_only: bool) -> io::Result<()> {
    let extra = (status == Status::MethodNotAllowed).then_some("Allow: GET, HEAD\r\n");
    write_response(
        stream,
        status.line(),
        "text/plain; charset=utf-8",
        status.message().as_bytes(),
        head_only,
        extra,
    )
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
    head_only: bool,
    extra_header: Option<&str>,
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nX-Content-Type-Options: nosniff\r\n{}Connection: close\r\n\r\n",
        body.len(),
        extra_header.unwrap_or("")
    )?;
    if !head_only {
        stream.write_all(body)?;
    }
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "graphoxide-site-test-{}-{nonce}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&root).expect("fixture directory");
            fs::create_dir(root.join("assets")).expect("assets directory");
            fs::write(root.join("index.html"), "home").expect("index");
            fs::write(root.join("assets/app.js"), "export {};").expect("javascript");
            fs::write(root.join(".nojekyll"), "").expect("nojekyll");
            Self {
                root: root.canonicalize().expect("canonical fixture"),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).expect("remove fixture");
        }
    }

    #[test]
    fn parses_get_and_head_but_rejects_other_methods() {
        let get = parse_request(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        assert_eq!(get.method, Method::Get);
        assert_eq!(get.target, "/");
        let head = parse_request(b"HEAD /app.js HTTP/1.0\r\n\r\n").unwrap();
        assert_eq!(head.method, Method::Head);
        assert_eq!(
            parse_request(b"POST / HTTP/1.1\r\nHost: localhost\r\n\r\n"),
            Err(Status::MethodNotAllowed)
        );
        assert_eq!(
            parse_request(b"get / HTTP/1.1\r\n\r\n"),
            Err(Status::BadRequest)
        );
    }

    #[test]
    fn resolves_root_assets_queries_and_nojekyll() {
        let fixture = Fixture::new();
        assert_eq!(
            resolve_path(&fixture.root, "/").unwrap(),
            fixture.root.join("index.html")
        );
        assert_eq!(
            resolve_path(&fixture.root, "/assets/app.js?v=1").unwrap(),
            fixture.root.join("assets/app.js")
        );
        assert_eq!(
            resolve_path(&fixture.root, "/.nojekyll").unwrap(),
            fixture.root.join(".nojekyll")
        );
        assert_eq!(
            resolve_path(&fixture.root, "/missing.html"),
            Err(Status::NotFound)
        );
    }

    #[test]
    fn rejects_plain_encoded_and_backslash_traversal() {
        let fixture = Fixture::new();
        for target in [
            "/../secret",
            "/%2e%2e/secret",
            "/%2E%2E%2Fsecret",
            "/assets/%2e%2e/index.html",
            "/..%5csecret",
            "/.git/config",
        ] {
            assert_eq!(resolve_path(&fixture.root, target), Err(Status::Forbidden));
        }
        assert_eq!(resolve_path(&fixture.root, "/%zz"), Err(Status::BadRequest));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_that_escape_the_site_root() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let outside = fixture
            .root
            .parent()
            .unwrap()
            .join(format!("graphoxide-site-outside-{}", std::process::id()));
        fs::write(&outside, "secret").expect("outside fixture");
        symlink(&outside, fixture.root.join("escape.txt")).expect("symlink");
        assert_eq!(
            resolve_path(&fixture.root, "/escape.txt"),
            Err(Status::Forbidden)
        );
        fs::remove_file(outside).expect("remove outside fixture");
    }

    #[test]
    fn chooses_browser_safe_mime_types() {
        assert_eq!(
            mime_type(Path::new("index.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            mime_type(Path::new("styles.css")),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            mime_type(Path::new("app.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(mime_type(Path::new("favicon.svg")), "image/svg+xml");
        assert_eq!(
            mime_type(Path::new(".nojekyll")),
            "application/octet-stream"
        );
    }
}
