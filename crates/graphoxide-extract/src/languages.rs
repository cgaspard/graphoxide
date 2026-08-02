//! Language registry: maps file suffixes to tree-sitter grammars.
//!
//! Tier-1 grammars are compiled in; the full upstream matrix is tracked in
//! HANDOFF.md § "Language matrix".

use std::path::Path;
use tree_sitter::Language;

pub struct Lang {
    pub name: &'static str,
    pub language: fn() -> Language,
}

/// Look up the language for a file path by extension.
pub fn for_path(path: &Path) -> Option<&'static Lang> {
    let ext = path.extension()?.to_str()?;
    LANGUAGES
        .iter()
        .find(|(exts, _)| exts.split(',').any(|e| e == ext))
        .map(|(_, l)| l)
}

pub fn named(name: &str) -> Option<&'static Lang> {
    LANGUAGES
        .iter()
        .map(|(_, language)| language)
        .find(|language| language.name == name)
}

static LANGUAGES: &[(&str, Lang)] = &[
    (
        "py,pyi",
        Lang {
            name: "python",
            language: || tree_sitter_python::LANGUAGE.into(),
        },
    ),
    (
        "js,mjs,cjs,jsx",
        Lang {
            name: "javascript",
            language: || tree_sitter_javascript::LANGUAGE.into(),
        },
    ),
    (
        "ts,mts,cts",
        Lang {
            name: "typescript",
            language: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        },
    ),
    (
        "tsx",
        Lang {
            name: "tsx",
            language: || tree_sitter_typescript::LANGUAGE_TSX.into(),
        },
    ),
    (
        "go",
        Lang {
            name: "go",
            language: || tree_sitter_go::LANGUAGE.into(),
        },
    ),
    (
        "rs",
        Lang {
            name: "rust",
            language: || tree_sitter_rust::LANGUAGE.into(),
        },
    ),
    (
        "java",
        Lang {
            name: "java",
            language: || tree_sitter_java::LANGUAGE.into(),
        },
    ),
    (
        "c,h",
        Lang {
            name: "c",
            language: || tree_sitter_c::LANGUAGE.into(),
        },
    ),
    (
        "cc,cpp,cxx,hpp,hh",
        Lang {
            name: "cpp",
            language: || tree_sitter_cpp::LANGUAGE.into(),
        },
    ),
    (
        "rb",
        Lang {
            name: "ruby",
            language: || tree_sitter_ruby::LANGUAGE.into(),
        },
    ),
    (
        "cs",
        Lang {
            name: "csharp",
            language: || tree_sitter_c_sharp::LANGUAGE.into(),
        },
    ),
    (
        "sh,bash,zsh",
        Lang {
            name: "bash",
            language: || tree_sitter_bash::LANGUAGE.into(),
        },
    ),
    (
        "json",
        Lang {
            name: "json",
            language: || tree_sitter_json::LANGUAGE.into(),
        },
    ),
];
