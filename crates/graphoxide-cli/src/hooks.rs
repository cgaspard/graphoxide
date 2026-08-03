//! Git-hook installation and native detached rebuild launching.
//!
//! Hooks run in unusually constrained environments: GUI Git clients often have
//! a reduced `PATH`, Git for Windows ships only a small POSIX shell, and linked
//! worktrees can share one hook directory.  The generated scripts therefore do
//! only cheap repository checks and hand the rebuild to a pinned Graphoxide
//! binary.  The binary performs the platform-specific detach and supervises the
//! real rebuild with a timeout.

use anyhow::{anyhow, bail, Context, Result};
use std::{
    env, fs,
    fs::OpenOptions,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
    thread,
    time::{Duration, Instant},
};

pub const HOOK_MARKER: &str = "# graphoxide-hook-start";
pub const HOOK_MARKER_END: &str = "# graphoxide-hook-end";
pub const CHECKOUT_MARKER: &str = "# graphoxide-checkout-hook-start";
pub const CHECKOUT_MARKER_END: &str = "# graphoxide-checkout-hook-end";
pub const MERGE_ATTRIBUTE: &str = "graphoxide-out/graph.json merge=graphoxide";

/// Flags used by the Windows supervisor launch.
pub const WINDOWS_DETACH_FLAGS: u32 = 0x0800_0000 | 0x0000_0200 | 0x0000_0008;

/// Shared shell guard used by both generated hooks.
pub const WORKTREE_GUARD: &str = r#"_GOX_GITDIR=$(cd "$(git rev-parse --git-dir 2>/dev/null)" 2>/dev/null && pwd)
_GOX_COMMONDIR=$(cd "$(git rev-parse --git-common-dir 2>/dev/null)" 2>/dev/null && pwd)
if [ -n "$_GOX_COMMONDIR" ] && [ "$_GOX_GITDIR" != "$_GOX_COMMONDIR" ]; then
    exit 0
fi
"#;

const COMMON_GUARDS: &str = r#"# Keep hook-triggered extraction sequential on Git for Windows unless explicitly overridden.
if [ -n "${WINDIR:-}" ] || [ -n "${MSYSTEM:-}" ]; then
    export GRAPHOXIDE_MAX_WORKERS="${GRAPHOXIDE_MAX_WORKERS:-1}"
    export RAYON_NUM_THREADS="${RAYON_NUM_THREADS:-$GRAPHOXIDE_MAX_WORKERS}"
fi

# Git exports GIT_DIR to hooks; only resolve it when this script is run by hand.
GIT_DIR=${GIT_DIR:-$(git rev-parse --git-dir 2>/dev/null)}
[ -d "$GIT_DIR/rebase-merge" ] && exit 0
[ -d "$GIT_DIR/rebase-apply" ] && exit 0
[ -f "$GIT_DIR/MERGE_HEAD" ] && exit 0
[ -f "$GIT_DIR/CHERRY_PICK_HEAD" ] && exit 0

[ "${GRAPHOXIDE_SKIP_HOOK:-${GRAPHIFY_SKIP_HOOK:-0}}" = "1" ] && exit 0

"#;

const BINARY_DETECT: &str = r#"# Prefer the binary recorded at install time; GUI clients frequently omit its directory from PATH.
_GRAPHOXIDE_BIN='__PINNED_EXECUTABLE__'
if [ -z "$_GRAPHOXIDE_BIN" ] || [ ! -x "$_GRAPHOXIDE_BIN" ]; then
    _GRAPHOXIDE_BIN=$(command -v graphoxide 2>/dev/null || true)
fi
if [ -z "$_GRAPHOXIDE_BIN" ]; then
    echo "[graphoxide hook] could not locate the graphoxide binary. Re-run 'graphoxide hook install' from the environment where graphoxide is installed." >&2
    exit 0
fi
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookMode {
    PostCommit,
    PostCheckout,
}

impl HookMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PostCommit => "post-commit",
            Self::PostCheckout => "post-checkout",
        }
    }
}

