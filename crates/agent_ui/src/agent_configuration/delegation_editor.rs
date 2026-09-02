use std::sync::Arc;

use agent_settings::{AgentProfileId, AgentSettings};
use fs::Fs;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Subscription, Task, WeakEntity, Window, prelude::*,
};
use picker::{Picker, PickerDelegate};
use settings::{Settings as _, SettingsStore, update_settings_file};
use ui::{
    Color, Icon, IconName, IconSize, Label, LabelSize, ListItem, ListItemSpacing, prelude::*,
};
use util::ResultExt as _;

const MIN_MAX_DEPTH: u32 = 1;
const MAX_MAX_DEPTH: u32 = 5;

/// Edits a profile's delegation settings: which agents it may spawn
/// (`delegation.allowed`) and how deeply they may nest (`max_depth`).
///
/// Every change is persisted to the settings file immediately, matching how
/// the tool picker toggles tools.
pub struct DelegationEditor {
    picker: Entity<Picker<DelegationPickerDelegate>>,
}

impl DelegationEditor {
    pub fn new(
        profile_id: AgentProfileId,
        fs: Arc<dyn Fs>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = DelegationPickerDelegate::new(cx.entity().downgrade(), profile_id, fs, cx);
        let picker = cx.new(|cx| Picker::list(delegate, window, cx).embedded());
        Self { picker }
    }
}

impl EventEmitter<DismissEvent> for DelegationEditor {}

impl Focusable for DelegationEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for DelegationEditor {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().child(self.picker.clone())
    }
}

#[derive(Debug, Clone)]
pub enum DelegationPickerItem {
    MaxDepth,
    Profile {
        id: AgentProfileId,
        name: SharedString,
        description: Option<SharedString>,
    },
}

pub struct DelegationPickerDelegate {
    delegation_editor: WeakEntity<DelegationEditor>,
    fs: Arc<dyn Fs>,
    profile_id: AgentProfileId,
    all_profiles: Vec<(AgentProfileId, SharedString, Option<SharedString>)>,
    filtered_items: Vec<DelegationPickerItem>,
    selected_index: usize,
    _settings_subscription: Subscription,
}

impl DelegationPickerDelegate {
    fn new(
        delegation_editor: WeakEntity<DelegationEditor>,
        profile_id: AgentProfileId,
        fs: Arc<dyn Fs>,
        cx: &mut Context<DelegationEditor>,
    ) -> Self {
        let settings = AgentSettings::get_global(cx);
        let mut all_profiles = Vec::new();
        for (id, profile) in settings.profiles.iter() {
            if id != &profile_id {
                all_profiles.push((
                    id.clone(),
                    profile.name.clone(),
                    profile.description.clone(),
                ));
            }
        }

        let initial_items = Self::filter_items(&all_profiles, "");

        let settings_subscription = cx.observe_global::<SettingsStore>(|_this, cx| {
            cx.notify();
        });

        Self {
            delegation_editor,
            fs,
            profile_id,
            all_profiles,
            filtered_items: initial_items,
            selected_index: 0,
            _settings_subscription: settings_subscription,
        }
    }

    fn filter_items(
        all_profiles: &[(AgentProfileId, SharedString, Option<SharedString>)],
        query: &str,
    ) -> Vec<DelegationPickerItem> {
        let query = query.trim().to_lowercase();
        let mut items = Vec::new();
        let matches_depth = query.is_empty()
            || "max depth".contains(&query)
            || "depth".contains(&query)
            || "max".contains(&query);
        if matches_depth {
            items.push(DelegationPickerItem::MaxDepth);
        }

        for (id, name, description) in all_profiles {
            let matches_name = name.to_lowercase().contains(&query);
            let matches_id = id.as_str().to_lowercase().contains(&query);
            let matches_desc = description
                .as_ref()
                .is_some_and(|d| d.to_lowercase().contains(&query));

            if query.is_empty() || matches_name || matches_id || matches_desc {
                items.push(DelegationPickerItem::Profile {
                    id: id.clone(),
                    name: name.clone(),
                    description: description.clone(),
                });
            }
        }

        items
    }

