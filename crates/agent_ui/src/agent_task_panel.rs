//! Agent Task Panel (TGR integration for Zed)
//!
//! Recommended Agent Profiles Configuration for Task Graph (TGR):
//!
//! ```json
//! {
//!   "agent": {
//!     "profiles": {
//!       "orchestrator": {
//!         "name": "Orchestrator",
//!         "delegation": {
//!           "allowed": ["backend_engineer", "reviewer"]
//!         }
//!       },
//!       "reviewer": {
//!         "name": "Reviewer",
//!         "tools": {
//!           "terminal": false,
//!           "edit_file": false
//!         }
//!       }
//!     }
//!   }
//! }
//! ```

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use agent::{
    AgentTaskArtifact, AgentTaskDetail, AgentTaskEvent, AgentTaskEventKind, AgentTaskId,
    AgentTaskStatus, AgentTaskStore, AgentTaskSummary,
};
use agent_settings::AgentSettings;
use fs::Fs;
use gpui::{
    Action, App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, KeyContext, Pixels,
    Task, WeakEntity, Window, actions, prelude::*,
};
use project::Project;
use settings::{Settings, SettingsStore};
use ui::{Color, Icon, IconButton, IconName, IconSize, Label, LabelSize, Tooltip, prelude::*};
use util::ResultExt;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

actions!(agent_tasks, [ToggleAgentTaskPanel]);

pub const AGENT_TASK_PANEL_KEY: &str = "AgentTaskPanel";

pub fn init(file_system: Arc<dyn Fs>, cx: &mut App) {
    let subscription = cx.observe_new(move |workspace: &mut Workspace, window, cx| {
        let project = workspace.project().clone();
        let context_server_store = project.read(cx).context_server_store();
        let server_id = context_server::ContextServerId(
            AgentSettings::get_for_project(project.read(cx), cx)
                .task_graph_server_id
                .clone()
                .into(),
        );
        let provider = Arc::new(agent::McpAgentTaskProvider::new(
            context_server_store,
            server_id,
        ));
        let store = cx.new(|cx| agent::AgentTaskStore::new(provider, cx));

        let panel = cx.new(|cx| {
            AgentTaskPanel::new(
                store,
                workspace.weak_handle(),
                project,
                file_system.clone(),
                cx,
            )
        });

        if let Some(window) = window {
            workspace.add_panel(panel, window, cx);
        }

        workspace.register_action(|workspace, _: &ToggleAgentTaskPanel, window, cx| {
            workspace.toggle_panel_focus::<AgentTaskPanel>(window, cx);
        });
    });
    subscription.detach();
}

pub struct AgentTaskPanel {
    pub store: Entity<AgentTaskStore>,
    pub selected_task_id: Option<AgentTaskId>,
    pub selected_detail: Option<AgentTaskDetail>,
    pub hide_completed_tasks: bool,
    pub goal_cleanup_result: Option<agent::task_worktree::GoalCleanupResult>,
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    file_system: Arc<dyn Fs>,
    _fetch_detail_task: Option<Task<()>>,
}

impl AgentTaskPanel {
    pub fn new(
        store: Entity<AgentTaskStore>,
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        file_system: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut swept = false;
        cx.observe(&store, move |this, store, cx| {
            if !swept && !store.read(cx).is_offline() && !store.read(cx).graph().tasks.is_empty() {
                swept = true;
                let project = this.project.clone();
                let tasks = store.read(cx).graph().tasks.clone();
                agent::task_worktree::startup_sweep(project, Some(&tasks), cx)
                    .detach_and_log_err(cx);
            }
            cx.notify();
        })
        .detach();

        cx.observe_global::<SettingsStore>(|this, cx| {
            this.sync_task_server(cx);
        })
        .detach();

        Self {
            store,
            selected_task_id: None,
            selected_detail: None,
            hide_completed_tasks: true,
            goal_cleanup_result: None,
            focus_handle: cx.focus_handle(),
            workspace,
            project,
            file_system,
            _fetch_detail_task: None,
        }
    }

    fn sync_task_server(&mut self, cx: &mut Context<Self>) {
        let server_name = AgentSettings::get_for_project(self.project.read(cx), cx)
            .task_graph_server_id
            .clone();
        if self.store.read(cx).provider().server_id().0.as_ref() == server_name {
            return;
        }

        let context_server_store = self.project.read(cx).context_server_store();
        let provider = Arc::new(agent::McpAgentTaskProvider::new(
            context_server_store,
            context_server::ContextServerId(server_name.into()),
        ));
        self.store
            .update(cx, |store, cx| store.set_provider(provider, cx));
    }

    pub fn select_task(&mut self, id: AgentTaskId, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_task_id = Some(id.clone());
        self.selected_detail = None;

        let provider = self.store.read(cx).provider().clone();
        let detail_task = provider.get_task(&id, cx);
        let artifacts_task = provider.list_artifacts(&id, cx);
        let workspace = self.workspace.clone();

        self._fetch_detail_task = Some(cx.spawn_in(window, async move |this, cx| {
            let detail_result = detail_task.await;
            let artifacts_result = artifacts_task.await;

            let detail = match detail_result {
                Ok(detail) => {
                    this.update(cx, |panel, cx| {
                        panel.selected_detail = Some(detail.clone());
                        cx.notify();
                    })
                    .log_err();
                    Some(detail)
                }
                Err(err) => {
                    log::error!("failed to fetch task detail for {id}: {err:?}");
                    None
                }
            };

            let artifacts = match artifacts_result {
                Ok(artifacts) => artifacts,
                Err(err) => {
                    log::error!("failed to fetch task artifacts for {id}: {err:?}");
                    Vec::new()
                }
            };

            if let Some(detail) = detail {
                if let Some(workspace) = workspace.upgrade() {
                    let title = format_task_editor_title(&id, &detail.summary.title);
                    let markdown = render_task_markdown(&detail, &artifacts);
                    let open_task = cx
                        .update(|window, cx| {
                            crate::open_markdown_in_workspace(
                                title, markdown, workspace, window, cx,
                            )
                        })
                        .log_err();
                    if let Some(open_task) = open_task {
                        open_task.await.log_err();
                    }
                }
            }
        }));
    }

