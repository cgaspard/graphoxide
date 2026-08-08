//! Executable Rust port of pinned Graphify `tests/test_detect.py`.

use filetime::{set_file_mtime, FileTime};
use graphoxide_extract::detect::{
    classify_file, collect_files, convert_office_text, count_words, detect, detect_incremental,
    is_ignored, is_ignored_with_cache, is_noise_dir, is_sensitive, load_ignore_patterns,
    load_manifest, save_manifest, shebang_interpreter, DetectOptions, DetectResult, DetectedFiles,
    FileType, IgnorePattern, ManifestKind, SaveManifestOptions, MAX_IGNORE_PATTERNS_PER_SOURCE,
    MAX_IGNORE_PATTERN_BYTES, MAX_IGNORE_SOURCE_BYTES, WORD_COUNT_MAX_BYTES,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use tempfile::TempDir;

fn fixture() -> TempDir {
    tempfile::tempdir().expect("temporary detector fixture")
}

fn init_git_marker(root: &Path) {
    write(root, ".git/HEAD", "ref: refs/heads/main\n");
}

fn write(root: &Path, relative: &str, contents: &str) -> PathBuf {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("fixture parent")).unwrap();
    fs::write(&path, contents).unwrap();
    path
}

fn write_bytes(root: &Path, relative: &str, contents: &[u8]) -> PathBuf {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("fixture parent")).unwrap();
    fs::write(&path, contents).unwrap();
    path
}

fn scan(root: &Path) -> DetectResult {
    detect(root, &DetectOptions::default()).unwrap()
}

fn scan_with(root: &Path, configure: impl FnOnce(&mut DetectOptions)) -> DetectResult {
    let mut options = DetectOptions::default();
    configure(&mut options);
    detect(root, &options).unwrap()
}

fn bucket<'a>(result: &'a DetectResult, name: &str) -> &'a [String] {
    result
        .files
        .get(name)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn all_files(result: &DetectResult) -> Vec<&str> {
    result
        .files
        .values()
        .flatten()
        .map(String::as_str)
        .collect()
}

fn has(result: &DetectResult, fragment: &str) -> bool {
    all_files(result).iter().any(|path| path.contains(fragment))
}

macro_rules! classify_case {
    ($name:ident, $path:literal, $expected:expr) => {
        #[test]
        fn $name() {
            assert_eq!(classify_file(Path::new($path)), $expected);
        }
    };
}

classify_case!(test_classify_python, "foo.py", Some(FileType::Code));
classify_case!(test_classify_typescript, "bar.ts", Some(FileType::Code));
classify_case!(
    test_classify_powershell_module,
    "Utils.psm1",
    Some(FileType::Code)
);
classify_case!(
    test_classify_powershell_manifest,
    "MyModule.psd1",
    Some(FileType::Code)
);
classify_case!(
    test_classify_markdown,
    "README.md",
    Some(FileType::Document)
);
classify_case!(
    test_classify_skill,
    "10_Orchestrator.skill",
    Some(FileType::Document)
);
classify_case!(test_classify_pdf, "paper.pdf", Some(FileType::Paper));
classify_case!(
    test_classify_pdf_in_xcassets_skipped,
    "MyApp/Images.xcassets/icon.imageset/icon.pdf",
    None
);
classify_case!(
    test_classify_pdf_in_xcassets_root_skipped,
    "Pods/HXPHPicker/Assets.xcassets/photo.pdf",
    None
);
classify_case!(test_classify_unknown_returns_none, "archive.zip", None);

#[test]
fn test_classify_image() {
    for path in ["screenshot.png", "design.jpg", "diagram.webp"] {
        assert_eq!(classify_file(Path::new(path)), Some(FileType::Image));
    }
}

#[test]
fn test_count_words_sample_md() {
    let fixture = fixture();
    let path = write(
        fixture.path(),
        "sample.md",
        "# Sample\n\nThis markdown fixture contains more than five words.",
    );
    assert!(count_words(&path) > 5);
}

#[test]
fn test_detect_finds_fixtures() {
    let fixture = fixture();
    write(fixture.path(), "main.py", "def main(): pass\n");
    write(fixture.path(), "README.md", "# Project\n\nDocumentation.\n");
    let result = scan(fixture.path());
    assert!(result.total_files >= 2);
    assert!(result.files.contains_key("code"));
    assert!(result.files.contains_key("document"));
}

#[test]
fn test_detect_warns_small_corpus() {
    let fixture = fixture();
    write(fixture.path(), "README.md", "tiny corpus");
    let result = scan(fixture.path());
    assert!(!result.needs_graph);
    assert!(result.warning.is_some());
}

#[test]
fn test_detect_skips_noise_dot_dirs() {
    let fixture = fixture();
    for directory in [".graphify", ".next", ".nuxt", ".turbo", ".angular"] {
        write(fixture.path(), &format!("{directory}/noise.py"), "x=1");
    }
    write(fixture.path(), ".github/workflows/real.yml", "name: CI");
    let result = scan(fixture.path());
    for path in all_files(&result) {
        for noise in [".graphify", ".next", ".nuxt", ".turbo", ".angular"] {
            assert!(!path.contains(noise));
        }
    }
    assert!(has(&result, ".github"));
}

#[test]
fn test_detect_skips_reserved_virtual_container_namespace_directories() {
    let fixture = fixture();
    write(fixture.path(), "literal!/file.rs", "fn literal() {}\n");
    write(fixture.path(), "ordinary/file.rs", "fn ordinary() {}\n");

    let result = scan(fixture.path());
    assert!(!has(&result, "literal!/file.rs"));
    assert!(has(&result, "ordinary/file.rs"));
    assert!(result.ignored.iter().any(|item| {
        item.contains("literal!") && item.contains("reserved for virtual container members")
    }));
}

#[test]
fn test_classify_md_paper_by_signals() {
    let fixture = fixture();
    let paper = write(
        fixture.path(),
        "paper.md",
        "# Abstract\nWe propose a method [1]. Journal preprint. ArXiv. Equation 3. \\cite{x}.",
    );
    assert_eq!(classify_file(&paper), Some(FileType::Paper));
}

#[test]
fn test_classify_md_doc_without_signals() {
    let fixture = fixture();
    let doc = write(fixture.path(), "notes.md", "# Notes\nProject notes.");
    assert_eq!(classify_file(&doc), Some(FileType::Document));
}

#[test]
fn test_classify_attention_paper() {
    let fixture = fixture();
    let paper = write(
        fixture.path(),
        "attention_is_all_you_need.md",
        "# Abstract\nArXiv 1706.03762. We propose attention. See [1]. Journal preprint.",
    );
    assert_eq!(classify_file(&paper), Some(FileType::Paper));
}

#[test]
fn test_classify_video_extensions() {
    for path in [
        "lecture.mp4",
        "podcast.mp3",
        "talk.mov",
        "recording.wav",
        "webinar.webm",
        "audio.m4a",
    ] {
        assert_eq!(classify_file(Path::new(path)), Some(FileType::Video));
    }
}

#[test]
fn test_classify_google_workspace_shortcuts() {
    for path in ["notes.gdoc", "budget.gsheet", "deck.gslides"] {
        assert_eq!(classify_file(Path::new(path)), Some(FileType::Document));
    }
}

macro_rules! sensitive_case {
    ($name:ident, true, [$($path:literal),+ $(,)?]) => {
        #[test]
        fn $name() { $(assert!(is_sensitive(Path::new($path)), "{}", $path);)+ }
    };
    ($name:ident, false, [$($path:literal),+ $(,)?]) => {
        #[test]
        fn $name() { $(assert!(!is_sensitive(Path::new($path)), "{}", $path);)+ }
    };
}

sensitive_case!(test_sensitive_flags_api_token_txt, true, ["api_token.txt"]);
sensitive_case!(
    test_sensitive_flags_oauth_token_json,
    true,
    ["oauth_token.json"]
);
sensitive_case!(
    test_sensitive_flags_underscore_secret,
    true,
    ["app_secret.yaml"]
);
sensitive_case!(
    test_sensitive_does_not_flag_tokenizer_py,
    false,
    ["tokenizer.py"]
);
sensitive_case!(
    test_sensitive_does_not_flag_tokenize_py,
    false,
    ["tokenize.py"]
);
sensitive_case!(
    test_sensitive_does_not_flag_passwords_py,
    false,
    ["passwords.py"]
);
sensitive_case!(
    test_sensitive_does_not_flag_ruby_code_modules,
    false,
    [
        "app/models/device_token.rb",
        "app/controllers/api/v1/passwords_controller.rb"
    ]
);
sensitive_case!(
    test_sensitive_still_flags_data_secret_stores,
    true,
    ["credentials.json", "oauth_token.json", "app_secret.yaml"]
);
#[test]
fn test_sensitive_flags_ssh_dir() {
    assert!(is_sensitive(Path::new("/home/user/.ssh/id_rsa")));
    assert!(is_sensitive(Path::new("backup/my-id_rsa")));
    assert!(!is_sensitive(Path::new("grid_rsa")));
}
sensitive_case!(
    test_sensitive_flags_secrets_dir,
    true,
    ["config/secrets/db.json"]
);
sensitive_case!(test_sensitive_flags_token_txt, true, ["token.txt"]);
sensitive_case!(
    test_sensitive_flags_credentials_json,
    true,
    ["credentials.json"]
);
sensitive_case!(
    test_sensitive_does_not_flag_root_file_named_credentials,
    true,
    ["credentials"]
);
sensitive_case!(
    test_sensitive_secret_handler_txt,
    true,
    ["secret_handler.txt"]
);
sensitive_case!(
    test_sensitive_token_config_yaml,
    true,
    ["token_config.yaml"]
);
sensitive_case!(
    test_sensitive_does_not_flag_source_under_secrets_dir,
    false,
    [
        "internal/secrets/vault.go",
        "app/services/credentials/manager.py"
    ]
);
sensitive_case!(
    test_sensitive_still_flags_data_under_secrets_dir,
    true,
    [
        "secrets/db.json",
        ".secrets/token.yaml",
        "deploy/credentials/prod.env",
        "internal/secrets/README.md"
    ]
);
sensitive_case!(
    test_sensitive_flags_everything_under_credential_store_dirs,
    true,
    [
        "/home/user/.ssh/config",
        ".aws/credentials",
        ".gnupg/helper.py",
        "backup/.gcloud/sync.sh"
    ]
);

#[test]
fn test_sensitive_dir_carveout_does_not_bypass_name_screens() {
    assert!(is_sensitive(Path::new("credentials/id_rsa")));
    assert!(is_sensitive(Path::new("secrets/deploy.pem")));
    assert!(!is_sensitive(Path::new("secrets/service_account.py")));
}

#[test]
fn test_sensitive_dir_carveout_still_drops_tfvars_values_store() {
    assert!(is_sensitive(Path::new("secrets/prod.tfvars")));
    assert!(!is_sensitive(Path::new("secrets/loader.py")));
    assert!(!is_sensitive(Path::new("secrets/main.tf")));
}

