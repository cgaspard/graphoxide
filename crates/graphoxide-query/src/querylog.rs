//! Privacy-preserving, opt-in JSONL query telemetry.

use serde_json::json;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct QueryLogConfig {
    pub path: Option<PathBuf>,
    pub include_response: bool,
}

#[derive(Debug, Clone)]
pub struct QueryLogRecord<'a> {
    pub kind: &'a str,
    pub question: &'a str,
    pub corpus: &'a Path,
    pub result: Option<&'a str>,
    pub duration_ms: Option<f64>,
    pub mode: Option<&'a str>,
    pub depth: Option<usize>,
    pub nodes_returned: Option<usize>,
}

fn truthy(value: Option<&str>) -> bool {
    value.is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

pub fn nodes_from_result(result: &str) -> Option<usize> {
    let words: Vec<_> = result.split_whitespace().collect();
    words.windows(3).find_map(|window| {
        (matches!(window[1], "node" | "nodes") && window[2] == "found")
            .then(|| window[0].parse().ok())
            .flatten()
    })
}

pub fn config_from_values(values: &HashMap<String, String>, home: Option<&Path>) -> QueryLogConfig {
    let get = |graphoxide: &str, graphify: &str| {
        values
            .get(graphoxide)
            .or_else(|| values.get(graphify))
            .map(String::as_str)
    };
    if truthy(get(
        "GRAPHOXIDE_QUERY_LOG_DISABLE",
        "GRAPHIFY_QUERY_LOG_DISABLE",
    )) {
        return QueryLogConfig::default();
    }
    let path = get("GRAPHOXIDE_QUERY_LOG", "GRAPHIFY_QUERY_LOG")
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            truthy(get(
                "GRAPHOXIDE_QUERY_LOG_ENABLE",
                "GRAPHIFY_QUERY_LOG_ENABLE",
            ))
            .then(|| {
                home.map(|home| {
                    let filename = if values.contains_key("GRAPHIFY_QUERY_LOG_ENABLE")
                        && !values.contains_key("GRAPHOXIDE_QUERY_LOG_ENABLE")
                    {
                        "graphify-queries.log"
                    } else {
                        "graphoxide-queries.log"
                    };
                    home.join(".cache").join(filename)
                })
            })
            .flatten()
        });
    QueryLogConfig {
        path,
        include_response: truthy(get(
            "GRAPHOXIDE_QUERY_LOG_RESPONSES",
            "GRAPHIFY_QUERY_LOG_RESPONSES",
        )),
    }
}

pub fn config_from_env() -> QueryLogConfig {
    const KEYS: &[&str] = &[
        "GRAPHOXIDE_QUERY_LOG",
        "GRAPHIFY_QUERY_LOG",
        "GRAPHOXIDE_QUERY_LOG_ENABLE",
        "GRAPHIFY_QUERY_LOG_ENABLE",
        "GRAPHOXIDE_QUERY_LOG_DISABLE",
        "GRAPHIFY_QUERY_LOG_DISABLE",
        "GRAPHOXIDE_QUERY_LOG_RESPONSES",
        "GRAPHIFY_QUERY_LOG_RESPONSES",
    ];
    let values = KEYS
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| ((*key).to_owned(), value))
        })
        .collect();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    config_from_values(&values, home.as_deref())
}

/// Best effort by design: telemetry must never break a successful query.
pub fn log_query(config: &QueryLogConfig, record: &QueryLogRecord<'_>) {
    let _ = try_log_query(config, record);
}

