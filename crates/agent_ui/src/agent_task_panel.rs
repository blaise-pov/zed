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
    AgentGoalSummary, AgentTaskArtifact, AgentTaskDetail, AgentTaskEvent, AgentTaskEventKind,
    AgentTaskId, AgentTaskStatus, AgentTaskStore, AgentTaskSummary, GoalGitState, OrphanWorktree,
    TaskGitSnapshot, TaskWorktreeStatus,
};
use agent_settings::AgentSettings;
use fs::Fs;
use gpui::{
    Action, App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, KeyContext, Pixels,
    Task, WeakEntity, Window, actions, prelude::*,
};
use project::{Project, git_store::Repository};
use settings::{Settings, SettingsStore};
use ui::{
    Color, ContextMenu, Icon, IconButton, IconName, IconPosition, IconSize, Label, LabelSize,
    PopoverMenu, Tooltip, prelude::*,
};
use util::ResultExt;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};
use zed_actions::{OpenWorktreeInNewWindow, SwitchWorktree};

pub const ALL_TASK_STATUSES: [AgentTaskStatus; 8] = [
    AgentTaskStatus::Ready,
    AgentTaskStatus::Blocked,
    AgentTaskStatus::Running,
    AgentTaskStatus::Stale,
    AgentTaskStatus::Review,
    AgentTaskStatus::Completed,
    AgentTaskStatus::Failed,
    AgentTaskStatus::Archived,
];

pub const NO_GOAL_KEY: &str = "__no_goal__";

fn paths_match(a: &std::path::Path, b: &std::path::Path) -> bool {
    a == b
        || util::paths::normalize_lexically(a).ok().as_deref()
            == util::paths::normalize_lexically(b).ok().as_deref()
}

pub(crate) fn find_repository_for_worktree_path(
    project: &Entity<Project>,
    worktree_path: &std::path::Path,
    cx: &App,
) -> Option<Entity<Repository>> {
    project
        .read(cx)
        .repositories(cx)
        .values()
        .find(|repo| {
            let work_dir = repo.read(cx).snapshot().work_directory_abs_path;
            paths_match(work_dir.as_ref(), worktree_path)
        })
        .cloned()
}

