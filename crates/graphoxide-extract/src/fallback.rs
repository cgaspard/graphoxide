//! Regex and structured-data extraction for languages without a compiled grammar.

use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use regex::Regex;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::Path,
};

pub fn extract_text(path: &Path, source_file: &str) -> anyhow::Result<Extraction> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let filename = path.file_name().and_then(|v| v.to_str()).unwrap_or("");
    if matches!(
        filename,
        "pyproject.toml" | "go.mod" | "pom.xml" | "apm.yml" | "apm.yaml"
    ) {
        return extract_manifest(&text, source_file, filename);
    }
    if path
        .extension()
        .and_then(|v| v.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
    {
        return extract_json(&text, source_file, path);
    }
    let stem = Path::new(source_file)
        .with_extension("")
        .to_string_lossy()
        .into_owned();
    let file_id = make_id(&[&stem]);
    let mut nodes = vec![node(
        file_id.clone(),
        path.file_name()
            .and_then(|v| v.to_str())
            .unwrap_or(source_file),
        source_file,
        1,
        "file",
    )];
    let mut edges = Vec::new();
    let mut seen = HashSet::from([file_id.clone()]);
    let definitions = Regex::new(
        r"(?m)^\s*(?:(?:pub(?:lic|lic static)?|private|protected|internal|export|abstract|static|async|final|open|partial)\s+)*(class|interface|struct|enum|trait|protocol|module|namespace|type|def|fn|fun|function|func|sub|procedure)\s+([\p{L}_][\p{L}\p{N}_]*)",
    )?;
    let mut labels = HashMap::new();
    for capture in definitions.captures_iter(&text) {
        let kind = &capture[1];
        let name = &capture[2];
        let id = make_id(&[&stem, name]);
        if !seen.insert(id.clone()) {
            continue;
        }
        let line = line_of(&text, capture.get(0).unwrap().start());
        let function = matches!(
            kind,
            "def" | "fn" | "fun" | "function" | "func" | "sub" | "procedure"
        );
        nodes.push(node(
            id.clone(),
            if function {
                format!("{name}()")
            } else {
                name.into()
            },
            source_file,
            line,
            if function { "function" } else { "class" },
        ));
        edges.push(edge(
            file_id.clone(),
            id.clone(),
            "contains",
            source_file,
            line,
            Confidence::Extracted,
        ));
        labels.insert(name.to_lowercase(), id);
    }
    for (kind, name, start, function) in special_definitions(path, &text)? {
        let id = make_id(&[&stem, &name]);
        if !seen.insert(id.clone()) {
            continue;
        }
        let line = line_of(&text, start);
        nodes.push(node(
            id.clone(),
            if function {
                format!("{name}()")
            } else {
                name.clone()
            },
            source_file,
            line,
            &kind,
        ));
        edges.push(edge(
            file_id.clone(),
            id.clone(),
            "contains",
            source_file,
            line,
            Confidence::Extracted,
        ));
        labels.insert(name.to_lowercase(), id);
    }
    let imports = Regex::new(
        r#"(?m)^\s*(?:import|from|use|using|require|include)\s*[('\"]*([\p{L}\p{N}_./:@-]+)"#,
    )?;
    for capture in imports.captures_iter(&text) {
        let module = &capture[1];
        let line = line_of(&text, capture.get(0).unwrap().start());
        let id = make_id(&[module]);
        if seen.insert(id.clone()) {
            nodes.push(node(
                id.clone(),
                module
                    .rsplit(['/', ':', '.'])
                    .find(|v| !v.is_empty())
                    .unwrap_or(module),
                source_file,
                line,
                "module",
            ));
        }
        edges.push(edge(
            file_id.clone(),
            id,
            "imports",
            source_file,
            line,
            Confidence::Extracted,
        ));
    }
    let calls = Regex::new(r"([\p{L}_][\p{L}\p{N}_]*)\s*\(")?;
    let keywords = [
        "if", "for", "while", "switch", "catch", "return", "class", "function", "func", "fn",
        "def", "sizeof", "typeof",
    ];
    for capture in calls.captures_iter(&text) {
        let name = &capture[1];
        if keywords.contains(&name) {
            continue;
        }
        if let Some(target) = labels.get(&name.to_lowercase()) {
            let line = line_of(&text, capture.get(0).unwrap().start());
            if let Some(source) = nearest_definition(&nodes, line) {
                if source != target {
                    edges.push(edge(
                        source.into(),
                        target.clone(),
                        "calls",
                        source_file,
                        line,
                        Confidence::Inferred,
                    ));
                }
            }
        }
    }
    if matches!(
        path.extension().and_then(|v| v.to_str()),
        Some("md" | "markdown")
    ) {
        let links = Regex::new(r"\[[^\]]+\]\(([^)#]+)")?;
        for capture in links.captures_iter(&text) {
            let target = &capture[1];
            let id = make_id(&[target]);
            let line = line_of(&text, capture.get(0).unwrap().start());
            if seen.insert(id.clone()) {
                nodes.push(node(id.clone(), target, source_file, line, "document"));
            }
            edges.push(edge(
                file_id.clone(),
                id,
                "references",
                source_file,
                line,
                Confidence::Extracted,
            ));
        }
    }
    Ok(Extraction {
        nodes,
        edges,
        hyperedges: Vec::new(),
    })
}

