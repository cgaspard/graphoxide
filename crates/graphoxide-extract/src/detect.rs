//! Gitignore-aware source-file collection.

use std::path::{Path, PathBuf};

const CODE_EXTENSIONS: &[&str] = &[
    "py", "pyi", "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "ejs", "ets", "go", "rs",
    "java", "groovy", "gradle", "cpp", "cc", "cxx", "c", "h", "hpp", "cu", "cuh", "metal", "rb",
    "rake", "swift", "kt", "kts", "cs", "scala", "php", "lua", "luau", "toc", "zig", "ps1", "psm1",
    "psd1", "ex", "exs", "m", "mm", "jl", "vue", "svelte", "astro", "dart", "v", "sv", "svh",
    "sql", "r", "f", "F", "f90", "F90", "f95", "F95", "f03", "F03", "f08", "F08", "pas", "pp",
    "dpr", "dpk", "lpr", "inc", "dfm", "lfm", "lpk", "sh", "bash", "json", "tf", "tfvars", "hcl",
    "dm", "dme", "dmi", "dmm", "dmf", "sln", "slnx", "csproj", "fsproj", "vbproj", "xaml", "razor",
    "cshtml", "cls", "trigger", "toml", "mod", "xml", "yaml", "yml",
];
const SKIP_FILES: &[&str] = &[
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Cargo.lock",
    "poetry.lock",
    "uv.lock",
];
const SKIP_DIRS: &[&str] = &[
    "venv",
    ".venv",
    "node_modules",
    "__pycache__",
    ".git",
    "dist",
    "build",
    "target",
    "out",
    "site-packages",
    "lib64",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".nox",
    ".eggs",
    "graphoxide-out",
    "lcov-report",
    "visual-tests",
    "visual-test",
    "__snapshots__",
    "storybook-static",
    "dist-protected",
    ".next",
    ".nuxt",
    ".turbo",
    ".angular",
    ".idea",
    ".cache",
    ".parcel-cache",
    ".svelte-kit",
    ".terraform",
    ".serverless",
    ".graphoxide",
    ".worktrees",
];

pub fn collect_files(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .add_custom_ignore_filename(".graphoxideignore")
        .hidden(false)
        .filter_entry(|entry| {
            !entry
                .file_type()
                .is_some_and(|kind| kind.is_dir() && noise_dir(entry.path()))
        })
        .build();
    for entry in walker {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type().is_some_and(|kind| kind.is_file()) || sensitive(path) {
            continue;
        }
        let name = path.file_name().and_then(|v| v.to_str()).unwrap_or("");
        if SKIP_FILES.contains(&name) {
            continue;
        }
        let supported = path
            .extension()
            .and_then(|v| v.to_str())
            .is_some_and(|ext| CODE_EXTENSIONS.contains(&ext))
            || (path.extension().is_none() && has_code_shebang(path));
        if supported {
            files.push(path.to_path_buf());
        }
    }
    files.sort();
    Ok(files)
}

fn noise_dir(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if SKIP_DIRS.contains(&name) || name.ends_with(".egg-info") || name.ends_with("_venv") {
        return true;
    }
    if name == "worktrees"
        && path
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|value| value.to_str())
            .is_some_and(|parent| parent.starts_with('.'))
    {
        return true;
    }
    if name == "env" || name == ".env" || name.ends_with("_env") {
        return path.join("pyvenv.cfg").is_file()
            || path.join("bin/activate").is_file()
            || path.join("Scripts/activate").is_file()
            || path.join("conda-meta").is_dir()
            || std::fs::read_dir(path.join("lib"))
                .ok()
                .is_some_and(|entries| {
                    entries.filter_map(Result::ok).any(|entry| {
                        entry.file_type().ok().is_some_and(|kind| kind.is_dir())
                            && entry.file_name().to_string_lossy().starts_with("python")
                    })
                });
    }
    if name == "coverage" {
        return [
            "lcov.info",
            "coverage-final.json",
            "clover.xml",
            "cobertura-coverage.xml",
        ]
        .iter()
        .any(|artifact| path.join(artifact).is_file())
            || ["lcov-report", "htmlcov"]
                .iter()
                .any(|artifact| path.join(artifact).is_dir());
    }
    if name == "snapshots" {
        if path
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|value| value.to_str())
            .is_some_and(|parent| matches!(parent, "__tests__" | "__test__"))
        {
            return true;
        }
        return std::fs::read_dir(path).ok().is_some_and(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                entry.path().extension().and_then(|value| value.to_str()) == Some("snap")
            })
        });
    }
    false
}

/// Whether a changed path belongs to the offline structural extraction tier.
pub fn is_supported_path(path: &Path) -> bool {
    let name = path.file_name().and_then(|v| v.to_str()).unwrap_or("");
    !SKIP_FILES.contains(&name)
        && !sensitive(path)
        && (path
            .extension()
            .and_then(|v| v.to_str())
            .is_some_and(|ext| CODE_EXTENSIONS.contains(&ext))
            || (path.extension().is_none() && has_code_shebang(path)))
}

fn sensitive(path: &Path) -> bool {
    let lower = path.to_string_lossy().to_lowercase();
    if ["/.ssh/", "/.gnupg/", "/.aws/", "/.gcloud/"]
        .iter()
        .any(|part| lower.contains(part))
    {
        return true;
    }
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("")
        .to_lowercase();
    let source_extension = path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "py" | "pyi"
                    | "js"
                    | "jsx"
                    | "ts"
                    | "tsx"
                    | "go"
                    | "rs"
                    | "java"
                    | "c"
                    | "h"
                    | "cc"
                    | "cpp"
                    | "cxx"
                    | "hpp"
                    | "cs"
                    | "rb"
                    | "php"
                    | "swift"
                    | "kt"
                    | "kts"
                    | "scala"
                    | "lua"
                    | "zig"
                    | "sh"
                    | "bash"
                    | "ps1"
                    | "pas"
                    | "dart"
                    | "ex"
                    | "exs"
            )
        });
    let credential_name = [
        "aws_credentials",
        "gcloud_credentials",
        "service_account",
        "client_secret",
        "access_token",
        "refresh_token",
        "secret_key",
    ]
    .iter()
    .any(|keyword| name.contains(keyword));
    name == ".env"
        || name.starts_with(".env.")
            && ![".example", ".sample", ".template", ".dist"]
                .iter()
                .any(|suffix| name.ends_with(suffix))
        || [
            ".pem", ".key", ".p12", ".pfx", ".cert", ".crt", ".der", ".p8",
        ]
        .iter()
        .any(|suffix| name.ends_with(suffix))
        || (credential_name && !source_extension)
}

fn has_code_shebang(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut bytes = [0; 256];
    let Ok(read) = file.read(&mut bytes) else {
        return false;
    };
    let head = String::from_utf8_lossy(&bytes[..read]).to_lowercase();
    head.starts_with("#!")
        && ["python", "node", "bash", "sh", "ruby", "perl", "php"]
            .iter()
            .any(|name| head.lines().next().is_some_and(|line| line.contains(name)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_sensitive_names() {
        assert!(sensitive(Path::new("x/.ssh/tool.py")));
        assert!(sensitive(Path::new(".env.local")));
        assert!(sensitive(Path::new("service_account.json")));
        assert!(!sensitive(Path::new("client_secret.py")));
        assert!(!sensitive(Path::new(".env.example")));
    }
}