pub fn default_status_filters() -> HashSet<AgentTaskStatus> {
    HashSet::from([
        AgentTaskStatus::Ready,
        AgentTaskStatus::Blocked,
        AgentTaskStatus::Running,
        AgentTaskStatus::Stale,
        AgentTaskStatus::Review,
        AgentTaskStatus::Completed,
        AgentTaskStatus::Failed,
    ])
}

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
    pub worktree_status: Entity<TaskWorktreeStatus>,
    pub selected_task_id: Option<AgentTaskId>,
    pub selected_detail: Option<AgentTaskDetail>,
    pub status_filters: HashSet<AgentTaskStatus>,
    pub seen_other_statuses: HashSet<AgentTaskStatus>,
    pub collapsed_goals: HashMap<String, bool>,
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
        let worktree_status = cx.new(|cx| TaskWorktreeStatus::new(project.clone(), cx));
        cx.observe(&worktree_status, |_, _, cx| {
            cx.notify();
        })
        .detach();

        let mut swept = false;
        cx.observe(&store, move |this, store, cx| {
            let (is_offline, tasks) = {
                let store_read = store.read(cx);
                (store_read.is_offline(), store_read.graph().tasks.clone())
            };
            if !is_offline {
                if !swept && !tasks.is_empty() {
                    swept = true;
                    let project = this.project.clone();
                    agent::task_worktree::startup_sweep(project, Some(&tasks), cx)
                        .detach_and_log_err(cx);
                }
                this.refresh_worktree_status(cx);
            }
            for task in &tasks {
                if let AgentTaskStatus::Other(_) = &task.status {
                    if this.seen_other_statuses.insert(task.status.clone()) {
                        this.status_filters.insert(task.status.clone());
                    }
                }
            }
            cx.notify();
        })
        .detach();

        cx.observe_global::<SettingsStore>(|this, cx| {
            this.sync_task_server(cx);
        })
        .detach();

        let mut seen_other_statuses = HashSet::new();
        let mut status_filters = default_status_filters();
        {
            let store_read = store.read(cx);
            for task in &store_read.graph().tasks {
                if let AgentTaskStatus::Other(_) = &task.status {
                    if seen_other_statuses.insert(task.status.clone()) {
                        status_filters.insert(task.status.clone());
                    }
                }
            }
        }

        let mut panel = Self {
            store,
            worktree_status,
            selected_task_id: None,
            selected_detail: None,
            status_filters,
            seen_other_statuses,
            collapsed_goals: HashMap::new(),
            goal_cleanup_result: None,
            focus_handle: cx.focus_handle(),
            workspace,
            project,
            file_system,
            _fetch_detail_task: None,
        };

        if !panel.store.read(cx).is_offline() {
            panel.refresh_worktree_status(cx);
        }

        panel
    }

    pub fn refresh_worktree_status(&mut self, cx: &mut Context<Self>) {
        let (tasks, goals) = {
            let store = self.store.read(cx);
            let graph = store.graph();
            (graph.tasks.clone(), graph.goals.clone())
        };
        self.worktree_status.update(cx, |status, cx| {
            status.refresh(tasks, goals, cx).detach_and_log_err(cx);
        });
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
                    let (goal, goal_git) = this
                        .update(cx, |panel, cx| {
                            let goal_id = detail.summary.goal_id.as_ref();
                            let goal = goal_id.and_then(|gid| {
                                panel
                                    .store
                                    .read(cx)
                                    .graph()
                                    .goals
                                    .iter()
                                    .find(|g| &g.goal_id == gid)
                                    .cloned()
                            });
                            let goal_git = goal_id.and_then(|gid| {
                                panel
                                    .worktree_status
                                    .read(cx)
                                    .snapshot()
                                    .and_then(|s| s.goals.get(gid))
                                    .cloned()
                            });
                            (goal, goal_git)
                        })
                        .unwrap_or((None, None));

                    let title = format_task_editor_title(&id, &detail.summary.title);
                    let markdown =
                        render_task_markdown(&detail, &artifacts, goal.as_ref(), goal_git.as_ref());
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
        let groups = build_goal_groups(&graph.tasks, &graph.goals, events, &self.status_filters);

        let snapshot = self.worktree_status.read(cx).snapshot().cloned();
        let has_orphans = snapshot
            .as_ref()
            .map_or(false, |s| !s.orphan_worktrees.is_empty());

        if groups.is_empty() && !has_orphans {
            return v_flex().p_4().items_center().child(
                Label::new("No tasks")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }

        let mut group_elements = Vec::with_capacity(groups.len() + 1);
        for group in groups {
            group_elements.push(self.render_goal_group(group, snapshot.as_ref(), cx));
        }

        if let Some(snapshot) = snapshot.as_ref() {
            if !snapshot.orphan_worktrees.is_empty() {
                group_elements.push(self.render_orphan_worktrees(&snapshot.orphan_worktrees, cx));
            }
        }

        v_flex().gap_2().children(group_elements)
    }

    fn render_orphan_worktrees(&self, orphans: &[OrphanWorktree], cx: &mut Context<Self>) -> Div {
        let mut rows = Vec::with_capacity(orphans.len());
        for orphan in orphans {
            let task_id_hint = orphan.task_id_hint.clone();
            let path = orphan.path.clone();
            let path_str = path.to_string_lossy().to_string();

            let delete_button = IconButton::new(
                SharedString::from(format!("delete-orphan-{}", task_id_hint)),
                IconName::Trash,
            )
            .icon_size(IconSize::Small)
            .tooltip(Tooltip::text("Delete Orphaned Worktree"))
            .on_click(cx.listener({
                let path = path.clone();
                let task_id_hint = task_id_hint.clone();
                move |this, _event, _window, cx| {
                    cx.stop_propagation();
                    let project = this.project.clone();
                    let path = path.clone();
                    let task_id_hint_for_async = task_id_hint.clone();
                    let remove_task =
                        agent::task_worktree::remove_orphan_worktree(project, path, cx);
                    cx.spawn(async move |this, cx| {
                        if let Err(e) = remove_task.await {
                            log::warn!(
                                "Failed to remove orphan worktree for {task_id_hint_for_async}: {e}"
                            );
                        }
                        this.update(cx, |this, cx| {
                            this.refresh_worktree_status(cx);
                        })
                        .ok();
                    })
                    .detach();
                }
            }));

            rows.push(
                h_flex()
                    .id(SharedString::from(format!(
                        "orphan-worktree-{}",
                        task_id_hint
                    )))
                    .debug_selector({
                        let task_id_hint = task_id_hint.clone();
                        move || format!("orphan-worktree-{}", task_id_hint)
                    })
                    .w_full()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .flex_1()
                            .overflow_hidden()
                            .child(
                                Icon::new(IconName::Warning)
                                    .size(IconSize::Small)
                                    .color(Color::Warning),
                            )
                            .child(
                                Label::new(format!("agent-task-{}", task_id_hint))
                                    .size(LabelSize::Small),
                            )
                            .child(
                                Label::new(path_str)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "delete-orphan-{}",
                                task_id_hint
                            )))
                            .debug_selector({
                                let task_id_hint = task_id_hint.clone();
                                move || format!("delete-orphan-{}", task_id_hint)
                            })
                            .child(delete_button),
                    ),
            );
        }

        div().child(
            v_flex()
                .id("orphan-worktrees-section")
                .debug_selector(|| "orphan-worktrees-section".to_string())
                .gap_1()
                .pt_2()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    h_flex()
                        .px_2()
                        .py_0p5()
                        .items_center()
                        .gap_2()
                        .child(
                            Label::new("Orphaned Worktrees")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(format!("({})", orphans.len()))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
                .children(rows),
        )
    }

    fn render_goal_group(
        &self,
        group: GoalGroup,
        snapshot: Option<&TaskGitSnapshot>,
        cx: &mut Context<Self>,
    ) -> Div {
        if let Some(goal_id) = group.goal_id {
            let is_collapsed = self.collapsed_goals.get(&goal_id).copied().unwrap_or(false);

            let title = group
                .goal_summary
                .as_ref()
                .map(|g| g.title.clone())
                .unwrap_or_else(|| goal_id.clone());

            let goal_git = snapshot.and_then(|s| s.goals.get(&goal_id));
            let branch_exists = goal_git.map_or(false, |g| g.branch_exists);
            let tip_sha = goal_git.and_then(|g| g.tip_sha.as_deref());

            let (tasks_done, tasks_total) = if let Some(summary) = &group.goal_summary {
                (summary.tasks_done, summary.tasks_total)
            } else {
                let store = self.store.read(cx);
                let total = store
                    .graph()
                    .tasks
                    .iter()
                    .filter(|t| t.goal_id.as_deref() == Some(&goal_id))
                    .count() as u64;
                let done = store
                    .graph()
                    .tasks
                    .iter()
                    .filter(|t| {
                        t.goal_id.as_deref() == Some(&goal_id)
                            && t.status == AgentTaskStatus::Completed
                    })
                    .count() as u64;
                (done, total)
            };

            let has_active_tasks = self
                .store
                .read(cx)
                .graph()
                .tasks
                .iter()
                .any(|t| t.goal_id.as_deref() == Some(&goal_id) && !t.status.is_terminal());
            let can_cleanup = !has_active_tasks && branch_exists;

            let cleanup_button = Button::new(
                SharedString::from(format!("cleanup-goal-{}", goal_id)),
                "Cleanup Goal Branches",
            )
            .style(ButtonStyle::Subtle)
            .label_size(LabelSize::Small)
            .disabled(!can_cleanup)
            .tooltip(if has_active_tasks {
                Tooltip::text("Cannot clean up goal: tasks are still active")
            } else if !branch_exists {
                Tooltip::text("Cannot clean up goal: branch does not exist")
            } else {
                Tooltip::text("Clean up goal branch and all associated task branches")
            })
            .on_click(cx.listener({
                let goal_id = goal_id.clone();
                move |this, _event, _window, cx| {
                    cx.stop_propagation();
                    if !can_cleanup {
                        return;
                    }
                    let goal_id_for_async = goal_id.clone();
                    let cleanup_task = agent::task_worktree::cleanup_graduated_goal(
                        this.project.clone(),
                        &goal_id,
                        cx,
                    );
                    cx.spawn(async move |this, cx| match cleanup_task.await {
                        Ok(result) => {
                            log::info!(
                                "Cleaned up goal {}: {} branches deleted, {} failed",
                                result.goal_id,
                                result.deleted_branches.len(),
                                result.failed_branches.len()
                            );
                            this.update(cx, |this, cx| {
                                this.goal_cleanup_result = Some(result);
                                this.refresh_worktree_status(cx);
                                cx.notify();
                            })
                            .ok();
                        }
                        Err(e) => {
                            log::warn!("Failed to clean up goal {goal_id_for_async}: {e}");
                        }
                    })
                    .detach();
                }
            }));

            let header = h_flex()
                .id(SharedString::from(format!("goal-group-header-{}", goal_id)))
                .debug_selector({
                    let goal_id = goal_id.clone();
                    move || format!("goal-group-header-{}", goal_id)
                })
                .w_full()
                .items_center()
                .justify_between()
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .bg(cx.theme().colors().element_background)
                .on_click(cx.listener({
                    let goal_id = goal_id.clone();
                    move |this, _event, _window, cx| {
                        let entry = this.collapsed_goals.entry(goal_id.clone()).or_insert(false);
                        *entry = !*entry;
                        cx.notify();
                    }
                }))
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .flex_1()
                        .overflow_hidden()
                        .child(
                            Icon::new(if is_collapsed {
                                IconName::ChevronRight
                            } else {
                                IconName::ChevronDown
                            })
                            .size(IconSize::Small)
                            .color(Color::Muted),
                        )
                        .child(Label::new(title).size(LabelSize::Small).truncate())
                        .child(
                            Label::new(format!("agent-goal/{goal_id}"))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .when(!branch_exists, |this| {
                            this.child(
                                h_flex()
                                    .id(SharedString::from(format!(
                                        "goal-branch-missing-{}",
                                        goal_id
                                    )))
                                    .child(
                                        Icon::new(IconName::Warning)
                                            .size(IconSize::Small)
                                            .color(Color::Warning),
                                    )
                                    .tooltip(Tooltip::text("Goal branch does not exist")),
                            )
                        })
                        .when_some(tip_sha, |this, sha| {
                            let short_sha = &sha[..7.min(sha.len())];
                            this.child(
                                Label::new(format!("[{short_sha}]"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        })
                        .child(
                            Label::new(format!("{tasks_done}/{tasks_total}"))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .when_some(group.goal_summary.as_ref(), |this, summary| {
                            this.child(
                                Label::new(format!("P{} · {}", summary.priority, summary.status))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
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
                                    Label::new(summary_text).size(LabelSize::Small).color(color),
                                )
                            },
                        ),
                )
                .child(
                    h_flex().gap_1().items_center().child(
                        div()
                            .id(SharedString::from(format!("cleanup-goal-{}", goal_id)))
                            .debug_selector({
                                let goal_id = goal_id.clone();
                                move || format!("cleanup-goal-{}", goal_id)
                            })
                            .child(cleanup_button),
                    ),
                );

            let mut group_div = v_flex().w_full().gap_1().child(header);

            if !is_collapsed {
                let mut row_elements = Vec::with_capacity(group.rows.len());
                for row in group.rows {
                    row_elements.push(self.render_task_row(row, snapshot, cx));
                }
                group_div = group_div.children(row_elements);
            }

            group_div
        } else {
            let is_collapsed = self
                .collapsed_goals
                .get(NO_GOAL_KEY)
                .copied()
                .unwrap_or(false);

            let header = h_flex()
                .id("goal-group-header-no-goal")
                .debug_selector(|| "goal-group-header-no-goal".to_string())
                .w_full()
                .items_center()
                .justify_between()
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .bg(cx.theme().colors().element_background)
                .on_click(cx.listener(|this, _event, _window, cx| {
                    let entry = this
                        .collapsed_goals
                        .entry(NO_GOAL_KEY.to_string())
                        .or_insert(false);
                    *entry = !*entry;
                    cx.notify();
                }))
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            Icon::new(if is_collapsed {
                                IconName::ChevronRight
                            } else {
                                IconName::ChevronDown
                            })
                            .size(IconSize::Small)
                            .color(Color::Muted),
                        )
                        .child(
                            Label::new("No goal")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(format!("({} tasks)", group.rows.len()))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                );

            let mut group_div = v_flex().w_full().gap_1().child(header);

            if !is_collapsed {
                let mut row_elements = Vec::with_capacity(group.rows.len());
                for row in group.rows {
                    row_elements.push(self.render_task_row(row, snapshot, cx));
                }
                group_div = group_div.children(row_elements);
            }

            group_div
        }
    }

    fn render_task_row(
        &self,
        row: TaskRow,
        snapshot: Option<&TaskGitSnapshot>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
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

        let task_git = snapshot.and_then(|s| s.tasks.get(&task.id));
        let has_worktree = task_git.map_or(false, |g| g.exists_on_disk);
        let worktree_path = task_git.map(|g| g.worktree_path.clone());
        let goal_git = task
            .goal_id
            .as_ref()
            .and_then(|gid| snapshot.and_then(|s| s.goals.get(gid)));
        let goal_branch_exists = goal_git.map_or(false, |g| g.branch_exists);
        let can_diff = task.goal_id.is_some() && goal_branch_exists && has_worktree;

        let open_worktree_btn = IconButton::new(
            SharedString::from(format!("open-worktree-{}", task.id)),
            IconName::FolderOpen,
        )
        .icon_size(IconSize::Small)
        .disabled(!has_worktree)
        .tooltip(if has_worktree {
            Tooltip::text("Switch Worktree (Right-click: Open in New Window)")
        } else {
            Tooltip::text("Worktree does not exist on disk")
        })
        .on_click(cx.listener({
            let path = worktree_path.clone();
            let task_id = task.id.clone();
            move |_this, _event, window, cx| {
                cx.stop_propagation();
                if let Some(path) = path.clone() {
                    let display_name = format!("agent-task-{}", task_id);
                    window.dispatch_action(Box::new(SwitchWorktree { path, display_name }), cx);
                }
            }
        }))
        .on_right_click(cx.listener({
            let path = worktree_path.clone();
            move |_this, _event: &gpui::ClickEvent, window, cx| {
                cx.stop_propagation();
                if let Some(path) = path.clone() {
                    window.dispatch_action(Box::new(OpenWorktreeInNewWindow { path }), cx);
                }
            }
        }));

        let diff_btn = IconButton::new(
            SharedString::from(format!("diff-task-{}", task.id)),
            IconName::Diff,
        )
        .icon_size(IconSize::Small)
        .disabled(!can_diff)
        .tooltip(if can_diff {
            Tooltip::text("Diff with Goal Branch")
        } else if task.goal_id.is_none() {
            Tooltip::text("Task has no goal")
        } else if !goal_branch_exists {
            Tooltip::text("Goal branch does not exist")
        } else {
            Tooltip::text("Worktree does not exist on disk")
        })
        .on_click(cx.listener({
            let goal_id = task.goal_id.clone();
            move |this, _event, window, cx| {
                cx.stop_propagation();
                if !can_diff {
                    return;
                }
                let Some(goal_id) = goal_id.clone() else {
                    return;
                };
                let Some(worktree_path) = worktree_path.clone() else {
                    return;
                };
                let base_ref: SharedString = format!("agent-goal/{goal_id}").into();
                let project = this.project.clone();
                let workspace = this.workspace.clone();
                let file_system = this.file_system.clone();

                if let Some(task_repo) =
                    find_repository_for_worktree_path(&project, &worktree_path, cx)
                {
                    if let Some(workspace) = workspace.upgrade() {
                        workspace.update(cx, |workspace, cx| {
                            git_ui::branch_diff::BranchDiff::deploy_branch_diff_with_base_ref(
                                workspace, project, task_repo, base_ref, None, window, cx,
                            );
                        });
                    }
                    return;
                }

                cx.spawn_in(window, async move |_this, cx| {
                    if !file_system.is_dir(&worktree_path).await {
                        log::warn!(
                            "failed to find worktree at {}: not a directory",
                            worktree_path.display()
                        );
                        return;
                    }

                    let find_task = project.update(cx, |project, cx| {
                        project.find_or_create_worktree(&worktree_path, true, cx)
                    });
                    let (worktree, _) = match find_task.await {
                        Ok(res) => res,
                        Err(err) => {
                            log::warn!(
                                "failed to find worktree at {}: {err:?}",
                                worktree_path.display()
                            );
                            return;
                        }
                    };
                    let scan_complete = cx
                        .update(|_window, cx| {
                            worktree
                                .read(cx)
                                .as_local()
                                .map(|local| local.scan_complete())
                        })
                        .ok()
                        .flatten();
                    if let Some(scan) = scan_complete {
                        scan.await;
                    }
                    let task_repo = cx
                        .update(|_window, cx| {
                            find_repository_for_worktree_path(&project, &worktree_path, cx)
                        })
                        .ok()
                        .flatten();
                    let Some(task_repo) = task_repo else {
                        log::warn!(
                            "could not resolve repository for worktree at {}",
                            worktree_path.display()
                        );
                        return;
                    };
                    if let Some(workspace) = workspace.upgrade() {
                        if let Err(err) = workspace.update_in(cx, |workspace, window, cx| {
                            git_ui::branch_diff::BranchDiff::deploy_branch_diff_with_base_ref(
                                workspace, project, task_repo, base_ref, None, window, cx,
                            );
                        }) {
                            log::warn!("failed to deploy branch diff in workspace: {err:?}");
                        }
                    }
                })
                .detach();
            }
        }));

        let remove_worktree_btn = IconButton::new(
            SharedString::from(format!("remove-worktree-{}", task.id)),
            IconName::Eraser,
        )
        .icon_size(IconSize::Small)
        .disabled(!has_worktree)
        .tooltip(if has_worktree {
            Tooltip::text("Remove Task Worktree from Disk")
        } else {
            Tooltip::text("Worktree does not exist on disk")
        })
        .on_click(cx.listener({
            let task_id = task.id.clone();
            move |this, _event, _window, cx| {
                cx.stop_propagation();
                if !has_worktree {
                    return;
                }
                let project = this.project.clone();
                let task_id_for_async = task_id.clone();
                let remove_task = agent::task_worktree::remove_task_worktree(project, &task_id, cx);
                cx.spawn(async move |this, cx| {
                    if let Err(e) = remove_task.await {
                        log::warn!("Failed to remove worktree for {task_id_for_async}: {e}");
                    }
                    this.update(cx, |this, cx| {
                        this.refresh_worktree_status(cx);
                    })
                    .ok();
                })
                .detach();
            }
        }));

        h_flex()
            .id(SharedString::from(format!("task-node-{}", task.id)))
            .debug_selector({
                let task_id = task.id.clone();
                move || format!("task-node-{}", task_id)
            })
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
                    .child(render_status_icon(&task.status))
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
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!("open-worktree-{}", task.id)))
                            .debug_selector({
                                let task_id = task.id.clone();
                                move || format!("open-worktree-{}", task_id)
                            })
                            .child(open_worktree_btn),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("diff-task-{}", task.id)))
                            .debug_selector({
                                let task_id = task.id.clone();
                                move || format!("diff-task-{}", task_id)
                            })
                            .child(diff_btn),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("remove-worktree-{}", task.id)))
                            .debug_selector({
                                let task_id = task.id.clone();
                                move || format!("remove-worktree-{}", task_id)
                            })
                            .child(remove_worktree_btn),
                    )
                    .when(task.status != AgentTaskStatus::Archived, |this| {
                        let task_id = task.id.clone();
                        this.child(
                            IconButton::new(
                                SharedString::from(format!("archive-task-{}", task.id)),
                                IconName::Archive,
                            )
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Archive Task"))
                            .on_click(cx.listener(
                                move |this, _event, _window, cx| {
                                    this.store
                                        .update(cx, |store, cx| store.archive_task(&task_id, cx))
                                        .detach_and_log_err(cx);
                                    cx.stop_propagation();
                                },
                            )),
                        )
                    })
                    .when(task.status == AgentTaskStatus::Archived, |this| {
                        let unarchive_task_id = task.id.clone();
                        let delete_task_id = task.id.clone();
                        this.child(
                            IconButton::new(
                                SharedString::from(format!("unarchive-task-{}", task.id)),
                                IconName::Archive,
                            )
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Unarchive Task"))
                            .on_click(cx.listener(
                                move |this, _event, _window, cx| {
                                    this.store
                                        .update(cx, |store, cx| {
                                            store.unarchive_task(&unarchive_task_id, cx)
                                        })
                                        .detach_and_log_err(cx);
                                    cx.stop_propagation();
                                },
                            )),
                        )
                        .child(
                            IconButton::new(
                                SharedString::from(format!("delete-task-{}", task.id)),
                                IconName::Trash,
                            )
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Delete Task"))
                            .on_click(cx.listener(
                                move |this, _event, _window, cx| {
                                    this.store
                                        .update(cx, |store, cx| {
                                            store.delete_task(&delete_task_id, cx)
                                        })
                                        .detach_and_log_err(cx);
                                    cx.stop_propagation();
                                },
                            )),
                        )
                    }),
            )
            .on_click(cx.listener({
                let task_id = task.id.clone();
                move |this, _event, window, cx| {
                    this.select_task(task_id.clone(), window, cx);
                }
            }))
            .into_any_element()
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
        0
    }
}

