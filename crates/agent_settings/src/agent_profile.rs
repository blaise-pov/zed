use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use collections::IndexMap;
use convert_case::{Case, Casing as _};
use fs::Fs;
use gpui::{App, SharedString};
use settings::{
    AgentProfileContent, ContextServerPresetContent, DelegationContent, LanguageModelSelection,
    Settings as _, SettingsContent, SettingsLocation, SettingsStore, update_settings_file,
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
        let enable_all_context_servers = false;
        let context_servers = base_profile
            .as_ref()
            .map(|profile| profile.context_servers.clone())
            .unwrap_or_default();
        // Preserve the base profile's model preference when cloning into a new profile.
        let default_model = base_profile
            .as_ref()
            .and_then(|profile| profile.default_model.clone());
        let custom_prompt_path = base_profile
            .as_ref()
            .and_then(|profile| profile.custom_prompt_path.clone());
        let system_prompt_template = base_profile
            .as_ref()
            .and_then(|profile| profile.system_prompt_template.clone());
        let description = base_profile
            .as_ref()
            .and_then(|profile| profile.description.clone());
        let skills = base_profile
            .as_ref()
            .and_then(|profile| profile.skills.clone())
            .or_else(|| Some(Vec::new()));
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
            custom_prompt_path,
            system_prompt_template,
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

    /// Returns a map of AgentProfileIds to their names for the given settings location (falling back to global).
    pub fn available_profiles(location: Option<SettingsLocation>, cx: &App) -> AvailableProfiles {
        let mut profiles = AvailableProfiles::default();
        for (id, profile) in AgentSettings::get(location, cx).profiles.iter() {
            profiles.insert(id.clone(), profile.name.clone());
        }
        profiles
    }

    /// Returns a map of AgentProfileIds to their names across all visible worktrees of the project.
    pub fn available_profiles_for_project(
        project: &project::Project,
        cx: &App,
    ) -> AvailableProfiles {
        let mut profiles = AvailableProfiles::default();
        let worktrees: Vec<_> = project.visible_worktrees(cx).collect();
        if worktrees.is_empty() {
            return Self::available_profiles(None, cx);
        }
        for worktree in worktrees {
            let location = settings::SettingsLocation {
                worktree_id: worktree.read(cx).id(),
                path: util::rel_path::RelPath::empty(),
            };
            for (id, name) in Self::available_profiles(Some(location), cx) {
                profiles.entry(id).or_insert(name);
            }
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
    /// Path to a file containing custom system prompt instructions for this profile.
    pub custom_prompt_path: Option<SharedString>,
    /// Path to a custom base Handlebars/Markdown template file for this profile.
    pub system_prompt_template: Option<SharedString>,
    /// What this profile is for; shown to the parent agent in the delegation
    /// catalog.
    pub description: Option<SharedString>,
    /// When set, only the listed skills are visible to sessions using this
    /// profile. When unset, built-in profiles see all skills while custom
    /// profiles see none.
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

    pub fn is_skill_allowed(&self, profile_id: &AgentProfileId, skill_name: &str) -> bool {
        if let Some(skills) = &self.skills {
            skills.iter().any(|s| s.as_ref() == skill_name)
        } else {
            builtin_profiles::is_builtin(profile_id)
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
                custom_prompt_path: self.custom_prompt_path.clone().map(|s| s.into()),
                system_prompt_template: self.system_prompt_template.clone().map(|s| s.into()),
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

#[derive(Debug, PartialEq, Eq)]
pub enum PromptResolveError {
    EmptyPath,
    ReadFailed { path: PathBuf, error: String },
    NotFound { attempted_paths: Vec<PathBuf> },
}

impl std::fmt::Display for PromptResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPath => write!(f, "prompt path is empty"),
            Self::ReadFailed { path, error } => {
                write!(
                    f,
                    "failed to read prompt file at {}: {error}",
                    path.display()
                )
            }
            Self::NotFound { attempted_paths } => {
                let candidates = attempted_paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "prompt file not found. Checked candidate paths: [{candidates}]"
                )
            }
        }
    }
}

impl std::error::Error for PromptResolveError {}

pub fn prompt_search_roots(worktree_root: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(worktree) = worktree_root {
        roots.push(worktree.to_path_buf());
    }
    roots.push(paths::config_dir().to_path_buf());
    roots
}

pub fn resolve_prompt(path_str: &str, anchors: &[PathBuf]) -> Result<String, PromptResolveError> {
    let trimmed = path_str.trim();
    if trimmed.is_empty() {
        return Err(PromptResolveError::EmptyPath);
    }

    let path = if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
        paths::home_dir().join(rest)
    } else if trimmed == "~" {
        paths::home_dir().to_path_buf()
    } else {
        PathBuf::from(trimmed)
    };

    if path.is_absolute() {
        if path.is_file() {
            return std::fs::read_to_string(&path).map_err(|err| PromptResolveError::ReadFailed {
                path: path.clone(),
                error: err.to_string(),
            });
        } else {
            return Err(PromptResolveError::NotFound {
                attempted_paths: vec![path],
            });
        }
    }

    let mut attempted_paths = Vec::with_capacity(anchors.len());
    for anchor in anchors {
        let candidate = anchor.join(&path);
        if candidate.is_file() {
            return std::fs::read_to_string(&candidate).map_err(|err| {
                PromptResolveError::ReadFailed {
                    path: candidate,
                    error: err.to_string(),
                }
            });
        }
        attempted_paths.push(candidate);
    }

    Err(PromptResolveError::NotFound { attempted_paths })
}

