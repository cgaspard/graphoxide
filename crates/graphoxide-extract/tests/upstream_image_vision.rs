//! Executable parity port of Graphify v0.9.32 `tests/test_image_vision.py`.

use graphoxide_core::{
    estimate_file_tokens, pack_chunks_by_tokens, FileUnit, IMAGE_TOKEN_ESTIMATE,
    MAX_IMAGES_PER_CHUNK,
};
use graphoxide_extract::{
    llm::{build_claude_cli_file_request, build_direct_extraction_plan_paths},
    vision::{
        anthropic_content, backend_supports_vision, bedrock_content, bedrock_response_text,
        build_bedrock_request_plan, build_image_refs, build_image_refs_with_limit,
        claude_cli_vision_plan, file_to_text, openai_content, parse_bedrock_response,
        partition_semantic_files, read_semantic_files, read_semantic_files_with_pdf, strip_pixels,
        BedrockContentBlock,
    },
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, path::PathBuf};
use tempfile::TempDir;

const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nFAKEPIXELDATA";

fn env(values: &[(&str, &str)]) -> BTreeMap<String, String> {
    values
        .iter()
        .map(|(key, value)| ((*key).into(), (*value).into()))
        .collect()
}

fn corpus(directory: &TempDir) -> (PathBuf, PathBuf, PathBuf) {
    fs::create_dir(directory.path().join("sub")).unwrap();
    let image = directory.path().join("sub/diagram.png");
    fs::write(&image, PNG_BYTES).unwrap();
    let svg = directory.path().join("icon.svg");
    fs::write(&svg, "<svg><rect/></svg>").unwrap();
    let document = directory.path().join("README.md");
    fs::write(&document, "# Title\nbody").unwrap();
    (image, svg, document)
}

fn symlink_file(source: &std::path::Path, target: &std::path::Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(source, target).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(source, target).unwrap();
}

fn image_refs(directory: &TempDir) -> Vec<graphoxide_extract::vision::ImageRef> {
    let (image, _, _) = corpus(directory);
    build_image_refs(&[image], directory.path(), true).refs
}

fn node_json() -> Value {
    json!({
        "nodes": [{"id": "x", "label": "L", "file_type": "image", "source_file": "a.png"}],
        "edges": [],
        "hyperedges": []
    })
}

fn bedrock_response(blocks: Vec<Value>) -> Value {
    json!({"output": {"message": {"content": blocks}}})
}

#[test]
fn test_pdf_routed_through_bounded_pdf_reader_not_readtext() {
    let directory = TempDir::new().unwrap();
    let pdf = directory.path().join("paper.pdf");
    fs::write(&pdf, b"%PDF-1.4 RAWBINARYGARBAGE\0\xff").unwrap();
    let mut called = false;
    let result =
        read_semantic_files_with_pdf(&[FileUnit::Path(pdf.clone())], directory.path(), |path| {
            called = true;
            assert_eq!(path, fs::canonicalize(&pdf).unwrap());
            Ok("EXTRACTED PDF TEXT".into())
        });
    assert!(called);
    assert!(result.text.contains("EXTRACTED PDF TEXT"));
    assert!(!result.text.contains("RAWBINARYGARBAGE"));
}

#[test]
fn test_pdf_is_not_treated_as_vision_image() {
    let directory = TempDir::new().unwrap();
    let pdf = directory.path().join("paper.pdf");
    fs::write(&pdf, b"%PDF-1.4").unwrap();
    let unit = FileUnit::Path(pdf.clone());
    let (text, images) = partition_semantic_files(std::slice::from_ref(&unit));
    assert_eq!(text, [unit]);
    assert!(images.is_empty());
}

#[test]
fn test_non_pdf_still_read_as_plain_text() {
    let directory = TempDir::new().unwrap();
    let markdown = directory.path().join("a.md");
    fs::write(&markdown, "# hello").unwrap();
    assert!(file_to_text(&markdown).unwrap().contains("# hello"));
}