fn try_log_query(config: &QueryLogConfig, record: &QueryLogRecord<'_>) -> anyhow::Result<()> {
    let Some(path) = &config.path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let result = record.result.unwrap_or("");
    let mut value = json!({
        "ts": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        "kind": record.kind,
        "question": record.question,
        "corpus": record.corpus.display().to_string(),
        "nodes_returned": record.nodes_returned.or_else(|| nodes_from_result(result)),
        "result_chars": result.len(),
        "duration_ms": record.duration_ms,
    });
    if let Some(mode) = record.mode {
        value["mode"] = mode.into();
    }
    if let Some(depth) = record.depth {
        value["depth"] = depth.into();
    }
    if config.include_response {
        value["response"] = result.into();
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{}", serde_json::to_string(&value)?)?;
    Ok(())
}

pub fn log_query_from_env(record: &QueryLogRecord<'_>) {
    log_query(&config_from_env(), record);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record<'a>(question: &'a str, result: Option<&'a str>) -> QueryLogRecord<'a> {
        QueryLogRecord {
            kind: "query",
            question,
            corpus: Path::new("/some/graph.json"),
            result,
            duration_ms: Some(12.5),
            mode: Some("bfs"),
            depth: Some(2),
            nodes_returned: None,
        }
    }

    fn fixture(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("graphoxide-querylog-{}-{name}", std::process::id()))
    }

    fn cleanup(path: &Path) {
        if path.is_file() {
            std::fs::remove_file(path).expect("remove query log fixture");
        }
        let mut parent = path.parent();
        while let Some(directory) = parent {
            if directory == std::env::temp_dir() {
                break;
            }
            if directory
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("graphoxide-querylog-"))
            {
                let _ = std::fs::remove_dir_all(directory);
                break;
            }
            parent = directory.parent();
        }
    }

    #[test]
    fn test_nodes_from_result_parses_header() {
        assert_eq!(
            nodes_from_result(
                "Traversal: BFS depth=2 | Start: ['foo'] | 7 nodes found\n\nNODE foo"
            ),
            Some(7)
        );
    }

    #[test]
    fn test_nodes_from_result_singular() {
        assert_eq!(nodes_from_result("1 node found"), Some(1));
    }

    #[test]
    fn test_nodes_from_result_missing() {
        assert_eq!(nodes_from_result("no match here"), None);
    }

    #[test]
    fn test_nodes_from_result_empty() {
        assert_eq!(nodes_from_result(""), None);
    }

    #[test]
    fn test_log_query_writes_jsonl() {
        let path = fixture("write/q.log");
        log_query(
            &QueryLogConfig {
                path: Some(path.clone()),
                include_response: false,
            },
            &record("what is X", Some("3 nodes found\nNODE a")),
        );
        let lines = std::fs::read_to_string(&path).unwrap();
        assert_eq!(lines.lines().count(), 1);
        let value: serde_json::Value = serde_json::from_str(lines.trim()).unwrap();
        assert_eq!(value["kind"], "query");
        assert_eq!(value["question"], "what is X");
        assert_eq!(value["corpus"], "/some/graph.json");
        assert_eq!(value["nodes_returned"], 3);
        assert!(value["result_chars"].as_u64().unwrap() > 0);
        assert_eq!(value["duration_ms"], 12.5);
        assert_eq!(value["mode"], "bfs");
        assert!(value.get("ts").is_some());
        cleanup(&path);
    }

    #[test]
    fn test_log_query_appends() {
        let path = fixture("append/q.log");
        let config = QueryLogConfig {
            path: Some(path.clone()),
            include_response: false,
        };
        log_query(&config, &record("q1", None));
        log_query(&config, &record("q2", None));
        let values: Vec<serde_json::Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["question"], "q1");
        assert_eq!(values[1]["question"], "q2");
        cleanup(&path);
    }

    fn disabled_values(value: &str) -> QueryLogConfig {
        config_from_values(
            &HashMap::from([
                (
                    "GRAPHIFY_QUERY_LOG".into(),
                    fixture("disabled/q.log").display().to_string(),
                ),
                ("GRAPHIFY_QUERY_LOG_DISABLE".into(), value.into()),
            ]),
            None,
        )
    }

    #[test]
    fn test_disable_env() {
        assert!(disabled_values("1").path.is_none());
    }

    #[test]
    fn test_disable_env_true() {
        assert!(disabled_values("true").path.is_none());
    }

    #[test]
    fn test_responses_not_logged_by_default() {
        let path = fixture("no-response/q.log");
        log_query(
            &QueryLogConfig {
                path: Some(path.clone()),
                include_response: false,
            },
            &record("q", Some("NODE foo")),
        );
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(value.get("response").is_none());
        cleanup(&path);
    }

    #[test]
    fn test_responses_optin() {
        let path = fixture("response/q.log");
        log_query(
            &QueryLogConfig {
                path: Some(path.clone()),
                include_response: true,
            },
            &record("q", Some("NODE foo bar")),
        );
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["response"], "NODE foo bar");
        cleanup(&path);
    }

    #[test]
    fn test_log_never_raises() {
        let directory = fixture("bad-path");
        std::fs::create_dir_all(&directory).unwrap();
        log_query(
            &QueryLogConfig {
                path: Some(directory.clone()),
                include_response: false,
            },
            &record("q", None),
        );
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn test_log_creates_parent_dirs() {
        let path = fixture("deep/nested/q.log");
        log_query(
            &QueryLogConfig {
                path: Some(path.clone()),
                include_response: false,
            },
            &record("q", None),
        );
        assert!(path.is_file());
        cleanup(&path);
    }

    #[test]
    fn test_nodes_returned_inferred_from_result() {
        let path = fixture("inferred/q.log");
        log_query(
            &QueryLogConfig {
                path: Some(path.clone()),
                include_response: false,
            },
            &record("q", Some("5 nodes found\nNODE a\nNODE b")),
        );
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["nodes_returned"], 5);
        cleanup(&path);
    }

    #[test]
    fn test_explicit_nodes_returned_takes_precedence() {
        let path = fixture("explicit/q.log");
        let mut item = record("A -> B", Some("99 nodes found"));
        item.kind = "path";
        item.nodes_returned = Some(3);
        log_query(
            &QueryLogConfig {
                path: Some(path.clone()),
                include_response: false,
            },
            &item,
        );
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["nodes_returned"], 3);
        cleanup(&path);
    }

    #[test]
    fn test_kind_mcp_query() {
        let path = fixture("mcp/q.log");
        let mut item = record("q", None);
        item.kind = "mcp_query";
        log_query(
            &QueryLogConfig {
                path: Some(path.clone()),
                include_response: false,
            },
            &item,
        );
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["kind"], "mcp_query");
        cleanup(&path);
    }

    #[test]
    fn test_query_log_off_by_default() {
        assert!(config_from_values(&HashMap::new(), Some(Path::new("/tmp")))
            .path
            .is_none());
    }

    #[test]
    fn test_query_log_enabled_by_explicit_flag() {
        let config = config_from_values(
            &HashMap::from([("GRAPHIFY_QUERY_LOG_ENABLE".into(), "1".into())]),
            Some(Path::new("/home/test")),
        );
        assert!(config.path.unwrap().ends_with("graphify-queries.log"));
    }

    #[test]
    fn test_query_log_enabled_by_explicit_path() {
        let path = fixture("explicit-path/q.log");
        let config = config_from_values(
            &HashMap::from([("GRAPHIFY_QUERY_LOG".into(), path.display().to_string())]),
            None,
        );
        assert_eq!(config.path, Some(path));
    }

    #[test]
    fn test_query_log_disable_wins() {
        let config = config_from_values(
            &HashMap::from([
                ("GRAPHIFY_QUERY_LOG_ENABLE".into(), "1".into()),
                ("GRAPHIFY_QUERY_LOG_DISABLE".into(), "1".into()),
            ]),
            Some(Path::new("/home/test")),
        );
        assert!(config.path.is_none());
    }

    #[test]
    fn test_log_query_writes_nothing_by_default() {
        let path = fixture("default-off/q.log");
        log_query(
            &QueryLogConfig::default(),
            &record("secret internal ticket TICKET-123", Some("1 node found")),
        );
        assert!(!path.exists());
    }
}
