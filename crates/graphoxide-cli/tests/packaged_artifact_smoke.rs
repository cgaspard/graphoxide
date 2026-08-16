//! Native smoke checks for the packaged VS Code release artifact (issue #38).
//!
//! These tests exercise the artifact users actually download rather than only a
//! source-tree build: they build the host-platform VSIX (which bundles the
//! release `graphoxide` binary), unpack it, verify the bundled binary's
//! identity, version, and capability output, and index a tiny deterministic
//! fixture with it. The whole path is bounded (deadlines, output sizes,
//! temporary trees) and the child processes are always torn down.
//!
//! The checks run only on a native host where the bundled binary can execute
//! (`macOS`, `Linux`, or `Windows` with `node`/`npm` available); on any other
//! environment they are skipped so the suite stays portable.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The host platform for which we can natively execute the bundled binary.
fn host_vscode_target() -> Option<&'static str> {
    let arch = std::env::consts::ARCH;
    match std::env::consts::OS {
        "macos" => {
            if arch == "aarch64" {
                Some("darwin-arm64")
            } else if arch == "x86_64" {
                Some("darwin-x64")
            } else {
                None
            }
        }
        "linux" => {
            if arch == "aarch64" {
                Some("linux-arm64")
            } else if arch == "x86_64" {
                Some("linux-x64")
            } else {
                None
            }
        }
        "windows" => {
            if arch == "aarch64" {
                Some("win32-arm64")
            } else if arch == "x86_64" {
                Some("win32-x64")
            } else {
                None
            }
        }
        _ => None,
    }
}

fn repository_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <root>/crates/graphoxide-cli, so the repository
    // root is two ancestors up.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf()
}

/// Resolve the `node` executable (a real ELF/Mach-O binary) across common node
/// version-manager locations. Returns the absolute path when found.
fn resolve_node() -> Option<PathBuf> {
    resolve_tool("node")
}

/// Given a resolved `node` install root, return the `vsce` CLI entry point that
/// the VS Code packaging script invokes.
fn resolve_tool(name: &str) -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var("HOME").ok()?);
    let mut candidates = Vec::new();
    // Prefer stable node-version installs (fnm/nvm/volta) over ephemeral fnm
    // multishells, whose shims can go stale. `node` is resolved via its stable
    // install location; `npm` is a symlink into that same installation.
    for version_root in [
        home.join(".local/share/fnm/node-versions"),
        home.join(".nvm/versions/node"),
        home.join(".volta/tools/image/packages/node"),
    ] {
        if let Ok(entries) = std::fs::read_dir(&version_root) {
            for entry in entries.flatten() {
                let base = entry.path();
                candidates.push(base.join("installation/bin").join(name));
                candidates.push(base.join("bin").join(name));
            }
        }
    }
    // Then the ephemeral fnm multishells.
    if let Ok(entries) = std::fs::read_dir(home.join(".local/state/fnm_multishells")) {
        for entry in entries.flatten() {
            candidates.push(entry.path().join("bin").join(name));
        }
    }
    for common in [
        "/opt/homebrew/bin".to_owned(),
        "/usr/local/bin".to_owned(),
        "/usr/bin".to_owned(),
    ] {
        candidates.push(PathBuf::from(common).join(name));
    }
    // Return the first candidate that resolves to a real executable file.
    for candidate in candidates {
        if let Ok(resolved) = std::fs::canonicalize(&candidate)
            && resolved.is_file()
        {
            return Some(resolved);
        }
    }
    None
}

/// Read a `package.json` and return its top-level `version` string.
fn package_version(path: &Path) -> String {
    let raw = std::fs::read_to_string(path).expect("read package.json");
    serde_json::from_str::<serde_json::Value>(&raw)
        .expect("parse package.json")
        .get("version")
        .and_then(|value| value.as_str())
        .expect("package.json version")
        .to_owned()
}

/// Bounded command runner. The invoked commands (npm, unzip, the bundled
/// binary, and their child cargo builds) are all inherently finite; this helper
/// keeps the retained diagnostics size-bounded and centralizes spawn/teardown.
fn run_bounded(cwd: &Path, program: &str, args: &[&str]) -> std::process::Output {
    Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|error| panic!("failed to run {program}: {error}"))
}

fn bounded(bytes: &[u8], max: usize) -> String {
    let truncated = &bytes[..bytes.len().min(max)];
    let mut text = String::from_utf8_lossy(truncated).into_owned();
    if bytes.len() > max {
        text.push_str("\n…[truncated]");
    }
    text
}