#[test]
fn test_read_files_skips_out_of_root_symlink() {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("root");
    let outside = directory.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    let secret = outside.join("secret.md");
    fs::write(&secret, "SECRET SHOULD NOT REACH THE PROMPT").unwrap();
    let link = root.join("secret.md");
    symlink_file(&secret, &link);
    let result = read_semantic_files(&[FileUnit::Path(link)], &root);
    assert!(result.text.is_empty());
    assert!(!result.text.contains("SECRET SHOULD NOT REACH THE PROMPT"));
    assert_eq!(result.warnings.len(), 1);
}

#[test]
fn test_partition_splits_raster_from_text() {
    let directory = TempDir::new().unwrap();
    let (image, svg, document) = corpus(&directory);
    let units = vec![
        FileUnit::Path(document.clone()),
        FileUnit::Path(image.clone()),
        FileUnit::Path(svg.clone()),
    ];
    let (text, images) = partition_semantic_files(&units);
    assert_eq!(images, [image]);
    assert_eq!(text.len(), 2);
    assert!(text.contains(&FileUnit::Path(document)));
    assert!(text.contains(&FileUnit::Path(svg)));
}

#[test]
fn test_build_image_refs_sets_rel_media_and_bytes() {
    let directory = TempDir::new().unwrap();
    let refs = image_refs(&directory);
    let image = &refs[0];
    assert_eq!(image.rel, "sub/diagram.png");
    assert_eq!(image.media_type, "image/png");
    assert_eq!(image.raw.as_deref(), Some(PNG_BYTES));
    assert!(!image.b64().is_empty());
    assert_eq!(image.bedrock_format(), "png");
}

#[test]
fn test_build_image_refs_skips_out_of_root_symlink() {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("root");
    let outside = directory.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    let secret = outside.join("secret.png");
    fs::write(&secret, PNG_BYTES).unwrap();
    let link = root.join("secret.png");
    symlink_file(&secret, &link);
    let result = build_image_refs(&[link], &root, true);
    assert!(result.refs.is_empty());
    assert_eq!(result.warnings.len(), 1);
}

#[test]
fn test_build_image_refs_drops_oversized() {
    let directory = TempDir::new().unwrap();
    let image = directory.path().join("big.jpg");
    fs::write(&image, vec![b'x'; 64]).unwrap();
    let result = build_image_refs_with_limit(&[image], directory.path(), true, 8);
    assert!(result.refs[0].raw.is_none());
    assert_eq!(result.refs[0].media_type, "image/jpeg");
}

#[test]
fn test_path_backend_skips_byte_read_and_size_cap() {
    let directory = TempDir::new().unwrap();
    let image = directory.path().join("huge.png");
    fs::write(&image, vec![b'x'; 64]).unwrap();
    let result = build_image_refs_with_limit(&[image], directory.path(), false, 8);
    assert!(result.refs[0].raw.is_none());
    assert_eq!(result.refs[0].rel, "huge.png");
    assert_eq!(result.refs[0].path.file_name().unwrap(), "huge.png");
    assert!(result.warnings.is_empty());
}

#[test]
fn test_claude_cli_passes_oversized_image_by_path() {
    let directory = TempDir::new().unwrap();
    let image = directory.path().join("huge.png");
    fs::write(&image, vec![b'x'; 100]).unwrap();
    let refs = build_image_refs_with_limit(&[image], directory.path(), false, 8).refs;
    let plan = claude_cli_vision_plan("CORPUS", &refs);
    assert!(plan
        .user_message
        .contains(&refs[0].path.to_string_lossy().to_string()));
}