sensitive_case!(
    test_sensitive_does_not_flag_token_economics_note,
    false,
    ["token-economics-of-recall.md"]
);
sensitive_case!(
    test_sensitive_does_not_flag_password_policy_discussion,
    false,
    ["password-policy-discussion.md"]
);
sensitive_case!(
    test_sensitive_flags_keyword_at_end_of_long_name,
    true,
    ["github-personal-access-token.txt"]
);
sensitive_case!(
    test_sensitive_flags_my_private_key_txt,
    true,
    ["my_private_key.txt"]
);
sensitive_case!(test_sensitive_flags_dotfile_token, true, [".token"]);
sensitive_case!(test_sensitive_flags_plural_tokens_txt, true, ["tokens.txt"]);

#[test]
fn test_sensitive_filter_indexes_topic_prose_and_source() {
    for path in [
        "wiki/privacy-tokens.md",
        "wiki/ai-token-economics.md",
        "wiki/chain-of-hope-tokenomics.md",
        "tokenizer.py",
        "secretary.py",
        "google/oauth2/service_account.py",
        "docs/service-account-setup.md",
        "wiki/aws_credentials_rotation_guide.md",
        "token.economics.notes.md",
        "password-reset/design.md",
    ] {
        assert!(!is_sensitive(Path::new(path)), "{path}");
    }
}

#[test]
fn test_sensitive_filter_still_excludes_real_secrets() {
    for path in [
        ".env",
        "id_rsa",
        "credentials.json",
        "server.pem",
        "certs/server.key",
        "secrets.md",
        "passwords.md",
        "token.md",
        "token.txt",
        "api_token.json",
        "service-account.json",
        ".npmrc",
        ".pypirc",
        "secring.gpg",
        ".git-credentials",
        "Secrets/creds.json",
        "SECRETS/db.json",
        "ID_RSA",
        "secrets/prod.tfvars",
        "credentials/id_rsa",
    ] {
        assert!(is_sensitive(Path::new(path)), "{path}");
    }
}

#[test]
fn test_sensitive_bare_keyword_prose_still_dropped() {
    assert!(is_sensitive(Path::new("secrets.md")));
    assert!(is_sensitive(Path::new("token.rst")));
    assert!(!is_sensitive(Path::new("token-lifecycle.md")));
}

#[test]
fn test_sensitive_filter_indexes_env_templates() {
    for path in [
        ".env.example",
        ".env.sample",
        ".env.template",
        ".env.dist",
        ".ENV.EXAMPLE",
        ".envrc.sample",
        ".env.production.example",
    ] {
        assert!(!is_sensitive(Path::new(path)), "{path}");
    }
}

#[test]
fn test_sensitive_filter_still_excludes_real_env_files() {
    for path in [
        ".env",
        ".env.local",
        ".env.production",
        ".envrc",
        ".env.example.local",
        ".env.example.bak",
    ] {
        assert!(is_sensitive(Path::new(path)), "{path}");
    }
}

#[test]
fn test_sensitive_env_template_inside_secrets_dir_still_dropped() {
    for path in ["secrets/.env.example", "deploy/credentials/.env.example"] {
        assert!(is_sensitive(Path::new(path)), "{path}");
    }
}

macro_rules! shebang_case {
    ($name:ident, $line:expr, $expected:expr) => {
        #[test]
        fn $name() {
            let fixture = fixture();
            let script = write_bytes(
                fixture.path(),
                "script",
                format!("{}\nbody\n", $line).as_bytes(),
            );
            assert_eq!(shebang_interpreter(&script).as_deref(), $expected);
        }
    };
}

shebang_case!(
    test_shebang_interpreter_plain,
    "#!/usr/bin/python3",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_single_arg,
    "#!/usr/bin/env python3",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_dash_s,
    "#!/usr/bin/env -S python3 -u",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_with_flags,
    "#!/usr/bin/env -i bash",
    Some("bash")
);
shebang_case!(
    test_shebang_interpreter_env_with_assignment,
    "#!/usr/bin/env DEBUG=1 python3",
    Some("python3")
);
shebang_case!(test_shebang_interpreter_no_shebang, "print('x')", None);
shebang_case!(
    test_shebang_interpreter_quoted_path,
    "#!\"/usr/local/bin/python3\"",
    Some("python3")
);

#[test]
fn test_shebang_file_type_classifies_via_interpreter() {
    let fixture = fixture();
    let script = write_bytes(fixture.path(), "tool", b"#!/usr/bin/env -S python3 -u\n");
    assert_eq!(classify_file(&script), Some(FileType::Code));
}

#[test]
fn test_shebang_interpreter_unreadable_returns_none() {
    assert_eq!(
        shebang_interpreter(Path::new("/definitely/missing/script")),
        None
    );
}

shebang_case!(
    test_shebang_interpreter_env_unset_with_operand,
    "#!/usr/bin/env -u PYTHONPATH python3",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_chdir_with_operand,
    "#!/usr/bin/env -C /tmp python3",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_path_with_operand,
    "#!/usr/bin/env -P /bin python3",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_dash_s_after_flag,
    "#!/usr/bin/env -i -S \"python3 -u\"",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_clumped_u_operand,
    "#!/usr/bin/env -uPYTHONPATH python3",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_missing_operand_returns_none,
    "#!/usr/bin/env -u",
    None
);
shebang_case!(
    test_shebang_interpreter_env_gnu_split_string_equals,
    "#!/usr/bin/env --split-string='python3 -u'",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_gnu_split_string_separate,
    "#!/usr/bin/env --split-string \"python3 -u\"",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_gnu_argv0_operand,
    "#!/usr/bin/env -a alias python3",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_compact_dash_s,
    "#!/usr/bin/env -Spython3 -u",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_compact_v_then_s,
    "#!/usr/bin/env -vSpython3 -u",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_long_unset_separate_operand,
    "#!/usr/bin/env --unset PYTHONPATH python3",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_long_unset_equals,
    "#!/usr/bin/env --unset=PYTHONPATH python3",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_long_chdir_separate_operand,
    "#!/usr/bin/env --chdir /tmp python3",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_long_chdir_equals,
    "#!/usr/bin/env --chdir=/tmp python3",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_signal_flags,
    "#!/usr/bin/env --default-signal=TERM --ignore-signal=PIPE python3",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_unknown_option_returns_none,
    "#!/usr/bin/env --no-such-flag python3",
    None
);
shebang_case!(
    test_shebang_interpreter_env_dash_s_assignment_before_interpreter,
    "#!/usr/bin/env -S PYTHONPATH=/opt/custom:${PYTHONPATH} python3",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_dash_s_flag_before_interpreter,
    "#!/usr/bin/env -S -i OLDUSER=${USER} python3",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_long_split_assignment_before_interpreter,
    "#!/usr/bin/env --split-string='PYTHONPATH=/opt/custom:${PYTHONPATH} python3 -u'",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_long_split_flag_before_interpreter,
    "#!/usr/bin/env --split-string='-i python3 -u'",
    Some("python3")
);
shebang_case!(
    test_shebang_interpreter_env_nested_split_string_rejected,
    "#!/usr/bin/env -S -S python3 -u",
    None
);
shebang_case!(
    test_shebang_interpreter_env_vs_assignment_before_interpreter,
    "#!/usr/bin/env -vS DEBUG=1 python3 -u",
    Some("python3")
);

#[test]
fn test_graphifyignore_excludes_file() {
    let fixture = fixture();
    write(
        fixture.path(),
        ".graphifyignore",
        "vendor/\n*.generated.py\n",
    );
    write(fixture.path(), "vendor/lib.py", "x=1");
    write(fixture.path(), "main.py", "x=1");
    write(fixture.path(), "schema.generated.py", "x=1");
    let result = scan(fixture.path());
    assert!(has(&result, "main.py"));
    assert!(!has(&result, "vendor"));
    assert!(!has(&result, "generated"));
    assert_eq!(result.graphifyignore_patterns, 2);
}

#[test]
fn test_graphifyignore_missing_is_fine() {
    let fixture = fixture();
    write(fixture.path(), "main.py", "x=1");
    assert_eq!(scan(fixture.path()).graphifyignore_patterns, 0);
}

#[test]
fn test_graphifyignore_comments_ignored() {
    let fixture = fixture();
    write(
        fixture.path(),
        ".graphifyignore",
        "# this is a comment\n\nmain.py\n",
    );
    write(fixture.path(), "main.py", "x=1");
    write(fixture.path(), "other.py", "x=2");
    let result = scan(fixture.path());
    assert!(!has(&result, "main.py"));
    assert!(has(&result, "other.py"));
}

#[test]
fn test_graphifyignore_utf8_bom_first_pattern_honored() {
    let fixture = fixture();
    write_bytes(
        fixture.path(),
        ".graphifyignore",
        b"\xef\xbb\xbf*.log\nbuild/\n",
    );
    write(fixture.path(), "build/lib.py", "x=1");
    write(fixture.path(), "app.log", "log");
    write(fixture.path(), "main.py", "x=1");
    let result = scan(fixture.path());
    assert!(!has(&result, "app.log"));
    assert!(!has(&result, "build"));
    assert!(has(&result, "main.py"));
    assert_eq!(result.graphifyignore_patterns, 2);
}

#[test]
fn test_gitignore_utf8_bom_matches_git() {
    let fixture = fixture();
    write_bytes(fixture.path(), ".gitignore", b"\xef\xbb\xbf*.log\n");
    write(fixture.path(), "app.log", "log");
    write(fixture.path(), "main.py", "x=1");
    let result = scan(fixture.path());
    assert!(!has(&result, "app.log"));
    assert!(has(&result, "main.py"));
}

#[test]
fn test_graphifyignore_bom_only_file() {
    let fixture = fixture();
    write_bytes(fixture.path(), ".graphifyignore", b"\xef\xbb\xbf");
    write(fixture.path(), "main.py", "x=1");
    let result = scan(fixture.path());
    assert_eq!(result.graphifyignore_patterns, 0);
    assert!(has(&result, "main.py"));
}

#[test]
fn test_graphifyignore_bom_then_comment() {
    let fixture = fixture();
    write_bytes(
        fixture.path(),
        ".graphifyignore",
        b"\xef\xbb\xbf# comment\nmain.py\n",
    );
    write(fixture.path(), "main.py", "x=1");
    write(fixture.path(), "other.py", "x=2");
    let result = scan(fixture.path());
    assert!(!has(&result, "main.py"));
    assert!(has(&result, "other.py"));
    assert_eq!(result.graphifyignore_patterns, 1);
}

#[test]
fn test_nested_gitignore_utf8_bom() {
    let fixture = fixture();
    write_bytes(fixture.path(), "sub/.gitignore", b"\xef\xbb\xbf*.log\n");
    write(fixture.path(), "sub/app.log", "log");
    write(fixture.path(), "sub/keep.py", "x=1");
    let result = scan(fixture.path());
    assert!(!has(&result, "app.log"));
    assert!(has(&result, "keep.py"));
}