impl FromStr for HookMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "post-commit" | "commit" => Ok(Self::PostCommit),
            "post-checkout" | "checkout" => Ok(Self::PostCheckout),
            _ => bail!("unknown hook mode {value:?}"),
        }
    }
}

/// Reject characters that can change shell parsing while accepting normal
/// POSIX and Windows paths, including profile directories containing spaces.
pub fn pinned_executable(executable: &Path) -> Option<&str> {
    let value = executable.to_str()?;
    (!value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '/' | '_' | '.' | '@' | ':' | ' ' | '\\' | '-')
        }))
    .then_some(value)
}

fn binary_detect(executable: &Path) -> String {
    BINARY_DETECT.replace(
        "__PINNED_EXECUTABLE__",
        pinned_executable(executable).unwrap_or_default(),
    )
}

pub fn post_commit_script(executable: &Path) -> String {
    let mut script = String::new();
    script.push_str(HOOK_MARKER);
    script.push_str(
        r#"
# Auto-rebuilds the knowledge graph after each commit (code files only).
# Installed by: graphoxide hook install

"#,
    );
    script.push_str(COMMON_GUARDS);
    script.push_str(WORKTREE_GUARD);
    script.push_str(
        r#"
CHANGED=$(git diff --name-only HEAD~1 HEAD 2>/dev/null || git diff --name-only HEAD 2>/dev/null)
if [ -z "$CHANGED" ]; then
    exit 0
fi

# Avoid a rebuild loop when generated graph artifacts are tracked in Git.
_NON_GRAPH=$(printf '%s\n' "$CHANGED" | grep -v -E '^(graphoxide-out|graphify-out)/' || true)
if [ -z "$_NON_GRAPH" ]; then
    exit 0
fi

"#,
    );
    script.push_str(&binary_detect(executable));
    script.push_str(
        r#"
export GRAPHOXIDE_CHANGED="$CHANGED"
_GRAPHOXIDE_LOG="${GRAPHOXIDE_REBUILD_LOG:-${HOME}/.cache/graphoxide-rebuild.log}"
mkdir -p "$(dirname "$_GRAPHOXIDE_LOG")"
echo "[graphoxide hook] launching background rebuild (log: $_GRAPHOXIDE_LOG)"
"$_GRAPHOXIDE_BIN" hook-launch post-commit . "$_GRAPHOXIDE_LOG"
"#,
    );
    script.push_str(HOOK_MARKER_END);
    script.push('\n');
    script
}

pub fn post_checkout_script(executable: &Path) -> String {
    let mut script = String::new();
    script.push_str(CHECKOUT_MARKER);
    script.push_str(
        r#"
# Auto-rebuilds the knowledge graph after switching branches.
# Installed by: graphoxide hook install

PREV_HEAD=$1
NEW_HEAD=$2
BRANCH_SWITCH=$3
[ "$BRANCH_SWITCH" = "1" ] || exit 0

_GRAPHOXIDE_OUT="${GRAPHOXIDE_OUT:-${GRAPHIFY_OUT:-graphoxide-out}}"
[ -d "$_GRAPHOXIDE_OUT" ] || exit 0

"#,
    );
    script.push_str(COMMON_GUARDS);
    script.push_str(WORKTREE_GUARD);
    script.push('\n');
    script.push_str(&binary_detect(executable));
    script.push_str(
        r#"
_GRAPHOXIDE_LOG="${GRAPHOXIDE_REBUILD_LOG:-${HOME}/.cache/graphoxide-rebuild.log}"
mkdir -p "$(dirname "$_GRAPHOXIDE_LOG")"
echo "[graphoxide hook] branch switched - launching background rebuild (log: $_GRAPHOXIDE_LOG)"
"$_GRAPHOXIDE_BIN" hook-launch post-checkout . "$_GRAPHOXIDE_LOG"
"#,
    );
    script.push_str(CHECKOUT_MARKER_END);
    script.push('\n');
    script
}