#[test]
fn test_capability_flags() {
    for backend in [
        "claude",
        "claude-cli",
        "openai",
        "gemini",
        "bedrock",
        "kimi",
    ] {
        assert!(
            backend_supports_vision(backend, &BTreeMap::new()),
            "{backend}"
        );
    }
    assert!(!backend_supports_vision("deepseek", &BTreeMap::new()));
    assert!(!backend_supports_vision("ollama", &BTreeMap::new()));
    assert!(backend_supports_vision(
        "ollama",
        &env(&[("GRAPHIFY_OLLAMA_VISION", "1")])
    ));
}

#[test]
fn test_image_token_estimate_is_flat() {
    let directory = TempDir::new().unwrap();
    let (image, _, _) = corpus(&directory);
    assert_eq!(
        estimate_file_tokens(&FileUnit::Path(image)),
        IMAGE_TOKEN_ESTIMATE
    );
}

#[test]
fn test_chunk_packing_caps_images_per_chunk() {
    let directory = TempDir::new().unwrap();
    let mut units = Vec::new();
    for index in 0..MAX_IMAGES_PER_CHUNK * 2 + 3 {
        let image = directory.path().join(format!("img{index:03}.png"));
        fs::write(&image, PNG_BYTES).unwrap();
        units.push(FileUnit::Path(image));
    }
    let chunks = pack_chunks_by_tokens(&units, 10_000_000);
    assert!(chunks.len() >= 3);
    assert!(chunks.iter().all(|chunk| {
        chunk
            .iter()
            .filter(|unit| matches!(unit, FileUnit::Path(path) if graphoxide_extract::vision::is_vision_image(path)))
            .count()
            <= MAX_IMAGES_PER_CHUNK
    }));
}

#[test]
fn test_anthropic_content_has_base64_block() {
    let directory = TempDir::new().unwrap();
    let refs = image_refs(&directory);
    let content = anthropic_content("CORPUS", &refs);
    let blocks = content.as_array().unwrap();
    assert_eq!(blocks[0]["type"], "image");
    assert_eq!(blocks[0]["source"]["type"], "base64");
    assert_eq!(blocks[0]["source"]["media_type"], "image/png");
    assert_eq!(blocks[0]["source"]["data"], refs[0].b64());
    assert_eq!(blocks.last().unwrap()["type"], "text");
    assert!(blocks.last().unwrap()["text"]
        .as_str()
        .unwrap()
        .contains("CORPUS"));
}

#[test]
fn test_openai_content_has_data_uri() {
    let directory = TempDir::new().unwrap();
    let refs = image_refs(&directory);
    let content = openai_content("CORPUS", &refs);
    let blocks = content.as_array().unwrap();
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[1]["type"], "image_url");
    assert_eq!(
        blocks[1]["image_url"]["url"],
        format!("data:image/png;base64,{}", refs[0].b64())
    );
}

#[test]
fn test_bedrock_content_uses_raw_bytes() {
    let directory = TempDir::new().unwrap();
    let refs = image_refs(&directory);
    let content = bedrock_content("CORPUS", &refs);
    assert_eq!(
        content[0],
        BedrockContentBlock::Image {
            format: "png".into(),
            bytes: PNG_BYTES.to_vec()
        }
    );
    assert!(matches!(&content[1], BedrockContentBlock::Text { text } if text.contains("CORPUS")));
}

#[test]
fn test_builders_fall_back_to_string_without_pixels() {
    let directory = TempDir::new().unwrap();
    let refs = strip_pixels(&image_refs(&directory));
    let anthropic = anthropic_content("CORPUS", &refs);
    let openai = openai_content("CORPUS", &refs);
    assert!(anthropic.as_str().unwrap().contains("sub/diagram.png"));
    assert!(openai.as_str().unwrap().contains("sub/diagram.png"));
}

#[test]
fn test_no_images_is_byte_identical() {
    assert_eq!(anthropic_content("PLAIN", &[]), "PLAIN");
    assert_eq!(openai_content("PLAIN", &[]), "PLAIN");
}

