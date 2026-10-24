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

use anyhow::Context as _;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use agent::{
    AgentGoalStatus, AgentGoalSummary, AgentTaskArtifact, AgentTaskDetail, AgentTaskEventKind,
    AgentTaskId, AgentTaskMetrics, AgentTaskStatus, AgentTaskStore, AgentTaskSummary, GoalGitState,
    OrphanWorktree, TaskWorktreeStatus,
};
use agent_settings::AgentSettings;
use editor::Editor;
use fs::Fs;
use gpui::{
    Action, App, Context, DismissEvent, Div, ElementId, Entity, EventEmitter, FocusHandle, Focusable,
    KeyContext, MouseButton, MouseDownEvent, Pixels, Point, Subscription, Task, WeakEntity, Window,
    actions, anchored, deferred, prelude::*, px,
};
use multi_buffer::MultiBuffer;
use project::{Project, git_store::Repository};
use settings::{Settings, SettingsStore};
use ui::{
    Color, CommonAnimationExt, ContextMenu, ContextMenuEntry, DiffStat, Icon, IconButton, IconName,
    IconPosition, IconSize, Label, LabelSize, PopoverMenu, Tooltip, prelude::*,
};
use util::ResultExt;
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::{PathList, ToggleZoom, Workspace};
use zed_actions::SwitchWorktree;

#[allow(dead_code)]
pub const ALL_TASK_STATUSES: [AgentTaskStatus; 9] = [
    AgentTaskStatus::Ready,
    AgentTaskStatus::Blocked,
    AgentTaskStatus::Running,
    AgentTaskStatus::Stale,
    AgentTaskStatus::Review,
    AgentTaskStatus::Completed,
    AgentTaskStatus::Failed,
    AgentTaskStatus::Cancelled,
    AgentTaskStatus::Archived,
];

pub const NO_GOAL_KEY: &str = "__no_goal__";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FilterCategory {
    Ready,
    Running,
    Blocked,
    Failed,
    Done,
    Cancelled,
    Archived,
}

impl FilterCategory {
    pub const ALL: [FilterCategory; 7] = [
        FilterCategory::Ready,
        FilterCategory::Running,
        FilterCategory::Blocked,
        FilterCategory::Failed,
        FilterCategory::Done,
        FilterCategory::Cancelled,
        FilterCategory::Archived,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            FilterCategory::Ready => "Ready",
            FilterCategory::Running => "Running",
            FilterCategory::Blocked => "Blocked",
            FilterCategory::Failed => "Failed",
            FilterCategory::Done => "Done",
            FilterCategory::Cancelled => "Cancelled",
            FilterCategory::Archived => "Archived",
        }
    }

    pub fn matches_task(&self, status: &AgentTaskStatus) -> bool {
        match self {
            FilterCategory::Ready => matches!(status, AgentTaskStatus::Ready),
            FilterCategory::Running => matches!(status, AgentTaskStatus::Running),
            FilterCategory::Blocked => matches!(status, AgentTaskStatus::Blocked),
            FilterCategory::Failed => matches!(status, AgentTaskStatus::Failed),
            FilterCategory::Done => matches!(status, AgentTaskStatus::Completed),
            FilterCategory::Cancelled => matches!(status, AgentTaskStatus::Cancelled),
            FilterCategory::Archived => matches!(status, AgentTaskStatus::Archived),
        }
    }

    pub fn matches_goal(&self, status: &AgentGoalStatus) -> bool {
        match self {
            FilterCategory::Ready => matches!(status, AgentGoalStatus::Running),
            FilterCategory::Running => matches!(status, AgentGoalStatus::Running),
            FilterCategory::Blocked => matches!(status, AgentGoalStatus::Blocked),
            FilterCategory::Failed => matches!(status, AgentGoalStatus::Failed),
            FilterCategory::Done => matches!(status, AgentGoalStatus::Completed),
            FilterCategory::Cancelled => matches!(status, AgentGoalStatus::Cancelled),
            FilterCategory::Archived => matches!(status, AgentGoalStatus::Archived),
        }
    }
}

pub fn default_category_filters() -> HashSet<FilterCategory> {
    HashSet::from([
        FilterCategory::Ready,
        FilterCategory::Running,
        FilterCategory::Blocked,
        FilterCategory::Failed,
        FilterCategory::Done,
        FilterCategory::Cancelled,
    ])
}

#[allow(dead_code)]
pub fn default_status_filters() -> HashSet<AgentTaskStatus> {
    HashSet::from([
        AgentTaskStatus::Ready,
        AgentTaskStatus::Blocked,
        AgentTaskStatus::Running,
        AgentTaskStatus::Stale,
        AgentTaskStatus::Review,
        AgentTaskStatus::Completed,
        AgentTaskStatus::Failed,
        AgentTaskStatus::Cancelled,
    ])
}

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

