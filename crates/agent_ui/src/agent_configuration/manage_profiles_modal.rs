mod profile_modal_header;

use std::sync::Arc;

use agent::ContextServerRegistry;
use agent_settings::{
    AgentProfile, AgentProfileId, AgentSettings, ProfileOrigin, builtin_profiles,
};
use editor::Editor;
use fs::Fs;
use gpui::{DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription, prelude::*};
use language_model::{LanguageModel, LanguageModelRegistry};
use settings::SettingsStore;
use settings::{LanguageModelProviderSetting, LanguageModelSelection, Settings as _};
use ui::{
    KeyBinding, ListItem, ListItemSpacing, ListSeparator, Navigable, NavigableEntry, prelude::*,
};
use workspace::{ModalView, Workspace};

use crate::agent_configuration::delegation_editor::DelegationEditor;
use crate::agent_configuration::manage_profiles_modal::profile_modal_header::ProfileModalHeader;
use crate::agent_configuration::skills_editor::SkillsEditor;
use crate::agent_configuration::tool_picker::{ToolPicker, ToolPickerDelegate};
use crate::language_model_selector::{LanguageModelSelector, language_model_selector};
use crate::{AgentPanel, ManageProfiles};

enum Mode {
    ChooseProfile(ChooseProfileMode),
    NewProfile(NewProfileMode),
    ViewProfile(ViewProfileMode),
    ConfigureTools {
        profile_id: AgentProfileId,
        tool_picker: Entity<ToolPicker>,
        _subscription: Subscription,
    },
    ConfigureMcps {
        profile_id: AgentProfileId,
        tool_picker: Entity<ToolPicker>,
        _subscription: Subscription,
    },
    ConfigureDefaultModel {
        profile_id: AgentProfileId,
        model_picker: Entity<LanguageModelSelector>,
        _subscription: Subscription,
    },
    ConfigureDelegation {
        profile_id: AgentProfileId,
        delegation_editor: Entity<DelegationEditor>,
        _subscription: Subscription,
    },
    ConfigureSkills {
        profile_id: AgentProfileId,
        skills_editor: Entity<SkillsEditor>,
    },
    ConfigureCustomPrompt {
        profile_id: AgentProfileId,
        prompt_editor: Entity<Editor>,
        _subscription: Subscription,
    },
    ConfigureDescription {
        profile_id: AgentProfileId,
        description_editor: Entity<Editor>,
        _subscription: Subscription,
    },
}

impl Mode {
    pub fn choose_profile(_window: &mut Window, cx: &mut Context<ManageProfilesModal>) -> Self {
        let settings = AgentSettings::get_global(cx);

        let mut builtin_profiles = Vec::new();
        let mut custom_profiles = Vec::new();

        for (profile_id, profile) in settings.profiles.iter() {
            let entry = ProfileEntry {
                id: profile_id.clone(),
                name: profile.name.clone(),
                navigation: NavigableEntry::focusable(cx),
            };
            if builtin_profiles::is_builtin(profile_id) {
                builtin_profiles.push(entry);
            } else {
                custom_profiles.push(entry);
            }
        }

        builtin_profiles.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        custom_profiles.sort_unstable_by(|a, b| a.name.cmp(&b.name));

        Self::ChooseProfile(ChooseProfileMode {
            builtin_profiles,
            custom_profiles,
            add_new_profile: NavigableEntry::focusable(cx),
        })
    }
}

#[derive(Clone)]
struct ProfileEntry {
    pub id: AgentProfileId,
    pub name: SharedString,
    pub navigation: NavigableEntry,
}

#[derive(Clone)]
pub struct ChooseProfileMode {
    builtin_profiles: Vec<ProfileEntry>,
    custom_profiles: Vec<ProfileEntry>,
    add_new_profile: NavigableEntry,
}

#[derive(Clone)]
pub struct ViewProfileMode {
    profile_id: AgentProfileId,
    fork_profile: NavigableEntry,
    configure_description: NavigableEntry,
    configure_custom_prompt: NavigableEntry,
    configure_default_model: NavigableEntry,
    configure_tools: NavigableEntry,
    configure_mcps: NavigableEntry,
    configure_delegation: NavigableEntry,
    configure_skills: NavigableEntry,
    delete_profile: NavigableEntry,
    cancel_item: NavigableEntry,
}

#[derive(Clone)]
pub struct NewProfileMode {
    name_editor: Entity<Editor>,
    base_profile_id: Option<AgentProfileId>,
    target_origin: ProfileOrigin,
    available_origins: Vec<ProfileOrigin>,
}

pub struct ManageProfilesModal {
    fs: Arc<dyn Fs>,
    context_server_registry: Entity<ContextServerRegistry>,
    active_model: Option<Arc<dyn LanguageModel>>,
    focus_handle: FocusHandle,
    mode: Mode,
    _settings_subscription: Subscription,
}

