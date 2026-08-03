//! Streamable HTTP transport for the Graphoxide MCP server.

use super::GraphoxideServer;
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::Router,
    Json,
};
use rmcp::transport::streamable_http_server::{
    session::local::{LocalSessionManager, SessionConfig},
    StreamableHttpServerConfig, StreamableHttpService,
};
use serde_json::json;
use std::{net::IpAddr, path::PathBuf, sync::Arc, time::Duration};

pub const DEFAULT_MAX_CONTEXTS: usize = 8;

#[derive(Debug, Clone)]
pub struct HttpOptions {
    pub mount_path: String,
    pub api_key: Option<String>,
    pub stateless: bool,
    pub json_response: bool,
    pub session_timeout: Option<Duration>,
    pub max_project_contexts: usize,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            mount_path: "/mcp".into(),
            api_key: None,
            stateless: false,
            json_response: false,
            session_timeout: Some(SessionConfig::DEFAULT_KEEP_ALIVE),
            max_project_contexts: max_server_contexts_from_env(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Stdio,
    Http,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeArguments {
    pub graph_path: PathBuf,
    pub transport: Transport,
    pub host: IpAddr,
    pub port: u16,
    pub api_key: Option<String>,
    pub stateless: bool,
}

/// Parse the stand-alone `graphoxide-mcp` argument contract. Keeping this pure
/// makes CLI behavior testable without starting a server.
pub fn parse_serve_arguments<I, S>(
    args: I,
    environment_api_key: Option<&str>,
) -> anyhow::Result<ServeArguments>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut arguments = args.into_iter().map(Into::into);
    let graph_path = arguments
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("missing graph path"))?;
    let mut parsed = ServeArguments {
        graph_path,
        transport: Transport::Stdio,
        host: "127.0.0.1".parse().expect("valid loopback address"),
        port: 8080,
        api_key: environment_api_key
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(str::to_owned),
        stateless: false,
    };
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--transport" => {
                parsed.transport = match arguments.next().as_deref() {
                    Some("stdio") => Transport::Stdio,
                    Some("http") => Transport::Http,
                    Some(value) => anyhow::bail!("unsupported transport {value:?}"),
                    None => anyhow::bail!("--transport requires a value"),
                };
            }
            "--host" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--host requires a value"))?;
                parsed.host = value.parse()?;
            }
            "--port" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--port requires a value"))?;
                parsed.port = value.parse()?;
            }
            "--api-key" => {
                parsed.api_key = arguments
                    .next()
                    .map(|key| key.trim().to_owned())
                    .filter(|key| !key.is_empty());
            }
            "--stateless" => parsed.stateless = true,
            unknown => anyhow::bail!("unknown serve argument {unknown:?}"),
        }
    }
    Ok(parsed)
}

pub fn parse_max_server_contexts(value: Option<&str>) -> usize {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<isize>().ok())
        .map_or(DEFAULT_MAX_CONTEXTS, |value| value.max(1) as usize)
}

pub fn max_server_contexts_from_env() -> usize {
    let value = std::env::var("GRAPHOXIDE_MAX_CONTEXTS")
        .ok()
        .or_else(|| std::env::var("GRAPHIFY_MAX_CONTEXTS").ok());
    parse_max_server_contexts(value.as_deref())
}

#[derive(Clone)]
struct AuthState {
    api_key: Option<Arc<str>>,
    mount_path: Arc<str>,
    json_response: bool,
}

/// Build a fully wired MCP HTTP application suitable for embedding or tests.
pub fn build_http_app(
    graph_path: impl Into<PathBuf>,
    options: HttpOptions,
) -> anyhow::Result<Router> {
    validate_mount_path(&options.mount_path)?;
    let mount_path = options.mount_path.clone();
    let server = GraphoxideServer::with_default_graph(
        graph_path.into(),
        options.max_project_contexts.max(1),
    );
    let mut manager = LocalSessionManager::default();
    manager.session_config.keep_alive = if options.stateless {
        None
    } else {
        options.session_timeout.filter(|timeout| !timeout.is_zero())
    };
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(!options.stateless)
        .with_json_response(options.json_response);
    let service: StreamableHttpService<GraphoxideServer, LocalSessionManager> =
        StreamableHttpService::new(move || Ok(server.clone()), Arc::new(manager), config);
    let auth = AuthState {
        api_key: options
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(Arc::<str>::from),
        mount_path: Arc::<str>::from(mount_path.as_str()),
        json_response: options.json_response,
    };
    Ok(Router::new()
        .nest_service(&mount_path, service)
        .layer(middleware::from_fn_with_state(auth, require_api_key)))
}

