use std::sync::Arc;

use agent_settings::{AgentProfileId, AgentSettings};
use agent_skills::SkillIndex;
use fs::Fs;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, Subscription,
    Task, WeakEntity, Window, prelude::*,
};
use picker::{Picker, PickerDelegate};
use settings::{Settings as _, SettingsStore, update_settings_file};
use ui::{
    Color, Icon, IconName, IconSize, Label, LabelSize, ListItem, ListItemSpacing, prelude::*,
};
use util::ResultExt as _;

/// Edits a profile's `skills` whitelist: which skills are visible and accessible
/// to sessions using this profile.
///
/// Skills must be explicitly allowed. Unchecked skills are completely hidden from
/// the agent (not present in system prompt catalog, available_skills, or skill tool).
pub struct SkillsEditor {
    picker: Entity<Picker<SkillsPickerDelegate>>,
}

impl SkillsEditor {
    pub fn new(
        profile_id: AgentProfileId,
        fs: Arc<dyn Fs>,
        settings_location: Option<settings::SettingsLocation<'static>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = SkillsPickerDelegate::new(
            cx.entity().downgrade(),
            profile_id,
            fs,
            settings_location,
            cx,
        );
        let picker = cx.new(|cx| Picker::list(delegate, window, cx).embedded());
        Self { picker }
    }
}

impl EventEmitter<DismissEvent> for SkillsEditor {}

impl Focusable for SkillsEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for SkillsEditor {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().child(self.picker.clone())
    }
}

#[derive(Clone, Debug)]
pub struct SkillListEntry {
    pub name: Arc<str>,
    pub source_line: String,
    pub is_project: bool,
}

pub struct SkillsPickerDelegate {
    skills_editor: WeakEntity<SkillsEditor>,
    fs: Arc<dyn Fs>,
    profile_id: AgentProfileId,
    settings_location: Option<settings::SettingsLocation<'static>>,
    all_skills: Vec<SkillListEntry>,
    filtered_skills: Vec<SkillListEntry>,
    selected_index: usize,
    _settings_subscription: Subscription,
    _skills_subscription: Subscription,
}

impl SkillsPickerDelegate {
    fn new(
        skills_editor: WeakEntity<SkillsEditor>,
        profile_id: AgentProfileId,
        fs: Arc<dyn Fs>,
        settings_location: Option<settings::SettingsLocation<'static>>,
        cx: &mut Context<SkillsEditor>,
    ) -> Self {
        let all_skills = collect_skills_with_source(cx);
        let filtered_skills = all_skills.clone();

        let settings_subscription = cx.observe_global::<SettingsStore>(|this, cx| {
            this.picker.update(cx, |_picker, cx| {
                cx.notify();
            });
        });
        let skills_subscription = cx.observe_global::<SkillIndex>(|this, cx| {
            this.picker.update(cx, |picker, cx| {
                picker.delegate.all_skills = collect_skills_with_source(cx);
                picker.delegate.filtered_skills =
                    Self::filter_items(&picker.delegate.all_skills, &picker.query(cx));
                picker.delegate.selected_index = picker
                    .delegate
                    .selected_index
                    .min(picker.delegate.filtered_skills.len().saturating_sub(1));
                cx.notify();
            });
        });

        // Proactively scan and load global skills if the index hasn't been
        // populated yet, so the editor lists them even before the agent's
        // own scan runs.
        let scan_fs = fs.clone();
        cx.spawn(async move |this, cx| {
            let global_dir = agent_skills::global_skills_dir();
            let global_skills = agent_skills::load_skills_from_directory(
                &scan_fs,
                &global_dir,
                agent_skills::SkillSource::Global,
            )
            .await
            .into_iter()
            .filter_map(|result| match result {
                Ok(skill) => Some(skill),
                Err(error) => {
                    log::warn!(
                        "Skipping global skill at {}: {}",
                        error.path.display(),
                        error.message
                    );
                    None
                }
            })
            .collect::<Vec<_>>();
            this.update(cx, |_this, cx| {
                let index_needs_seed = cx
                    .try_global::<SkillIndex>()
                    .map(|index| index.global_skills.is_empty())
                    .unwrap_or(true);
                if index_needs_seed && !global_skills.is_empty() {
                    if cx.has_global::<SkillIndex>() {
                        cx.update_global::<SkillIndex, _>(|index, _cx| {
                            index.global_skills = global_skills;
                        });
                    } else {
                        cx.set_global(SkillIndex {
                            global_skills,
                            project_skills: Vec::new(),
                        });
                    }
                    cx.notify();
                }
            })
            .log_err();
        })
        .detach();

        Self {
            skills_editor,
            fs,
            profile_id,
            settings_location,
            all_skills,
            filtered_skills,
            selected_index: 0,
            _settings_subscription: settings_subscription,
            _skills_subscription: skills_subscription,
        }
    }