impl ManageProfilesModal {
    pub fn register(
        workspace: &mut Workspace,
        _window: Option<&mut Window>,
        _cx: &mut Context<Workspace>,
    ) {
        workspace.register_action(|workspace, action: &ManageProfiles, window, cx| {
            if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                let fs = workspace.app_state().fs.clone();
                let active_model = panel
                    .read(cx)
                    .active_native_agent_thread(cx)
                    .and_then(|thread| thread.read(cx).model().cloned());

                let context_server_registry = panel.read(cx).context_server_registry().clone();
                workspace.toggle_modal(window, cx, |window, cx| {
                    let mut this = Self::new(fs, active_model, context_server_registry, window, cx);

                    if let Some(profile_id) = action.customize_tools.clone() {
                        this.configure_builtin_tools(profile_id, window, cx);
                    }

                    this
                })
            }
        });
    }

    pub fn new(
        fs: Arc<dyn Fs>,
        active_model: Option<Arc<dyn LanguageModel>>,
        context_server_registry: Entity<ContextServerRegistry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        // Keep this modal in sync with settings changes (including profile deletion).
        let settings_subscription =
            cx.observe_global_in::<SettingsStore>(window, |this, window, cx| {
                if matches!(this.mode, Mode::ChooseProfile(_)) {
                    this.mode = Mode::choose_profile(window, cx);
                    this.focus_handle(cx).focus(window, cx);
                    cx.notify();
                }
            });

        Self {
            fs,
            active_model,
            context_server_registry,
            focus_handle,
            mode: Mode::choose_profile(window, cx),
            _settings_subscription: settings_subscription,
        }
    }

    pub fn save_profile_change_by_origin(
        fs: Arc<dyn Fs>,
        origin: &ProfileOrigin,
        cx: &App,
        update: impl 'static + Send + FnOnce(&mut settings::SettingsContent, &App),
    ) {
        match origin {
            ProfileOrigin::Global => {
                settings::update_settings_file(fs, cx, update);
            }
            ProfileOrigin::Project { worktree_id, path } => {
                settings::update_project_settings_file(fs, *worktree_id, path.clone(), cx, update);
            }
        }
    }

    fn choose_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = Mode::choose_profile(window, cx);
        self.focus_handle(cx).focus(window, cx);
    }

    fn new_profile(
        &mut self,
        base_profile_id: Option<AgentProfileId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name_editor = cx.new(|cx| Editor::single_line(window, cx));
        name_editor.update(cx, |editor, cx| {
            editor.set_placeholder_text("Profile name", window, cx);
        });

        let mut available_origins = vec![ProfileOrigin::Global];
        let app_state = workspace::AppState::global(cx);
        for workspace in app_state
            .workspace_store
            .read(cx)
            .workspaces()
            .filter_map(|w| w.upgrade())
        {
            let project = workspace.read(cx).project();
            for worktree in project.read(cx).worktrees(cx) {
                let worktree_read = worktree.read(cx);
                if worktree_read.is_visible() {
                    let origin = ProfileOrigin::Project {
                        worktree_id: worktree_read.id(),
                        path: paths::local_settings_file_relative_path().into(),
                    };
                    if !available_origins.contains(&origin) {
                        available_origins.push(origin);
                    }
                }
            }
        }

        let target_origin = base_profile_id
            .as_ref()
            .and_then(|id| AgentSettings::get_global(cx).profiles.get(id))
            .map(|p| p.origin.clone())
            .unwrap_or_default();

        self.mode = Mode::NewProfile(NewProfileMode {
            name_editor,
            base_profile_id,
            target_origin,
            available_origins,
        });
        self.focus_handle(cx).focus(window, cx);
    }

    pub fn view_profile(
        &mut self,
        profile_id: AgentProfileId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mode = Mode::ViewProfile(ViewProfileMode {
            profile_id,
            fork_profile: NavigableEntry::focusable(cx),
            configure_description: NavigableEntry::focusable(cx),
            configure_custom_prompt: NavigableEntry::focusable(cx),
            configure_default_model: NavigableEntry::focusable(cx),
            configure_tools: NavigableEntry::focusable(cx),
            configure_mcps: NavigableEntry::focusable(cx),
            configure_delegation: NavigableEntry::focusable(cx),
            configure_skills: NavigableEntry::focusable(cx),
            delete_profile: NavigableEntry::focusable(cx),
            cancel_item: NavigableEntry::focusable(cx),
        });
        self.focus_handle(cx).focus(window, cx);
    }

    fn configure_default_model(
        &mut self,
        profile_id: AgentProfileId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        telemetry::event!(
            "Agent Profile Default Model Configured",
            profile_id = profile_id.as_str(),
            is_builtin = builtin_profiles::is_builtin(&profile_id)
        );
        let fs = self.fs.clone();
        let profile_id_for_closure = profile_id.clone();

        let profile = AgentSettings::get_global(cx)
            .profiles
            .get(&profile_id)
            .cloned();
        let origin = profile
            .as_ref()
            .map(|p| p.origin.clone())
            .unwrap_or_default();

        let model_picker = cx.new(|cx| {
            let profile_id = profile_id_for_closure.clone();

            language_model_selector(
                {
                    let profile_id = profile_id.clone();
                    move |cx| {
                        let settings = AgentSettings::get_global(cx);

                        settings
                            .profiles
                            .get(&profile_id)
                            .and_then(|profile| profile.default_model.as_ref())
                            .and_then(|selection| {
                                let registry = LanguageModelRegistry::read_global(cx);
                                let provider_id = language_model::LanguageModelProviderId(
                                    gpui::SharedString::from(selection.provider.0.clone()),
                                );
                                let provider = registry.provider(&provider_id)?;
                                let model = provider
                                    .provided_models(cx)
                                    .iter()
                                    .find(|m| m.id().0 == selection.model.as_str())?
                                    .clone();
                                Some(language_model::ConfiguredModel { provider, model })
                            })
                    }
                },
                {
                    let fs = fs.clone();
                    let origin = origin.clone();
                    move |model, cx| {
                        let provider = model.provider_id().0.to_string();
                        let model_id = model.id().0.to_string();
                        let profile_id = profile_id.clone();

                        Self::save_profile_change_by_origin(
                            fs.clone(),
                            &origin,
                            cx,
                            move |settings, _cx| {
                                let agent_settings = settings.agent.get_or_insert_default();
                                if let Some(profiles) = agent_settings.profiles.as_mut() {
                                    if let Some(profile) = profiles.get_mut(profile_id.0.as_ref()) {
                                        profile.default_model = Some(LanguageModelSelection {
                                            provider: LanguageModelProviderSetting(
                                                provider.clone(),
                                            ),
                                            model: model_id.clone(),
                                            enable_thinking: model.supports_thinking(),
                                            effort: model
                                                .default_effort_level()
                                                .map(|effort| effort.value.to_string()),
                                            speed: None,
                                        });
                                    }
                                }
                            },
                        );
                    }
                },
                {
                    let fs = fs.clone();
                    move |model, should_be_favorite, cx| {
                        crate::favorite_models::toggle_in_settings(
                            model,
                            should_be_favorite,
                            fs.clone(),
                            cx,
                        );
                    }
                },
                false, // Do not use popover styles for the model picker
                self.focus_handle.clone(),
                window,
                cx,
            )
            .embedded()
        });

        let dismiss_subscription = cx.subscribe_in(&model_picker, window, {
            let profile_id = profile_id.clone();
            move |this, _picker, _: &DismissEvent, window, cx| {
                this.view_profile(profile_id.clone(), window, cx);
            }
        });

        self.mode = Mode::ConfigureDefaultModel {
            profile_id,
            model_picker,
            _subscription: dismiss_subscription,
        };
        self.focus_handle(cx).focus(window, cx);
    }

    fn configure_mcp_tools(
        &mut self,
        profile_id: AgentProfileId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        telemetry::event!(
            "Agent Profile MCPs Configured",
            profile_id = profile_id.as_str(),
            is_builtin = builtin_profiles::is_builtin(&profile_id)
        );
        let settings = AgentSettings::get_global(cx);
        let Some(profile) = settings.profiles.get(&profile_id).cloned() else {
            return;
        };

        let tool_picker = cx.new(|cx| {
            let delegate = ToolPickerDelegate::mcp_tools(
                &self.context_server_registry,
                self.fs.clone(),
                profile_id.clone(),
                profile,
                cx,
            );
            ToolPicker::mcp_tools(delegate, window, cx)
        });
        let dismiss_subscription = cx.subscribe_in(&tool_picker, window, {
            let profile_id = profile_id.clone();
            move |this, _tool_picker, _: &DismissEvent, window, cx| {
                this.view_profile(profile_id.clone(), window, cx);
            }
        });

        self.mode = Mode::ConfigureMcps {
            profile_id,
            tool_picker,
            _subscription: dismiss_subscription,
        };
        self.focus_handle(cx).focus(window, cx);
    }

    fn configure_delegation(
        &mut self,
        profile_id: AgentProfileId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        telemetry::event!(
            "Agent Profile Delegation Configured",
            profile_id = profile_id.as_str(),
            is_builtin = builtin_profiles::is_builtin(&profile_id)
        );
        let delegation_editor =
            cx.new(|cx| DelegationEditor::new(profile_id.clone(), self.fs.clone(), window, cx));
        let dismiss_subscription = cx.subscribe_in(&delegation_editor, window, {
            let profile_id = profile_id.clone();
            move |this, _delegation_editor, _: &DismissEvent, window, cx| {
                this.view_profile(profile_id.clone(), window, cx);
            }
        });

        self.mode = Mode::ConfigureDelegation {
            profile_id,
            delegation_editor,
            _subscription: dismiss_subscription,
        };
        self.focus_handle(cx).focus(window, cx);
    }

    fn configure_skills(
        &mut self,
        profile_id: AgentProfileId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        telemetry::event!(
            "Agent Profile Skills Configured",
            profile_id = profile_id.as_str(),
            is_builtin = builtin_profiles::is_builtin(&profile_id)
        );
        let skills_editor =
            cx.new(|cx| SkillsEditor::new(profile_id.clone(), self.fs.clone(), window, cx));

        self.mode = Mode::ConfigureSkills {
            profile_id,
            skills_editor,
        };
        self.focus_handle(cx).focus(window, cx);
    }

    fn configure_custom_prompt(
        &mut self,
        profile_id: AgentProfileId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        telemetry::event!(
            "Agent Profile Custom Prompt Configured",
            profile_id = profile_id.as_str(),
            is_builtin = builtin_profiles::is_builtin(&profile_id)
        );
        let settings = AgentSettings::get_global(cx);
        let prompt = settings
            .profiles
            .get(&profile_id)
            .and_then(|profile| profile.custom_prompt.clone())
            .unwrap_or_default();

        let prompt_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(6, 16, window, cx);
            editor.set_placeholder_text(
                "Custom prompt / instructions for this profile…",
                window,
                cx,
            );
            if !prompt.is_empty() {
                editor.set_text(prompt.to_string(), window, cx);
            }
            editor
        });

        let fs = self.fs.clone();
        let target_profile_id = profile_id.clone();
        let subscription = cx.subscribe(&prompt_editor, move |_this, editor, event, cx| {
            // Persist only on blur: saving on every keystroke would rewrite
            // the settings file per character typed.
            if matches!(event, editor::EditorEvent::Blurred) {
                let text = editor.read(cx).text(cx);
                let text = text.trim().to_string();
                let fs = fs.clone();
                let profile_id = target_profile_id.clone();
                let origin = AgentSettings::get_global(cx)
                    .profiles
                    .get(&profile_id)
                    .map(|p| p.origin.clone())
                    .unwrap_or_default();
                Self::save_profile_change_by_origin(fs, &origin, cx, move |settings, _cx| {
                    let Some(profile) = settings
                        .agent
                        .get_or_insert_default()
                        .profiles
                        .get_or_insert_default()
                        .get_mut(profile_id.0.as_ref())
                    else {
                        return;
                    };
                    profile.custom_prompt = (!text.is_empty()).then(|| Arc::from(text.as_str()));
                });
            }
        });

        self.mode = Mode::ConfigureCustomPrompt {
            profile_id,
            prompt_editor,
            _subscription: subscription,
        };
        self.focus_handle(cx).focus(window, cx);
    }

    fn configure_description(
        &mut self,
        profile_id: AgentProfileId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        telemetry::event!(
            "Agent Profile Description Configured",
            profile_id = profile_id.as_str(),
            is_builtin = builtin_profiles::is_builtin(&profile_id)
        );
        let settings = AgentSettings::get_global(cx);
        let description = settings
            .profiles
            .get(&profile_id)
            .and_then(|profile| profile.description.clone())
            .unwrap_or_default();

        let description_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 6, window, cx);
            editor.set_placeholder_text(
                "Short description shown in delegation catalog…",
                window,
                cx,
            );
            if !description.is_empty() {
                editor.set_text(description.to_string(), window, cx);
            }
            editor
        });

        let fs = self.fs.clone();
        let target_profile_id = profile_id.clone();
        let subscription = cx.subscribe(&description_editor, move |_this, editor, event, cx| {
            // Persist only on blur: saving on every keystroke would rewrite
            // the settings file per character typed.
            if matches!(event, editor::EditorEvent::Blurred) {
                let text = editor.read(cx).text(cx);
                let text = text.trim().to_string();
                let fs = fs.clone();
                let profile_id = target_profile_id.clone();
                let origin = AgentSettings::get_global(cx)
                    .profiles
                    .get(&profile_id)
                    .map(|p| p.origin.clone())
                    .unwrap_or_default();
                Self::save_profile_change_by_origin(fs, &origin, cx, move |settings, _cx| {
                    let Some(profile) = settings
                        .agent
                        .get_or_insert_default()
                        .profiles
                        .get_or_insert_default()
                        .get_mut(profile_id.0.as_ref())
                    else {
                        return;
                    };
                    profile.description = (!text.is_empty()).then(|| Arc::from(text.as_str()));
                });
            }
        });

        self.mode = Mode::ConfigureDescription {
            profile_id,
            description_editor,
            _subscription: subscription,
        };
        self.focus_handle(cx).focus(window, cx);
    }

    fn configure_builtin_tools(
        &mut self,
        profile_id: AgentProfileId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        telemetry::event!(
            "Agent Profile Tools Configured",
            profile_id = profile_id.as_str(),
            is_builtin = builtin_profiles::is_builtin(&profile_id)
        );
        let settings = AgentSettings::get_global(cx);
        let Some(profile) = settings.profiles.get(&profile_id).cloned() else {
            return;
        };

        let provider = self.active_model.as_ref().map(|model| model.provider_id());
        let tool_names: Vec<Arc<str>> = agent::ALL_TOOL_NAMES
            .iter()
            .copied()
            .filter(|name| {
                let supported_by_provider = provider.as_ref().map_or(true, |provider| {
                    agent::tool_supports_provider(name, provider)
                });
                // Don't offer tools the agent can't actually use: tools gated
                // behind an inactive feature flag are silently dropped before
                // they reach the model (#56778).
                supported_by_provider && agent::tool_feature_flag_enabled(name, cx)
            })
            .map(Arc::from)
            .collect();

        let tool_picker = cx.new(|cx| {
            let delegate = ToolPickerDelegate::builtin_tools(
                tool_names,
                self.fs.clone(),
                profile_id.clone(),
                profile,
                cx,
            );
            ToolPicker::builtin_tools(delegate, window, cx)
        });
        let dismiss_subscription = cx.subscribe_in(&tool_picker, window, {
            let profile_id = profile_id.clone();
            move |this, _tool_picker, _: &DismissEvent, window, cx| {
                this.view_profile(profile_id.clone(), window, cx);
            }
        });

        self.mode = Mode::ConfigureTools {
            profile_id,
            tool_picker,
            _subscription: dismiss_subscription,
        };
        self.focus_handle(cx).focus(window, cx);
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.mode {
            Mode::ChooseProfile { .. } => {}
            Mode::NewProfile(mode) => {
                let name = mode.name_editor.read(cx).text(cx);
                let base_profile_id = mode.base_profile_id.clone();
                let target_origin = mode.target_origin.clone();

                let profile_id = AgentProfile::create(
                    name,
                    base_profile_id.clone(),
                    target_origin,
                    self.fs.clone(),
                    cx,
                );
                telemetry::event!(
                    "Agent Profile Created",
                    profile_id = profile_id.as_str(),
                    is_fork = base_profile_id.is_some(),
                    base_profile_id = base_profile_id.as_ref().map(|id| id.as_str())
                );
                self.view_profile(profile_id, window, cx);
            }
            Mode::ViewProfile(_) => {}
            Mode::ConfigureTools { .. } => {}
            Mode::ConfigureMcps { .. } => {}
            Mode::ConfigureDefaultModel { .. } => {}
            Mode::ConfigureDelegation { .. } => {}
            Mode::ConfigureSkills { .. } => {}
            Mode::ConfigureCustomPrompt { .. } => {}
            Mode::ConfigureDescription { .. } => {}
        }
    }

    fn delete_profile(
        &mut self,
        profile_id: AgentProfileId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if builtin_profiles::is_builtin(&profile_id) {
            self.view_profile(profile_id, window, cx);
            return;
        }

        telemetry::event!("Agent Profile Deleted", profile_id = profile_id.as_str());

        let origin = AgentSettings::get_global(cx)
            .profiles
            .get(&profile_id)
            .map(|p| p.origin.clone())
            .unwrap_or_default();

        let fs = self.fs.clone();

        Self::save_profile_change_by_origin(fs, &origin, cx, move |settings, _cx| {
            let Some(agent_settings) = settings.agent.as_mut() else {
                return;
            };

            let Some(profiles) = agent_settings.profiles.as_mut() else {
                return;
            };

            profiles.shift_remove(profile_id.0.as_ref());

            if agent_settings
                .default_profile
                .as_deref()
                .is_some_and(|default_profile| default_profile == profile_id.0.as_ref())
            {
                agent_settings.default_profile = Some(AgentProfileId::default().0);
            }
        });

        self.choose_profile(window, cx);
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.mode {
            Mode::ChooseProfile { .. } => {
                cx.emit(DismissEvent);
            }
            Mode::NewProfile(mode) => {
                if let Some(profile_id) = mode.base_profile_id.clone() {
                    self.view_profile(profile_id, window, cx);
                } else {
                    self.choose_profile(window, cx);
                }
            }
            Mode::ViewProfile(_) => self.choose_profile(window, cx),
            Mode::ConfigureTools { profile_id, .. } => {
                self.view_profile(profile_id.clone(), window, cx)
            }
            Mode::ConfigureMcps { profile_id, .. } => {
                self.view_profile(profile_id.clone(), window, cx)
            }
            Mode::ConfigureDefaultModel { profile_id, .. } => {
                self.view_profile(profile_id.clone(), window, cx)
            }
            Mode::ConfigureDelegation { profile_id, .. } => {
                self.view_profile(profile_id.clone(), window, cx)
            }
            Mode::ConfigureSkills { profile_id, .. } => {
                self.view_profile(profile_id.clone(), window, cx)
            }
            Mode::ConfigureCustomPrompt { profile_id, .. } => {
                self.view_profile(profile_id.clone(), window, cx)
            }
            Mode::ConfigureDescription { profile_id, .. } => {
                self.view_profile(profile_id.clone(), window, cx)
            }
        }
    }
}