    fn render_task_tree(&self, cx: &mut Context<Self>) -> Div {
        let store = self.store.read(cx);
        let graph = store.graph().clone();
        let events = store.events();
        let rows = build_task_rows(&graph.tasks, events, self.hide_completed_tasks);

        if rows.is_empty() {
            return v_flex().p_4().items_center().child(
                Label::new("No tasks")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }

        let mut row_elements = Vec::with_capacity(rows.len());
        for row in rows {
            row_elements.push(self.render_task_row(row, cx));
        }

        v_flex().gap_1().children(row_elements)
    }

    fn render_task_row(&self, row: TaskRow, cx: &mut Context<Self>) -> Div {
        let task = row.task;
        let prefix = row.prefix;
        let is_selected = self
            .selected_task_id
            .as_ref()
            .map_or(false, |selected_id| selected_id == &task.id);

        let policy_denied = self
            .store
            .read(cx)
            .policy_denied_event_for_task(&task.id)
            .cloned();

        let task_element = h_flex()
            .id(SharedString::from(format!("task-node-{}", task.id)))
            .w_full()
            .items_center()
            .justify_between()
            .px_2()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .when(is_selected, |this| {
                this.bg(cx.theme().colors().element_selected)
            })
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .flex_1()
                    .overflow_hidden()
                    .when(!prefix.is_empty(), |this| {
                        this.child(
                            Label::new(prefix)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .child(render_status_icon(task.status))
                    .when_some(task.assignee.as_ref(), |this, assignee| {
                        this.child(
                            Label::new(format!("[{assignee}]"))
                                .size(LabelSize::Small)
                                .color(Color::Accent),
                        )
                    })
                    .child(
                        Label::new(task.title.clone())
                            .size(LabelSize::Small)
                            .truncate(),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .when(task.attempt > 1, |this| {
                        this.child(
                            Label::new(format!("#{}", task.attempt))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .when_some(policy_denied, |this, denied_event| {
                        let denied_message = denied_event.message;
                        this.child(
                            IconButton::new("policy_warning", IconName::Warning)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Warning)
                                .tooltip(Tooltip::text(denied_message)),
                        )
                    }),
            )
            .on_click(cx.listener({
                let task_id = task.id.clone();
                move |this, _event, window, cx| {
                    this.select_task(task_id.clone(), window, cx);
                }
            }));

        let goal_summary = agent::task_worktree::goal_branch_summary_for_task(&task.id);
        let has_worktree = agent::task_worktree::task_worktree_path_for_id(&task.id).is_some();

        let goal_row = if let Some(summary) = goal_summary.as_ref() {
            let short_sha = summary
                .tip_sha
                .as_deref()
                .map(|sha| &sha[..7.min(sha.len())])
                .unwrap_or("none");
            let goal_id = summary.goal_id.clone();
            let has_active_sibling_tasks = agent::task_worktree::goal_has_active_tasks(&goal_id);
            let is_terminal = task.status.is_terminal()
                || agent::task_worktree::get_goal_graduation_summary(&goal_id).is_some();
            let project = self.project.clone();
            let task_id_clone = task.id.clone();

            Some(
                h_flex()
                    .w_full()
                    .pl_6()
                    .pr_2()
                    .py_0p5()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .child(
                                Label::new(format!("goal: {}", summary.branch))
                                    .size(LabelSize::Small)
                                    .color(Color::Accent),
                            )
                            .child(
                                Label::new(format!("[{short_sha}]"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(format!("{} merged", summary.merged_tasks_count))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .when(summary.has_conflict, |this| {
                                this.child(
                                    Label::new("conflict")
                                        .size(LabelSize::Small)
                                        .color(Color::Error),
                                )
                            })
                            .when_some(
                                self.goal_cleanup_result
                                    .as_ref()
                                    .filter(|res| res.goal_id == goal_id),
                                |this, res| {
                                    let summary_text = if res.failed_branches.is_empty() {
                                        format!("Cleaned ({} deleted)", res.deleted_branches.len())
                                    } else {
                                        format!(
                                            "Cleaned ({} deleted, {} failed)",
                                            res.deleted_branches.len(),
                                            res.failed_branches.len()
                                        )
                                    };
                                    let color = if res.failed_branches.is_empty() {
                                        Color::Success
                                    } else {
                                        Color::Warning
                                    };
                                    this.child(
                                        Label::new(summary_text)
                                            .size(LabelSize::Small)
                                            .color(color),
                                    )
                                },
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .when(has_worktree, |this| {
                                let project = project.clone();
                                let task_id = task_id_clone.clone();
                                this.child(
                                    Button::new(
                                        SharedString::from(format!("remove-worktree-{}", task_id)),
                                        "Remove Worktree",
                                    )
                                    .style(ButtonStyle::Subtle)
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(
                                        move |_this, _event, _window, cx| {
                                            agent::task_worktree::remove_task_worktree(
                                                project.clone(),
                                                &task_id,
                                                cx,
                                            )
                                            .detach_and_log_err(cx);
                                        },
                                    )),
                                )
                            })
                            .when(is_terminal, |this| {
                                let project = project.clone();
                                let goal_id = goal_id.clone();
                                this.child(
                                    Button::new(
                                        SharedString::from(format!("cleanup-goal-{}", task.id)),
                                        "Cleanup Goal Branches",
                                    )
                                    .style(ButtonStyle::Subtle)
                                    .label_size(LabelSize::Small)
                                    .disabled(has_active_sibling_tasks)
                                    .tooltip(if has_active_sibling_tasks {
                                        Tooltip::text("Cannot clean up goal: sibling tasks are still active")
                                    } else {
                                        Tooltip::text("Clean up goal branch and all associated task branches")
                                    })
                                    .on_click(cx.listener(
                                        move |_this, _event, _window, cx| {
                                            if agent::task_worktree::goal_has_active_tasks(&goal_id) {
                                                log::warn!(
                                                    "Cannot clean up goal {goal_id}: sibling tasks still running"
                                                );
                                                return;
                                            }
                                            let goal_id_for_async = goal_id.clone();
                                            let cleanup_task =
                                                agent::task_worktree::cleanup_graduated_goal(
                                                    project.clone(),
                                                    &goal_id,
                                                    cx,
                                                );
                                            cx.spawn(async move |this, cx| {
                                                match cleanup_task.await {
                                                    Ok(result) => {
                                                        log::info!(
                                                            "Cleaned up goal {}: {} branches deleted, {} failed",
                                                            result.goal_id,
                                                            result.deleted_branches.len(),
                                                            result.failed_branches.len()
                                                        );
                                                        this.update(cx, |this, cx| {
                                                            this.goal_cleanup_result = Some(result);
                                                            cx.notify();
                                                        })
                                                        .ok();
                                                    }
                                                    Err(e) => {
                                                        log::warn!(
                                                            "Failed to clean up goal {goal_id_for_async}: {e}"
                                                        );
                                                    }
                                                }
                                            })
                                            .detach();
                                        },
                                    )),
                                )
                            }),
                    ),
            )
        } else if has_worktree {
            let project = self.project.clone();
            let task_id = task.id.clone();
            Some(
                h_flex()
                    .w_full()
                    .pl_6()
                    .pr_2()
                    .py_0p5()
                    .justify_end()
                    .child(
                        Button::new(
                            SharedString::from(format!("remove-worktree-{}", task.id)),
                            "Remove Worktree",
                        )
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(
                            move |_this, _event, _window, cx| {
                                agent::task_worktree::remove_task_worktree(
                                    project.clone(),
                                    &task_id,
                                    cx,
                                )
                                .detach_and_log_err(cx);
                            },
                        )),
                    ),
            )
        } else {
            None
        };

        v_flex().w_full().child(task_element).children(goal_row)
    }
}

impl Focusable for AgentTaskPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for AgentTaskPanel {}

impl Panel for AgentTaskPanel {
    fn persistent_name() -> &'static str {
        "AgentTaskPanel"
    }

    fn panel_key() -> &'static str {
        AGENT_TASK_PANEL_KEY
    }

    fn activation_focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn position(&self, _window: &Window, cx: &App) -> DockPosition {
        AgentSettings::get_global(cx).task_dock.into()
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position != DockPosition::Bottom
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        let side = match position {
            DockPosition::Left => "left",
            DockPosition::Right | DockPosition::Bottom => "right",
        };
        telemetry::event!("Agent Task Panel Side Changed", side = side);
        let completion = settings::update_settings_file_with_completion(
            self.file_system.clone(),
            cx,
            move |settings, _| {
                settings
                    .agent
                    .get_or_insert_default()
                    .set_task_dock(position.into());
            },
        );
        cx.spawn(async move |_this, _cx| {
            if let Err(error) = completion.await {
                log::error!("Failed to update agent task panel dock position: {error:?}");
            }
        })
        .detach();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(320.0)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ListTodo)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Agent Tasks Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleAgentTaskPanel)
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}

impl Render for AgentTaskPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let is_offline = store.is_offline();
        let last_error = store.last_error().map(|error| error.to_string());

        let mut key_context = KeyContext::new_with_defaults();
        key_context.add("AgentTaskPanel");

        let (filter_icon, filter_tooltip) = if self.hide_completed_tasks {
            (IconName::EyeOff, "Show Completed Tasks")
        } else {
            (IconName::Eye, "Hide Completed Tasks")
        };

        v_flex()
            .key_context(key_context)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(Icon::new(IconName::Check).size(IconSize::Small))
                            .child(Label::new("Agent Tasks").size(LabelSize::Default)),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                IconButton::new("toggle_hide_completed", filter_icon)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text(filter_tooltip))
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.hide_completed_tasks = !this.hide_completed_tasks;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                IconButton::new("refresh_tasks", IconName::RotateCw)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Refresh Tasks"))
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        let store = this.store.clone();
                                        store
                                            .update(cx, |store, cx| store.refresh(cx))
                                            .detach_and_log_err(cx);
                                    })),
                            ),
                    ),
            )
            .when(is_offline, |this| {
                this.child(
                    h_flex()
                        .px_3()
                        .py_2()
                        .bg(cx.theme().status().warning_background)
                        .items_center()
                        .gap_2()
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::Small)
                                .color(Color::Warning),
                        )
                        .child(
                            Label::new(
                                last_error.unwrap_or_else(|| "Task server offline".to_string()),
                            )
                            .size(LabelSize::Small)
                            .color(Color::Warning),
                        ),
                )
            })
            .child(
                v_flex()
                    .id("agent_tasks_scroll_container")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(self.render_task_tree(cx)),
            )
    }
}

