use graphoxide_extract::llm::build_claude_cli_request;

#[test]
fn test_instructions_ride_in_user_turn_not_system_prompt() {
    let request = build_claude_cli_request("payload", None);
    assert!(!request.argv.iter().any(|arg| arg == "--system-prompt"));
    assert!(!request
        .argv
        .iter()
        .any(|arg| arg == "--append-system-prompt"));
    assert!(request.stdin.contains("graphify semantic extraction agent"));
    assert!(request.stdin.contains("output ONLY the JSON object"));
    assert!(request.stdin.contains("payload"));
}

#[test]
fn test_model_env_var_adds_model_flag() {
    let request = build_claude_cli_request("payload", Some("haiku"));
    let index = request
        .argv
        .iter()
        .position(|arg| arg == "--model")
        .unwrap();
    assert_eq!(request.argv[index + 1], "haiku");
}

#[test]
fn test_no_model_flag_when_env_var_unset() {
    assert!(!build_claude_cli_request("payload", None)
        .argv
        .iter()
        .any(|arg| arg == "--model"));
}
