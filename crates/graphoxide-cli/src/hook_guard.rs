//! Shell-agnostic coding-agent hook guard.
//!
//! The guard is deliberately fail-open: malformed host payloads, inaccessible
//! graph paths, and unknown modes produce no restriction. Gemini's `BeforeTool`
//! contract is the exception only in shape—it always emits an explicit `allow`.

use serde_json::{json, Map, Value};
use std::{
    env, fs,
    fs::{Metadata, OpenOptions},
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

pub const SOURCE_EXTENSIONS: &[&str] = &[
    ".py", ".js", ".cjs", ".ts", ".tsx", ".jsx", ".astro", ".vue", ".svelte", ".go", ".rs",
    ".java", ".rb", ".c", ".h", ".cpp", ".hpp", ".cc", ".cs", ".kt", ".swift", ".php", ".scala",
    ".lua", ".sh", ".md", ".rst", ".txt", ".mdx",
];

const SEARCH_NUDGE_TEXT: &str = concat!(
    "MANDATORY: graphoxide-out/graph.json exists. You MUST run ",
    "`graphoxide query \"<question>\"` before grepping raw files. Only grep ",
    "after graphoxide has oriented you, or to modify/debug specific lines."
);

const READ_NUDGE_TEXT: &str = concat!(
    "MANDATORY: graphoxide-out/graph.json exists. You MUST run graphoxide ",
    "before reading source files. Use: `graphoxide query \"<question>\"` ",
    "(scoped subgraph), `graphoxide explain \"<concept>\"`, or ",
    "`graphoxide path \"<A>\" \"<B>\"`. Only read raw files after graphoxide has ",
    "oriented you, or to modify/debug specific lines. This rule applies to ",
    "subagents too — include it in every subagent prompt involving code exploration."
);

const READ_NUDGE_STALE_TEXT: &str = concat!(
    "graphoxide-out/graph.json exists but may be STALE for this file (the file ",
    "changed after the last build). Prefer `graphoxide query \"<question>\"` for ",
    "orientation, and run `graphoxide update` to refresh the graph. Reading the ",
    "file directly is fine."
);

const READ_DENY_TEXT: &str = concat!(
    "graphoxide strict mode: this project has a fresh knowledge graph that covers ",
    "this file. Run `graphoxide query \"<your question>\"` (or `graphoxide explain` / ",
    "`graphoxide path`) FIRST to orient yourself, then re-issue this Read — it ",
    "will be allowed. This block fires at most once per session; reading raw ",
    "files to modify or debug specific lines is fine after one query. Apply the ",
    "same rule in any subagent prompt that explores code."
);

const GEMINI_NUDGE_TEXT: &str = concat!(
    "graphoxide: knowledge graph at graphoxide-out/. For focused questions, run ",
    "`graphoxide query \"<question>\"` (scoped subgraph, usually much smaller than ",
    "GRAPH_REPORT.md) instead of grepping raw files. Read GRAPH_REPORT.md only ",
    "for broad architecture context."
);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuardContext {
    pub project_root: PathBuf,
    pub output_directory: PathBuf,
}

impl GuardContext {
    pub fn new(project_root: impl Into<PathBuf>, output_directory: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
            output_directory: output_directory.into(),
        }
    }

    pub fn for_current_process() -> Self {
        let project_root = env::var_os("CLAUDE_PROJECT_DIR")
            .map(PathBuf::from)
            .or_else(|| env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let output_directory = env::var_os("GRAPHOXIDE_OUT")
            .or_else(|| env::var_os("GRAPHIFY_OUT"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("graphoxide-out"));
        Self::new(project_root, output_directory)
    }

    pub fn graph_path(&self) -> PathBuf {
        self.output_root().join("graph.json")
    }

    pub fn output_root(&self) -> PathBuf {
        if self.output_directory.is_absolute() {
            self.output_directory.clone()
        } else {
            self.project_root.join(&self.output_directory)
        }
    }

    fn output_name(&self) -> String {
        self.output_directory
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("graphoxide-out")
            .to_ascii_lowercase()
    }
}

/// Evaluate a hook payload with the real filesystem graph check.
pub fn evaluate(mode: &str, input: &[u8], context: &GuardContext) -> String {
    evaluate_strict(mode, input, context, false)
}

/// Evaluate a hook payload, optionally enabling the once-per-session strict
/// read redirect. Environment overrides remain authoritative so an installed
/// strict hook can be disabled without reinstalling it.
pub fn evaluate_strict(mode: &str, input: &[u8], context: &GuardContext, strict: bool) -> String {
    if mode == "gemini" {
        return gemini_output(
            fs::metadata(context.graph_path())
                .map(|metadata| metadata.is_file())
                .unwrap_or(false),
        );
    }
    if !matches!(mode, "search" | "read") {
        return String::new();
    }
    let Some(document_value) = parse_document(input) else {
        return String::new();
    };
    let Some(document) = document_value.as_object() else {
        return String::new();
    };
    let Some(tool) = payload_tool(document) else {
        return String::new();
    };

    if mode == "search" {
        if !is_search(tool)
            || !fs::metadata(context.graph_path())
                .map(|metadata| metadata.is_file())
                .unwrap_or(false)
        {
            return String::new();
        }
        return nudge_output(SEARCH_NUDGE_TEXT);
    }

    if !is_source_read(tool, context) || !is_in_project(tool, context) {
        return String::new();
    }
    let Ok(graph_metadata) = fs::metadata(context.graph_path()) else {
        return String::new();
    };
    if !graph_metadata.is_file() {
        return String::new();
    }
    if target_is_stale(tool, context, &graph_metadata) {
        return nudge_output(READ_NUDGE_STALE_TEXT);
    }

    let tool_name_is_read = match document.get("tool_name") {
        None | Some(Value::Null) => true,
        Some(Value::String(name)) => name == "Read",
        Some(_) => false,
    };
    let file_path = tool
        .get("file_path")
        .map(json_scalar_text)
        .unwrap_or_default();
    let session_id = document
        .get("session_id")
        .map(json_scalar_text)
        .unwrap_or_default();
    if strict_enabled(strict)
        && tool_name_is_read
        && !query_stamp_fresh(context)
        && target_is_indexed(&file_path, context)
        && mark_session_denied(&session_id, context)
    {
        return deny_output();
    }
    nudge_output(READ_NUDGE_TEXT)
}

/// Injectable graph probe keeps error/fail-open behavior deterministic in tests.
pub fn evaluate_with_graph_probe<F>(
    mode: &str,
    input: &[u8],
    context: &GuardContext,
    mut graph_exists: F,
) -> String
where
    F: FnMut(&Path) -> io::Result<bool>,
{
    if mode == "gemini" {
        return gemini_output(graph_exists(&context.graph_path()).unwrap_or(false));
    }
    if !matches!(mode, "search" | "read") {
        return String::new();
    }
    let Some(document_value) = parse_document(input) else {
        return String::new();
    };
    let Some(document) = document_value.as_object() else {
        return String::new();
    };
    let Some(tool) = payload_tool(document) else {
        return String::new();
    };

    let should_nudge = match mode {
        "search" => is_search(tool),
        "read" => is_source_read(tool, context),
        _ => false,
    };
    if !should_nudge || !graph_exists(&context.graph_path()).unwrap_or(false) {
        return String::new();
    }
    nudge_output(if mode == "search" {
        SEARCH_NUDGE_TEXT
    } else {
        READ_NUDGE_TEXT
    })
}

fn parse_document(input: &[u8]) -> Option<Value> {
    let document = serde_json::from_slice::<Value>(input).ok()?;
    document.is_object().then_some(document)
}

fn payload_tool(document: &Map<String, Value>) -> Option<&Map<String, Value>> {
    match document.get("tool_input") {
        Some(value) => value.as_object(),
        None => Some(document),
    }
}

fn nudge_output(message: &str) -> String {
    let mut output = serde_json::to_string(&json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "additionalContext": message,
        }
    }))
    .unwrap_or_default();
    output.push('\n');
    output
}