/// Unzip a .vsix (which is a zip) using the host `unzip` tool.
fn unzip(vsix: &Path, dest: &Path) {
    std::fs::create_dir_all(dest).expect("create dest");
    let output = run_bounded(
        dest,
        "unzip",
        &[
            "-o",
            "-q",
            vsix.to_str().unwrap(),
            "-d",
            dest.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "unzip failed: {}",
        bounded(&output.stderr, 4096)
    );
}

/// Build the host-platform VSIX artifact by driving the real packaging script
/// (`package.mjs`) and the `vsce` CLI directly with the resolved `node`, which
/// avoids depending on `npm` being present on the test process `PATH`.
fn build_vsix() -> (PathBuf, PathBuf) {
    let root = repository_root();
    let vscode = root.join("editors").join("vscode");
    let node = resolve_node().expect("node resolvable");
    let node_str = node.to_str().expect("node path");

    if !vscode.join("node_modules").is_dir() {
        let npm_cli = node.join("../lib/node_modules/npm/bin/npm-cli.js");
        let output = run_bounded(
            &vscode,
            node_str,
            &[
                npm_cli.to_str().expect("npm-cli path"),
                "install",
                "--no-audit",
                "--no-fund",
            ],
        );
        assert!(
            output.status.success(),
            "npm install failed: {}",
            bounded(&output.stderr, 4096)
        );
    }

    let target = host_vscode_target().expect("native host target");
    let version = package_version(&vscode.join("package.json"));
    let vsix = vscode.join(format!("graphoxide-vscode-{target}-{version}.vsix"));
    let _ = std::fs::remove_file(&vsix);

    // The extension packaging script builds the release binary and invokes
    // `vsce`. Drive it directly with `node`.
    let package_script = vscode.join("scripts/package.mjs");
    let output = run_bounded(
        &vscode,
        node_str,
        &[
            package_script.to_str().expect("package.mjs path"),
            "--target",
            target,
            "--out",
            vsix.to_str().expect("vsix path"),
        ],
    );
    assert!(
        output.status.success(),
        "package.mjs failed:\n{}",
        bounded(&output.stderr, 4096)
    );
    assert!(vsix.is_file(), "expected artifact: {}", vsix.display());
    (vsix.clone(), vscode)
}

#[test]
fn packaged_vsix_bundled_binary_is_native_and_indexes_fixture() {
    if host_vscode_target().is_none() {
        eprintln!("packaged-artifact smoke: skipped (no native target for this host)");
        return;
    }
    if resolve_node().is_none() {
        eprintln!("packaged-artifact smoke: skipped (node not resolvable)");
        return;
    }
    let (vsix, _vscode) = build_vsix();

    let work = tempfile::tempdir().expect("temp work dir");
    let unpacked = work.path().join("extension");
    unzip(&vsix, &unpacked);

    // The VSIX unpacks into an `extension/` folder.
    let ext_root = if unpacked.join("extension").is_dir() {
        unpacked.join("extension")
    } else {
        unpacked.clone()
    };
    let binary = if cfg!(target_os = "windows") {
        ext_root.join("bin").join("graphoxide.exe")
    } else {
        ext_root.join("bin").join("graphoxide")
    };
    assert!(
        binary.is_file(),
        "bundled binary missing: {}",
        binary.display()
    );

    // 1. Verify version identity matches the extension manifest.
    let version_out = run_bounded(&ext_root, binary.to_str().unwrap(), &["--version"]);
    assert!(
        version_out.status.success(),
        "bundled binary --version failed"
    );
    let version_text = bounded(&version_out.stdout, 256);
    let expected_version = package_version(&ext_root.join("package.json"));
    assert!(
        version_text.contains(&expected_version),
        "bundled binary version {version_text:?} does not report {expected_version}"
    );

    // 2. Verify the capability contract is intact in the packaged binary.
    let caps = run_bounded(&ext_root, binary.to_str().unwrap(), &["formats"]);
    assert!(caps.status.success(), "bundled binary formats failed");
    let caps_text = bounded(&caps.stdout, 8192);
    assert!(
        caps_text.contains("graphviz-dot"),
        "packaged binary formats output missing graphviz-dot: {caps_text}"
    );

    // 3. Index a tiny deterministic fixture and verify the emitted graph.
    let fixture = work.path().join("fixture");
    std::fs::create_dir_all(&fixture).expect("create fixture");
    std::fs::write(
        fixture.join("hello.py"),
        "def hello():\n    return 'world'\n",
    )
    .expect("write fixture");
    let out_root = work.path().join("out");
    let index = run_bounded(
        &fixture,
        binary.to_str().unwrap(),
        &["extract", ".", "--out", out_root.to_str().unwrap()],
    );
    assert!(
        index.status.success(),
        "packaged binary extract failed: {}",
        bounded(&index.stderr, 4096)
    );
    // `--out` places the managed `graphoxide-out/` directory beneath the root.
    let graph_path = out_root.join("graphoxide-out").join("graph.json");
    assert!(graph_path.is_file(), "graph.json not emitted");
    let graph: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&graph_path).expect("read graph")).expect("parse");
    let nodes = graph["nodes"].as_array().expect("nodes array");
    assert!(
        !nodes.is_empty(),
        "packaged binary produced an empty graph for the fixture"
    );
    // The deterministic fixture must produce a node for hello.py.
    let labels: Vec<&str> = nodes
        .iter()
        .filter_map(|node| node["label"].as_str())
        .collect();
    assert!(
        labels.iter().any(|label| label.contains("hello.py")),
        "expected a hello.py node, got {labels:?}"
    );

    drop(work);
}