impl ModalView for ManageProfilesModal {}

impl Focusable for ManageProfilesModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.mode {
            Mode::ChooseProfile(_) => self.focus_handle.clone(),
            Mode::NewProfile(mode) => mode.name_editor.focus_handle(cx),
            Mode::ViewProfile(_) => self.focus_handle.clone(),
            Mode::ConfigureTools {
                tool_picker,
                profile_id: _,
                _subscription: _,
            } => tool_picker.focus_handle(cx),
            Mode::ConfigureMcps {
                tool_picker,
                profile_id: _,
                _subscription: _,
            } => tool_picker.focus_handle(cx),
            Mode::ConfigureDefaultModel {
                model_picker,
                profile_id: _,
                _subscription: _,
            } => model_picker.focus_handle(cx),
            Mode::ConfigureDelegation {
                delegation_editor,
                profile_id: _,
                _subscription: _,
            } => delegation_editor.focus_handle(cx),
            Mode::ConfigureSkills {
                skills_editor,
                profile_id: _,
            } => skills_editor.focus_handle(cx),
            Mode::ConfigureCustomPrompt {
                prompt_editor,
                profile_id: _,
                _subscription: _,
            } => prompt_editor.focus_handle(cx),
            Mode::ConfigureDescription {
                description_editor,
                profile_id: _,
                _subscription: _,
            } => description_editor.focus_handle(cx),
        }
    }
}

