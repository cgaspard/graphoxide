//! Host-independent lexical paths for references inside an admitted project.
//!
//! These helpers intentionally operate on strings instead of host filesystem
//! components. A source corpus can contain path spellings from another host,
//! and accepting those spellings must not depend on where Graphoxide runs.

/// Marks a source-relative placeholder whose identity names one exact logical
/// project file. Corpus resolution uses this provenance to prefer the real
/// file node without falling back to a same-basename candidate.
pub(crate) const EXACT_PROJECT_RELATIVE_PLACEHOLDER: &str = "_exact_project_relative_placeholder";

/// The result of resolving a relative reference against a project source file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProjectPath {
    /// The reference remains inside the logical project root.
    Contained(String),
    /// The reference walks above the logical project root.
    ///
    /// This is evidence about spelling only. Callers must not admit or bind an
    /// escaping target without separately proving it in their I/O-owning path.
    EscapesRoot(String),
}

/// Normalize a logical project path without consulting the host filesystem.
///
/// Both slash spellings are accepted as separators so callers for languages
/// with Windows path syntax can share the same primitive. Languages where a
/// backslash is not a path separator must reject it before calling this helper.
/// The empty string and `.` name the logical project root.
pub(crate) fn normalize_project_path(path: &str) -> Option<String> {
    if path.is_empty() || path == "." {
        return Some(String::new());
    }
    let mut components = Vec::new();
    apply_path(path, &mut components, false)?;
    Some(components.join("/"))
}

/// Resolve `reference` against the directory containing `source_file`.
///
/// The source must be a contained, non-root project file. Rooted, drive-based,
/// UNC, and non-portable references are rejected. A lexically valid relative
/// reference that crosses the project root is returned as `EscapesRoot`; byte
/// consumers must accept only `Contained` results.
pub(crate) fn source_relative_project_path(
    source_file: &str,
    reference: &str,
) -> Option<ProjectPath> {
    let source_file = normalize_project_path(source_file)?;
    if source_file.is_empty() {
        return None;
    }
    let mut components = source_file
        .split('/')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    components.pop()?;

    let escaped = apply_path(reference, &mut components, true)?;
    let normalized = if escaped == 0 {
        components.join("/")
    } else {
        let mut normalized = "../".repeat(escaped);
        normalized.push_str(&components.join("/"));
        while normalized.ends_with('/') {
            normalized.pop();
        }
        normalized
    };
    if normalized.is_empty() {
        return None;
    }
    if escaped == 0 {
        Some(ProjectPath::Contained(normalized))
    } else {
        Some(ProjectPath::EscapesRoot(normalized))
    }
}

fn apply_path(path: &str, components: &mut Vec<String>, allow_escape: bool) -> Option<usize> {
    if path.is_empty() || path.starts_with(['/', '\\']) {
        return None;
    }

    let mut escaped = 0usize;
    for component in path.split(['/', '\\']) {
        match component {
            "" => return None,
            "." => {}
            ".." => {
                if components.pop().is_none() {
                    if !allow_escape {
                        return None;
                    }
                    escaped = escaped.checked_add(1)?;
                }
            }
            component if portable_component(component) => components.push(component.to_owned()),
            _ => return None,
        }
    }
    Some(escaped)
}

