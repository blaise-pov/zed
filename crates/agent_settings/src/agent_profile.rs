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

use crate::{AgentProfileId, AgentSettings};

pub mod builtin_profiles {
    use super::AgentProfileId;

    pub const WRITE: &str = "write";
    pub const ASK: &str = "ask";
    pub const MINIMAL: &str = "minimal";

    pub fn is_builtin(profile_id: &AgentProfileId) -> bool {
        profile_id.as_str() == WRITE || profile_id.as_str() == ASK || profile_id.as_str() == MINIMAL
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

        let profile_settings = AgentProfileSettings {
            name: name.into(),
            tools,
            enable_all_context_servers,
            context_servers,
            default_model,
            custom_prompt,
            description: None,
            skills: None,
            delegation: None,
        };

        update_settings_file(fs, cx, {
            let id = id.clone();
            move |settings, _cx| {
                profile_settings.save_to_settings(id, settings).log_err();
            }
        });

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
#[derive(Debug, Clone)]
pub struct AgentProfileSettings {
    /// The name of the profile.
    pub name: SharedString,
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
            allowed: content
                .allowed
                .into_iter()
                .map(AgentProfileId)
                .collect(),
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
        self.context_servers
            .get(server_id)
            .and_then(|preset| preset.tools.get(tool_name).copied())
            .unwrap_or(self.enable_all_context_servers)
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
                tools: self.tools.clone(),
                enable_all_context_servers: Some(self.enable_all_context_servers),
                context_servers: self
                    .context_servers
                    .clone()
                    .into_iter()
                    .map(|(server_id, preset)| {
                        (
                            server_id,
                            ContextServerPresetContent {
                                tools: preset.tools,
                            },
                        )
                    })
                    .collect(),
                default_model: self.default_model.clone(),
                custom_prompt: self.custom_prompt.clone().map(|s| s.into()),
                description: self.description.clone().map(|s| s.into()),
                skills: self.skills.clone(),
                delegation: self.delegation.as_ref().map(|delegation| DelegationContent {
                    allowed: delegation
                        .allowed
                        .iter()
                        .map(|id| Arc::from(id.as_str()))
                        .collect(),
                    max_depth: Some(u32::from(delegation.max_depth)),
                }),
            },
        );

        Ok(())
    }
}

impl From<AgentProfileContent> for AgentProfileSettings {
    fn from(content: AgentProfileContent) -> Self {
        let AgentProfileContent {
            name,
            tools,
            enable_all_context_servers,
            context_servers,
            default_model,
            custom_prompt,
            description,
            skills,
            delegation,
        } = content;

        Self {
            name: name.into(),
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
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ContextServerPreset {
    pub tools: IndexMap<Arc<str>, bool>,
}

impl From<settings::ContextServerPresetContent> for ContextServerPreset {
    fn from(content: settings::ContextServerPresetContent) -> Self {
        Self {
            tools: content.tools,
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
            tools: IndexMap::default(),
            enable_all_context_servers,
            context_servers,
            default_model: None,
            custom_prompt: None,
            description: None,
            skills: None,
            delegation: None,
        }
    }

    fn preset(tools: &[(&str, bool)]) -> ContextServerPreset {
        ContextServerPreset {
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
}