impl EventEmitter<DismissEvent> for ManageProfilesModal {}

impl ManageProfilesModal {
    fn render_profile(
        &self,
        profile: &ProfileEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let is_focused = profile.navigation.focus_handle.contains_focused(window, cx);

        let origin = AgentSettings::get_global(cx)
            .profiles
            .get(&profile.id)
            .map(|p| &p.origin);

        div()
            .id(format!("profile-{}", profile.id))
            .track_focus(&profile.navigation.focus_handle)
            .on_action({
                let profile_id = profile.id.clone();
                cx.listener(move |this, _: &menu::Confirm, window, cx| {
                    this.view_profile(profile_id.clone(), window, cx);
                })
            })
            .child(
                ListItem::new(format!("profile-{}", profile.id))
                    .toggle_state(is_focused)
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .child(Label::new(profile.name.clone()))
                    .when(
                        matches!(origin, Some(ProfileOrigin::Project { .. })),
                        |this| {
                            this.end_slot(
                                Label::new("Project")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Accent),
                            )
                        },
                    )
                    .when(is_focused, |this| {
                        this.end_slot(
                            h_flex()
                                .gap_1()
                                .child(
                                    Label::new("Customize")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(KeyBinding::for_action_in(
                                    &menu::Confirm,
                                    &self.focus_handle,
                                    cx,
                                )),
                        )
                    })
                    .on_click({
                        let profile_id = profile.id.clone();
                        cx.listener(move |this, _, window, cx| {
                            this.view_profile(profile_id.clone(), window, cx);
                        })
                    }),
            )
    }

