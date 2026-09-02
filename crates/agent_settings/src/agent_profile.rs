use std::sync::Arc;

use anyhow::{Result, bail};
use collections::IndexMap;
use convert_case::{Case, Casing as _};
use fs::Fs;
use gpui::{App, SharedString};
use settings::{
    AgentProfileContent, ContextServerPresetContent, DelegationContent, LanguageModelSelection,
    Settings as _, SettingsContent, SettingsStore, update_settings_file,
};
use util::ResultExt as _;

use crate::{AgentProfileId, AgentSettings, ToolPermissions, compile_tool_permissions};

pub mod builtin_profiles {
    use super::AgentProfileId;

    pub const WRITE: &str = "write";
    pub const ASK: &str = "ask";
    pub const MINIMAL: &str = "minimal";

    pub fn is_builtin(profile_id: &AgentProfileId) -> bool {
        profile_id.as_str() == WRITE || profile_id.as_str() == ASK || profile_id.as_str() == MINIMAL
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ProfileOrigin {
    #[default]
    Global,
    Project {
        worktree_id: settings::WorktreeId,
        path: Arc<util::rel_path::RelPath>,
    },
}

impl From<settings::ProfileOriginContent> for ProfileOrigin {
    fn from(content: settings::ProfileOriginContent) -> Self {
        match content {
            settings::ProfileOriginContent::Global => Self::Global,
            settings::ProfileOriginContent::Project { worktree_id, path } => {
                Self::Project { worktree_id, path }
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentProfile {
    id: AgentProfileId,
}

pub type AvailableProfiles = IndexMap<AgentProfileId, SharedString>;

impl AgentProfile {
    pub fn new(id: AgentProfileId) -> Self {
        Self { id }
    }

    pub fn id(&self) -> &AgentProfileId {
        &self.id
    }

    /// Saves a new profile to the settings.
    pub fn create(
        name: String,
        base_profile_id: Option<AgentProfileId>,
        origin: ProfileOrigin,
        fs: Arc<dyn Fs>,
        cx: &App,
    ) -> AgentProfileId {
        let id = AgentProfileId(name.to_case(Case::Kebab).into());

        let base_profile =
            base_profile_id.and_then(|id| AgentSettings::get_global(cx).profiles.get(&id).cloned());

        // Copy toggles from the base profile so the new profile starts with familiar defaults.
        let tools = base_profile
            .as_ref()
            .map(|profile| profile.tools.clone())
            .unwrap_or_default();
        let enable_all_context_servers = base_profile
            .as_ref()
            .map(|profile| profile.enable_all_context_servers)
            .unwrap_or_default();
        let context_servers = base_profile
            .as_ref()
            .map(|profile| profile.context_servers.clone())
            .unwrap_or_default();
        // Preserve the base profile's model preference when cloning into a new profile.
        let default_model = base_profile
            .as_ref()
            .and_then(|profile| profile.default_model.clone());
        // Preserve the base profile's custom prompt when cloning into a new profile.
        let custom_prompt = base_profile
            .as_ref()
            .and_then(|profile| profile.custom_prompt.clone());
        let description = base_profile
            .as_ref()
            .and_then(|profile| profile.description.clone());
        let skills = base_profile
            .as_ref()
            .and_then(|profile| profile.skills.clone());
        let delegation = base_profile
            .as_ref()
            .and_then(|profile| profile.delegation.clone());
        let tool_permissions = base_profile
            .as_ref()
            .and_then(|profile| profile.tool_permissions.clone());

        let profile_settings = AgentProfileSettings {
            name: name.into(),
            origin: origin.clone(),
            tools,
            enable_all_context_servers,
            context_servers,
            default_model,
            custom_prompt,
            description,
            skills,
            delegation,
            tool_permissions,
        };

        match &origin {
            ProfileOrigin::Global => {
                update_settings_file(fs, cx, {
                    let id = id.clone();
                    move |settings, _cx| {
                        profile_settings.save_to_settings(id, settings).log_err();
                    }
                });
            }
            ProfileOrigin::Project { worktree_id, path } => {
                settings::update_project_settings_file(fs, *worktree_id, path.clone(), cx, {
                    let id = id.clone();
                    move |settings, _cx| {
                        profile_settings.save_to_settings(id, settings).log_err();
                    }
                });
            }
        }

        id
    }

    /// Returns a map of AgentProfileIds to their names
    pub fn available_profiles(cx: &App) -> AvailableProfiles {
        let mut profiles = AvailableProfiles::default();
        for (id, profile) in AgentSettings::get_global(cx).profiles.iter() {
            profiles.insert(id.clone(), profile.name.clone());
        }
        profiles
    }
}

/// A profile for the Zed Agent that controls its behavior.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentProfileSettings {
    /// The name of the profile.
    pub name: SharedString,
    pub origin: ProfileOrigin,
    pub tools: IndexMap<Arc<str>, bool>,
    pub enable_all_context_servers: bool,
    pub context_servers: IndexMap<Arc<str>, ContextServerPreset>,
    /// Default language model to apply when this profile becomes active.
    pub default_model: Option<LanguageModelSelection>,
    /// Custom system prompt instructions for this profile.
    pub custom_prompt: Option<SharedString>,
    /// What this profile is for; shown to the parent agent in the delegation
    /// catalog.
    pub description: Option<SharedString>,
    /// When set, only the listed skills are visible to sessions using this
    /// profile.
    pub skills: Option<Vec<Arc<str>>>,
    /// When present, this profile may delegate via `spawn_agent`; a profile
    /// without it is a solo agent.
    pub delegation: Option<Delegation>,
    /// Tool permissions and write scopes for this profile.
    pub tool_permissions: Option<ToolPermissions>,
}

/// Which sub-agents a profile may spawn, and how deeply they may nest.
#[derive(Debug, Clone, PartialEq)]
pub struct Delegation {
    pub allowed: Vec<AgentProfileId>,
    /// Maximum delegation levels below an agent running this profile.
    /// Clamped to [1, 5]; default 1.
    pub max_depth: u8,
}

impl Default for Delegation {
    fn default() -> Self {
        Self {
            allowed: Vec::new(),
            max_depth: 1,
        }
    }
}

impl From<DelegationContent> for Delegation {
    fn from(content: DelegationContent) -> Self {
        let default = Self::default();
        Self {
            allowed: content.allowed.into_iter().map(AgentProfileId).collect(),
            max_depth: content
                .max_depth
                .map(|depth| depth.clamp(1, 5) as u8)
                .unwrap_or(default.max_depth),
        }
    }
}

impl AgentProfileSettings {
    pub fn is_tool_enabled(&self, tool_name: &str) -> bool {
        self.tools.get(tool_name) == Some(&true)
    }

    /// Whether the built-in profile with the given id still matches the shipped
    /// default — i.e. the user has neither customized the built-in profile nor
    /// shadowed it with a custom profile of the same id. Custom profile ids are
    /// never considered unmodified defaults.
    pub fn is_unmodified_default(profile_id: &AgentProfileId, cx: &App) -> bool {
        if !builtin_profiles::is_builtin(profile_id) {
            return false;
        }
        let store = cx.global::<SettingsStore>();
        let profile_in = |content: &SettingsContent| {
            content
                .agent
                .as_ref()
                .and_then(|agent| agent.profiles.as_ref())
                .and_then(|profiles| profiles.get(profile_id.as_str()))
                .cloned()
        };
        match (
            profile_in(store.merged_settings()),
            profile_in(store.raw_default_settings()),
        ) {
            (Some(merged), Some(default)) => merged == default,
            _ => false,
        }
    }

    pub fn is_context_server_tool_enabled(&self, server_id: &str, tool_name: &str) -> bool {
        match self.context_servers.get(server_id) {
            Some(preset) => match preset.enabled {
                Some(enabled) => enabled,
                None => preset
                    .tools
                    .get(tool_name)
                    .copied()
                    .unwrap_or(self.enable_all_context_servers),
            },
            None => self.enable_all_context_servers,
        }
    }

    pub fn save_to_settings(
        &self,
        profile_id: AgentProfileId,
        content: &mut SettingsContent,
    ) -> Result<()> {
        let profiles = content
            .agent
            .get_or_insert_default()
            .profiles
            .get_or_insert_default();
        if profiles.contains_key(&profile_id.0) {
            bail!("profile with ID '{profile_id}' already exists");
        }

        profiles.insert(
            profile_id.0,
            AgentProfileContent {
                name: self.name.clone().into(),
                origin: None,
                tools: self.tools.clone(),
                enable_all_context_servers: Some(self.enable_all_context_servers),
                context_servers: self
                    .context_servers
                    .clone()
                    .into_iter()
                    .map(|(server_id, preset)| {
                        (
                            server_id,
                            match preset.enabled {
                                Some(enabled) => ContextServerPresetContent::Enabled(enabled),
                                None => ContextServerPresetContent::Tools {
                                    tools: preset.tools,
                                },
                            },
                        )
                    })
                    .collect(),
                default_model: self.default_model.clone(),
                custom_prompt: self.custom_prompt.clone().map(|s| s.into()),
                description: self.description.clone().map(|s| s.into()),
                skills: self.skills.clone(),
                delegation: self
                    .delegation
                    .as_ref()
                    .map(|delegation| DelegationContent {
                        allowed: delegation
                            .allowed
                            .iter()
                            .map(|id| Arc::from(id.as_str()))
                            .collect(),
                        max_depth: Some(u32::from(delegation.max_depth)),
                    }),
                tool_permissions: self
                    .tool_permissions
                    .as_ref()
                    .map(|tool_permissions| tool_permissions.to_content()),
            },
        );

        Ok(())
    }
}

impl From<AgentProfileContent> for AgentProfileSettings {
    fn from(content: AgentProfileContent) -> Self {
        let origin = content.origin.map(Into::into).unwrap_or_default();
        let AgentProfileContent {
            name,
            origin: _,
            tools,
            enable_all_context_servers,
            context_servers,
            default_model,
            custom_prompt,
            description,
            skills,
            delegation,
            tool_permissions,
        } = content;

        Self {
            name: name.into(),
            origin,
            tools,
            enable_all_context_servers: enable_all_context_servers.unwrap_or_default(),
            context_servers: context_servers
                .into_iter()
                .map(|(server_id, preset)| (server_id, preset.into()))
                .collect(),
            default_model,
            custom_prompt: custom_prompt.map(|s| s.into()),
            description: description.map(|s| s.into()),
            skills,
            delegation: delegation.map(|delegation| delegation.into()),
            tool_permissions: tool_permissions.map(|tp| compile_tool_permissions(Some(tp))),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContextServerPreset {
    /// When set, enables or disables the entire server for this profile,
    /// overriding any per-tool toggles.
    pub enabled: Option<bool>,
    pub tools: IndexMap<Arc<str>, bool>,
}

impl From<settings::ContextServerPresetContent> for ContextServerPreset {
    fn from(content: settings::ContextServerPresetContent) -> Self {
        match content {
            settings::ContextServerPresetContent::Enabled(enabled) => Self {
                enabled: Some(enabled),
                tools: IndexMap::default(),
            },
            settings::ContextServerPresetContent::Tools { tools } => Self {
                enabled: None,
                tools,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(
        enable_all_context_servers: bool,
        context_servers: IndexMap<Arc<str>, ContextServerPreset>,
    ) -> AgentProfileSettings {
        AgentProfileSettings {
            name: "test".into(),
            origin: ProfileOrigin::Global,
            tools: IndexMap::default(),
            enable_all_context_servers,
            context_servers,
            default_model: None,
            custom_prompt: None,
            description: None,
            skills: None,
            delegation: None,
            tool_permissions: None,
        }
    }

    fn preset(tools: &[(&str, bool)]) -> ContextServerPreset {
        ContextServerPreset {
            enabled: None,
            tools: tools
                .iter()
                .map(|(name, enabled)| (Arc::from(*name), *enabled))
                .collect(),
        }
    }

    #[test]
    fn explicit_false_disables_tool_when_enable_all_is_true() {
        let mut servers = IndexMap::default();
        servers.insert(Arc::from("server"), preset(&[("disabled_tool", false)]));
        let profile = profile(true, servers);

        assert!(!profile.is_context_server_tool_enabled("server", "disabled_tool"));
        assert!(profile.is_context_server_tool_enabled("server", "other_tool"));
        assert!(profile.is_context_server_tool_enabled("other_server", "any_tool"));
    }

    #[test]
    fn explicit_true_enables_tool_when_enable_all_is_false() {
        let mut servers = IndexMap::default();
        servers.insert(Arc::from("server"), preset(&[("enabled_tool", true)]));
        let profile = profile(false, servers);

        assert!(profile.is_context_server_tool_enabled("server", "enabled_tool"));
        assert!(!profile.is_context_server_tool_enabled("server", "other_tool"));
        assert!(!profile.is_context_server_tool_enabled("other_server", "any_tool"));
    }

    #[gpui::test]
    fn unmodified_default_detection(cx: &mut gpui::App) {
        use gpui::UpdateGlobal as _;

        let store = SettingsStore::test(cx);
        cx.set_global(store);
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);

        let write = AgentProfileId(builtin_profiles::WRITE.into());
        let minimal = AgentProfileId(builtin_profiles::MINIMAL.into());
        let custom = AgentProfileId("custom".into());

        // Fresh defaults: the shipped built-in profiles are unmodified.
        assert!(AgentProfileSettings::is_unmodified_default(&write, cx));
        assert!(AgentProfileSettings::is_unmodified_default(&minimal, cx));
        // Custom (non-built-in) ids are never considered unmodified defaults.
        assert!(!AgentProfileSettings::is_unmodified_default(&custom, cx));

        // The user customizes the `write` profile; `minimal` stays untouched.
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(
                    r#"{ "agent": { "profiles": { "write": { "name": "Write", "tools": { "fetch": false } } } } }"#,
                    cx,
                )
                .unwrap();
        });

        assert!(!AgentProfileSettings::is_unmodified_default(&write, cx));
        assert!(AgentProfileSettings::is_unmodified_default(&minimal, cx));
    }

    #[gpui::test]
    fn project_local_agent_settings_merge_with_user_settings(cx: &mut gpui::App) {
        use gpui::UpdateGlobal as _;
        use settings::{LocalSettingsKind, LocalSettingsPath, WorktreeId};

        let store = SettingsStore::test(cx);
        cx.set_global(store);
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);

        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(
                    r#"{ "agent": { "default_profile": "write", "profiles": { "orchestrator": { "name": "User Orchestrator" } } } }"#,
                    cx,
                )
                .unwrap();
        });

        let root = std::sync::Arc::from(util::rel_path::RelPath::from_unix_str("root").unwrap());
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_local_settings(
                    WorktreeId::from_usize(1),
                    LocalSettingsPath::InWorktree(root),
                    LocalSettingsKind::Settings,
                    Some(
                        r#"{
                            "agent": {
                                "default_profile": "orchestrator",
                                "context_servers": {
                                    "demo": { "command": "npx", "args": ["-y", "demo-mcp"] }
                                },
                                "profiles": {
                                    "orchestrator": {
                                        "name": "Project Orchestrator",
                                        "context_servers": { "postgres": true, "docker": false }
                                    },
                                    "backend": { "name": "Backend" }
                                }
                            }
                        }"#,
                    ),
                    cx,
                )
                .unwrap();
        });

        let profiles = AgentProfile::available_profiles(cx);
        // The project's entry overrides the user's profile of the same id.
        assert_eq!(
            profiles.get(&AgentProfileId("orchestrator".into())),
            Some(&"Project Orchestrator".into())
        );
        // Profiles only present in the project file are available too.
        assert_eq!(
            profiles.get(&AgentProfileId("backend".into())),
            Some(&"Backend".into())
        );

        let settings = AgentSettings::get_global(cx);
        assert_eq!(
            settings.default_profile,
            AgentProfileId("orchestrator".into())
        );

        let profile = settings
            .profiles
            .get(&AgentProfileId("orchestrator".into()))
            .unwrap();
        // A boolean preset enables or disables the whole server.
        assert!(profile.is_context_server_tool_enabled("postgres", "any_tool"));
        assert!(!profile.is_context_server_tool_enabled("docker", "any_tool"));

        // Server definitions under `agent.context_servers` are surfaced
        // through the project context server settings.
        assert!(
            project::project_settings::ProjectSettings::get_global(cx)
                .context_servers
                .contains_key("demo")
        );

        // Check origins:
        let write_profile = settings
            .profiles
            .get(&AgentProfileId("write".into()))
            .unwrap();
        assert_eq!(write_profile.origin, ProfileOrigin::Global);

        let orchestrator_profile = settings
            .profiles
            .get(&AgentProfileId("orchestrator".into()))
            .unwrap();
        assert_eq!(
            orchestrator_profile.origin,
            ProfileOrigin::Project {
                worktree_id: WorktreeId::from_usize(1),
                path: std::sync::Arc::from(
                    util::rel_path::RelPath::from_unix_str("root/.zed/settings.json").unwrap()
                ),
            }
        );

        let backend_profile = settings
            .profiles
            .get(&AgentProfileId("backend".into()))
            .unwrap();
        assert_eq!(
            backend_profile.origin,
            ProfileOrigin::Project {
                worktree_id: WorktreeId::from_usize(1),
                path: std::sync::Arc::from(
                    util::rel_path::RelPath::from_unix_str("root/.zed/settings.json").unwrap()
                ),
            }
        );
    }

    #[gpui::test]
    fn test_create_project_profile_and_global_profile(cx: &mut gpui::App) {
        use fs::FakeFs;
        use gpui::UpdateGlobal as _;
        use settings::{LocalSettingsKind, LocalSettingsPath, WorktreeId};

        let fs = FakeFs::new(cx.background_executor().clone());
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);

        let root = std::sync::Arc::from(util::rel_path::RelPath::from_unix_str("").unwrap());
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_local_settings(
                    WorktreeId::from_usize(1),
                    LocalSettingsPath::InWorktree(root),
                    LocalSettingsKind::Settings,
                    Some(
                        r#"{
                            "agent": {
                                "profiles": {
                                    "existing_project_profile": { "name": "Existing" }
                                }
                            }
                        }"#,
                    ),
                    cx,
                )
                .unwrap();
        });

        // 1. Create global profile
        let global_id = AgentProfile::create(
            "Global Profile".to_string(),
            None,
            ProfileOrigin::Global,
            fs.clone(),
            cx,
        );
        assert_eq!(global_id, AgentProfileId("global-profile".into()));

        // 2. Create project profile
        let project_origin = ProfileOrigin::Project {
            worktree_id: WorktreeId::from_usize(1),
            path: std::sync::Arc::from(
                util::rel_path::RelPath::from_unix_str(".zed/settings.json").unwrap(),
            ),
        };
        let project_id =
            AgentProfile::create("Project Profile".to_string(), None, project_origin, fs, cx);
        assert_eq!(project_id, AgentProfileId("project-profile".into()));
    }

    #[test]
    fn test_profile_tool_permissions_deserialization_and_matching() {
        let json = serde_json::json!({
            "name": "Backend Engineer",
            "tool_permissions": {
                "default": "deny",
                "tools": {
                    "terminal": {
                        "default": "deny",
                        "always_allow": [
                            { "pattern": "^go\\s+(test|build|vet)" },
                            { "pattern": "^cargo\\s+(test|check)" }
                        ],
                        "always_deny": [
                            { "pattern": "^git\\s+push\\s+--force" },
                            { "pattern": "^dropdb" }
                        ]
                    },
                    "edit_file": {
                        "default": "allow",
                        "write_scopes": ["backend/**", "proto/**"]
                    },
                    "write_file": {
                        "default": "allow",
                        "write_scopes": ["backend/**", "proto/**"]
                    }
                }
            }
        });

        let content: AgentProfileContent = serde_json::from_value(json).unwrap();
        let settings = AgentProfileSettings::from(content);

        let perms = settings
            .tool_permissions
            .expect("tool_permissions should be parsed");
        assert_eq!(perms.default, settings::ToolPermissionMode::Deny);

        let terminal_rules = perms.tools.get("terminal").expect("terminal rules present");
        assert_eq!(
            terminal_rules.default,
            Some(settings::ToolPermissionMode::Deny)
        );
        assert!(
            terminal_rules
                .always_allow
                .iter()
                .any(|r| r.is_match("go test ./..."))
        );
        assert!(
            terminal_rules
                .always_allow
                .iter()
                .any(|r| r.is_match("cargo check"))
        );
        assert!(
            !terminal_rules
                .always_allow
                .iter()
                .any(|r| r.is_match("cargo run"))
        );
        assert!(
            terminal_rules
                .always_deny
                .iter()
                .any(|r| r.is_match("git push --force"))
        );
        assert!(
            terminal_rules
                .always_deny
                .iter()
                .any(|r| r.is_match("dropdb test"))
        );

        let edit_rules = perms
            .tools
            .get("edit_file")
            .expect("edit_file rules present");
        assert_eq!(
            edit_rules.default,
            Some(settings::ToolPermissionMode::Allow)
        );
        let write_scopes = edit_rules
            .write_scopes
            .as_ref()
            .expect("write_scopes present");
        assert!(
            write_scopes
                .is_match(util::rel_path::RelPath::new_test("backend/src/main.rs").as_ref())
        );
        assert!(
            write_scopes
                .is_match(util::rel_path::RelPath::new_test("proto/service.proto").as_ref())
        );
        assert!(
            !write_scopes
                .is_match(util::rel_path::RelPath::new_test("frontend/src/App.tsx").as_ref())
        );
        assert!(!write_scopes.is_match(util::rel_path::RelPath::new_test("README.md").as_ref()));
    }

    #[gpui::test]
    fn test_invalid_write_scopes_fail_closed() {
        let json = serde_json::json!({
            "name": "Backend Engineer",
            "tool_permissions": {
                "default": "deny",
                "tools": {
                    "edit_file": {
                        "default": "allow",
                        "write_scopes": ["backend/**", "[invalid"]
                    }
                }
            }
        });

        let content: AgentProfileContent = serde_json::from_value(json).unwrap();
        let settings = AgentProfileSettings::from(content);

        let perms = settings
            .tool_permissions
            .expect("tool_permissions should be parsed");
        let edit_rules = perms
            .tools
            .get("edit_file")
            .expect("edit_file rules present");

        // An invalid glob must not silently drop write restrictions: the
        // scopes are withheld and the tool is blocked via invalid_patterns,
        // mirroring how invalid regex rules fail closed.
        assert!(edit_rules.write_scopes.is_none());
        assert_eq!(edit_rules.invalid_patterns.len(), 1);
        assert_eq!(edit_rules.invalid_patterns[0].rule_type, "write_scopes");
        assert!(
            edit_rules.invalid_patterns[0]
                .pattern
                .contains("backend/**")
        );
    }
}