fn deny_output() -> String {
    let mut output = serde_json::to_string(&json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": READ_DENY_TEXT,
        }
    }))
    .unwrap_or_default();
    output.push('\n');
    output
}

fn gemini_output(graph_exists: bool) -> String {
    let mut payload = Map::from_iter([("decision".to_owned(), Value::String("allow".to_owned()))]);
    if graph_exists {
        payload.insert(
            "additionalContext".to_owned(),
            Value::String(GEMINI_NUDGE_TEXT.to_owned()),
        );
    }
    serde_json::to_string(&payload).unwrap_or_else(|_| r#"{"decision":"allow"}"#.to_owned())
}

/// Resolve strict mode using the runtime compatibility environment. The native
/// Graphoxide spelling wins when both variables are present.
pub fn strict_enabled(installed_flag: bool) -> bool {
    let value = env::var("GRAPHOXIDE_HOOK_STRICT")
        .ok()
        .or_else(|| env::var("GRAPHIFY_HOOK_STRICT").ok());
    strict_enabled_with_override(installed_flag, value.as_deref())
}

/// Pure strict-mode resolution used by the ported precedence tests.
pub fn strict_enabled_with_override(installed_flag: bool, value: Option<&str>) -> bool {
    match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("1" | "true" | "yes" | "on") => true,
        Some("0" | "false" | "no" | "off") => false,
        _ => installed_flag,
    }
}

fn is_in_project(tool: &Map<String, Value>, context: &GuardContext) -> bool {
    let explicit = ["file_path", "path"]
        .into_iter()
        .filter_map(|key| tool.get(key))
        .map(json_scalar_text)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if explicit.is_empty() || explicit.iter().any(|value| !Path::new(value).is_absolute()) {
        return true;
    }
    let root = canonical_or_owned(&context.project_root);
    explicit.iter().any(|value| {
        canonical_or_owned(Path::new(value))
            .strip_prefix(&root)
            .is_ok()
    })
}

