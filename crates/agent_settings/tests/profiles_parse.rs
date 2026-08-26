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