#[test]
fn test_git_info_exclude_utf8_bom() {
    let fixture = fixture();
    write_bytes(
        fixture.path(),
        ".git/info/exclude",
        b"\xef\xbb\xbfsecrets/\n",
    );
    write(fixture.path(), "secrets/x.py", "x=1");
    write(fixture.path(), "real.py", "x=1");
    let result = scan(fixture.path());
    assert!(!has(&result, "secrets"));
    assert!(has(&result, "real.py"));
}

#[cfg(unix)]
fn make_symlink(source: &Path, target: &Path) {
    std::os::unix::fs::symlink(source, target).unwrap();
}

#[test]
#[cfg(unix)]
fn test_detect_follows_symlinked_directory() {
    let fixture = fixture();
    write(fixture.path(), "real_lib/util.py", "x=1");
    make_symlink(
        &fixture.path().join("real_lib"),
        &fixture.path().join("linked_lib"),
    );
    let no = scan(fixture.path());
    let yes = scan_with(fixture.path(), |options| options.follow_symlinks = true);
    assert!(has(&no, "real_lib"));
    assert!(!has(&no, "linked_lib"));
    assert!(has(&yes, "real_lib"));
    assert!(!has(&yes, "linked_lib"));
    assert_eq!(bucket(&yes, "code").len(), 1);
}

#[test]
#[cfg(unix)]
fn test_detect_managed_memory_never_crosses_symlink_boundaries() {
    let fixture = fixture();
    let outside = fixture.path().join("outside");
    write(
        &outside,
        "must_not_be_opened.py",
        "raise RuntimeError('outside')\n",
    );

    let linked_memory_project = fixture.path().join("linked-memory-project");
    write(&linked_memory_project, "app.py", "x=1\n");
    fs::create_dir_all(linked_memory_project.join("graphoxide-out")).unwrap();
    make_symlink(
        &outside,
        &linked_memory_project.join("graphoxide-out/memory"),
    );
    let linked = scan_with(&linked_memory_project, |options| {
        options.follow_symlinks = true;
    });
    assert!(has(&linked, "app.py"));
    assert!(!has(&linked, "must_not_be_opened.py"));
    assert!(linked
        .files
        .values()
        .flatten()
        .all(|path| !path.starts_with(outside.to_string_lossy().as_ref())));

    let nested_link_project = fixture.path().join("nested-link-project");
    write(&nested_link_project, "app.py", "x=1\n");
    write(
        &nested_link_project,
        "graphoxide-out/memory/note.md",
        "# Safe managed memory\n",
    );
    make_symlink(
        &outside,
        &nested_link_project.join("graphoxide-out/memory/escape"),
    );
    let nested = scan_with(&nested_link_project, |options| {
        options.follow_symlinks = true;
    });
    assert!(has(&nested, "graphoxide-out/memory/note.md"));
    assert!(!has(&nested, "must_not_be_opened.py"));
    assert!(nested
        .skipped_sensitive
        .iter()
        .any(|item| item.contains("managed memory symlink or reparse point")));
}

#[test]
#[cfg(unix)]
fn test_detect_follows_symlinked_file() {
    let fixture = fixture();
    let real = write(fixture.path(), "real.py", "x=1");
    make_symlink(&real, &fixture.path().join("link.py"));
    let result = scan_with(fixture.path(), |options| options.follow_symlinks = true);
    assert!(has(&result, "real.py"));
    assert!(!has(&result, "link.py"));
    assert_eq!(bucket(&result, "code").len(), 1);
}

#[test]
#[cfg(unix)]
fn test_detect_rejects_in_root_file_symlink_by_default() {
    let fixture = fixture();
    let real = write(fixture.path(), "real.py", "x=1");
    make_symlink(&real, &fixture.path().join("link.py"));
    let result = scan(fixture.path());
    assert!(has(&result, "real.py"));
    assert!(!has(&result, "link.py"));
    assert!(result
        .skipped_sensitive
        .iter()
        .any(|item| item.contains("file symlink skipped")));
}

#[test]
#[cfg(unix)]
fn test_detect_applies_sensitive_policy_to_followed_file_target() {
    let fixture = fixture();
    let sensitive = write(fixture.path(), ".env", "TOKEN=do-not-index");
    make_symlink(&sensitive, &fixture.path().join("benign.py"));
    let result = scan_with(fixture.path(), |options| options.follow_symlinks = true);
    assert!(!has(&result, "benign.py"));
    assert!(!has(&result, ".env"));
    assert!(result
        .skipped_sensitive
        .iter()
        .any(|item| item.contains("benign.py")));
}

#[test]
fn test_graphifyignore_hermetic_without_vcs() {
    let fixture = fixture();
    write(fixture.path(), ".graphifyignore", "vendor/\n");
    let sub = fixture.path().join("packages/mylib");
    write(&sub, "main.py", "x=1");
    write(&sub, "vendor/dep.py", "x=2");
    let result = scan(&sub);
    assert!(has(&result, "main.py"));
    assert!(has(&result, "vendor"));
    assert_eq!(result.graphifyignore_patterns, 0);
}

#[test]
fn test_graphifyignore_discovered_from_parent_in_vcs() {
    let fixture = fixture();
    init_git_marker(fixture.path());
    write(fixture.path(), ".graphifyignore", "vendor/\n");
    let sub = fixture.path().join("packages/mylib");
    write(&sub, "main.py", "x=1");
    write(&sub, "vendor/dep.py", "x=2");
    let result = scan(&sub);
    assert!(has(&result, "main.py"));
    assert!(!has(&result, "vendor"));
    assert!(result.graphifyignore_patterns >= 1);
}

#[test]
fn test_graphifyignore_stops_at_git_boundary() {
    let fixture = fixture();
    write(fixture.path(), ".graphifyignore", "main.py\n");
    init_git_marker(&fixture.path().join("repo"));
    let sub = fixture.path().join("repo/sub");
    write(&sub, "main.py", "x=1");
    let result = scan(&sub);
    assert!(has(&result, "main.py"));
    assert_eq!(result.graphifyignore_patterns, 0);
}

#[test]
fn test_graphifyignore_at_git_root_is_included() {
    let fixture = fixture();
    init_git_marker(&fixture.path().join("repo"));
    write(fixture.path(), "repo/.graphifyignore", "vendor/\n");
    let sub = fixture.path().join("repo/packages/mylib");
    write(&sub, "main.py", "x=1");
    write(&sub, "vendor/dep.py", "x=2");
    let result = scan(&sub);
    assert!(has(&result, "main.py"));
    assert!(!has(&result, "vendor"));
    assert_eq!(result.graphifyignore_patterns, 1);
}

#[test]
fn test_gitignore_nested_below_root_excludes_file() {
    let fixture = fixture();
    write(fixture.path(), ".gitignore", "*.log\n");
    write(fixture.path(), "vendor/sub/.gitignore", "secret.txt\n");
    write(fixture.path(), "root.py", "x=1");
    write(fixture.path(), "root.log", "noise");
    write(fixture.path(), "vendor/sub/keep.py", "x=2");
    write(fixture.path(), "vendor/sub/secret.txt", "shh");
    let result = scan(fixture.path());
    assert!(has(&result, "root.py"));
    assert!(has(&result, "keep.py"));
    assert!(!has(&result, "root.log"));
    assert!(!has(&result, "secret.txt"));
    assert_eq!(result.graphifyignore_patterns, 2);
}

#[test]
fn test_gitignore_nested_below_root_prunes_whole_directory() {
    let fixture = fixture();
    write(fixture.path(), "vendor/sub/.gitignore", "build/\n");
    write(fixture.path(), "vendor/sub/build/generated.py", "x=1");
    write(fixture.path(), "vendor/sub/keep.py", "x=2");
    let result = scan(fixture.path());
    assert!(has(&result, "keep.py"));
    assert!(!has(&result, "generated.py"));
}

#[test]
fn test_gitignore_nested_negation_overrides_broader_root_rule() {
    let fixture = fixture();
    write(fixture.path(), ".gitignore", "*.py\n");
    write(fixture.path(), "vendor/sub/.gitignore", "!important.py\n");
    write(fixture.path(), "root.py", "x=1");
    write(fixture.path(), "vendor/sub/important.py", "x=2");
    write(fixture.path(), "vendor/sub/other.py", "x=3");
    let result = scan(fixture.path());
    assert!(has(&result, "vendor/sub/important.py"));
    assert!(!has(&result, "root.py"));
    assert!(!has(&result, "other.py"));
}

#[test]
fn test_nested_ignore_overrides_git_info_exclude_and_root() {
    let fixture = fixture();
    write(fixture.path(), ".git/info/exclude", "*.py\n");
    write(fixture.path(), ".gitignore", "keep.py\n");
    write(fixture.path(), "a/b/.gitignore", "!keep.py\n");
    write(fixture.path(), "a/b/keep.py", "x=1");
    write(fixture.path(), "drop.py", "x=2");
    let result = scan(fixture.path());
    assert!(has(&result, "a/b/keep.py"));
    assert!(!has(&result, "drop.py"));
}

#[test]
#[cfg(unix)]
fn test_detect_handles_circular_symlinks() {
    let fixture = fixture();
    write(fixture.path(), "a/main.py", "x=1");
    make_symlink(fixture.path(), &fixture.path().join("a/loop"));
    let result = scan_with(fixture.path(), |options| options.follow_symlinks = true);
    assert!(has(&result, "main.py"));
    assert_eq!(bucket(&result, "code").len(), 1);
}

#[test]
#[cfg(unix)]
fn test_detect_default_does_not_auto_follow_direct_symlink_child() {
    let fixture = fixture();
    write(fixture.path(), "real_lib/util.py", "x=1");
    make_symlink(
        &fixture.path().join("real_lib"),
        &fixture.path().join("linked_lib"),
    );
    let result = scan(fixture.path());
    assert!(has(&result, "real_lib"));
    assert!(!has(&result, "linked_lib"));
}

#[test]
fn test_detect_default_does_not_follow_when_no_symlinks() {
    let fixture = fixture();
    write(fixture.path(), "main.py", "x=1");
    write(fixture.path(), "sub/other.py", "x=2");
    let result = scan(fixture.path());
    assert!(has(&result, "main.py"));
    assert!(has(&result, "other.py"));
}

#[test]
#[cfg(unix)]
fn test_detect_explicit_false_overrides_auto_detect() {
    let fixture = fixture();
    write(fixture.path(), "real_lib/util.py", "x=1");
    make_symlink(
        &fixture.path().join("real_lib"),
        &fixture.path().join("linked_lib"),
    );
    assert!(!has(&scan(fixture.path()), "linked_lib"));
}

#[test]
#[cfg(unix)]
fn test_detect_skips_out_of_root_symlinked_directory_even_when_following() {
    let fixture = fixture();
    let root = fixture.path().join("root");
    let outside = fixture.path().join("outside");
    write(&outside, "secret.py", "x=1");
    fs::create_dir_all(&root).unwrap();
    make_symlink(&outside, &root.join("linked_secret"));
    let result = scan_with(&root, |options| options.follow_symlinks = true);
    assert!(!has(&result, "linked_secret"));
    assert!(result
        .skipped_sensitive
        .iter()
        .any(|item| item.contains("symlink target outside scan root")));
}