#[test]
fn test_call_claude_sends_image_block() {
    let directory = TempDir::new().unwrap();
    let (image, _, _) = corpus(&directory);
    let plan = build_direct_extraction_plan_paths(
        &[image],
        "claude",
        directory.path(),
        &env(&[("ANTHROPIC_API_KEY", "k")]),
        false,
    )
    .unwrap();
    assert!(plan
        .request
        .user_content
        .as_array()
        .unwrap()
        .iter()
        .any(|block| block["type"] == "image"));
}

#[test]
fn test_call_openai_compat_sends_image_url() {
    let directory = TempDir::new().unwrap();
    let (image, _, _) = corpus(&directory);
    let plan = build_direct_extraction_plan_paths(
        &[image],
        "openai",
        directory.path(),
        &env(&[("OPENAI_API_KEY", "k")]),
        false,
    )
    .unwrap();
    assert!(plan
        .request
        .user_content
        .as_array()
        .unwrap()
        .iter()
        .any(|part| part["type"] == "image_url"));
}

#[test]
fn test_call_openai_compat_text_only_without_images() {
    let directory = TempDir::new().unwrap();
    let source = directory.path().join("doc.md");
    fs::write(&source, "CORPUS").unwrap();
    let plan = build_direct_extraction_plan_paths(
        &[source],
        "openai",
        directory.path(),
        &env(&[("OPENAI_API_KEY", "k")]),
        false,
    )
    .unwrap();
    assert_eq!(plan.request.user_content, plan.request.user);
}

#[test]
fn test_call_bedrock_sends_raw_image_bytes() {
    let directory = TempDir::new().unwrap();
    let refs = image_refs(&directory);
    let plan =
        build_bedrock_request_plan("model", "system", "CORPUS", &refs, 8_192, &BTreeMap::new());
    assert!(
        matches!(&plan.content[0], BedrockContentBlock::Image { bytes, .. } if bytes == PNG_BYTES)
    );
}

#[test]
fn test_bedrock_response_text_single_text_block_unchanged() {
    let expected = node_json().to_string();
    assert_eq!(
        bedrock_response_text(&bedrock_response(vec![json!({"text": expected})]), ""),
        expected
    );
}

#[test]
fn test_bedrock_response_text_skips_leading_reasoning_block() {
    let expected = node_json().to_string();
    let response = bedrock_response(vec![
        json!({"reasoningContent": {"reasoningText": {"text": "deliberating"}}}),
        json!({"text": expected}),
    ]);
    assert_eq!(bedrock_response_text(&response, "{}"), expected);
}

fn assert_skips_leading(leading: Value) {
    let expected = node_json().to_string();
    let response = bedrock_response(vec![leading, json!({"text": expected})]);
    assert_eq!(bedrock_response_text(&response, "{}"), expected);
}

#[test]
fn test_bedrock_response_text_skips_non_text_leading_blocks_leading0() {
    assert_skips_leading(json!({"reasoningContent": {}}));
}

#[test]
fn test_bedrock_response_text_skips_non_text_leading_blocks_leading1() {
    assert_skips_leading(json!({"toolUse": {"name": "x", "input": {}}}));
}

#[test]
fn test_bedrock_response_text_skips_non_text_leading_blocks_leading2() {
    assert_skips_leading(json!({"someFutureBlockType": {"a": 1}}));
}

#[test]
fn test_bedrock_response_text_skips_non_text_leading_blocks_leading3() {
    assert_skips_leading(json!({"text": "   "}));
}

fn assert_bedrock_fallback(response: Value) {
    assert_eq!(bedrock_response_text(&response, "SENTINEL"), "SENTINEL");
}

#[test]
fn test_bedrock_response_text_falls_back_without_text_resp0() {
    assert_bedrock_fallback(json!({"output": {"message": {"content": []}}}));
}

#[test]
fn test_bedrock_response_text_falls_back_without_text_resp1() {
    assert_bedrock_fallback(
        json!({"output": {"message": {"content": [{"reasoningContent": {}}]}}}),
    );
}