    fn render_choose_profile(
        &mut self,
        mode: ChooseProfileMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        Navigable::new(
            div()
                .track_focus(&self.focus_handle(cx))
                .size_full()
                .child(ProfileModalHeader::new("Agent Profiles", None))
                .child(
                    v_flex()
                        .pb_1()
                        .child(ListSeparator)
                        .children(
                            mode.builtin_profiles
                                .iter()
                                .map(|profile| self.render_profile(profile, window, cx)),
                        )
                        .when(!mode.custom_profiles.is_empty(), |this| {
                            this.child(ListSeparator)
                                .child(
                                    div().pl_2().pb_1().child(
                                        Label::new("Custom Profiles")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                                )
                                .children(
                                    mode.custom_profiles
                                        .iter()
                                        .map(|profile| self.render_profile(profile, window, cx)),
                                )
                        })
                        .child(ListSeparator)
                        .child(
                            div()
                                .id("new-profile")
                                .track_focus(&mode.add_new_profile.focus_handle)
                                .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                                    this.new_profile(None, window, cx);
                                }))
                                .child(
                                    ListItem::new("new-profile")
                                        .toggle_state(
                                            mode.add_new_profile
                                                .focus_handle
                                                .contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ListItemSpacing::Sparse)
                                        .start_slot(Icon::new(IconName::Plus))
                                        .child(Label::new("Add New Profile"))
                                        .on_click({
                                            cx.listener(move |this, _, window, cx| {
                                                this.new_profile(None, window, cx);
                                            })
                                        }),
                                ),
                        ),
                )
                .into_any_element(),
        )
        .map(|mut navigable| {
            for profile in mode.builtin_profiles {
                navigable = navigable.entry(profile.navigation);
            }
            for profile in mode.custom_profiles {
                navigable = navigable.entry(profile.navigation);
            }

            navigable
        })
        .entry(mode.add_new_profile)
    }

    fn render_new_profile(
        &mut self,
        mode: NewProfileMode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let settings = AgentSettings::get_global(cx);

        let base_profile_name = mode.base_profile_id.as_ref().map(|base_profile_id| {
            settings
                .profiles
                .get(base_profile_id)
                .map(|profile| profile.name.clone())
                .unwrap_or_else(|| "Unknown".into())
        });

        let has_project_origin = mode
            .available_origins
            .iter()
            .any(|origin| matches!(origin, ProfileOrigin::Project { .. }));

        v_flex()
            .id("new-profile")
            .track_focus(&self.focus_handle(cx))
            .child(ProfileModalHeader::new(
                match &base_profile_name {
                    Some(base_profile) => format!("Fork {base_profile}"),
                    None => "New Profile".into(),
                },
                match base_profile_name {
                    Some(_) => Some(IconName::Scissors),
                    None => Some(IconName::Plus),
                },
            ))
            .child(ListSeparator)
            .child(h_flex().p_2().child(mode.name_editor))
            .when(has_project_origin, |this| {
                let is_global = matches!(mode.target_origin, ProfileOrigin::Global);
                let project_origin = mode
                    .available_origins
                    .iter()
                    .find(|o| matches!(o, ProfileOrigin::Project { .. }))
                    .cloned();

                this.child(ListSeparator).child(
                    h_flex()
                        .px_2()
                        .py_1p5()
                        .gap_2()
                        .items_center()
                        .justify_between()
                        .child(
                            Label::new("Save to:")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new("origin-global", "Global")
                                        .style(if is_global {
                                            ButtonStyle::Filled
                                        } else {
                                            ButtonStyle::Subtle
                                        })
                                        .size(ButtonSize::None)
                                        .on_click(cx.listener(|this, _, _window, cx| {
                                            if let Mode::NewProfile(ref mut mode) = this.mode {
                                                mode.target_origin = ProfileOrigin::Global;
                                                cx.notify();
                                            }
                                        })),
                                )
                                .when_some(project_origin, |this, project_origin| {
                                    let is_project =
                                        matches!(mode.target_origin, ProfileOrigin::Project { .. });
                                    this.child(
                                        Button::new("origin-project", "Current Project")
                                            .style(if is_project {
                                                ButtonStyle::Filled
                                            } else {
                                                ButtonStyle::Subtle
                                            })
                                            .size(ButtonSize::None)
                                            .on_click(cx.listener(move |this, _, _window, cx| {
                                                if let Mode::NewProfile(ref mut mode) = this.mode {
                                                    mode.target_origin = project_origin.clone();
                                                    cx.notify();
                                                }
                                            })),
                                    )
                                }),
                        ),
                )
            })
    }

    fn render_view_profile(
        &mut self,
        mode: ViewProfileMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let settings = AgentSettings::get_global(cx);
        let profile = settings.profiles.get(&mode.profile_id);

        let profile_name = profile
            .map(|profile| profile.name.clone())
            .unwrap_or_else(|| "Unknown".into());
        let profile_origin = profile.map(|p| p.origin.clone());

        let icon = match mode.profile_id.as_str() {
            "write" => IconName::Pencil,
            "ask" => IconName::Chat,
            _ => IconName::UserRoundPen,
        };

        Navigable::new(
            div()
                .track_focus(&self.focus_handle(cx))
                .size_full()
                .child(
                    ProfileModalHeader::new(profile_name, Some(icon)).with_origin(profile_origin),
                )
                .child(
                    v_flex()
                        .pb_1()
                        .child(ListSeparator)
                        .child(
                            div()
                                .id("fork-profile")
                                .track_focus(&mode.fork_profile.focus_handle)
                                .on_action({
                                    let profile_id = mode.profile_id.clone();
                                    cx.listener(move |this, _: &menu::Confirm, window, cx| {
                                        this.new_profile(Some(profile_id.clone()), window, cx);
                                    })
                                })
                                .child(
                                    ListItem::new("fork-profile")
                                        .toggle_state(
                                            mode.fork_profile
                                                .focus_handle
                                                .contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::Scissors)
                                                .size(IconSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(Label::new("Fork Profile"))
                                        .on_click({
                                            let profile_id = mode.profile_id.clone();
                                            cx.listener(move |this, _, window, cx| {
                                                this.new_profile(
                                                    Some(profile_id.clone()),
                                                    window,
                                                    cx,
                                                );
                                            })
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .id("configure-description")
                                .track_focus(&mode.configure_description.focus_handle)
                                .on_action({
                                    let profile_id = mode.profile_id.clone();
                                    cx.listener(move |this, _: &menu::Confirm, window, cx| {
                                        this.configure_description(profile_id.clone(), window, cx);
                                    })
                                })
                                .child(
                                    ListItem::new("configure-description-item")
                                        .toggle_state(
                                            mode.configure_description
                                                .focus_handle
                                                .contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::Info)
                                                .size(IconSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(Label::new("Configure Description"))
                                        .on_click({
                                            let profile_id = mode.profile_id.clone();
                                            cx.listener(move |this, _, window, cx| {
                                                this.configure_description(
                                                    profile_id.clone(),
                                                    window,
                                                    cx,
                                                );
                                            })
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .id("configure-custom-prompt")
                                .track_focus(&mode.configure_custom_prompt.focus_handle)
                                .on_action({
                                    let profile_id = mode.profile_id.clone();
                                    cx.listener(move |this, _: &menu::Confirm, window, cx| {
                                        this.configure_custom_prompt(
                                            profile_id.clone(),
                                            window,
                                            cx,
                                        );
                                    })
                                })
                                .child(
                                    ListItem::new("configure-custom-prompt-item")
                                        .toggle_state(
                                            mode.configure_custom_prompt
                                                .focus_handle
                                                .contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::Quote)
                                                .size(IconSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(Label::new("Configure Custom Prompt"))
                                        .on_click({
                                            let profile_id = mode.profile_id.clone();
                                            cx.listener(move |this, _, window, cx| {
                                                this.configure_custom_prompt(
                                                    profile_id.clone(),
                                                    window,
                                                    cx,
                                                );
                                            })
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .id("configure-default-model")
                                .track_focus(&mode.configure_default_model.focus_handle)
                                .on_action({
                                    let profile_id = mode.profile_id.clone();
                                    cx.listener(move |this, _: &menu::Confirm, window, cx| {
                                        this.configure_default_model(
                                            profile_id.clone(),
                                            window,
                                            cx,
                                        );
                                    })
                                })
                                .child(
                                    ListItem::new("model-item")
                                        .toggle_state(
                                            mode.configure_default_model
                                                .focus_handle
                                                .contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::ZedAssistant)
                                                .size(IconSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(Label::new("Configure Default Model"))
                                        .on_click({
                                            let profile_id = mode.profile_id.clone();
                                            cx.listener(move |this, _, window, cx| {
                                                this.configure_default_model(
                                                    profile_id.clone(),
                                                    window,
                                                    cx,
                                                );
                                            })
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .id("configure-builtin-tools")
                                .track_focus(&mode.configure_tools.focus_handle)
                                .on_action({
                                    let profile_id = mode.profile_id.clone();
                                    cx.listener(move |this, _: &menu::Confirm, window, cx| {
                                        this.configure_builtin_tools(
                                            profile_id.clone(),
                                            window,
                                            cx,
                                        );
                                    })
                                })
                                .child(
                                    ListItem::new("configure-builtin-tools-item")
                                        .toggle_state(
                                            mode.configure_tools
                                                .focus_handle
                                                .contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::Settings)
                                                .size(IconSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(Label::new("Configure Built-in Tools"))
                                        .on_click({
                                            let profile_id = mode.profile_id.clone();
                                            cx.listener(move |this, _, window, cx| {
                                                this.configure_builtin_tools(
                                                    profile_id.clone(),
                                                    window,
                                                    cx,
                                                );
                                            })
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .id("configure-mcps")
                                .track_focus(&mode.configure_mcps.focus_handle)
                                .on_action({
                                    let profile_id = mode.profile_id.clone();
                                    cx.listener(move |this, _: &menu::Confirm, window, cx| {
                                        this.configure_mcp_tools(profile_id.clone(), window, cx);
                                    })
                                })
                                .child(
                                    ListItem::new("configure-mcp-tools")
                                        .toggle_state(
                                            mode.configure_mcps
                                                .focus_handle
                                                .contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::ToolHammer)
                                                .size(IconSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(Label::new("Configure MCP Tools"))
                                        .on_click({
                                            let profile_id = mode.profile_id.clone();
                                            cx.listener(move |this, _, window, cx| {
                                                this.configure_mcp_tools(
                                                    profile_id.clone(),
                                                    window,
                                                    cx,
                                                );
                                            })
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .id("configure-delegation")
                                .track_focus(&mode.configure_delegation.focus_handle)
                                .on_action({
                                    let profile_id = mode.profile_id.clone();
                                    cx.listener(move |this, _: &menu::Confirm, window, cx| {
                                        this.configure_delegation(profile_id.clone(), window, cx);
                                    })
                                })
                                .child(
                                    ListItem::new("configure-delegation-item")
                                        .toggle_state(
                                            mode.configure_delegation
                                                .focus_handle
                                                .contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::UserGroup)
                                                .size(IconSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(Label::new("Configure Delegation"))
                                        .on_click({
                                            let profile_id = mode.profile_id.clone();
                                            cx.listener(move |this, _, window, cx| {
                                                this.configure_delegation(
                                                    profile_id.clone(),
                                                    window,
                                                    cx,
                                                );
                                            })
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .id("configure-skills")
                                .track_focus(&mode.configure_skills.focus_handle)
                                .on_action({
                                    let profile_id = mode.profile_id.clone();
                                    cx.listener(move |this, _: &menu::Confirm, window, cx| {
                                        this.configure_skills(profile_id.clone(), window, cx);
                                    })
                                })
                                .child(
                                    ListItem::new("configure-skills-item")
                                        .toggle_state(
                                            mode.configure_skills
                                                .focus_handle
                                                .contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::Sparkle)
                                                .size(IconSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(Label::new("Configure Skills"))
                                        .on_click({
                                            let profile_id = mode.profile_id.clone();
                                            cx.listener(move |this, _, window, cx| {
                                                this.configure_skills(
                                                    profile_id.clone(),
                                                    window,
                                                    cx,
                                                );
                                            })
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .id("delete-profile")
                                .track_focus(&mode.delete_profile.focus_handle)
                                .on_action({
                                    let profile_id = mode.profile_id.clone();
                                    cx.listener(move |this, _: &menu::Confirm, window, cx| {
                                        this.delete_profile(profile_id.clone(), window, cx);
                                    })
                                })
                                .child(
                                    ListItem::new("delete-profile")
                                        .toggle_state(
                                            mode.delete_profile
                                                .focus_handle
                                                .contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::Trash)
                                                .size(IconSize::Small)
                                                .color(Color::Error),
                                        )
                                        .child(Label::new("Delete Profile").color(Color::Error))
                                        .disabled(builtin_profiles::is_builtin(&mode.profile_id))
                                        .on_click({
                                            let profile_id = mode.profile_id.clone();
                                            cx.listener(move |this, _, window, cx| {
                                                this.delete_profile(profile_id.clone(), window, cx);
                                            })
                                        }),
                                ),
                        )
                        .child(ListSeparator)
                        .child(
                            div()
                                .id("cancel-item")
                                .track_focus(&mode.cancel_item.focus_handle)
                                .on_action({
                                    cx.listener(move |this, _: &menu::Confirm, window, cx| {
                                        this.cancel(window, cx);
                                    })
                                })
                                .child(
                                    ListItem::new("cancel-item")
                                        .toggle_state(
                                            mode.cancel_item
                                                .focus_handle
                                                .contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::ArrowLeft)
                                                .size(IconSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(Label::new("Go Back"))
                                        .end_slot(
                                            div().child(
                                                KeyBinding::for_action_in(
                                                    &menu::Cancel,
                                                    &self.focus_handle,
                                                    cx,
                                                )
                                                .size(rems_from_px(12_f32)),
                                            ),
                                        )
                                        .on_click({
                                            cx.listener(move |this, _, window, cx| {
                                                this.cancel(window, cx);
                                            })
                                        }),
                                ),
                        ),
                )
                .into_any_element(),
        )
        .entry(mode.fork_profile)
        .entry(mode.configure_description)
        .entry(mode.configure_custom_prompt)
        .entry(mode.configure_default_model)
        .entry(mode.configure_tools)
        .entry(mode.configure_mcps)
        .entry(mode.configure_delegation)
        .entry(mode.configure_skills)
        .entry(mode.delete_profile)
        .entry(mode.cancel_item)
    }
}

impl Render for ManageProfilesModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = AgentSettings::get_global(cx);

        let go_back_item = div()
            .id("cancel-item")
            .track_focus(&self.focus_handle)
            .on_action({
                cx.listener(move |this, _: &menu::Confirm, window, cx| {
                    this.cancel(window, cx);
                })
            })
            .child(
                ListItem::new("cancel-item")
                    .toggle_state(self.focus_handle.contains_focused(window, cx))
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .start_slot(
                        Icon::new(IconName::ArrowLeft)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new("Go Back"))
                    .end_slot(
                        div().child(
                            KeyBinding::for_action_in(&menu::Cancel, &self.focus_handle, cx)
                                .size(rems_from_px(12_f32)),
                        ),
                    )
                    .on_click({
                        cx.listener(move |this, _, window, cx| {
                            this.cancel(window, cx);
                        })
                    }),
            );

        div()
            .elevation_3(cx)
            .w(rems(34.))
            .key_context("ManageProfilesModal")
            .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| this.cancel(window, cx)))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.confirm(window, cx)))
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                this.focus_handle(cx).focus(window, cx);
            }))
            .on_mouse_down_out(cx.listener(|_this, _, _, cx| cx.emit(DismissEvent)))
            .child(
                // Any mode's list can outgrow the screen; keep the modal
                // content bounded and scrollable.
                div()
                    .id("manage-profiles-content")
                    .max_h(rems(36.))
                    .overflow_y_scroll()
                    .child(match &self.mode {
                        Mode::ChooseProfile(mode) => self
                            .render_choose_profile(mode.clone(), window, cx)
                            .into_any_element(),
                        Mode::NewProfile(mode) => self
                            .render_new_profile(mode.clone(), window, cx)
                            .into_any_element(),
                        Mode::ViewProfile(mode) => self
                            .render_view_profile(mode.clone(), window, cx)
                            .into_any_element(),
                        Mode::ConfigureTools {
                            profile_id,
                            tool_picker,
                            _subscription: _,
                        } => {
                            let profile = settings.profiles.get(profile_id);
                            let profile_name = profile
                                .map(|profile| profile.name.clone())
                                .unwrap_or_else(|| "Unknown".into());
                            let profile_origin = profile.map(|p| p.origin.clone());

                            v_flex()
                                .pb_1()
                                .child(
                                    ProfileModalHeader::new(
                                        format!("{profile_name} — Configure Built-in Tools"),
                                        Some(IconName::Settings),
                                    )
                                    .with_origin(profile_origin),
                                )
                                .child(ListSeparator)
                                .child(tool_picker.clone())
                                .child(ListSeparator)
                                .child(go_back_item)
                                .into_any_element()
                        }
                        Mode::ConfigureDefaultModel {
                            profile_id,
                            model_picker,
                            _subscription: _,
                        } => {
                            let profile = settings.profiles.get(profile_id);
                            let profile_name = profile
                                .map(|profile| profile.name.clone())
                                .unwrap_or_else(|| "Unknown".into());
                            let profile_origin = profile.map(|p| p.origin.clone());

                            v_flex()
                                .pb_1()
                                .child(
                                    ProfileModalHeader::new(
                                        format!("{profile_name} — Configure Default Model"),
                                        Some(IconName::ZedAgent),
                                    )
                                    .with_origin(profile_origin),
                                )
                                .child(ListSeparator)
                                .child(v_flex().w(rems(34.)).child(model_picker.clone()))
                                .child(ListSeparator)
                                .child(go_back_item)
                                .into_any_element()
                        }
                        Mode::ConfigureMcps {
                            profile_id,
                            tool_picker,
                            _subscription: _,
                        } => {
                            let profile = settings.profiles.get(profile_id);
                            let profile_name = profile
                                .map(|profile| profile.name.clone())
                                .unwrap_or_else(|| "Unknown".into());
                            let profile_origin = profile.map(|p| p.origin.clone());

                            v_flex()
                                .pb_1()
                                .child(
                                    ProfileModalHeader::new(
                                        format!("{profile_name} — Configure MCP Tools"),
                                        Some(IconName::ToolHammer),
                                    )
                                    .with_origin(profile_origin),
                                )
                                .child(ListSeparator)
                                .child(tool_picker.clone())
                                .child(ListSeparator)
                                .child(go_back_item)
                                .into_any_element()
                        }
                        Mode::ConfigureDelegation {
                            profile_id,
                            delegation_editor,
                            _subscription: _,
                        } => {
                            let profile = settings.profiles.get(profile_id);
                            let profile_name = profile
                                .map(|profile| profile.name.clone())
                                .unwrap_or_else(|| "Unknown".into());
                            let profile_origin = profile.map(|p| p.origin.clone());

                            v_flex()
                                .pb_1()
                                .child(
                                    ProfileModalHeader::new(
                                        format!("{profile_name} — Configure Delegation"),
                                        Some(IconName::UserGroup),
                                    )
                                    .with_origin(profile_origin),
                                )
                                .child(ListSeparator)
                                .child(delegation_editor.clone())
                                .child(ListSeparator)
                                .child(go_back_item)
                                .into_any_element()
                        }
                        Mode::ConfigureSkills {
                            profile_id,
                            skills_editor,
                        } => {
                            let profile = settings.profiles.get(profile_id);
                            let profile_name = profile
                                .map(|profile| profile.name.clone())
                                .unwrap_or_else(|| "Unknown".into());
                            let profile_origin = profile.map(|p| p.origin.clone());

                            v_flex()
                                .pb_1()
                                .child(
                                    ProfileModalHeader::new(
                                        format!("{profile_name} — Configure Skills"),
                                        Some(IconName::Sparkle),
                                    )
                                    .with_origin(profile_origin),
                                )
                                .child(ListSeparator)
                                .child(skills_editor.clone())
                                .child(ListSeparator)
                                .child(go_back_item)
                                .into_any_element()
                        }
                        Mode::ConfigureCustomPrompt {
                            profile_id,
                            prompt_editor,
                            _subscription: _,
                        } => {
                            let profile = settings.profiles.get(profile_id);
                            let profile_name = profile
                                .map(|profile| profile.name.clone())
                                .unwrap_or_else(|| "Unknown".into());
                            let profile_origin = profile.map(|p| p.origin.clone());

                            v_flex()
                                .pb_1()
                                .child(
                                    ProfileModalHeader::new(
                                        format!("{profile_name} — Configure Custom Prompt"),
                                        Some(IconName::Quote),
                                    )
                                    .with_origin(profile_origin),
                                )
                                .child(ListSeparator)
                                .child(
                                    v_flex()
                                        .p_2()
                                        .gap_1()
                                        .child(
                                            Label::new(
                                                "Custom system instructions injected for this profile",
                                            )
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                        )
                                        .child(
                                            div()
                                                .p_1()
                                                .border_1()
                                                .border_color(cx.theme().colors().border)
                                                .rounded_md()
                                                .child(prompt_editor.clone()),
                                        ),
                                )
                                .child(ListSeparator)
                                .child(go_back_item)
                                .into_any_element()
                        }
                        Mode::ConfigureDescription {
                            profile_id,
                            description_editor,
                            _subscription: _,
                        } => {
                            let profile = settings.profiles.get(profile_id);
                            let profile_name = profile
                                .map(|profile| profile.name.clone())
                                .unwrap_or_else(|| "Unknown".into());
                            let profile_origin = profile.map(|p| p.origin.clone());

                            v_flex()
                                .pb_1()
                                .child(
                                    ProfileModalHeader::new(
                                        format!("{profile_name} — Configure Description"),
                                        Some(IconName::Info),
                                    )
                                    .with_origin(profile_origin),
                                )
                                .child(ListSeparator)
                                .child(
                                    v_flex()
                                        .p_2()
                                        .gap_1()
                                        .child(
                                            Label::new(
                                                "Short description shown to parent agents in the delegation catalog",
                                            )
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                        )
                                        .child(
                                            div()
                                                .p_1()
                                                .border_1()
                                                .border_color(cx.theme().colors().border)
                                                .rounded_md()
                                                .child(description_editor.clone()),
                                        ),
                                )
                                .child(ListSeparator)
                                .child(go_back_item)
                                .into_any_element()
                        }
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::UpdateGlobal as _;
    use settings::{LocalSettingsKind, LocalSettingsPath, WorktreeId};

    #[gpui::test]
    fn test_save_profile_change_routes_to_correct_origin(cx: &mut gpui::App) {
        let fs = FakeFs::new(cx.background_executor().clone());
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);

        // 1. Setup user settings with a global profile
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(
                    r#"{ "agent": { "profiles": { "global_agent": { "name": "Global Agent" } } } }"#,
                    cx,
                )
                .unwrap();
        });

        // 2. Setup local project settings with a project profile
        let root = std::sync::Arc::from(util::rel_path::RelPath::from_unix_str("").unwrap());
        let worktree_id = WorktreeId::from_usize(1);
        let project_path: Arc<util::rel_path::RelPath> = std::sync::Arc::from(
            util::rel_path::RelPath::from_unix_str(".zed/settings.json").unwrap(),
        );

        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_local_settings(
                    worktree_id,
                    LocalSettingsPath::InWorktree(root),
                    LocalSettingsKind::Settings,
                    Some(
                        r#"{
                            "agent": {
                                "profiles": {
                                    "project_agent": { "name": "Project Agent" }
                                }
                            }
                        }"#,
                    ),
                    cx,
                )
                .unwrap();
        });

        let settings = AgentSettings::get_global(cx);
        let global_profile = settings
            .profiles
            .get(&AgentProfileId("global_agent".into()))
            .unwrap();
        assert_eq!(global_profile.origin, ProfileOrigin::Global);

        let project_profile = settings
            .profiles
            .get(&AgentProfileId("project_agent".into()))
            .unwrap();
        assert_eq!(
            project_profile.origin,
            ProfileOrigin::Project {
                worktree_id,
                path: project_path,
            }
        );

        // 3. Test saving change to global profile
        ManageProfilesModal::save_profile_change_by_origin(
            fs.clone(),
            &global_profile.origin,
            cx,
            |settings, _cx| {
                let agent = settings.agent.get_or_insert_default();
                let profiles = agent.profiles.get_or_insert_default();
                if let Some(profile) = profiles.get_mut("global_agent") {
                    profile.custom_prompt = Some(Arc::from("Global Custom Prompt"));
                }
            },
        );

        // 4. Test saving change to project profile
        ManageProfilesModal::save_profile_change_by_origin(
            fs.clone(),
            &project_profile.origin,
            cx,
            |settings, _cx| {
                let agent = settings.agent.get_or_insert_default();
                let profiles = agent.profiles.get_or_insert_default();
                if let Some(profile) = profiles.get_mut("project_agent") {
                    profile.custom_prompt = Some(Arc::from("Project Custom Prompt"));
                }
            },
        );

        // 5. Test deleting project profile
        ManageProfilesModal::save_profile_change_by_origin(
            fs,
            &project_profile.origin,
            cx,
            |settings, _cx| {
                if let Some(agent) = settings.agent.as_mut() {
                    if let Some(profiles) = agent.profiles.as_mut() {
                        profiles.shift_remove("project_agent");
                    }
                }
            },
        );
    }
}
