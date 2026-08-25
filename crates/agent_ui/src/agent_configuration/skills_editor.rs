use std::sync::Arc;

use agent_skills::{SkillIndex, SkillSource};
use agent_settings::{AgentProfileId, AgentSettings};
use fs::Fs;
use gpui::{App, Context, FocusHandle, Focusable, Render, SharedString, Subscription, Window, prelude::*};
use settings::{Settings as _, SettingsStore, update_settings_file};
use ui::{
    Icon, IconName, IconSize, Label, LabelSize, ListItem, ListItemSpacing, ListSeparator, Color,
    prelude::*,
};

/// Edits a profile's `skills` filter: which skills are visible to sessions
/// using the profile. A disabled filter (the default) leaves all skills
/// visible; enabling it restricts the model to the checked skills across the
/// system-prompt catalog, `available_skills`, and the `skill` tool.
///
/// Every change is persisted to the settings file immediately, matching how
/// the tool picker toggles tools.
pub struct SkillsEditor {
    profile_id: AgentProfileId,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    _settings_subscription: Subscription,
}

impl SkillsEditor {
    pub fn new(
        profile_id: AgentProfileId,
        fs: Arc<dyn Fs>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings_subscription = cx.observe_global::<SettingsStore>(|_this, cx| {
            cx.notify();
        });

        Self {
            profile_id,
            fs,
            focus_handle: cx.focus_handle(),
            _settings_subscription: settings_subscription,
        }
    }

    fn restricted_skill_names(&self, cx: &App) -> Option<Vec<Arc<str>>> {
        AgentSettings::get_global(cx)
            .profiles
            .get(&self.profile_id)
            .and_then(|profile| profile.skills.clone())
    }

    fn set_restriction_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let new_filter = if enabled {
            // Start from all currently known skill names so enabling the
            // filter doesn't immediately hide everything.
            let names = collect_skill_names(cx);
            (!names.is_empty()).then(|| names)
        } else {
            None
        };
        self.write_filter(new_filter, cx);
    }

    fn toggle_skill(&mut self, name: Arc<str>, cx: &mut Context<Self>) {
        let mut names = self.restricted_skill_names(cx).unwrap_or_default();
        match names.iter().position(|existing| **existing == *name) {
            Some(ix) => {
                names.remove(ix);
            }
            None => names.push(name),
        }
        // Toggling the last skill off disables the filter entirely: an empty
        // `skills` list would mean "no skills visible", which is better
        // expressed (and reset) by removing the filter.
        self.write_filter((!names.is_empty()).then_some(names), cx);
    }

    fn write_filter(&mut self, filter: Option<Vec<Arc<str>>>, cx: &mut Context<Self>) {
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
            profile.skills = filter;
        });
        cx.notify();
    }

    fn render_skills_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let restriction = self.restricted_skill_names(cx);
        let filter_enabled = restriction.is_some();

        let mut list = v_flex().child(
            ListItem::new("restrict-skills")
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .child(
                    v_flex()
                        .child(Label::new("Restrict Skills"))
                        .child(
                            Label::new(
                                "When enabled, only the checked skills are visible to this profile",
                            )
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                        ),
                )
                .end_slot::<Icon>(filter_enabled.then(|| {
                    Icon::new(IconName::Check)
                        .size(IconSize::Small)
                        .color(Color::Success)
                }))
                .on_click({
                    let enabled = filter_enabled;
                    cx.listener(move |this, _, _window, cx| {
                        this.set_restriction_enabled(!enabled, cx);
                    })
                }),
        );

        if let Some(restriction) = restriction {
            list = list.child(ListSeparator);
            for (name, source) in collect_skills_with_source(cx) {
                let is_allowed = restriction.iter().any(|allowed| **allowed == *name);
                let skill_name = name.clone();
                list = list.child(
                    ListItem::new(SharedString::from(name.as_ref()))
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .child(
                            v_flex()
                                .child(Label::new(name.clone()))
                                .child(
                                    Label::new(source)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        )
                        .end_slot::<Icon>(is_allowed.then(|| {
                            Icon::new(IconName::Check)
                                .size(IconSize::Small)
                                .color(Color::Success)
                        }))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.toggle_skill(skill_name.clone(), cx);
                        })),
                );
            }
        }

        list
    }
}

fn collect_skills_with_source(cx: &App) -> Vec<(Arc<str>, String)> {
    let mut entries: Vec<(Arc<str>, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |name: String, source: String| {
        if seen.insert(name.clone()) {
            entries.push((Arc::from(name.as_str()), source));
        }
    };

    if let Some(index) = cx.try_global::<SkillIndex>() {
        for skill in &index.global_skills {
            push(skill.name.clone(), format!("global — {}", skill.description));
        }
        for group in &index.project_skills {
            for skill in &group.skills {
                push(skill.name.clone(), format!("project — {}", skill.description));
            }
        }
    }
    for skill in agent_skills::builtin_skills() {
        push(skill.name, "built-in".to_string());
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

fn collect_skill_names(cx: &App) -> Vec<Arc<str>> {
    collect_skills_with_source(cx)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// Kept for future use in descriptions; the source label is embedded into
/// the list entry text instead.
#[allow(dead_code)]
fn source_label(source: &SkillSource) -> &'static str {
    match source {
        SkillSource::BuiltIn => "built-in",
        SkillSource::Global => "global",
        SkillSource::ProjectLocal { .. } => "project",
    }
}

impl Focusable for SkillsEditor {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SkillsEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .track_focus(&self.focus_handle(cx))
            .child(self.render_skills_list(cx))
    }
}