#[test]
fn test_bedrock_response_text_falls_back_without_text_resp2() {
    assert_bedrock_fallback(json!({"output": {"message": {"content": "not-a-list"}}}));
}

#[test]
fn test_bedrock_response_text_falls_back_without_text_resp3() {
    assert_bedrock_fallback(json!({"output": {}}));
}

#[test]
fn test_bedrock_response_text_falls_back_without_text_resp4() {
    assert_bedrock_fallback(json!({}));
}

#[test]
fn test_bedrock_response_text_tolerates_malformed_blocks() {
    let expected = node_json().to_string();
    let response = bedrock_response(vec![
        json!("not-a-dict"),
        json!({"text": 123}),
        json!({"text": expected}),
    ]);
    assert_eq!(bedrock_response_text(&response, "{}"), expected);
}

#[test]
fn test_call_bedrock_parses_reasoning_model_response() {
    let response = json!({
        "output": {"message": {"content": [
            {"reasoningContent": {"reasoningText": {"text": "think"}}},
            {"text": node_json().to_string()}
        ]}},
        "usage": {"inputTokens": 1, "outputTokens": 2},
        "stopReason": "end_turn"
    });
    let parsed = parse_bedrock_response(&response, "model").unwrap();
    assert_eq!(parsed.fragment["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(parsed.finish_reason, "stop");
}

#[test]
fn test_call_bedrock_honors_api_timeout() {
    let plan = build_bedrock_request_plan(
        "model",
        "system",
        "CORPUS",
        &[],
        8_192,
        &env(&[("GRAPHIFY_API_TIMEOUT", "1800")]),
    );
    assert_eq!(plan.read_timeout_seconds, 1_800.0);
    assert_eq!(plan.connect_timeout_seconds, 10);
    assert_eq!(plan.max_attempts, 7);
    assert_eq!(plan.retry_mode, "adaptive");
}

#[test]
fn test_call_bedrock_api_timeout_defaults_when_unset() {
    let plan =
        build_bedrock_request_plan("model", "system", "CORPUS", &[], 8_192, &BTreeMap::new());
    assert_eq!(plan.read_timeout_seconds, 600.0);
}

#[test]
fn test_claude_cli_adds_dir_and_read_instruction() {
    let directory = TempDir::new().unwrap();
    let (image, _, _) = corpus(&directory);
    let request = build_claude_cli_file_request(
        "claude",
        &[FileUnit::Path(image.clone())],
        directory.path(),
        None,
        false,
        &BTreeMap::new(),
    )
    .unwrap();
    let add_dir = request
        .argv
        .iter()
        .position(|value| value == "--add-dir")
        .unwrap();
    assert_eq!(
        request.argv[add_dir + 1],
        fs::canonicalize(image.parent().unwrap())
            .unwrap()
            .to_string_lossy()
    );
    assert!(request.stdin.contains("Read tool"));
    assert!(request.stdin.contains(
        &fs::canonicalize(image)
            .unwrap()
            .to_string_lossy()
            .to_string()
    ));
}

#[test]
fn test_extract_files_direct_gates_pixels_by_capability() {
    let directory = TempDir::new().unwrap();
    let (image, _, document) = corpus(&directory);
    let files = [document, image];
    let openai = build_direct_extraction_plan_paths(
        &files,
        "openai",
        directory.path(),
        &env(&[("OPENAI_API_KEY", "k")]),
        false,
    )
    .unwrap();
    assert!(openai.request.user_content.is_array());

    let deepseek = build_direct_extraction_plan_paths(
        &files,
        "deepseek",
        directory.path(),
        &env(&[("DEEPSEEK_API_KEY", "k")]),
        false,
    )
    .unwrap();
    assert!(deepseek.request.user_content.is_string());
    assert!(deepseek
        .request
        .user_content
        .as_str()
        .unwrap()
        .contains("sub/diagram.png"));
}