fn portable_component(component: &str) -> bool {
    if component.ends_with(['.', ' '])
        || component.chars().any(|character| {
            character.is_control() || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*')
        })
    {
        return false;
    }

    let stem = component
        .split_once('.')
        .map_or(component, |(stem, _)| stem)
        .trim_end_matches(['.', ' '])
        .to_ascii_lowercase();
    !matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
        && !(stem.len() == 4
            && (stem.starts_with("com") || stem.starts_with("lpt"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
}

#[cfg(test)]
mod tests {
    use super::{normalize_project_path, source_relative_project_path, ProjectPath};

    #[test]
    fn normalizes_portable_project_paths_without_host_semantics() {
        for (input, expected) in [
            ("", ""),
            (".", ""),
            ("src/main.rs", "src/main.rs"),
            ("./src/./main.rs", "src/main.rs"),
            ("src/lib/../main.rs", "src/main.rs"),
            ("src/..", ""),
            (r"src\windows\main.rs", "src/windows/main.rs"),
            ("src/naïve/東京.rs", "src/naïve/東京.rs"),
        ] {
            assert_eq!(normalize_project_path(input).as_deref(), Some(expected));
        }
    }

    #[test]
    fn rejects_nonportable_or_escaping_project_paths() {
        for input in [
            "/src/main.rs",
            r"\src\main.rs",
            "//server/share/main.rs",
            r"\\server\share\main.rs",
            "C:/src/main.rs",
            r"C:\src\main.rs",
            "C:src/main.rs",
            "scheme:value",
            "src/node:module.rs",
            "src//main.rs",
            r"src\\main.rs",
            "../main.rs",
            "src/../../main.rs",
            "con",
            "CON.txt",
            "src/nul.rs",
            "src/con .txt",
            "src/aux...log",
            "src/com1.rs",
            "src/lpt9 .log",
            "src/LPT9.log",
            "src/name.",
            "src/name ",
            "src/na<me.rs",
            "src/na>me.rs",
            "src/na\"me.rs",
            "src/na|me.rs",
            "src/na?me.rs",
            "src/na*me.rs",
            "src/line\nbreak.rs",
            "src/nul\0byte.rs",
        ] {
            assert_eq!(normalize_project_path(input), None, "accepted {input:?}");
        }
        assert_eq!(
            normalize_project_path("src/com0.rs").as_deref(),
            Some("src/com0.rs")
        );
    }

    #[test]
    fn resolves_contained_and_escaping_source_relative_paths() {
        for (source, reference, expected) in [
            (
                "src/features/main.ts",
                "./worker.ts",
                ProjectPath::Contained("src/features/worker.ts".into()),
            ),
            (
                "src/features/main.ts",
                "../shared.ts",
                ProjectPath::Contained("src/shared.ts".into()),
            ),
            (
                "src/features/main.ts",
                "../../root.ts",
                ProjectPath::Contained("root.ts".into()),
            ),
            (
                "src/features/main.ts",
                "../../../external.ts",
                ProjectPath::EscapesRoot("../external.ts".into()),
            ),
            (
                "src/features/main.ts",
                "../../../../external.ts",
                ProjectPath::EscapesRoot("../../external.ts".into()),
            ),
            (
                r"src\features\main.ts",
                r"..\shared\worker.ts",
                ProjectPath::Contained("src/shared/worker.ts".into()),
            ),
            (
                "main.ts",
                "../shared/./worker.ts",
                ProjectPath::EscapesRoot("../shared/worker.ts".into()),
            ),
            (
                "main.ts",
                "../../shared/temp/../worker.ts",
                ProjectPath::EscapesRoot("../../shared/worker.ts".into()),
            ),
            (
                "src/nested/main.ts",
                ".",
                ProjectPath::Contained("src/nested".into()),
            ),
            (
                "src/nested/main.ts",
                "..",
                ProjectPath::Contained("src".into()),
            ),
            ("main.ts", "..", ProjectPath::EscapesRoot("..".into())),
        ] {
            assert_eq!(
                source_relative_project_path(source, reference),
                Some(expected),
                "source={source:?}, reference={reference:?}",
            );
        }
    }

    #[test]
    fn rejects_invalid_sources_and_references() {
        for source in ["", ".", "../main.rs", "/src/main.rs", "src/con.rs"] {
            assert_eq!(source_relative_project_path(source, "worker.rs"), None);
        }
        for reference in [
            "",
            "/worker.rs",
            r"\worker.rs",
            "//server/share/worker.rs",
            r"\\server\share\worker.rs",
            "C:/worker.rs",
            r"C:\worker.rs",
            "C:worker.rs",
            "./C:worker.rs",
            "./dir/node:worker.rs",
            "./con.rs",
            "./worker.rs.",
            "./worker.rs ",
            "./dir//worker.rs",
            "./work*er.rs",
        ] {
            assert_eq!(
                source_relative_project_path("src/main.rs", reference),
                None,
                "accepted {reference:?}",
            );
        }
        assert_eq!(source_relative_project_path("main.rs", "."), None);
        assert_eq!(
            source_relative_project_path("src/main.rs", ".."),
            None,
            "a terminal parent that resolves to the project root has no path identity",
        );
    }
}
