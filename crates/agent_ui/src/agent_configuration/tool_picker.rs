use std::{collections::BTreeMap, sync::Arc};

use agent::ContextServerRegistry;
use agent_settings::{AgentProfileId, AgentProfileSettings};
use fs::Fs;
use gpui::{App, Context, DismissEvent, Entity, EventEmitter, Focusable, Task, WeakEntity, Window};
use picker::{Picker, PickerDelegate};
use settings::{
    AgentProfileContent, ContextServerPresetContent, DelegationContent, update_settings_file,
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

        for (id, tools) in registry.read(cx).servers() {
            let server_id = id.clone().0;
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
        }
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
                    custom_prompt: default_profile.custom_prompt.clone().map(|s| s.into()),
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
                });

            match item {
                PickerItem::ContextServer { server_id } => {
                    profile.context_servers.insert(
                        server_id,
                        ContextServerPresetContent::Enabled(!is_currently_enabled),
                    );
                }
                PickerItem::Tool {
                    name: tool_name,
                    server_id,
                } => {
                    if let Some(server_id) = server_id {
                        let preset = profile.context_servers.entry(server_id).or_default();
                        match preset {
                            ContextServerPresetContent::Enabled(enabled) => {
                                let enabled = *enabled;
                                let tools = [(tool_name, !enabled)].into_iter().collect();
                                *preset = ContextServerPresetContent::Tools { tools };
                            }
                            ContextServerPresetContent::Tools { tools } => {
                                *tools.entry(tool_name).or_default() = !is_currently_enabled;
                            }
                        }
                    } else {
                        *profile.tools.entry(tool_name).or_default() = !is_currently_enabled;
                    }
                }
            }
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
            .log_err();
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

                Some(
                    ListItem::new(ix)
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
                        )
                        .end_slot::<Icon>(is_server_enabled.then(|| {
                            Icon::new(IconName::Check)
                                .size(IconSize::Small)
                                .color(Color::Success)
                        }))
                        .into_any_element(),
                )
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