pub fn find_git_root(path: &Path) -> Option<PathBuf> {
    let start = path.canonicalize().ok()?;
    let start = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start
    };
    start
        .ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .map(Path::to_path_buf)
}

fn windows_style_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\'))
        || value.contains('\\')
}

/// Validate and anchor the single-line output of `git rev-parse --git-path hooks`.
/// `None` means the caller should use the conventional `.git/hooks` fallback.
pub fn resolve_hooks_output(
    root: &Path,
    stdout: &str,
    reject_windows: bool,
) -> Result<Option<PathBuf>> {
    let raw = stdout.trim();
    if raw.is_empty() || raw.contains(['\n', '\r', '\0']) {
        return Ok(None);
    }
    if reject_windows && windows_style_path(raw) {
        bail!(
            "git hooks path looks like a Windows path on POSIX: {raw:?}; unset core.hooksPath or configure a POSIX path"
        );
    }
    Ok(Some(if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        root.join(raw)
    }))
}

pub fn hooks_dir(root: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--git-path", "hooks"])
        .output();
    let resolved = match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            resolve_hooks_output(root, &stdout, !cfg!(windows))?
        }
        _ => None,
    };
    let directory = resolved.unwrap_or_else(|| root.join(".git/hooks"));
    fs::create_dir_all(&directory)
        .with_context(|| format!("could not create hooks directory {}", directory.display()))?;
    let directory = directory.canonicalize().unwrap_or(directory);
    Ok(if directory.file_name().is_some_and(|name| name == "_") {
        directory
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(directory)
    } else {
        directory
    })
}

fn install_hook(path: &Path, script: &str, marker: &str) -> Result<String> {
    if path.exists() {
        let content = fs::read_to_string(path)
            .with_context(|| format!("could not read existing hook {}", path.display()))?;
        if content.contains(marker) {
            return Ok(format!("already installed at {}", path.display()));
        }
        let mut merged = content.trim_end().to_owned();
        merged.push_str("\n\n");
        merged.push_str(script);
        write_text(path, &merged)?;
    } else {
        write_text(path, &("#!/bin/sh\n".to_owned() + script))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(permissions.mode() | 0o111);
        fs::set_permissions(path, permissions)?;
    }
    Ok(format!("installed at {}", path.display()))
}