pub fn status_color(status: AgentTaskStatus) -> Color {
    match status {
        AgentTaskStatus::Ready => Color::Muted,
        AgentTaskStatus::Blocked => Color::Warning,
        AgentTaskStatus::Running => Color::Info,
        AgentTaskStatus::Stale => Color::Warning,
        AgentTaskStatus::Review => Color::Accent,
        AgentTaskStatus::Completed => Color::Success,
        AgentTaskStatus::Failed => Color::Error,
    }
}

pub fn status_icon(status: AgentTaskStatus) -> IconName {
    match status {
        AgentTaskStatus::Ready => IconName::Circle,
        AgentTaskStatus::Blocked => IconName::Stop,
        AgentTaskStatus::Running => IconName::PlayFilled,
        AgentTaskStatus::Stale => IconName::Clock,
        AgentTaskStatus::Review => IconName::Eye,
        AgentTaskStatus::Completed => IconName::Check,
        AgentTaskStatus::Failed => IconName::Close,
    }
}

pub fn render_status_icon(status: AgentTaskStatus) -> impl IntoElement {
    Icon::new(status_icon(status))
        .size(IconSize::Small)
        .color(status_color(status))
}

pub fn status_label(status: AgentTaskStatus) -> &'static str {
    match status {
        AgentTaskStatus::Ready => "Ready",
        AgentTaskStatus::Blocked => "Blocked",
        AgentTaskStatus::Running => "Running",
        AgentTaskStatus::Stale => "Stale",
        AgentTaskStatus::Review => "Review",
        AgentTaskStatus::Completed => "Completed",
        AgentTaskStatus::Failed => "Failed",
    }
}

