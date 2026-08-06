//! Script extraction for Vue, Astro, and Svelte single-file components.
//!
//! Markup is replaced byte-for-byte with spaces while line endings and script
//! bodies remain untouched.  This lets the existing JavaScript/TypeScript AST
//! walker retain exact source locations without attempting to parse HTML as JS.

use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use regex::Regex;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Vue,
    Astro,
    Svelte,
}

impl Kind {
    fn for_path(path: &Path) -> Option<Self> {
        match path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("vue") => Some(Self::Vue),
            Some("astro") => Some(Self::Astro),
            Some("svelte") => Some(Self::Svelte),
            _ => None,
        }
    }
}

pub(crate) struct PreparedSource {
    pub parser: Vec<u8>,
    pub language: &'static str,
}

fn script_regex() -> Regex {
    Regex::new(r#"(?is)(<script\b(?:"[^"]*"|'[^']*'|[^>"'])*>)(.*?)(</script\s*>)"#)
        .expect("valid SFC script-block regex")
}

fn script_language(open_tag: &str) -> Option<String> {
    Regex::new(r#"(?i)\blang\s*=\s*['"]?([A-Za-z]+)['"]?"#)
        .expect("valid SFC script-language regex")
        .captures(open_tag)
        .map(|capture| capture[1].to_ascii_lowercase())
}

fn blank(source: &[u8]) -> Vec<u8> {
    source
        .iter()
        .map(|byte| {
            if matches!(byte, b'\r' | b'\n') {
                *byte
            } else {
                b' '
            }
        })
        .collect()
}

fn preserve_range(masked: &mut [u8], source: &[u8], range: std::ops::Range<usize>) {
    masked[range.clone()].copy_from_slice(&source[range]);
}

fn mask_script_blocks(source: &str) -> (Vec<u8>, Option<String>) {
    let mut masked = blank(source.as_bytes());
    let mut language = None;
    for capture in script_regex().captures_iter(source) {
        let Some(body) = capture.get(2) else {
            continue;
        };
        preserve_range(&mut masked, source.as_bytes(), body.range());
        if language.is_none() {
            language = capture.get(1).and_then(|tag| script_language(tag.as_str()));
        }
    }
    (masked, language)
}

fn mask_astro(source: &str) -> Vec<u8> {
    let (mut masked, _) = mask_script_blocks(source);
    let frontmatter = Regex::new(r"(?s)\A\s*---\s*\r?\n(.*?)\r?\n---\s*(?:\r?\n|\z)")
        .expect("valid Astro frontmatter regex");
    if let Some(body) = frontmatter
        .captures(source)
        .and_then(|capture| capture.get(1))
    {
        preserve_range(&mut masked, source.as_bytes(), body.range());
    }
    masked
}

fn parser_language(kind: Kind, declared: Option<&str>) -> &'static str {
    match kind {
        Kind::Astro => "typescript",
        Kind::Vue => match declared {
            Some("tsx") => "tsx",
            Some("js" | "jsx") => "javascript",
            _ => "typescript",
        },
        Kind::Svelte => match declared {
            Some("ts" | "typescript") => "typescript",
            Some("tsx") => "tsx",
            _ => "javascript",
        },
    }
}

pub(crate) fn prepare(path: &Path) -> anyhow::Result<Option<PreparedSource>> {
    let original = fs::read(path)?;
    prepare_bytes(path, &original)
}

/// Prepare a single-file component from source bytes already supplied by the
/// I/O plane. The only allocated parser view is the required masked SFC
/// buffer; the original source remains borrowed by the caller.
pub(crate) fn prepare_bytes(
    path: &Path,
    original: &[u8],
) -> anyhow::Result<Option<PreparedSource>> {
    let Some(kind) = Kind::for_path(path) else {
        return Ok(None);
    };
    let source = String::from_utf8_lossy(original);
    let (parser, declared) = match kind {
        Kind::Astro => (mask_astro(&source), None),
        Kind::Vue | Kind::Svelte => mask_script_blocks(&source),
    };
    Ok(Some(PreparedSource {
        parser,
        language: parser_language(kind, declared.as_deref()),
    }))
}

/// Public compatibility helper for the upstream Vue masking contract.
pub fn mask_vue_non_script(source: &str) -> (String, Option<String>) {
    let (masked, language) = mask_script_blocks(source);
    (
        String::from_utf8(masked).expect("masking valid UTF-8 preserves valid UTF-8"),
        language,
    )
}

/// Return only executable regions for project-level import/export parsing.
pub(crate) fn resolution_source(path: &Path, source: &str) -> Option<String> {
    resolution_source_bytes(path, source)
}

/// Byte-only counterpart of [`resolution_source`] for the isolated project
/// resolver. The source is already admitted by the I/O plane, and this helper
/// deliberately does not inspect the physical path beyond its extension.
pub(crate) fn resolution_source_bytes(path: &Path, source: &str) -> Option<String> {
    let kind = Kind::for_path(path)?;
    let masked = match kind {
        Kind::Astro => mask_astro(source),
        Kind::Vue | Kind::Svelte => mask_script_blocks(source).0,
    };
    String::from_utf8(masked).ok()
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ImportSpec {
    line: usize,
    specifier: String,
    dynamic: bool,
}

fn line_number(source: &str, offset: usize) -> usize {
    source.as_bytes()[..offset]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

fn imports(masked: &str, original: &str) -> BTreeSet<ImportSpec> {
    let mut result = BTreeSet::new();
    let from = Regex::new(
        r#"(?m)(?:^|;)[ \t]*import[ \t]+(?:type[ \t]+)?[^;\n]+?[ \t]+from[ \t]+['"]([^'"]+)['"]"#,
    )
    .expect("valid SFC static import regex");
    for capture in from.captures_iter(masked) {
        let whole = capture.get(0).expect("static import has a whole match");
        result.insert(ImportSpec {
            line: line_number(masked, whole.start()),
            specifier: capture[1].into(),
            dynamic: false,
        });
    }
    let side_effect = Regex::new(r#"(?m)(?:^|;)[ \t]*import[ \t]*['"]([^'"]+)['"]"#)
        .expect("valid SFC side-effect import regex");
    for capture in side_effect.captures_iter(masked) {
        let whole = capture
            .get(0)
            .expect("side-effect import has a whole match");
        result.insert(ImportSpec {
            line: line_number(masked, whole.start()),
            specifier: capture[1].into(),
            dynamic: false,
        });
    }
    for dynamic in [
        Regex::new(r#"import\s*\(\s*['"]([^'"]+)['"]\s*\)"#),
        Regex::new(r#"import\s*\(\s*`([^`]+)`\s*\)"#),
    ]
    .map(|regex| regex.expect("valid SFC dynamic import regex"))
    {
        for capture in dynamic.captures_iter(original) {
            if capture[1].contains("${") {
                continue;
            }
            let whole = capture.get(0).expect("dynamic import has a whole match");
            result.insert(ImportSpec {
                line: line_number(original, whole.start()),
                specifier: capture[1].into(),
                dynamic: true,
            });
        }
    }
    result
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() {
                    output.push(component.as_os_str());
                }
            }
            _ => output.push(component.as_os_str()),
        }
    }
    output
}

fn normalized_text(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn inferred_root(physical: &Path, source_file: &str) -> Option<PathBuf> {
    let source = Path::new(source_file);
    if source.is_absolute() {
        return None;
    }
    let physical = fs::canonicalize(physical).unwrap_or_else(|_| normalize_path(physical));
    let mut root = physical;
    for _ in source.components() {
        root.pop();
    }
    Some(root)
}

fn preserve_importer_spelling(physical: &Path, target: &Path) -> PathBuf {
    let lexical_importer = normalize_path(physical);
    let canonical_importer =
        fs::canonicalize(physical).unwrap_or_else(|_| lexical_importer.clone());
    if lexical_importer == canonical_importer {
        return target.to_path_buf();
    }
    let lexical_components = lexical_importer.components().collect::<Vec<_>>();
    let canonical_components = canonical_importer.components().collect::<Vec<_>>();
    let shared_suffix = lexical_components
        .iter()
        .rev()
        .zip(canonical_components.iter().rev())
        .take_while(|(left, right)| left.as_os_str() == right.as_os_str())
        .count();
    if shared_suffix == 0 {
        return target.to_path_buf();
    }
    let mut lexical_root = lexical_importer;
    let mut canonical_root = canonical_importer;
    for _ in 0..shared_suffix {
        lexical_root.pop();
        canonical_root.pop();
    }
    target
        .strip_prefix(&canonical_root)
        .map(|relative| lexical_root.join(relative))
        .unwrap_or_else(|_| target.to_path_buf())
}

fn logical_target(physical: &Path, source_file: &str, target: &Path) -> String {
    if let Some(root) = inferred_root(physical, source_file) {
        let target = fs::canonicalize(target).unwrap_or_else(|_| normalize_path(target));
        if let Ok(relative) = target.strip_prefix(root) {
            return normalized_text(relative);
        }
    }
    normalized_text(&preserve_importer_spelling(physical, target))
}

fn file_identity(path: &str, preserve_extension: bool) -> String {
    if preserve_extension {
        make_id(&[path])
    } else {
        make_id(&[&Path::new(path).with_extension("").to_string_lossy()])
    }
}

fn import_edge(source: &str, target: &str, relation: &str, source_file: &str, line: usize) -> Edge {
    Edge {
        source: source.into(),
        target: target.into(),
        relation: relation.into(),
        confidence: Confidence::Extracted,
        source_file: source_file.into(),
        extra: BTreeMap::from([
            ("_src".into(), source.into()),
            ("_tgt".into(), target.into()),
            ("context".into(), "import".into()),
            ("source_location".into(), format!("L{line}").into()),
            ("weight".into(), 1.0.into()),
        ]),
    }
}

fn unresolved_stub(id: String, label: &str, source_file: String, line: usize) -> Node {
    Node {
        id,
        label: label.into(),
        file_type: "code".into(),
        source_file,
        source_location: Some(format!("L{line}")),
        community: None,
        extra: BTreeMap::from([
            ("_origin".into(), "ast".into()),
            ("type".into(), "module".into()),
            ("unresolved_import".into(), true.into()),
        ]),
    }
}

/// Canonicalize rescued SFC imports before corpus resolution. Real local files
/// are represented by their eventual file-node identity and unresolved local
/// paths use a portable repo-relative stub; external packages stay namespaced
/// dangling references so they cannot collapse onto unrelated local symbols.
pub(crate) fn augment_imports(
    extraction: &mut Extraction,
    path: &Path,
    source_file: &str,
    original: &[u8],
    parser: &[u8],
) {
    if Kind::for_path(path).is_none() {
        return;
    }
    let original = String::from_utf8_lossy(original);
    let parser = String::from_utf8_lossy(parser);
    let facts = imports(&parser, &original);
    if facts.is_empty() {
        return;
    }
    let file_id = make_id(&[&Path::new(source_file).with_extension("").to_string_lossy()]);
    let locations = facts
        .iter()
        .map(|fact| format!("L{}", fact.line))
        .collect::<BTreeSet<_>>();
    extraction.edges.retain(|edge| {
        !(matches!(edge.relation.as_str(), "imports_from" | "dynamic_import")
            && edge
                .extra
                .get("source_location")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|location| locations.contains(location)))
    });

    for fact in facts {
        let classification =
            crate::js_resolution::classify_es_module_specifier(source_file, &fact.specifier);
        let resolved =
            crate::js_resolution::resolve_import_path(&fact.specifier, path, source_file);
        let (target, target_file, unresolved_source) = if let Some(resolved) = resolved {
            let logical = logical_target(path, source_file, &resolved);
            (
                file_identity(&logical, Path::new(source_file).is_absolute()),
                Some(logical),
                None,
            )
        } else {
            match classification {
                crate::js_resolution::EsModuleSpecifier::ProjectRelative(logical) => (
                    file_identity(&logical, Path::new(source_file).is_absolute()),
                    None,
                    Some(logical),
                ),
                crate::js_resolution::EsModuleSpecifier::Bare => {
                    (make_id(&["ref", &fact.specifier]), None, None)
                }
                crate::js_resolution::EsModuleSpecifier::Unsafe => (
                    make_id(&["ref", "unsafe", source_file, &fact.specifier]),
                    None,
                    None,
                ),
            }
        };
        if target.is_empty() || target == file_id {
            continue;
        }
        if let Some(unresolved_source) = unresolved_source
            && extraction.nodes.iter().all(|node| node.id != target)
        {
            extraction.nodes.push(unresolved_stub(
                target.clone(),
                &fact.specifier,
                unresolved_source,
                fact.line,
            ));
        }
        let mut edge = import_edge(
            &file_id,
            &target,
            if fact.dynamic {
                "dynamic_import"
            } else {
                "imports_from"
            },
            source_file,
            fact.line,
        );
        if fact.dynamic {
            edge.extra.insert("deferred".into(), true.into());
        }
        if let Some(target_file) = target_file {
            edge.extra.insert("target_file".into(), target_file.into());
        }
        extraction.edges.push(edge);
    }
}