/// Run the HTTP transport until the process is stopped.
pub fn serve_http(
    graph_path: impl Into<PathBuf>,
    host: IpAddr,
    port: u16,
    options: HttpOptions,
) -> anyhow::Result<()> {
    let graph_path = graph_path.into();
    tokio::runtime::Runtime::new()?.block_on(async move {
        let app = build_http_app(graph_path, options)?;
        let listener = tokio::net::TcpListener::bind((host, port)).await?;
        axum::serve(listener, app).await?;
        anyhow::Ok(())
    })
}

fn validate_mount_path(path: &str) -> anyhow::Result<()> {
    anyhow::ensure!(path.starts_with('/'), "MCP mount path must start with '/'");
    anyhow::ensure!(path.len() > 1, "MCP mount path may not be the server root");
    anyhow::ensure!(
        !path.contains("//"),
        "MCP mount path contains an empty segment"
    );
    anyhow::ensure!(
        !path.contains(['{', '}', '*']),
        "MCP mount path must be literal"
    );
    Ok(())
}

async fn require_api_key(
    State(state): State<AuthState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let applies = request.uri().path() == state.mount_path.as_ref()
        || request
            .uri()
            .path()
            .strip_prefix(state.mount_path.as_ref())
            .is_some_and(|suffix| suffix.starts_with('/'));
    if !applies || state.api_key.is_none() {
        let response = next.run(request).await;
        return normalize_json_response(response, applies && state.json_response).await;
    }
    let expected = state.api_key.as_deref().expect("checked above");
    if request_key(request.headers()).is_some_and(|provided| constant_time_eq(provided, expected)) {
        normalize_json_response(next.run(request).await, state.json_response).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"unauthorized"})),
        )
            .into_response()
    }
}

/// rmcp emits legacy stateful replies as finite SSE bodies. The inherited
/// Graphify API promises a plain JSON-RPC envelope when `json_response` is
/// requested, so collapse the finite POST event stream to its JSON event while
/// preserving status and session headers.
async fn normalize_json_response(response: Response, enabled: bool) -> Response {
    if !enabled
        || !response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"))
    {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, 8 * 1024 * 1024).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let stream = String::from_utf8_lossy(&bytes);
    let payload = stream.lines().find_map(|line| {
        line.strip_prefix("data:")
            .map(str::trim)
            .filter(|data| data.starts_with('{'))
    });
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    Response::from_parts(parts, Body::from(payload.unwrap_or_default().to_owned()))
}