fn uninstall_hook(path: &Path, marker: &str, marker_end: &str) -> Result<String> {
    if !path.exists() {
        return Ok(format!(
            "no {} hook found - nothing to remove.",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    let content = fs::read_to_string(path)?;
    let Some(start) = content.find(marker) else {
        return Ok(format!(
            "graphoxide hook not found in {} - nothing to remove.",
            path.display()
        ));
    };
    let Some(relative_end) = content[start..].find(marker_end) else {
        bail!("managed hook block in {} has no end marker", path.display());
    };
    let mut end = start + relative_end + marker_end.len();
    if content.as_bytes().get(end) == Some(&b'\n') {
        end += 1;
    }
    let mut retained = String::with_capacity(content.len() - (end - start));
    retained.push_str(&content[..start]);
    retained.push_str(&content[end..]);
    let trimmed = retained.trim();
    if trimmed.is_empty() || matches!(trimmed, "#!/bin/sh" | "#!/bin/bash") {
        fs::remove_file(path)?;
        Ok(format!("removed {} hook", path.display()))
    } else {
        write_text(path, &(retained.trim_end().to_owned() + "\n"))?;
        Ok(format!(
            "graphoxide removed from {} (other hook content preserved)",
            path.display()
        ))
    }
}

fn shell_driver(executable: &Path) -> String {
    pinned_executable(executable).map_or_else(
        || "graphoxide merge-driver %O %A %B".to_owned(),
        |value| format!("\"{value}\" merge-driver %O %A %B"),
    )
}

fn is_merge_attribute(line: &str) -> bool {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return false;
    }
    let fields = line.split_whitespace().collect::<Vec<_>>();
    fields
        .first()
        .is_some_and(|field| field.ends_with("graph.json"))
        && fields[1..].contains(&"merge=graphoxide")
}

fn register_merge_driver(root: &Path, executable: &Path) -> Result<String> {
    for (key, value) in [
        (
            "merge.graphoxide.name",
            "Graphoxide graph.json union merge".to_owned(),
        ),
        ("merge.graphoxide.driver", shell_driver(executable)),
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["config", key, &value])
            .output()
            .with_context(|| format!("could not run git config for {key}"))?;
        if !output.status.success() {
            bail!(
                "git config {key} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    let attributes = root.join(".gitattributes");
    let mut content = fs::read_to_string(&attributes).unwrap_or_default();
    if content.lines().any(is_merge_attribute) {
        return Ok("already registered".to_owned());
    }
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(MERGE_ATTRIBUTE);
    content.push('\n');
    write_text(&attributes, &content)?;
    Ok("registered".to_owned())
}

fn unregister_merge_driver(root: &Path) -> Result<String> {
    for key in ["merge.graphoxide.name", "merge.graphoxide.driver"] {
        let _ = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["config", "--unset", key])
            .output();
    }
    let attributes = root.join(".gitattributes");
    if !attributes.exists() {
        return Ok("not registered - nothing to remove.".to_owned());
    }
    let content = fs::read_to_string(&attributes)?;
    let kept = content
        .lines()
        .filter(|line| !is_merge_attribute(line))
        .collect::<Vec<_>>();
    if kept.len() == content.lines().count() {
        return Ok("not registered - nothing to remove.".to_owned());
    }
    if kept.is_empty() {
        fs::remove_file(attributes)?;
    } else {
        write_text(&attributes, &(kept.join("\n") + "\n"))?;
    }
    Ok("removed".to_owned())
}

fn merge_driver_status(root: &Path) -> &'static str {
    let configured = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["config", "--get", "merge.graphoxide.driver"])
        .output()
        .is_ok_and(|output| output.status.success() && !output.stdout.is_empty());
    let attributed = fs::read_to_string(root.join(".gitattributes"))
        .is_ok_and(|content| content.lines().any(is_merge_attribute));
    match (configured, attributed) {
        (true, true) => "registered",
        (true, false) => "partially registered (git config only)",
        (false, true) => "partially registered (.gitattributes only)",
        (false, false) => "not registered",
    }
}

pub fn install(path: &Path, executable: &Path) -> Result<String> {
    let root = find_git_root(path)
        .ok_or_else(|| anyhow!("No git repository found at or above {}", path.display()))?;
    let directory = hooks_dir(&root)?;
    let commit = install_hook(
        &directory.join("post-commit"),
        &post_commit_script(executable),
        HOOK_MARKER,
    )?;
    let checkout = install_hook(
        &directory.join("post-checkout"),
        &post_checkout_script(executable),
        CHECKOUT_MARKER,
    )?;
    let merge = register_merge_driver(&root, executable)?;
    Ok(format!(
        "post-commit: {commit}\npost-checkout: {checkout}\nmerge driver: {merge}"
    ))
}

pub fn uninstall(path: &Path) -> Result<String> {
    let root = find_git_root(path)
        .ok_or_else(|| anyhow!("No git repository found at or above {}", path.display()))?;
    let directory = hooks_dir(&root)?;
    let commit = uninstall_hook(&directory.join("post-commit"), HOOK_MARKER, HOOK_MARKER_END)?;
    let checkout = uninstall_hook(
        &directory.join("post-checkout"),
        CHECKOUT_MARKER,
        CHECKOUT_MARKER_END,
    )?;
    let merge = unregister_merge_driver(&root)?;
    Ok(format!(
        "post-commit: {commit}\npost-checkout: {checkout}\nmerge driver: {merge}"
    ))
}

pub fn status(path: &Path) -> String {
    let Some(root) = find_git_root(path) else {
        return "Not in a git repository.".to_owned();
    };
    let Ok(directory) = hooks_dir(&root) else {
        return "Git hooks path could not be resolved.".to_owned();
    };
    let check = |name: &str, marker: &str| {
        let path = directory.join(name);
        match fs::read_to_string(path) {
            Ok(content) if content.contains(marker) => "installed",
            Ok(_) => "not installed (hook exists but graphoxide not found)",
            Err(_) => "not installed",
        }
    };
    format!(
        "post-commit: {}\npost-checkout: {}\nmerge driver: {}",
        check("post-commit", HOOK_MARKER),
        check("post-checkout", CHECKOUT_MARKER),
        merge_driver_status(&root)
    )
}

fn write_text(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content).with_context(|| format!("could not write {}", path.display()))
}