fn extract_manifest(text: &str, source_file: &str, filename: &str) -> anyhow::Result<Extraction> {
    let package = match filename {
        "go.mod" => Regex::new(r"(?m)^module\s+([^\s]+)")?
            .captures(text)
            .map(|c| c[1].to_owned()),
        "pom.xml" => Regex::new(r"(?s)<artifactId>\s*([^<]+)")?
            .captures(text)
            .map(|c| c[1].trim().to_owned()),
        "pyproject.toml" => Regex::new(r#"(?m)^name\s*=\s*[\"']([^\"']+)"#)?
            .captures(text)
            .map(|c| c[1].to_owned()),
        _ => Regex::new(r"(?m)^name:\s*([^\s]+)")?
            .captures(text)
            .map(|c| c[1].to_owned()),
    }
    .unwrap_or_else(|| filename.to_owned());
    let package_id = make_id(&["pkg", &package]);
    let nodes = vec![node(package_id.clone(), package, source_file, 1, "package")];
    let mut edges = Vec::new();
    let mut seen = HashSet::new();
    for pattern in [
        r#"(?m)^\s*([A-Za-z0-9_.-]+)\s*(?:=|:)\s*[\"']?[0-9*^~><]"#,
        r"(?m)^\s*require\s+([^\s]+)",
        r"(?s)<dependency>.*?<artifactId>\s*([^<]+)",
    ] {
        let regex = Regex::new(pattern)?;
        for capture in regex.captures_iter(text) {
            let name = capture[1].trim();
            if name == "name" || !seen.insert(name.to_owned()) {
                continue;
            }
            let line = line_of(text, capture.get(0).unwrap().start());
            let id = make_id(&["pkg", name]);
            edges.push(edge(
                package_id.clone(),
                id,
                "depends_on",
                source_file,
                line,
                Confidence::Extracted,
            ));
        }
    }
    Ok(Extraction {
        nodes,
        edges,
        hyperedges: Vec::new(),
    })
}

fn extract_json(text: &str, source_file: &str, path: &Path) -> anyhow::Result<Extraction> {
    let stem = Path::new(source_file)
        .with_extension("")
        .to_string_lossy()
        .into_owned();
    let file_id = make_id(&[&stem]);
    let mut nodes = vec![node(
        file_id.clone(),
        path.file_name()
            .and_then(|v| v.to_str())
            .unwrap_or(source_file),
        source_file,
        1,
        "file",
    )];
    let mut edges = Vec::new();
    if let Ok(value) = serde_json::from_str(text) {
        walk_json(&value, "", &file_id, source_file, &mut nodes, &mut edges);
    }
    Ok(Extraction {
        nodes,
        edges,
        hyperedges: Vec::new(),
    })
}
fn walk_json(
    value: &serde_json::Value,
    prefix: &str,
    parent: &str,
    source: &str,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
) {
    if let Some(object) = value.as_object() {
        for (key, value) in object {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            let id = make_id(&[source, &path]);
            nodes.push(node(id.clone(), key, source, 1, "json_key"));
            edges.push(edge(
                parent.into(),
                id.clone(),
                "contains",
                source,
                1,
                Confidence::Extracted,
            ));
            walk_json(value, &path, &id, source, nodes, edges);
        }
    } else if let Some(array) = value.as_array() {
        for value in array {
            walk_json(value, prefix, parent, source, nodes, edges);
        }
    }
}
fn node(id: String, label: impl Into<String>, source: &str, line: usize, kind: &str) -> Node {
    Node {
        id,
        label: label.into(),
        file_type: if kind == "document" {
            "document"
        } else {
            "code"
        }
        .into(),
        source_file: source.into(),
        source_location: Some(format!("L{line}")),
        community: None,
        extra: BTreeMap::from([
            ("_origin".into(), "ast".into()),
            ("type".into(), kind.into()),
        ]),
    }
}
fn edge(
    source: String,
    target: String,
    relation: &str,
    file: &str,
    line: usize,
    confidence: Confidence,
) -> Edge {
    Edge {
        source: source.clone(),
        target: target.clone(),
        relation: relation.into(),
        confidence,
        source_file: file.into(),
        extra: BTreeMap::from([
            ("source_location".into(), format!("L{line}").into()),
            ("weight".into(), 1.0.into()),
            ("_src".into(), source.into()),
            ("_tgt".into(), target.into()),
        ]),
    }
}
fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].bytes().filter(|b| *b == b'\n').count() + 1
}
fn nearest_definition(nodes: &[Node], line: usize) -> Option<&str> {
    nodes
        .iter()
        .filter_map(|n| {
            Some((
                n.source_location
                    .as_deref()?
                    .trim_start_matches('L')
                    .parse::<usize>()
                    .ok()?,
                n.id.as_str(),
            ))
        })
        .filter(|(at, _)| *at <= line)
        .max_by_key(|(at, _)| *at)
        .map(|(_, id)| id)
}