    fn filter_items(skills: &[SkillListEntry], query: &str) -> Vec<SkillListEntry> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return skills.to_vec();
        }
        skills
            .iter()
            .filter(|skill| {
                skill.name.to_lowercase().contains(&query)
                    || skill.source_line.to_lowercase().contains(&query)
            })
            .cloned()
            .collect()
    }

    fn is_skill_allowed(&self, skill_name: &str, cx: &App) -> bool {
        let Some(profile) = AgentSettings::get(self.settings_location, cx)
            .profiles
            .get(&self.profile_id)
        else {
            return false;
        };
        profile.is_skill_allowed(&self.profile_id, skill_name)
    }

    fn toggle_skill(&mut self, name: Arc<str>, cx: &App) {
        let all_names = collect_skill_names(cx);
        let mut currently_allowed: Vec<Arc<str>> = all_names
            .into_iter()
            .filter(|n| self.is_skill_allowed(n.as_ref(), cx))
            .collect();

        if let Some(ix) = currently_allowed
            .iter()
            .position(|existing| **existing == *name)
        {
            currently_allowed.remove(ix);
        } else {
            currently_allowed.push(name);
        }

        self.write_filter(Some(currently_allowed), cx);
    }

    fn write_filter(&mut self, filter: Option<Vec<Arc<str>>>, cx: &App) {
        let origin = AgentSettings::get(self.settings_location, cx)
            .profiles
            .get(&self.profile_id)
            .map(|p| p.origin.clone())
            .unwrap_or_default();

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
            profile.skills = filter;
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

impl PickerDelegate for SkillsPickerDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "skills picker"
    }

    fn match_count(&self) -> usize {
        self.filtered_skills.len()
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
        "Search skills…".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let all_skills = self.all_skills.clone();

        cx.spawn_in(window, async move |this, cx| {
            let filtered_skills = cx
                .background_spawn(async move { Self::filter_items(&all_skills, &query) })
                .await;

            this.update(cx, |this, _cx| {
                this.delegate.filtered_skills = filtered_skills;
                this.delegate.selected_index = this
                    .delegate
                    .selected_index
                    .min(this.delegate.filtered_skills.len().saturating_sub(1));
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if self.filtered_skills.is_empty() {
            self.dismissed(window, cx);
            return;
        }

        let Some(item) = self.filtered_skills.get(self.selected_index).cloned() else {
            return;
        };

        self.toggle_skill(item.name, cx);
        cx.notify();
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.skills_editor
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
        let entry = self.filtered_skills.get(ix)?;
        let is_allowed = self.is_skill_allowed(entry.name.as_ref(), cx);
        let is_project = entry.is_project;

        let mut list_item = ListItem::new(ix)
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected)
            .child(
                v_flex().child(Label::new(entry.name.clone())).child(
                    Label::new(entry.source_line.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            );

        if is_project || is_allowed {
            list_item = list_item.end_slot(
                h_flex()
                    .gap_2()
                    .items_center()
                    .when(is_project, |this| {
                        this.child(
                            Label::new("Project")
                                .size(LabelSize::XSmall)
                                .color(Color::Accent),
                        )
                    })
                    .when(is_allowed, |this| {
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
}

fn collect_skills_with_source(cx: &App) -> Vec<SkillListEntry> {
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |name: String, source_line: String, is_project: bool| {
        if seen.insert(name.clone()) {
            entries.push(SkillListEntry {
                name: Arc::from(name.as_str()),
                source_line,
                is_project,
            });
        }
    };

    if let Some(index) = cx.try_global::<SkillIndex>() {
        // Project skills take precedence over global skills with identical names.
        for group in &index.project_skills {
            for skill in &group.skills {
                push(skill.name.clone(), skill.description.clone(), true);
            }
        }
        for skill in &index.global_skills {
            push(
                skill.name.clone(),
                format!("global — {}", skill.description),
                false,
            );
        }
    }
    for skill in agent_skills::builtin_skills() {
        push(skill.name, "built-in".to_string(), false);
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

fn collect_skill_names(cx: &App) -> Vec<Arc<str>> {
    collect_skills_with_source(cx)
        .into_iter()
        .map(|entry| entry.name)
        .collect()
}