/// Spawn the detached timeout supervisor. The hook waits only for this cheap
/// spawn operation, never for graph extraction.
pub fn launch_detached(mode: HookMode, root: &Path, log: &Path) -> Result<u32> {
    if let Some(parent) = log.parent() {
        fs::create_dir_all(parent)?;
    }
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("could not open hook log {}", log.display()))?;
    let stderr = stdout.try_clone()?;
    let executable = env::current_exe().context("could not resolve current graphoxide binary")?;
    let mut command = Command::new(executable);
    command
        .arg("hook-supervise")
        .arg(mode.as_str())
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    configure_detached(&mut command);
    let child = command
        .spawn()
        .context("could not launch hook supervisor")?;
    Ok(child.id())
}

#[cfg(unix)]
fn configure_detached(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn configure_detached(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(WINDOWS_DETACH_FLAGS);
}

#[cfg(not(any(unix, windows)))]
fn configure_detached(_command: &mut Command) {}

pub fn rebuild_timeout_from(value: Option<&str>) -> Duration {
    let seconds = value
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(600);
    Duration::from_secs(seconds)
}

/// Run in the detached supervisor process and terminate a wedged rebuild after
/// `GRAPHOXIDE_REBUILD_TIMEOUT` seconds. Zero disables the deadline.
pub fn supervise(mode: HookMode, root: &Path) -> Result<()> {
    let executable = env::current_exe().context("could not resolve current graphoxide binary")?;
    let mut child = Command::new(executable)
        .arg("hook-rebuild")
        .arg(mode.as_str())
        .arg(root)
        .stdin(Stdio::null())
        .spawn()
        .context("could not start hook rebuild")?;
    let timeout = rebuild_timeout_from(
        env::var("GRAPHOXIDE_REBUILD_TIMEOUT")
            .ok()
            .or_else(|| env::var("GRAPHIFY_REBUILD_TIMEOUT").ok())
            .as_deref(),
    );
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            bail!("graphoxide hook rebuild exited with {status}");
        }
        if !timeout.is_zero() && started.elapsed() >= timeout {
            child
                .kill()
                .context("could not terminate timed-out hook rebuild")?;
            let _ = child.wait();
            bail!("graphoxide hook rebuild exceeded {}s", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Perform the actual rebuild inside the supervised child.
pub fn rebuild(mode: HookMode, root: &Path) -> Result<()> {
    let force = env_truthy("GRAPHOXIDE_FORCE") || env_truthy("GRAPHIFY_FORCE");
    let changed_paths = match mode {
        HookMode::PostCommit => Some(
            env::var("GRAPHOXIDE_CHANGED")
                .or_else(|_| env::var("GRAPHIFY_CHANGED"))
                .unwrap_or_default()
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(PathBuf::from)
                .collect::<Vec<_>>(),
        ),
        HookMode::PostCheckout => None,
    };
    if changed_paths.as_ref().is_some_and(Vec::is_empty) {
        return Ok(());
    }
    let result = crate::watch::rebuild_project(
        root,
        &crate::watch::RebuildOptions {
            changed_paths,
            force,
            acquire_lock: true,
            block_on_lock: false,
            ..Default::default()
        },
    )?;
    for warning in result.warnings {
        eprintln!("[graphoxide hook] {warning}");
    }
    println!("[graphoxide hook] rebuild status: {:?}", result.status);
    Ok(())
}

fn env_truthy(name: &str) -> bool {
    env::var(name)
        .is_ok_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}