fn target_is_stale(
    tool: &Map<String, Value>,
    context: &GuardContext,
    graph_metadata: &Metadata,
) -> bool {
    let graph_modified = graph_metadata.modified().ok();
    let file_path = tool
        .get("file_path")
        .map(json_scalar_text)
        .unwrap_or_default();
    let source_stale = (!file_path.is_empty())
        && graph_modified.is_some_and(|graph_modified| {
            let path = project_path(&file_path, context);
            fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .map(|source_modified| source_modified > graph_modified)
                .unwrap_or(false)
        });
    source_stale || context.output_root().join("needs_update").exists()
}

fn project_path(value: &str, context: &GuardContext) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        context.project_root.join(path)
    }
}

fn canonical_or_owned(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn query_stamp_fresh(context: &GuardContext) -> bool {
    let ttl_value = env::var("GRAPHOXIDE_HOOK_STRICT_TTL")
        .ok()
        .or_else(|| env::var("GRAPHIFY_HOOK_STRICT_TTL").ok());
    let ttl = match ttl_value {
        Some(value) => match value.parse::<f64>() {
            Ok(value) => value,
            Err(_) => return false,
        },
        None => 1_800.0,
    };
    let modified = match fs::metadata(context.output_root().join("cache/last_query_stamp"))
        .and_then(|metadata| metadata.modified())
    {
        Ok(modified) => modified,
        Err(_) => return false,
    };
    match SystemTime::now().duration_since(modified) {
        Ok(age) => age.as_secs_f64() < ttl,
        Err(_) => ttl > 0.0,
    }
}

fn target_is_indexed(file_path: &str, context: &GuardContext) -> bool {
    if file_path.is_empty() {
        return true;
    }
    let manifest_path = context.output_root().join("manifest.json");
    let manifest = (|| -> Option<Value> {
        if fs::metadata(&manifest_path).ok()?.len() > 2_000_000 {
            return None;
        }
        serde_json::from_slice(&fs::read(&manifest_path).ok()?).ok()
    })();
    let Some(manifest) = manifest else {
        return true;
    };
    let Some(entries) = manifest.as_object() else {
        return true;
    };
    if entries.is_empty() {
        return true;
    }

    let raw_path = Path::new(file_path);
    let resolved = canonical_or_owned(&project_path(file_path, context));
    let root = canonical_or_owned(&context.project_root);
    let mut relatives = Vec::new();
    if let Ok(relative) = resolved.strip_prefix(root) {
        let relative = slash_path(relative);
        if !relative.is_empty() {
            relatives.push(relative);
        }
    }
    if let Some(name) = raw_path.file_name().and_then(|name| name.to_str()) {
        relatives.push(name.replace('\\', "/"));
    }
    let absolute_key = file_path.replace('\\', "/");
    entries.keys().map(|key| key.replace('\\', "/")).any(|key| {
        key == absolute_key
            || relatives
                .iter()
                .any(|relative| key == *relative || key.ends_with(&format!("/{relative}")))
    })
}

fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn mark_session_denied(session_id: &str, context: &GuardContext) -> bool {
    let sanitized = session_id
        .chars()
        .take(64)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        return false;
    }
    let directory = context.output_root().join("cache/hook_sessions");
    if fs::create_dir_all(&directory).is_err() {
        return false;
    }
    let marker = directory.join(format!("{sanitized}.denied"));
    if OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)
        .is_err()
    {
        return false;
    }
    collect_old_session_markers(&directory);
    true
}

fn collect_old_session_markers(directory: &Path) {
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(86_400))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .map(|modified| modified < cutoff)
            .unwrap_or(false);
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn is_search(tool: &Map<String, Value>) -> bool {
    let command = tool
        .get("command")
        .map(json_scalar_text)
        .unwrap_or_default();
    let dedicated_grep = command.is_empty() && tool.get("pattern").is_some_and(json_truthy);
    let bash_search = ["grep", "ripgrep", "rg ", "find ", "fd ", "ack ", "ag "]
        .iter()
        .any(|token| command.contains(token));
    dedicated_grep || bash_search
}

fn is_source_read(tool: &Map<String, Value>, context: &GuardContext) -> bool {
    let values = ["file_path", "pattern", "path"]
        .map(|key| tool.get(key).map(json_scalar_text).unwrap_or_default());
    let joined = values
        .iter()
        .map(|value| value.to_ascii_lowercase().replace('\\', "/"))
        .collect::<Vec<_>>()
        .join(" ");
    let output_name = context.output_name();
    let under_output = ["graphoxide-out", "graphify-out", output_name.as_str()]
        .iter()
        .any(|name| !name.is_empty() && joined.contains(&format!("{name}/")));
    if under_output {
        return false;
    }
    values
        .iter()
        .filter_map(|value| path_tail(value))
        .any(|tail| SOURCE_EXTENSIONS.iter().any(|extension| tail == *extension))
}

fn path_tail(value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    let normalized = value.to_ascii_lowercase().replace('\\', "/");
    let basename = normalized.rsplit('/').next().unwrap_or_default();
    let (_, extension) = basename.rsplit_once('.')?;
    Some(format!(".{extension}"))
}

fn json_scalar_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}