pub fn event_kind_name(kind: AgentTaskEventKind) -> &'static str {
    match kind {
        AgentTaskEventKind::Info => "INFO",
        AgentTaskEventKind::ToolCall => "TOOL",
        AgentTaskEventKind::PolicyDenied => "DENIED",
        AgentTaskEventKind::StatusChanged => "STATUS",
        AgentTaskEventKind::ReviewVerdict => "REVIEW",
    }
}

pub fn format_task_editor_title(id: &AgentTaskId, title: &str) -> String {
    let full = format!("Task {id}: {title}");
    if full.chars().count() <= 48 {
        full
    } else {
        let truncated: String = full.chars().take(47).collect();
        format!("{truncated}…")
    }
}

pub fn render_task_markdown(detail: &AgentTaskDetail, artifacts: &[AgentTaskArtifact]) -> String {
    let mut doc = String::new();
    doc.push_str(&format!(
        "# {}

",
        detail.summary.title
    ));
    doc.push_str(&format!(
        "- **Status:** {}
",
        status_label(detail.summary.status)
    ));
    doc.push_str(&format!(
        "- **Task ID:** {}
",
        detail.summary.id
    ));
    if let Some(assignee) = &detail.summary.assignee {
        doc.push_str(&format!(
            "- **Assignee:** {}
",
            assignee
        ));
    }
    if detail.summary.attempt > 1 {
        doc.push_str(&format!(
            "- **Attempt:** {}
",
            detail.summary.attempt
        ));
    }

    doc.push_str(
        "
## Description

",
    );
    doc.push_str(detail.description.trim_end());
    doc.push_str(
        "
",
    );

    if !detail.acceptance_criteria.is_empty() {
        doc.push_str(
            "
## Acceptance Criteria

",
        );
        for criterion in &detail.acceptance_criteria {
            doc.push_str(&format!(
                "- [ ] {}
",
                criterion
            ));
        }
    }

    if !artifacts.is_empty() {
        doc.push_str(
            "
## Artifacts

",
        );
        for (index, artifact) in artifacts.iter().enumerate() {
            if index > 0 {
                doc.push_str(
                    "
",
                );
            }
            doc.push_str(&format!(
                "### {} — {}

",
                artifact.kind, artifact.id
            ));
            doc.push_str(artifact.content.trim_end());
            doc.push_str(
                "
",
            );
        }
    }

    if let Some(summary) = agent::task_worktree::goal_branch_summary_for_task(&detail.summary.id) {
        doc.push_str("\n## Goal Branch\n\n");
        doc.push_str(&format!("- **Branch:** `{}`\n", summary.branch));
        if let Some(sha) = &summary.tip_sha {
            let short_sha = &sha[..7.min(sha.len())];
            doc.push_str(&format!("- **Tip SHA:** `{short_sha}` ({sha})\n"));
        }
        doc.push_str(&format!(
            "- **Merged tasks:** {}\n",
            summary.merged_tasks_count
        ));
        if summary.has_conflict {
            doc.push_str("- **Status:** ⚠️ Merge conflict detected\n");
            if let Some(files) = &summary.conflicts {
                doc.push_str("  - Conflicting files:\n");
                for f in files {
                    doc.push_str(&format!("    - `{f}`\n"));
                }
            }
        }
    }

    if !detail.events_tail.is_empty() {
        doc.push_str(
            "
## Events

",
        );
        for event in &detail.events_tail {
            let kind = event_kind_name(event.kind);
            doc.push_str(&format!(
                "- `#{}` [{}] {}
",
                event.seq, kind, event.message
            ));
        }
    }

    doc
}

pub fn compute_task_creation_timestamps<'a>(
    events: impl IntoIterator<Item = &'a AgentTaskEvent>,
) -> HashMap<AgentTaskId, u64> {
    let mut timestamps = HashMap::new();
    for event in events {
        if let Some(task_id) = &event.task_id {
            timestamps
                .entry(task_id.clone())
                .and_modify(|existing: &mut u64| {
                    *existing = (*existing).min(event.timestamp_millis)
                })
                .or_insert(event.timestamp_millis);
        }
    }
    timestamps
}

pub fn filter_visible_tasks(
    tasks: &[AgentTaskSummary],
    hide_completed: bool,
) -> Vec<AgentTaskSummary> {
    if hide_completed {
        tasks
            .iter()
            .filter(|task| task.status != AgentTaskStatus::Completed)
            .cloned()
            .collect()
    } else {
        tasks.to_vec()
    }
}

