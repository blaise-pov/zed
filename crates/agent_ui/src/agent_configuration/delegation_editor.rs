use std::sync::Arc;

use agent_settings::{AgentProfileId, AgentSettings};
use editor::Editor;
use fs::Fs;
use gpui::{App, Context, Entity, FocusHandle, Focusable, Render, SharedString, Subscription, Window, prelude::*};
use settings::{Settings as _, SettingsStore, update_settings_file};
use ui::{
    Icon, IconName, IconSize, Label, LabelSize, ListItem, ListItemSpacing, Color, prelude::*,
};

const MIN_MAX_DEPTH: u32 = 1;
const MAX_MAX_DEPTH: u32 = 5;

/// Edits a profile's delegation settings: which agents it may spawn
/// (`delegation.allowed`), how deeply they may nest (`max_depth`), and the
/// description shown to the parent agent in the delegation catalog.
///
/// Every change is persisted to the settings file immediately, matching how
/// the tool picker toggles tools.
pub struct DelegationEditor {
    profile_id: AgentProfileId,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    description_editor: Entity<Editor>,
    _settings_subscription: Subscription,
}

impl DelegationEditor {
    pub fn new(
        profile_id: AgentProfileId,
        fs: Arc<dyn Fs>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let description = AgentSettings::get_global(cx)
            .profiles
            .get(&profile_id)
            .and_then(|profile| profile.description.clone())
            .unwrap_or_default();

        let description_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(
                "Description shown to the parent agent in the delegation catalog",
                window,
                cx,
            );
            if !description.is_empty() {
                editor.set_text(description.to_string(), window, cx);
            }
            editor
        });

        cx.subscribe(&description_editor, |this, _, event, cx| {
            if matches!(event, editor::EditorEvent::Blurred) {
                this.save_description(cx);
            }
        })
        .detach();

        let settings_subscription = cx.observe_global::<SettingsStore>(|_this, cx| {
            // Toggles write through the settings store; re-render from it.
            cx.notify();
        });

        Self {
            profile_id,
            fs,
            focus_handle: cx.focus_handle(),
            description_editor,
            _settings_subscription: settings_subscription,
        }
    }

    fn save_description(&mut self, cx: &mut Context<Self>) {
        let text = self.description_editor.read(cx).text(cx);
        let text = text.trim().to_string();
        let fs = self.fs.clone();
        let profile_id = self.profile_id.clone();
        update_settings_file(fs, cx, move |settings, _cx| {
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

    fn toggle_allowed(&mut self, target: AgentProfileId, cx: &mut Context<Self>) {
        let currently_allowed = AgentSettings::get_global(cx)
            .profiles
            .get(&self.profile_id)
            .and_then(|profile| profile.delegation.as_ref())
            .is_some_and(|delegation| delegation.allowed.contains(&target));

        let fs = self.fs.clone();
        let profile_id = self.profile_id.clone();
        update_settings_file(fs, cx, move |settings, _cx| {
            let Some(profile) = settings
                .agent
                .get_or_insert_default()
                .profiles
                .get_or_insert_default()
                .get_mut(profile_id.0.as_ref())
            else {
                return;
            };
            if currently_allowed {
                if let Some(delegation) = profile.delegation.as_mut() {
                    delegation
                        .allowed
                        .retain(|allowed| allowed.as_ref() != target.as_str());
                }
                // An empty `allowed` list is a configuration error; removing
                // the last entry disables delegation entirely.
                if profile
                    .delegation
                    .as_ref()
                    .is_some_and(|delegation| delegation.allowed.is_empty())
                {
                    profile.delegation = None;
                }
            } else {
                profile
                    .delegation
                    .get_or_insert_default()
                    .allowed
                    .push(Arc::from(target.as_str()));
            }
        });
        cx.notify();
    }

    fn cycle_max_depth(&mut self, cx: &mut Context<Self>) {
        let current = AgentSettings::get_global(cx)
            .profiles
            .get(&self.profile_id)
            .and_then(|profile| profile.delegation.as_ref())
            .map(|delegation| u32::from(delegation.max_depth))
            .unwrap_or(MIN_MAX_DEPTH);
        let next = current % MAX_MAX_DEPTH + MIN_MAX_DEPTH;

        let fs = self.fs.clone();
        let profile_id = self.profile_id.clone();
        update_settings_file(fs, cx, move |settings, _cx| {
            let Some(profile) = settings
                .agent
                .get_or_insert_default()
                .profiles
                .get_or_insert_default()
                .get_mut(profile_id.0.as_ref())
            else {
                return;
            };
            profile
                .delegation
                .get_or_insert_default()
                .max_depth = Some(next);
        });
        cx.notify();
    }

    fn render_profiles_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = AgentSettings::get_global(cx);
        let delegation = settings
            .profiles
            .get(&self.profile_id)
            .and_then(|profile| profile.delegation.clone());
        let max_depth = delegation.as_ref().map(|delegation| delegation.max_depth);

        let mut list = v_flex().child(
            ListItem::new("description")
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .child(
                    v_flex()
                        .child(Label::new("Description").color(Color::Muted))
                        .child(self.description_editor.clone()),
                ),
        );

        if let Some(max_depth) = max_depth {
            list = list.child(
                ListItem::new("max-depth")
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .child(Label::new(format!("Max Depth: {max_depth}")))
                    .end_slot(
                        Label::new("click to cycle 1-5")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.cycle_max_depth(cx);
                    })),
            );
        }

        for (id, profile) in settings.profiles.iter() {
            // Delegating to itself would be a self-cycle.
            if id == &self.profile_id {
                continue;
            }
            let target = id.clone();
            let is_allowed = delegation
                .as_ref()
                .is_some_and(|delegation| delegation.allowed.contains(id));
            list = list.child(
                ListItem::new(SharedString::from(id.as_str()))
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .child(
                        v_flex()
                            .child(Label::new(profile.name.clone()))
                            .when_some(
                                profile.description.clone(),
                                |this, description| {
                                    this.child(
                                        Label::new(format!("id: {}", id.as_str()))
                                            .size(ui::LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        Label::new(description)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                },
                            ),
                    )
                    .end_slot::<Icon>(is_allowed.then(|| {
                        Icon::new(IconName::Check)
                            .size(IconSize::Small)
                            .color(Color::Success)
                    }))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.toggle_allowed(target.clone(), cx);
                    })),
            );
        }

        list
    }
}

impl Focusable for DelegationEditor {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DelegationEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .track_focus(&self.focus_handle(cx))
            .child(self.render_profiles_list(cx))
    }
}