#[test]
#[cfg(unix)]
fn test_detect_skips_out_of_root_symlinked_file_by_default() {
    let fixture = fixture();
    let root = fixture.path().join("root");
    let secret = write(fixture.path(), "outside/secret.py", "x=1");
    fs::create_dir_all(&root).unwrap();
    make_symlink(&secret, &root.join("secret_link.py"));
    let result = scan(&root);
    assert!(!has(&result, "secret_link.py"));
    assert!(result
        .skipped_sensitive
        .iter()
        .any(|item| item.contains("file symlink skipped")));
}

#[test]
fn test_anchored_root_wildcard_negation_reincludes_subtree() {
    let fixture = fixture();
    for relative in [
        "src/app/main.py",
        "src/lib/util.py",
        "docs/guide.md",
        "README.md",
    ] {
        write(fixture.path(), relative, "x\n");
    }
    write(fixture.path(), ".graphifyignore", "/*\n!/src/\n");
    let result = scan(fixture.path());
    let canonical_root = fs::canonicalize(fixture.path()).unwrap();
    let found: BTreeSet<_> = all_files(&result)
        .into_iter()
        .map(|path| {
            Path::new(path)
                .strip_prefix(&canonical_root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    assert_eq!(
        found,
        BTreeSet::from(["src/app/main.py".into(), "src/lib/util.py".into()])
    );
}

#[test]
fn test_anchored_negation_cannot_skip_excluded_parent() {
    let fixture = fixture();
    write(fixture.path(), "src/app/main.py", "x\n");
    write(fixture.path(), ".graphifyignore", "/*\n!/src/app/\n");
    assert_eq!(scan(fixture.path()).total_files, 0);
}

#[test]
fn test_path_pattern_single_star_does_not_cross_segment() {
    for pattern in ["/src/*.py", "src/*.py"] {
        let fixture = fixture();
        write(fixture.path(), "src/main.py", "x\n");
        write(fixture.path(), "src/app/main.py", "x\n");
        write(fixture.path(), ".graphifyignore", &format!("{pattern}\n"));
        let result = scan(fixture.path());
        assert!(!has(&result, "/src/main.py"));
        assert!(has(&result, "src/app/main.py"));
    }
}

#[test]
fn test_directory_only_negation_does_not_reinclude_file() {
    let fixture = fixture();
    write(fixture.path(), "README.md", "# docs\n");
    write(fixture.path(), ".graphifyignore", "/*\n!/README.md/\n");
    assert_eq!(scan(fixture.path()).total_files, 0);
}

#[test]
fn test_anchored_double_star_crosses_path_segments() {
    let fixture = fixture();
    write(fixture.path(), "src/generated.py", "x\n");
    write(fixture.path(), "src/app/deep/generated.py", "x\n");
    write(fixture.path(), ".graphifyignore", "/src/**/generated.py\n");
    assert_eq!(scan(fixture.path()).total_files, 0);
}

#[test]
fn test_negation_cannot_rescue_file_under_excluded_dir() {
    let fixture = fixture();
    let victim = write(fixture.path(), "android/app/src/Main.kt", "fun main() {}\n");
    write(fixture.path(), ".graphifyignore", "android/\n!src/\n");
    let patterns = load_ignore_patterns(fixture.path(), true);
    assert!(is_ignored(&victim, fixture.path(), &patterns));
}

#[test]
fn oversized_root_ignore_source_fails_closed_with_a_diagnostic() {
    let fixture = fixture();
    let _sentinel = write(fixture.path(), "sentinel.rs", "fn must_not_be_read() {}\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&_sentinel, fs::Permissions::from_mode(0o000))
            .expect("make no-read sentinel");
    }
    let mut oversized = b"sentinel.rs\n".to_vec();
    oversized.resize(MAX_IGNORE_SOURCE_BYTES + 1, b'#');
    write_bytes(fixture.path(), ".graphoxideignore", &oversized);

    let error = detect(fixture.path(), &DetectOptions::default())
        .expect_err("oversized root ignore must abort discovery");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&_sentinel, fs::Permissions::from_mode(0o600))
            .expect("restore no-read sentinel");
    }

    let error = error.to_string();
    assert!(error.contains(".graphoxideignore"));
    assert!(error.contains("byte limit"));
    assert!(error.contains("no rules from this source were applied"));

    let legacy_patterns = load_ignore_patterns(fixture.path(), true);
    assert_eq!(legacy_patterns.len(), 1);
    assert_eq!(legacy_patterns[0].pattern, "**");
    assert!(is_ignored(
        &fixture.path().join("sentinel.rs"),
        fixture.path(),
        &legacy_patterns
    ));
}

#[test]
fn oversized_nested_ignore_source_skips_that_subtree_without_reading_it() {
    let fixture = fixture();
    write(fixture.path(), "root.rs", "fn root() {}\n");
    write(
        fixture.path(),
        "nested/sentinel.rs",
        "fn must_not_be_read() {}\n",
    );
    let mut oversized = String::from("sentinel.rs\n");
    for index in 0..MAX_IGNORE_PATTERNS_PER_SOURCE {
        oversized.push_str(&format!("generated-{index}\n"));
    }
    assert!(oversized.len() <= MAX_IGNORE_SOURCE_BYTES);
    write(fixture.path(), "nested/.gitignore", &oversized);

    let error = detect(fixture.path(), &DetectOptions::default())
        .expect_err("oversized nested ignore must abort discovery")
        .to_string();

    assert!(error.contains("nested/.gitignore"));
    assert!(error.contains("pattern source limit"));
}

#[test]
fn overlong_ignore_rule_fails_closed_before_project_traversal() {
    let fixture = fixture();
    write(fixture.path(), "sentinel.rs", "fn must_not_be_read() {}\n");
    write(
        fixture.path(),
        ".graphoxideignore",
        &format!("{}\n", "x".repeat(MAX_IGNORE_PATTERN_BYTES + 1)),
    );

    let error = detect(fixture.path(), &DetectOptions::default())
        .expect_err("overlong ignore rule must abort discovery")
        .to_string();

    assert!(error.contains("rule exceeding"));
    assert!(error.contains("no rules from this source were applied"));
}

#[test]
fn test_negation_works_when_no_ancestor_excluded() {
    let fixture = fixture();
    let keep = write(fixture.path(), "src/keep.py", "x=1\n");
    write(fixture.path(), ".graphifyignore", "*.py\n!src/keep.py\n");
    let patterns = load_ignore_patterns(fixture.path(), true);
    assert!(!is_ignored(&keep, fixture.path(), &patterns));
}

#[test]
fn test_negation_ancestor_itself_reincluded() {
    let fixture = fixture();
    let file = write(fixture.path(), "vendor/lib/utils.py", "x=1\n");
    write(fixture.path(), ".graphifyignore", "vendor/\n!vendor/\n");
    let patterns = load_ignore_patterns(fixture.path(), true);
    assert!(!is_ignored(&file, fixture.path(), &patterns));
}

#[test]
fn test_negation_does_not_disable_directory_pruning() {
    let fixture = fixture();
    write(
        fixture.path(),
        ".graphifyignore",
        "myignored/\n*.md\n!docs/**\n",
    );
    write(fixture.path(), "myignored/deep/deeper/junk.py", "x=1\n");
    write(fixture.path(), "docs/guide.md", "# guide\n");
    write(fixture.path(), "src/app.py", "x=2\n");
    let result = scan(fixture.path());
    assert!(has(&result, "app.py"));
    assert!(has(&result, "guide.md"));
    assert!(!has(&result, "junk.py"));
    assert!(result.ignored.iter().any(|path| path.contains("myignored")));
}

#[test]
fn test_anchored_dir_not_matched_at_depth() {
    let fixture = fixture();
    let file = write(fixture.path(), "src/inbox/main.rs", "fn main() {}\n");
    write(fixture.path(), ".graphifyignore", "/inbox/\n");
    let patterns = load_ignore_patterns(fixture.path(), true);
    assert!(!is_ignored(&file, fixture.path(), &patterns));
    assert!(!is_ignored(
        &fixture.path().join("src/inbox"),
        fixture.path(),
        &patterns
    ));
}

#[test]
fn test_anchored_dir_matches_at_root() {
    let fixture = fixture();
    let file = write(fixture.path(), "inbox/data.json", "{}\n");
    write(fixture.path(), ".graphifyignore", "/inbox/\n");
    let patterns = load_ignore_patterns(fixture.path(), true);
    assert!(is_ignored(&file, fixture.path(), &patterns));
    assert!(is_ignored(
        &fixture.path().join("inbox"),
        fixture.path(),
        &patterns
    ));
}

#[test]
fn test_anchored_file_not_matched_at_depth() {
    let fixture = fixture();
    fs::create_dir_all(fixture.path().join("src/build")).unwrap();
    write(fixture.path(), ".graphifyignore", "/build\n");
    let patterns = load_ignore_patterns(fixture.path(), true);
    assert!(!is_ignored(
        &fixture.path().join("src/build"),
        fixture.path(),
        &patterns
    ));
}

#[test]
fn test_unanchored_dir_still_matches_at_depth() {
    let fixture = fixture();
    let file = write(fixture.path(), "src/inbox/main.rs", "fn main() {}\n");
    write(fixture.path(), ".graphifyignore", "inbox/\n");
    let patterns = load_ignore_patterns(fixture.path(), true);
    assert!(is_ignored(&file, fixture.path(), &patterns));
}

#[test]
fn test_anchored_multi_segment_pattern() {
    let fixture = fixture();
    let expected = write(fixture.path(), "src/inbox/a.py", "x=1\n");
    let other = write(fixture.path(), "x/src/inbox/b.py", "x=1\n");
    write(fixture.path(), ".graphifyignore", "/src/inbox/\n");
    let patterns = load_ignore_patterns(fixture.path(), true);
    assert!(is_ignored(&expected, fixture.path(), &patterns));
    assert!(!is_ignored(&other, fixture.path(), &patterns));
}

#[test]
fn test_is_ignored_cache_matches_uncached_results() {
    let fixture = fixture();
    write(
        fixture.path(),
        ".graphifyignore",
        "build/\n*.log\n!logs/keep.log\n",
    );
    let paths = [
        "build",
        "build/out.o",
        "build/sub",
        "build/sub/deep.o",
        "logs",
        "logs/drop.log",
        "logs/keep.log",
        "src/main.py",
    ];
    for path in paths
        .iter()
        .filter(|path| Path::new(path).extension().is_some())
    {
        write(fixture.path(), path, "x\n");
    }
    let patterns = load_ignore_patterns(fixture.path(), true);
    let first: Vec<_> = paths
        .iter()
        .map(|path| is_ignored(&fixture.path().join(path), fixture.path(), &patterns))
        .collect();
    let mut cache = std::collections::HashMap::new();
    let second: Vec<_> = paths
        .iter()
        .map(|path| {
            is_ignored_with_cache(
                &fixture.path().join(path),
                fixture.path(),
                &patterns,
                &mut cache,
            )
        })
        .collect();
    assert_eq!(first, second);
    assert!(first[5]);
    assert!(!first[6]);
}

#[test]
fn test_is_ignored_cache_evaluates_each_dir_once() {
    let root = Path::new("/repo");
    let patterns = vec![IgnorePattern {
        anchor: root.to_path_buf(),
        pattern: "*.tmp".into(),
    }];
    let files = [
        "/repo/a/b/f1.py",
        "/repo/a/b/f2.py",
        "/repo/a/b/f3.py",
        "/repo/a/c/f4.py",
        "/repo/a/c/f5.py",
    ];
    let mut cache = std::collections::HashMap::new();
    for file in files {
        assert!(!is_ignored_with_cache(
            Path::new(file),
            root,
            &patterns,
            &mut cache
        ));
    }
    for shared in ["/repo/a", "/repo/a/b", "/repo/a/c"] {
        assert_eq!(cache.get(Path::new(shared)), Some(&false));
    }
    // Five unique files plus the three unique shared directories.
    assert_eq!(cache.len(), 8);
}

#[test]
fn test_gitignore_fallback_when_no_graphifyignore() {
    let fixture = fixture();
    init_git_marker(fixture.path());
    write(fixture.path(), ".gitignore", "vendor/\n*.generated.py\n");
    write(fixture.path(), "vendor/lib.py", "x=1\n");
    write(fixture.path(), "main.py", "x=1\n");
    write(fixture.path(), "schema.generated.py", "x=1\n");
    let result = scan(fixture.path());
    assert!(has(&result, "main.py"));
    assert!(!has(&result, "vendor"));
    assert!(!has(&result, "generated"));
}

#[test]
fn test_graphifyignore_and_gitignore_are_merged() {
    let fixture = fixture();
    init_git_marker(fixture.path());
    write(fixture.path(), ".gitignore", "main.py\n");
    write(fixture.path(), ".graphifyignore", "other.py\n");
    write(fixture.path(), "main.py", "x=1\n");
    write(fixture.path(), "other.py", "x=2\n");
    write(fixture.path(), "keep.py", "x=3\n");
    let result = scan(fixture.path());
    assert!(!has(&result, "main.py"));
    assert!(!has(&result, "other.py"));
    assert!(has(&result, "keep.py"));
}

#[test]
fn test_graphifyignore_negation_overrides_gitignore() {
    let fixture = fixture();
    init_git_marker(fixture.path());
    write(fixture.path(), ".gitignore", "*.py\n");
    write(fixture.path(), ".graphifyignore", "!keep.py\n");
    write(fixture.path(), "main.py", "x=1\n");
    write(fixture.path(), "keep.py", "x=2\n");
    let result = scan(fixture.path());
    assert!(has(&result, "keep.py"));
    assert!(!has(&result, "main.py"));
}

#[test]
fn test_git_info_exclude_ranks_below_gitignore_negation() {
    let fixture = fixture();
    init_git_marker(fixture.path());
    write(fixture.path(), ".git/info/exclude", "secret*.txt\n");
    write(fixture.path(), ".gitignore", "!secret-ok.txt\n");
    let bad = write(fixture.path(), "secret-bad.txt", "x\n");
    let good = write(fixture.path(), "secret-ok.txt", "x\n");
    let patterns = load_ignore_patterns(fixture.path(), true);
    assert!(is_ignored(&bad, fixture.path(), &patterns));
    assert!(!is_ignored(&good, fixture.path(), &patterns));
}

#[test]
fn test_detect_skips_google_workspace_shortcuts_by_default() {
    let fixture = fixture();
    write(fixture.path(), "notes.gdoc", r#"{"doc_id":"doc-1"}"#);
    let result = scan(fixture.path());
    assert!(bucket(&result, "document").is_empty());
    assert!(result
        .skipped_sensitive
        .iter()
        .any(|item| item.contains("Google Workspace shortcut skipped")));
}

#[test]
fn test_detect_converts_google_workspace_shortcuts_when_enabled() {
    let fixture = fixture();
    write(fixture.path(), "notes.gdoc", r#"{"doc_id":"doc-1"}"#);
    let result = scan_with(fixture.path(), |options| options.google_workspace = true);
    assert_eq!(bucket(&result, "document").len(), 1);
    assert!(bucket(&result, "document")[0].ends_with(".md"));
    assert!(result.total_words > 0);
}

#[test]
fn test_detect_includes_video_key() {
    let fixture = fixture();
    write(fixture.path(), "main.py", "x=1\n");
    assert!(scan(fixture.path()).files.contains_key("video"));
}

#[test]
fn test_detect_finds_video_files() {
    let fixture = fixture();
    write_bytes(fixture.path(), "lecture.mp4", b"fake video data");
    write(fixture.path(), "notes.md", "# Notes\nSome content here.\n");
    let result = scan(fixture.path());
    assert_eq!(bucket(&result, "video").len(), 1);
    assert!(bucket(&result, "video")[0].contains("lecture.mp4"));
}

#[test]
fn test_detect_video_not_in_words() {
    let fixture = fixture();
    write_bytes(fixture.path(), "clip.mp4", &[0; 100]);
    assert_eq!(scan(fixture.path()).total_words, 0);
}

#[test]
fn test_detect_streams_registered_source_word_count_under_static_cap() {
    let fixture = fixture();
    let path = fixture.path().join("large.jsonl");
    let file = fs::File::create(&path).unwrap();
    file.set_len((WORD_COUNT_MAX_BYTES as u64).saturating_add(1))
        .unwrap();

    let result = scan(fixture.path());
    assert!(has(&result, "large.jsonl"));
    assert_eq!(result.total_words, 1);
    assert_eq!(result.word_count_truncations.len(), 1);
    assert!(result.word_count_truncations[0].contains(&format!(
        "word count truncated at {WORD_COUNT_MAX_BYTES} bytes"
    )));
}

#[test]
fn test_detect_skips_coverage_dir() {
    let fixture = fixture();
    write(fixture.path(), "coverage/lcov-report/index.html", "<html/>");
    write(
        fixture.path(),
        "coverage/lcov-report/src.ts.html",
        "<html/>",
    );
    write(fixture.path(), "main.py", "def hello(): pass\n");
    let result = scan(fixture.path());
    assert!(!has(&result, "coverage"));
    assert!(has(&result, "main.py"));
}

#[test]
fn test_detect_skips_coverage_dir_by_lcov_info() {
    let fixture = fixture();
    write(fixture.path(), "coverage/lcov.info", "TN:\nSF:src/app.ts\n");
    write(fixture.path(), "coverage/prettify.js", "var x=1;");
    write(fixture.path(), "main.py", "def hello(): pass\n");
    let result = scan(fixture.path());
    assert!(!has(&result, "coverage"));
    assert!(has(&result, "main.py"));
}

#[test]
fn test_detect_keeps_coverage_code_namespace() {
    let fixture = fixture();
    write(
        fixture.path(),
        "auditor_toolkit/assurance/coverage/__init__.py",
        "from .impact import Impact\n",
    );
    write(
        fixture.path(),
        "auditor_toolkit/assurance/coverage/impact.py",
        "class Impact: pass\n",
    );
    write(
        fixture.path(),
        "auditor_toolkit/assurance/coverage/inventory.py",
        "def inventory(): return []\n",
    );
    let result = scan(fixture.path());
    assert!(has(&result, "impact.py"));
    assert!(has(&result, "inventory.py"));
    assert!(has(&result, "coverage/__init__.py"));
}

#[test]
fn test_collect_files_keeps_coverage_code_namespace() {
    let fixture = fixture();
    let package = fixture.path().join("auditor_toolkit/assurance/coverage");
    for name in ["__init__.py", "impact.py", "mapping.py"] {
        write(&package, name, "def f(): pass\n");
    }
    write(fixture.path(), "webapp/coverage/index.html", "<html/>");
    write(fixture.path(), "webapp/coverage/prettify.js", "var PR=1;");
    let direct: Vec<_> = collect_files(&package)
        .unwrap()
        .into_iter()
        .filter_map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .collect();
    assert_eq!(direct, ["__init__.py", "impact.py", "mapping.py"]);
    let walked = collect_files(fixture.path()).unwrap();
    assert!(walked.iter().any(|path| path.ends_with("impact.py")));
    assert!(!walked
        .iter()
        .any(|path| path.to_string_lossy().contains("webapp")));
}

#[test]
fn test_is_noise_dir_coverage_is_evidence_gated() {
    let fixture = fixture();
    fs::create_dir(fixture.path().join("coverage")).unwrap();
    assert!(!is_noise_dir("coverage", None));
    assert!(!is_noise_dir("coverage", Some(fixture.path())));
    write(fixture.path(), "coverage/lcov.info", "TN:\n");
    assert!(is_noise_dir("coverage", Some(fixture.path())));
}

macro_rules! noise_scan_case {
    ($name:ident, $noise:literal, $noise_file:literal, $real:literal) => {
        #[test]
        fn $name() {
            let fixture = fixture();
            write(fixture.path(), concat!($noise, "/", $noise_file), "noise\n");
            write(fixture.path(), $real, "real\n");
            let result = scan(fixture.path());
            assert!(!has(&result, $noise));
            assert!(has(&result, $real));
        }
    };
}

noise_scan_case!(
    test_detect_skips_visual_tests_dir,
    "visual-tests",
    "screens.tsx",
    "app.py"
);
noise_scan_case!(
    test_detect_skips_storybook_static_dir,
    "storybook-static",
    "main.js",
    "Button.tsx"
);
noise_scan_case!(
    test_detect_skips_next_cache,
    ".next",
    "cache/build.js",
    "pages/index.tsx"
);
noise_scan_case!(
    test_detect_skips_nox_virtualenv,
    ".nox",
    "tests/lib/site-packages/widget.py",
    "app.py"
);
noise_scan_case!(
    test_detect_skips_graphify_own_cache,
    ".graphify",
    "cache/abc123.json",
    "app.py"
);
noise_scan_case!(
    test_detect_skips_worktrees_dir,
    ".worktrees",
    "feature/main.py",
    "app.py"
);

#[test]
fn test_detect_skips_nested_worktrees_dir() {
    let fixture = fixture();
    write(fixture.path(), ".claude/worktrees/feature/main.py", "x=1\n");
    write(fixture.path(), "app.py", "x=2\n");
    let result = scan(fixture.path());
    assert!(has(&result, "app.py"));
    assert!(!has(&result, "worktrees"));
}

#[test]
fn test_detect_skips_snapshots_dir() {
    let fixture = fixture();
    write(fixture.path(), "__snapshots__/app.test.ts.snap", "snapshot");
    write(fixture.path(), "snapshots/component.ts.snap", "snapshot");
    write(fixture.path(), "app.ts", "export const x=1;\n");
    let result = scan(fixture.path());
    assert!(!has(&result, "__snapshots__"));
    assert!(!has(&result, "/snapshots/"));
    assert!(has(&result, "app.ts"));
}

#[test]
fn test_detect_keeps_snapshots_code_namespace() {
    let fixture = fixture();
    write(
        fixture.path(),
        "app/services/snapshots/round_reader.rb",
        "class RoundReader; end\n",
    );
    write(
        fixture.path(),
        "app/services/snapshots/backfill_marker.rb",
        "class BackfillMarker; end\n",
    );
    let result = scan(fixture.path());
    assert!(has(&result, "round_reader.rb"));
    assert!(has(&result, "backfill_marker.rb"));
}

#[test]
fn test_detect_allows_github_dir() {
    let fixture = fixture();
    write(
        fixture.path(),
        ".github/workflows/ci.yml",
        "name: CI\non: push\n",
    );
    write(fixture.path(), "main.py", "x=1\n");
    assert!(has(&scan(fixture.path()), ".github"));
}

#[test]
fn test_detect_honors_git_info_exclude() {
    let fixture = fixture();
    write(fixture.path(), ".git/info/exclude", "worktrees/\n");
    write(fixture.path(), "worktrees/foo/dupe.py", "x=1\n");
    write(fixture.path(), "real.py", "x=2\n");
    let result = scan(fixture.path());
    assert!(!has(&result, "dupe.py"));
    assert!(has(&result, "real.py"));
}

#[test]
fn test_detect_extra_excludes_pattern() {
    let fixture = fixture();
    write(fixture.path(), "main.py", "x=1\n");
    write(fixture.path(), "secret.py", "x=2\n");
    write(fixture.path(), "legacy/old.py", "x=3\n");
    let result = scan_with(fixture.path(), |options| {
        options.extra_excludes = vec!["secret.py".into(), "legacy/".into()]
    });
    assert!(has(&result, "main.py"));
    assert!(!has(&result, "secret.py"));
    assert!(!has(&result, "legacy"));
}

#[test]
fn test_detect_keeps_env_source_dirs() {
    let fixture = fixture();
    write(
        fixture.path(),
        "src_env/env/ctrl_mem_env.py",
        "def build_env(): return 1\n",
    );
    write(
        fixture.path(),
        "src_env/other_dir/also_real.py",
        "def x(): return 2\n",
    );
    let result = scan(fixture.path());
    assert!(has(&result, "ctrl_mem_env.py"));
    assert!(has(&result, "also_real.py"));
    assert!(has(
        &scan(&fixture.path().join("src_env")),
        "ctrl_mem_env.py"
    ));
}

#[test]
fn test_detect_still_prunes_real_env_venv() {
    let fixture = fixture();
    write(fixture.path(), "env/pyvenv.cfg", "home=/usr/bin\n");
    write(fixture.path(), "env/lib/sixish.py", "x=1\n");
    write(fixture.path(), "main.py", "x=1\n");
    let result = scan(fixture.path());
    assert!(!has(&result, "sixish.py"));
    assert!(has(&result, "main.py"));
    assert!(result
        .pruned_noise_dirs
        .iter()
        .any(|path| path.contains("/env/")));
}

#[test]
fn test_detect_prunes_venv_names_without_markers() {
    let fixture = fixture();
    for name in ["venv", ".venv", "my_venv"] {
        write(fixture.path(), &format!("{name}/mod.py"), "x=1\n");
    }
    write(fixture.path(), "app.py", "x=1\n");
    let result = scan(fixture.path());
    assert!(has(&result, "app.py"));
    for name in ["venv", ".venv", "my_venv"] {
        assert!(!has(&result, &format!("/{name}/")));
    }
}

#[test]
#[cfg(unix)]
fn test_nested_graphify_out_prunes_only_configured_path() {
    for (configured, absolute, symlink_target) in [
        ("graphoxide-out/nlp", false, None),
        ("artifacts/nlp", false, None),
        ("artifacts/nlp", true, None),
        ("aliases/output-link", false, Some("artifacts/nlp")),
    ] {
        let fixture = fixture();
        let source = write(
            fixture.path(),
            "src/revil/nexus/nlp/core.py",
            "def tokenize(x): return x.split()\n",
        );
        let target = symlink_target.unwrap_or(configured);
        write(
            fixture.path(),
            &format!("{target}/generated.py"),
            "NOISE=1\n",
        );
        if let Some(target) = symlink_target {
            let link = fixture.path().join(configured);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            make_symlink(&fixture.path().join(target), &link);
        }
        let configured_path = if absolute {
            fixture.path().join(configured)
        } else {
            PathBuf::from(configured)
        };
        let result = scan_with(fixture.path(), |options| {
            options.output_dir = Some(configured_path)
        });
        let source = fs::canonicalize(source).unwrap();
        assert!(all_files(&result)
            .iter()
            .any(|path| Path::new(path) == source));
        assert!(!has(&result, "generated.py"));
    }
}

#[test]
fn test_detect_records_unclassified_extensionless_files() {
    let fixture = fixture();
    write(fixture.path(), "app.py", "x=1\n");
    for name in ["Dockerfile", "Makefile", "LICENSE"] {
        write(fixture.path(), name, "plain text\n");
    }
    let result = scan(fixture.path());
    let names: Vec<_> = result
        .unclassified
        .iter()
        .filter_map(|path| Path::new(path).file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["Dockerfile", "LICENSE", "Makefile"]);
    assert!(has(&result, "app.py"));
}

#[test]
fn test_detect_unclassified_empty_when_all_supported() {
    let fixture = fixture();
    write(fixture.path(), "a.py", "x=1\n");
    write(fixture.path(), "README.md", "# hi\n");
    assert!(scan(fixture.path()).unclassified.is_empty());
}

#[test]
fn test_graphifyinclude_is_inert_and_not_unclassified() {
    let fixture = fixture();
    write(fixture.path(), "main.py", "x=1\n");
    let baseline = scan(fixture.path());
    write(fixture.path(), ".graphifyinclude", ".github/\ndocs/**\n");
    let result = scan(fixture.path());
    assert!(result
        .unclassified
        .iter()
        .all(|path| !path.contains(".graphifyinclude")));
    assert_eq!(result.files, baseline.files);
}

#[test]
fn test_detect_reports_walk_errors_key() {
    let fixture = fixture();
    write(fixture.path(), "a.py", "def f(): pass\n");
    assert!(scan(fixture.path()).walk_errors.is_empty());
}

#[test]
#[cfg(unix)]
fn test_detect_surfaces_unreadable_dir_instead_of_silent_skip() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = fixture();
    write(fixture.path(), "a.py", "x=1\n");
    write(fixture.path(), "locked/b.py", "x=2\n");
    let locked = fixture.path().join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o0)).unwrap();
    if fs::read_dir(&locked).is_ok() {
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let result = scan(fixture.path());
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(has(&result, "a.py"));
    assert!(!result.walk_errors.is_empty());
}

#[test]
fn test_nested_gitignore_star_does_not_ignore_outside_its_dir() {
    let fixture = fixture();
    write(fixture.path(), "README.md", "# hello\n");
    write(fixture.path(), "main.py", "x=1\n");
    write(fixture.path(), ".hypothesis/.gitignore", "*\n");
    write(fixture.path(), ".hypothesis/cached.py", "x=2\n");
    assert_eq!(scan(fixture.path()).total_files, 2);
}

#[test]
fn test_nested_gitignore_patterns_still_apply_inside_their_dir() {
    let fixture = fixture();
    write(fixture.path(), "main.py", "x=1\n");
    write(fixture.path(), "sub/.gitignore", "*.log\n");
    write(fixture.path(), "sub/keep.py", "x=2\n");
    write(fixture.path(), "sub/noise.log", "z\n");
    assert_eq!(scan(fixture.path()).total_files, 2);
}

#[test]
fn test_nested_gitignore_does_not_govern_sibling_project() {
    let fixture = fixture();
    write(fixture.path(), "run.py", "x=1\n");
    write(
        fixture.path(),
        "project_a/data/loader.py",
        "def load(): pass\n",
    );
    write(fixture.path(), "project_b/.gitignore", "data/\n");
    write(fixture.path(), "project_b/data/dump.csv", "a,b\n");
    let result = scan(fixture.path());
    assert!(has(&result, "project_a/data/loader.py"));
    assert!(!has(&result, "dump.csv"));
    assert!(result
        .ignored
        .iter()
        .any(|path| path.contains("project_b/data")));
}

fn detected_files(kind: &str, paths: impl IntoIterator<Item = PathBuf>) -> DetectedFiles {
    BTreeMap::from([(
        kind.to_owned(),
        paths
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
    )])
}

fn rooted_manifest(root: &Path) -> SaveManifestOptions {
    SaveManifestOptions {
        root: Some(root.to_path_buf()),
        ..SaveManifestOptions::default()
    }
}

#[test]
fn test_save_manifest_skips_semantic_hash_for_files_without_cache() {
    let fixture = fixture();
    let first = write(fixture.path(), "docs/a.md", "# A\ncontent a\n");
    let second = write(fixture.path(), "docs/b.md", "# B\ncontent b\n");
    let manifest = fixture.path().join("manifest.json");
    save_manifest(
        &detected_files("document", [first.clone()]),
        &manifest,
        &SaveManifestOptions::default(),
    )
    .unwrap();
    let raw = load_manifest(&manifest, None);
    assert!(raw.contains_key(&first.to_string_lossy().into_owned()));
    assert!(!raw.contains_key(&second.to_string_lossy().into_owned()));
    assert_ne!(
        raw[&first.to_string_lossy().into_owned()]["semantic_hash"],
        ""
    );
}

#[test]
fn test_save_manifest_clear_semantic_erases_stale_hash_for_omitted_file() {
    let fixture = fixture();
    let doc = write(fixture.path(), "docs/doc.md", "# Doc\ncontent\n");
    let manifest = fixture.path().join("graphoxide-out/manifest.json");
    let corpus = BTreeSet::from([doc.to_string_lossy().into_owned()]);
    let mut options = rooted_manifest(fixture.path());
    options.scan_corpus = Some(corpus.clone());
    save_manifest(
        &detected_files("document", [doc.clone()]),
        &manifest,
        &options,
    )
    .unwrap();
    options.clear_semantic = corpus;
    save_manifest(
        &BTreeMap::from([("document".into(), Vec::new())]),
        &manifest,
        &options,
    )
    .unwrap();
    let raw: BTreeMap<String, Value> =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(raw["docs/doc.md"]["semantic_hash"], "");
    let incremental = detect_incremental(
        fixture.path(),
        &manifest,
        &DetectOptions::default(),
        ManifestKind::Semantic,
    )
    .unwrap();
    assert!(incremental.new_files["document"]
        .iter()
        .any(|path| path.ends_with("doc.md")));
}

#[test]
fn test_save_manifest_without_filter_unchanged_for_code() {
    let fixture = fixture();
    let code = write(fixture.path(), "main.py", "print('hello')\n");
    let manifest = fixture.path().join("manifest.json");
    save_manifest(
        &detected_files("code", [code.clone()]),
        &manifest,
        &SaveManifestOptions::default(),
    )
    .unwrap();
    let raw = load_manifest(&manifest, None);
    assert!(!raw[&code.to_string_lossy().into_owned()]["ast_hash"]
        .as_str()
        .unwrap()
        .is_empty());
}

#[test]
fn test_save_manifest_relativizes_keys_when_root_given() {
    let fixture = fixture();
    let code = write(fixture.path(), "src/foo.py", "def x(): pass\n");
    let doc = write(fixture.path(), "doc.md", "hello\n");
    let manifest = fixture.path().join("graphoxide-out/manifest.json");
    save_manifest(
        &BTreeMap::from([
            ("code".into(), vec![code.to_string_lossy().into_owned()]),
            ("document".into(), vec![doc.to_string_lossy().into_owned()]),
        ]),
        &manifest,
        &rooted_manifest(fixture.path()),
    )
    .unwrap();
    let raw: BTreeMap<String, Value> =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(
        raw.keys().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from(["doc.md".into(), "src/foo.py".into()])
    );
    let loaded = load_manifest(&manifest, Some(fixture.path()));
    let root = fs::canonicalize(fixture.path()).unwrap();
    assert!(loaded.contains_key(&root.join("src/foo.py").to_string_lossy().into_owned()));
    assert!(loaded.contains_key(&root.join("doc.md").to_string_lossy().into_owned()));
}

#[test]
fn test_save_manifest_without_root_keeps_absolute_keys() {
    let fixture = fixture();
    let code = write(fixture.path(), "foo.py", "pass\n");
    let manifest = fixture.path().join("manifest.json");
    save_manifest(
        &detected_files("code", [code.clone()]),
        &manifest,
        &SaveManifestOptions::default(),
    )
    .unwrap();
    let raw: BTreeMap<String, Value> =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert!(Path::new(raw.keys().next().unwrap()).is_absolute());
}

#[test]
fn test_load_manifest_absolutizes_relative_keys() {
    let fixture = fixture();
    let manifest = fixture.path().join("graphoxide-out/manifest.json");
    write(
        fixture.path(),
        "graphoxide-out/manifest.json",
        &serde_json::to_string(&json!({
            "src/foo.py": {"mtime": 0.0, "ast_hash": "h1", "semantic_hash": ""},
            "doc.md": {"mtime": 0.0, "ast_hash": "h2", "semantic_hash": ""}
        }))
        .unwrap(),
    );
    let loaded = load_manifest(&manifest, Some(fixture.path()));
    let root = fs::canonicalize(fixture.path()).unwrap();
    assert!(loaded.contains_key(&root.join("src/foo.py").to_string_lossy().into_owned()));
    assert!(loaded.contains_key(&root.join("doc.md").to_string_lossy().into_owned()));
}

#[test]
fn test_load_manifest_passes_through_legacy_absolute_keys() {
    let fixture = fixture();
    let key = fs::canonicalize(fixture.path())
        .unwrap()
        .join("foo.py")
        .to_string_lossy()
        .into_owned();
    let manifest = fixture.path().join("manifest.json");
    fs::write(
        &manifest,
        serde_json::to_vec(
            &json!({key.clone(): {"mtime": 0.0, "ast_hash": "h", "semantic_hash": ""}}),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(load_manifest(&manifest, Some(fixture.path())).contains_key(&key));
}

#[test]
fn test_save_manifest_out_of_root_keeps_absolute() {
    let fixture = fixture();
    let outside_dir = fixture.path().parent().unwrap().join(format!(
        "{}-outside",
        fixture.path().file_name().unwrap().to_string_lossy()
    ));
    fs::create_dir_all(&outside_dir).unwrap();
    let outside = write(&outside_dir, "outside.py", "pass\n");
    let manifest = fixture.path().join("manifest.json");
    save_manifest(
        &detected_files("code", [outside]),
        &manifest,
        &rooted_manifest(fixture.path()),
    )
    .unwrap();
    let raw: BTreeMap<String, Value> =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert!(Path::new(raw.keys().next().unwrap()).is_absolute());
    fs::remove_dir_all(outside_dir).unwrap();
}

#[test]
fn test_detect_incremental_portable_across_paths() {
    let fixture = fixture();
    let repo_a = fixture.path().join("repo_a");
    let a_code = write(&repo_a, "src/foo.py", "pass\n");
    let a_doc = write(&repo_a, "doc.md", "hello\n");
    let manifest_a = repo_a.join("graphoxide-out/manifest.json");
    save_manifest(
        &BTreeMap::from([
            ("code".into(), vec![a_code.to_string_lossy().into_owned()]),
            (
                "document".into(),
                vec![a_doc.to_string_lossy().into_owned()],
            ),
        ]),
        &manifest_a,
        &rooted_manifest(&repo_a),
    )
    .unwrap();
    let repo_b = fixture.path().join("repo_b");
    write(&repo_b, "src/foo.py", "pass\n");
    write(&repo_b, "doc.md", "hello\n");
    write_bytes(
        &repo_b,
        "graphoxide-out/manifest.json",
        &fs::read(&manifest_a).unwrap(),
    );
    let incremental = detect_incremental(
        &repo_b,
        &repo_b.join("graphoxide-out/manifest.json"),
        &DetectOptions::default(),
        ManifestKind::Semantic,
    )
    .unwrap();
    assert_eq!(incremental.new_total, 0);
}

fn rewrite_keys_decomposed(path: &Path) {
    use unicode_normalization::UnicodeNormalization;
    let raw: BTreeMap<String, Value> = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let decomposed: BTreeMap<_, _> = raw
        .into_iter()
        .map(|(key, value)| (key.nfd().collect::<String>(), value))
        .collect();
    fs::write(path, serde_json::to_vec(&decomposed).unwrap()).unwrap();
}

#[test]
fn test_manifest_nfc_keys_survive_macos_path_forms() {
    let fixture = fixture();
    let corpus = fixture.path().join("corpus");
    write(&corpus, "docs/café.md", "hello unicode\n");
    let manifest = fixture.path().join("out/manifest.json");
    let full = scan(&corpus);
    save_manifest(&full.files, &manifest, &rooted_manifest(&corpus)).unwrap();
    rewrite_keys_decomposed(&manifest);
    let incremental = detect_incremental(
        &corpus,
        &manifest,
        &DetectOptions::default(),
        ManifestKind::Semantic,
    )
    .unwrap();
    assert_eq!(incremental.new_total, 0);
    assert!(incremental.deleted_files.is_empty());
    assert!(incremental.excluded_files.is_empty());
}

#[test]
fn test_manifest_nfc_keys_legacy_absolute() {
    let fixture = fixture();
    let corpus = fixture.path().join("corpus");
    write(&corpus, "docs/café.md", "hello unicode\n");
    let manifest = fixture.path().join("out/manifest.json");
    save_manifest(
        &scan(&corpus).files,
        &manifest,
        &SaveManifestOptions::default(),
    )
    .unwrap();
    rewrite_keys_decomposed(&manifest);
    let incremental = detect_incremental(
        &corpus,
        &manifest,
        &DetectOptions::default(),
        ManifestKind::Semantic,
    )
    .unwrap();
    assert_eq!(incremental.new_total, 0);
    assert!(incremental.deleted_files.is_empty());
}

#[test]
#[cfg(unix)]
fn test_save_manifest_in_root_symlink_roundtrips() {
    let fixture = fixture();
    let target = write(fixture.path(), "sub/target.py", "pass\n");
    let alias = fixture.path().join("alias.py");
    make_symlink(&target, &alias);
    let manifest = fixture.path().join("manifest.json");
    save_manifest(
        &detected_files("code", [alias.clone()]),
        &manifest,
        &rooted_manifest(fixture.path()),
    )
    .unwrap();
    let raw: BTreeMap<String, Value> =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert!(raw.contains_key("alias.py"));
    assert!(!raw.contains_key("sub/target.py"));
    assert!(load_manifest(&manifest, Some(fixture.path())).contains_key(
        &fs::canonicalize(fixture.path())
            .unwrap()
            .join("alias.py")
            .to_string_lossy()
            .into_owned()
    ));
}

#[test]
fn test_save_manifest_full_scan_prunes_excluded_but_alive_row() {
    let fixture = fixture();
    let a = write(fixture.path(), "a.py", "x=1\n");
    let b = write(fixture.path(), "b.py", "x=2\n");
    let manifest = fixture.path().join("manifest.json");
    save_manifest(
        &detected_files("code", [a.clone(), b]),
        &manifest,
        &rooted_manifest(fixture.path()),
    )
    .unwrap();
    let mut options = rooted_manifest(fixture.path());
    options.scan_corpus = Some(BTreeSet::from([a.to_string_lossy().into_owned()]));
    save_manifest(&detected_files("code", [a]), &manifest, &options).unwrap();
    let raw: BTreeMap<String, Value> =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(raw.keys().collect::<Vec<_>>(), ["a.py"]);
}

#[test]
fn test_save_manifest_full_scan_still_prunes_missing_file() {
    let fixture = fixture();
    let a = write(fixture.path(), "a.py", "x=1\n");
    let gone = write(fixture.path(), "gone.py", "x=2\n");
    let manifest = fixture.path().join("manifest.json");
    save_manifest(
        &detected_files("code", [a.clone(), gone.clone()]),
        &manifest,
        &rooted_manifest(fixture.path()),
    )
    .unwrap();
    fs::remove_file(gone).unwrap();
    let mut options = rooted_manifest(fixture.path());
    options.scan_corpus = Some(BTreeSet::from([a.to_string_lossy().into_owned()]));
    save_manifest(&detected_files("code", [a]), &manifest, &options).unwrap();
    let raw: BTreeMap<String, Value> =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(raw.keys().collect::<Vec<_>>(), ["a.py"]);
}

#[test]
fn test_save_manifest_subset_save_preserves_untouched_rows() {
    let fixture = fixture();
    let a = write(fixture.path(), "a.py", "x=1\n");
    let b = write(fixture.path(), "b.py", "x=2\n");
    let manifest = fixture.path().join("manifest.json");
    let options = rooted_manifest(fixture.path());
    save_manifest(&detected_files("code", [a.clone(), b]), &manifest, &options).unwrap();
    save_manifest(&detected_files("code", [a]), &manifest, &options).unwrap();
    let raw: BTreeMap<String, Value> =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(raw.keys().collect::<Vec<_>>(), ["a.py", "b.py"]);
}

#[test]
fn test_save_manifest_full_scan_keeps_out_of_root_rows() {
    let fixture = fixture();
    let a = write(fixture.path(), "a.py", "x=1\n");
    let outside_dir = fixture.path().parent().unwrap().join(format!(
        "{}-external",
        fixture.path().file_name().unwrap().to_string_lossy()
    ));
    fs::create_dir_all(&outside_dir).unwrap();
    let outside = write(&outside_dir, "outside.py", "x=3\n");
    let outside_key = outside.to_string_lossy().into_owned();
    let manifest = fixture.path().join("manifest.json");
    save_manifest(
        &detected_files("code", [a.clone(), outside]),
        &manifest,
        &rooted_manifest(fixture.path()),
    )
    .unwrap();
    let mut options = rooted_manifest(fixture.path());
    options.scan_corpus = Some(BTreeSet::from([a.to_string_lossy().into_owned()]));
    save_manifest(&detected_files("code", [a]), &manifest, &options).unwrap();
    let raw: BTreeMap<String, Value> =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert!(raw.contains_key(&outside_key));
    fs::remove_dir_all(outside_dir).unwrap();
}

#[test]
fn test_detect_incremental_survives_dict_valued_mtime() {
    let fixture = fixture();
    let source = write(fixture.path(), "mod.py", "def f(): return 1\n");
    let manifest = fixture.path().join("graphoxide-out/manifest.json");
    write(
        fixture.path(),
        "graphoxide-out/manifest.json",
        &serde_json::to_string(&json!({
            source.to_string_lossy().into_owned(): {
                "mtime": {"mtime": 123.0},
                "ast_hash": "deadbeef",
                "semantic_hash": "cafebabe"
            }
        }))
        .unwrap(),
    );
    let result = detect_incremental(
        fixture.path(),
        &manifest,
        &DetectOptions::default(),
        ManifestKind::Semantic,
    )
    .unwrap();
    assert!(result.new_files["code"]
        .iter()
        .any(|path| path.ends_with("mod.py")));
}

#[test]
fn test_detect_incremental_legacy_float_reextracts_on_backwards_mtime() {
    let fixture = fixture();
    let source = write(fixture.path(), "mod.py", "def old(): return 1\n");
    let current = fs::metadata(&source)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let manifest = fixture.path().join("graphoxide-out/manifest.json");
    write(
        fixture.path(),
        "graphoxide-out/manifest.json",
        &serde_json::to_string(&json!({source.to_string_lossy().into_owned(): current + 3600.0}))
            .unwrap(),
    );
    let result = detect_incremental(
        fixture.path(),
        &manifest,
        &DetectOptions::default(),
        ManifestKind::Semantic,
    )
    .unwrap();
    assert!(result.new_files["code"]
        .iter()
        .any(|path| path.ends_with("mod.py")));
}

#[test]
fn test_detect_incremental_legacy_float_skips_when_mtime_matches() {
    let fixture = fixture();
    let source = write(fixture.path(), "mod.py", "def stable(): return 1\n");
    // This nanosecond timestamp regressed under serde_json's faster, inexact
    // float parser: 1785781385.8600407 reloaded as 1785781385.860041.
    set_file_mtime(
        &source,
        FileTime::from_unix_time(1_785_781_385, 860_040_700),
    )
    .unwrap();
    let current = fs::metadata(&source)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let manifest = fixture.path().join("graphoxide-out/manifest.json");
    write(
        fixture.path(),
        "graphoxide-out/manifest.json",
        &serde_json::to_string(&json!({source.to_string_lossy().into_owned(): current})).unwrap(),
    );
    let result = detect_incremental(
        fixture.path(),
        &manifest,
        &DetectOptions::default(),
        ManifestKind::Semantic,
    )
    .unwrap();
    assert!(result.new_files["code"].is_empty());
    assert!(result.unchanged_files["code"]
        .iter()
        .any(|path| path.ends_with("mod.py")));
}

#[test]
#[cfg(unix)]
fn test_detect_incremental_propagates_follow_symlinks() {
    let fixture = fixture();
    let real = fixture.path().join("real_corpus");
    write(&real, "note.md", "# real note\nsome content\n");
    make_symlink(&real, &fixture.path().join("linked_corpus"));
    let manifest = fixture.path().join("graphoxide-out/manifest.json");
    let no = detect_incremental(
        fixture.path(),
        &manifest,
        &DetectOptions::default(),
        ManifestKind::Semantic,
    )
    .unwrap();
    assert!(!no.new_files["document"]
        .iter()
        .any(|path| path.contains("linked_corpus")));
    let options = DetectOptions {
        follow_symlinks: true,
        ..DetectOptions::default()
    };
    let yes =
        detect_incremental(fixture.path(), &manifest, &options, ManifestKind::Semantic).unwrap();
    assert!(yes.new_files["document"]
        .iter()
        .any(|path| path.contains("real_corpus")));
    assert!(!yes.new_files["document"]
        .iter()
        .any(|path| path.contains("linked_corpus")));
    assert_eq!(yes.new_total, 1);
    save_manifest(
        &yes.detection.files,
        &manifest,
        &rooted_manifest(fixture.path()),
    )
    .unwrap();
    assert_eq!(
        detect_incremental(fixture.path(), &manifest, &options, ManifestKind::Semantic)
            .unwrap()
            .new_total,
        0
    );
}

#[test]
fn test_detect_incremental_reports_excluded_not_deleted() {
    let fixture = fixture();
    write(fixture.path(), "a.py", "x=1\n");
    write(fixture.path(), "b.py", "x=2\n");
    let manifest = fixture.path().join("graphoxide-out/manifest.json");
    save_manifest(
        &scan(fixture.path()).files,
        &manifest,
        &rooted_manifest(fixture.path()),
    )
    .unwrap();
    let mut options = DetectOptions::default();
    options.extra_excludes.push("b.py".into());
    let result =
        detect_incremental(fixture.path(), &manifest, &options, ManifestKind::Semantic).unwrap();
    assert!(result.deleted_files.is_empty());
    assert_eq!(
        result
            .excluded_files
            .iter()
            .filter_map(|path| Path::new(path).file_name())
            .collect::<Vec<_>>(),
        ["b.py"]
    );
}

#[test]
fn test_detect_incremental_still_reports_real_deletions() {
    let fixture = fixture();
    write(fixture.path(), "a.py", "x=1\n");
    let b = write(fixture.path(), "b.py", "x=2\n");
    let manifest = fixture.path().join("graphoxide-out/manifest.json");
    save_manifest(
        &scan(fixture.path()).files,
        &manifest,
        &rooted_manifest(fixture.path()),
    )
    .unwrap();
    fs::remove_file(b).unwrap();
    let result = detect_incremental(
        fixture.path(),
        &manifest,
        &DetectOptions::default(),
        ManifestKind::Semantic,
    )
    .unwrap();
    assert_eq!(
        result
            .deleted_files
            .iter()
            .filter_map(|path| Path::new(path).file_name())
            .collect::<Vec<_>>(),
        ["b.py"]
    );
    assert!(result.excluded_files.is_empty());
}

#[test]
fn test_detect_incremental_exclusion_stable_across_runs() {
    let fixture = fixture();
    write(fixture.path(), "a.py", "x=1\n");
    write(fixture.path(), "b.py", "x=2\n");
    let manifest = fixture.path().join("graphoxide-out/manifest.json");
    save_manifest(
        &scan(fixture.path()).files,
        &manifest,
        &rooted_manifest(fixture.path()),
    )
    .unwrap();
    let mut detect_options = DetectOptions::default();
    detect_options.extra_excludes.push("b.py".into());
    let first = detect_incremental(
        fixture.path(),
        &manifest,
        &detect_options,
        ManifestKind::Semantic,
    )
    .unwrap();
    assert_eq!(first.excluded_files.len(), 1);
    let corpus: BTreeSet<_> = first.detection.files.values().flatten().cloned().collect();
    let mut save_options = rooted_manifest(fixture.path());
    save_options.scan_corpus = Some(corpus);
    save_manifest(&first.detection.files, &manifest, &save_options).unwrap();
    let second = detect_incremental(
        fixture.path(),
        &manifest,
        &detect_options,
        ManifestKind::Semantic,
    )
    .unwrap();
    assert!(second.deleted_files.is_empty());
    assert!(second.excluded_files.is_empty());
}

#[test]
fn test_convert_office_file_hash_stable_across_nfc_nfd() {
    use unicode_normalization::UnicodeNormalization;
    let fixture = fixture();
    let out = fixture.path().join("converted");
    let nfc = "café.docx".nfc().collect::<String>();
    let nfd = "café.docx".nfd().collect::<String>();
    let first = convert_office_text(
        &fixture.path().join("report").join(nfc),
        &out,
        None,
        "hello",
    )
    .unwrap()
    .unwrap();
    let second = convert_office_text(
        &fixture.path().join("report").join(nfd),
        &out,
        None,
        "hello",
    )
    .unwrap()
    .unwrap();
    let suffix = |path: &Path| {
        path.file_name()
            .unwrap()
            .to_string_lossy()
            .rsplit('_')
            .next()
            .unwrap()
            .to_owned()
    };
    assert_eq!(suffix(&first), suffix(&second));
}

#[test]
fn test_convert_office_file_does_not_rewrite_existing_sidecar() {
    let fixture = fixture();
    let source = write_bytes(fixture.path(), "doc.docx", b"source");
    let out = fixture.path().join("converted");
    let first = convert_office_text(&source, &out, None, "hello world")
        .unwrap()
        .unwrap();
    let before = fs::metadata(&first).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(2));
    let second = convert_office_text(&source, &out, None, "changed text")
        .unwrap()
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(fs::metadata(second).unwrap().modified().unwrap(), before);
}

#[test]
fn test_convert_office_file_sidecar_name_stable_across_checkouts() {
    let fixture = fixture();
    let checkout_a = fixture.path().join("checkout-a");
    let checkout_b = fixture.path().join("somewhere/checkout-b");
    let a = convert_office_text(
        &checkout_a.join("docs/report.xlsx"),
        &checkout_a.join("graphoxide-out/converted"),
        Some(&checkout_a),
        "sheet body",
    )
    .unwrap()
    .unwrap();
    let b = convert_office_text(
        &checkout_b.join("docs/report.xlsx"),
        &checkout_b.join("graphoxide-out/converted"),
        Some(&checkout_b),
        "sheet body",
    )
    .unwrap()
    .unwrap();
    assert_eq!(a.file_name(), b.file_name());
    let fallback = convert_office_text(
        &checkout_a.join("docs/report.xlsx"),
        &checkout_a.join("graphoxide-out/converted"),
        None,
        "sheet body",
    )
    .unwrap()
    .unwrap();
    assert_eq!(fallback.file_name(), a.file_name());
}

#[test]
fn test_convert_office_file_hash_disambiguates_same_stem() {
    let fixture = fixture();
    let root = fixture.path().join("repo");
    let out = root.join("graphoxide-out/converted");
    let a = convert_office_text(&root.join("a/report.xlsx"), &out, Some(&root), "body")
        .unwrap()
        .unwrap();
    let b = convert_office_text(&root.join("b/report.xlsx"), &out, Some(&root), "body")
        .unwrap()
        .unwrap();
    assert_ne!(a.file_name(), b.file_name());
}

#[test]
fn test_convert_office_file_outside_root_falls_back() {
    let fixture = fixture();
    let root = fixture.path().join("repo");
    let outside = fixture.path().join("elsewhere/doc.docx");
    let out = root.join("graphoxide-out/converted");
    let first = convert_office_text(&outside, &out, Some(&root), "body")
        .unwrap()
        .unwrap();
    let second = convert_office_text(&outside, &out, Some(&root), "body")
        .unwrap()
        .unwrap();
    assert_eq!(first, second);
}
