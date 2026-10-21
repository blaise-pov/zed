use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use agent::ContextServerRegistry;
use agent_settings::{AgentProfileId, AgentProfileSettings};
use fs::Fs;
use gpui::{App, Context, DismissEvent, Entity, EventEmitter, Focusable, Task, WeakEntity, Window};
use picker::{Picker, PickerDelegate};
use project::project_settings::ProjectSettings;
use settings::{
    AgentProfileContent, ContextServerPresetContent, DelegationContent, Settings as _,
    SettingsFile, SettingsStore, ToolPermissionMode, ToolPermissionsContent, ToolRulesContent,
    update_settings_file,
};
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;

pub struct ToolPicker {
    picker: Entity<Picker<ToolPickerDelegate>>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ToolPickerMode {
    BuiltinTools,
    McpTools,
}

impl ToolPicker {
    pub fn builtin_tools(
        delegate: ToolPickerDelegate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx).embedded());
        Self { picker }
    }

    pub fn mcp_tools(
        delegate: ToolPickerDelegate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picker = cx.new(|cx| Picker::list(delegate, window, cx).embedded());
        Self { picker }
    }
}

impl EventEmitter<DismissEvent> for ToolPicker {}

impl Focusable for ToolPicker {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for ToolPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().child(self.picker.clone())
    }
}

#[derive(Debug, Clone)]
pub enum PickerItem {
    Tool {
        server_id: Option<Arc<str>>,
        name: Arc<str>,
    },
    ContextServer {
        server_id: Arc<str>,
    },
}

pub struct ToolPickerDelegate {
    tool_picker: WeakEntity<ToolPicker>,
    fs: Arc<dyn Fs>,
    items: Arc<Vec<PickerItem>>,
    profile_id: AgentProfileId,
    profile_settings: AgentProfileSettings,
    filtered_items: Vec<PickerItem>,
    selected_index: usize,
    mode: ToolPickerMode,
    /// Servers defined in a project worktree's `.zed/settings.json` rather
    /// than in user settings, badged in the UI like project-origin profiles.
    project_server_ids: HashSet<Arc<str>>,
}

impl ToolPickerDelegate {
    pub fn builtin_tools(
        tool_names: Vec<Arc<str>>,
        fs: Arc<dyn Fs>,
        profile_id: AgentProfileId,
        profile_settings: AgentProfileSettings,
        cx: &mut Context<ToolPicker>,
    ) -> Self {
        Self::new(
            Arc::new(
                tool_names
                    .into_iter()
                    .map(|name| PickerItem::Tool {
                        name,
                        server_id: None,
                    })
                    .collect(),
            ),
            ToolPickerMode::BuiltinTools,
            fs,
            profile_id,
            profile_settings,
            cx,
        )
    }

    pub fn mcp_tools(
        registry: &Entity<ContextServerRegistry>,
        fs: Arc<dyn Fs>,
        profile_id: AgentProfileId,
        profile_settings: AgentProfileSettings,
        cx: &mut Context<ToolPicker>,
    ) -> Self {
        let mut items = Vec::new();
        let mut project_server_ids = HashSet::new();

        let registry_ref = registry.read(cx);
        let server_store = registry_ref.server_store();
        let global_servers = &ProjectSettings::get_global(cx).context_servers;
        let store = server_store.read(cx);
        let settings_store = cx.global::<SettingsStore>();
        for (_, _, local_project_settings) in settings_store.get_all_locals::<ProjectSettings>() {
            for server_id in local_project_settings.context_servers.keys() {
                project_server_ids.insert(server_id.clone());
            }
        }
        for file in settings_store.get_all_files() {
            if let SettingsFile::Project(ref key) = file {
                if let Some(content) =
                    settings_store.get_content_for_file(SettingsFile::Project(key.clone()))
                {
                    for server_id in content.project.context_servers.keys() {
                        project_server_ids.insert(server_id.clone());
                    }
                    if let Some(agent) = &content.agent {
                        if let Some(servers) = &agent.context_servers {
                            for server_id in servers.keys() {
                                project_server_ids.insert(Arc::from(server_id.as_ref()));
                            }
                        }
                    }
                }
            }
        }
        for (id, tools) in registry_ref.servers() {
            let server_id = id.clone().0;
            // A custom server absent from the user settings must have been
            // contributed by a project-local settings file; extension-provided
            // servers are excluded since they are configured elsewhere.
            if !store.is_extension_provided(id, cx)
                && !global_servers.contains_key(server_id.as_ref())
            {
                project_server_ids.insert(server_id.clone());
            }
            items.push(PickerItem::ContextServer {
                server_id: server_id.clone(),
            });
            items.extend(tools.keys().map(|tool_name| PickerItem::Tool {
                name: tool_name.clone().into(),
                server_id: Some(server_id.clone()),
            }));
        }

        Self::new(
            Arc::new(items),
            ToolPickerMode::McpTools,
            fs,
            profile_id,
            profile_settings,
            cx,
        )
        .with_project_server_ids(project_server_ids)
    }