pub fn sort_tasks_newest_first(
    tasks: &mut [AgentTaskSummary],
    timestamps: &HashMap<AgentTaskId, u64>,
) {
    tasks.sort_by(|a, b| {
        let time_a = timestamps.get(&a.id);
        let time_b = timestamps.get(&b.id);
        match (time_a, time_b) {
            (Some(ta), Some(tb)) => tb.cmp(ta),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });
}

pub fn visible_roots(
    visible_tasks: &[AgentTaskSummary],
    timestamps: &HashMap<AgentTaskId, u64>,
) -> Vec<AgentTaskSummary> {
    let visible_ids: HashSet<AgentTaskId> =
        visible_tasks.iter().map(|task| task.id.clone()).collect();
    let mut roots: Vec<AgentTaskSummary> = visible_tasks
        .iter()
        .filter(|task| {
            task.parent_id
                .as_ref()
                .map_or(true, |parent_id| !visible_ids.contains(parent_id))
        })
        .cloned()
        .collect();
    sort_tasks_newest_first(&mut roots, timestamps);
    roots
}

pub fn visible_children(
    parent_id: &AgentTaskId,
    visible_tasks: &[AgentTaskSummary],
    timestamps: &HashMap<AgentTaskId, u64>,
) -> Vec<AgentTaskSummary> {
    let mut children: Vec<AgentTaskSummary> = visible_tasks
        .iter()
        .filter(|task| task.parent_id.as_ref().map_or(false, |id| id == parent_id))
        .cloned()
        .collect();
    sort_tasks_newest_first(&mut children, timestamps);
    children
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskRow {
    pub task: AgentTaskSummary,
    pub prefix: String,
    pub depth: usize,
}

pub fn build_task_rows<'a>(
    tasks: &[AgentTaskSummary],
    events: impl IntoIterator<Item = &'a AgentTaskEvent>,
    hide_completed: bool,
) -> Vec<TaskRow> {
    let timestamps = compute_task_creation_timestamps(events);
    let visible = filter_visible_tasks(tasks, hide_completed);
    let roots = visible_roots(&visible, &timestamps);
    let mut rows = Vec::new();
    let mut visited = HashSet::new();

    fn collect_node(
        task: AgentTaskSummary,
        visible: &[AgentTaskSummary],
        timestamps: &HashMap<AgentTaskId, u64>,
        prefix: &str,
        ancestor_continuation: &str,
        depth: usize,
        visited: &mut HashSet<AgentTaskId>,
        rows: &mut Vec<TaskRow>,
    ) {
        if depth > 16 || !visited.insert(task.id.clone()) {
            log::error!("skipping cyclic or too deep task subtree at {}", task.id);
            return;
        }

        let task_id = task.id.clone();
        rows.push(TaskRow {
            task,
            prefix: prefix.to_string(),
            depth,
        });

        let children = visible_children(&task_id, visible, timestamps);
        let num_children = children.len();
        for (index, child) in children.into_iter().enumerate() {
            let is_last_child = index + 1 == num_children;
            let child_prefix = format!(
                "{}{}",
                ancestor_continuation,
                if is_last_child { "└─ " } else { "├─ " }
            );
            let next_continuation = format!(
                "{}{}",
                ancestor_continuation,
                if is_last_child { "   " } else { "│  " }
            );
            collect_node(
                child,
                visible,
                timestamps,
                &child_prefix,
                &next_continuation,
                depth + 1,
                visited,
                rows,
            );
        }
    }

    for root in roots {
        collect_node(
            root,
            &visible,
            &timestamps,
            "",
            "",
            0,
            &mut visited,
            &mut rows,
        );
    }

    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::{AgentTaskEvent, AgentTaskGraph, AgentTaskProvider, AgentTaskStatus};
    use context_server::ContextServerId;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use project::Project;
    use settings::SettingsStore;

    struct TestProvider {
        offline: bool,
        tasks: Vec<AgentTaskSummary>,
        events: Vec<AgentTaskEvent>,
    }

    impl Default for TestProvider {
        fn default() -> Self {
            Self {
                offline: false,
                tasks: vec![AgentTaskSummary {
                    id: AgentTaskId::from("TASK-1"),
                    parent_id: None,
                    title: "Test Task".to_string(),
                    status: AgentTaskStatus::Ready,
                    attempt: 1,
                    assignee: None,
                    write_scopes: vec![],
                }],
                events: vec![],
            }
        }
    }

    impl AgentTaskProvider for TestProvider {
        fn server_id(&self) -> ContextServerId {
            ContextServerId("test".into())
        }

        fn fetch_graph(&self, _cx: &mut App) -> Task<anyhow::Result<AgentTaskGraph>> {
            if self.offline {
                Task::ready(Err(anyhow::anyhow!("Task server offline")))
            } else {
                Task::ready(Ok(AgentTaskGraph {
                    tasks: self.tasks.clone(),
                }))
            }
        }

        fn get_task(
            &self,
            id: &AgentTaskId,
            _cx: &mut App,
        ) -> Task<anyhow::Result<AgentTaskDetail>> {
            let task = self
                .tasks
                .iter()
                .find(|t| &t.id == id)
                .cloned()
                .unwrap_or_else(|| AgentTaskSummary {
                    id: id.clone(),
                    parent_id: None,
                    title: "Test Task".to_string(),
                    status: AgentTaskStatus::Ready,
                    attempt: 1,
                    assignee: None,
                    write_scopes: vec![],
                });
            Task::ready(Ok(AgentTaskDetail {
                summary: task,
                description: "Test Description".to_string(),
                acceptance_criteria: vec!["Criterion 1".to_string()],
                events_tail: vec![],
            }))
        }

        fn complete_task(&self, _id: &AgentTaskId, _cx: &mut App) -> Task<anyhow::Result<()>> {
            Task::ready(Ok(()))
        }

        fn fail_task(
            &self,
            _id: &AgentTaskId,
            _reason: &str,
            _cx: &mut App,
        ) -> Task<anyhow::Result<()>> {
            Task::ready(Ok(()))
        }

        fn list_events(
            &self,
            _limit: u32,
            _cx: &mut App,
        ) -> Task<anyhow::Result<Vec<AgentTaskEvent>>> {
            Task::ready(Ok(self.events.clone()))
        }

        fn list_artifacts(
            &self,
            _task_id: &AgentTaskId,
            _cx: &mut App,
        ) -> Task<anyhow::Result<Vec<AgentTaskArtifact>>> {
            Task::ready(Ok(vec![]))
        }

        fn get_artifact(
            &self,
            _artifact_id: &str,
            _cx: &mut App,
        ) -> Task<anyhow::Result<AgentTaskArtifact>> {
            Task::ready(Err(anyhow::anyhow!("not implemented")))
        }
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme::init(theme::LoadThemes::JustBase, cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            project::project_settings::ProjectSettings::register(cx);
            AgentSettings::register(cx);
        });
    }

    #[gpui::test]
    async fn test_agent_task_panel_offline_rendering(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;
        let provider = Arc::new(TestProvider {
            offline: true,
            ..Default::default()
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(store, WeakEntity::new_invalid(), project, file_system, cx)
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            assert!(panel.store.read(cx).is_offline());
            assert!(panel.store.read(cx).last_error().is_some());
        });
    }

    #[gpui::test]
    async fn test_agent_task_panel_online_tree_rendering(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;
        let provider = Arc::new(TestProvider::default());
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(store, WeakEntity::new_invalid(), project, file_system, cx)
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            let store = panel.store.read(cx);
            assert!(!store.is_offline());
            assert_eq!(store.graph().tasks.len(), 1);
            assert_eq!(store.graph().tasks[0].id.0.as_ref(), "TASK-1");
        });

        panel.update_in(cx, |panel, window, cx| {
            panel.select_task(AgentTaskId::from("TASK-1"), window, cx);
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let detail = panel
                .selected_detail
                .as_ref()
                .expect("task detail should be fetched");
            assert_eq!(detail.summary.id.0.as_ref(), "TASK-1");
            assert!(!detail.acceptance_criteria.is_empty());
        });
    }

    #[gpui::test]
    async fn test_agent_task_panel_dock_position_independent_of_agent_panel(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;
        let provider = Arc::new(TestProvider::default());
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(store, WeakEntity::new_invalid(), project, file_system, cx)
        });
        cx.run_until_parked();

        let (initial_task_pos, initial_agent_pos) = cx.read(|cx| {
            (
                AgentSettings::get_global(cx).task_dock,
                AgentSettings::get_global(cx).dock,
            )
        });
        assert_eq!(initial_task_pos, settings::DockPosition::Left);

        panel.update_in(cx, |panel, window, cx| {
            panel.set_position(DockPosition::Right, window, cx);
        });
        cx.run_until_parked();

        let (updated_task_pos, updated_agent_pos) = cx.read(|cx| {
            (
                AgentSettings::get_global(cx).task_dock,
                AgentSettings::get_global(cx).dock,
            )
        });
        assert_eq!(updated_task_pos, settings::DockPosition::Right);
        assert_eq!(updated_agent_pos, initial_agent_pos);
    }

    #[gpui::test]
    async fn test_agent_task_panel_filter_and_orphan_promotion(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;

        let root_completed = AgentTaskSummary {
            id: AgentTaskId::from("TASK-COMPLETED"),
            parent_id: None,
            title: "Completed Root".to_string(),
            status: AgentTaskStatus::Completed,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let child_failed = AgentTaskSummary {
            id: AgentTaskId::from("TASK-FAILED-CHILD"),
            parent_id: Some(AgentTaskId::from("TASK-COMPLETED")),
            title: "Failed Child".to_string(),
            status: AgentTaskStatus::Failed,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let standalone_ready = AgentTaskSummary {
            id: AgentTaskId::from("TASK-READY"),
            parent_id: None,
            title: "Ready Standalone".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };

        let tasks = vec![root_completed, child_failed, standalone_ready];
        let provider = Arc::new(TestProvider {
            offline: false,
            tasks,
            events: vec![],
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(store, WeakEntity::new_invalid(), project, file_system, cx)
        });
        cx.run_until_parked();

        // Default: hide_completed_tasks is true.
        // Completed root is filtered out; child is promoted to root.
        panel.read_with(cx, |panel, cx| {
            assert!(panel.hide_completed_tasks);
            let store = panel.store.read(cx);
            let visible = filter_visible_tasks(&store.graph().tasks, panel.hide_completed_tasks);
            assert_eq!(visible.len(), 2);
            assert!(
                visible
                    .iter()
                    .all(|t| t.status != AgentTaskStatus::Completed)
            );

            let timestamps = HashMap::new();
            let roots = visible_roots(&visible, &timestamps);
            assert_eq!(roots.len(), 2);
            let root_ids: Vec<&str> = roots.iter().map(|t| t.id.0.as_ref()).collect();
            assert!(root_ids.contains(&"TASK-FAILED-CHILD"));
            assert!(root_ids.contains(&"TASK-READY"));

            let rows = build_task_rows(
                &store.graph().tasks,
                store.events(),
                panel.hide_completed_tasks,
            );
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].prefix, "");
            assert_eq!(rows[0].depth, 0);
            assert_eq!(rows[1].prefix, "");
            assert_eq!(rows[1].depth, 0);
        });

        // Toggle filter off: all tasks visible, Completed root is present and Failed child is under it.
        panel.update(cx, |panel, cx| {
            panel.hide_completed_tasks = false;
            cx.notify();
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            assert!(!panel.hide_completed_tasks);
            let store = panel.store.read(cx);
            let visible = filter_visible_tasks(&store.graph().tasks, panel.hide_completed_tasks);
            assert_eq!(visible.len(), 3);

            let timestamps = HashMap::new();
            let roots = visible_roots(&visible, &timestamps);
            assert_eq!(roots.len(), 2);
            let root_ids: Vec<&str> = roots.iter().map(|t| t.id.0.as_ref()).collect();
            assert!(root_ids.contains(&"TASK-COMPLETED"));
            assert!(root_ids.contains(&"TASK-READY"));

            let children =
                visible_children(&AgentTaskId::from("TASK-COMPLETED"), &visible, &timestamps);
            assert_eq!(children.len(), 1);
            assert_eq!(children[0].id.0.as_ref(), "TASK-FAILED-CHILD");

            let rows = build_task_rows(
                &store.graph().tasks,
                store.events(),
                panel.hide_completed_tasks,
            );
            assert_eq!(rows.len(), 3);
            let row_info: Vec<(&str, &str, usize)> = rows
                .iter()
                .map(|r| (r.task.id.0.as_ref(), r.prefix.as_str(), r.depth))
                .collect();
            assert_eq!(
                row_info,
                vec![
                    ("TASK-COMPLETED", "", 0),
                    ("TASK-FAILED-CHILD", "└─ ", 1),
                    ("TASK-READY", "", 0),
                ]
            );
        });
    }

    #[test]
    fn test_agent_task_panel_newest_first_ordering() {
        let task_a = AgentTaskSummary {
            id: AgentTaskId::from("TASK-A"),
            parent_id: None,
            title: "Task A".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let task_b = AgentTaskSummary {
            id: AgentTaskId::from("TASK-B"),
            parent_id: None,
            title: "Task B".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let task_b_child_1 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-B-CHILD-1"),
            parent_id: Some(AgentTaskId::from("TASK-B")),
            title: "Task B Child 1".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let task_b_grandchild = AgentTaskSummary {
            id: AgentTaskId::from("TASK-B-GRANDCHILD"),
            parent_id: Some(AgentTaskId::from("TASK-B-CHILD-1")),
            title: "Task B Grandchild".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let task_b_child_2 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-B-CHILD-2"),
            parent_id: Some(AgentTaskId::from("TASK-B")),
            title: "Task B Child 2".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let task_c = AgentTaskSummary {
            id: AgentTaskId::from("TASK-C"),
            parent_id: None,
            title: "Task C".to_string(),
            status: AgentTaskStatus::Blocked,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let task_no_events = AgentTaskSummary {
            id: AgentTaskId::from("TASK-NO-EVENTS"),
            parent_id: None,
            title: "Task No Events".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let tasks = vec![
            task_a,
            task_b,
            task_b_child_1,
            task_b_grandchild,
            task_b_child_2,
            task_c,
            task_no_events,
        ];
        let events = vec![
            AgentTaskEvent {
                seq: 1,
                timestamp_millis: 1000,
                task_id: Some(AgentTaskId::from("TASK-A")),
                kind: AgentTaskEventKind::StatusChanged,
                message: "created A".to_string(),
            },
            AgentTaskEvent {
                seq: 2,
                timestamp_millis: 3000,
                task_id: Some(AgentTaskId::from("TASK-B")),
                kind: AgentTaskEventKind::StatusChanged,
                message: "created B".to_string(),
            },
            AgentTaskEvent {
                seq: 3,
                timestamp_millis: 4000,
                task_id: Some(AgentTaskId::from("TASK-B")),
                kind: AgentTaskEventKind::ToolCall,
                message: "tool call on B".to_string(),
            },
            AgentTaskEvent {
                seq: 4,
                timestamp_millis: 3500,
                task_id: Some(AgentTaskId::from("TASK-B-CHILD-1")),
                kind: AgentTaskEventKind::StatusChanged,
                message: "created B child 1".to_string(),
            },
            AgentTaskEvent {
                seq: 5,
                timestamp_millis: 3400,
                task_id: Some(AgentTaskId::from("TASK-B-GRANDCHILD")),
                kind: AgentTaskEventKind::StatusChanged,
                message: "created B grandchild".to_string(),
            },
            AgentTaskEvent {
                seq: 6,
                timestamp_millis: 3100,
                task_id: Some(AgentTaskId::from("TASK-B-CHILD-2")),
                kind: AgentTaskEventKind::StatusChanged,
                message: "created B child 2".to_string(),
            },
            AgentTaskEvent {
                seq: 7,
                timestamp_millis: 2000,
                task_id: Some(AgentTaskId::from("TASK-C")),
                kind: AgentTaskEventKind::StatusChanged,
                message: "created C".to_string(),
            },
        ];
        let rows = build_task_rows(&tasks, &events, false);
        let extracted: Vec<(&str, &str, usize)> = rows
            .iter()
            .map(|r| (r.task.id.0.as_ref(), r.prefix.as_str(), r.depth))
            .collect();
        assert_eq!(
            extracted,
            vec![
                ("TASK-B", "", 0),
                ("TASK-B-CHILD-1", "├─ ", 1),
                ("TASK-B-GRANDCHILD", "│  └─ ", 2),
                ("TASK-B-CHILD-2", "└─ ", 1),
                ("TASK-C", "", 0),
                ("TASK-A", "", 0),
                ("TASK-NO-EVENTS", "", 0),
            ]
        );
    }

    #[test]
    fn test_agent_task_panel_render_task_markdown() {
        let detail = AgentTaskDetail {
            summary: AgentTaskSummary {
                id: AgentTaskId::from("TASK-35"),
                parent_id: None,
                title: "Rework Task Panel".to_string(),
                status: AgentTaskStatus::Running,
                attempt: 2,
                assignee: Some("ui_engineer".into()),
                write_scopes: vec![],
            },
            description: "Detailed description of the rework.".to_string(),
            acceptance_criteria: vec![
                "Criteria 1: colored icons".to_string(),
                "Criteria 2: box tree".to_string(),
            ],
            events_tail: vec![
                AgentTaskEvent {
                    seq: 159,
                    timestamp_millis: 1790630864009,
                    task_id: Some(AgentTaskId::from("TASK-35")),
                    kind: AgentTaskEventKind::StatusChanged,
                    message: "task.created".to_string(),
                },
                AgentTaskEvent {
                    seq: 160,
                    timestamp_millis: 1790630868818,
                    task_id: Some(AgentTaskId::from("TASK-35")),
                    kind: AgentTaskEventKind::ToolCall,
                    message: "run terminal".to_string(),
                },
            ],
        };

        let artifacts = vec![AgentTaskArtifact {
            id: "art-1".to_string(),
            task_id: AgentTaskId::from("TASK-35"),
            kind: "review".to_string(),
            content: "LGTM approved".to_string(),
        }];

        let md = render_task_markdown(&detail, &artifacts);
        assert!(md.contains("# Rework Task Panel"));
        assert!(md.contains("- **Status:** Running"));
        assert!(md.contains("- **Task ID:** TASK-35"));
        assert!(md.contains("- **Assignee:** ui_engineer"));
        assert!(md.contains("- **Attempt:** 2"));
        assert!(md.contains(
            "## Description

Detailed description of the rework."
        ));
        assert!(md.contains("## Acceptance Criteria"));
        assert!(md.contains("- [ ] Criteria 1: colored icons"));
        assert!(md.contains("- [ ] Criteria 2: box tree"));
        assert!(md.contains("## Artifacts"));
        assert!(md.contains(
            "### review — art-1

LGTM approved"
        ));
        assert!(md.contains("## Events"));
        assert!(md.contains("- `#159` [STATUS] task.created"));
        assert!(md.contains("- `#160` [TOOL] run terminal"));

        let minimal_detail = AgentTaskDetail {
            summary: AgentTaskSummary {
                id: AgentTaskId::from("TASK-1"),
                parent_id: None,
                title: "Simple Task".to_string(),
                status: AgentTaskStatus::Ready,
                attempt: 1,
                assignee: None,
                write_scopes: vec![],
            },
            description: "Simple description".to_string(),
            acceptance_criteria: vec![],
            events_tail: vec![],
        };
        let minimal_md = render_task_markdown(&minimal_detail, &[]);
        assert!(minimal_md.contains("# Simple Task"));
        assert!(minimal_md.contains("- **Status:** Ready"));
        assert!(minimal_md.contains("- **Task ID:** TASK-1"));
        assert!(!minimal_md.contains("- **Assignee:**"));
        assert!(!minimal_md.contains("- **Attempt:**"));
        assert!(!minimal_md.contains("## Acceptance Criteria"));
        assert!(!minimal_md.contains("## Artifacts"));
        assert!(!minimal_md.contains("## Events"));
    }

    #[test]
    fn test_format_task_editor_title() {
        assert_eq!(
            format_task_editor_title(&AgentTaskId::from("TASK-1"), "Short"),
            "Task TASK-1: Short"
        );
        let long_title = "A".repeat(100);
        let formatted = format_task_editor_title(&AgentTaskId::from("TASK-1"), &long_title);
        assert_eq!(formatted.chars().count(), 48);
        assert!(formatted.ends_with('…'));
    }

    #[test]
    fn test_agent_task_panel_renders_goal_row_and_markdown() {
        let task_id = AgentTaskId::from("TASK-GOAL-ROW");
        let goal_id = "GOAL-UI-1";
        agent::task_worktree::register_task_worktree_policy(
            &task_id,
            std::path::PathBuf::from("/tmp/fake"),
            Some(format!("agent-goal/{goal_id}")),
            None,
            None,
            None,
        );
        agent::task_worktree::record_goal_tip_sha(goal_id, "1234567890abcdef".to_string());

        let summary = agent::task_worktree::goal_branch_summary_for_task(&task_id)
            .expect("must produce goal branch summary");
        assert_eq!(summary.branch, "agent-goal/GOAL-UI-1");
        assert_eq!(summary.tip_sha, Some("1234567890abcdef".to_string()));
        assert_eq!(summary.merged_tasks_count, 0);
        assert!(!summary.has_conflict);

        let detail = AgentTaskDetail {
            summary: AgentTaskSummary {
                id: task_id.clone(),
                parent_id: None,
                title: "Goal Row Task".to_string(),
                status: AgentTaskStatus::Ready,
                attempt: 1,
                assignee: None,
                write_scopes: vec![],
            },
            description: "Task description".to_string(),
            acceptance_criteria: vec![],
            events_tail: vec![],
        };

        let md = render_task_markdown(&detail, &[]);
        assert!(md.contains("## Goal Branch"));
        assert!(md.contains("- **Branch:** `agent-goal/GOAL-UI-1`"));
        assert!(md.contains("- **Tip SHA:** `1234567` (1234567890abcdef)"));
        assert!(md.contains("- **Merged tasks:** 0"));
        assert!(!md.contains("Merge conflict detected"));

        // When conflict is recorded
        agent::task_worktree::record_task_worktree_merge_conflict(
            &task_id,
            vec!["src/main.rs".to_string()],
        );
        let summary_conf = agent::task_worktree::goal_branch_summary_for_task(&task_id).unwrap();
        assert!(summary_conf.has_conflict);
        assert_eq!(
            summary_conf.conflicts,
            Some(vec!["src/main.rs".to_string()])
        );

        let md_conf = render_task_markdown(&detail, &[]);
        assert!(md_conf.contains("- **Status:** ⚠️ Merge conflict detected"));
        assert!(md_conf.contains("`src/main.rs`"));
    }

    #[gpui::test]
    async fn test_agent_task_panel_renders_goal_row_in_tree(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;

        let task_id = AgentTaskId::from("TASK-1");
        agent::task_worktree::register_task_worktree_policy(
            &task_id,
            std::path::PathBuf::from("/fake/wt"),
            Some("agent-goal/GOAL-TEST".to_string()),
            None,
            None,
            None,
        );
        agent::task_worktree::record_goal_tip_sha("GOAL-TEST", "deadbeef012345".to_string());

        let provider = Arc::new(TestProvider::default());
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(store, WeakEntity::new_invalid(), project, file_system, cx)
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            let summary = agent::task_worktree::goal_branch_summary_for_task(&task_id).unwrap();
            assert_eq!(summary.branch, "agent-goal/GOAL-TEST");
            assert_eq!(summary.tip_sha, Some("deadbeef012345".to_string()));
            assert!(!panel.store.read(cx).is_offline());
        });
    }
}
