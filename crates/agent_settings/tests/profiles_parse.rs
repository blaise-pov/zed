use settings::{RootUserSettings as _, SettingsContent};

#[test]
fn diagnose_profiles_parse() {
    let json = r#"{
        "agent": {
            "default_profile": "orchestrator",
            "profiles": {
                "orchestrator": {
                    "name": "Orchestrator",
                    "description": "desc",
                    "custom_prompt": "prompt",
                    "delegation": { "allowed": ["backend"], "max_depth": 2 },
                    "context_servers": { "postgres": true, "docker": false }
                },
                "architect": {
                    "name": "Architect",
                    "context_servers": { "ast-grep": true },
                    "tools": { "write_file": false, "edit_file": false }
                },
                "backend": {
                    "name": "Backend",
                    "delegation": { "allowed": ["repository-engineer"], "max_depth": 1 },
                    "skills": ["vertical-slice"],
                    "context_servers": { "postgres": true }
                },
                "plain-guy": {
                    "name": "Plain"
                }
            }
        }
    }"#;

    let (content, status) = SettingsContent::parse_json(json);
    match status {
        settings::ParseStatus::Success => println!("PARSE: Success"),
        settings::ParseStatus::Unchanged => println!("PARSE: Unchanged"),
        settings::ParseStatus::Failed { error } => println!("PARSE: Failed: {error}"),
    }
    let profiles: Vec<String> = content
        .as_ref()
        .and_then(|c| c.agent.as_ref())
        .and_then(|a| a.profiles.as_ref())
        .map(|p| p.keys().map(|k| k.to_string()).collect())
        .unwrap_or_default();
    println!("PROFILES: {profiles:?}");
    assert!(
        profiles.contains(&"orchestrator".to_string())
            && profiles.contains(&"architect".to_string())
            && profiles.contains(&"backend".to_string())
            && profiles.contains(&"plain-guy".to_string()),
        "some profiles were dropped: {profiles:?}"
    );
}

#[test]
fn test_profiles_with_custom_prompt_path() {
    let temp_dir = std::env::temp_dir().join(format!(
        "zed_test_profiles_parse_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let prompt_file = temp_dir.join("system_prompt.txt");
    std::fs::write(&prompt_file, "System prompt from external file").unwrap();

    let file_path_str = prompt_file.to_str().unwrap().replace('\\', "/");

    let json = format!(
        r#"{{
        "agent": {{
            "profiles": {{
                "file-priority": {{
                    "name": "File Priority",
                    "custom_prompt": "Ignored inline text",
                    "custom_prompt_path": "{file_path_str}"
                }},
                "text-only": {{
                    "name": "Text Only",
                    "custom_prompt": "Direct inline prompt"
                }},
                "file-only": {{
                    "name": "File Only",
                    "custom_prompt_path": "{file_path_str}"
                }}
            }}
        }}
    }}"#
    );

    let (content, status) = SettingsContent::parse_json(&json);
    assert_eq!(status, settings::ParseStatus::Success);

    let content = content.unwrap();
    let agent_content = content.agent.as_ref().unwrap();
    let profiles_content = agent_content.profiles.as_ref().unwrap();

    // 1. Check file-priority: both specified, file wins
    let file_priority_content = profiles_content.get("file-priority").unwrap();
    let profile_settings =
        agent_settings::AgentProfileSettings::from(file_priority_content.clone());
    assert_eq!(
        profile_settings.custom_prompt.as_deref(),
        Some("System prompt from external file")
    );
    assert_eq!(
        profile_settings.custom_prompt_path.as_deref(),
        Some(file_path_str.as_str())
    );

    // 2. Check text-only: only text specified
    let text_only_content = profiles_content.get("text-only").unwrap();
    let profile_settings = agent_settings::AgentProfileSettings::from(text_only_content.clone());
    assert_eq!(
        profile_settings.custom_prompt.as_deref(),
        Some("Direct inline prompt")
    );
    assert_eq!(profile_settings.custom_prompt_path, None);

    // 3. Check file-only: only file specified
    let file_only_content = profiles_content.get("file-only").unwrap();
    let profile_settings = agent_settings::AgentProfileSettings::from(file_only_content.clone());
    assert_eq!(
        profile_settings.custom_prompt.as_deref(),
        Some("System prompt from external file")
    );
    assert_eq!(
        profile_settings.custom_prompt_path.as_deref(),
        Some(file_path_str.as_str())
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_model_env_vars_expansion() {
    unsafe {
        std::env::set_var("ZED_TEST_MODEL_PROVIDER", "TestProvider");
        std::env::set_var("ZED_TEST_MODEL_NAME", "test-model-4");
    }

    let json = r#"{
        "agent": {
            "default_model": {
                "provider": "${ZED_TEST_MODEL_PROVIDER}",
                "model": "${ZED_TEST_MODEL_NAME}"
            },
            "profiles": {
                "custom": {
                    "name": "Custom Profile",
                    "default_model": {
                        "provider": "${ZED_TEST_FALLBACK_PROVIDER:-FallbackProvider}",
                        "model": "${ZED_TEST_MODEL_NAME}"
                    }
                }
            }
        }
    }"#;

    let (content, status) = SettingsContent::parse_json(json);
    assert_eq!(status, settings::ParseStatus::Success);

    let content = content.unwrap();
    let agent_content = content.agent.as_ref().unwrap();
    let profiles_content = agent_content.profiles.as_ref().unwrap();

    let custom_profile_content = profiles_content.get("custom").unwrap();
    let profile_settings =
        agent_settings::AgentProfileSettings::from(custom_profile_content.clone());

    let profile_model = profile_settings.default_model.unwrap();
    assert_eq!(profile_model.provider.0, "FallbackProvider");
    assert_eq!(profile_model.model, "test-model-4");
}