    fn new(
        items: Arc<Vec<PickerItem>>,
        mode: ToolPickerMode,
        fs: Arc<dyn Fs>,
        profile_id: AgentProfileId,
        profile_settings: AgentProfileSettings,
        cx: &mut Context<ToolPicker>,
    ) -> Self {
        Self {
            tool_picker: cx.entity().downgrade(),
            mode,
            fs,
            items,
            profile_id,
            profile_settings,
            filtered_items: Vec::new(),
            selected_index: 0,
            project_server_ids: HashSet::new(),
        }
    }

    fn with_project_server_ids(mut self, project_server_ids: HashSet<Arc<str>>) -> Self {
        self.project_server_ids = project_server_ids;
        self
    }
}

impl PickerDelegate for ToolPickerDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "tool picker"
    }

    fn match_count(&self) -> usize {
        self.filtered_items.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn can_select(&self, ix: usize, _window: &mut Window, _cx: &mut Context<Picker<Self>>) -> bool {
        matches!(
            self.filtered_items.get(ix),
            Some(PickerItem::Tool { .. } | PickerItem::ContextServer { .. })
        )
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        match self.mode {
            ToolPickerMode::BuiltinTools => "Search built-in tools…",
            ToolPickerMode::McpTools => "Search MCP tools…",
        }
        .into()
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let all_items = self.items.clone();

        cx.spawn_in(window, async move |this, cx| {
            let filtered_items = cx
                .background_spawn(async move {
                    let mut tools_by_provider: BTreeMap<Option<Arc<str>>, Vec<Arc<str>>> =
                        BTreeMap::default();

                    for item in all_items.iter() {
                        match item.clone() {
                            PickerItem::Tool { server_id, name } => {
                                let matches_tool = name.contains(&query);
                                let matches_server = server_id
                                    .as_ref()
                                    .map(|s| s.contains(&query))
                                    .unwrap_or(false);
                                if matches_tool || matches_server {
                                    tools_by_provider.entry(server_id).or_default().push(name);
                                }
                            }
                            PickerItem::ContextServer { server_id } => {
                                if server_id.contains(&query) {
                                    tools_by_provider.entry(Some(server_id)).or_default();
                                }
                            }
                        }
                    }

                    let mut items = Vec::new();

                    for (server_id, names) in tools_by_provider {
                        if let Some(server_id) = server_id.clone() {
                            items.push(PickerItem::ContextServer { server_id });
                        }
                        for name in names {
                            items.push(PickerItem::Tool {
                                server_id: server_id.clone(),
                                name,
                            });
                        }
                    }

                    items
                })
                .await;

            this.update(cx, |this, _cx| {
                this.delegate.filtered_items = filtered_items;
                this.delegate.selected_index = this
                    .delegate
                    .selected_index
                    .min(this.delegate.filtered_items.len().saturating_sub(1));
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if self.filtered_items.is_empty() {
            self.dismissed(window, cx);
            return;
        }

        let Some(item) = self.filtered_items.get(self.selected_index) else {
            return;
        };

        let is_currently_enabled = match item {
            PickerItem::ContextServer { server_id } => {
                let is_enabled = self
                    .profile_settings
                    .context_servers
                    .get(server_id.as_ref())
                    .and_then(|preset| preset.enabled)
                    .unwrap_or(self.profile_settings.enable_all_context_servers);

                let preset = self
                    .profile_settings
                    .context_servers
                    .entry(server_id.clone())
                    .or_default();
                preset.enabled = Some(!is_enabled);
                is_enabled
            }
            PickerItem::Tool {
                name: tool_name,
                server_id,
            } => {
                if let Some(server_id) = server_id.clone() {
                    let preset = self
                        .profile_settings
                        .context_servers
                        .entry(server_id)
                        .or_default();
                    let is_enabled = match preset.enabled {
                        Some(enabled) => preset.tools.get(tool_name).copied().unwrap_or(enabled),
                        None => preset
                            .tools
                            .get(tool_name)
                            .copied()
                            .unwrap_or(self.profile_settings.enable_all_context_servers),
                    };
                    preset.tools.insert(tool_name.clone(), !is_enabled);
                    preset.enabled = None;
                    is_enabled
                } else {
                    let is_enabled = *self
                        .profile_settings
                        .tools
                        .entry(tool_name.clone())
                        .or_default();
                    *self
                        .profile_settings
                        .tools
                        .entry(tool_name.clone())
                        .or_default() = !is_enabled;
                    is_enabled
                }
            }
        };

        let profile_id = self.profile_id.clone();
        let default_profile = self.profile_settings.clone();
        let item = item.clone();
        let all_items = self.items.clone();

        let update_fn = move |settings: &mut settings::SettingsContent, _cx: &App| {
            let profiles = settings
                .agent
                .get_or_insert_default()
                .profiles
                .get_or_insert_default();
            let profile = profiles
                .entry(profile_id.0)
                .or_insert_with(|| AgentProfileContent {
                    name: default_profile.name.into(),
                    origin: None,
                    tools: default_profile.tools,
                    enable_all_context_servers: Some(default_profile.enable_all_context_servers),
                    context_servers: default_profile
                        .context_servers
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
                    default_model: default_profile.default_model.clone(),
                    custom_prompt_path: default_profile
                        .custom_prompt_path
                        .clone()
                        .map(|s| s.into()),
                    system_prompt_template: default_profile
                        .system_prompt_template
                        .clone()
                        .map(|s| s.into()),
                    description: default_profile.description.clone().map(|s| s.into()),
                    skills: default_profile.skills.clone(),
                    delegation: default_profile.delegation.as_ref().map(|delegation| {
                        DelegationContent {
                            allowed: delegation
                                .allowed
                                .iter()
                                .map(|id| Arc::from(id.as_str()))
                                .collect(),
                            max_depth: Some(u32::from(delegation.max_depth)),
                        }
                    }),
                    tool_permissions: default_profile
                        .tool_permissions
                        .as_ref()
                        .map(|tool_permissions| tool_permissions.to_content()),
                    permission_mode: default_profile.permission_mode,
                    terminal_wrapper_command: default_profile
                        .terminal_wrapper_command
                        .clone()
                        .map(|s| s.to_string()),
                    task_isolation: Some(default_profile.task_isolation),
                    task_worktree_language_servers: default_profile.task_worktree_language_servers,
                });

            match &item {
                PickerItem::ContextServer { server_id } => {
                    profile.context_servers.insert(
                        server_id.clone(),
                        ContextServerPresetContent::Enabled(!is_currently_enabled),
                    );
                }
                PickerItem::Tool {
                    name: tool_name,
                    server_id,
                } => {
                    if let Some(server_id) = server_id {
                        let preset = profile.context_servers.entry(server_id.clone()).or_default();
                        match preset {
                            ContextServerPresetContent::Enabled(enabled) => {
                                let enabled = *enabled;
                                let tools = [(tool_name.clone(), !enabled)].into_iter().collect();
                                *preset = ContextServerPresetContent::Tools { tools };
                            }
                            ContextServerPresetContent::Tools { tools } => {
                                *tools.entry(tool_name.clone()).or_default() = !is_currently_enabled;
                            }
                        }
                    } else {
                        *profile.tools.entry(tool_name.clone()).or_default() = !is_currently_enabled;
                    }
                }
            }

            sync_profile_tool_permissions(
                profile,
                &item,
                &all_items,
                !is_currently_enabled,
            );
        };

        match &self.profile_settings.origin {
            agent_settings::ProfileOrigin::Global => {
                update_settings_file(self.fs.clone(), cx, update_fn);
            }
            agent_settings::ProfileOrigin::Project { worktree_id, path } => {
                settings::update_project_settings_file(
                    self.fs.clone(),
                    *worktree_id,
                    path.clone(),
                    cx,
                    update_fn,
                );
            }
        }
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.tool_picker
            .update(cx, |_this, cx| cx.emit(DismissEvent))
            .ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let item = &self.filtered_items.get(ix)?;
        match item {
            PickerItem::ContextServer { server_id, .. } => {
                let is_server_enabled = self
                    .profile_settings
                    .context_servers
                    .get(server_id.as_ref())
                    .and_then(|preset| preset.enabled)
                    .unwrap_or(self.profile_settings.enable_all_context_servers);
                let is_project_server = self.project_server_ids.contains(server_id.as_ref());

                let mut list_item = ListItem::new(ix)
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .toggle_state(selected)
                    .start_slot(
                        Icon::new(IconName::Server)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        h_flex().gap_2().child(Label::new(server_id.clone())).child(
                            Label::new("(All Tools)")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                    );

                if is_project_server || is_server_enabled {
                    list_item = list_item.end_slot(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .when(is_project_server, |this| {
                                this.child(
                                    Label::new("Project")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Accent),
                                )
                            })
                            .when(is_server_enabled, |this| {
                                this.child(
                                    Icon::new(IconName::Check)
                                        .size(IconSize::Small)
                                        .color(Color::Success),
                                )
                            }),
                    );
                }

                Some(list_item.into_any_element())
            }
            PickerItem::Tool { name, server_id } => {
                let is_enabled = if let Some(server_id) = server_id {
                    self.profile_settings
                        .context_servers
                        .get(server_id.as_ref())
                        .map(|preset| match preset.enabled {
                            Some(enabled) => enabled,
                            None => preset
                                .tools
                                .get(name)
                                .copied()
                                .unwrap_or(self.profile_settings.enable_all_context_servers),
                        })
                        .unwrap_or(self.profile_settings.enable_all_context_servers)
                } else {
                    self.profile_settings
                        .tools
                        .get(name)
                        .copied()
                        .unwrap_or(false)
                };

                Some(
                    ListItem::new(ix)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .toggle_state(selected)
                        .child(Label::new(name.clone()))
                        .end_slot::<Icon>(is_enabled.then(|| {
                            Icon::new(IconName::Check)
                                .size(IconSize::Small)
                                .color(Color::Success)
                        }))
                        .into_any_element(),
                )
            }
        }
    }
}

fn sync_tool_permission(
    tool_permissions: &mut ToolPermissionsContent,
    server_id: Option<&str>,
    tool_name: &str,
    enabled: bool,
) {
    let key: Arc<str> = match server_id {
        Some(server_id) => format!("mcp:{server_id}:{tool_name}").into(),
        None => tool_name.to_string().into(),
    };
    if enabled {
        tool_permissions.tools.insert(
            key,
            ToolRulesContent {
                default: Some(ToolPermissionMode::Allow),
                ..Default::default()
            },
        );
    } else {
        tool_permissions.tools.remove(&key);
    }
}

fn sync_profile_tool_permissions(
    profile: &mut AgentProfileContent,
    item: &PickerItem,
    all_items: &[PickerItem],
    enabled: bool,
) {
    let Some(tool_permissions) = profile.tool_permissions.as_mut() else {
        return;
    };

    match item {
        PickerItem::ContextServer { server_id } => {
            for picker_item in all_items {
                if let PickerItem::Tool {
                    name: tool_name,
                    server_id: Some(tool_server_id),
                } = picker_item
                {
                    if tool_server_id == server_id {
                        sync_tool_permission(
                            tool_permissions,
                            Some(server_id.as_ref()),
                            tool_name,
                            enabled,
                        );
                    }
                }
            }
        }
        PickerItem::Tool {
            name: tool_name,
            server_id,
        } => {
            sync_tool_permission(
                tool_permissions,
                server_id.as_deref(),
                tool_name,
                enabled,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use collections::HashMap;

    fn test_profile(tool_permissions: Option<ToolPermissionsContent>) -> AgentProfileContent {
        AgentProfileContent {
            name: "test_profile".into(),
            origin: None,
            tools: Default::default(),
            enable_all_context_servers: None,
            context_servers: Default::default(),
            default_model: None,
            custom_prompt_path: None,
            system_prompt_template: None,
            description: None,
            skills: None,
            delegation: None,
            tool_permissions,
            permission_mode: None,
            terminal_wrapper_command: None,
            task_isolation: None,
            task_worktree_language_servers: None,
        }
    }

    #[test]
    fn test_sync_mcp_tool_enables_and_inserts_allow_permission() {
        let mut profile = test_profile(Some(ToolPermissionsContent {
            default: Some(ToolPermissionMode::Deny),
            tools: HashMap::default(),
        }));

        let item = PickerItem::Tool {
            name: "search".into(),
            server_id: Some("fetch".into()),
        };
        let items = vec![item.clone()];

        sync_profile_tool_permissions(&mut profile, &item, &items, true);

        let permissions = profile.tool_permissions.as_ref().unwrap();
        let rule = permissions
            .tools
            .get("mcp:fetch:search")
            .expect("permission rule should be inserted for enabled MCP tool");
        assert_eq!(rule.default, Some(ToolPermissionMode::Allow));
    }

    #[test]
    fn test_sync_mcp_tool_disables_and_removes_permission() {
        let mut tools = HashMap::default();
        tools.insert(
            "mcp:fetch:search".into(),
            ToolRulesContent {
                default: Some(ToolPermissionMode::Allow),
                ..Default::default()
            },
        );

        let mut profile = test_profile(Some(ToolPermissionsContent {
            default: Some(ToolPermissionMode::Deny),
            tools,
        }));

        let item = PickerItem::Tool {
            name: "search".into(),
            server_id: Some("fetch".into()),
        };
        let items = vec![item.clone()];

        sync_profile_tool_permissions(&mut profile, &item, &items, false);

        let permissions = profile.tool_permissions.as_ref().unwrap();
        assert!(
            !permissions.tools.contains_key("mcp:fetch:search"),
            "permission rule should be removed when MCP tool is disabled"
        );
    }

    #[test]
    fn test_sync_builtin_tool_enables_and_inserts_allow_permission() {
        let mut profile = test_profile(Some(ToolPermissionsContent {
            default: Some(ToolPermissionMode::Deny),
            tools: HashMap::default(),
        }));

        let item = PickerItem::Tool {
            name: "terminal".into(),
            server_id: None,
        };
        let items = vec![item.clone()];

        sync_profile_tool_permissions(&mut profile, &item, &items, true);

        let permissions = profile.tool_permissions.as_ref().unwrap();
        let rule = permissions
            .tools
            .get("terminal")
            .expect("permission rule should be inserted for enabled builtin tool");
        assert_eq!(rule.default, Some(ToolPermissionMode::Allow));
    }

    #[test]
    fn test_sync_builtin_tool_disables_and_removes_permission() {
        let mut tools = HashMap::default();
        tools.insert(
            "terminal".into(),
            ToolRulesContent {
                default: Some(ToolPermissionMode::Allow),
                ..Default::default()
            },
        );

        let mut profile = test_profile(Some(ToolPermissionsContent {
            default: Some(ToolPermissionMode::Deny),
            tools,
        }));

        let item = PickerItem::Tool {
            name: "terminal".into(),
            server_id: None,
        };
        let items = vec![item.clone()];

        sync_profile_tool_permissions(&mut profile, &item, &items, false);

        let permissions = profile.tool_permissions.as_ref().unwrap();
        assert!(
            !permissions.tools.contains_key("terminal"),
            "permission rule should be removed when builtin tool is disabled"
        );
    }

    #[test]
    fn test_sync_context_server_enables_all_its_tools() {
        let mut profile = test_profile(Some(ToolPermissionsContent {
            default: Some(ToolPermissionMode::Deny),
            tools: HashMap::default(),
        }));

        let server_id: Arc<str> = "server_alpha".into();
        let items = vec![
            PickerItem::ContextServer {
                server_id: server_id.clone(),
            },
            PickerItem::Tool {
                name: "query".into(),
                server_id: Some(server_id.clone()),
            },
            PickerItem::Tool {
                name: "mutate".into(),
                server_id: Some(server_id.clone()),
            },
            PickerItem::Tool {
                name: "other_tool".into(),
                server_id: Some("server_beta".into()),
            },
            PickerItem::Tool {
                name: "builtin_tool".into(),
                server_id: None,
            },
        ];

        let context_server_item = PickerItem::ContextServer {
            server_id: server_id.clone(),
        };

        sync_profile_tool_permissions(&mut profile, &context_server_item, &items, true);

        let permissions = profile.tool_permissions.as_ref().unwrap();
        let rule1 = permissions
            .tools
            .get("mcp:server_alpha:query")
            .expect("query tool should be allowed");
        assert_eq!(rule1.default, Some(ToolPermissionMode::Allow));

        let rule2 = permissions
            .tools
            .get("mcp:server_alpha:mutate")
            .expect("mutate tool should be allowed");
        assert_eq!(rule2.default, Some(ToolPermissionMode::Allow));

        assert!(
            !permissions.tools.contains_key("mcp:server_beta:other_tool"),
            "tools from other servers should not be touched"
        );
        assert!(
            !permissions.tools.contains_key("builtin_tool"),
            "builtin tools should not be touched when toggling context server"
        );
    }

    #[test]
    fn test_sync_context_server_disables_all_its_tools() {
        let mut tools = HashMap::default();
        tools.insert(
            "mcp:server_alpha:query".into(),
            ToolRulesContent {
                default: Some(ToolPermissionMode::Allow),
                ..Default::default()
            },
        );
        tools.insert(
            "mcp:server_alpha:mutate".into(),
            ToolRulesContent {
                default: Some(ToolPermissionMode::Allow),
                ..Default::default()
            },
        );
        tools.insert(
            "mcp:server_beta:other_tool".into(),
            ToolRulesContent {
                default: Some(ToolPermissionMode::Allow),
                ..Default::default()
            },
        );

        let mut profile = test_profile(Some(ToolPermissionsContent {
            default: Some(ToolPermissionMode::Deny),
            tools,
        }));

        let server_id: Arc<str> = "server_alpha".into();
        let items = vec![
            PickerItem::ContextServer {
                server_id: server_id.clone(),
            },
            PickerItem::Tool {
                name: "query".into(),
                server_id: Some(server_id.clone()),
            },
            PickerItem::Tool {
                name: "mutate".into(),
                server_id: Some(server_id.clone()),
            },
            PickerItem::Tool {
                name: "other_tool".into(),
                server_id: Some("server_beta".into()),
            },
        ];

        let context_server_item = PickerItem::ContextServer {
            server_id: server_id.clone(),
        };

        sync_profile_tool_permissions(&mut profile, &context_server_item, &items, false);

        let permissions = profile.tool_permissions.as_ref().unwrap();
        assert!(
            !permissions.tools.contains_key("mcp:server_alpha:query"),
            "query tool should be removed"
        );
        assert!(
            !permissions.tools.contains_key("mcp:server_alpha:mutate"),
            "mutate tool should be removed"
        );
        assert!(
            permissions.tools.contains_key("mcp:server_beta:other_tool"),
            "tools from other servers must remain intact"
        );
    }

    #[test]
    fn test_sync_tool_permissions_noop_when_profile_has_no_permissions() {
        let mut profile = test_profile(None);

        let mcp_tool = PickerItem::Tool {
            name: "query".into(),
            server_id: Some("server_alpha".into()),
        };
        let context_server = PickerItem::ContextServer {
            server_id: "server_alpha".into(),
        };
        let items = vec![context_server.clone(), mcp_tool.clone()];

        sync_profile_tool_permissions(&mut profile, &mcp_tool, &items, true);
        assert!(profile.tool_permissions.is_none());

        sync_profile_tool_permissions(&mut profile, &context_server, &items, true);
        assert!(profile.tool_permissions.is_none());

        sync_profile_tool_permissions(&mut profile, &mcp_tool, &items, false);
        assert!(profile.tool_permissions.is_none());

        sync_profile_tool_permissions(&mut profile, &context_server, &items, false);
        assert!(profile.tool_permissions.is_none());
    }
}