fn special_definitions(
    path: &Path,
    text: &str,
) -> anyhow::Result<Vec<(String, String, usize, bool)>> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut found = Vec::new();
    let mut capture =
        |pattern: &str, kind_group: Option<usize>, name_group: usize, function: bool| {
            let regex = Regex::new(pattern)?;
            for row in regex.captures_iter(text) {
                let Some(name) = row.get(name_group) else {
                    continue;
                };
                let kind = kind_group
                    .and_then(|index| row.get(index))
                    .map(|value| value.as_str().to_ascii_lowercase())
                    .unwrap_or_else(|| {
                        if function {
                            "function".into()
                        } else {
                            "class".into()
                        }
                    });
                found.push((
                    kind,
                    name.as_str().trim_matches(['"', '\'']).to_owned(),
                    row.get(0).unwrap().start(),
                    function,
                ));
            }
            anyhow::Ok(())
        };
    match extension.as_str() {
        "sql" => capture(
            r"(?im)^\s*create\s+(?:or\s+replace\s+)?(table|view|function|procedure|trigger|type)\s+(?:if\s+not\s+exists\s+)?([\p{L}_][\p{L}\p{N}_.$]*)",
            Some(1),
            2,
            false,
        )?,
        "tf" | "tfvars" | "hcl" => capture(
            r#"(?m)^\s*(resource|data|module|variable|output|provider|terraform)\s*(?:\"([^\"]+)\")?"#,
            Some(1),
            2,
            false,
        )?,
        "ps1" | "psm1" | "psd1" => {
            capture(
                r"(?im)^\s*(function|filter|class|enum)\s+([\p{L}_][\p{L}\p{N}_-]*)",
                Some(1),
                2,
                false,
            )?;
        }
        "v" | "sv" | "svh" => capture(
            r"(?im)^\s*(module|interface|package|program|function|task|class)\s+(?:automatic\s+)?([\p{L}_][\p{L}\p{N}_]*)",
            Some(1),
            2,
            false,
        )?,
        "f" | "f90" | "f95" | "f03" | "f08" => capture(
            r"(?im)^\s*(module|program|subroutine|function|type)\s+([\p{L}_][\p{L}\p{N}_]*)",
            Some(1),
            2,
            false,
        )?,
        "pas" | "pp" | "dpr" | "dpk" | "lpr" | "inc" => {
            capture(
                r"(?im)^\s*(unit|program|library|package)\s+([\p{L}_][\p{L}\p{N}_]*)",
                Some(1),
                2,
                false,
            )?;
            capture(
                r"(?im)^\s*(?:class\s+)?(function|procedure|constructor|destructor)\s+(?:[\p{L}_][\p{L}\p{N}_]*\.)?([\p{L}_][\p{L}\p{N}_]*)",
                Some(1),
                2,
                true,
            )?;
        }
        "dart" => capture(
            r"(?m)^\s*(class|mixin|enum|extension|typedef)\s+([\p{L}_][\p{L}\p{N}_]*)",
            Some(1),
            2,
            false,
        )?,
        "cls" | "trigger" => capture(
            r"(?im)^\s*(?:public|private|global|protected|virtual|abstract|with\s+sharing|without\s+sharing|inherited\s+sharing|\s)*(class|interface|enum|trigger)\s+([\p{L}_][\p{L}\p{N}_]*)",
            Some(1),
            2,
            false,
        )?,
        "h" | "m" | "mm" => capture(
            r"(?im)^\s*@(interface|implementation|protocol)\s+([\p{L}_][\p{L}\p{N}_]*)",
            Some(1),
            2,
            false,
        )?,
        "dfm" | "lfm" => capture(
            r"(?im)^\s*(?:object|inherited|inline)\s+([\p{L}_][\p{L}\p{N}_]*)\s*:",
            None,
            1,
            false,
        )?,
        "sln" => capture(
            r#"(?m)^Project\([^\r\n]+\)\s*=\s*\"([^\"]+)\""#,
            None,
            1,
            false,
        )?,
        "slnx" | "csproj" | "fsproj" | "vbproj" | "xaml" | "lpk" | "xml" => {
            capture(
                r#"(?i)<(?:Project|Package|Compile|Page|ApplicationDefinition|ProjectReference)[^>]*(?:Name|Include|Source)=\"([^\"]+)\""#,
                None,
                1,
                false,
            )?;
            capture(r#"(?i)x:Class=\"([^\"]+)\""#, None, 1, false)?;
        }
        _ => {}
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_fallbacks_find_tier_two_and_three_symbols() {
        let cases = [
            ("schema.sql", "CREATE TABLE users (id int);", "users"),
            (
                "main.tf",
                "resource \"aws_s3_bucket\" \"assets\" {}",
                "aws_s3_bucket",
            ),
            ("build.ps1", "function Invoke-Build { }", "Invoke-Build"),
            ("chip.sv", "module counter(input clk); endmodule", "counter"),
            ("main.pas", "procedure RunApp; begin end;", "RunApp"),
            (
                "Demo.trigger",
                "trigger Demo on Account (before insert) {}",
                "Demo",
            ),
            ("App.xaml", "<Application x:Class=\"Demo.App\">", "Demo.App"),
        ];
        for (path, source, expected) in cases {
            let symbols = special_definitions(Path::new(path), source).unwrap();
            assert!(
                symbols.iter().any(|(_, name, _, _)| name == expected),
                "{path}: {symbols:?}"
            );
        }
    }

    #[test]
    fn manifests_emit_only_the_local_package_node() {
        let result = extract_manifest(
            "[project]\nname = \"demo\"\ndependencies = [\"serde>=1\"]",
            "pyproject.toml",
            "pyproject.toml",
        )
        .unwrap();
        assert_eq!(result.nodes.len(), 1);
        assert!(result
            .edges
            .iter()
            .all(|edge| edge.relation == "depends_on"));
    }
}