    fn toggle_allowed(&mut self, target: AgentProfileId, cx: &mut App) {
        let profile = AgentSettings::get_global(cx)
            .profiles
            .get(&self.profile_id)
            .cloned();
        let currently_allowed = profile
            .as_ref()
            .and_then(|profile| profile.delegation.as_ref())
            .is_some_and(|delegation| delegation.allowed.contains(&target));
        let origin = profile.map(|p| p.origin).unwrap_or_default();

        let fs = self.fs.clone();
        let profile_id = self.profile_id.clone();
        let update_fn = move |settings: &mut settings::SettingsContent, _cx: &App| {
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
        };

        match origin {
            agent_settings::ProfileOrigin::Global => {
                update_settings_file(fs, cx, update_fn);
            }
            agent_settings::ProfileOrigin::Project { worktree_id, path } => {
                settings::update_project_settings_file(fs, worktree_id, path, cx, update_fn);
            }
        }
    }

    fn cycle_max_depth(&mut self, cx: &mut App) {
        let profile = AgentSettings::get_global(cx)
            .profiles
            .get(&self.profile_id)
            .cloned();
        let current = profile
            .as_ref()
            .and_then(|profile| profile.delegation.as_ref())
            .map(|delegation| u32::from(delegation.max_depth))
            .unwrap_or(MIN_MAX_DEPTH);
        let origin = profile.map(|p| p.origin).unwrap_or_default();
        let next = current % MAX_MAX_DEPTH + MIN_MAX_DEPTH;

        let fs = self.fs.clone();
        let profile_id = self.profile_id.clone();
        let update_fn = move |settings: &mut settings::SettingsContent, _cx: &App| {
            let Some(profile) = settings
                .agent
                .get_or_insert_default()
                .profiles
                .get_or_insert_default()
                .get_mut(profile_id.0.as_ref())
            else {
                return;
            };
            profile.delegation.get_or_insert_default().max_depth = Some(next);
        };

        match origin {
            agent_settings::ProfileOrigin::Global => {
                update_settings_file(fs, cx, update_fn);
            }
            agent_settings::ProfileOrigin::Project { worktree_id, path } => {
                settings::update_project_settings_file(fs, worktree_id, path, cx, update_fn);
            }
        }
    }
}

impl PickerDelegate for DelegationPickerDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "delegation picker"
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

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search profiles…".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let all_profiles = self.all_profiles.clone();

        cx.spawn_in(window, async move |this, cx| {
            let filtered_items = cx
                .background_spawn(async move { Self::filter_items(&all_profiles, &query) })
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

        let Some(item) = self.filtered_items.get(self.selected_index).cloned() else {
            return;
        };

        match item {
            DelegationPickerItem::MaxDepth => {
                self.cycle_max_depth(cx);
            }
            DelegationPickerItem::Profile { id, .. } => {
                self.toggle_allowed(id, cx);
            }
        }
        cx.notify();
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.delegation_editor
            .update(cx, |_this, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let item = self.filtered_items.get(ix)?;
        let settings = AgentSettings::get_global(cx);
        let delegation = settings
            .profiles
            .get(&self.profile_id)
            .and_then(|profile| profile.delegation.as_ref());

        match item {
            DelegationPickerItem::MaxDepth => {
                let current_depth = delegation
                    .map(|d| u32::from(d.max_depth))
                    .unwrap_or(MIN_MAX_DEPTH);
                Some(
                    ListItem::new(ix)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .toggle_state(selected)
                        .child(Label::new(format!("Max Depth: {current_depth}")))
                        .end_slot(
                            Label::new("click to cycle 1-5")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .into_any_element(),
                )
            }
            DelegationPickerItem::Profile {
                id,
                name,
                description,
            } => {
                let is_allowed =
                    delegation.is_some_and(|delegation| delegation.allowed.contains(id));

                Some(
                    ListItem::new(ix)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .toggle_state(selected)
                        .child(
                            v_flex()
                                .child(Label::new(name.clone()))
                                .child(
                                    Label::new(format!("id: {}", id.as_str()))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .when_some(description.clone(), |this, description| {
                                    this.child(
                                        Label::new(description)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                }),
                        )
                        .end_slot::<Icon>(is_allowed.then(|| {
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