actions!(agent, [ToggleAgentTaskPanel]);

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
        let store = cx
            .new(|cx| agent::AgentTaskStore::new_with_project(provider, Some(project.clone()), cx));

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoveredRow {
    Goal(String),
    Task(AgentTaskId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextMenuTarget {
    Goal(String),
    Task(AgentTaskId),
}

pub struct DeployedContextMenu {
    pub menu: Entity<ContextMenu>,
    pub position: Point<Pixels>,
    pub anchor: Option<gpui::Anchor>,
    pub _subscription: Subscription,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenamingTarget {
    Goal(String),
    Task(AgentTaskId),
}

struct TaskOperationErrorToast;
pub(crate) struct WorktreeDeletedToast;

pub struct RenamingItem {
    pub target: RenamingTarget,
    pub editor: Entity<Editor>,
    pub initial_title: String,
    pub _subscription: Subscription,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewItem {
    Task(AgentTaskId),
    Goal(String),
}

pub struct AgentTaskPanel {
    pub store: Entity<AgentTaskStore>,
    pub worktree_status: Entity<TaskWorktreeStatus>,
    pub selected_task_id: Option<AgentTaskId>,
    pub selected_detail: Option<AgentTaskDetail>,
    pub category_filters: HashSet<FilterCategory>,
    pub other_status_filters: HashSet<AgentTaskStatus>,
    pub seen_other_statuses: HashSet<AgentTaskStatus>,
    pub search_query: String,
    search_editor: Option<Entity<Editor>>,
    _search_subscription: Option<Subscription>,
    pub collapsed_goals: HashMap<String, bool>,
    pub collapsed_tasks: HashSet<AgentTaskId>,
    pub seen_tasks: HashSet<AgentTaskId>,
    pub hovered_row: Option<HoveredRow>,
    pub context_menu: Option<DeployedContextMenu>,
    pub goal_cleanup_result: Option<agent::task_worktree::GoalCleanupResult>,
    pub zoomed: bool,
    pub editing_item: Option<RenamingItem>,
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    file_system: Arc<dyn Fs>,
    _fetch_detail_task: Option<Task<()>>,
}

impl AgentTaskPanel {
    pub fn show_error_toast(
        workspace: WeakEntity<Workspace>,
        message: impl Into<std::borrow::Cow<'static, str>>,
        cx: &mut App,
    ) {
        if let Some(workspace) = workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    workspace::Toast::new(
                        workspace::notifications::NotificationId::unique::<TaskOperationErrorToast>(
                        ),
                        message,
                    ),
                    cx,
                );
            });
        }
    }

    pub fn humanize_error(error: &anyhow::Error) -> String {
        let err_str = format!("{error:#}");
        Self::humanize_error_str(&err_str)
    }

    pub fn humanize_error_str(err_str: &str) -> String {
        let lower = err_str.to_lowercase();
        if lower.contains("database is locked") || lower.contains("database locked") {
            "Database is locked by another process. Please wait and try again.".to_string()
        } else if lower.contains("index.lock")
            || lower.contains("file is locked")
            || lower.contains("being used by another process")
            || lower.contains("could not lock config file")
            || lower.contains("resource temporarily unavailable")
        {
            "Git index or file is locked by another process. Please wait and try again.".to_string()
        } else if lower.contains("not a valid object name")
            || (lower.contains("branch") && lower.contains("not found"))
            || lower.contains("did not match any file")
            || (lower.contains("reference") && lower.contains("not found"))
            || lower.contains("no such branch")
        {
            format!("Git branch not found: {err_str}")
        } else {
            err_str.to_string()
        }
    }

    pub fn format_task_panel_error(action: Option<&str>, error: &anyhow::Error) -> String {
        let humanized = Self::humanize_error(error);
        if let Some(action) = action {
            format!("Task Panel: {action}: {humanized}")
        } else if humanized.starts_with("Task Panel: ") {
            humanized
        } else {
            format!("Task Panel: {humanized}")
        }
    }

    #[allow(dead_code)]
    pub fn spawn_with_error_toast<F>(&self, task: F, _window: &mut Window, cx: &mut Context<Self>)
    where
        F: Future<Output = anyhow::Result<()>> + 'static,
    {
        Self::spawn_task_with_error_toast(task, self.workspace.clone(), cx);
    }

    pub fn spawn_with_action_error_toast<F>(
        &self,
        task: F,
        action: &'static str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        F: Future<Output = anyhow::Result<()>> + 'static,
    {
        Self::spawn_task_with_action_error_toast(task, Some(action), self.workspace.clone(), cx);
    }

    #[allow(dead_code)]
    pub fn spawn_task_with_error_toast<F>(task: F, workspace: WeakEntity<Workspace>, cx: &mut App)
    where
        F: Future<Output = anyhow::Result<()>> + 'static,
    {
        Self::spawn_task_with_action_error_toast(task, None, workspace, cx);
    }

    pub fn spawn_task_with_action_error_toast<F>(
        task: F,
        action: Option<&'static str>,
        workspace: WeakEntity<Workspace>,
        cx: &mut App,
    ) where
        F: Future<Output = anyhow::Result<()>> + 'static,
    {
        cx.spawn(async move |cx| {
            if let Err(error) = task.await {
                let message = Self::format_task_panel_error(action, &error);
                cx.update(|cx| {
                    Self::show_error_toast(workspace, message, cx);
                });
            }
        })
        .detach();
    }

    pub fn toggle_zoom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.zoomed {
            cx.emit(PanelEvent::ZoomOut);
        } else {
            if !self.focus_handle(cx).contains_focused(window, cx) {
                self.activation_focus_handle(cx).focus(window, cx);
            }
            cx.emit(PanelEvent::ZoomIn);
        }
    }

    pub fn start_renaming(
        &mut self,
        target: RenamingTarget,
        initial_title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing_item.is_some() {
            self.confirm_renaming(window, cx);
        }

        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(initial_title.clone(), window, cx);
            editor
        });

        let subscription = cx.subscribe_in(
            &editor,
            window,
            move |this, _editor, event: &editor::EditorEvent, window, cx| {
                if matches!(event, editor::EditorEvent::Blurred) {
                    this.confirm_renaming(window, cx);
                }
            },
        );

        editor.update(cx, |editor, cx| {
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor.focus_handle(cx).focus(window, cx);
        });

        self.editing_item = Some(RenamingItem {
            target,
            editor,
            initial_title,
            _subscription: subscription,
        });
        cx.notify();
    }

    pub fn confirm_renaming(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(renaming) = self.editing_item.take() else {
            return;
        };
        self.focus_handle.focus(window, cx);
        let new_title = renaming.editor.read(cx).text(cx).trim().to_string();
        if new_title.is_empty() || new_title == renaming.initial_title {
            cx.notify();
            return;
        }
        match renaming.target {
            RenamingTarget::Goal(goal_id) => {
                let store = self.store.clone();
                let task = store.update(cx, |store, cx| {
                    store.update_goal_title(&goal_id, &new_title, cx)
                });
                self.spawn_with_action_error_toast(task, "Rename Goal", window, cx);
            }
            RenamingTarget::Task(task_id) => {
                let store = self.store.clone();
                let task = store.update(cx, |store, cx| {
                    store.update_task_title(&task_id, &new_title, cx)
                });
                self.spawn_with_action_error_toast(task, "Rename Task", window, cx);
            }
        }
        cx.notify();
    }

    pub fn cancel_renaming(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing_item.take().is_some() {
            self.focus_handle.focus(window, cx);
            cx.notify();
        }
    }

    fn render_rename_editor(
        &self,
        editor: &Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .flex_1()
            .min_w_0()
            .on_action(
                cx.listener(|this, _: &editor::actions::Newline, window, cx| {
                    this.confirm_renaming(window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.confirm_renaming(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &editor::actions::Cancel, window, cx| {
                    this.cancel_renaming(window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                this.cancel_renaming(window, cx);
            }))
            .child(editor.clone())
            .into_any_element()
    }

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
                this.refresh_worktree_status_if_needed(cx);
            }
            for task in &tasks {
                if this.seen_tasks.insert(task.id.clone()) {
                    this.collapsed_tasks.insert(task.id.clone());
                }
                if let AgentTaskStatus::Other(_) = &task.status {
                    if this.seen_other_statuses.insert(task.status.clone()) {
                        this.other_status_filters.insert(task.status.clone());
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
        let mut other_status_filters = HashSet::new();
        let mut seen_tasks = HashSet::new();
        let mut collapsed_tasks = HashSet::new();
        {
            let store_read = store.read(cx);
            for task in &store_read.graph().tasks {
                if seen_tasks.insert(task.id.clone()) {
                    collapsed_tasks.insert(task.id.clone());
                }
                if let AgentTaskStatus::Other(_) = &task.status {
                    if seen_other_statuses.insert(task.status.clone()) {
                        other_status_filters.insert(task.status.clone());
                    }
                }
            }
        }

        let mut panel = Self {
            store,
            worktree_status,
            selected_task_id: None,
            selected_detail: None,
            category_filters: default_category_filters(),
            other_status_filters,
            seen_other_statuses,
            search_query: String::new(),
            search_editor: None,
            _search_subscription: None,
            collapsed_goals: HashMap::new(),
            collapsed_tasks,
            seen_tasks,
            hovered_row: None,
            context_menu: None,
            goal_cleanup_result: None,
            zoomed: false,
            editing_item: None,
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

    pub fn refresh_worktree_status_if_needed(&mut self, cx: &mut Context<Self>) {
        let (tasks, goals) = {
            let store = self.store.read(cx);
            let graph = store.graph();
            (graph.tasks.clone(), graph.goals.clone())
        };
        self.worktree_status.update(cx, |status, cx| {
            status
                .refresh_if_needed(tasks, goals, cx)
                .detach_and_log_err(cx);
        });
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

        self._fetch_detail_task =
            Some(
                cx.spawn_in(window, async move |this, cx| match detail_task.await {
                    Ok(detail) => {
                        this.update(cx, |panel, cx| {
                            panel.selected_detail = Some(detail);
                            cx.notify();
                        })
                        .log_err();
                    }
                    Err(err) => {
                        log::error!("failed to fetch task detail for {id}: {err:?}");
                    }
                }),
            );
    }

    pub fn open_item_preview(
        &mut self,
        item: PreviewItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.workspace.clone();
        match item {
            PreviewItem::Task(id) => {
                let provider = self.store.read(cx).provider().clone();
                let detail_task = provider.get_task(&id, cx);
                let artifacts_task = provider.list_artifacts(&id, cx);

                cx.spawn_in(window, async move |this, cx| {
                    let detail_result = detail_task.await;
                    let artifacts_result = artifacts_task.await;

                    let Ok(detail) = detail_result else {
                        log::error!("failed to fetch task detail for {id}");
                        return;
                    };
                    let artifacts = artifacts_result.unwrap_or_default();

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

                    if let Some(workspace) = workspace.upgrade() {
                        let preview_task = cx
                            .update(|window, cx| {
                                open_markdown_preview(title, markdown, workspace, window, cx)
                            })
                            .log_err();
                        if let Some(task) = preview_task {
                            task.await.log_err();
                        }
                    }
                })
                .detach();
            }
            PreviewItem::Goal(goal_id) => {
                let goal_summary = self
                    .store
                    .read(cx)
                    .graph()
                    .goals
                    .iter()
                    .find(|g| g.goal_id == goal_id)
                    .cloned();
                let goal_git = self
                    .worktree_status
                    .read(cx)
                    .snapshot()
                    .and_then(|s| s.goals.get(&goal_id))
                    .cloned();

                if let Some(goal) = goal_summary {
                    let title = format!("Goal {}: {}", goal.goal_id, goal.title);
                    let markdown = render_goal_markdown(&goal, goal_git.as_ref());

                    if let Some(workspace) = workspace.upgrade() {
                        open_markdown_preview(title, markdown, workspace, window, cx)
                            .detach_and_log_err(cx);
                    }
                }
            }
        }
    }

    pub fn deploy_context_menu(
        &mut self,
        target: ContextMenuTarget,
        position: Point<Pixels>,
        anchor: Option<gpui::Anchor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.entity().downgrade();
        let project = self.project.clone();
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        let file_system = self.file_system.clone();

        let graph = self.store.read(cx).graph().clone();
        let snapshot = self.worktree_status.read(cx).snapshot().cloned();

        let context_menu = ContextMenu::build(
            window,
            cx,
            move |menu, _window, _cx| match &target {
                ContextMenuTarget::Goal(goal_id) => {
                    let goal_id = goal_id.clone();
                    let goal_summary = graph.goals.iter().find(|g| g.goal_id == goal_id).cloned();
                    let goal_git = snapshot
                        .as_ref()
                        .and_then(|s| s.goals.get(&goal_id))
                        .cloned();
                    let branch_exists = goal_git.as_ref().map_or(false, |g| g.branch_exists);
                    let branch_name = format!("agent-goal/{goal_id}");

                    let has_active_tasks = graph
                        .tasks
                        .iter()
                        .any(|t| t.goal_id.as_deref() == Some(&goal_id) && !t.status.is_terminal());
                    let can_cleanup = !has_active_tasks && branch_exists;

                    let is_archived = goal_summary
                        .as_ref()
                        .map_or(false, |g| g.status == AgentGoalStatus::Archived);
                    let current_status = goal_summary
                        .as_ref()
                        .map(|g| g.status.clone())
                        .unwrap_or(AgentGoalStatus::Running);

                    let mut menu = menu;

                    {
                        let view = view.clone();
                        let goal_id = goal_id.clone();
                        menu = menu.entry("Open", None, move |window, cx| {
                            if let Some(panel) = view.upgrade() {
                                panel.update(cx, |this, cx| {
                                    this.open_item_preview(
                                        PreviewItem::Goal(goal_id.clone()),
                                        window,
                                        cx,
                                    );
                                });
                            }
                        });
                    }

                    let goal_session_id = goal_summary
                        .as_ref()
                        .and_then(|g| g.session_id.as_deref())
                        .filter(|s| !s.trim().is_empty())
                        .map(ToString::to_string);

                    if let Some(session_id) = goal_session_id {
                        let workspace_for_open_thread = workspace.clone();
                        let goal_title_for_open_thread = goal_summary
                            .as_ref()
                            .map(|g| SharedString::from(g.title.clone()));
                        let session_id_for_open_thread = session_id.clone();
                        menu = menu.entry("Open Thread", None, move |window, cx| {
                            if let Some(workspace) = workspace_for_open_thread.upgrade() {
                                workspace.update(cx, |workspace, cx| {
                                    if let Some(panel) = workspace.panel::<crate::AgentPanel>(cx) {
                                        let session_id =
                                            agent_client_protocol::schema::v1::SessionId::new(
                                                session_id_for_open_thread.clone(),
                                            );
                                        let title = goal_title_for_open_thread.clone();
                                        panel.update(cx, |panel, cx| {
                                            panel.open_thread_standalone(
                                                session_id, None, title, window, cx,
                                            );
                                        });
                                    }
                                    workspace.focus_panel::<crate::AgentPanel>(window, cx);
                                });
                            }
                        });

                        let workspace_for_view_thread = workspace.clone();
                        let goal_title_for_view_thread = goal_summary
                            .as_ref()
                            .map(|g| SharedString::from(g.title.clone()));
                        let session_id_for_view_thread = session_id.clone();
                        menu = menu.entry("View Thread", None, move |window, cx| {
                            if let Some(workspace) = workspace_for_view_thread.upgrade() {
                                workspace.update(cx, |workspace, cx| {
                                    if let Some(panel) = workspace.panel::<crate::AgentPanel>(cx) {
                                        let session_id =
                                            agent_client_protocol::schema::v1::SessionId::new(
                                                session_id_for_view_thread.clone(),
                                            );
                                        let title = goal_title_for_view_thread.clone();
                                        panel.update(cx, |panel, cx| {
                                            panel.view_thread_readonly(
                                                session_id, title, window, cx,
                                            );
                                        });
                                    }
                                    workspace.focus_panel::<crate::AgentPanel>(window, cx);
                                });
                            }
                        });

                        let workspace_for_transcript = workspace.clone();
                        let goal_id_for_preview = goal_id.clone();
                        let session_id_for_transcript = session_id;
                        menu = menu.entry("View Thread as Markdown", None, move |window, cx| {
                            let db_task = agent::ThreadsDatabase::connect(cx);
                            let session_id_acp = agent_client_protocol::schema::v1::SessionId::new(
                                session_id_for_transcript.clone(),
                            );
                            let workspace = workspace_for_transcript.clone();
                            let goal_id_for_preview = goal_id_for_preview.clone();
                            let task = window.spawn(cx, async move |cx| {
                                let database = db_task.await.map_err(|e| anyhow::anyhow!("{e}"))?;
                                let thread = database.load_thread(session_id_acp).await?;
                                let markdown = if let Some(thread) = thread {
                                    thread.to_markdown()
                                } else {
                                    format!(
                                        "No saved transcript found for goal {}.",
                                        goal_id_for_preview
                                    )
                                };
                                if let Some(workspace) = workspace.upgrade() {
                                    let preview_task = cx.update(|window, cx| {
                                        open_markdown_preview(
                                            format!("Goal {} Thread", goal_id_for_preview),
                                            markdown,
                                            workspace,
                                            window,
                                            cx,
                                        )
                                    })?;
                                    preview_task.await?;
                                }
                                Ok(())
                            });
                            Self::spawn_task_with_action_error_toast(
                                task,
                                Some("View Thread as Markdown"),
                                workspace_for_transcript.clone(),
                                cx,
                            );
                        });
                    }

                    if branch_exists {
                        let branch = branch_name.clone();
                        let project = project.clone();
                        let workspace = workspace.clone();
                        menu = menu.entry("Go to Branch", None, move |window, cx| {
                            Self::go_to_branch(
                                branch.clone(),
                                project.clone(),
                                workspace.clone(),
                                window,
                                cx,
                            );
                        });
                    }

                    if branch_exists {
                        let branch = branch_name;
                        let project = project.clone();
                        let workspace = workspace.clone();
                        menu = menu.entry("Open Diff", None, move |window, cx| {
                            Self::open_diff_for_branch(
                                branch.clone(),
                                project.clone(),
                                workspace.clone(),
                                window,
                                cx,
                            );
                        });
                    }

                    {
                        let view = view.clone();
                        let goal_id = goal_id.clone();
                        let title = goal_summary
                            .as_ref()
                            .map(|g| g.title.clone())
                            .unwrap_or_else(|| goal_id.clone());
                        menu = menu.entry("Rename", None, move |window, cx| {
                            if let Some(panel) = view.upgrade() {
                                panel.update(cx, |panel, cx| {
                                    panel.start_renaming(
                                        RenamingTarget::Goal(goal_id.clone()),
                                        title.clone(),
                                        window,
                                        cx,
                                    );
                                });
                            }
                        });
                    }

                    menu = menu.separator();

                    if !is_archived {
                        let store = store.clone();
                        let goal_id = goal_id.clone();
                        let workspace = workspace.clone();
                        menu = menu.entry("Archive", None, move |_window, cx| {
                            let task =
                                store.update(cx, |store, cx| store.archive_goal(&goal_id, cx));
                            Self::spawn_task_with_action_error_toast(
                                task,
                                Some("Archive Goal"),
                                workspace.clone(),
                                cx,
                            );
                        });
                    } else {
                        let store = store.clone();
                        let goal_id = goal_id.clone();
                        let workspace = workspace.clone();
                        menu = menu.entry("Unarchive", None, move |_window, cx| {
                            let task =
                                store.update(cx, |store, cx| store.unarchive_goal(&goal_id, cx));
                            Self::spawn_task_with_action_error_toast(
                                task,
                                Some("Unarchive Goal"),
                                workspace.clone(),
                                cx,
                            );
                        });
                    }

                    if is_archived {
                        let store = store.clone();
                        let goal_id = goal_id.clone();
                        let workspace = workspace.clone();
                        menu = menu.entry("Delete", None, move |_window, cx| {
                            let task =
                                store.update(cx, |store, cx| store.delete_goal(&goal_id, cx));
                            Self::spawn_task_with_action_error_toast(
                                task,
                                Some("Delete Goal"),
                                workspace.clone(),
                                cx,
                            );
                        });
                    }

                    {
                        let store = store.clone();
                        let goal_id = goal_id.clone();
                        let workspace = workspace.clone();
                        menu = menu.submenu_with_colored_icon(
                            "Change Status",
                            IconName::Circle,
                            Color::Muted,
                            move |submenu, _window, _cx| {
                                let statuses = [
                                    (AgentGoalStatus::Running, "Running"),
                                    (AgentGoalStatus::Blocked, "Blocked"),
                                    (AgentGoalStatus::Completed, "Completed"),
                                    (AgentGoalStatus::Failed, "Failed"),
                                    (AgentGoalStatus::Cancelled, "Cancelled"),
                                ];
                                let mut sub = submenu;
                                for (target_status, label) in statuses {
                                    let is_active = current_status == target_status;
                                    let store = store.clone();
                                    let goal_id = goal_id.clone();
                                    let target_status_for_action = target_status.clone();
                                    let workspace = workspace.clone();
                                    sub = sub.toggleable_entry_disabled_when(
                                        label,
                                        is_active,
                                        is_active,
                                        IconPosition::Start,
                                        None,
                                        move |_window, cx| {
                                            let task = store.update(cx, |store, cx| {
                                                store.set_goal_status(
                                                    &goal_id,
                                                    &target_status_for_action,
                                                    cx,
                                                )
                                            });
                                            Self::spawn_task_with_action_error_toast(
                                                task,
                                                Some("Set Goal Status"),
                                                workspace.clone(),
                                                cx,
                                            );
                                        },
                                    );
                                }
                                sub
                            },
                        );
                    }

                    if can_cleanup {
                        let project = project.clone();
                        let view = view.clone();
                        let workspace = workspace.clone();
                        menu = menu.separator().entry(
                            "Cleanup Goal Branches",
                            None,
                            move |_window, cx| {
                                let project = project.clone();
                                let goal_id = goal_id.clone();
                                let view = view.clone();
                                let workspace = workspace.clone();
                                let cleanup_task = agent::task_worktree::cleanup_graduated_goal(
                                    project, &goal_id, cx,
                                );
                                cx.spawn(async move |cx| match cleanup_task.await {
                                    Ok(result) => {
                                        log::info!(
                                            "Cleaned up goal {}: {} branches deleted, {} failed, {} skipped",
                                            result.goal_id,
                                            result.deleted_branches.len(),
                                            result.failed_branches.len(),
                                            result.skipped_branches.len()
                                        );
                                        if let Some(panel) = view.upgrade() {
                                            panel.update(cx, |this, cx| {
                                                this.goal_cleanup_result = Some(result);
                                                this.refresh_worktree_status(cx);
                                                cx.notify();
                                            });
                                        }
                                    }
                                    Err(e) => {
                                        log::warn!("Failed to clean up goal {goal_id}: {e}");
                                        let humanized = Self::humanize_error(&e);
                                        cx.update(|cx| {
                                            Self::show_error_toast(
                                                workspace,
                                                format!("Task Panel: Clean up Goal: {humanized}"),
                                                cx,
                                            );
                                        });
                                    }
                                })
                                .detach();
                            },
                        );
                    }

                    menu
                }
                ContextMenuTarget::Task(task_id) => {
                    let task_id = task_id.clone();
                    let task = graph.tasks.iter().find(|t| t.id == task_id).cloned();
                    let task_git = snapshot
                        .as_ref()
                        .and_then(|s| s.tasks.get(&task_id))
                        .cloned();

                    let exists_on_disk = task_git.as_ref().map_or(false, |g| g.exists_on_disk);
                    let branch_exists = task_git.as_ref().map_or(false, |g| g.branch_exists);
                    let worktree_path = task_git.as_ref().map(|g| g.worktree_path.clone());
                    let branch_name = task_git
                        .as_ref()
                        .and_then(|g| g.branch.clone())
                        .unwrap_or_else(|| format!("agent-task/{task_id}"));

                    let goal_id = task.as_ref().and_then(|t| t.goal_id.clone());
                    let goal_git = goal_id
                        .as_ref()
                        .and_then(|gid| snapshot.as_ref().and_then(|s| s.goals.get(gid)));
                    let goal_branch_exists = goal_git.map_or(false, |g| g.branch_exists);
                    let can_diff = goal_id.is_some() && goal_branch_exists && exists_on_disk;

                    let is_archived = task
                        .as_ref()
                        .map_or(false, |t| t.status == AgentTaskStatus::Archived);
                    let current_status = task
                        .as_ref()
                        .map(|t| t.status.clone())
                        .unwrap_or(AgentTaskStatus::Ready);

                    let mut menu = menu;

                    {
                        let view = view.clone();
                        let task_id = task_id.clone();
                        menu = menu.entry("Open", None, move |window, cx| {
                            if let Some(panel) = view.upgrade() {
                                panel.update(cx, |this, cx| {
                                    this.open_item_preview(
                                        PreviewItem::Task(task_id.clone()),
                                        window,
                                        cx,
                                    );
                                });
                            }
                        });
                    }

                    {
                        let view = view.clone();
                        let task_id = task_id.clone();
                        let title = task
                            .as_ref()
                            .map(|t| t.title.clone())
                            .unwrap_or_else(|| task_id.to_string());
                        menu = menu.entry("Rename", None, move |window, cx| {
                            if let Some(panel) = view.upgrade() {
                                panel.update(cx, |panel, cx| {
                                    panel.start_renaming(
                                        RenamingTarget::Task(task_id.clone()),
                                        title.clone(),
                                        window,
                                        cx,
                                    );
                                });
                            }
                        });
                    }

                    if let Some(session_id) = agent::task_worktree::task_session_id_for_id(&task_id)
                    {
                        let workspace_for_transcript = workspace.clone();
                        let task_id_for_preview = task_id.clone();
                        let session_id_for_transcript = session_id.clone();
                        menu = menu.entry("View Thread as Markdown", None, move |window, cx| {
                            let db_task = agent::ThreadsDatabase::connect(cx);
                            let session_id_acp = agent_client_protocol::schema::v1::SessionId::new(
                                session_id_for_transcript.clone(),
                            );
                            let workspace = workspace_for_transcript.clone();
                            let task_id_for_preview = task_id_for_preview.clone();
                            let task = window.spawn(cx, async move |cx| {
                                let database = db_task.await.map_err(|e| anyhow::anyhow!("{e}"))?;
                                let thread = database.load_thread(session_id_acp).await?;
                                let markdown = if let Some(thread) = thread {
                                    thread.to_markdown()
                                } else {
                                    format!(
                                        "No saved transcript found for task {}.",
                                        task_id_for_preview
                                    )
                                };
                                if let Some(workspace) = workspace.upgrade() {
                                    let preview_task = cx.update(|window, cx| {
                                        open_markdown_preview(
                                            format!("Task {} Transcript", task_id_for_preview),
                                            markdown,
                                            workspace,
                                            window,
                                            cx,
                                        )
                                    })?;
                                    preview_task.await?;
                                }
                                Ok(())
                            });
                            Self::spawn_task_with_action_error_toast(
                                task,
                                Some("View Thread as Markdown"),
                                workspace_for_transcript.clone(),
                                cx,
                            );
                        });

                        let workspace_for_panel = workspace.clone();
                        let task_title = task.as_ref().map(|t| SharedString::from(t.title.clone()));
                        let session_id_for_view = session_id.clone();
                        menu = menu.entry("View Thread", None, move |window, cx| {
                            if let Some(workspace) = workspace_for_panel.upgrade() {
                                workspace.update(cx, |workspace, cx| {
                                    if let Some(panel) = workspace.panel::<crate::AgentPanel>(cx) {
                                        let session_id =
                                            agent_client_protocol::schema::v1::SessionId::new(
                                                session_id_for_view.clone(),
                                            );
                                        let title = task_title.clone();
                                        panel.update(cx, |panel, cx| {
                                            panel.view_thread_readonly(
                                                session_id, title, window, cx,
                                            );
                                        });
                                    }
                                    workspace.focus_panel::<crate::AgentPanel>(window, cx);
                                });
                            }
                        });

                        let workspace_for_open_thread = workspace.clone();
                        let task_title_for_open_thread =
                            task.as_ref().map(|t| SharedString::from(t.title.clone()));
                        let session_id_for_open_thread = session_id;
                        let project_for_open_thread = project.clone();
                        let task_id_for_open_thread = task_id.clone();
                        let task_for_open_thread = task.clone();
                        let branch_name_for_open_thread = branch_name.clone();
                        let branch_exists_for_open_thread = branch_exists;
                        let exists_on_disk_for_open_thread = exists_on_disk;
                        let worktree_path_for_open_thread = worktree_path.clone();
                        let goal_id_for_open_thread = goal_id.clone();
                        let view_for_open_thread = view.clone();

                        menu = menu.entry("Open Thread", None, move |window, cx| {
                            let session_id = agent_client_protocol::schema::v1::SessionId::new(
                                session_id_for_open_thread.clone(),
                            );
                            let title = task_title_for_open_thread.clone();
                            let workspace = workspace_for_open_thread.clone();

                            if exists_on_disk_for_open_thread {
                                if let Some(workspace) = workspace.upgrade() {
                                    workspace.update(cx, |workspace, cx| {
                                        if let Some(panel) =
                                            workspace.panel::<crate::AgentPanel>(cx)
                                        {
                                            panel.update(cx, |panel, cx| {
                                                panel.open_thread_standalone(
                                                    session_id,
                                                    worktree_path_for_open_thread
                                                        .as_ref()
                                                        .map(|p| PathList::new(&[p.as_path()])),
                                                    title,
                                                    window,
                                                    cx,
                                                );
                                            });
                                        }
                                        workspace.focus_panel::<crate::AgentPanel>(window, cx);
                                    });
                                }
                            } else if !branch_exists_for_open_thread {
                                let task_id = task_id_for_open_thread.clone();
                                let branch_name = branch_name_for_open_thread.clone();
                                Self::show_error_toast(
                                    workspace,
                                    format!(
                                        "Task Panel: Open Thread: Cannot open thread for task {task_id}: branch '{branch_name}' was not found in git repository; worktree cannot be restored."
                                    ),
                                    cx,
                                );
                            } else {
                                let task_id = task_id_for_open_thread.clone();
                                let task_summary = task_for_open_thread
                                    .clone()
                                    .unwrap_or_else(|| agent::AgentTaskSummary {
                                        id: task_id_for_open_thread.clone(),
                                        title: task_id_for_open_thread.to_string(),
                                        ..Default::default()
                                    });
                                let ensure_task =
                                    agent::task_worktree::ensure_task_worktree_with_options(
                                        project_for_open_thread.clone(),
                                        &task_summary,
                                        goal_id_for_open_thread.clone(),
                                        None,
                                        None,
                                        true,
                                        cx,
                                    );
                                let view = view_for_open_thread.clone();
                                window
                                    .spawn(cx, async move |cx| -> anyhow::Result<()> {
                                        let path = match ensure_task.await {
                                            Ok(path) => path,
                                            Err(err) => {
                                                let humanized = Self::humanize_error(&err);
                                                cx.update(|_window, cx| {
                                                    Self::show_error_toast(
                                                        workspace,
                                                        format!(
                                                            "Task Panel: Open Thread: Failed to restore worktree for task {task_id}: {humanized}"
                                                        ),
                                                        cx,
                                                    );
                                                })
                                                .log_err();
                                                return Ok(());
                                            }
                                        };
                                        cx.update(|window, cx| {
                                            if let Some(panel) = view.upgrade() {
                                                panel.update(cx, |panel, cx| {
                                                    panel.refresh_worktree_status(cx);
                                                });
                                            }
                                            if let Some(workspace) = workspace.upgrade() {
                                                workspace.update(cx, |workspace, cx| {
                                                    if let Some(panel) =
                                                        workspace.panel::<crate::AgentPanel>(cx)
                                                    {
                                                        panel.update(cx, |panel, cx| {
                                                            panel.open_thread_standalone(
                                                                session_id,
                                                                Some(PathList::new(&[path.as_path()])),
                                                                title,
                                                                window,
                                                                cx,
                                                            );
                                                        });
                                                    }
                                                    workspace.focus_panel::<crate::AgentPanel>(
                                                        window, cx,
                                                    );
                                                });
                                            }
                                        })?;
                                        Ok(())
                                    })
                                    .detach();
                            }
                        });
                    }

                    if exists_on_disk {
                        if let Some(path) = worktree_path.clone() {
                            let task_id = task_id.clone();
                            menu = menu.entry("Go to Worktree", None, move |window, cx| {
                                let display_name = format!("agent-task-{}", task_id);
                                window.dispatch_action(
                                    Box::new(SwitchWorktree {
                                        path: path.clone(),
                                        display_name,
                                    }),
                                    cx,
                                );
                            });
                        }
                    }

                    if branch_exists {
                        let branch = branch_name.clone();
                        let project = project.clone();
                        let workspace = workspace.clone();
                        menu = menu.entry("Go to Branch", None, move |window, cx| {
                            Self::go_to_branch(
                                branch.clone(),
                                project.clone(),
                                workspace.clone(),
                                window,
                                cx,
                            );
                        });
                    }

                    if !exists_on_disk && branch_exists {
                        if let Some(task) = task {
                            let project = project.clone();
                            let branch = branch_name;
                            let view = view.clone();
                            let workspace = workspace.clone();
                            let goal_id = goal_id.clone();
                            menu = menu.entry(
                                "Restore Worktree from Branch",
                                None,
                                move |window, cx| {
                                    let task_id = task.id.clone();
                                    let branch_for_switch = branch.clone();
                                    let view = view.clone();
                                    let workspace = workspace.clone();
                                    let ensure_task =
                                        agent::task_worktree::ensure_task_worktree_with_options(
                                            project.clone(),
                                            &task,
                                            goal_id.clone(),
                                            None,
                                            None,
                                            true,
                                            cx,
                                        );
                                    window.spawn(cx, async move |cx| {
                                        match ensure_task.await {
                                            Ok(path) => {
                                                cx.update(|window, cx| {
                                                    window.dispatch_action(
                                                        Box::new(SwitchWorktree {
                                                            path,
                                                            display_name: format!(
                                                                "agent-task-{}",
                                                                task_id
                                                            ),
                                                        }),
                                                        cx,
                                                    );
                                                    if let Some(panel) = view.upgrade() {
                                                        panel.update(cx, |panel, cx| {
                                                            panel.refresh_worktree_status(cx);
                                                        });
                                                    }
                                                })
                                                .log_err();
                                            }
                                            Err(e) => {
                                                log::warn!(
                                                    "failed to restore worktree from branch {branch_for_switch}: {e:?}"
                                                );
                                                let humanized = Self::humanize_error(&e);
                                                cx.update(|_window, cx| {
                                                    Self::show_error_toast(
                                                        workspace,
                                                        format!(
                                                            "Task Panel: Restore Worktree: {humanized}"
                                                        ),
                                                        cx,
                                                    );
                                                })
                                                .log_err();
                                            }
                                        }
                                    })
                                    .detach();
                                },
                            );
                        }
                    }

                    if exists_on_disk {
                        let project = project.clone();
                        let task_id = task_id.clone();
                        let view = view.clone();
                        let workspace = workspace.clone();
                        menu = menu.entry("Delete Worktree", None, move |_window, cx| {
                            let project = project.clone();
                            let task_id = task_id.clone();
                            let view = view.clone();
                            let workspace_for_task = workspace.clone();
                            let remove_task =
                                agent::task_worktree::remove_task_worktree(project, &task_id, cx);
                            let task = cx.spawn(async move |cx| {
                                let remove_result = remove_task.await;
                                if let Some(panel) = view.upgrade() {
                                    panel.update(cx, |this, cx| {
                                        this.refresh_worktree_status(cx);
                                    });
                                }
                                remove_result.with_context(|| {
                                    format!("failed to remove worktree for {task_id}")
                                })?;
                                if let Some(workspace) = workspace_for_task.upgrade() {
                                    workspace.update(cx, |workspace, cx| {
                                        workspace.show_toast(
                                            workspace::Toast::new(
                                                workspace::notifications::NotificationId::unique::<
                                                    WorktreeDeletedToast,
                                                >(),
                                                format!("Worktree for {task_id} deleted"),
                                            )
                                            .autohide(),
                                            cx,
                                        );
                                    });
                                }
                                Ok(())
                            });
                            Self::spawn_task_with_action_error_toast(
                                task,
                                Some("Delete Worktree"),
                                workspace.clone(),
                                cx,
                            );
                        });
                    }

                    if can_diff {
                        if let (Some(goal_id), Some(worktree_path)) = (goal_id, worktree_path) {
                            let project = project.clone();
                            let workspace = workspace.clone();
                            let file_system = file_system.clone();
                            menu = menu.entry("Open Diff", None, move |window, cx| {
                                Self::deploy_task_diff(
                                    goal_id.clone(),
                                    worktree_path.clone(),
                                    project.clone(),
                                    workspace.clone(),
                                    file_system.clone(),
                                    window,
                                    cx,
                                );
                            });
                        }
                    }

                    menu = menu.separator();

                    if current_status != AgentTaskStatus::Completed {
                        let store = store.clone();
                        let task_id = task_id.clone();
                        let workspace = workspace.clone();
                        menu = menu.entry("Complete", None, move |_window, cx| {
                            let task =
                                store.update(cx, |store, cx| store.complete_task(&task_id, cx));
                            Self::spawn_task_with_action_error_toast(
                                task,
                                Some("Complete Task"),
                                workspace.clone(),
                                cx,
                            );
                        });
                    }

                    if !is_archived {
                        let store = store.clone();
                        let task_id = task_id.clone();
                        let workspace = workspace.clone();
                        menu = menu.entry("Archive", None, move |_window, cx| {
                            let task =
                                store.update(cx, |store, cx| store.archive_task(&task_id, cx));
                            Self::spawn_task_with_action_error_toast(
                                task,
                                Some("Archive Task"),
                                workspace.clone(),
                                cx,
                            );
                        });
                    } else {
                        let store = store.clone();
                        let task_id = task_id.clone();
                        let workspace = workspace.clone();
                        menu = menu.entry("Unarchive", None, move |_window, cx| {
                            let task =
                                store.update(cx, |store, cx| store.unarchive_task(&task_id, cx));
                            Self::spawn_task_with_action_error_toast(
                                task,
                                Some("Unarchive Task"),
                                workspace.clone(),
                                cx,
                            );
                        });
                    }

                    if is_archived {
                        let store = store.clone();
                        let task_id = task_id.clone();
                        let workspace = workspace.clone();
                        menu = menu.entry("Delete", None, move |_window, cx| {
                            let task =
                                store.update(cx, |store, cx| store.delete_task(&task_id, cx));
                            Self::spawn_task_with_action_error_toast(
                                task,
                                Some("Delete Task"),
                                workspace.clone(),
                                cx,
                            );
                        });
                    }

                    {
                        let store = store.clone();
                        let workspace = workspace.clone();
                        menu = menu.submenu_with_colored_icon(
                            "Change Status",
                            IconName::Circle,
                            Color::Muted,
                            move |submenu, _window, _cx| {
                                let statuses = [
                                    (AgentTaskStatus::Ready, "Ready"),
                                    (AgentTaskStatus::Running, "Running"),
                                    (AgentTaskStatus::Completed, "Completed"),
                                    (AgentTaskStatus::Failed, "Failed"),
                                    (AgentTaskStatus::Cancelled, "Cancelled"),
                                ];
                                let mut sub = submenu;
                                for (target_status, label) in statuses {
                                    let is_active = current_status == target_status;
                                    let store = store.clone();
                                    let task_id = task_id.clone();
                                    let target_status_for_action = target_status.clone();
                                    let workspace = workspace.clone();
                                    sub = sub.toggleable_entry_disabled_when(
                                        label,
                                        is_active,
                                        is_active,
                                        IconPosition::Start,
                                        None,
                                        move |_window, cx| {
                                            let task = store.update(cx, |store, cx| {
                                                store.set_task_status(
                                                    &task_id,
                                                    &target_status_for_action,
                                                    cx,
                                                )
                                            });
                                            Self::spawn_task_with_action_error_toast(
                                                task,
                                                Some("Set Task Status"),
                                                workspace.clone(),
                                                cx,
                                            );
                                        },
                                    );
                                }
                                sub
                            },
                        );
                    }

                    menu
                }
            },
        );

        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&context_menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some(DeployedContextMenu {
            menu: context_menu,
            position,
            anchor,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn go_to_branch(
        branch: String,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let existing_repo = project
            .read(cx)
            .repositories(cx)
            .values()
            .find(|repo| {
                repo.read(cx).snapshot().branch.as_ref().is_some_and(|b| {
                    b.ref_name == branch.as_str() || b.ref_name == format!("refs/heads/{branch}")
                })
            })
            .cloned();

        if let Some(repo) = existing_repo {
            let work_dir = repo.read(cx).snapshot().work_directory_abs_path;
            window.dispatch_action(
                Box::new(SwitchWorktree {
                    path: work_dir.as_ref().to_path_buf(),
                    display_name: branch,
                }),
                cx,
            );
        } else if let Some(repo) = project
            .read(cx)
            .active_repository(cx)
            .or_else(|| project.read(cx).repositories(cx).values().next().cloned())
        {
            let work_dir = repo.read(cx).snapshot().work_directory_abs_path;
            let work_dir_buf = work_dir.as_ref().to_path_buf();
            let branch_clone = branch.clone();
            let checkout_rx = repo.update(cx, |repo, _| {
                repo.checkout_branch_in_worktree(branch_clone, work_dir_buf.clone(), false)
            });
            window
                .spawn(cx, async move |cx| match checkout_rx.await {
                    Ok(Ok(())) => {
                        cx.update(|window, cx| {
                            window.dispatch_action(
                                Box::new(SwitchWorktree {
                                    path: work_dir_buf,
                                    display_name: branch,
                                }),
                                cx,
                            );
                        })
                        .log_err();
                    }
                    Ok(Err(err)) => {
                        log::warn!("failed to checkout branch {branch}: {err:?}");
                        let humanized = Self::humanize_error_str(&err.to_string());
                        cx.update(|_window, cx| {
                            Self::show_error_toast(
                                workspace,
                                format!(
                                    "Task Panel: Go to Branch: Failed to checkout '{branch}': {humanized}"
                                ),
                                cx,
                            );
                        })
                        .log_err();
                    }
                    Err(err) => {
                        log::warn!("checkout receiver cancelled for branch {branch}: {err:?}");
                        let humanized = Self::humanize_error_str(&err.to_string());
                        cx.update(|_window, cx| {
                            Self::show_error_toast(
                                workspace,
                                format!(
                                    "Task Panel: Go to Branch: Checkout cancelled for '{branch}': {humanized}"
                                ),
                                cx,
                            );
                        })
                        .log_err();
                    }
                })
                .detach();
        } else {
            log::warn!("could not find repository to checkout branch {branch}");
            Self::show_error_toast(
                workspace,
                format!(
                    "Task Panel: Go to Branch: Could not find repository to checkout branch '{branch}'"
                ),
                cx,
            );
        }
    }

    fn deploy_task_diff(
        _goal_id: String,
        worktree_path: std::path::PathBuf,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        file_system: Arc<dyn Fs>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(_task_repo) = find_repository_for_worktree_path(&project, &worktree_path, cx) {
            let worktree_id = project.read(cx).worktrees(cx).find_map(|worktree| {
                if paths_match(worktree.read(cx).abs_path().as_ref(), &worktree_path) {
                    Some(worktree.read(cx).id())
                } else {
                    None
                }
            });
            if let Some(worktree_id) = worktree_id {
                let git_store = project.read(cx).git_store().clone();
                git_store.update(cx, |git_store, cx| {
                    git_store.set_active_repo_for_worktree(worktree_id, cx);
                });
            }
            if let Some(workspace) = workspace.upgrade() {
                workspace.update(cx, |_workspace, cx| {
                    match cx.build_action("git::DiffBranch", None) {
                        Ok(action) => window.dispatch_action(action, cx),
                        Err(error) => {
                            log::error!("failed to resolve git::DiffBranch action: {error:?}");
                        }
                    }
                });
            }
            return;
        }

        window
            .spawn(cx, async move |cx| {
                if !file_system.is_dir(&worktree_path).await {
                    log::warn!(
                        "failed to find worktree at {}: not a directory",
                        worktree_path.display()
                    );
                    cx.update(|_window, cx| {
                        Self::show_error_toast(
                            workspace,
                            format!(
                                "Task Panel: Open Diff: Worktree path '{}' is not a directory",
                                worktree_path.display()
                            ),
                            cx,
                        );
                    })
                    .log_err();
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
                        let humanized = Self::humanize_error(&err);
                        cx.update(|_window, cx| {
                            Self::show_error_toast(
                                workspace,
                                format!(
                                    "Task Panel: Open Diff: Failed to find worktree at '{}': {humanized}",
                                    worktree_path.display()
                                ),
                                cx,
                            );
                        })
                        .log_err();
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
                    .log_err()
                    .flatten();
                if let Some(scan) = scan_complete {
                    scan.await;
                }
                let task_repo = cx
                    .update(|_window, cx| {
                        find_repository_for_worktree_path(&project, &worktree_path, cx)
                    })
                    .log_err()
                    .flatten();
                let Some(_task_repo) = task_repo else {
                    log::warn!(
                        "could not resolve repository for worktree at {}",
                        worktree_path.display()
                    );
                    cx.update(|_window, cx| {
                        Self::show_error_toast(
                            workspace,
                            format!(
                                "Task Panel: Open Diff: Could not resolve repository for worktree at '{}'",
                                worktree_path.display()
                            ),
                            cx,
                        );
                    })
                    .log_err();
                    return;
                };
                let worktree_id = cx.update(|_window, cx| worktree.read(cx).id()).log_err();
                if let Some(worktree_id) = worktree_id {
                    cx.update(|_window, cx| {
                        let git_store = project.read(cx).git_store().clone();
                        git_store.update(cx, |git_store, cx| {
                            git_store.set_active_repo_for_worktree(worktree_id, cx);
                        });
                    })
                    .log_err();
                }
                if let Some(workspace) = workspace.upgrade() {
                    if let Err(err) = workspace.update_in(cx, |_workspace, window, cx| {
                        match cx.build_action("git::DiffBranch", None) {
                            Ok(action) => window.dispatch_action(action, cx),
                            Err(error) => {
                                log::error!("failed to resolve git::DiffBranch action: {error:?}");
                            }
                        }
                    }) {
                        log::warn!("failed to deploy branch diff in workspace: {err:?}");
                        let humanized = Self::humanize_error(&err);
                        cx.update(|_window, cx| {
                            Self::show_error_toast(
                                workspace.downgrade(),
                                format!(
                                    "Task Panel: Open Diff: Failed to deploy branch diff: {humanized}"
                                ),
                                cx,
                            );
                        })
                        .log_err();
                    }
                }
            })
            .detach();
    }

    fn open_diff_for_branch(
        branch: String,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let repo = project
            .read(cx)
            .active_repository(cx)
            .or_else(|| project.read(cx).repositories(cx).values().next().cloned());
        if let (Some(_repo), Some(workspace)) = (repo, workspace.upgrade()) {
            workspace.update(cx, |_workspace, cx| {
                match cx.build_action("git::DiffBranch", None) {
                    Ok(action) => window.dispatch_action(action, cx),
                    Err(error) => {
                        log::error!("failed to resolve git::DiffBranch action: {error:?}");
                    }
                }
            });
        } else {
            Self::show_error_toast(
                workspace,
                format!("Task Panel: Open Diff: Could not find repository for branch '{branch}'"),
                cx,
            );
        }
    }

    fn render_task_tree(&self, cx: &mut Context<Self>) -> Div {
        let store = self.store.read(cx);
        let graph = store.graph();
        let groups = build_goal_groups(
            &graph.tasks,
            &graph.goals,
            &self.category_filters,
            &self.other_status_filters,
            &self.search_query,
            &self.collapsed_tasks,
        );

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
            group_elements.push(self.render_goal_group(group, cx));
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
                    let workspace = this.workspace.clone();
                    let remove_task =
                        agent::task_worktree::remove_orphan_worktree(project, path, cx);
                    cx.spawn(async move |this, cx| {
                        if let Err(e) = remove_task.await {
                            log::warn!(
                                "Failed to remove orphan worktree for {task_id_hint_for_async}: {e}"
                            );
                            let humanized = Self::humanize_error(&e);
                            cx.update(|cx| {
                                Self::show_error_toast(
                                    workspace,
                                    format!("Task Panel: Delete Orphan Worktree: {humanized}"),
                                    cx,
                                );
                            });
                        }
                        this.update(cx, |this, cx| {
                            this.refresh_worktree_status(cx);
                        })
                        .log_err();
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

    fn render_goal_group(&self, group: GoalGroup, cx: &mut Context<Self>) -> Div {
        let hover_color = cx
            .theme()
            .colors()
            .element_active
            .blend(cx.theme().colors().element_background.opacity(0.2));

        if let Some(goal_id) = group.goal_id {
            let is_collapsed = self.collapsed_goals.get(&goal_id).copied().unwrap_or(true);

            let title = group
                .goal_summary
                .as_ref()
                .map(|g| g.title.clone())
                .unwrap_or_else(|| goal_id.clone());

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

            let goal_status = group
                .goal_summary
                .as_ref()
                .map(|s| s.status.clone())
                .unwrap_or(AgentGoalStatus::Running);

            let is_hovered = self.hovered_row == Some(HoveredRow::Goal(goal_id.clone()));

            let cleanup_summary = self
                .goal_cleanup_result
                .as_ref()
                .filter(|res| res.goal_id == goal_id)
                .map(|res| {
                    let text = match (res.failed_branches.is_empty(), res.skipped_branches.is_empty()) {
                        (true, true) => format!("Cleaned ({} deleted)", res.deleted_branches.len()),
                        (true, false) => format!(
                            "Cleaned ({} deleted, {} skipped)",
                            res.deleted_branches.len(),
                            res.skipped_branches.len()
                        ),
                        (false, true) => format!(
                            "Cleaned ({} deleted, {} failed)",
                            res.deleted_branches.len(),
                            res.failed_branches.len()
                        ),
                        (false, false) => format!(
                            "Cleaned ({} deleted, {} failed, {} skipped)",
                            res.deleted_branches.len(),
                            res.failed_branches.len(),
                            res.skipped_branches.len()
                        ),
                    };
                    let color = if res.failed_branches.is_empty() {
                        Color::Success
                    } else {
                        Color::Warning
                    };
                    (text, color)
                });

            let has_children = tasks_total > 0 || !group.rows.is_empty();

            let chevron_or_spacer = if has_children {
                div()
                    .id(SharedString::from(format!("goal-chevron-{}", goal_id)))
                    .debug_selector({
                        let goal_id = goal_id.clone();
                        move || format!("goal-chevron-{}", goal_id)
                    })
                    .w_4()
                    .h_4()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .on_click(cx.listener({
                        let goal_id = goal_id.clone();
                        move |this, _event, _window, cx| {
                            cx.stop_propagation();
                            let entry = this.collapsed_goals.entry(goal_id.clone()).or_insert(true);
                            *entry = !*entry;
                            cx.notify();
                        }
                    }))
                    .child(
                        Icon::new(if is_collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    )
                    .into_any_element()
            } else {
                div().w_4().h_4().flex_shrink_0().into_any_element()
            };

            let header = v_flex()
                .id(SharedString::from(format!("goal-group-header-{}", goal_id)))
                .debug_selector({
                    let goal_id = goal_id.clone();
                    move || format!("goal-group-header-{}", goal_id)
                })
                .w_full()
                .px_1p5()
                .cursor_pointer()
                .hover(move |s| s.bg(hover_color))
                .on_hover(cx.listener({
                    let goal_id = goal_id.clone();
                    move |this, is_hovered, _window, cx| {
                        let prev = this.hovered_row.clone();
                        if *is_hovered {
                            this.hovered_row = Some(HoveredRow::Goal(goal_id.clone()));
                        } else if this.hovered_row == Some(HoveredRow::Goal(goal_id.clone())) {
                            this.hovered_row = None;
                        }
                        if this.hovered_row != prev {
                            cx.notify();
                        }
                    }
                }))
                .on_click(cx.listener({
                    let goal_id = goal_id.clone();
                    move |this, _event, window, cx| {
                        if this.editing_item.as_ref().is_some_and(|item| {
                            item.target == RenamingTarget::Goal(goal_id.clone())
                        }) {
                            return;
                        }
                        this.open_item_preview(PreviewItem::Goal(goal_id.clone()), window, cx);
                    }
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener({
                        let goal_id = goal_id.clone();
                        move |this, event: &MouseDownEvent, window, cx| {
                            this.deploy_context_menu(
                                ContextMenuTarget::Goal(goal_id.clone()),
                                event.position,
                                None,
                                window,
                                cx,
                            );
                        }
                    }),
                )
                .child(
                    h_flex()
                        .h_5()
                        .w_full()
                        .items_center()
                        .justify_between()
                        .child(
                            h_flex()
                                .gap_1p5()
                                .items_center()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .child(chevron_or_spacer)
                                .child(render_goal_status_icon(&goal_status))
                                .map(|this| {
                                    if let Some(renaming) =
                                        self.editing_item.as_ref().filter(|item| {
                                            item.target == RenamingTarget::Goal(goal_id.clone())
                                        })
                                    {
                                        this.child(self.render_rename_editor(&renaming.editor, cx))
                                    } else {
                                        this.child(
                                            Label::new(title).size(LabelSize::Default).truncate(),
                                        )
                                    }
                                }),
                        )
                        .child(h_flex().items_center().when(is_hovered, |this| {
                            this.child(
                                IconButton::new(
                                    SharedString::from(format!("goal-menu-{}", goal_id)),
                                    IconName::Ellipsis,
                                )
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("More Actions"))
                                .on_click(cx.listener({
                                    let goal_id = goal_id.clone();
                                    move |this, event: &gpui::ClickEvent, window, cx| {
                                        cx.stop_propagation();
                                        this.deploy_context_menu(
                                            ContextMenuTarget::Goal(goal_id.clone()),
                                            event.position(),
                                            Some(gpui::Anchor::TopRight),
                                            window,
                                            cx,
                                        );
                                    }
                                })),
                            )
                        })),
                )
                .child(
                    h_flex()
                        .id(SharedString::from(format!("goal-meta-{}", goal_id)))
                        .debug_selector({
                            let goal_id = goal_id.clone();
                            move || format!("goal-meta-{}", goal_id)
                        })
                        .h_5()
                        .w_full()
                        .items_center()
                        .justify_between()
                        .child(
                            h_flex()
                                .items_center()
                                .gap_1p5()
                                .min_w_0()
                                .overflow_hidden()
                                .child(if has_children && !is_collapsed {
                                    div()
                                        .w_4()
                                        .h_4()
                                        .flex_shrink_0()
                                        .flex()
                                        .items_center()
                                        .child(
                                            Label::new("│")
                                                .size(LabelSize::Small)
                                                .color(Color::Muted)
                                                .buffer_font(cx),
                                        )
                                        .into_any_element()
                                } else {
                                    div().w_4().h_4().flex_shrink_0().into_any_element()
                                })
                                .child({
                                    let goal_label = if goal_id.starts_with("GOAL-") {
                                        goal_id.clone()
                                    } else {
                                        format!("GOAL-{goal_id}")
                                    };
                                    Label::new(goal_label)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                })
                                .child(dot_separator())
                                .child(
                                    Label::new(format!("{tasks_done} / {tasks_total}"))
                                        .size(LabelSize::Small)
                                        .color(if tasks_done == tasks_total && tasks_total > 0 {
                                            Color::Success
                                        } else {
                                            Color::Muted
                                        }),
                                )
                                .child(dot_separator())
                                .child({
                                    let priority = group
                                        .goal_summary
                                        .as_ref()
                                        .map(|g| g.priority)
                                        .unwrap_or(0);
                                    Label::new(format!("P{priority}"))
                                        .size(LabelSize::Small)
                                        .color(priority_color(priority))
                                })
                                .when_some(
                                    group.goal_summary.as_ref().and_then(|g| g.metrics.as_ref()),
                                    |mut this, metrics| {
                                        for element in render_metrics_elements(
                                            (ElementId::from("diff-stat"), goal_id.clone()).into(),
                                            Some(metrics),
                                            false,
                                        ) {
                                            this = this.child(dot_separator()).child(element);
                                        }
                                        this
                                    },
                                )
                                .when_some(cleanup_summary, |this, (text, color)| {
                                    this.child(dot_separator())
                                        .child(Label::new(text).size(LabelSize::Small).color(color))
                                }),
                        )
                        .when_some(
                            group
                                .goal_summary
                                .as_ref()
                                .and_then(|g| g.metrics.as_ref())
                                .and_then(|m| m.duration_ms),
                            |this, ms| {
                                this.child(
                                    Label::new(format_duration_ms(ms))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .flex_shrink_0(),
                                )
                            },
                        ),
                );

            let mut group_div = v_flex().w_full().child(header);

            if !is_collapsed {
                let mut row_elements = Vec::with_capacity(group.rows.len());
                for row in group.rows {
                    row_elements.push(self.render_task_row(row, cx));
                }
                group_div = group_div.children(row_elements);
            }

            group_div
        } else {
            let is_collapsed = self
                .collapsed_goals
                .get(NO_GOAL_KEY)
                .copied()
                .unwrap_or(true);

            let has_children = !group.rows.is_empty();
            let chevron_or_spacer = if has_children {
                div()
                    .id("goal-chevron-no-goal")
                    .debug_selector(|| "goal-chevron-no-goal".to_string())
                    .w_4()
                    .h_4()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        cx.stop_propagation();
                        let entry = this
                            .collapsed_goals
                            .entry(NO_GOAL_KEY.to_string())
                            .or_insert(true);
                        *entry = !*entry;
                        cx.notify();
                    }))
                    .child(
                        Icon::new(if is_collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    )
                    .into_any_element()
            } else {
                div().w_4().h_4().flex_shrink_0().into_any_element()
            };

            let header = v_flex()
                .id("goal-group-header-no-goal")
                .debug_selector(|| "goal-group-header-no-goal".to_string())
                .w_full()
                .px_1p5()
                .cursor_pointer()
                .hover(move |s| s.bg(hover_color))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    let entry = this
                        .collapsed_goals
                        .entry(NO_GOAL_KEY.to_string())
                        .or_insert(true);
                    *entry = !*entry;
                    cx.notify();
                }))
                .child(
                    h_flex()
                        .h_5()
                        .gap_1p5()
                        .items_center()
                        .child(chevron_or_spacer)
                        .child(Label::new("No goal").size(LabelSize::Default))
                        .child(
                            Label::new(format!("({} tasks)", group.rows.len()))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                );

            let mut group_div = v_flex().w_full().child(header);

            if !is_collapsed {
                let mut row_elements = Vec::with_capacity(group.rows.len());
                for row in group.rows {
                    row_elements.push(self.render_task_row(row, cx));
                }
                group_div = group_div.children(row_elements);
            }

            group_div
        }
    }

    fn render_task_row(&self, row: TaskRow, cx: &mut Context<Self>) -> gpui::AnyElement {
        let task = row.task;
        let is_selected = self
            .selected_task_id
            .as_ref()
            .map_or(false, |selected_id| selected_id == &task.id);

        let is_hovered = self.hovered_row == Some(HoveredRow::Task(task.id.clone()));

        let policy_denied: Option<agent::AgentTaskEvent> = None;

        let exists_on_disk = self
            .worktree_status
            .read(cx)
            .snapshot()
            .as_ref()
            .and_then(|s| s.tasks.get(&task.id))
            .map_or(false, |g| g.exists_on_disk);

        let hover_color = cx
            .theme()
            .colors()
            .element_active
            .blend(cx.theme().colors().element_background.opacity(0.2));

        v_flex()
            .id(SharedString::from(format!("task-node-{}", task.id)))
            .debug_selector({
                let task_id = task.id.clone();
                move || format!("task-node-{}", task_id)
            })
            .w_full()
            .px_1p5()
            .cursor_pointer()
            .when(is_selected, |this| {
                this.bg(cx.theme().colors().element_active)
            })
            .hover(move |s| s.bg(hover_color))
            .on_hover(cx.listener({
                let task_id = task.id.clone();
                move |this, is_hovered, _window, cx| {
                    let prev = this.hovered_row.clone();
                    if *is_hovered {
                        this.hovered_row = Some(HoveredRow::Task(task_id.clone()));
                    } else if this.hovered_row == Some(HoveredRow::Task(task_id.clone())) {
                        this.hovered_row = None;
                    }
                    if this.hovered_row != prev {
                        cx.notify();
                    }
                }
            }))
            .on_click(cx.listener({
                let task_id = task.id.clone();
                move |this, _event, window, cx| {
                    if this
                        .editing_item
                        .as_ref()
                        .is_some_and(|item| item.target == RenamingTarget::Task(task_id.clone()))
                    {
                        return;
                    }
                    this.select_task(task_id.clone(), window, cx);
                    this.open_item_preview(PreviewItem::Task(task_id.clone()), window, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener({
                    let task_id = task.id.clone();
                    move |this, event: &MouseDownEvent, window, cx| {
                        this.deploy_context_menu(
                            ContextMenuTarget::Task(task_id.clone()),
                            event.position,
                            None,
                            window,
                            cx,
                        );
                    }
                }),
            )
            .child(
                h_flex()
                    .h_5()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .when(!row.prefix.is_empty(), |this| {
                                this.child(
                                    Label::new(row.prefix.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .buffer_font(cx),
                                )
                            })
                            .when(row.has_children, |this| {
                                this.child(
                                    div()
                                        .id(SharedString::from(format!("task-chevron-{}", task.id)))
                                        .debug_selector({
                                            let task_id = task.id.clone();
                                            move || format!("task-chevron-{}", task_id)
                                        })
                                        .w_4()
                                        .h_4()
                                        .flex_shrink_0()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .cursor_pointer()
                                        .on_click(cx.listener({
                                            let task_id = task.id.clone();
                                            move |this, _event, _window, cx| {
                                                cx.stop_propagation();
                                                if this.collapsed_tasks.contains(&task_id) {
                                                    this.collapsed_tasks.remove(&task_id);
                                                } else {
                                                    this.collapsed_tasks.insert(task_id.clone());
                                                }
                                                cx.notify();
                                            }
                                        }))
                                        .child(
                                            Icon::new(if row.is_collapsed {
                                                IconName::ChevronRight
                                            } else {
                                                IconName::ChevronDown
                                            })
                                            .size(IconSize::Small)
                                            .color(Color::Muted),
                                        ),
                                )
                            })
                            .when(!row.has_children && row.prefix.is_empty(), |this| {
                                this.child(div().w_4().h_4().flex_shrink_0())
                            })
                            .child(render_status_icon(&task.status))
                            .map(|this| {
                                if let Some(renaming) = self.editing_item.as_ref().filter(|item| {
                                    item.target == RenamingTarget::Task(task.id.clone())
                                }) {
                                    this.child(self.render_rename_editor(&renaming.editor, cx))
                                } else {
                                    this.child(
                                        Label::new(task.title.clone())
                                            .size(LabelSize::Default)
                                            .truncate(),
                                    )
                                }
                            })
                            .when(task.attempt > 1, |this| {
                                this.child(
                                    Label::new(format!("#{}", task.attempt))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .when_some(policy_denied, |this, denied_event| {
                                let denied_message = denied_event.message;
                                this.child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "task-policy-warning-{}",
                                            task.id
                                        )))
                                        .child(
                                            Icon::new(IconName::Warning)
                                                .size(IconSize::Small)
                                                .color(Color::Warning),
                                        )
                                        .tooltip(Tooltip::text(denied_message)),
                                )
                            })
                            .when(is_hovered, |this| {
                                this.child(
                                    IconButton::new(
                                        SharedString::from(format!("task-menu-{}", task.id)),
                                        IconName::Ellipsis,
                                    )
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("More Actions"))
                                    .on_click(cx.listener({
                                        let task_id = task.id.clone();
                                        move |this, event: &gpui::ClickEvent, window, cx| {
                                            cx.stop_propagation();
                                            this.deploy_context_menu(
                                                ContextMenuTarget::Task(task_id.clone()),
                                                event.position(),
                                                Some(gpui::Anchor::TopRight),
                                                window,
                                                cx,
                                            );
                                        }
                                    })),
                                )
                            }),
                    ),
            )
            .child(
                h_flex()
                    .id(SharedString::from(format!("task-meta-{}", task.id)))
                    .debug_selector({
                        let task_id = task.id.clone();
                        move || format!("task-meta-{}", task_id)
                    })
                    .h_5()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1p5()
                            .min_w_0()
                            .overflow_hidden()
                            .when(!row.continuation_prefix.is_empty(), |this| {
                                this.child(
                                    Label::new(row.continuation_prefix.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .buffer_font(cx),
                                )
                            })
                            .when(row.has_children && !row.is_collapsed, |this| {
                                this.child(
                                    div()
                                        .w_4()
                                        .h_4()
                                        .flex_shrink_0()
                                        .flex()
                                        .items_center()
                                        .child(
                                            Label::new("│")
                                                .size(LabelSize::Small)
                                                .color(Color::Muted)
                                                .buffer_font(cx),
                                        ),
                                )
                            })
                            .when(
                                (row.has_children && row.is_collapsed)
                                    || (!row.has_children && row.prefix.is_empty()),
                                |this| this.child(div().w_4().h_4().flex_shrink_0()),
                            )
                            .when(exists_on_disk, |this| {
                                this.child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "task-worktree-icon-{}",
                                            task.id
                                        )))
                                        .debug_selector({
                                            let task_id = task.id.clone();
                                            move || format!("task-worktree-icon-{}", task_id)
                                        })
                                        .child(
                                            Icon::new(IconName::GitWorktree)
                                                .size(IconSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .tooltip(Tooltip::text("Worktree on disk")),
                                )
                            })
                            .child(
                                Label::new({
                                    if task.id.starts_with("TASK-") {
                                        task.id.to_string()
                                    } else {
                                        format!("TASK-{}", task.id)
                                    }
                                })
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            )
                            .child(dot_separator())
                            .child({
                                let (profile_str, profile_color) = if let Some(p) = task
                                    .assigned_profile
                                    .as_deref()
                                    .or(task.assignee.as_deref())
                                    .filter(|s| !s.is_empty())
                                {
                                    (p.to_string(), Color::Default)
                                } else {
                                    ("no profile".to_string(), Color::Muted)
                                };
                                Label::new(profile_str)
                                    .size(LabelSize::Small)
                                    .color(profile_color)
                            })
                            .when_some(
                                task.model.as_ref().filter(|m| !m.is_empty()),
                                |this, model| {
                                    this.child(dot_separator()).child(
                                        Label::new(model.to_string())
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                },
                            )
                            .when_some(task.metrics.as_ref(), |mut this, metrics| {
                                for element in render_metrics_elements(
                                    (ElementId::from("diff-stat"), task.id.to_string()).into(),
                                    Some(metrics),
                                    true,
                                ) {
                                    this = this.child(dot_separator()).child(element);
                                }
                                this
                            }),
                    )
                    .when_some(
                        task.metrics.as_ref().and_then(|m| m.duration_ms),
                        |this, ms| {
                            this.child(
                                Label::new(format_duration_ms(ms))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .flex_shrink_0(),
                            )
                        },
                    ),
            )
            .into_any_element()
    }
}

fn dot_separator() -> impl IntoElement {
    Label::new("·").size(LabelSize::Small).color(Color::Muted)
}

#[derive(Clone, Debug, PartialEq)]
pub struct FormattedMetricItem {
    pub text: String,
    pub color: Color,
}

pub fn format_duration_ms(ms: u64) -> String {
    let s = ms / 1000;
    if ms < 60_000 {
        format!("{s}s")
    } else {
        let m = s / 60;
        let rem_s = s % 60;
        format!("{m}m {rem_s}s")
    }
}

pub fn format_compact_tokens(n: u64) -> String {
    if n >= 1000 {
        let k = n as f64 / 1000.0;
        if ((k * 10.0).round() as u64).is_multiple_of(10) {
            format!("{:.0}k", k)
        } else {
            format!("{:.1}k", k)
        }
    } else {
        n.to_string()
    }
}

pub fn format_turns(turns_count: u32) -> String {
    format!("{turns_count}")
}

pub fn format_cost(cost_usd: f64) -> String {
    format!("${cost_usd:.2}")
}

pub fn render_metrics_elements(
    id: ElementId,
    metrics: Option<&AgentTaskMetrics>,
    include_turns: bool,
) -> Vec<gpui::AnyElement> {
    let Some(metrics) = metrics else {
        return Vec::new();
    };
    let mut elements = Vec::new();

    if metrics.input_tokens.is_some()
        || metrics.output_tokens.is_some()
        || metrics.cache_read_tokens.is_some()
        || metrics.cache_creation_tokens.is_some()
    {
        let in_tok = format_compact_tokens(metrics.input_tokens.unwrap_or(0));
        let out_tok = format_compact_tokens(metrics.output_tokens.unwrap_or(0));
        let cache_tok = format_compact_tokens(
            metrics.cache_read_tokens.unwrap_or(0) + metrics.cache_creation_tokens.unwrap_or(0),
        );
        let tokens_str = format!("{in_tok}/{out_tok}/{cache_tok}");
        elements.push(
            Label::new(tokens_str)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        );
    }

    if include_turns {
        if let Some(turns) = metrics.turns_count {
            elements.push(
                h_flex()
                    .items_center()
                    .gap_0p5()
                    .child(
                        Icon::new(IconName::RotateCw)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!("{turns}"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            );
        }
    }

    if let Some(diff) = &metrics.diff {
        elements.push(
            DiffStat::new(id, diff.insertions as usize, diff.deletions as usize)
                .tooltip(format!("{} files changed", diff.files_changed))
                .into_any_element(),
        );
    }

    if let Some(cost) = metrics.cost_usd {
        let cost_str = format_cost(cost);
        elements.push(
            Label::new(cost_str)
                .size(LabelSize::Small)
                .color(Color::Success)
                .into_any_element(),
        );
    }

    elements
}

pub fn format_metrics_items(metrics: Option<&AgentTaskMetrics>) -> Vec<FormattedMetricItem> {
    let Some(metrics) = metrics else {
        return Vec::new();
    };
    let mut items = Vec::new();

    if let Some(ms) = metrics.duration_ms {
        items.push(FormattedMetricItem {
            text: format_duration_ms(ms),
            color: Color::Muted,
        });
    }

    if metrics.input_tokens.is_some()
        || metrics.output_tokens.is_some()
        || metrics.cache_read_tokens.is_some()
        || metrics.cache_creation_tokens.is_some()
    {
        let in_tok = format_compact_tokens(metrics.input_tokens.unwrap_or(0));
        let out_tok = format_compact_tokens(metrics.output_tokens.unwrap_or(0));
        let cache_tok = format_compact_tokens(
            metrics.cache_read_tokens.unwrap_or(0) + metrics.cache_creation_tokens.unwrap_or(0),
        );
        items.push(FormattedMetricItem {
            text: format!("{in_tok}/{out_tok}/{cache_tok}"),
            color: Color::Muted,
        });
    }

    if let Some(turns) = metrics.turns_count {
        items.push(FormattedMetricItem {
            text: format_turns(turns),
            color: Color::Muted,
        });
    }

    if let Some(diff) = &metrics.diff {
        items.push(FormattedMetricItem {
            text: format!("+{} -{}", diff.insertions, diff.deletions),
            color: Color::Muted,
        });
    }

    if let Some(cost) = metrics.cost_usd {
        items.push(FormattedMetricItem {
            text: format_cost(cost),
            color: Color::Success,
        });
    }

    items
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

    fn supports_flexible_size(&self) -> bool {
        true
    }

    fn has_flexible_size(&self, _window: &Window, cx: &App) -> bool {
        AgentSettings::get_global(cx).task_flexible
    }

    fn set_flexible_size(&mut self, flexible: bool, _window: &mut Window, cx: &mut Context<Self>) {
        let completion = settings::update_settings_file_with_completion(
            self.file_system.clone(),
            cx,
            move |settings, _| {
                settings
                    .agent
                    .get_or_insert_default()
                    .set_task_flexible_size(flexible);
            },
        );
        cx.spawn(async move |_this, _cx| {
            if let Err(error) = completion.await {
                log::error!("Failed to update agent task panel flexible size: {error:?}");
            }
        })
        .detach();
    }

    fn hide_button_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        Some(workspace::HideStatusItem::new(|settings| {
            settings.agent.get_or_insert_default().task_button = Some(false);
        }))
    }

    fn is_zoomed(&self, _window: &Window, _cx: &App) -> bool {
        self.zoomed
    }

    fn set_zoomed(&mut self, zoomed: bool, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoomed = zoomed;
        cx.notify();
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<IconName> {
        (AgentSettings::get_global(cx).task_button).then_some(IconName::ListTodo)
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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

        let search_editor = self
            .search_editor
            .get_or_insert_with(|| {
                let editor = cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text("Search tasks and goals…", window, cx);
                    editor
                });
                let subscription = cx.subscribe(
                    &editor,
                    |this: &mut Self, editor, event: &editor::EditorEvent, cx| {
                        if let editor::EditorEvent::BufferEdited = event {
                            this.search_query = editor.read(cx).text(cx);
                            cx.notify();
                        }
                    },
                );
                self._search_subscription = Some(subscription);
                editor
            })
            .clone();

        v_flex()
            .key_context(key_context)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &ToggleZoom, window, cx| {
                this.toggle_zoom(window, cx);
            }))
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
                    .gap_2()
                    .child(
                        h_flex()
                            .id("task-search-input")
                            .items_center()
                            .gap_1p5()
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .py_0p5()
                            .rounded_sm()
                            .bg(cx.theme().colors().editor_background)
                            .border_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(
                                Icon::new(IconName::MagnifyingGlass)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(search_editor.clone())
                            .when(!self.search_query.is_empty(), |this| {
                                let search_editor = search_editor.clone();
                                this.child(
                                    IconButton::new("clear-task-search", IconName::Close)
                                        .icon_size(IconSize::XSmall)
                                        .tooltip(Tooltip::text("Clear Search"))
                                        .on_click(cx.listener(move |this, _event, window, cx| {
                                            search_editor.update(cx, |editor, cx| {
                                                editor.set_text("", window, cx);
                                            });
                                            this.search_query.clear();
                                            cx.notify();
                                        })),
                                )
                            }),
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
                                                let (
                                                    current_categories,
                                                    current_others,
                                                    known_others,
                                                ) = view
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
                                                        (
                                                            panel.category_filters.clone(),
                                                            panel.other_status_filters.clone(),
                                                            others,
                                                        )
                                                    })
                                                    .unwrap_or_default();

                                                for category in FilterCategory::ALL {
                                                    let is_selected =
                                                        current_categories.contains(&category);
                                                    let view = view.clone();
                                                    let (icon, color) = match category {
                                                        FilterCategory::Ready => {
                                                            (IconName::Check, Color::Muted)
                                                        }
                                                        FilterCategory::Running => {
                                                            (IconName::PlayFilled, Color::Accent)
                                                        }
                                                        FilterCategory::Blocked => {
                                                            (IconName::Stop, Color::Warning)
                                                        }
                                                        FilterCategory::Failed => {
                                                            (IconName::Close, Color::Error)
                                                        }
                                                        FilterCategory::Done => {
                                                            (IconName::Check, Color::Success)
                                                        }
                                                        FilterCategory::Cancelled => {
                                                            (IconName::Close, Color::Muted)
                                                        }
                                                        FilterCategory::Archived => {
                                                            (IconName::Archive, Color::Muted)
                                                        }
                                                    };
                                                    menu = menu.item(
                                                        ContextMenuEntry::new(category.label())
                                                            .icon(icon)
                                                            .icon_color(color)
                                                            .icon_position(IconPosition::Start)
                                                            .toggle(IconPosition::End, is_selected)
                                                            .handler(move |_window, cx| {
                                                                view.update(cx, |this, cx| {
                                                                    if this
                                                                        .category_filters
                                                                        .contains(&category)
                                                                    {
                                                                        this.category_filters
                                                                            .remove(&category);
                                                                    } else {
                                                                        this.category_filters
                                                                            .insert(category);
                                                                    }
                                                                    cx.notify();
                                                                })
                                                                .log_err();
                                                            }),
                                                    );
                                                }

                                                if !known_others.is_empty() {
                                                    menu = menu.separator();
                                                    for other_status in known_others {
                                                        let is_selected =
                                                            current_others.contains(&other_status);
                                                        let view = view.clone();
                                                        let filter_status = other_status.clone();
                                                        let icon = status_icon(&other_status);
                                                        let color = status_color(&other_status);
                                                        menu = menu.item(
                                                            ContextMenuEntry::new(status_label(
                                                                &other_status,
                                                            ))
                                                            .icon(icon)
                                                            .icon_color(color)
                                                            .icon_position(IconPosition::Start)
                                                            .toggle(IconPosition::End, is_selected)
                                                            .handler(move |_window, cx| {
                                                                view.update(cx, |this, cx| {
                                                                    if this
                                                                        .other_status_filters
                                                                        .contains(&filter_status)
                                                                    {
                                                                        this.other_status_filters
                                                                            .remove(&filter_status);
                                                                    } else {
                                                                        this.other_status_filters
                                                                            .insert(
                                                                                filter_status
                                                                                    .clone(),
                                                                            );
                                                                    }
                                                                    cx.notify();
                                                                })
                                                                .log_err();
                                                            }),
                                                        );
                                                    }
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
                                        let task = store.update(cx, |store, cx| store.refresh(cx));
                                        Self::spawn_task_with_action_error_toast(
                                            task,
                                            Some("Refresh"),
                                            this.workspace.clone(),
                                            cx,
                                        );
                                        this.refresh_worktree_status(cx);
                                    })),
                            )
                            .child({
                                let is_zoomed = self.zoomed;
                                let (zoom_icon, zoom_tooltip) = if is_zoomed {
                                    (IconName::Minimize, "Disable Full Screen")
                                } else {
                                    (IconName::Maximize, "Enable Full Screen")
                                };
                                IconButton::new("toggle_zoom", zoom_icon)
                                    .icon_size(IconSize::Small)
                                    .tooltip(move |_window, cx| {
                                        Tooltip::for_action(zoom_tooltip, &ToggleZoom, cx)
                                    })
                                    .on_click(cx.listener(|this, _event, window, cx| {
                                        this.toggle_zoom(window, cx);
                                    }))
                            }),
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
            .children(self.context_menu.as_ref().map(|context_menu| {
                deferred(
                    anchored()
                        .position(context_menu.position)
                        .when_some(context_menu.anchor, |anchored, anchor| {
                            anchored.anchor(anchor)
                        })
                        .child(context_menu.menu.clone()),
                )
                .with_priority(3)
            }))
    }
}

pub fn goal_status_icon(status: &AgentGoalStatus) -> IconName {
    match status {
        AgentGoalStatus::Running => IconName::PlayFilled,
        AgentGoalStatus::Blocked => IconName::Stop,
        AgentGoalStatus::Failed => IconName::Close,
        AgentGoalStatus::Completed => IconName::Check,
        AgentGoalStatus::Cancelled => IconName::XCircle,
        AgentGoalStatus::Archived => IconName::Archive,
        AgentGoalStatus::Other(_) => IconName::Circle,
    }
}

pub fn goal_status_color(status: &AgentGoalStatus) -> Color {
    match status {
        AgentGoalStatus::Running => Color::Info,
        AgentGoalStatus::Blocked => Color::Warning,
        AgentGoalStatus::Failed => Color::Error,
        AgentGoalStatus::Completed => Color::Success,
        AgentGoalStatus::Cancelled => Color::Muted,
        AgentGoalStatus::Archived => Color::Muted,
        AgentGoalStatus::Other(_) => Color::Muted,
    }
}

pub fn render_goal_status_icon(status: &AgentGoalStatus) -> impl IntoElement {
    if *status == AgentGoalStatus::Running {
        Icon::new(IconName::LoadCircle)
            .size(IconSize::Small)
            .color(Color::Accent)
            .with_rotate_animation(3)
            .into_any_element()
    } else {
        Icon::new(goal_status_icon(status))
            .size(IconSize::Small)
            .color(goal_status_color(status))
            .into_any_element()
    }
}

pub fn priority_color(priority: i64) -> Color {
    match priority {
        0 | 1 => Color::Error,
        2 => Color::Warning,
        _ => Color::Muted,
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
        AgentTaskStatus::Cancelled => Color::Muted,
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
        AgentTaskStatus::Cancelled => IconName::XCircle,
        AgentTaskStatus::Archived => IconName::Archive,
        AgentTaskStatus::Other(_) => IconName::Circle,
    }
}

pub fn render_status_icon(status: &AgentTaskStatus) -> impl IntoElement {
    if *status == AgentTaskStatus::Running {
        Icon::new(IconName::LoadCircle)
            .size(IconSize::Small)
            .color(Color::Accent)
            .with_rotate_animation(3)
            .into_any_element()
    } else {
        Icon::new(status_icon(status))
            .size(IconSize::Small)
            .color(status_color(status))
            .into_any_element()
    }
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
        AgentTaskStatus::Cancelled => "Cancelled",
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

pub fn render_goal_markdown(goal: &AgentGoalSummary, goal_git: Option<&GoalGitState>) -> String {
    let mut doc = String::new();
    doc.push_str(&format!("# {}\n\n", goal.title));
    doc.push_str(&format!("- **Status:** {}\n", goal.status));
    doc.push_str(&format!("- **Goal ID:** {}\n", goal.goal_id));
    doc.push_str(&format!("- **Priority:** P{}\n", goal.priority));
    doc.push_str(&format!(
        "- **Tasks:** {}/{} completed\n",
        goal.tasks_done, goal.tasks_total
    ));
    if let Some(created_at) = goal.created_at {
        doc.push_str(&format!("- **Created At:** {}\n", created_at));
    }

    if let Some(metrics) = &goal.metrics {
        render_metrics_markdown(&mut doc, metrics);
    }

    if let Some(git) = goal_git {
        doc.push_str("\n## Git\n\n");
        doc.push_str(&format!("- **Branch:** `{}`\n", git.branch));
        doc.push_str(&format!("- **Branch Exists:** {}\n", git.branch_exists));
        if let Some(sha) = &git.tip_sha {
            let short_sha: String = sha.chars().take(7).collect();
            doc.push_str(&format!("- **Tip SHA:** `{short_sha}` ({sha})\n"));
        }
    }

    doc
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
    if let Some(profile) = detail
        .summary
        .assigned_profile
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        doc.push_str(&format!("- **Profile:** {}\n", profile));
    }
    if let Some(model) = detail.summary.model.as_deref().filter(|s| !s.is_empty()) {
        doc.push_str(&format!("- **Model:** {}\n", model));
    }
    if let Some(assignee) = &detail.summary.assignee {
        doc.push_str(&format!("- **Assignee:** {}\n", assignee));
    }
    if detail.summary.attempt > 1 {
        doc.push_str(&format!("- **Attempt:** {}\n", detail.summary.attempt));
    }

    if let Some(metrics) = &detail.summary.metrics {
        render_metrics_markdown(&mut doc, metrics);
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
            let short_sha: String = sha.chars().take(7).collect();
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
        doc.push_str("\n## Events\n\n");
        for event in &detail.events_tail {
            let kind = event_kind_name(&event.kind);
            doc.push_str(&format!(
                "- `#{}` [{}] {}\n",
                event.seq, kind, event.message
            ));
        }
    }

    doc
}

pub fn render_metrics_markdown(doc: &mut String, metrics: &AgentTaskMetrics) {
    doc.push_str("\n## Metrics\n\n");
    if let Some(ms) = metrics.duration_ms {
        let duration_s = ms / 1000;
        doc.push_str(&format!("- **Duration:** {duration_s}s\n"));
    }
    let in_tok = metrics.input_tokens.unwrap_or(0);
    let out_tok = metrics.output_tokens.unwrap_or(0);
    let cache_tok =
        metrics.cache_read_tokens.unwrap_or(0) + metrics.cache_creation_tokens.unwrap_or(0);
    if metrics.input_tokens.is_some()
        || metrics.output_tokens.is_some()
        || metrics.cache_read_tokens.is_some()
        || metrics.cache_creation_tokens.is_some()
    {
        doc.push_str(&format!(
            "- **Tokens:** {in_tok} in / {out_tok} out / {cache_tok} cache\n"
        ));
    }
    if let Some(turns) = metrics.turns_count {
        doc.push_str(&format!("- **Turns:** {turns}\n"));
    }
    if let Some(cost) = metrics.cost_usd {
        doc.push_str(&format!("- **Cost:** ${cost:.2}\n"));
    }
    if let Some(diff) = &metrics.diff {
        doc.push_str(&format!(
            "- **Diff:** {} file(s) changed (+{}, -{})\n",
            diff.files_changed, diff.insertions, diff.deletions
        ));
    }
}

#[allow(dead_code)]
pub fn format_task_meta_line(task: &AgentTaskSummary) -> String {
    let task_seg = if task.id.starts_with("TASK-") {
        task.id.to_string()
    } else {
        format!("TASK-{}", task.id)
    };
    let mut parts = vec![task_seg];
    let profile = task
        .assigned_profile
        .as_deref()
        .or(task.assignee.as_deref())
        .filter(|s| !s.is_empty())
        .unwrap_or("no profile");
    parts.push(profile.to_string());
    if let Some(model) = task.model.as_ref().filter(|m| !m.is_empty()) {
        parts.push(model.to_string());
    }
    for item in format_metrics_items(task.metrics.as_ref()) {
        parts.push(item.text);
    }

    parts.join(" · ")
}

#[allow(dead_code)]
pub fn format_goal_meta_line(
    goal_id: &str,
    _status: &AgentGoalStatus,
    priority: i64,
    tasks_done: u64,
    tasks_total: u64,
    metrics: Option<&AgentTaskMetrics>,
) -> String {
    let goal_seg = if goal_id.starts_with("GOAL-") {
        goal_id.to_string()
    } else {
        format!("GOAL-{goal_id}")
    };
    let mut parts = vec![
        goal_seg,
        format!("{tasks_done} / {tasks_total}"),
        format!("P{priority}"),
    ];
    for item in format_metrics_items(metrics) {
        parts.push(item.text);
    }
    parts.join(" · ")
}

pub fn open_markdown_preview(
    title: String,
    markdown: String,
    workspace: Entity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Task<anyhow::Result<()>> {
    let markdown_language_task = workspace
        .read(cx)
        .app_state()
        .languages
        .language_for_name("Markdown");
    let project = workspace.read(cx).project().clone();

    window.spawn(cx, async move |cx| {
        let markdown_language = markdown_language_task.await?;

        let buffer = project
            .update(cx, |project, cx| {
                project.create_buffer(Some(markdown_language), false, cx)
            })
            .await?;

        buffer.update(cx, |buffer, cx| {
            buffer.set_text(markdown, cx);
            buffer.set_capability(language::Capability::ReadOnly, cx);
        });

        workspace.update_in(cx, |workspace, window, cx| {
            let multibuffer =
                cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title.clone()));
            let editor = cx.new(|cx| {
                let mut editor =
                    Editor::for_multibuffer(multibuffer, Some(project.clone()), window, cx);
                editor.set_breadcrumb_header(title);
                editor.disable_mouse_wheel_zoom();
                editor
            });
            let pane = workspace.active_pane().clone();
            markdown_preview::markdown_preview_view::MarkdownPreviewView::open_preview_in_pane(
                workspace, editor, pane, window, cx,
            );
        })?;
        Ok(())
    })
}

pub fn filter_visible_tasks(
    tasks: &[AgentTaskSummary],
    category_filters: &HashSet<FilterCategory>,
    other_status_filters: &HashSet<AgentTaskStatus>,
) -> Vec<AgentTaskSummary> {
    tasks
        .iter()
        .filter(|task| {
            category_filters
                .iter()
                .any(|cat| cat.matches_task(&task.status))
                || other_status_filters.contains(&task.status)
        })
        .cloned()
        .collect()
}

pub fn sort_tasks_by_created_at(tasks: &mut [AgentTaskSummary]) {
    tasks.sort_by(|a, b| match (a.created_at, b.created_at) {
        (Some(ta), Some(tb)) => ta.cmp(&tb).then_with(|| a.id.cmp(&b.id)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.id.cmp(&b.id),
    });
}

pub fn visible_roots(visible_tasks: &[AgentTaskSummary]) -> Vec<AgentTaskSummary> {
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
    sort_tasks_by_created_at(&mut roots);
    roots
}

#[allow(dead_code)]
pub fn visible_children(
    parent_id: &AgentTaskId,
    visible_tasks: &[AgentTaskSummary],
) -> Vec<AgentTaskSummary> {
    let mut children: Vec<AgentTaskSummary> = visible_tasks
        .iter()
        .filter(|task| task.parent_id.as_ref().map_or(false, |id| id == parent_id))
        .cloned()
        .collect();
    sort_tasks_by_created_at(&mut children);
    children
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskRow {
    pub task: AgentTaskSummary,
    pub prefix: String,
    pub continuation_prefix: String,
    pub depth: usize,
    pub has_children: bool,
    pub is_collapsed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GoalGroup {
    pub goal_id: Option<String>,
    pub goal_summary: Option<AgentGoalSummary>,
    pub rows: Vec<TaskRow>,
}

pub fn build_task_tree_rows(
    visible_tasks: &[AgentTaskSummary],
    collapsed_tasks: &HashSet<AgentTaskId>,
    under_goal: bool,
) -> Vec<TaskRow> {
    let roots = visible_roots(visible_tasks);
    let num_roots = roots.len();
    let mut rows = Vec::new();
    let mut visited = HashSet::new();

    let mut children_by_parent: HashMap<AgentTaskId, Vec<AgentTaskSummary>> = HashMap::new();
    for task in visible_tasks {
        if let Some(parent_id) = &task.parent_id {
            children_by_parent
                .entry(parent_id.clone())
                .or_default()
                .push(task.clone());
        }
    }
    for children in children_by_parent.values_mut() {
        sort_tasks_by_created_at(children);
    }

    fn collect_node(
        task: AgentTaskSummary,
        children_by_parent: &mut HashMap<AgentTaskId, Vec<AgentTaskSummary>>,
        prefix: &str,
        continuation_prefix: &str,
        ancestor_continuation: &str,
        depth: usize,
        collapsed_tasks: &HashSet<AgentTaskId>,
        visited: &mut HashSet<AgentTaskId>,
        rows: &mut Vec<TaskRow>,
    ) {
        if depth > 16 || !visited.insert(task.id.clone()) {
            log::error!("skipping cyclic or too deep task subtree at {}", task.id);
            return;
        }

        let children = children_by_parent.remove(&task.id).unwrap_or_default();
        let has_children = !children.is_empty();
        let is_collapsed = collapsed_tasks.contains(&task.id);

        rows.push(TaskRow {
            task,
            prefix: prefix.to_string(),
            continuation_prefix: continuation_prefix.to_string(),
            depth,
            has_children,
            is_collapsed,
        });

        if !is_collapsed && has_children {
            let num_children = children.len();
            for (index, child) in children.into_iter().enumerate() {
                let is_last_child = index + 1 == num_children;
                let branch = if is_last_child {
                    "└── "
                } else {
                    "├── "
                };
                let cont = if is_last_child { "    " } else { "│   " };
                let child_prefix = format!("{ancestor_continuation}{branch}");
                let child_continuation = format!("{ancestor_continuation}{cont}");
                let next_ancestor = format!("{ancestor_continuation}{cont}");
                collect_node(
                    child,
                    children_by_parent,
                    &child_prefix,
                    &child_continuation,
                    &next_ancestor,
                    depth + 1,
                    collapsed_tasks,
                    visited,
                    rows,
                );
            }
        }
    }

    for (root_idx, root) in roots.into_iter().enumerate() {
        let is_last_root = root_idx + 1 == num_roots;
        let (prefix, continuation_prefix, ancestor_continuation) = if under_goal {
            let branch = if is_last_root {
                "└── "
            } else {
                "├── "
            };
            let cont = if is_last_root { "    " } else { "│   " };
            (branch, cont, cont)
        } else {
            ("", "", "")
        };

        collect_node(
            root,
            &mut children_by_parent,
            prefix,
            continuation_prefix,
            ancestor_continuation,
            0,
            collapsed_tasks,
            &mut visited,
            &mut rows,
        );
    }

    rows
}

pub fn build_goal_groups(
    tasks: &[AgentTaskSummary],
    goals: &[AgentGoalSummary],
    category_filters: &HashSet<FilterCategory>,
    other_status_filters: &HashSet<AgentTaskStatus>,
    search_query: &str,
    collapsed_tasks: &HashSet<AgentTaskId>,
) -> Vec<GoalGroup> {
    let visible_tasks = filter_visible_tasks(tasks, category_filters, other_status_filters);

    let trimmed_search = search_query.trim().to_lowercase();
    let has_search = !trimmed_search.is_empty();

    let mut tasks_by_goal: HashMap<Option<String>, Vec<AgentTaskSummary>> = HashMap::new();
    for task in visible_tasks {
        tasks_by_goal
            .entry(task.goal_id.clone())
            .or_default()
            .push(task);
    }

    let goals_by_id: HashMap<&str, &AgentGoalSummary> =
        goals.iter().map(|g| (g.goal_id.as_str(), g)).collect();

    let mut all_goal_ids: HashSet<String> = goals.iter().map(|g| g.goal_id.clone()).collect();
    for id in tasks_by_goal.keys().flatten() {
        all_goal_ids.insert(id.clone());
    }

    let mut sorted_goals: Vec<(String, Option<AgentGoalSummary>)> = all_goal_ids
        .into_iter()
        .map(|id| {
            let summary = goals_by_id.get(id.as_str()).copied().cloned();
            (id, summary)
        })
        .collect();

    sorted_goals.sort_by(|(id_a, summary_a), (id_b, summary_b)| {
        let created_a = summary_a.as_ref().and_then(|s| s.created_at);
        let created_b = summary_b.as_ref().and_then(|s| s.created_at);
        match (created_a, created_b) {
            (Some(ca), Some(cb)) => cb
                .cmp(&ca)
                .then_with(|| {
                    let prio_a = summary_a.as_ref().map_or(i64::MAX, |s| s.priority);
                    let prio_b = summary_b.as_ref().map_or(i64::MAX, |s| s.priority);
                    prio_a.cmp(&prio_b)
                })
                .then_with(|| id_a.cmp(id_b)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => {
                let prio_a = summary_a.as_ref().map_or(i64::MAX, |s| s.priority);
                let prio_b = summary_b.as_ref().map_or(i64::MAX, |s| s.priority);
                prio_a.cmp(&prio_b).then_with(|| id_a.cmp(id_b))
            }
        }
    });

    let filter_task_set_by_search = |tasks: Vec<AgentTaskSummary>| -> Vec<AgentTaskSummary> {
        if !has_search {
            return tasks;
        }
        let matching_ids: HashSet<AgentTaskId> = tasks
            .iter()
            .filter(|t| t.title.to_lowercase().contains(&trimmed_search))
            .map(|t| t.id.clone())
            .collect();

        if matching_ids.is_empty() {
            return Vec::new();
        }

        let parents_by_id: HashMap<&AgentTaskId, &Option<AgentTaskId>> =
            tasks.iter().map(|t| (&t.id, &t.parent_id)).collect();

        let mut keep_ids = matching_ids.clone();
        for task in &tasks {
            if matching_ids.contains(&task.id) {
                let mut current_parent = task.parent_id.as_ref();
                while let Some(parent_id) = current_parent {
                    if !keep_ids.insert(parent_id.clone()) {
                        break;
                    }
                    current_parent = parents_by_id.get(parent_id).and_then(|p| p.as_ref());
                }
            }
        }

        tasks
            .into_iter()
            .filter(|t| keep_ids.contains(&t.id))
            .collect()
    };

    let mut result = Vec::new();

    for (goal_id, summary) in sorted_goals {
        let goal_tasks = tasks_by_goal
            .remove(&Some(goal_id.clone()))
            .unwrap_or_default();

        let goal_title = summary
            .as_ref()
            .map(|s| s.title.as_str())
            .unwrap_or(goal_id.as_str());
        let goal_title_matches = has_search && goal_title.to_lowercase().contains(&trimmed_search);

        let filtered_tasks = if goal_title_matches {
            goal_tasks
        } else {
            filter_task_set_by_search(goal_tasks)
        };

        let goal_status_matches = summary.as_ref().map_or(false, |s| {
            category_filters
                .iter()
                .any(|cat| cat.matches_goal(&s.status))
                || match &s.status {
                    AgentGoalStatus::Other(name) => {
                        other_status_filters.iter().any(|st| match st {
                            AgentTaskStatus::Other(n) => n == name,
                            _ => false,
                        })
                    }
                    _ => false,
                }
        });

        if !filtered_tasks.is_empty() || goal_title_matches || (!has_search && goal_status_matches)
        {
            let rows = build_task_tree_rows(&filtered_tasks, collapsed_tasks, true);
            result.push(GoalGroup {
                goal_id: Some(goal_id),
                goal_summary: summary,
                rows,
            });
        }
    }

    if let Some(no_goal_tasks) = tasks_by_goal.remove(&None) {
        let filtered_no_goal = filter_task_set_by_search(no_goal_tasks);
        if !filtered_no_goal.is_empty() {
            let rows = build_task_tree_rows(&filtered_no_goal, collapsed_tasks, false);
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
pub fn build_task_rows(
    tasks: &[AgentTaskSummary],
    category_filters: &HashSet<FilterCategory>,
) -> Vec<TaskRow> {
    let empty_others = HashSet::new();
    let empty_collapsed = HashSet::new();
    let visible = filter_visible_tasks(tasks, category_filters, &empty_others);
    build_task_tree_rows(&visible, &empty_collapsed, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::{
        AgentDiffShortstat, AgentTaskEvent, AgentTaskGraph, AgentTaskProvider, AgentTaskStatus,
        GoalGitState, OrphanWorktree, TaskGitSnapshot, TaskGitState,
    };
    use context_server::ContextServerId;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use project::Project;
    use settings::SettingsStore;

    #[derive(Default)]
    struct CallCounts {
        archive_calls: Vec<AgentTaskId>,
        unarchive_calls: Vec<AgentTaskId>,
        delete_calls: Vec<AgentTaskId>,
        archive_goal_calls: Vec<String>,
        unarchive_goal_calls: Vec<String>,
        delete_goal_calls: Vec<String>,
        set_task_status_calls: Vec<(AgentTaskId, AgentTaskStatus)>,
        set_goal_status_calls: Vec<(String, AgentGoalStatus)>,
        complete_calls: Vec<AgentTaskId>,
        update_task_title_calls: Vec<(AgentTaskId, String)>,
        update_goal_title_calls: Vec<(String, String)>,
    }

    struct TestProvider {
        offline: bool,
        tasks: Vec<AgentTaskSummary>,
        goals: Vec<AgentGoalSummary>,
        #[allow(dead_code)]
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
                        status: AgentGoalStatus::Running,
                        priority: 1,
                        tasks_total: 1,
                        tasks_done: 0,
                        created_at: Some(100),
                        metrics: None,
                        session_id: None,
                    },
                    AgentGoalSummary {
                        goal_id: "GOAL-2".to_string(),
                        title: "Goal 2".to_string(),
                        status: AgentGoalStatus::Running,
                        priority: 2,
                        tasks_total: 0,
                        tasks_done: 0,
                        created_at: Some(200),
                        metrics: None,
                        session_id: None,
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
                        created_at: Some(150),
                        assigned_profile: Some("agent_engineer".into()),
                        model: Some("glm-5.3".into()),
                        session_id: None,
                        metrics: None,
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
                        created_at: Some(250),
                        assigned_profile: None,
                        model: None,
                        session_id: None,
                        metrics: None,
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
                    created_at: None,
                    assigned_profile: None,
                    model: None,
                    session_id: None,
                    metrics: None,
                });
            Task::ready(Ok(AgentTaskDetail {
                summary: task,
                description: "Test Description".to_string(),
                acceptance_criteria: vec!["Criterion 1".to_string()],
                events_tail: vec![],
            }))
        }

        fn complete_task(&self, id: &AgentTaskId, _cx: &mut App) -> Task<anyhow::Result<()>> {
            self.calls.lock().unwrap().complete_calls.push(id.clone());
            Task::ready(Ok(()))
        }

        fn update_task_title(
            &self,
            id: &AgentTaskId,
            title: &str,
            _cx: &mut App,
        ) -> Task<anyhow::Result<()>> {
            self.calls
                .lock()
                .unwrap()
                .update_task_title_calls
                .push((id.clone(), title.to_string()));
            Task::ready(Ok(()))
        }

        fn update_goal_title(
            &self,
            goal_id: &str,
            title: &str,
            _cx: &mut App,
        ) -> Task<anyhow::Result<()>> {
            self.calls
                .lock()
                .unwrap()
                .update_goal_title_calls
                .push((goal_id.to_string(), title.to_string()));
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

        fn archive_goal(&self, id: &str, _cx: &mut App) -> Task<anyhow::Result<()>> {
            self.calls
                .lock()
                .unwrap()
                .archive_goal_calls
                .push(id.to_string());
            Task::ready(Ok(()))
        }

        fn unarchive_goal(&self, id: &str, _cx: &mut App) -> Task<anyhow::Result<()>> {
            self.calls
                .lock()
                .unwrap()
                .unarchive_goal_calls
                .push(id.to_string());
            Task::ready(Ok(()))
        }

        fn delete_goal(&self, id: &str, _cx: &mut App) -> Task<anyhow::Result<()>> {
            self.calls
                .lock()
                .unwrap()
                .delete_goal_calls
                .push(id.to_string());
            Task::ready(Ok(()))
        }

        fn set_task_status(
            &self,
            id: &AgentTaskId,
            status: &AgentTaskStatus,
            _cx: &mut App,
        ) -> Task<anyhow::Result<()>> {
            self.calls
                .lock()
                .unwrap()
                .set_task_status_calls
                .push((id.clone(), status.clone()));
            Task::ready(Ok(()))
        }

        fn set_goal_status(
            &self,
            id: &str,
            status: &AgentGoalStatus,
            _cx: &mut App,
        ) -> Task<anyhow::Result<()>> {
            self.calls
                .lock()
                .unwrap()
                .set_goal_status_calls
                .push((id.to_string(), status.clone()));
            Task::ready(Ok(()))
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
            created_at: Some(10),
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: Some(20),
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: Some(30),
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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

        panel.read_with(cx, |panel, cx| {
            assert!(panel.category_filters.contains(&FilterCategory::Done));
            let store = panel.store.read(cx);
            let visible = filter_visible_tasks(
                &store.graph().tasks,
                &panel.category_filters,
                &panel.other_status_filters,
            );
            assert_eq!(visible.len(), 3);

            let roots = visible_roots(&visible);
            assert_eq!(roots.len(), 2);
            let root_ids: Vec<&str> = roots.iter().map(|t| t.id.0.as_ref()).collect();
            assert!(root_ids.contains(&"TASK-COMPLETED"));
            assert!(root_ids.contains(&"TASK-READY"));

            let children = visible_children(&AgentTaskId::from("TASK-COMPLETED"), &visible);
            assert_eq!(children.len(), 1);
            assert_eq!(children[0].id.0.as_ref(), "TASK-FAILED-CHILD");

            let rows = build_task_rows(&store.graph().tasks, &panel.category_filters);
            assert_eq!(rows.len(), 3);
            let row_info: Vec<(&str, &str, usize)> = rows
                .iter()
                .map(|r| (r.task.id.0.as_ref(), r.prefix.as_str(), r.depth))
                .collect();
            assert_eq!(
                row_info,
                vec![
                    ("TASK-COMPLETED", "", 0),
                    ("TASK-FAILED-CHILD", "└── ", 1),
                    ("TASK-READY", "", 0),
                ]
            );
        });

        panel.update(cx, |panel, cx| {
            panel.category_filters.remove(&FilterCategory::Done);
            cx.notify();
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            assert!(!panel.category_filters.contains(&FilterCategory::Done));
            let store = panel.store.read(cx);
            let visible = filter_visible_tasks(
                &store.graph().tasks,
                &panel.category_filters,
                &panel.other_status_filters,
            );
            assert_eq!(visible.len(), 2);
            assert!(
                visible
                    .iter()
                    .all(|t| t.status != AgentTaskStatus::Completed)
            );

            let roots = visible_roots(&visible);
            assert_eq!(roots.len(), 2);
            let root_ids: Vec<&str> = roots.iter().map(|t| t.id.0.as_ref()).collect();
            assert!(root_ids.contains(&"TASK-FAILED-CHILD"));
            assert!(root_ids.contains(&"TASK-READY"));

            let rows = build_task_rows(&store.graph().tasks, &panel.category_filters);
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].prefix, "");
            assert_eq!(rows[0].depth, 0);
            assert_eq!(rows[1].prefix, "");
            assert_eq!(rows[1].depth, 0);
        });
    }

    #[test]
    fn test_filter_category_mapping() {
        assert!(FilterCategory::Ready.matches_task(&AgentTaskStatus::Ready));
        assert!(!FilterCategory::Ready.matches_task(&AgentTaskStatus::Running));
        assert!(FilterCategory::Running.matches_task(&AgentTaskStatus::Running));
        assert!(FilterCategory::Blocked.matches_task(&AgentTaskStatus::Blocked));
        assert!(FilterCategory::Failed.matches_task(&AgentTaskStatus::Failed));
        assert!(FilterCategory::Done.matches_task(&AgentTaskStatus::Completed));
        assert!(FilterCategory::Cancelled.matches_task(&AgentTaskStatus::Cancelled));
        assert!(FilterCategory::Archived.matches_task(&AgentTaskStatus::Archived));

        assert!(FilterCategory::Ready.matches_goal(&AgentGoalStatus::Running));
        assert!(FilterCategory::Running.matches_goal(&AgentGoalStatus::Running));
        assert!(!FilterCategory::Ready.matches_goal(&AgentGoalStatus::Blocked));
        assert!(FilterCategory::Blocked.matches_goal(&AgentGoalStatus::Blocked));
        assert!(FilterCategory::Failed.matches_goal(&AgentGoalStatus::Failed));
        assert!(FilterCategory::Done.matches_goal(&AgentGoalStatus::Completed));
        assert!(FilterCategory::Cancelled.matches_goal(&AgentGoalStatus::Cancelled));
        assert!(FilterCategory::Archived.matches_goal(&AgentGoalStatus::Archived));
    }

    #[test]
    fn test_search_filtering() {
        let goal_1 = AgentGoalSummary {
            goal_id: "GOAL-1".to_string(),
            title: "Database Refactor".to_string(),
            status: AgentGoalStatus::Running,
            priority: 1,
            tasks_total: 2,
            tasks_done: 0,
            created_at: Some(100),
            metrics: None,
            session_id: None,
        };
        let goal_2 = AgentGoalSummary {
            goal_id: "GOAL-2".to_string(),
            title: "Frontend Overhaul".to_string(),
            status: AgentGoalStatus::Running,
            priority: 2,
            tasks_total: 2,
            tasks_done: 0,
            created_at: Some(200),
            metrics: None,
            session_id: None,
        };

        let task_1a = AgentTaskSummary {
            id: AgentTaskId::from("TASK-1A"),
            parent_id: None,
            goal_id: Some("GOAL-1".to_string()),
            title: "Schema Migration".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: Some(110),
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        let task_1b = AgentTaskSummary {
            id: AgentTaskId::from("TASK-1B"),
            parent_id: Some(AgentTaskId::from("TASK-1A")),
            goal_id: Some("GOAL-1".to_string()),
            title: "Run Migration Script".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: Some(120),
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        let task_2_parent = AgentTaskSummary {
            id: AgentTaskId::from("TASK-2P"),
            parent_id: None,
            goal_id: Some("GOAL-2".to_string()),
            title: "Setup Build".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: Some(210),
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        let task_2_child = AgentTaskSummary {
            id: AgentTaskId::from("TASK-2C"),
            parent_id: Some(AgentTaskId::from("TASK-2P")),
            goal_id: Some("GOAL-2".to_string()),
            title: "Tailwind Integration".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: Some(220),
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        let goals = vec![goal_1, goal_2];
        let tasks = vec![task_1a, task_1b, task_2_parent, task_2_child];
        let cat_filters = default_category_filters();
        let other_filters = HashSet::new();
        let collapsed = HashSet::new();

        let groups_by_goal_match = build_goal_groups(
            &tasks,
            &goals,
            &cat_filters,
            &other_filters,
            "Database",
            &collapsed,
        );
        assert_eq!(groups_by_goal_match.len(), 1);
        assert_eq!(groups_by_goal_match[0].goal_id.as_deref(), Some("GOAL-1"));
        assert_eq!(groups_by_goal_match[0].rows.len(), 2);

        let groups_by_task_match = build_goal_groups(
            &tasks,
            &goals,
            &cat_filters,
            &other_filters,
            "Tailwind",
            &collapsed,
        );
        assert_eq!(groups_by_task_match.len(), 1);
        assert_eq!(groups_by_task_match[0].goal_id.as_deref(), Some("GOAL-2"));
        assert_eq!(groups_by_task_match[0].rows.len(), 2);
        assert_eq!(
            groups_by_task_match[0].rows[0].task.id.0.as_ref(),
            "TASK-2P"
        );
        assert_eq!(
            groups_by_task_match[0].rows[1].task.id.0.as_ref(),
            "TASK-2C"
        );
    }

    #[test]
    fn test_agent_task_panel_created_at_ordering() {
        let task_1 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-1"),
            parent_id: None,
            goal_id: None,
            title: "Task 1".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: Some(200),
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        let task_2 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-2"),
            parent_id: None,
            goal_id: None,
            title: "Task 2".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: Some(100),
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        let task_none = AgentTaskSummary {
            id: AgentTaskId::from("TASK-NONE"),
            parent_id: None,
            goal_id: None,
            title: "Task None".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        let tasks = vec![task_1, task_2, task_none];
        let rows = build_task_rows(&tasks, &default_category_filters());
        let ids: Vec<&str> = rows.iter().map(|r| r.task.id.0.as_ref()).collect();
        assert_eq!(ids, vec!["TASK-2", "TASK-1", "TASK-NONE"]);
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
            created_at: Some(1000),
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: Some(3000),
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: Some(3500),
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: Some(3400),
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: Some(3100),
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
        let rows = build_task_rows(&tasks, &default_category_filters());
        let extracted: Vec<(&str, &str, usize)> = rows
            .iter()
            .map(|r| (r.task.id.0.as_ref(), r.prefix.as_str(), r.depth))
            .collect();
        assert_eq!(
            extracted,
            vec![
                ("TASK-A", "", 0),
                ("TASK-B", "", 0),
                ("TASK-B-CHILD-2", "├── ", 1),
                ("TASK-B-CHILD-1", "└── ", 1),
                ("TASK-B-GRANDCHILD", "    └── ", 2),
                ("TASK-C", "", 0),
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
                created_at: Some(100),
                assigned_profile: Some("agent_engineer".into()),
                model: Some("glm-5.3".into()),
                session_id: None,
                metrics: None,
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
        assert!(md.contains("- **Profile:** agent_engineer"));
        assert!(md.contains("- **Model:** glm-5.3"));
        assert!(md.contains("- **Assignee:** ui_engineer"));
        assert!(md.contains("- **Attempt:** 2"));
        assert!(md.contains("## Description\n\nDetailed description of the rework."));
        assert!(md.contains("## Acceptance Criteria"));
        assert!(md.contains("- [ ] Criteria 1: colored icons"));
        assert!(md.contains("- [ ] Criteria 2: box tree"));
        assert!(md.contains("## Artifacts"));
        assert!(md.contains("### review — art-1\n\nLGTM approved"));
        assert!(md.contains("## Events"));
        assert!(md.contains("- `#159` [STATUS] task.created"));
        assert!(md.contains("- `#160` [TOOL] run terminal"));
        assert!(!md.contains("- **Diff:**"));

        let mut detail_with_diff = detail;
        detail_with_diff.summary.metrics = Some(AgentTaskMetrics {
            diff: Some(AgentDiffShortstat {
                files_changed: 4,
                insertions: 25,
                deletions: 12,
            }),
            ..Default::default()
        });
        let md_with_diff = render_task_markdown(&detail_with_diff, &artifacts, None, None);
        assert!(md_with_diff.contains("- **Diff:** 4 file(s) changed (+25, -12)"));
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
    fn test_agent_task_panel_renders_goal_markdown() {
        let goal_summary = AgentGoalSummary {
            goal_id: "GOAL-UI-1".to_string(),
            title: "Goal UI 1".to_string(),
            status: AgentGoalStatus::Running,
            priority: 1,
            tasks_total: 2,
            tasks_done: 1,
            created_at: Some(12345),
            metrics: None,
            session_id: None,
        };
        let goal_git = GoalGitState {
            branch: "agent-goal/GOAL-UI-1".to_string(),
            branch_exists: true,
            tip_sha: Some("1234567890abcdef".to_string()),
        };

        let md = render_goal_markdown(&goal_summary, Some(&goal_git));
        assert!(md.contains("# Goal UI 1"));
        assert!(md.contains("- **Status:** running"));
        assert!(md.contains("- **Goal ID:** GOAL-UI-1"));
        assert!(md.contains("- **Priority:** P1"));
        assert!(md.contains("- **Tasks:** 1/2 completed"));
        assert!(md.contains("- **Branch:** `agent-goal/GOAL-UI-1`"));
        assert!(md.contains("- **Tip SHA:** `1234567` (1234567890abcdef)"));
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

        cx.debug_bounds("goal-group-header-GOAL-1")
            .expect("GOAL-1 header should be rendered");
        cx.debug_bounds("goal-group-header-GOAL-2")
            .expect("GOAL-2 header should be rendered");
        cx.debug_bounds("goal-group-header-no-goal")
            .expect("No goal header should be rendered");

        assert!(
            cx.debug_bounds("task-node-TASK-1").is_none(),
            "TASK-1 should be collapsed by default"
        );
        assert!(
            cx.debug_bounds("task-node-TASK-2").is_none(),
            "TASK-2 should be collapsed by default"
        );

        let goal_1_chevron_bounds = cx.debug_bounds("goal-chevron-GOAL-1").unwrap();
        cx.simulate_click(goal_1_chevron_bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert!(cx.debug_bounds("task-node-TASK-1").is_some());
        assert!(cx.debug_bounds("task-node-TASK-2").is_none());

        let no_goal_chevron_bounds = cx.debug_bounds("goal-chevron-no-goal").unwrap();
        cx.simulate_click(no_goal_chevron_bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert!(cx.debug_bounds("task-node-TASK-1").is_some());
        assert!(cx.debug_bounds("task-node-TASK-2").is_some());

        cx.simulate_click(goal_1_chevron_bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(cx.debug_bounds("task-node-TASK-1").is_none());
        assert!(cx.debug_bounds("task-node-TASK-2").is_some());
    }

    #[gpui::test]
    async fn test_agent_task_panel_two_line_rows_render(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;
        let provider = Arc::new(TestProvider::default());
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (_panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(store, WeakEntity::new_invalid(), project, file_system, cx)
        });
        cx.run_until_parked();

        cx.debug_bounds("goal-meta-GOAL-1")
            .expect("GOAL-1 line 2 meta should be rendered");
        assert!(cx.debug_bounds("task-meta-TASK-1").is_none());
        assert!(cx.debug_bounds("task-meta-TASK-2").is_none());

        let goal_1_chevron_bounds = cx.debug_bounds("goal-chevron-GOAL-1").unwrap();
        cx.simulate_click(goal_1_chevron_bounds.center(), gpui::Modifiers::default());
        let no_goal_chevron_bounds = cx.debug_bounds("goal-chevron-no-goal").unwrap();
        cx.simulate_click(no_goal_chevron_bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        cx.debug_bounds("task-meta-TASK-1")
            .expect("TASK-1 line 2 meta should be rendered after expanding GOAL-1");
        cx.debug_bounds("task-meta-TASK-2")
            .expect("TASK-2 line 2 meta should be rendered after expanding No goal");
    }

    #[test]
    fn test_format_task_and_goal_meta_lines() {
        let task_full = AgentTaskSummary {
            id: AgentTaskId::from("TASK-1"),
            parent_id: None,
            goal_id: Some("GOAL-1".to_string()),
            title: "Task 1".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: Some("agent_engineer".into()),
            model: Some("claude-3-7-sonnet".into()),
            session_id: None,
            metrics: None,
        };
        let meta = format_task_meta_line(&task_full);
        assert_eq!(meta, "TASK-1 · agent_engineer · claude-3-7-sonnet");

        let goal_meta = format_goal_meta_line(
            "GOAL-1",
            &AgentGoalStatus::Running,
            2,
            1,
            3,
            None,
        );
        assert_eq!(goal_meta, "GOAL-1 · 1 / 3 · P2");

        assert_eq!(
            format_goal_meta_line(
                "GOAL-1",
                &AgentGoalStatus::Running,
                1,
                0,
                1,
                None,
            ),
            "GOAL-1 · 0 / 1 · P1"
        );

        assert_eq!(
            format_task_meta_line(&task_full),
            "TASK-1 · agent_engineer · claude-3-7-sonnet"
        );
        assert_eq!(
            format_goal_meta_line("GOAL-1", &AgentGoalStatus::Completed, 1, 2, 2, None),
            "GOAL-1 · 2 / 2 · P1"
        );

        let sample_metrics = AgentTaskMetrics {
            duration_ms: Some(14_000),
            input_tokens: Some(5100),
            output_tokens: Some(1300),
            cache_read_tokens: Some(15100),
            cache_creation_tokens: None,
            total_tokens: Some(21500),
            cost_usd: Some(0.04),
            turns_count: Some(1),
            diff: None,
        };
        let mut task_with_metrics = task_full;
        task_with_metrics.metrics = Some(sample_metrics.clone());
        let task_meta_no_diff = format_task_meta_line(&task_with_metrics);
        assert_eq!(
            task_meta_no_diff,
            "TASK-1 · agent_engineer · claude-3-7-sonnet · 14s · 5.1k/1.3k/15.1k · 1 · $0.04"
        );
        assert!(!task_meta_no_diff.contains(" ·  · "));

        let goal_meta_no_diff = format_goal_meta_line(
            "GOAL-10",
            &AgentGoalStatus::Running,
            1,
            3,
            3,
            Some(&sample_metrics),
        );
        assert_eq!(
            goal_meta_no_diff,
            "GOAL-10 · 3 / 3 · P1 · 14s · 5.1k/1.3k/15.1k · 1 · $0.04"
        );
        assert!(!goal_meta_no_diff.contains(" ·  · "));

        let mut metrics_with_diff = sample_metrics;
        metrics_with_diff.diff = Some(AgentDiffShortstat {
            files_changed: 2,
            insertions: 15,
            deletions: 4,
        });
        task_with_metrics.metrics = Some(metrics_with_diff.clone());
        let task_meta_with_diff = format_task_meta_line(&task_with_metrics);
        assert_eq!(
            task_meta_with_diff,
            "TASK-1 · agent_engineer · claude-3-7-sonnet · 14s · 5.1k/1.3k/15.1k · 1 · +15 -4 · $0.04"
        );
        assert!(!task_meta_with_diff.contains(" ·  · "));

        let goal_meta_with_diff = format_goal_meta_line(
            "GOAL-10",
            &AgentGoalStatus::Running,
            1,
            3,
            3,
            Some(&metrics_with_diff),
        );
        assert_eq!(
            goal_meta_with_diff,
            "GOAL-10 · 3 / 3 · P1 · 14s · 5.1k/1.3k/15.1k · 1 · +15 -4 · $0.04"
        );
        assert!(!goal_meta_with_diff.contains(" ·  · "));
    }

    #[test]
    fn test_format_task_meta_line_no_goal_and_profile_fallback() {
        let task_no_goal = AgentTaskSummary {
            id: AgentTaskId::from("TASK-2"),
            parent_id: None,
            goal_id: None,
            title: "No Goal Task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: Some("code_mechanic".into()),
            model: Some("gemini-2.5-pro".into()),
            session_id: None,
            metrics: None,
        };
        assert_eq!(
            format_task_meta_line(&task_no_goal),
            "TASK-2 · code_mechanic · gemini-2.5-pro"
        );

        let task_assignee_fallback = AgentTaskSummary {
            id: AgentTaskId::from("TASK-3"),
            parent_id: None,
            goal_id: Some("GOAL-9".to_string()),
            title: "Fallback Task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: Some("backend_profile".into()),
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        assert_eq!(
            format_task_meta_line(&task_assignee_fallback),
            "TASK-3 · backend_profile"
        );

        let task_no_profile = AgentTaskSummary {
            id: AgentTaskId::from("TASK-4"),
            parent_id: None,
            goal_id: None,
            title: "Unassigned Task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        assert_eq!(
            format_task_meta_line(&task_no_profile),
            "TASK-4 · no profile"
        );

        let task_both = AgentTaskSummary {
            id: AgentTaskId::from("TASK-5"),
            parent_id: None,
            goal_id: None,
            title: "Both Profiles Task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: Some("fallback_assignee".into()),
            write_scopes: vec![],
            created_at: None,
            assigned_profile: Some("primary_profile".into()),
            model: None,
            session_id: None,
            metrics: None,
        };
        assert_eq!(
            format_task_meta_line(&task_both),
            "TASK-5 · primary_profile"
        );
    }

    #[gpui::test]
    async fn test_agent_task_panel_collapsed_tasks_toggle(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;

        let task_parent = AgentTaskSummary {
            id: AgentTaskId::from("TASK-PARENT"),
            parent_id: None,
            goal_id: Some("GOAL-1".to_string()),
            title: "Parent Task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: Some(10),
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        let task_child = AgentTaskSummary {
            id: AgentTaskId::from("TASK-CHILD"),
            parent_id: Some(AgentTaskId::from("TASK-PARENT")),
            goal_id: Some("GOAL-1".to_string()),
            title: "Child Task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: Some(20),
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        let provider = Arc::new(TestProvider {
            tasks: vec![task_parent, task_child],
            ..Default::default()
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(store, WeakEntity::new_invalid(), project, file_system, cx)
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert!(
                panel
                    .collapsed_tasks
                    .contains(&AgentTaskId::from("TASK-PARENT")),
                "tasks should start in collapsed_tasks by default"
            );
        });
        assert!(
            cx.debug_bounds("task-node-TASK-PARENT").is_none(),
            "goal is collapsed by default so parent task is hidden"
        );

        let goal_1_chevron_bounds = cx.debug_bounds("goal-chevron-GOAL-1").unwrap();
        cx.simulate_click(goal_1_chevron_bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert!(cx.debug_bounds("task-node-TASK-PARENT").is_some());
        assert!(
            cx.debug_bounds("task-node-TASK-CHILD").is_none(),
            "parent task is collapsed by default so child task is hidden"
        );

        let parent_chevron_bounds = cx.debug_bounds("task-chevron-TASK-PARENT").unwrap();
        cx.simulate_click(parent_chevron_bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert!(
                !panel
                    .collapsed_tasks
                    .contains(&AgentTaskId::from("TASK-PARENT"))
            );
        });
        assert!(cx.debug_bounds("task-node-TASK-CHILD").is_some());

        // Refresh store and verify user expansion is preserved across store refreshes
        panel.update(cx, |panel, cx| {
            panel
                .store
                .update(cx, |store, cx| store.refresh(cx))
                .detach();
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert!(
                !panel
                    .collapsed_tasks
                    .contains(&AgentTaskId::from("TASK-PARENT")),
                "expanded task should remain expanded across store refreshes"
            );
        });
        assert!(cx.debug_bounds("task-node-TASK-CHILD").is_some());

        cx.simulate_click(parent_chevron_bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert!(
                panel
                    .collapsed_tasks
                    .contains(&AgentTaskId::from("TASK-PARENT"))
            );
        });
        assert!(cx.debug_bounds("task-node-TASK-CHILD").is_none());
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

        panel.read_with(cx, |panel, _| {
            assert!(panel.category_filters.contains(&FilterCategory::Ready));
            assert!(panel.category_filters.contains(&FilterCategory::Done));
            assert!(!panel.category_filters.contains(&FilterCategory::Archived));
        });

        let filter_menu_btn = cx
            .debug_bounds("ICON-Filter")
            .expect("status filter menu button should be rendered");
        cx.simulate_click(filter_menu_btn.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        let archived_entry = cx
            .debug_bounds("MENU_ITEM-Archived")
            .expect("Archived entry should be present in status filter menu");
        cx.simulate_click(archived_entry.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert!(panel.category_filters.contains(&FilterCategory::Archived));
        });

        let archived_entry_again = cx
            .debug_bounds("MENU_ITEM-Archived")
            .expect("Archived entry should still be present in persistent menu");
        cx.simulate_click(archived_entry_again.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert!(!panel.category_filters.contains(&FilterCategory::Archived));
        });
    }

    #[gpui::test]
    async fn test_agent_task_panel_context_menu_actions(cx: &mut TestAppContext) {
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

        let calls = Arc::new(std::sync::Mutex::new(CallCounts::default()));
        let mut goals = TestProvider::default().goals;
        goals[1].session_id = Some("session-goal-2".to_string());
        let provider = Arc::new(TestProvider {
            calls: calls.clone(),
            goals,
            ..Default::default()
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(store, WeakEntity::new_invalid(), project, file_system, cx)
        });
        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| {
            panel.deploy_context_menu(
                ContextMenuTarget::Task(AgentTaskId::from("TASK-1")),
                Point::new(px(100.0), px(100.0)),
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("MENU_ITEM-Open Thread").is_none(),
            "Open Thread should not be present when no session is registered"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-View Thread").is_none(),
            "View Thread should not be present when no session is registered"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-View Thread as Markdown")
                .is_none(),
            "View Thread as Markdown should not be present when no session is registered"
        );

        let archive_item = cx
            .debug_bounds("MENU_ITEM-Archive")
            .expect("Archive menu item should be in context menu");
        cx.simulate_click(archive_item.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert_eq!(
            calls.lock().unwrap().archive_calls,
            vec![AgentTaskId::from("TASK-1")]
        );

        agent::task_worktree::register_task_session(
            &AgentTaskId::from("TASK-1"),
            "session-task-1".to_string(),
        );

        panel.update_in(cx, |panel, window, cx| {
            panel.deploy_context_menu(
                ContextMenuTarget::Task(AgentTaskId::from("TASK-1")),
                Point::new(px(100.0), px(100.0)),
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("MENU_ITEM-Open Thread").is_some(),
            "Open Thread should be present when a session is registered for the task"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-View Thread").is_some(),
            "View Thread should be present when a session is registered for the task"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-View Thread as Markdown")
                .is_some(),
            "View Thread as Markdown should be present when a session is registered for the task"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-Rename").is_some(),
            "Rename should be present in context menu"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-Complete").is_some(),
            "Complete should be present in context menu"
        );

        // Goal context menu without session_id (GOAL-1)
        panel.update_in(cx, |panel, window, cx| {
            panel.deploy_context_menu(
                ContextMenuTarget::Goal("GOAL-1".to_string()),
                Point::new(px(100.0), px(100.0)),
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("MENU_ITEM-Open Thread").is_none(),
            "Open Thread should not be present for goal when session_id is None"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-View Thread").is_none(),
            "View Thread should not be present for goal when session_id is None"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-View Thread as Markdown")
                .is_none(),
            "View Thread as Markdown should not be present for goal when session_id is None"
        );

        // Goal context menu with session_id (GOAL-2)
        panel.update_in(cx, |panel, window, cx| {
            panel.deploy_context_menu(
                ContextMenuTarget::Goal("GOAL-2".to_string()),
                Point::new(px(100.0), px(100.0)),
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("MENU_ITEM-Open Thread").is_some(),
            "Open Thread should be present for goal when session_id is Some"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-View Thread").is_some(),
            "View Thread should be present for goal when session_id is Some"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-View Thread as Markdown")
                .is_some(),
            "View Thread as Markdown should be present for goal when session_id is Some"
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

        assert!(cx.debug_bounds("orphan-worktrees-section").is_none());

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

        assert!(
            file_system
                .is_dir(std::path::Path::new("/fake/orphan-42"))
                .await
        );
        assert!(cx.debug_bounds("orphan-worktrees-section").is_some());
        assert!(cx.debug_bounds("orphan-worktree-ORPHAN-42").is_some());

        let delete_orphan_btn = cx
            .debug_bounds("delete-orphan-ORPHAN-42")
            .expect("delete orphan button should be rendered");
        cx.simulate_click(delete_orphan_btn.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert!(
            !file_system
                .is_dir(std::path::Path::new("/fake/orphan-42"))
                .await,
            "orphan worktree folder should be removed from disk"
        );

        assert!(cx.debug_bounds("orphan-worktree-ORPHAN-42").is_none());
        assert!(cx.debug_bounds("orphan-worktrees-section").is_none());
    }

    #[gpui::test]
    async fn test_agent_task_panel_worktree_status_error_banner(cx: &mut TestAppContext) {
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

        assert!(cx.debug_bounds("worktree_status_error_banner").is_none());

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

        panel_empty.read_with(cx, |panel, cx| {
            assert!(panel.worktree_status.read(cx).last_error().is_some());
        });

        assert!(
            cx.debug_bounds("worktree_status_error_banner").is_some(),
            "worktree_status_error_banner should be rendered when last_error is present"
        );
    }

    #[gpui::test]
    async fn test_agent_task_panel_cleanup_menu_item_gating(cx: &mut TestAppContext) {
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

        // 1. Goal with active task -> cleanup item not in context menu
        let provider = Arc::new(TestProvider {
            goals: vec![AgentGoalSummary {
                goal_id: "GOAL-1".to_string(),
                title: "Goal 1".to_string(),
                status: AgentGoalStatus::Running,
                priority: 1,
                tasks_total: 1,
                tasks_done: 0,
                created_at: Some(10),
                metrics: None,
                session_id: None,
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
                created_at: Some(10),
                assigned_profile: None,
                model: None,
                session_id: None,
                metrics: None,
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

        panel.update_in(cx, |panel, window, cx| {
            panel.deploy_context_menu(
                ContextMenuTarget::Goal("GOAL-1".to_string()),
                Point::new(px(100.0), px(100.0)),
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("MENU_ITEM-Cleanup Goal Branches").is_none(),
            "Cleanup menu item should not be present when goal has active tasks"
        );

        // 2. Goal with completed tasks and branch exists -> cleanup item is present and executes
        let completed_provider = Arc::new(TestProvider {
            goals: vec![AgentGoalSummary {
                goal_id: "GOAL-1".to_string(),
                title: "Goal 1".to_string(),
                status: AgentGoalStatus::Completed,
                priority: 1,
                tasks_total: 1,
                tasks_done: 1,
                created_at: Some(10),
                metrics: None,
                session_id: None,
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
                created_at: Some(10),
                assigned_profile: None,
                model: None,
                session_id: None,
                metrics: None,
            }],
            ..Default::default()
        });
        panel.update(cx, |panel, cx| {
            panel.store.update(cx, |store, cx| {
                store.set_provider(completed_provider, cx);
            });
            panel.context_menu = None;
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

        panel.update_in(cx, |panel, window, cx| {
            panel.deploy_context_menu(
                ContextMenuTarget::Goal("GOAL-1".to_string()),
                Point::new(px(100.0), px(100.0)),
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let cleanup_item = cx
            .debug_bounds("MENU_ITEM-Cleanup Goal Branches")
            .expect("Cleanup menu item should be present when tasks are done and branch exists");
        cx.simulate_click(cleanup_item.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert!(
                panel.goal_cleanup_result.is_some(),
                "goal cleanup result should be set after clicking menu item"
            );
        });
    }

    #[gpui::test]
    async fn test_agent_task_panel_unknown_status_filter_and_display(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;

        let custom_status = AgentTaskStatus::Other(gpui::SharedString::from("custom_status"));
        let provider = Arc::new(TestProvider {
            tasks: vec![AgentTaskSummary {
                id: AgentTaskId::from("TASK-CUSTOM"),
                parent_id: None,
                goal_id: None,
                title: "Custom Status Task".to_string(),
                status: custom_status.clone(),
                attempt: 1,
                assignee: None,
                write_scopes: vec![],
                created_at: Some(10),
                assigned_profile: None,
                model: None,
                session_id: None,
                metrics: None,
            }],
            ..Default::default()
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(store, WeakEntity::new_invalid(), project, file_system, cx)
        });
        cx.run_until_parked();

        let no_goal_chevron = cx
            .debug_bounds("goal-chevron-no-goal")
            .expect("no-goal chevron should be rendered");
        cx.simulate_click(no_goal_chevron.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        cx.debug_bounds("task-node-TASK-CUSTOM")
            .expect("TASK-CUSTOM should be rendered");

        let filter_menu_btn = cx
            .debug_bounds("ICON-Filter")
            .expect("status filter menu button should be rendered");
        cx.simulate_click(filter_menu_btn.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        let custom_entry = cx
            .debug_bounds("MENU_ITEM-custom_status")
            .expect("custom_status entry should be in filter menu");

        cx.simulate_click(custom_entry.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert!(!panel.other_status_filters.contains(&custom_status));
        });

        assert!(cx.debug_bounds("task-node-TASK-CUSTOM").is_none());
    }

    #[gpui::test]
    async fn test_agent_task_panel_context_menu_worktree_actions(cx: &mut TestAppContext) {
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
        let (workspace, _) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let provider = Arc::new(TestProvider::default());
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(
                store,
                workspace.downgrade(),
                project.clone(),
                file_system.clone(),
                cx,
            )
        });
        cx.run_until_parked();

        panel.update(cx, |panel, cx| {
            panel.worktree_status.update(cx, |status, cx| {
                let mut snap = TaskGitSnapshot::default();
                snap.tasks.insert(
                    AgentTaskId::from("TASK-1"),
                    TaskGitState {
                        worktree_path: std::path::PathBuf::from("/fake/task-1"),
                        exists_on_disk: true,
                        branch: Some("agent-task/TASK-1".to_string()),
                        branch_exists: true,
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

        panel.update_in(cx, |panel, window, cx| {
            panel.deploy_context_menu(
                ContextMenuTarget::Task(AgentTaskId::from("TASK-1")),
                Point::new(px(100.0), px(100.0)),
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        assert!(cx.debug_bounds("MENU_ITEM-Go to Worktree").is_some());
        assert!(cx.debug_bounds("MENU_ITEM-Open Diff").is_some());

        let remove_item = cx
            .debug_bounds("MENU_ITEM-Delete Worktree")
            .expect("Delete Worktree should be in context menu when worktree exists");
        cx.simulate_click(remove_item.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert!(
            workspace.read_with(cx, |workspace, _cx| {
                workspace.has_notification(&workspace::notifications::NotificationId::unique::<
                    WorktreeDeletedToast,
                >())
            }),
            "Success toast should be shown on worktree deletion"
        );
    }

    #[gpui::test]
    async fn test_agent_task_panel_context_menu_delete_worktree_error_toast(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;
        let (workspace, _) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let provider = Arc::new(TestProvider::default());
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(
                store,
                workspace.downgrade(),
                project.clone(),
                file_system.clone(),
                cx,
            )
        });
        cx.run_until_parked();

        panel.update(cx, |panel, cx| {
            panel.worktree_status.update(cx, |status, cx| {
                let mut snap = TaskGitSnapshot::default();
                snap.tasks.insert(
                    AgentTaskId::from("TASK-1"),
                    TaskGitState {
                        worktree_path: std::path::PathBuf::from("/fake/task-1"),
                        exists_on_disk: true,
                        branch: Some("agent-task/TASK-1".to_string()),
                        branch_exists: true,
                    },
                );
                status.set_snapshot_for_test(snap, cx);
            });
        });
        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| {
            panel.deploy_context_menu(
                ContextMenuTarget::Task(AgentTaskId::from("TASK-1")),
                Point::new(px(100.0), px(100.0)),
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let remove_item = cx
            .debug_bounds("MENU_ITEM-Delete Worktree")
            .expect("Delete Worktree should be in context menu when worktree exists");
        cx.simulate_click(remove_item.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert!(
            workspace.read_with(cx, |workspace, _cx| {
                workspace.has_notification(&workspace::notifications::NotificationId::unique::<
                    TaskOperationErrorToast,
                >())
            }),
            "Error toast should be shown when worktree deletion fails"
        );
    }

    #[test]
    fn test_agent_task_panel_renders_goal_markdown_and_conflict() {
        let task_id = AgentTaskId::from("TASK-GOAL-ROW");
        let goal_id = "GOAL-UI-1";

        let goal_summary = AgentGoalSummary {
            goal_id: goal_id.to_string(),
            title: "Goal UI 1".to_string(),
            status: AgentGoalStatus::Running,
            priority: 1,
            tasks_total: 2,
            tasks_done: 0,
            created_at: Some(10),
            metrics: None,
            session_id: None,
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
                created_at: Some(10),
                assigned_profile: None,
                model: None,
                session_id: None,
                metrics: None,
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

    #[test]
    fn test_task_tree_branch_guides_under_goal_and_no_goal() {
        let tasks = vec![
            AgentTaskSummary {
                id: AgentTaskId::from("TASK-ROOT-1"),
                parent_id: None,
                goal_id: Some("GOAL-1".to_string()),
                title: "Root 1".to_string(),
                status: AgentTaskStatus::Ready,
                attempt: 1,
                assignee: None,
                write_scopes: vec![],
                created_at: Some(10),
                assigned_profile: None,
                model: None,
                session_id: None,
                metrics: None,
            },
            AgentTaskSummary {
                id: AgentTaskId::from("TASK-CHILD-1"),
                parent_id: Some(AgentTaskId::from("TASK-ROOT-1")),
                goal_id: Some("GOAL-1".to_string()),
                title: "Child 1".to_string(),
                status: AgentTaskStatus::Ready,
                attempt: 1,
                assignee: None,
                write_scopes: vec![],
                created_at: Some(20),
                assigned_profile: None,
                model: None,
                session_id: None,
                metrics: None,
            },
            AgentTaskSummary {
                id: AgentTaskId::from("TASK-GRANDCHILD-1"),
                parent_id: Some(AgentTaskId::from("TASK-CHILD-1")),
                goal_id: Some("GOAL-1".to_string()),
                title: "Grandchild 1".to_string(),
                status: AgentTaskStatus::Ready,
                attempt: 1,
                assignee: None,
                write_scopes: vec![],
                created_at: Some(30),
                assigned_profile: None,
                model: None,
                session_id: None,
                metrics: None,
            },
            AgentTaskSummary {
                id: AgentTaskId::from("TASK-CHILD-2"),
                parent_id: Some(AgentTaskId::from("TASK-ROOT-1")),
                goal_id: Some("GOAL-1".to_string()),
                title: "Child 2".to_string(),
                status: AgentTaskStatus::Ready,
                attempt: 1,
                assignee: None,
                write_scopes: vec![],
                created_at: Some(40),
                assigned_profile: None,
                model: None,
                session_id: None,
                metrics: None,
            },
            AgentTaskSummary {
                id: AgentTaskId::from("TASK-ROOT-2"),
                parent_id: None,
                goal_id: Some("GOAL-1".to_string()),
                title: "Root 2".to_string(),
                status: AgentTaskStatus::Ready,
                attempt: 1,
                assignee: None,
                write_scopes: vec![],
                created_at: Some(50),
                assigned_profile: None,
                model: None,
                session_id: None,
                metrics: None,
            },
            AgentTaskSummary {
                id: AgentTaskId::from("TASK-CHILD-3"),
                parent_id: Some(AgentTaskId::from("TASK-ROOT-2")),
                goal_id: Some("GOAL-1".to_string()),
                title: "Child 3".to_string(),
                status: AgentTaskStatus::Ready,
                attempt: 1,
                assignee: None,
                write_scopes: vec![],
                created_at: Some(60),
                assigned_profile: None,
                model: None,
                session_id: None,
                metrics: None,
            },
        ];

        let collapsed = HashSet::new();

        // Under goal: roots receive ├── / └── and subtasks inherit continuation prefix
        let under_goal_rows = build_task_tree_rows(&tasks, &collapsed, true);
        let under_goal_info: Vec<(&str, &str, &str)> = under_goal_rows
            .iter()
            .map(|r| {
                (
                    r.task.id.0.as_ref(),
                    r.prefix.as_str(),
                    r.continuation_prefix.as_str(),
                )
            })
            .collect();

        assert_eq!(
            under_goal_info,
            vec![
                ("TASK-ROOT-1", "├── ", "│   "),
                ("TASK-CHILD-1", "│   ├── ", "│   │   "),
                ("TASK-GRANDCHILD-1", "│   │   └── ", "│   │       "),
                ("TASK-CHILD-2", "│   └── ", "│       "),
                ("TASK-ROOT-2", "└── ", "    "),
                ("TASK-CHILD-3", "    └── ", "        "),
            ]
        );

        // No goal: roots have "" and subtasks receive ├── / └──
        let no_goal_rows = build_task_tree_rows(&tasks, &collapsed, false);
        let no_goal_info: Vec<(&str, &str, &str)> = no_goal_rows
            .iter()
            .map(|r| {
                (
                    r.task.id.0.as_ref(),
                    r.prefix.as_str(),
                    r.continuation_prefix.as_str(),
                )
            })
            .collect();

        assert_eq!(
            no_goal_info,
            vec![
                ("TASK-ROOT-1", "", ""),
                ("TASK-CHILD-1", "├── ", "│   "),
                ("TASK-GRANDCHILD-1", "│   └── ", "│       "),
                ("TASK-CHILD-2", "└── ", "    "),
                ("TASK-ROOT-2", "", ""),
                ("TASK-CHILD-3", "└── ", "    "),
            ]
        );
    }

    #[test]
    fn test_status_and_priority_colors_and_labels() {
        assert_eq!(priority_color(0), Color::Error);
        assert_eq!(priority_color(1), Color::Error);
        assert_eq!(priority_color(2), Color::Warning);
        assert_eq!(priority_color(3), Color::Muted);
        assert_eq!(priority_color(4), Color::Muted);
    }

    #[gpui::test]
    async fn test_click_separation_chevron_toggles_and_row_card_selects(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;

        let task_parent = AgentTaskSummary {
            id: AgentTaskId::from("TASK-P1"),
            parent_id: None,
            goal_id: Some("GOAL-1".to_string()),
            title: "Parent Task 1".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: Some(10),
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        let task_child = AgentTaskSummary {
            id: AgentTaskId::from("TASK-C1"),
            parent_id: Some(AgentTaskId::from("TASK-P1")),
            goal_id: Some("GOAL-1".to_string()),
            title: "Child Task 1".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: Some(20),
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        let provider = Arc::new(TestProvider {
            tasks: vec![task_parent, task_child],
            ..Default::default()
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(store, WeakEntity::new_invalid(), project, file_system, cx)
        });
        cx.run_until_parked();

        let goal_chevron = cx.debug_bounds("goal-chevron-GOAL-1").unwrap();
        cx.simulate_click(goal_chevron.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert!(cx.debug_bounds("task-node-TASK-P1").is_some());
        assert!(cx.debug_bounds("task-node-TASK-C1").is_none());

        let chevron_bounds = cx.debug_bounds("task-chevron-TASK-P1").unwrap();
        cx.simulate_click(chevron_bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert!(cx.debug_bounds("task-node-TASK-P1").is_some());
        assert!(cx.debug_bounds("task-node-TASK-C1").is_some());

        // Clicking the row card selects the task, does NOT toggle collapse
        let parent_card_bounds = cx.debug_bounds("task-node-TASK-P1").unwrap();
        cx.simulate_click(parent_card_bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.selected_task_id, Some(AgentTaskId::from("TASK-P1")));
            assert!(
                !panel
                    .collapsed_tasks
                    .contains(&AgentTaskId::from("TASK-P1"))
            );
        });
        // Child still visible
        assert!(cx.debug_bounds("task-node-TASK-C1").is_some());

        // Clicking the chevron toggles collapse
        let chevron_bounds = cx.debug_bounds("task-chevron-TASK-P1").unwrap();
        cx.simulate_click(chevron_bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert!(
                panel
                    .collapsed_tasks
                    .contains(&AgentTaskId::from("TASK-P1"))
            );
        });
        // Child is now collapsed
        assert!(cx.debug_bounds("task-node-TASK-C1").is_none());
    }

    #[test]
    fn test_markdown_preview_metrics() {
        let sample_metrics = AgentTaskMetrics {
            duration_ms: Some(14_000),
            input_tokens: Some(5100),
            output_tokens: Some(1300),
            cache_read_tokens: Some(15000),
            cache_creation_tokens: Some(100),
            total_tokens: Some(21500),
            cost_usd: Some(0.04),
            turns_count: Some(1),
            diff: None,
        };
        let goal = AgentGoalSummary {
            goal_id: "GOAL-10".to_string(),
            title: "Goal 10".to_string(),
            status: AgentGoalStatus::Completed,
            priority: 1,
            tasks_total: 3,
            tasks_done: 3,
            created_at: Some(10),
            metrics: Some(sample_metrics.clone()),
            session_id: None,
        };
        let goal_md = render_goal_markdown(&goal, None);
        assert!(goal_md.contains("## Metrics"));
        assert!(goal_md.contains("- **Duration:** 14s"));
        assert!(goal_md.contains("- **Tokens:** 5100 in / 1300 out / 15100 cache"));
        assert!(goal_md.contains("- **Turns:** 1"));
        assert!(goal_md.contains("- **Cost:** $0.04"));

        let detail = AgentTaskDetail {
            summary: AgentTaskSummary {
                id: AgentTaskId::from("TASK-1"),
                parent_id: None,
                goal_id: Some("GOAL-10".to_string()),
                title: "Task 1".to_string(),
                status: AgentTaskStatus::Completed,
                attempt: 1,
                assignee: None,
                write_scopes: vec![],
                created_at: Some(10),
                assigned_profile: Some("agent_engineer".into()),
                model: Some("glm-5.3".into()),
                session_id: None,
                metrics: Some(sample_metrics.clone()),
            },
            description: "Desc".to_string(),
            acceptance_criteria: vec![],
            events_tail: vec![],
        };
        let task_md = render_task_markdown(&detail, &[], Some(&goal), None);
        assert!(task_md.contains("- **Profile:** agent_engineer"));
        assert!(task_md.contains("- **Model:** glm-5.3"));
        assert!(task_md.contains("## Metrics"));
        assert!(task_md.contains("- **Duration:** 14s"));
        assert!(task_md.contains("- **Tokens:** 5100 in / 1300 out / 15100 cache"));
        assert!(task_md.contains("- **Turns:** 1"));
        assert!(task_md.contains("- **Cost:** $0.04"));
        assert!(!goal_md.contains("- **Diff:**"));
        assert!(!task_md.contains("- **Diff:**"));

        let mut metrics_with_diff = sample_metrics;
        metrics_with_diff.diff = Some(AgentDiffShortstat {
            files_changed: 3,
            insertions: 42,
            deletions: 7,
        });

        let mut goal_with_diff = goal;
        goal_with_diff.metrics = Some(metrics_with_diff.clone());
        let goal_with_diff_md = render_goal_markdown(&goal_with_diff, None);
        assert!(goal_with_diff_md.contains("- **Diff:** 3 file(s) changed (+42, -7)"));

        let mut detail_with_diff = detail;
        detail_with_diff.summary.metrics = Some(metrics_with_diff);
        let task_with_diff_md =
            render_task_markdown(&detail_with_diff, &[], Some(&goal_with_diff), None);
        assert!(task_with_diff_md.contains("- **Diff:** 3 file(s) changed (+42, -7)"));
    }

    #[gpui::test]
    async fn test_agent_task_panel_zoom_and_flex_properties(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;
        let provider = Arc::new(TestProvider::default());
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(store, WeakEntity::new_invalid(), project, file_system, cx)
        });
        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| {
            assert!(!panel.is_zoomed(window, cx));
            panel.set_zoomed(true, window, cx);
            assert!(panel.is_zoomed(window, cx));
            panel.set_zoomed(false, window, cx);
            assert!(!panel.is_zoomed(window, cx));

            assert!(panel.supports_flexible_size());
            assert!(!panel.has_flexible_size(window, cx));

            assert_eq!(panel.icon(window, cx), Some(IconName::ListTodo));
            assert!(panel.hide_button_setting(cx).is_some());
        });
    }

    #[gpui::test]
    async fn test_agent_task_panel_inline_renaming(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;

        let calls = Arc::new(std::sync::Mutex::new(CallCounts::default()));
        let provider = Arc::new(TestProvider {
            calls: calls.clone(),
            ..Default::default()
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(store, WeakEntity::new_invalid(), project, file_system, cx)
        });
        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| {
            panel.start_renaming(
                RenamingTarget::Task(AgentTaskId::from("TASK-1")),
                "Initial Title".to_string(),
                window,
                cx,
            );
            assert!(panel.editing_item.is_some());

            panel
                .editing_item
                .as_ref()
                .unwrap()
                .editor
                .update(cx, |editor, cx| {
                    editor.set_text("Updated Task Title", window, cx);
                });

            panel.confirm_renaming(window, cx);
            assert!(panel.editing_item.is_none());
        });
        cx.run_until_parked();

        assert_eq!(
            calls.lock().unwrap().update_task_title_calls,
            vec![(
                AgentTaskId::from("TASK-1"),
                "Updated Task Title".to_string()
            )]
        );

        panel.update_in(cx, |panel, window, cx| {
            panel.start_renaming(
                RenamingTarget::Task(AgentTaskId::from("TASK-1")),
                "Initial Title".to_string(),
                window,
                cx,
            );
            assert!(panel.editing_item.is_some());
            panel.cancel_renaming(window, cx);
            assert!(panel.editing_item.is_none());
        });
        cx.run_until_parked();
        assert_eq!(calls.lock().unwrap().update_task_title_calls.len(), 1);

        panel.update_in(cx, |panel, window, cx| {
            panel.start_renaming(
                RenamingTarget::Goal("GOAL-1".to_string()),
                "Goal 1".to_string(),
                window,
                cx,
            );
            assert!(panel.editing_item.is_some());

            panel
                .editing_item
                .as_ref()
                .unwrap()
                .editor
                .update(cx, |editor, cx| {
                    editor.set_text("Updated Goal Title", window, cx);
                });

            panel.confirm_renaming(window, cx);
            assert!(panel.editing_item.is_none());
        });
        cx.run_until_parked();

        assert_eq!(
            calls.lock().unwrap().update_goal_title_calls,
            vec![("GOAL-1".to_string(), "Updated Goal Title".to_string())]
        );
    }

    #[gpui::test]
    async fn test_agent_task_panel_error_toast(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));

        let provider = Arc::new(TestProvider::default());
        let store = cx.update(|_window, cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let panel = cx
            .new(|cx| AgentTaskPanel::new(store, workspace.downgrade(), project, file_system, cx));

        panel.update_in(cx, |panel, window, cx| {
            let failed_task = Task::ready(Err(anyhow::anyhow!("Invalid status transition")));
            panel.spawn_with_error_toast(failed_task, window, cx);
        });
        cx.run_until_parked();

        assert!(
            workspace.read_with(cx, |workspace, _cx| {
                workspace.has_notification(&workspace::notifications::NotificationId::unique::<
                    TaskOperationErrorToast,
                >())
            }),
            "Error toast should be shown on operation failure"
        );

        // Advance executor past the 5-second autohide threshold; error toast must remain persistent.
        cx.background_executor
            .advance_clock(std::time::Duration::from_secs(10));
        cx.run_until_parked();

        assert!(
            workspace.read_with(cx, |workspace, _cx| {
                workspace.has_notification(&workspace::notifications::NotificationId::unique::<
                    TaskOperationErrorToast,
                >())
            }),
            "Error toast must not autohide after 10s"
        );
    }

    #[test]
    fn test_format_task_panel_error_humanization_and_prefix() {
        let err = anyhow::anyhow!("sqlite error: database is locked");
        let formatted = AgentTaskPanel::format_task_panel_error(Some("Archive Goal"), &err);
        assert_eq!(
            formatted,
            "Task Panel: Archive Goal: Database is locked by another process. Please wait and try again."
        );
        assert!(!formatted.contains("TGS: "));

        let err = anyhow::anyhow!("fatal: Unable to create '.git/index.lock': File exists");
        let formatted = AgentTaskPanel::format_task_panel_error(Some("Restore Worktree"), &err);
        assert_eq!(
            formatted,
            "Task Panel: Restore Worktree: Git index or file is locked by another process. Please wait and try again."
        );
        assert!(!formatted.contains("TGS: "));

        let err = anyhow::anyhow!("fatal: branch 'agent-task/TASK-99' not found");
        let formatted = AgentTaskPanel::format_task_panel_error(Some("Go to Branch"), &err);
        assert_eq!(
            formatted,
            "Task Panel: Go to Branch: Git branch not found: fatal: branch 'agent-task/TASK-99' not found"
        );
        assert!(!formatted.contains("TGS: "));

        let err = anyhow::anyhow!("operation timeout");
        let formatted = AgentTaskPanel::format_task_panel_error(None, &err);
        assert_eq!(formatted, "Task Panel: operation timeout");
        assert!(!formatted.contains("TGS: "));
    }

    #[test]
    fn test_open_thread_error_toast_strings() {
        let task_id = AgentTaskId::from("TASK-123");
        let branch_name = "agent-task/TASK-123";
        let missing_branch_message = format!(
            "Task Panel: Open Thread: Cannot open thread for task {task_id}: branch '{branch_name}' was not found in git repository; worktree cannot be restored."
        );
        assert_eq!(
            missing_branch_message,
            "Task Panel: Open Thread: Cannot open thread for task TASK-123: branch 'agent-task/TASK-123' was not found in git repository; worktree cannot be restored."
        );

        let err = anyhow::anyhow!("fatal: branch 'agent-task/TASK-123' not found");
        let humanized = AgentTaskPanel::humanize_error(&err);
        let restore_error_message = format!(
            "Task Panel: Open Thread: Failed to restore worktree for task {task_id}: {humanized}"
        );
        assert_eq!(
            restore_error_message,
            "Task Panel: Open Thread: Failed to restore worktree for task TASK-123: Git branch not found: fatal: branch 'agent-task/TASK-123' not found"
        );
    }

    #[gpui::test]
    async fn test_open_thread_missing_branch_shows_persistent_error_toast_immediately(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;
        let (workspace, _) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));

        let task_id = AgentTaskId::from("TASK-NO-BRANCH");
        let provider = Arc::new(TestProvider {
            tasks: vec![AgentTaskSummary {
                id: task_id.clone(),
                title: "Task without branch".to_string(),
                status: AgentTaskStatus::Ready,
                ..Default::default()
            }],
            ..Default::default()
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(
                store,
                workspace.downgrade(),
                project.clone(),
                file_system.clone(),
                cx,
            )
        });
        cx.run_until_parked();

        agent::task_worktree::register_task_session(&task_id, "session-no-branch".to_string());

        panel.update(cx, |panel, cx| {
            panel.worktree_status.update(cx, |status, cx| {
                let mut snap = TaskGitSnapshot::default();
                snap.tasks.insert(
                    task_id.clone(),
                    TaskGitState {
                        worktree_path: std::path::PathBuf::from("/fake/task-no-branch"),
                        exists_on_disk: false,
                        branch: Some("agent-task/TASK-NO-BRANCH".to_string()),
                        branch_exists: false,
                    },
                );
                status.set_snapshot_for_test(snap, cx);
            });
        });
        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| {
            panel.deploy_context_menu(
                ContextMenuTarget::Task(task_id.clone()),
                Point::new(px(100.0), px(100.0)),
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let open_thread_item = cx
            .debug_bounds("MENU_ITEM-Open Thread")
            .expect("Open Thread should be present in context menu");
        cx.simulate_click(open_thread_item.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert!(
            workspace.read_with(cx, |workspace, _cx| {
                workspace.has_notification(&workspace::notifications::NotificationId::unique::<
                    TaskOperationErrorToast,
                >())
            }),
            "Persistent error toast should be shown immediately when branch is missing"
        );

        cx.background_executor
            .advance_clock(std::time::Duration::from_secs(10));
        cx.run_until_parked();

        assert!(
            workspace.read_with(cx, |workspace, _cx| {
                workspace.has_notification(&workspace::notifications::NotificationId::unique::<
                    TaskOperationErrorToast,
                >())
            }),
            "Missing branch error toast must remain persistent"
        );
    }

    #[gpui::test]
    async fn test_open_thread_existing_branch_restores_worktree(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            serde_json::json!({
                ".git": {
                    "HEAD": "ref: refs/heads/main\n"
                }
            }),
        )
        .await;

        let dot_git = std::path::Path::new("/root/.git");
        fs.with_git_state(dot_git, true, |state| {
            state
                .refs
                .insert("refs/heads/main".into(), "main-sha".into());
            state.refs.insert("HEAD".into(), "main-sha".into());
            state.branches.insert("main".into());
            state.refs.insert(
                "refs/heads/agent-task/TASK-RESTORE".into(),
                "main-sha".into(),
            );
            state.branches.insert("agent-task/TASK-RESTORE".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [std::path::Path::new("/root")], cx).await;
        let (workspace, _) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));

        let task_id = AgentTaskId::from("TASK-RESTORE");
        let provider = Arc::new(TestProvider {
            tasks: vec![AgentTaskSummary {
                id: task_id.clone(),
                title: "Task with branch".to_string(),
                status: AgentTaskStatus::Ready,
                ..Default::default()
            }],
            ..Default::default()
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(
                store,
                workspace.downgrade(),
                project.clone(),
                fs.clone(),
                cx,
            )
        });
        cx.run_until_parked();

        agent::task_worktree::register_task_session(&task_id, "session-restore".to_string());

        panel.update(cx, |panel, cx| {
            panel.worktree_status.update(cx, |status, cx| {
                let mut snap = TaskGitSnapshot::default();
                snap.tasks.insert(
                    task_id.clone(),
                    TaskGitState {
                        worktree_path: std::path::PathBuf::from(
                            "/root/.zed/worktrees/agent-task-TASK-RESTORE",
                        ),
                        exists_on_disk: false,
                        branch: Some("agent-task/TASK-RESTORE".to_string()),
                        branch_exists: true,
                    },
                );
                status.set_snapshot_for_test(snap, cx);
            });
        });
        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| {
            panel.deploy_context_menu(
                ContextMenuTarget::Task(task_id.clone()),
                Point::new(px(100.0), px(100.0)),
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let open_thread_item = cx
            .debug_bounds("MENU_ITEM-Open Thread")
            .expect("Open Thread should be present in context menu");
        cx.simulate_click(open_thread_item.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert!(
            !workspace.read_with(cx, |workspace, _cx| {
                workspace.has_notification(&workspace::notifications::NotificationId::unique::<
                    TaskOperationErrorToast,
                >())
            }),
            "No error toast should be shown when worktree is restored successfully"
        );
    }

    #[gpui::test]
    async fn test_open_thread_exists_on_disk(cx: &mut TestAppContext) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;
        let (workspace, _) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));

        let task_id = AgentTaskId::from("TASK-DISK");
        let provider = Arc::new(TestProvider {
            tasks: vec![AgentTaskSummary {
                id: task_id.clone(),
                title: "Task on disk".to_string(),
                status: AgentTaskStatus::Ready,
                ..Default::default()
            }],
            ..Default::default()
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(
                store,
                workspace.downgrade(),
                project.clone(),
                file_system.clone(),
                cx,
            )
        });
        cx.run_until_parked();

        agent::task_worktree::register_task_session(&task_id, "session-disk".to_string());

        panel.update(cx, |panel, cx| {
            panel.worktree_status.update(cx, |status, cx| {
                let mut snap = TaskGitSnapshot::default();
                snap.tasks.insert(
                    task_id.clone(),
                    TaskGitState {
                        worktree_path: std::path::PathBuf::from("/fake/task-disk"),
                        exists_on_disk: true,
                        branch: Some("agent-task/TASK-DISK".to_string()),
                        branch_exists: true,
                    },
                );
                status.set_snapshot_for_test(snap, cx);
            });
        });
        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| {
            panel.deploy_context_menu(
                ContextMenuTarget::Task(task_id.clone()),
                Point::new(px(100.0), px(100.0)),
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let open_thread_item = cx
            .debug_bounds("MENU_ITEM-Open Thread")
            .expect("Open Thread should be present in context menu");
        cx.simulate_click(open_thread_item.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert!(
            !workspace.read_with(cx, |workspace, _cx| {
                workspace.has_notification(&workspace::notifications::NotificationId::unique::<
                    TaskOperationErrorToast,
                >())
            }),
            "No error toast when opening existing thread on disk"
        );
    }

    #[gpui::test]
    async fn test_agent_task_panel_worktree_icon_and_goal_turns_and_duration(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let file_system = FakeFs::new(cx.executor());
        let project = Project::test(file_system.clone(), [], cx).await;

        let sample_metrics = AgentTaskMetrics {
            duration_ms: Some(15_000),
            input_tokens: Some(1000),
            output_tokens: Some(500),
            cache_read_tokens: Some(2000),
            cache_creation_tokens: None,
            total_tokens: Some(3500),
            cost_usd: Some(0.05),
            turns_count: Some(7),
            diff: Some(AgentDiffShortstat {
                files_changed: 2,
                insertions: 15,
                deletions: 4,
            }),
        };

        // Check render_metrics_elements directly: duration is omitted, turns controlled by flag
        let goal_elements = render_metrics_elements(
            (ElementId::from("diff-stat"), "goal-1").into(),
            Some(&sample_metrics),
            false,
        );
        let task_elements = render_metrics_elements(
            (ElementId::from("diff-stat"), "task-1").into(),
            Some(&sample_metrics),
            true,
        );
        assert_eq!(
            goal_elements.len(),
            3,
            "Goal elements should have tokens, diff, and cost, but no duration or turns"
        );
        assert_eq!(
            task_elements.len(),
            4,
            "Task elements should have tokens, turns, diff, and cost, but no duration"
        );

        let mut sample_metrics_no_diff = sample_metrics.clone();
        sample_metrics_no_diff.diff = None;
        assert_eq!(
            render_metrics_elements(
                (ElementId::from("diff-stat"), "goal-no-diff").into(),
                Some(&sample_metrics_no_diff),
                false,
            )
            .len(),
            2,
        );
        assert_eq!(
            render_metrics_elements(
                (ElementId::from("diff-stat"), "task-no-diff").into(),
                Some(&sample_metrics_no_diff),
                true,
            )
            .len(),
            3,
        );

        let mut provider_inner = TestProvider::default();
        provider_inner.goals = vec![AgentGoalSummary {
            goal_id: "GOAL-1".to_string(),
            title: "Goal 1".to_string(),
            status: AgentGoalStatus::Running,
            priority: 1,
            tasks_total: 1,
            tasks_done: 0,
            created_at: Some(10),
            metrics: Some(sample_metrics.clone()),
            session_id: None,
        }];
        provider_inner.tasks = vec![AgentTaskSummary {
            id: AgentTaskId::from("TASK-1"),
            parent_id: None,
            goal_id: Some("GOAL-1".to_string()),
            title: "Task 1".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: Some(10),
            assigned_profile: Some("agent_engineer".into()),
            model: Some("test-model".into()),
            session_id: None,
            metrics: Some(sample_metrics),
        }];
        let provider = Arc::new(provider_inner);
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

        // Expand GOAL-1
        let goal_chevron = cx.debug_bounds("goal-chevron-GOAL-1").unwrap();
        cx.simulate_click(goal_chevron.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        // Goal meta and task meta are rendered
        cx.debug_bounds("goal-meta-GOAL-1")
            .expect("goal-meta should be rendered");
        cx.debug_bounds("task-meta-TASK-1")
            .expect("task-meta should be rendered");

        // Initially exists_on_disk is false: no worktree icon
        assert!(
            cx.debug_bounds("task-worktree-icon-TASK-1").is_none(),
            "Worktree icon should not be present when exists_on_disk is false"
        );

        // Update snapshot to exists_on_disk = true
        panel.update(cx, |panel, cx| {
            panel.worktree_status.update(cx, |status, cx| {
                let mut snap = TaskGitSnapshot::default();
                snap.tasks.insert(
                    AgentTaskId::from("TASK-1"),
                    TaskGitState {
                        worktree_path: std::path::PathBuf::from("/fake/task-1"),
                        exists_on_disk: true,
                        branch: Some("agent-task/TASK-1".to_string()),
                        branch_exists: true,
                    },
                );
                status.set_snapshot_for_test(snap, cx);
            });
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("task-worktree-icon-TASK-1").is_some(),
            "Worktree icon should be present when exists_on_disk is true"
        );

        let icon_bounds = cx
            .debug_bounds("task-worktree-icon-TASK-1")
            .expect("worktree icon should be present");
        let meta_bounds = cx
            .debug_bounds("task-meta-TASK-1")
            .expect("task-meta should be present");
        assert!(
            icon_bounds.top() >= meta_bounds.top() && icon_bounds.bottom() <= meta_bounds.bottom(),
            "Worktree icon should be in task-meta row"
        );

        // Update snapshot to exists_on_disk = false
        panel.update(cx, |panel, cx| {
            panel.worktree_status.update(cx, |status, cx| {
                let mut snap = TaskGitSnapshot::default();
                snap.tasks.insert(
                    AgentTaskId::from("TASK-1"),
                    TaskGitState {
                        worktree_path: std::path::PathBuf::from("/fake/task-1"),
                        exists_on_disk: false,
                        branch: Some("agent-task/TASK-1".to_string()),
                        branch_exists: true,
                    },
                );
                status.set_snapshot_for_test(snap, cx);
            });
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("task-worktree-icon-TASK-1").is_none(),
            "Worktree icon should disappear when exists_on_disk becomes false"
        );
    }
}