fn request_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(value) = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
    {
        return Some(value);
    }
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())?;
    let (scheme, key) = authorization.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(key)
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::to_bytes,
        http::{Request as HttpRequest, Response as HttpResponse},
    };
    use serde_json::Value;
    use std::{fs, path::Path};
    use tower::ServiceExt;

    const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;

    fn graph_file(directory: &Path, nodes: usize, relative: &str) -> PathBuf {
        let path = directory.join(relative);
        fs::create_dir_all(path.parent().expect("graph parent")).expect("graph parent");
        let nodes: Vec<_> = (0..nodes)
            .map(
                |index| json!({"id":format!("n{index}"),"label":format!("N{index}"),"community":0}),
            )
            .collect();
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "directed":true,
                "nodes":nodes,
                "edges":[]
            }))
            .expect("serialize graph"),
        )
        .expect("write graph");
        path
    }

    fn sample_graph(directory: &Path) -> PathBuf {
        let path = directory.join("graph.json");
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "directed":true,
                "nodes":[
                    {"id":"a","label":"Alpha","community":0},
                    {"id":"b","label":"Beta","community":0}
                ],
                "edges":[
                    {"source":"a","target":"b","relation":"calls","confidence":"EXTRACTED"}
                ]
            }))
            .expect("serialize graph"),
        )
        .expect("write graph");
        path
    }

    fn request(path: &str, body: &str, headers: &[(&str, &str)]) -> HttpRequest<Body> {
        let mut builder = HttpRequest::builder()
            .method("POST")
            .uri(path)
            .header(header::HOST, "127.0.0.1")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::from(body.to_owned())).expect("request")
    }

    async fn json_body(response: HttpResponse<Body>) -> Value {
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("<missing>")
            .to_owned();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            panic!(
                "JSON response: {error}; status={status}; content-type={content_type}; body={:?}",
                String::from_utf8_lossy(&bytes)
            )
        })
    }

    async fn initialize(app: &Router, path: &str) -> (StatusCode, HeaderMap, Value) {
        let response = app
            .clone()
            .oneshot(request(path, INITIALIZE, &[]))
            .await
            .expect("initialize response");
        let status = response.status();
        let headers = response.headers().clone();
        let body = json_body(response).await;
        (status, headers, body)
    }

    async fn stateful_session(app: &Router) -> String {
        let (status, headers, _) = initialize(app, "/mcp").await;
        assert_eq!(status, StatusCode::OK);
        let session = headers
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .expect("stateful session ID")
            .to_owned();
        let notification = request(
            "/mcp",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            &[("mcp-session-id", &session)],
        );
        let response = app
            .clone()
            .oneshot(notification)
            .await
            .expect("initialized notification");
        assert!(response.status().is_success());
        session
    }

    async fn tool_call(
        app: &Router,
        session: &str,
        name: &str,
        arguments: Value,
        id: u64,
    ) -> String {
        let body = json!({
            "jsonrpc":"2.0",
            "id":id,
            "method":"tools/call",
            "params":{"name":name,"arguments":arguments}
        })
        .to_string();
        let response = app
            .clone()
            .oneshot(request("/mcp", &body, &[("mcp-session-id", session)]))
            .await
            .expect("tool response");
        assert_eq!(response.status(), StatusCode::OK);
        json_body(response).await["result"]["content"][0]["text"]
            .as_str()
            .expect("tool text")
            .to_owned()
    }

    #[tokio::test]
    async fn test_app_builds_and_initialize_succeeds() {
        let directory = tempfile::tempdir().expect("temp directory");
        let app = build_http_app(
            sample_graph(directory.path()),
            HttpOptions {
                json_response: true,
                ..Default::default()
            },
        )
        .expect("HTTP app");
        let (status, _, body) = initialize(&app, "/mcp").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["result"]["serverInfo"]["name"], "graphoxide");
    }

    #[tokio::test]
    async fn test_unknown_path_is_404() {
        let directory = tempfile::tempdir().expect("temp directory");
        let app = build_http_app(sample_graph(directory.path()), HttpOptions::default())
            .expect("HTTP app");
        let response = app
            .oneshot(request("/nope", INITIALIZE, &[]))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    fn authenticated_app(directory: &Path, key: &str) -> Router {
        build_http_app(
            sample_graph(directory),
            HttpOptions {
                api_key: Some(key.into()),
                json_response: true,
                ..Default::default()
            },
        )
        .expect("HTTP app")
    }

    #[tokio::test]
    async fn test_api_key_missing_is_401() {
        let directory = tempfile::tempdir().expect("temp directory");
        let response = authenticated_app(directory.path(), "s3cret")
            .oneshot(request("/mcp", INITIALIZE, &[]))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(json_body(response).await["error"], "unauthorized");
    }

    #[tokio::test]
    async fn test_api_key_wrong_is_401() {
        let directory = tempfile::tempdir().expect("temp directory");
        let response = authenticated_app(directory.path(), "s3cret")
            .oneshot(request(
                "/mcp",
                INITIALIZE,
                &[("authorization", "Bearer nope")],
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_api_key_bearer_ok() {
        let directory = tempfile::tempdir().expect("temp directory");
        let app = authenticated_app(directory.path(), "s3cret");
        let response = app
            .oneshot(request(
                "/mcp",
                INITIALIZE,
                &[("authorization", "Bearer s3cret")],
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            json_body(response).await["result"]["serverInfo"]["name"],
            "graphoxide"
        );
    }

    #[tokio::test]
    async fn test_api_key_x_api_key_header_ok() {
        let directory = tempfile::tempdir().expect("temp directory");
        let response = authenticated_app(directory.path(), "s3cret")
            .oneshot(request("/mcp", INITIALIZE, &[("x-api-key", "s3cret")]))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_blank_api_key_means_no_auth() {
        let directory = tempfile::tempdir().expect("temp directory");
        let response = authenticated_app(directory.path(), "   ")
            .oneshot(request("/mcp", INITIALIZE, &[]))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_api_key_bearer_scheme_case_insensitive() {
        let directory = tempfile::tempdir().expect("temp directory");
        let response = authenticated_app(directory.path(), "s3cret")
            .oneshot(request(
                "/mcp",
                INITIALIZE,
                &[("authorization", "bearer s3cret")],
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_custom_mount_path() {
        let directory = tempfile::tempdir().expect("temp directory");
        let app = build_http_app(
            sample_graph(directory.path()),
            HttpOptions {
                mount_path: "/graph".into(),
                json_response: true,
                ..Default::default()
            },
        )
        .expect("HTTP app");
        assert_eq!(initialize(&app, "/graph").await.0, StatusCode::OK);
        let missing = app
            .oneshot(request("/mcp", INITIALIZE, &[]))
            .await
            .expect("response");
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_tools_list_over_http() {
        let directory = tempfile::tempdir().expect("temp directory");
        let app = build_http_app(
            sample_graph(directory.path()),
            HttpOptions {
                json_response: true,
                ..Default::default()
            },
        )
        .expect("HTTP app");
        let session = stateful_session(&app).await;
        let response = app
            .clone()
            .oneshot(request(
                "/mcp",
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
                &[("mcp-session-id", &session)],
            ))
            .await
            .expect("tools response");
        let body = json_body(response).await;
        let names: std::collections::BTreeSet<_> = body["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        for name in ["query_graph", "get_node", "graph_stats"] {
            assert!(names.contains(name));
        }
    }

    #[tokio::test]
    async fn test_project_path_is_optional_on_every_tool() {
        let directory = tempfile::tempdir().expect("temp directory");
        let app = build_http_app(
            sample_graph(directory.path()),
            HttpOptions {
                json_response: true,
                ..Default::default()
            },
        )
        .expect("HTTP app");
        let session = stateful_session(&app).await;
        let response = app
            .oneshot(request(
                "/mcp",
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
                &[("mcp-session-id", &session)],
            ))
            .await
            .expect("tools response");
        let body = json_body(response).await;
        for tool in body["result"]["tools"].as_array().expect("tools") {
            let properties = tool["inputSchema"]["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{} missing properties", tool["name"]));
            assert!(
                properties.contains_key("project_path"),
                "{} missing project_path",
                tool["name"]
            );
            assert!(!tool["inputSchema"]["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|field| field == "project_path")));
        }
    }

    #[tokio::test]
    async fn test_project_path_routes_to_that_projects_graph() {
        let directory = tempfile::tempdir().expect("temp directory");
        let default = sample_graph(directory.path());
        let project = directory.path().join("project");
        graph_file(&project, 3, "graphify-out/graph.json");
        let app = build_http_app(
            default,
            HttpOptions {
                json_response: true,
                ..Default::default()
            },
        )
        .expect("HTTP app");
        let session = stateful_session(&app).await;
        assert!(tool_call(&app, &session, "graph_stats", json!({}), 2)
            .await
            .contains("Nodes: 2"));
        assert!(tool_call(
            &app,
            &session,
            "graph_stats",
            json!({"project_path":project}),
            3
        )
        .await
        .contains("Nodes: 3"));
        assert!(tool_call(&app, &session, "graph_stats", json!({}), 4)
            .await
            .contains("Nodes: 2"));
    }

    #[test]
    fn test_max_server_contexts_parsing_none_8() {
        assert_eq!(parse_max_server_contexts(None), 8);
    }

    #[test]
    fn test_max_server_contexts_parsing_blank_8() {
        assert_eq!(parse_max_server_contexts(Some("")), 8);
    }

    #[test]
    fn test_max_server_contexts_parsing_bad_8() {
        assert_eq!(parse_max_server_contexts(Some("bad")), 8);
    }

    #[test]
    fn test_max_server_contexts_parsing_zero_1() {
        assert_eq!(parse_max_server_contexts(Some("0")), 1);
    }

    #[test]
    fn test_max_server_contexts_parsing_negative_1() {
        assert_eq!(parse_max_server_contexts(Some("-4")), 1);
    }

    #[test]
    fn test_max_server_contexts_parsing_three_3() {
        assert_eq!(parse_max_server_contexts(Some("3")), 3);
    }

    #[test]
    fn test_project_context_cache_is_lru_and_pins_default_graph() {
        let directory = tempfile::tempdir().expect("temp directory");
        let default = sample_graph(directory.path());
        let project_paths: Vec<_> = (0..3)
            .map(|index| {
                let project = directory.path().join(format!("project-{index}"));
                graph_file(&project, index + 3, "graphify-out/graph.json");
                project
            })
            .collect();
        let server = GraphoxideServer::with_default_graph(default.clone(), 2);
        let default_first = server.load_graph(&default, true).expect("default graph");
        let first_path = server.graph_path(Some(project_paths[0].to_string_lossy().into()));
        let second_path = server.graph_path(Some(project_paths[1].to_string_lossy().into()));
        let third_path = server.graph_path(Some(project_paths[2].to_string_lossy().into()));
        let first = server
            .load_graph(&first_path, false)
            .expect("first project");
        let second = server
            .load_graph(&second_path, false)
            .expect("second project");
        let promoted = server
            .load_graph(&first_path, false)
            .expect("promote first");
        assert!(Arc::ptr_eq(&first, &promoted));
        server
            .load_graph(&third_path, false)
            .expect("third project");
        let reloaded_second = server
            .load_graph(&second_path, false)
            .expect("reload second");
        assert!(!Arc::ptr_eq(&second, &reloaded_second));
        let default_after = server.load_graph(&default, true).expect("default graph");
        assert!(Arc::ptr_eq(&default_first, &default_after));
    }

    #[tokio::test]
    async fn test_bad_project_path_errors_without_killing_server() {
        let directory = tempfile::tempdir().expect("temp directory");
        let app = build_http_app(
            sample_graph(directory.path()),
            HttpOptions {
                json_response: true,
                ..Default::default()
            },
        )
        .expect("HTTP app");
        let session = stateful_session(&app).await;
        let bad = tool_call(
            &app,
            &session,
            "graph_stats",
            json!({"project_path":directory.path().join("missing")}),
            2,
        )
        .await;
        assert!(bad.to_lowercase().contains("could not load graph.json"));
        assert!(tool_call(&app, &session, "graph_stats", json!({}), 3)
            .await
            .contains("Nodes: 2"));
    }

    #[tokio::test]
    async fn test_corrupt_project_graph_is_a_tool_error_without_killing_server() {
        let directory = tempfile::tempdir().expect("temp directory");
        let project = directory.path().join("project");
        let corrupt = graph_file(&project, 3, "graphify-out/graph.json");
        fs::write(corrupt, "{not json").expect("corrupt graph");
        let app = build_http_app(
            sample_graph(directory.path()),
            HttpOptions {
                json_response: true,
                ..Default::default()
            },
        )
        .expect("HTTP app");
        let session = stateful_session(&app).await;
        let bad = tool_call(
            &app,
            &session,
            "graph_stats",
            json!({"project_path":project}),
            2,
        )
        .await;
        assert!(bad.to_lowercase().contains("could not load graph.json"));
        assert!(tool_call(&app, &session, "graph_stats", json!({}), 3)
            .await
            .contains("Nodes: 2"));
    }

    #[tokio::test]
    async fn test_stateless_mode_initialize() {
        let directory = tempfile::tempdir().expect("temp directory");
        let app = build_http_app(
            sample_graph(directory.path()),
            HttpOptions {
                stateless: true,
                json_response: true,
                ..Default::default()
            },
        )
        .expect("HTTP app");
        assert_eq!(initialize(&app, "/mcp").await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn test_stateless_with_timeout_does_not_raise() {
        let directory = tempfile::tempdir().expect("temp directory");
        let app = build_http_app(
            sample_graph(directory.path()),
            HttpOptions {
                stateless: true,
                json_response: true,
                session_timeout: Some(Duration::from_secs(3600)),
                ..Default::default()
            },
        )
        .expect("HTTP app");
        assert_eq!(initialize(&app, "/mcp").await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn test_session_timeout_zero_disables() {
        let directory = tempfile::tempdir().expect("temp directory");
        let app = build_http_app(
            sample_graph(directory.path()),
            HttpOptions {
                json_response: true,
                session_timeout: Some(Duration::ZERO),
                ..Default::default()
            },
        )
        .expect("HTTP app");
        assert_eq!(initialize(&app, "/mcp").await.0, StatusCode::OK);
    }

    #[test]
    fn test_cli_defaults_to_stdio() {
        let parsed =
            parse_serve_arguments(["graphoxide-out/graph.json"], None).expect("serve arguments");
        assert_eq!(
            parsed.graph_path,
            PathBuf::from("graphoxide-out/graph.json")
        );
        assert_eq!(parsed.transport, Transport::Stdio);
    }

    #[test]
    fn test_cli_http_passes_flags() {
        let parsed = parse_serve_arguments(
            [
                "g.json",
                "--transport",
                "http",
                "--host",
                "0.0.0.0",
                "--port",
                "9000",
                "--api-key",
                "k",
                "--stateless",
            ],
            None,
        )
        .expect("serve arguments");
        assert_eq!(parsed.graph_path, PathBuf::from("g.json"));
        assert_eq!(parsed.transport, Transport::Http);
        assert_eq!(parsed.host, "0.0.0.0".parse::<IpAddr>().unwrap());
        assert_eq!(parsed.port, 9000);
        assert_eq!(parsed.api_key.as_deref(), Some("k"));
        assert!(parsed.stateless);
    }

    #[test]
    fn test_cli_api_key_from_env() {
        let parsed = parse_serve_arguments(["g.json", "--transport", "http"], Some("from-env"))
            .expect("serve arguments");
        assert_eq!(parsed.api_key.as_deref(), Some("from-env"));
    }
}