/// Reads prompt content from a file path.
/// Handles absolute paths, `~` home directory prefix, and relative paths
/// resolved against worktree root and Zed config directory.
pub fn read_prompt_file(path_str: &str, worktree_root: Option<&Path>) -> Option<String> {
    let anchors = prompt_search_roots(worktree_root);
    match resolve_prompt(path_str, &anchors) {
        Ok(content) => Some(content),
        Err(err) => {
            log::warn!("failed to read custom prompt from {path_str}: {err}");
            None
        }
    }
}

/// Reads prompt content from a file path quietly without logging warnings if not found.
pub fn try_read_prompt_file(path_str: &str, worktree_root: Option<&Path>) -> Option<String> {
    let anchors = prompt_search_roots(worktree_root);
    resolve_prompt(path_str, &anchors).ok()
}

pub fn is_safe_profile_id(id: &str) -> bool {
    !id.is_empty() && !id.contains('/') && !id.contains('\\') && !id.contains("..")
}

/// Resolves the custom prompt text from a custom prompt path, or falls back to
/// `.zed/prompts/{profile_id}.md` or `{config_dir}/prompts/{profile_id}.md`.
pub fn resolve_custom_prompt(
    profile_id: Option<&AgentProfileId>,
    custom_prompt_path: Option<&str>,
    worktree_root: Option<&Path>,
) -> Option<SharedString> {
    if let Some(path_str) = custom_prompt_path {
        if let Some(file_content) = read_prompt_file(path_str, worktree_root) {
            return Some(file_content.into());
        }
        return None;
    }

    if let Some(profile_id) = profile_id {
        let id_str = profile_id.as_str();
        if is_safe_profile_id(id_str) {
            let conventional_rel_path = format!(".zed/prompts/{id_str}.md");
            if let Some(file_content) = try_read_prompt_file(&conventional_rel_path, worktree_root)
            {
                return Some(file_content.into());
            }

            let config_prompt = paths::config_dir()
                .join("prompts")
                .join(format!("{id_str}.md"));
            if config_prompt.is_file() {
                if let Ok(content) = std::fs::read_to_string(&config_prompt) {
                    return Some(content.into());
                }
            }
        }
    }

    None
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
            custom_prompt_path,
            system_prompt_template,
            description,
            skills,
            delegation,
            tool_permissions,
        } = content;

        let custom_prompt_path_shared = custom_prompt_path
            .as_ref()
            .map(|p| SharedString::from(p.to_string()));

        Self {
            name: name.into(),
            origin,
            tools,
            enable_all_context_servers: enable_all_context_servers.unwrap_or_default(),
            context_servers: context_servers
                .into_iter()
                .map(|(server_id, preset)| (server_id, preset.into()))
                .collect(),
            default_model: default_model.map(crate::expand_model_selection),
            custom_prompt_path: custom_prompt_path_shared,
            system_prompt_template: system_prompt_template.map(|s| s.to_string().into()),
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
            custom_prompt_path: None,
            system_prompt_template: None,
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
    fn test_resolve_custom_prompt_file_and_fallback() {
        let temp_dir = std::env::temp_dir().join(format!(
            "zed_test_profile_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let prompt_file = temp_dir.join("prompt.md");
        std::fs::write(&prompt_file, "Prompt content from file").unwrap();

        let prompt_path_str = prompt_file.to_str().unwrap();

        // 1. Explicit path specified -> file is read
        let resolved = resolve_custom_prompt(None, Some(prompt_path_str), None);
        assert_eq!(resolved.as_deref(), Some("Prompt content from file"));

        // 2. Explicit path specified but doesn't exist -> None
        let non_existent = temp_dir.join("non_existent.md");
        let non_existent_str = non_existent.to_str().unwrap();
        let resolved = resolve_custom_prompt(None, Some(non_existent_str), None);
        assert_eq!(resolved, None);

        // 3. Fallback to .zed/prompts/{profile_id}.md in worktree
        let zed_prompts_dir = temp_dir.join(".zed").join("prompts");
        std::fs::create_dir_all(&zed_prompts_dir).unwrap();
        std::fs::write(
            zed_prompts_dir.join("my-profile.md"),
            "Prompt from conventional file",
        )
        .unwrap();

        let profile_id = AgentProfileId("my-profile".into());
        let resolved = resolve_custom_prompt(Some(&profile_id), None, Some(&temp_dir));
        assert_eq!(resolved.as_deref(), Some("Prompt from conventional file"));

        // 4. Fallback missing -> None quietly
        let missing_profile_id = AgentProfileId("missing-profile".into());
        let resolved = resolve_custom_prompt(Some(&missing_profile_id), None, Some(&temp_dir));
        assert_eq!(resolved, None);

        // 5. Unsafe profile id -> rejected quietly
        let unsafe_profile_id = AgentProfileId("../escaped".into());
        let resolved = resolve_custom_prompt(Some(&unsafe_profile_id), None, Some(&temp_dir));
        assert_eq!(resolved, None);

        std::fs::remove_dir_all(&temp_dir).log_err();
    }

    #[test]
    fn test_resolve_prompt_absolute_path() {
        let temp_dir = std::env::temp_dir().join(format!(
            "zed_test_prompt_abs_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let prompt_file = temp_dir.join("prompt.md");
        std::fs::write(&prompt_file, "custom absolute prompt").unwrap();

        let anchors = vec![];
        let result = resolve_prompt(prompt_file.to_str().unwrap(), &anchors);
        assert_eq!(result, Ok("custom absolute prompt".to_string()));

        std::fs::remove_dir_all(&temp_dir).log_err();
    }

    #[test]
    fn test_resolve_prompt_relative_path_from_worktree() {
        let worktree_dir = std::env::temp_dir().join(format!(
            "zed_test_prompt_wt_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let config_dir = std::env::temp_dir().join(format!(
            "zed_test_prompt_cfg_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let prompt_subdir = worktree_dir.join(".zed").join("prompts");
        std::fs::create_dir_all(&prompt_subdir).unwrap();
        std::fs::create_dir_all(&config_dir).unwrap();

        let prompt_file = prompt_subdir.join("custom.md");
        std::fs::write(&prompt_file, "worktree prompt content").unwrap();

        let anchors = vec![worktree_dir.clone(), config_dir.clone()];
        let result = resolve_prompt(".zed/prompts/custom.md", &anchors);
        assert_eq!(result, Ok("worktree prompt content".to_string()));

        std::fs::remove_dir_all(&worktree_dir).log_err();
        std::fs::remove_dir_all(&config_dir).log_err();
    }

    #[test]
    fn test_resolve_prompt_fallback_to_config_dir() {
        let worktree_dir = std::env::temp_dir().join(format!(
            "zed_test_prompt_wt_fallback_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let config_dir = std::env::temp_dir().join(format!(
            "zed_test_prompt_cfg_fallback_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&worktree_dir).unwrap();
        let prompt_subdir = config_dir.join("prompts");
        std::fs::create_dir_all(&prompt_subdir).unwrap();

        let prompt_file = prompt_subdir.join("global.md");
        std::fs::write(&prompt_file, "global prompt content").unwrap();

        let anchors = vec![worktree_dir.clone(), config_dir.clone()];
        let result = resolve_prompt("prompts/global.md", &anchors);
        assert_eq!(result, Ok("global prompt content".to_string()));

        std::fs::remove_dir_all(&worktree_dir).log_err();
        std::fs::remove_dir_all(&config_dir).log_err();
    }

    #[test]
    fn test_resolve_prompt_not_found_lists_candidates() {
        let worktree_dir = std::env::temp_dir().join(format!(
            "zed_test_prompt_wt_nf_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let config_dir = std::env::temp_dir().join(format!(
            "zed_test_prompt_cfg_nf_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&worktree_dir).unwrap();
        std::fs::create_dir_all(&config_dir).unwrap();

        let anchors = vec![worktree_dir.clone(), config_dir.clone()];
        let result = resolve_prompt("missing/prompt.md", &anchors);

        let expected_candidates = vec![
            worktree_dir.join("missing/prompt.md"),
            config_dir.join("missing/prompt.md"),
        ];

        match result {
            Err(PromptResolveError::NotFound { attempted_paths }) => {
                assert_eq!(attempted_paths, expected_candidates);
            }
            other => panic!("expected NotFound error, got: {:?}", other),
        }

        std::fs::remove_dir_all(&worktree_dir).log_err();
        std::fs::remove_dir_all(&config_dir).log_err();
    }

    #[test]
    fn test_resolve_prompt_empty_path() {
        let anchors = vec![std::env::temp_dir()];
        assert_eq!(
            resolve_prompt("", &anchors),
            Err(PromptResolveError::EmptyPath)
        );
        assert_eq!(
            resolve_prompt("   ", &anchors),
            Err(PromptResolveError::EmptyPath)
        );
        assert_eq!(
            resolve_prompt("\t\n", &anchors),
            Err(PromptResolveError::EmptyPath)
        );
    }

    #[test]
    fn test_prompt_search_roots() {
        let wt = Path::new("/some/worktree");
        let roots_with_wt = prompt_search_roots(Some(wt));
        assert_eq!(
            roots_with_wt,
            vec![wt.to_path_buf(), paths::config_dir().to_path_buf()]
        );

        let roots_without_wt = prompt_search_roots(None);
        assert_eq!(roots_without_wt, vec![paths::config_dir().to_path_buf()]);
    }

    #[test]
    fn test_agent_profile_content_from_json_with_prompt_path() {
        let json = r#"{
            "name": "Custom Agent",
            "custom_prompt_path": "nonexistent_file.md"
        }"#;

        let content: AgentProfileContent = serde_json::from_str(json).unwrap();
        assert_eq!(content.name.as_ref(), "Custom Agent");
        assert_eq!(
            content.custom_prompt_path.as_deref(),
            Some("nonexistent_file.md")
        );
    }

    #[test]
    fn test_agent_profile_content_aliases() {
        let json1 = r#"{
            "name": "Alias Agent 1",
            "custom_prompt_file": "path1.md"
        }"#;
        let content1: AgentProfileContent = serde_json::from_str(json1).unwrap();
        assert_eq!(content1.custom_prompt_path.as_deref(), Some("path1.md"));

        let json2 = r#"{
            "name": "Alias Agent 2",
            "prompt_path": "path2.md"
        }"#;
        let content2: AgentProfileContent = serde_json::from_str(json2).unwrap();
        assert_eq!(content2.custom_prompt_path.as_deref(), Some("path2.md"));
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

        let root: Arc<util::rel_path::RelPath> =
            std::sync::Arc::from(util::rel_path::RelPath::from_unix_str("root").unwrap());
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_local_settings(
                    WorktreeId::from_usize(1),
                    LocalSettingsPath::InWorktree(root.clone()),
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

        let location = Some(settings::SettingsLocation {
            worktree_id: WorktreeId::from_usize(1),
            path: &root,
        });
        let profiles = AgentProfile::available_profiles(location, cx);
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

        let settings = AgentSettings::get(location, cx);
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
            project::project_settings::ProjectSettings::get(location, cx)
                .context_servers
                .contains_key("demo")
        );

        // Global settings remain unpolluted:
        let global_settings = AgentSettings::get_global(cx);
        assert_eq!(
            global_settings.default_profile,
            AgentProfileId("write".into())
        );
        assert_eq!(
            global_settings
                .profiles
                .get(&AgentProfileId("orchestrator".into()))
                .unwrap()
                .name,
            "User Orchestrator"
        );
        assert!(
            !global_settings
                .profiles
                .contains_key(&AgentProfileId("backend".into()))
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
    fn test_project_local_agent_settings_do_not_override_layout_keys(cx: &mut gpui::App) {
        use gpui::UpdateGlobal as _;
        use settings::{DockPosition, LocalSettingsKind, LocalSettingsPath, WorktreeId};

        let store = SettingsStore::test(cx);
        cx.set_global(store);
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);

        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(
                    r#"{
                        "agent": {
                            "dock": "right",
                            "task_dock": "right",
                            "flexible": false
                        }
                    }"#,
                    cx,
                )
                .unwrap();
        });

        let initial_settings = AgentSettings::get_global(cx);
        assert_eq!(initial_settings.dock, DockPosition::Right);
        assert_eq!(initial_settings.task_dock, DockPosition::Right);
        assert_eq!(initial_settings.flexible, false);

        let root: Arc<util::rel_path::RelPath> =
            std::sync::Arc::from(util::rel_path::RelPath::from_unix_str("root").unwrap());
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_local_settings(
                    WorktreeId::from_usize(1),
                    LocalSettingsPath::InWorktree(root.clone()),
                    LocalSettingsKind::Settings,
                    Some(
                        r#"{
                            "agent": {
                                "dock": "left",
                                "task_dock": "left",
                                "flexible": true,
                                "default_profile": "project-profile"
                            }
                        }"#,
                    ),
                    cx,
                )
                .unwrap();
        });

        let location = Some(settings::SettingsLocation {
            worktree_id: WorktreeId::from_usize(1),
            path: &root,
        });
        let updated_settings = AgentSettings::get(location, cx);
        assert_eq!(
            updated_settings.default_profile,
            AgentProfileId("project-profile".into())
        );
        assert_eq!(updated_settings.dock, DockPosition::Right);
        assert_eq!(updated_settings.task_dock, DockPosition::Right);
        assert_eq!(updated_settings.flexible, false);
    }

    #[gpui::test]
    fn test_active_profile_agent_settings_survive_local_settings_update(cx: &mut gpui::App) {
        use gpui::UpdateGlobal as _;
        use settings::{
            ActiveSettingsProfileName, LocalSettingsKind, LocalSettingsPath, WorktreeId,
        };

        let store = SettingsStore::test(cx);
        cx.set_global(store);
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);

        // Settings profiles are activated at runtime via the
        // `ActiveSettingsProfileName` global (set by the profile selector),
        // not via a user-settings key, so seed the global before parsing.
        cx.set_global(ActiveSettingsProfileName("work".to_string()));

        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(
                    r#"{
                        "agent": {
                            "default_profile": "base-profile"
                        },
                        "profiles": {
                            "work": {
                                "base": "user",
                                "settings": {
                                    "agent": {
                                        "default_profile": "profile-from-settings-profile"
                                    }
                                }
                            }
                        }
                    }"#,
                    cx,
                )
                .unwrap();
        });

        assert_eq!(
            AgentSettings::get_global(cx).default_profile,
            AgentProfileId("profile-from-settings-profile".into())
        );

        let root = std::sync::Arc::from(util::rel_path::RelPath::from_unix_str("root").unwrap());
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_local_settings(
                    WorktreeId::from_usize(1),
                    LocalSettingsPath::InWorktree(root),
                    LocalSettingsKind::Settings,
                    Some(
                        r#"{
                            "languages": {
                                "Rust": { "tab_size": 4 }
                            }
                        }"#,
                    ),
                    cx,
                )
                .unwrap();
        });

        assert_eq!(
            AgentSettings::get_global(cx).default_profile,
            AgentProfileId("profile-from-settings-profile".into())
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

    #[gpui::test]
    fn test_multiple_projects_local_agent_settings_isolation(cx: &mut gpui::App) {
        use gpui::UpdateGlobal as _;
        use settings::{LocalSettingsKind, LocalSettingsPath, SettingsLocation, WorktreeId};

        let store = SettingsStore::test(cx);
        cx.set_global(store);
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);

        // Global user settings:
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(
                    r#"{ "agent": { "default_profile": "user-default", "profiles": { "global_prof": { "name": "Global" } } } }"#,
                    cx,
                )
                .unwrap();
        });

        // Project 1 settings (worktree 1):
        let root: Arc<util::rel_path::RelPath> =
            std::sync::Arc::from(util::rel_path::RelPath::from_unix_str("").unwrap());
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_local_settings(
                    WorktreeId::from_usize(1),
                    LocalSettingsPath::InWorktree(root.clone()),
                    LocalSettingsKind::Settings,
                    Some(
                        r#"{
                            "agent": {
                                "default_profile": "project1-profile",
                                "profiles": {
                                    "project1-profile": { "name": "Project 1 Profile" }
                                }
                            }
                        }"#,
                    ),
                    cx,
                )
                .unwrap();
        });

        // Project 2 settings (worktree 2) - has NO agent settings:
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_local_settings(
                    WorktreeId::from_usize(2),
                    LocalSettingsPath::InWorktree(root),
                    LocalSettingsKind::Settings,
                    Some(r#"{ "languages": { "Rust": { "tab_size": 2 } } }"#),
                    cx,
                )
                .unwrap();
        });

        let loc1 = Some(SettingsLocation {
            worktree_id: WorktreeId::from_usize(1),
            path: &util::rel_path::RelPath::empty(),
        });
        let loc2 = Some(SettingsLocation {
            worktree_id: WorktreeId::from_usize(2),
            path: &util::rel_path::RelPath::empty(),
        });

        // Project 1 sees its own settings:
        let s1 = AgentSettings::get(loc1, cx);
        assert_eq!(
            s1.default_profile,
            AgentProfileId("project1-profile".into())
        );
        assert!(
            s1.profiles
                .contains_key(&AgentProfileId("project1-profile".into()))
        );

        // Project 2 DOES NOT see Project 1's settings! It gets user settings:
        let s2 = AgentSettings::get(loc2, cx);
        assert_eq!(s2.default_profile, AgentProfileId("user-default".into()));
        assert!(
            !s2.profiles
                .contains_key(&AgentProfileId("project1-profile".into()))
        );

        // Global settings are clean:
        let global = AgentSettings::get_global(cx);
        assert_eq!(
            global.default_profile,
            AgentProfileId("user-default".into())
        );
        assert!(
            !global
                .profiles
                .contains_key(&AgentProfileId("project1-profile".into()))
        );
    }
}