impl Render for AgentTaskPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let is_offline = store.is_offline();
        let last_error = store.last_error().map(|error| error.to_string());
        let worktree_last_error = self
            .worktree_status
            .read(cx)
            .last_error()
            .map(|error| error.to_string());

        let mut key_context = KeyContext::new_with_defaults();
        key_context.add("AgentTaskPanel");

        let view = cx.entity().downgrade();

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
                                PopoverMenu::new("task-status-filter-menu")
                                    .trigger(
                                        IconButton::new("status_filter_menu", IconName::Filter)
                                            .icon_size(IconSize::Small)
                                            .tooltip(Tooltip::text("Filter Tasks by Status")),
                                    )
                                    .anchor(gpui::Anchor::TopRight)
                                    .menu(move |window, cx| {
                                        let view = view.clone();
                                        Some(ContextMenu::build_persistent(
                                            window,
                                            cx,
                                            move |menu, _window, cx| {
                                                let mut menu = menu;
                                                let (current_filters, other_statuses) = view
                                                    .upgrade()
                                                    .map(|v| {
                                                        let panel = v.read(cx);
                                                        let mut others = Vec::new();
                                                        for task in
                                                            &panel.store.read(cx).graph().tasks
                                                        {
                                                            if let AgentTaskStatus::Other(_) =
                                                                &task.status
                                                            {
                                                                if !others.contains(&task.status) {
                                                                    others
                                                                        .push(task.status.clone());
                                                                }
                                                            }
                                                        }
                                                        others.sort_by(|a, b| {
                                                            a.as_str().cmp(b.as_str())
                                                        });
                                                        (panel.status_filters.clone(), others)
                                                    })
                                                    .unwrap_or_default();

                                                let mut all_statuses: Vec<AgentTaskStatus> =
                                                    ALL_TASK_STATUSES.to_vec();
                                                all_statuses.extend(other_statuses);

                                                for status in all_statuses {
                                                    let is_selected =
                                                        current_filters.contains(&status);
                                                    let view = view.clone();
                                                    let filter_status = status.clone();
                                                    menu = menu.toggleable_entry(
                                                        status_label(&status),
                                                        is_selected,
                                                        IconPosition::Start,
                                                        None,
                                                        move |_window, cx| {
                                                            view.update(cx, |this, cx| {
                                                                if this
                                                                    .status_filters
                                                                    .contains(&filter_status)
                                                                {
                                                                    this.status_filters
                                                                        .remove(&filter_status);
                                                                } else {
                                                                    this.status_filters.insert(
                                                                        filter_status.clone(),
                                                                    );
                                                                }
                                                                cx.notify();
                                                            })
                                                            .log_err();
                                                        },
                                                    );
                                                }

                                                menu
                                            },
                                        ))
                                    }),
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
                                        this.refresh_worktree_status(cx);
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
            .when_some(worktree_last_error, |this, error_msg| {
                this.child(
                    h_flex()
                        .id("worktree_status_error_banner")
                        .debug_selector(|| "worktree_status_error_banner".to_string())
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
                            Label::new(error_msg)
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

pub fn status_color(status: &AgentTaskStatus) -> Color {
    match status {
        AgentTaskStatus::Ready => Color::Muted,
        AgentTaskStatus::Blocked => Color::Warning,
        AgentTaskStatus::Running => Color::Info,
        AgentTaskStatus::Stale => Color::Warning,
        AgentTaskStatus::Review => Color::Accent,
        AgentTaskStatus::Completed => Color::Success,
        AgentTaskStatus::Failed => Color::Error,
        AgentTaskStatus::Archived | AgentTaskStatus::Other(_) => Color::Muted,
    }
}

pub fn status_icon(status: &AgentTaskStatus) -> IconName {
    match status {
        AgentTaskStatus::Ready => IconName::Circle,
        AgentTaskStatus::Blocked => IconName::Stop,
        AgentTaskStatus::Running => IconName::PlayFilled,
        AgentTaskStatus::Stale => IconName::Clock,
        AgentTaskStatus::Review => IconName::Eye,
        AgentTaskStatus::Completed => IconName::Check,
        AgentTaskStatus::Failed => IconName::Close,
        AgentTaskStatus::Archived => IconName::Archive,
        AgentTaskStatus::Other(_) => IconName::Circle,
    }
}

pub fn render_status_icon(status: &AgentTaskStatus) -> impl IntoElement {
    Icon::new(status_icon(status))
        .size(IconSize::Small)
        .color(status_color(status))
}

pub fn status_label(status: &AgentTaskStatus) -> &str {
    match status {
        AgentTaskStatus::Ready => "Ready",
        AgentTaskStatus::Blocked => "Blocked",
        AgentTaskStatus::Running => "Running",
        AgentTaskStatus::Stale => "Stale",
        AgentTaskStatus::Review => "Review",
        AgentTaskStatus::Completed => "Completed",
        AgentTaskStatus::Failed => "Failed",
        AgentTaskStatus::Archived => "Archived",
        AgentTaskStatus::Other(name) => name.as_ref(),
    }
}

pub fn event_kind_name(kind: &AgentTaskEventKind) -> &str {
    match kind {
        AgentTaskEventKind::Info => "INFO",
        AgentTaskEventKind::ToolCall => "TOOL",
        AgentTaskEventKind::PolicyDenied => "DENIED",
        AgentTaskEventKind::StatusChanged => "STATUS",
        AgentTaskEventKind::ReviewVerdict => "REVIEW",
        AgentTaskEventKind::Other(name) => name.as_ref(),
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

pub fn render_task_markdown(
    detail: &AgentTaskDetail,
    artifacts: &[AgentTaskArtifact],
    goal: Option<&AgentGoalSummary>,
    goal_git: Option<&GoalGitState>,
) -> String {
    let mut doc = String::new();
    doc.push_str(&format!("# {}\n\n", detail.summary.title));
    doc.push_str(&format!(
        "- **Status:** {}\n",
        status_label(&detail.summary.status)
    ));
    doc.push_str(&format!("- **Task ID:** {}\n", detail.summary.id));
    if let Some(assignee) = &detail.summary.assignee {
        doc.push_str(&format!("- **Assignee:** {}\n", assignee));
    }
    if detail.summary.attempt > 1 {
        doc.push_str(&format!("- **Attempt:** {}\n", detail.summary.attempt));
    }

    doc.push_str("\n## Description\n\n");
    doc.push_str(detail.description.trim_end());
    doc.push_str("\n\n");

    if !detail.acceptance_criteria.is_empty() {
        doc.push_str("\n## Acceptance Criteria\n\n");
        for criterion in &detail.acceptance_criteria {
            doc.push_str(&format!("- [ ] {}\n", criterion));
        }
    }

    if !artifacts.is_empty() {
        doc.push_str("\n## Artifacts\n\n");
        for (index, artifact) in artifacts.iter().enumerate() {
            if index > 0 {
                doc.push_str("\n\n");
            }
            doc.push_str(&format!("### {} — {}\n\n", artifact.kind, artifact.id));
            doc.push_str(artifact.content.trim_end());
            doc.push_str("\n\n");
        }
    }

    let task_ref: Option<agent::task_worktree::TaskRefArtifact> = artifacts
        .iter()
        .filter(|a| a.kind == "task_ref")
        .find_map(|a| serde_json::from_str(&a.content).ok());

    let conflict_files = task_ref
        .as_ref()
        .and_then(|tr| tr.merge_conflict.clone())
        .unwrap_or_default();

    if goal.is_some() || goal_git.is_some() || task_ref.is_some() {
        doc.push_str("\n## Goal Branch\n\n");
        let branch_name = if let Some(git) = goal_git {
            Some(git.branch.clone())
        } else if let Some(g) = goal {
            Some(format!("agent-goal/{}", g.goal_id))
        } else {
            task_ref.as_ref().map(|tr| tr.branch.clone())
        };
        if let Some(branch) = branch_name {
            doc.push_str(&format!("- **Branch:** `{}`\n", branch));
        }
        let tip_sha = goal_git
            .and_then(|g| g.tip_sha.as_deref())
            .or_else(|| task_ref.as_ref().and_then(|tr| tr.head_sha.as_deref()));
        if let Some(sha) = tip_sha {
            let short_sha = &sha[..7.min(sha.len())];
            doc.push_str(&format!("- **Tip SHA:** `{short_sha}` ({sha})\n"));
        }
        if let Some(g) = goal {
            doc.push_str(&format!(
                "- **Tasks:** {}/{}\n",
                g.tasks_done, g.tasks_total
            ));
        }
        if !conflict_files.is_empty() {
            doc.push_str("- **Status:** ⚠️ Merge conflict detected\n");
            doc.push_str("  - Conflicting files:\n");
            for f in &conflict_files {
                doc.push_str(&format!("    - `{f}`\n"));
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
            let kind = event_kind_name(&event.kind);
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
    status_filters: &HashSet<AgentTaskStatus>,
) -> Vec<AgentTaskSummary> {
    tasks
        .iter()
        .filter(|task| status_filters.contains(&task.status))
        .cloned()
        .collect()
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

#[derive(Debug, Clone, PartialEq)]
pub struct GoalGroup {
    pub goal_id: Option<String>,
    pub goal_summary: Option<AgentGoalSummary>,
    pub rows: Vec<TaskRow>,
}

pub fn build_task_tree_rows(
    visible_tasks: &[AgentTaskSummary],
    timestamps: &HashMap<AgentTaskId, u64>,
) -> Vec<TaskRow> {
    let roots = visible_roots(visible_tasks, timestamps);
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
            visible_tasks,
            timestamps,
            "",
            "",
            0,
            &mut visited,
            &mut rows,
        );
    }

    rows
}

pub fn build_goal_groups<'a>(
    tasks: &[AgentTaskSummary],
    goals: &[AgentGoalSummary],
    events: impl IntoIterator<Item = &'a AgentTaskEvent>,
    status_filters: &HashSet<AgentTaskStatus>,
) -> Vec<GoalGroup> {
    let timestamps = compute_task_creation_timestamps(events);
    let visible = filter_visible_tasks(tasks, status_filters);

    let goals_by_id: HashMap<&str, &AgentGoalSummary> =
        goals.iter().map(|g| (g.goal_id.as_str(), g)).collect();

    let mut tasks_by_goal: HashMap<Option<String>, Vec<AgentTaskSummary>> = HashMap::new();
    for task in visible {
        tasks_by_goal
            .entry(task.goal_id.clone())
            .or_default()
            .push(task);
    }

    let mut all_goal_ids: HashSet<String> = goals.iter().map(|g| g.goal_id.clone()).collect();
    for id in tasks_by_goal.keys().flatten() {
        all_goal_ids.insert(id.clone());
    }

    let mut sorted_goal_ids: Vec<String> = all_goal_ids.into_iter().collect();
    sorted_goal_ids.sort_by(|a, b| {
        let prio_a = goals_by_id.get(a.as_str()).map_or(i64::MAX, |g| g.priority);
        let prio_b = goals_by_id.get(b.as_str()).map_or(i64::MAX, |g| g.priority);
        prio_a.cmp(&prio_b).then_with(|| a.cmp(b))
    });

    let mut result = Vec::new();
    for goal_id in sorted_goal_ids {
        let summary = goals_by_id.get(goal_id.as_str()).copied().cloned();
        let goal_tasks = tasks_by_goal
            .remove(&Some(goal_id.clone()))
            .unwrap_or_default();
        let rows = build_task_tree_rows(&goal_tasks, &timestamps);
        result.push(GoalGroup {
            goal_id: Some(goal_id),
            goal_summary: summary,
            rows,
        });
    }

    if let Some(no_goal_tasks) = tasks_by_goal.remove(&None) {
        if !no_goal_tasks.is_empty() {
            let rows = build_task_tree_rows(&no_goal_tasks, &timestamps);
            result.push(GoalGroup {
                goal_id: None,
                goal_summary: None,
                rows,
            });
        }
    }

    result
}

#[allow(dead_code)]
pub fn build_task_rows<'a>(
    tasks: &[AgentTaskSummary],
    events: impl IntoIterator<Item = &'a AgentTaskEvent>,
    status_filters: &HashSet<AgentTaskStatus>,
) -> Vec<TaskRow> {
    let timestamps = compute_task_creation_timestamps(events);
    let visible = filter_visible_tasks(tasks, status_filters);
    build_task_tree_rows(&visible, &timestamps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::{AgentTaskEvent, AgentTaskGraph, AgentTaskProvider, AgentTaskStatus, TaskGitState};
    use context_server::ContextServerId;
    use fs::FakeFs;
    use git_ui::branch_diff::BranchDiff;
    use gpui::TestAppContext;
    use project::Project;
    use settings::SettingsStore;
    use workspace::{Item, MultiWorkspace};

    #[derive(Default)]
    struct CallCounts {
        archive_calls: Vec<AgentTaskId>,
        unarchive_calls: Vec<AgentTaskId>,
        delete_calls: Vec<AgentTaskId>,
    }

    struct TestProvider {
        offline: bool,
        tasks: Vec<AgentTaskSummary>,
        goals: Vec<AgentGoalSummary>,
        events: Vec<AgentTaskEvent>,
        calls: Arc<std::sync::Mutex<CallCounts>>,
    }

    impl Default for TestProvider {
        fn default() -> Self {
            Self {
                offline: false,
                goals: vec![
                    AgentGoalSummary {
                        goal_id: "GOAL-1".to_string(),
                        title: "Goal 1".to_string(),
                        status: "active".to_string(),
                        priority: 1,
                        tasks_total: 1,
                        tasks_done: 0,
                    },
                    AgentGoalSummary {
                        goal_id: "GOAL-2".to_string(),
                        title: "Goal 2".to_string(),
                        status: "active".to_string(),
                        priority: 2,
                        tasks_total: 0,
                        tasks_done: 0,
                    },
                ],
                tasks: vec![
                    AgentTaskSummary {
                        id: AgentTaskId::from("TASK-1"),
                        parent_id: None,
                        goal_id: Some("GOAL-1".to_string()),
                        title: "Test Task".to_string(),
                        status: AgentTaskStatus::Ready,
                        attempt: 1,
                        assignee: None,
                        write_scopes: vec![],
                    },
                    AgentTaskSummary {
                        id: AgentTaskId::from("TASK-2"),
                        parent_id: None,
                        goal_id: None,
                        title: "No Goal Task".to_string(),
                        status: AgentTaskStatus::Ready,
                        attempt: 1,
                        assignee: None,
                        write_scopes: vec![],
                    },
                ],
                events: vec![],
                calls: Arc::new(std::sync::Mutex::new(CallCounts::default())),
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
                    goals: self.goals.clone(),
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
                    goal_id: None,
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

        fn archive_task(&self, id: &AgentTaskId, _cx: &mut App) -> Task<anyhow::Result<()>> {
            self.calls.lock().unwrap().archive_calls.push(id.clone());
            Task::ready(Ok(()))
        }

        fn unarchive_task(&self, id: &AgentTaskId, _cx: &mut App) -> Task<anyhow::Result<()>> {
            self.calls.lock().unwrap().unarchive_calls.push(id.clone());
            Task::ready(Ok(()))
        }

        fn delete_task(&self, id: &AgentTaskId, _cx: &mut App) -> Task<anyhow::Result<()>> {
            self.calls.lock().unwrap().delete_calls.push(id.clone());
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
            assert_eq!(store.graph().tasks.len(), 2);
            assert_eq!(store.graph().goals.len(), 2);
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
            goal_id: None,
            title: "Completed Root".to_string(),
            status: AgentTaskStatus::Completed,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let child_failed = AgentTaskSummary {
            id: AgentTaskId::from("TASK-FAILED-CHILD"),
            parent_id: Some(AgentTaskId::from("TASK-COMPLETED")),
            goal_id: None,
            title: "Failed Child".to_string(),
            status: AgentTaskStatus::Failed,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let standalone_ready = AgentTaskSummary {
            id: AgentTaskId::from("TASK-READY"),
            parent_id: None,
            goal_id: None,
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
            goals: vec![],
            events: vec![],
            ..Default::default()
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(store, WeakEntity::new_invalid(), project, file_system, cx)
        });
        cx.run_until_parked();

        // Default: status_filters has all non-archived statuses (including Completed).
        // All 3 tasks are visible; child_failed is under root_completed.
        panel.read_with(cx, |panel, cx| {
            assert!(panel.status_filters.contains(&AgentTaskStatus::Completed));
            let store = panel.store.read(cx);
            let visible = filter_visible_tasks(&store.graph().tasks, &panel.status_filters);
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

            let rows = build_task_rows(&store.graph().tasks, store.events(), &panel.status_filters);
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

        // Filter out Completed: Completed root is filtered out; child is promoted to root.
        panel.update(cx, |panel, cx| {
            panel.status_filters.remove(&AgentTaskStatus::Completed);
            cx.notify();
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            assert!(!panel.status_filters.contains(&AgentTaskStatus::Completed));
            let store = panel.store.read(cx);
            let visible = filter_visible_tasks(&store.graph().tasks, &panel.status_filters);
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

            let rows = build_task_rows(&store.graph().tasks, store.events(), &panel.status_filters);
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].prefix, "");
            assert_eq!(rows[0].depth, 0);
            assert_eq!(rows[1].prefix, "");
            assert_eq!(rows[1].depth, 0);
        });
    }

    #[test]
    fn test_agent_task_panel_newest_first_ordering() {
        let task_a = AgentTaskSummary {
            id: AgentTaskId::from("TASK-A"),
            parent_id: None,
            goal_id: None,
            title: "Task A".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let task_b = AgentTaskSummary {
            id: AgentTaskId::from("TASK-B"),
            parent_id: None,
            goal_id: None,
            title: "Task B".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let task_b_child_1 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-B-CHILD-1"),
            parent_id: Some(AgentTaskId::from("TASK-B")),
            goal_id: None,
            title: "Task B Child 1".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let task_b_grandchild = AgentTaskSummary {
            id: AgentTaskId::from("TASK-B-GRANDCHILD"),
            parent_id: Some(AgentTaskId::from("TASK-B-CHILD-1")),
            goal_id: None,
            title: "Task B Grandchild".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let task_b_child_2 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-B-CHILD-2"),
            parent_id: Some(AgentTaskId::from("TASK-B")),
            goal_id: None,
            title: "Task B Child 2".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let task_c = AgentTaskSummary {
            id: AgentTaskId::from("TASK-C"),
            parent_id: None,
            goal_id: None,
            title: "Task C".to_string(),
            status: AgentTaskStatus::Blocked,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let task_no_events = AgentTaskSummary {
            id: AgentTaskId::from("TASK-NO-EVENTS"),
            parent_id: None,
            goal_id: None,
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
        let rows = build_task_rows(&tasks, &events, &default_status_filters());
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
                goal_id: None,
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

        let md = render_task_markdown(&detail, &artifacts, None, None);
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
                goal_id: None,
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
        let minimal_md = render_task_markdown(&minimal_detail, &[], None, None);
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
    fn test_agent_task_panel_renders_goal_markdown_and_conflict() {
        let task_id = AgentTaskId::from("TASK-GOAL-ROW");
        let goal_id = "GOAL-UI-1";

        let goal_summary = AgentGoalSummary {
            goal_id: goal_id.to_string(),
            title: "Goal UI 1".to_string(),
            status: "active".to_string(),
            priority: 1,
            tasks_total: 2,
            tasks_done: 0,
        };
        let goal_git = GoalGitState {
            branch: format!("agent-goal/{goal_id}"),
            branch_exists: true,
            tip_sha: Some("1234567890abcdef".to_string()),
        };

        let detail = AgentTaskDetail {
            summary: AgentTaskSummary {
                id: task_id.clone(),
                parent_id: None,
                goal_id: Some(goal_id.to_string()),
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

        let md = render_task_markdown(&detail, &[], Some(&goal_summary), Some(&goal_git));
        assert!(md.contains("## Goal Branch"));
        assert!(md.contains("- **Branch:** `agent-goal/GOAL-UI-1`"));
        assert!(md.contains("- **Tip SHA:** `1234567` (1234567890abcdef)"));
        assert!(md.contains("- **Tasks:** 0/2"));
        assert!(!md.contains("Merge conflict detected"));

        // When task_ref artifact contains merge conflict
        let conflict_artifact = AgentTaskArtifact {
            id: "art-task-ref".to_string(),
            task_id,
            kind: "task_ref".to_string(),
            content: serde_json::to_string(&agent::task_worktree::TaskRefArtifact {
                branch: "agent/TASK-GOAL-ROW".to_string(),
                head_sha: Some("abcdef123456".to_string()),
                merged_into: None,
                merge_conflict: Some(vec!["src/main.rs".to_string()]),
            })
            .unwrap(),
        };

        let md_conf = render_task_markdown(
            &detail,
            &[conflict_artifact],
            Some(&goal_summary),
            Some(&goal_git),
        );
        assert!(md_conf.contains("- **Status:** ⚠️ Merge conflict detected"));
        assert!(md_conf.contains("`src/main.rs`"));
    }

    #[gpui::test]
    async fn test_agent_task_panel_goal_grouping_and_collapse(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;
        let provider = Arc::new(TestProvider::default());
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(store, WeakEntity::new_invalid(), project, file_system, cx)
        });
        cx.run_until_parked();

        // Populate snapshot with goal git state
        panel.update(cx, |panel, cx| {
            panel.worktree_status.update(cx, |status, cx| {
                let mut snap = TaskGitSnapshot::default();
                snap.goals.insert(
                    "GOAL-1".to_string(),
                    GoalGitState {
                        branch: "agent-goal/GOAL-1".to_string(),
                        branch_exists: true,
                        tip_sha: Some("1234567890abcdef".to_string()),
                    },
                );
                snap.goals.insert(
                    "GOAL-2".to_string(),
                    GoalGitState {
                        branch: "agent-goal/GOAL-2".to_string(),
                        branch_exists: false,
                        tip_sha: None,
                    },
                );
                status.set_snapshot_for_test(snap, cx);
            });
        });
        cx.run_until_parked();

        // Check group headers are rendered
        cx.debug_bounds("goal-group-header-GOAL-1")
            .expect("GOAL-1 header should be rendered");
        cx.debug_bounds("goal-group-header-GOAL-2")
            .expect("GOAL-2 header should be rendered");
        cx.debug_bounds("goal-group-header-no-goal")
            .expect("No goal header should be rendered");

        // TASK-1 (under GOAL-1) and TASK-2 (under No goal) should be rendered
        cx.debug_bounds("task-node-TASK-1")
            .expect("TASK-1 should be rendered initially");
        cx.debug_bounds("task-node-TASK-2")
            .expect("TASK-2 should be rendered initially");

        // Click GOAL-1 header to collapse it
        let goal_1_header_bounds = cx.debug_bounds("goal-group-header-GOAL-1").unwrap();
        cx.simulate_click(goal_1_header_bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        // TASK-1 should now be hidden because GOAL-1 is collapsed
        assert!(cx.debug_bounds("task-node-TASK-1").is_none());
        // TASK-2 (in No goal) is still visible
        assert!(cx.debug_bounds("task-node-TASK-2").is_some());

        // Click GOAL-1 header again to expand
        cx.simulate_click(goal_1_header_bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(cx.debug_bounds("task-node-TASK-1").is_some());
    }

    #[gpui::test]
    async fn test_agent_task_panel_unknown_status_filter_and_display(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;

        let cancelled_status = AgentTaskStatus::Other(gpui::SharedString::from("cancelled"));
        let provider = Arc::new(TestProvider {
            offline: false,
            goals: vec![],
            tasks: vec![AgentTaskSummary {
                id: AgentTaskId::from("TASK-UNKNOWN"),
                parent_id: None,
                goal_id: None,
                title: "Unknown Status Task".to_string(),
                status: cancelled_status.clone(),
                attempt: 1,
                assignee: None,
                write_scopes: vec![],
            }],
            events: vec![],
            ..Default::default()
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(store, WeakEntity::new_invalid(), project, file_system, cx)
        });
        cx.run_until_parked();

        // Task is displayed in the tree
        cx.debug_bounds("task-node-TASK-UNKNOWN")
            .expect("TASK-UNKNOWN should be rendered");

        // Verify status label and icon helper functions
        assert_eq!(status_label(&cancelled_status), "cancelled");
        assert_eq!(status_icon(&cancelled_status), IconName::Circle);
        assert_eq!(status_color(&cancelled_status), Color::Muted);

        // Open status filter dropdown menu
        let filter_menu_btn = cx
            .debug_bounds("ICON-Filter")
            .expect("status filter menu button should be rendered");
        cx.simulate_click(filter_menu_btn.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        // "cancelled" appears in the filter menu
        let cancelled_entry = cx
            .debug_bounds("MENU_ITEM-cancelled")
            .expect("cancelled status entry should be in menu");

        // Toggle "cancelled" off
        cx.simulate_click(cancelled_entry.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert!(!panel.status_filters.contains(&cancelled_status));
        });

        // TASK-UNKNOWN should now be filtered out
        assert!(cx.debug_bounds("task-node-TASK-UNKNOWN").is_none());
    }

    #[gpui::test]
    async fn test_agent_task_panel_cleanup_button_state(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        file_system
            .insert_tree(
                "/main-repo",
                serde_json::json!({
                    ".git": {},
                    "main.rs": "fn main() {}",
                }),
            )
            .await;
        let project = Project::test(
            file_system.clone(),
            [std::path::Path::new("/main-repo")],
            cx,
        )
        .await;

        // 1. Goal with active task -> cleanup button is disabled
        let provider = Arc::new(TestProvider {
            goals: vec![AgentGoalSummary {
                goal_id: "GOAL-1".to_string(),
                title: "Goal 1".to_string(),
                status: "active".to_string(),
                priority: 1,
                tasks_total: 1,
                tasks_done: 0,
            }],
            tasks: vec![AgentTaskSummary {
                id: AgentTaskId::from("TASK-1"),
                parent_id: None,
                goal_id: Some("GOAL-1".to_string()),
                title: "Active Task".to_string(),
                status: AgentTaskStatus::Ready,
                attempt: 1,
                assignee: None,
                write_scopes: vec![],
            }],
            ..Default::default()
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));
        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(
                store,
                WeakEntity::new_invalid(),
                project.clone(),
                file_system.clone(),
                cx,
            )
        });
        cx.run_until_parked();

        panel.update(cx, |panel, cx| {
            panel.worktree_status.update(cx, |status, cx| {
                let mut snap = TaskGitSnapshot::default();
                snap.goals.insert(
                    "GOAL-1".to_string(),
                    GoalGitState {
                        branch: "agent-goal/GOAL-1".to_string(),
                        branch_exists: true,
                        tip_sha: Some("1234567890abcdef".to_string()),
                    },
                );
                status.set_snapshot_for_test(snap, cx);
            });
        });
        cx.run_until_parked();

        // Cleanup button is rendered in DOM
        let cleanup_btn = cx
            .debug_bounds("cleanup-goal-GOAL-1")
            .expect("cleanup button should be rendered");

        // Active task exists -> cleanup should be disabled: click does not execute cleanup
        cx.simulate_click(cleanup_btn.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(
            panel.read_with(cx, |panel, _| panel.goal_cleanup_result.is_none()),
            "cleanup should not execute when button is disabled due to active tasks"
        );

        // 2. Now with completed task
        let completed_provider = Arc::new(TestProvider {
            goals: vec![AgentGoalSummary {
                goal_id: "GOAL-1".to_string(),
                title: "Goal 1".to_string(),
                status: "active".to_string(),
                priority: 1,
                tasks_total: 1,
                tasks_done: 1,
            }],
            tasks: vec![AgentTaskSummary {
                id: AgentTaskId::from("TASK-1"),
                parent_id: None,
                goal_id: Some("GOAL-1".to_string()),
                title: "Completed Task".to_string(),
                status: AgentTaskStatus::Completed,
                attempt: 1,
                assignee: None,
                write_scopes: vec![],
            }],
            ..Default::default()
        });
        panel.update(cx, |panel, cx| {
            panel.store.update(cx, |store, cx| {
                store.set_provider(completed_provider, cx);
            });
        });
        cx.run_until_parked();

        // Restore snapshot with existing branch for completed task
        panel.update(cx, |panel, cx| {
            panel.worktree_status.update(cx, |status, cx| {
                let mut snap = TaskGitSnapshot::default();
                snap.goals.insert(
                    "GOAL-1".to_string(),
                    GoalGitState {
                        branch: "agent-goal/GOAL-1".to_string(),
                        branch_exists: true,
                        tip_sha: Some("1234567890abcdef".to_string()),
                    },
                );
                status.set_snapshot_for_test(snap, cx);
            });
        });
        cx.run_until_parked();

        // Cleanup button is enabled: clicking it executes cleanup
        let cleanup_btn = cx
            .debug_bounds("cleanup-goal-GOAL-1")
            .expect("cleanup button should still be rendered");
        cx.simulate_click(cleanup_btn.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let res = panel
                .goal_cleanup_result
                .as_ref()
                .expect("cleanup should have executed when button is enabled");
            assert_eq!(res.goal_id, "GOAL-1");
        });

        // 3. Goal branch does not exist -> cleanup button is disabled
        panel.update(cx, |panel, cx| {
            panel.worktree_status.update(cx, |status, cx| {
                let mut snap = TaskGitSnapshot::default();
                snap.goals.insert(
                    "GOAL-1".to_string(),
                    GoalGitState {
                        branch: "agent-goal/GOAL-1".to_string(),
                        branch_exists: false,
                        tip_sha: None,
                    },
                );
                status.set_snapshot_for_test(snap, cx);
            });
            panel.goal_cleanup_result = None;
        });
        cx.run_until_parked();

        let cleanup_btn = cx
            .debug_bounds("cleanup-goal-GOAL-1")
            .expect("cleanup button should still be rendered");
        cx.simulate_click(cleanup_btn.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(
            panel.read_with(cx, |panel, _| panel.goal_cleanup_result.is_none()),
            "cleanup should not execute when goal branch does not exist"
        );
    }

    #[gpui::test]
    async fn test_agent_task_panel_archive_unarchive_delete_actions(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;

        let task_active = AgentTaskSummary {
            id: AgentTaskId::from("TASK-ACTIVE"),
            parent_id: None,
            goal_id: None,
            title: "Active Task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let task_archived = AgentTaskSummary {
            id: AgentTaskId::from("TASK-ARCHIVED"),
            parent_id: None,
            goal_id: None,
            title: "Archived Task".to_string(),
            status: AgentTaskStatus::Archived,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };

        let calls = Arc::new(std::sync::Mutex::new(CallCounts::default()));
        let provider = Arc::new(TestProvider {
            offline: false,
            tasks: vec![task_active, task_archived],
            goals: vec![],
            events: vec![],
            calls: calls.clone(),
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(store, WeakEntity::new_invalid(), project, file_system, cx)
        });
        cx.run_until_parked();

        // Active task archive button is present
        let archive_btn_bounds = cx
            .debug_bounds("ICON-Archive")
            .expect("archive button should be rendered for active task");
        cx.simulate_click(archive_btn_bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        // Check archive was called and task selection did NOT trigger
        assert_eq!(
            calls.lock().unwrap().archive_calls,
            vec![AgentTaskId::from("TASK-ACTIVE")]
        );
        panel.read_with(cx, |panel, _| {
            assert!(panel.selected_task_id.is_none());
        });

        // Filter to only Archived status to show archived task
        panel.update(cx, |panel, cx| {
            panel.status_filters.clear();
            panel.status_filters.insert(AgentTaskStatus::Archived);
            cx.notify();
        });
        cx.run_until_parked();

        // Archived task unarchive and delete buttons are present
        let unarchive_btn_bounds = cx
            .debug_bounds("ICON-Archive")
            .expect("unarchive button should be rendered for archived task");
        let delete_btn_bounds = cx
            .debug_bounds("ICON-Trash")
            .expect("delete button should be rendered for archived task");

        // Clicking unarchive
        cx.simulate_click(unarchive_btn_bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            calls.lock().unwrap().unarchive_calls,
            vec![AgentTaskId::from("TASK-ARCHIVED")]
        );
        panel.read_with(cx, |panel, _| {
            assert!(panel.selected_task_id.is_none());
        });

        // Clicking delete
        cx.simulate_click(delete_btn_bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            calls.lock().unwrap().delete_calls,
            vec![AgentTaskId::from("TASK-ARCHIVED")]
        );
        panel.read_with(cx, |panel, _| {
            assert!(panel.selected_task_id.is_none());
        });

        // Restore default filters and test task selection
        panel.update(cx, |panel, cx| {
            panel.status_filters = default_status_filters();
            cx.notify();
        });
        cx.run_until_parked();

        // Clicking the task node (left side, avoiding action buttons) triggers task selection
        let task_node_bounds = cx
            .debug_bounds("task-node-TASK-ACTIVE")
            .expect("task node should be rendered");
        cx.simulate_click(
            gpui::Point::new(
                task_node_bounds.left() + gpui::px(10.0),
                task_node_bounds.center().y,
            ),
            gpui::Modifiers::default(),
        );
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.selected_task_id,
                Some(AgentTaskId::from("TASK-ACTIVE"))
            );
        });
    }

    #[gpui::test]
    async fn test_agent_task_panel_status_filter_dropdown(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;
        let provider = Arc::new(TestProvider::default());
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(store, WeakEntity::new_invalid(), project, file_system, cx)
        });
        cx.run_until_parked();

        // Default filters contain all non-archived statuses
        panel.read_with(cx, |panel, _| {
            assert!(panel.status_filters.contains(&AgentTaskStatus::Ready));
            assert!(panel.status_filters.contains(&AgentTaskStatus::Completed));
            assert!(!panel.status_filters.contains(&AgentTaskStatus::Archived));
        });

        // Click status filter menu button to open dropdown
        let filter_menu_btn = cx
            .debug_bounds("ICON-Filter")
            .expect("status filter menu button should be rendered");
        cx.simulate_click(filter_menu_btn.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        // Click "Archived" entry to toggle it on
        let archived_entry = cx
            .debug_bounds("MENU_ITEM-Archived")
            .expect("Archived entry should be present in status filter menu");
        cx.simulate_click(archived_entry.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert!(panel.status_filters.contains(&AgentTaskStatus::Archived));
        });

        // Click "Archived" entry again to toggle it off (persistent menu stays open)
        let archived_entry_again = cx
            .debug_bounds("MENU_ITEM-Archived")
            .expect("Archived entry should still be present in persistent menu");
        cx.simulate_click(archived_entry_again.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert!(!panel.status_filters.contains(&AgentTaskStatus::Archived));
        });
    }

    #[gpui::test]
    async fn test_agent_task_panel_activation_priority(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;
        let provider = Arc::new(TestProvider::default());
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(store, WeakEntity::new_invalid(), project, file_system, cx)
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.activation_priority(), 0);
        });
    }

    #[gpui::test]
    async fn test_agent_task_panel_task_row_action_buttons(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        file_system
            .insert_tree(
                "/main-repo",
                serde_json::json!({
                    ".git": {},
                    "main.rs": "fn main() {}",
                }),
            )
            .await;
        let project = Project::test(
            file_system.clone(),
            [std::path::Path::new("/main-repo")],
            cx,
        )
        .await;
        let provider = Arc::new(TestProvider::default());
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let switched_worktrees = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let opened_in_new_window = Arc::new(parking_lot::Mutex::new(Vec::new()));

        struct ActionCaptureView {
            panel: Entity<AgentTaskPanel>,
            focus_handle: FocusHandle,
            switched: Arc<parking_lot::Mutex<Vec<SwitchWorktree>>>,
            opened: Arc<parking_lot::Mutex<Vec<OpenWorktreeInNewWindow>>>,
        }

        impl Focusable for ActionCaptureView {
            fn focus_handle(&self, _cx: &App) -> FocusHandle {
                self.focus_handle.clone()
            }
        }

        impl Render for ActionCaptureView {
            fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let switched = self.switched.clone();
                let opened = self.opened.clone();
                h_flex()
                    .track_focus(&self.focus_handle)
                    .size_full()
                    .on_action(
                        cx.listener(move |_this, action: &SwitchWorktree, _window, _cx| {
                            switched.lock().push(action.clone());
                        }),
                    )
                    .on_action(cx.listener(
                        move |_this, action: &OpenWorktreeInNewWindow, _window, _cx| {
                            opened.lock().push(action.clone());
                        },
                    ))
                    .child(self.panel.clone())
            }
        }

        let (capture_view, cx) = cx.add_window_view(|_window, cx| {
            let panel = cx.new(|cx| {
                AgentTaskPanel::new(
                    store,
                    WeakEntity::new_invalid(),
                    project.clone(),
                    file_system.clone(),
                    cx,
                )
            });
            ActionCaptureView {
                panel,
                focus_handle: cx.focus_handle(),
                switched: switched_worktrees.clone(),
                opened: opened_in_new_window.clone(),
            }
        });
        cx.run_until_parked();

        let panel = capture_view.read_with(cx, |view, _| view.panel.clone());
        cx.focus(&capture_view);

        // 1. Action buttons exist for task
        let open_btn_bounds = cx.debug_bounds("open-worktree-TASK-1");
        assert!(open_btn_bounds.is_some());
        let diff_btn_bounds = cx.debug_bounds("diff-task-TASK-1");
        assert!(diff_btn_bounds.is_some());
        let remove_btn_bounds = cx.debug_bounds("remove-worktree-TASK-1");
        assert!(remove_btn_bounds.is_some());

        // 2a. Populate snapshot with goal branch existing, but worktree NOT on disk (has_worktree = false)
        panel.update(cx, |panel, cx| {
            panel.worktree_status.update(cx, |status, cx| {
                let mut snap = TaskGitSnapshot::default();
                snap.tasks.insert(
                    AgentTaskId::from("TASK-1"),
                    TaskGitState {
                        worktree_path: std::path::PathBuf::from("/fake/task-1"),
                        exists_on_disk: false,
                    },
                );
                snap.goals.insert(
                    "GOAL-1".to_string(),
                    GoalGitState {
                        branch: "agent-goal/GOAL-1".to_string(),
                        branch_exists: true,
                        tip_sha: Some("1234567890abcdef".to_string()),
                    },
                );
                status.set_snapshot_for_test(snap, cx);
            });
        });
        cx.run_until_parked();

        // When worktree does not exist on disk, open_btn is disabled, and diff_btn is disabled
        let open_btn = cx
            .debug_bounds("open-worktree-TASK-1")
            .expect("open worktree button should exist");
        let diff_btn = cx
            .debug_bounds("diff-task-TASK-1")
            .expect("diff task button should exist");
        cx.simulate_click(open_btn.center(), gpui::Modifiers::default());
        cx.simulate_click(diff_btn.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(
            switched_worktrees.lock().is_empty(),
            "no switch action should be dispatched when worktree does not exist on disk"
        );

        // 2b. Populate snapshot with worktree on disk (has_worktree = true) and goal branch existing
        panel.update(cx, |panel, cx| {
            panel.worktree_status.update(cx, |status, cx| {
                let mut snap = TaskGitSnapshot::default();
                snap.tasks.insert(
                    AgentTaskId::from("TASK-1"),
                    TaskGitState {
                        worktree_path: std::path::PathBuf::from("/fake/task-1"),
                        exists_on_disk: true,
                    },
                );
                snap.goals.insert(
                    "GOAL-1".to_string(),
                    GoalGitState {
                        branch: "agent-goal/GOAL-1".to_string(),
                        branch_exists: true,
                        tip_sha: Some("1234567890abcdef".to_string()),
                    },
                );
                status.set_snapshot_for_test(snap, cx);
            });
        });
        cx.run_until_parked();

        // Buttons exist and are clickable
        let open_btn = cx
            .debug_bounds("open-worktree-TASK-1")
            .expect("open worktree button should exist");
        let diff_btn = cx
            .debug_bounds("diff-task-TASK-1")
            .expect("diff task button should exist");
        let remove_btn = cx
            .debug_bounds("remove-worktree-TASK-1")
            .expect("remove worktree button should exist");

        // Simulate left click on open worktree (triggers SwitchWorktree action with agent-task-{id})
        cx.simulate_click(open_btn.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        let switched = switched_worktrees.lock().clone();
        assert_eq!(switched.len(), 1);
        assert_eq!(switched[0].display_name, "agent-task-TASK-1");
        assert_eq!(switched[0].path, std::path::PathBuf::from("/fake/task-1"));

        // Simulate right click on open worktree (triggers OpenWorktreeInNewWindow action)
        cx.simulate_mouse_down(
            open_btn.center(),
            gpui::MouseButton::Right,
            gpui::Modifiers::default(),
        );
        cx.simulate_mouse_up(
            open_btn.center(),
            gpui::MouseButton::Right,
            gpui::Modifiers::default(),
        );
        cx.run_until_parked();

        let opened = opened_in_new_window.lock().clone();
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].path, std::path::PathBuf::from("/fake/task-1"));

        // Simulate click on diff button:
        // since /fake/task-1 does not exist on disk, find_or_create_worktree (creatable=false)
        // returns an error, so no worktree is added to project
        cx.simulate_click(diff_btn.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(
            project.read_with(cx, |p, cx| {
                p.worktrees(cx)
                    .all(|w| w.read(cx).abs_path().as_ref() != std::path::Path::new("/fake/task-1"))
            }),
            "non-existent worktree path must not be created or registered in project"
        );

        // Simulate click on remove worktree button:
        // triggers remove_task_worktree and refreshes worktree snapshot
        cx.simulate_click(remove_btn.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        // After remove worktree, refresh_worktree_status has updated the snapshot:
        // /fake/task-1 does not exist on disk, so exists_on_disk becomes false
        panel.read_with(cx, |panel, cx| {
            let snap = panel.worktree_status.read(cx).snapshot();
            if let Some(snap) = snap {
                if let Some(task_state) = snap.tasks.get(&AgentTaskId::from("TASK-1")) {
                    assert!(
                        !task_state.exists_on_disk,
                        "task worktree must no longer exist on disk after remove"
                    );
                }
            }
        });
    }

    #[gpui::test]
    async fn test_agent_task_panel_diff_button_targets_task_worktree_repository(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        file_system
            .insert_tree(
                "/main-repo",
                serde_json::json!({
                    ".git": {},
                    "main.rs": "fn main() {}",
                }),
            )
            .await;
        file_system
            .insert_tree(
                "/fake/task-1",
                serde_json::json!({
                    ".git": {},
                    "task.rs": "fn task() {}",
                }),
            )
            .await;

        let project = Project::test(
            file_system.clone(),
            [std::path::Path::new("/main-repo")],
            cx,
        )
        .await;
        cx.run_until_parked();

        let active_repo = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("main-repo should be active repository")
        });
        let active_work_dir =
            active_repo.read_with(cx, |repo, _| repo.snapshot().work_directory_abs_path);
        assert_eq!(active_work_dir.as_ref(), std::path::Path::new("/main-repo"));

        let provider = Arc::new(TestProvider::default());
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();

        let mut cx = gpui::VisualTestContext::from_window(multi_workspace.into(), cx);

        let workspace_weak = workspace.downgrade();
        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = cx.new(|cx| {
                AgentTaskPanel::new(
                    store,
                    workspace_weak,
                    project.clone(),
                    file_system.clone(),
                    cx,
                )
            });
            workspace.add_panel(panel.clone(), window, cx);
            workspace.focus_panel::<AgentTaskPanel>(window, cx);
            panel
        });
        cx.run_until_parked();

        panel.update(&mut cx, |panel, cx| {
            panel.worktree_status.update(cx, |status, cx| {
                let mut snap = TaskGitSnapshot::default();
                snap.tasks.insert(
                    AgentTaskId::from("TASK-1"),
                    TaskGitState {
                        worktree_path: std::path::PathBuf::from("/fake/task-1"),
                        exists_on_disk: true,
                    },
                );
                snap.goals.insert(
                    "GOAL-1".to_string(),
                    GoalGitState {
                        branch: "agent-goal/GOAL-1".to_string(),
                        branch_exists: true,
                        tip_sha: Some("1234567890abcdef".to_string()),
                    },
                );
                status.set_snapshot_for_test(snap, cx);
            });
        });
        cx.run_until_parked();

        // 1. Task repo is not yet in project.repositories
        let task_worktree_path = std::path::Path::new("/fake/task-1");
        assert!(project.read_with(&cx, |_, cx| {
            find_repository_for_worktree_path(&project, task_worktree_path, cx).is_none()
        }));

        // 2. Click diff button: should find or create worktree, resolve task repository, and deploy BranchDiff
        let diff_btn = cx
            .debug_bounds("diff-task-TASK-1")
            .expect("diff task button should exist");
        cx.simulate_click(diff_btn.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        // Task repository was resolved and loaded into project
        let task_repo = project
            .read_with(&cx, |_, cx| {
                find_repository_for_worktree_path(&project, task_worktree_path, cx)
            })
            .expect("task repository should now be resolved in project");
        let task_work_dir =
            task_repo.read_with(&cx, |repo, _| repo.snapshot().work_directory_abs_path);
        assert_eq!(task_work_dir.as_ref(), task_worktree_path);
        let task_repo_id = task_repo.read_with(&cx, |repo, _| repo.id);
        let active_repo_id = active_repo.read_with(&cx, |repo, _| repo.id);
        assert_ne!(
            task_repo_id, active_repo_id,
            "task repository must be different from active repository"
        );

        // BranchDiff was deployed in the workspace for the goal base ref
        let diff_items: Vec<_> =
            workspace.read_with(&cx, |ws, cx| ws.items_of_type::<BranchDiff>(cx).collect());
        assert_eq!(
            diff_items.len(),
            1,
            "exactly one branch diff should be deployed"
        );

        let tab_text = diff_items[0].read_with(&cx, |item, cx| item.tab_content_text(0, cx));
        assert_eq!(
            tab_text, "Changes since agent-goal/GOAL-1",
            "diff tab text must reflect agent-goal/GOAL-1"
        );

        // Re-deploying with task_repo matches and reuses existing BranchDiff item
        workspace.update_in(&mut cx, |ws, window, cx| {
            BranchDiff::deploy_branch_diff_with_base_ref(
                ws,
                project.clone(),
                task_repo.clone(),
                "agent-goal/GOAL-1".into(),
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let count = workspace.read_with(&cx, |ws, cx| ws.items_of_type::<BranchDiff>(cx).count());
        assert_eq!(
            count, 1,
            "deploying with task_repo should reuse the existing item"
        );
    }

    #[gpui::test]
    async fn test_agent_task_panel_orphan_worktrees_rendering_and_delete(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        file_system
            .insert_tree(
                "/main-repo",
                serde_json::json!({
                    ".git": {},
                    "main.rs": "fn main() {}",
                }),
            )
            .await;
        file_system
            .create_dir(std::path::Path::new("/fake/orphan-42"))
            .await
            .unwrap();

        let project = Project::test(
            file_system.clone(),
            [std::path::Path::new("/main-repo")],
            cx,
        )
        .await;
        let provider = Arc::new(TestProvider::default());
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(
                store,
                WeakEntity::new_invalid(),
                project.clone(),
                file_system.clone(),
                cx,
            )
        });
        cx.run_until_parked();

        // Initially no orphan worktrees
        assert!(cx.debug_bounds("orphan-worktrees-section").is_none());

        // Populate snapshot with an orphan worktree
        panel.update(cx, |panel, cx| {
            panel.worktree_status.update(cx, |status, cx| {
                let mut snap = TaskGitSnapshot::default();
                snap.orphan_worktrees.push(OrphanWorktree {
                    task_id_hint: "ORPHAN-42".to_string(),
                    path: std::path::PathBuf::from("/fake/orphan-42"),
                });
                status.set_snapshot_for_test(snap, cx);
            });
        });
        cx.run_until_parked();

        // Orphan section and orphan item are rendered
        assert!(
            file_system
                .is_dir(std::path::Path::new("/fake/orphan-42"))
                .await
        );
        assert!(cx.debug_bounds("orphan-worktrees-section").is_some());
        assert!(cx.debug_bounds("orphan-worktree-ORPHAN-42").is_some());

        // Click delete orphan button
        let delete_orphan_btn = cx
            .debug_bounds("delete-orphan-ORPHAN-42")
            .expect("delete orphan button should be rendered");
        cx.simulate_click(delete_orphan_btn.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        // The orphan directory was deleted from the file system
        assert!(
            !file_system
                .is_dir(std::path::Path::new("/fake/orphan-42"))
                .await,
            "orphan worktree folder should be removed from disk"
        );

        // After deletion and snapshot refresh, the orphan section and item disappeared from the DOM
        assert!(cx.debug_bounds("orphan-worktree-ORPHAN-42").is_none());
        assert!(cx.debug_bounds("orphan-worktrees-section").is_none());
    }

    #[gpui::test]
    async fn test_agent_task_panel_worktree_status_error_banner(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        // 1. Project with git repo -> snapshot refresh succeeds without error banner
        file_system
            .insert_tree(
                "/main-repo",
                serde_json::json!({
                    ".git": {},
                    "main.rs": "fn main() {}",
                }),
            )
            .await;
        let project = Project::test(
            file_system.clone(),
            [std::path::Path::new("/main-repo")],
            cx,
        )
        .await;
        let provider = Arc::new(TestProvider::default());
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider.clone(), cx)));

        let (_panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(
                store,
                WeakEntity::new_invalid(),
                project.clone(),
                file_system.clone(),
                cx,
            )
        });
        cx.run_until_parked();

        // Initially no error banner when snapshot is error-free
        assert!(cx.debug_bounds("worktree_status_error_banner").is_none());

        // 2. Panel with empty project has last_error and renders warning banner
        let empty_project = Project::test(file_system.clone(), [], cx).await;
        let store_empty = cx.update(|_window, cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));
        let (panel_empty, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(
                store_empty,
                WeakEntity::new_invalid(),
                empty_project,
                file_system.clone(),
                cx,
            )
        });
        cx.run_until_parked();

        // Status should now have last_error
        panel_empty.read_with(cx, |panel, cx| {
            assert!(panel.worktree_status.read(cx).last_error().is_some());
        });

        // The error banner is rendered in the DOM
        assert!(
            cx.debug_bounds("worktree_status_error_banner").is_some(),
            "worktree_status_error_banner should be rendered when last_error is present"
        );
    }
}
