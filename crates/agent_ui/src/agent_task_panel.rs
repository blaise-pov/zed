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

use std::collections::HashSet;
use std::sync::Arc;

use agent::{
    AgentTaskArtifact, AgentTaskDetail, AgentTaskId, AgentTaskStatus, AgentTaskStore,
    AgentTaskSummary,
};
use agent_client_protocol::schema::v1 as acp;
use agent_settings::AgentSettings;
use fs::Fs;
use gpui::{
    Action, App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, KeyContext, Pixels,
    Stateful, Task, WeakEntity, Window, actions, prelude::*,
};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use settings::Settings;
use ui::{
    Button, ButtonStyle, Color, Icon, IconButton, IconName, IconSize, Label, LabelSize, Tooltip,
    prelude::*,
};
use util::ResultExt;
use workspace::PathList;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use crate::agent_panel::{AgentPanel, CreateThreadOptions};
use crate::agent_task_timeline::render_task_timeline;
use crate::agent_task_worktree::ensure_task_worktree;
use crate::{AgentInitialContent, AgentThreadSource};

actions!(agent_tasks, [ToggleAgentTaskPanel]);

pub const AGENT_TASK_PANEL_KEY: &str = "AgentTaskPanel";

pub fn init(file_system: Arc<dyn Fs>, cx: &mut App) {
    let subscription = cx.observe_new(move |workspace: &mut Workspace, window, cx| {
        let project = workspace.project().clone();
        let context_server_store = project.read(cx).context_server_store();
        let provider = Arc::new(agent::McpAgentTaskProvider::new(
            context_server_store,
            context_server::ContextServerId("tgr".into()),
        ));
        let store = cx.new(|cx| agent::AgentTaskStore::new(provider, cx));

        let panel = cx
            .new(|cx| AgentTaskPanel::new(store, workspace.weak_handle(), file_system.clone(), cx));

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
    store: Entity<AgentTaskStore>,
    selected_task_id: Option<AgentTaskId>,
    selected_detail: Option<AgentTaskDetail>,
    artifacts: Vec<(AgentTaskArtifact, Entity<Markdown>)>,
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    file_system: Arc<dyn Fs>,
    _fetch_detail_task: Option<Task<()>>,
    _action_task: Option<Task<()>>,
}

impl AgentTaskPanel {
    pub fn new(
        store: Entity<AgentTaskStore>,
        workspace: WeakEntity<Workspace>,
        file_system: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&store, |_this, _store, cx| {
            cx.notify();
        })
        .detach();

        Self {
            store,
            selected_task_id: None,
            selected_detail: None,
            artifacts: Vec::new(),
            focus_handle: cx.focus_handle(),
            workspace,
            file_system,
            _fetch_detail_task: None,
            _action_task: None,
        }
    }

    fn select_task(&mut self, id: AgentTaskId, cx: &mut Context<Self>) {
        self.selected_task_id = Some(id.clone());
        self.selected_detail = None;
        self.artifacts.clear();

        let provider = self.store.read(cx).provider().clone();
        let detail_task = provider.get_task(&id, cx);
        let artifacts_task = provider.list_artifacts(&id, cx);

        self._fetch_detail_task = Some(cx.spawn(async move |this, cx| {
            let detail_result = detail_task.await;
            let artifacts_result = artifacts_task.await;

            this.update(cx, |panel, cx| {
                match detail_result {
                    Ok(detail) => panel.selected_detail = Some(detail),
                    Err(err) => log::error!("failed to fetch task detail for {id}: {err:?}"),
                }
                match artifacts_result {
                    Ok(artifacts) => {
                        panel.artifacts = artifacts
                            .into_iter()
                            .map(|artifact| {
                                let markdown = cx.new(|cx| {
                                    Markdown::new(artifact.content.clone().into(), None, None, cx)
                                });
                                (artifact, markdown)
                            })
                            .collect();
                    }
                    Err(err) => log::error!("failed to fetch task artifacts for {id}: {err:?}"),
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn status_color(&self, status: AgentTaskStatus) -> Color {
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

    fn render_status_badge(&self, status: AgentTaskStatus) -> impl IntoElement {
        let label = match status {
            AgentTaskStatus::Ready => "READY",
            AgentTaskStatus::Blocked => "BLOCKED",
            AgentTaskStatus::Running => "RUNNING",
            AgentTaskStatus::Stale => "STALE",
            AgentTaskStatus::Review => "REVIEW",
            AgentTaskStatus::Completed => "COMPLETED",
            AgentTaskStatus::Failed => "FAILED",
        };
        let color = self.status_color(status);

        h_flex()
            .px_1p5()
            .py_0p5()
            .rounded_sm()
            .child(Label::new(label).size(LabelSize::Small).color(color))
    }

    fn render_task_tree(&self, cx: &mut Context<Self>) -> Div {
        let graph = self.store.read(cx).graph().clone();

        let roots: Vec<AgentTaskSummary> = graph
            .tasks
            .iter()
            .filter(|task| task.parent_id.is_none())
            .cloned()
            .collect();

        let mut visited = HashSet::new();
        let mut root_elements = Vec::new();
        for root in roots {
            root_elements.push(self.render_task_node(root, &graph, 0, &mut visited, cx));
        }

        v_flex().gap_1().children(root_elements)
    }

    fn render_task_node(
        &self,
        task: AgentTaskSummary,
        graph: &agent::AgentTaskGraph,
        depth: usize,
        visited: &mut HashSet<AgentTaskId>,
        cx: &mut Context<Self>,
    ) -> Div {
        // The graph arrives from an external server; a cyclic or very deep
        // `parent_id` chain must not overflow the stack while rendering.
        if depth > 16 || !visited.insert(task.id.clone()) {
            log::error!("skipping cyclic or too deep task subtree at {}", task.id);
            return v_flex();
        }

        let is_selected = self
            .selected_task_id
            .as_ref()
            .map_or(false, |selected_id| selected_id == &task.id);

        let children: Vec<AgentTaskSummary> = graph
            .tasks
            .iter()
            .filter(|child_task| {
                child_task
                    .parent_id
                    .as_ref()
                    .map_or(false, |parent_id| parent_id == &task.id)
            })
            .cloned()
            .collect();

        let policy_denied = self
            .store
            .read(cx)
            .policy_denied_event_for_task(&task.id)
            .cloned();

        let row = h_flex()
            .id(SharedString::from(format!("task-node-{}", task.id)))
            .w_full()
            .items_center()
            .justify_between()
            .px_2()
            .py_1()
            .pl(rems_from_px((depth * 16 + 8) as f32))
            .rounded_md()
            .cursor_pointer()
            .when(is_selected, |this| {
                this.bg(cx.theme().colors().element_selected)
            })
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(self.render_status_badge(task.status))
                    .child(
                        Label::new(task.title.clone())
                            .size(LabelSize::Small)
                            .truncate(),
                    )
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
                    .when_some(task.assignee.as_ref(), |this, assignee| {
                        this.child(
                            Label::new(assignee.clone())
                                .size(LabelSize::Small)
                                .color(Color::Accent),
                        )
                    }),
            )
            .on_click(cx.listener({
                let task_id = task.id;
                move |this, _event, _window, cx| {
                    this.select_task(task_id.clone(), cx);
                }
            }));

        let mut child_elements = Vec::new();
        for child in children {
            child_elements.push(self.render_task_node(child, graph, depth + 1, visited, cx));
        }

        v_flex().child(row).children(child_elements)
    }

    /// Launches an agent thread for the task in its isolated worktree. The
    /// thread is attached to the worktree via `work_dirs` rather than opening
    /// a new workspace: `create_worktree_workspace` cannot be reused here
    /// because it refuses paths that already exist, and the worktree has
    /// already been created by `ensure_task_worktree`.
    fn run_task(&mut self, summary: AgentTaskSummary, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        let project = workspace.read(cx).project().clone();
        let ensure_task = ensure_task_worktree(project, &summary, cx);

        let mut prompt = format!("## Task: {}\n\n", summary.title);
        if let Some(detail) = &self.selected_detail {
            prompt.push_str(&format!("### Description\n{}\n\n", detail.description));
            if !detail.acceptance_criteria.is_empty() {
                prompt.push_str("### Acceptance Criteria\n");
                for criterion in &detail.acceptance_criteria {
                    prompt.push_str(&format!("- {}\n", criterion));
                }
                prompt.push('\n');
            }
        }
        if !summary.write_scopes.is_empty() {
            prompt.push_str("### Write Scopes\n");
            for write_scope in &summary.write_scopes {
                prompt.push_str(&format!("- {}\n", write_scope));
            }
            prompt.push('\n');
        }
        prompt.push_str(&format!("Task reference: {}\n", summary.id));

        let task_id = summary.id.clone();
        let title: SharedString = format!("Task {}", summary.title).into();

        self._action_task = Some(cx.spawn_in(window, async move |_this, cx| {
            let worktree_path = match ensure_task.await {
                Ok(path) => path,
                Err(error) => {
                    log::error!("failed to create worktree for task {task_id}: {error:?}");
                    return;
                }
            };

            let agent_panel =
                workspace.read_with(cx, |workspace, cx| workspace.panel::<AgentPanel>(cx));
            let Some(agent_panel) = agent_panel else {
                log::error!("no agent panel available to run task {task_id}");
                return;
            };

            let thread_result = agent_panel.update_in(cx, |panel, window, cx| {
                let options = CreateThreadOptions {
                    title: Some(title),
                    initial_content: Some(AgentInitialContent::ContentBlock {
                        blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(prompt))],
                        auto_submit: true,
                    }),
                    agent: None,
                    model: None,
                    work_dirs: Some(PathList::new(&[worktree_path])),
                };
                panel.create_thread_with_options(
                    options,
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                );
            });
            if let Err(error) = thread_result {
                log::error!("failed to create agent thread for task {task_id}: {error:?}");
            }
        }));
    }

    /// Opens the branch diff for the task's `agent-task/{id}` branch using the
    /// existing `git::DiffBranch` action (`git_ui::project_diff`).
    fn view_task_diff(&self, window: &mut Window, cx: &mut Context<Self>) {
        match cx.build_action("git::DiffBranch", None) {
            Ok(action) => window.dispatch_action(action, cx),
            Err(error) => log::error!("failed to resolve git::DiffBranch action: {error:?}"),
        }
    }

    /// Removes the task's isolated worktree once the task has reached a
    /// terminal state and the user no longer needs its checkout.
    fn remove_task_worktree(&mut self, task_id: &AgentTaskId, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = workspace.read(cx).project().clone();
        let remove_task = crate::agent_task_worktree::remove_task_worktree(project, task_id, cx);
        remove_task.detach_and_log_err(cx);
    }

    fn render_task_detail(&self, window: &mut Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let Some(detail) = self.selected_detail.as_ref() else {
            return v_flex()
                .id("no_task_selected")
                .p_4()
                .items_center()
                .child(Label::new("Select a task to view details").color(Color::Muted));
        };

        let task_id = detail.summary.id.clone();
        let status = detail.summary.status;
        let policy_denied = self
            .store
            .read(cx)
            .policy_denied_event_for_task(&task_id)
            .cloned();
        let show_conflict_indicator = AgentSettings::get_global(cx).show_merge_conflict_indicator;

        v_flex()
            .id("task_detail")
            .gap_3()
            .p_3()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(Label::new(detail.summary.title.clone()).size(LabelSize::Default))
                    .child(self.render_status_badge(status)),
            )
            .when_some(policy_denied, |this, event| {
                this.child(
                    h_flex()
                        .gap_2()
                        .p_2()
                        .rounded_md()
                        .bg(cx.theme().status().warning_background)
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::Small)
                                .color(Color::Warning),
                        )
                        .child(
                            Label::new(format!("Policy Denied: {}", event.message))
                                .size(LabelSize::Small)
                                .color(Color::Warning),
                        ),
                )
            })
            .child(
                Label::new(detail.description.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when(!detail.acceptance_criteria.is_empty(), |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(Label::new("Acceptance Criteria:").size(LabelSize::Small))
                        .children(detail.acceptance_criteria.iter().map(|criterion| {
                            h_flex()
                                .gap_1p5()
                                .child(
                                    Icon::new(IconName::Check)
                                        .size(IconSize::Small)
                                        .color(Color::Success),
                                )
                                .child(Label::new(criterion.clone()).size(LabelSize::Small))
                        })),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("run_task", "Run Task")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener({
                                let summary = detail.summary.clone();
                                move |this, _event, window, cx| {
                                    this.run_task(summary.clone(), window, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("view_diff", "View Task Diff")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|this, _event, window, cx| {
                                this.view_task_diff(window, cx);
                            })),
                    )
                    .child(
                        Button::new("complete_task", "Complete Task")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener({
                                let task_id = task_id.clone();
                                move |this, _event, _window, cx| {
                                    let store = this.store.clone();
                                    store
                                        .update(cx, |store, cx| store.complete_task(&task_id, cx))
                                        .detach_and_log_err(cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("fail_task", "Fail Task")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener({
                                let task_id = task_id.clone();
                                move |this, _event, _window, cx| {
                                    let store = this.store.clone();
                                    store
                                        .update(cx, |store, cx| {
                                            store.fail_task(&task_id, "User failed task", cx)
                                        })
                                        .detach_and_log_err(cx);
                                }
                            })),
                    )
                    .when(status.is_terminal(), |this| {
                        this.child(
                            Button::new("remove_worktree", "Remove Worktree")
                                .style(ButtonStyle::Subtle)
                                .on_click(cx.listener({
                                    let task_id = task_id.clone();
                                    move |this, _event, _window, cx| {
                                        this.remove_task_worktree(&task_id, cx);
                                    }
                                })),
                        )
                    }),
            )
            .when(!self.artifacts.is_empty(), |this| {
                let task_id = task_id.clone();
                this.child(
                    v_flex()
                        .gap_2()
                        .child(Label::new("Review Artifacts:").size(LabelSize::Small))
                        .children(self.artifacts.iter().map(|(artifact, markdown)| {
                            let is_review = artifact.kind == "review";
                            let artifact_id = SharedString::from(artifact.id.clone());
                            let markdown_element = MarkdownElement::new(
                                markdown.clone(),
                                MarkdownStyle::themed(MarkdownFont::Agent, window, cx),
                            );

                            v_flex()
                                .gap_1()
                                .p_2()
                                .rounded_md()
                                .bg(cx.theme().colors().element_background)
                                .child(
                                    h_flex().justify_between().child(
                                        Label::new(format!("[{}] {}", artifact.kind, artifact_id))
                                            .size(LabelSize::Small)
                                            .color(Color::Accent),
                                    ),
                                )
                                .child(markdown_element)
                                .when(is_review, |this| {
                                    let task_id = task_id.clone();
                                    let approve_id =
                                        SharedString::from(format!("force_approve_{artifact_id}"));
                                    let reject_id =
                                        SharedString::from(format!("reject_retry_{artifact_id}"));
                                    this.child(
                                        h_flex()
                                            .gap_2()
                                            .mt_1()
                                            .child(
                                                Button::new(approve_id, "Force approve")
                                                    .style(ButtonStyle::Subtle)
                                                    .on_click(cx.listener({
                                                        let task_id = task_id.clone();
                                                        move |this, _event, _window, cx| {
                                                            let store = this.store.clone();
                                                            store
                                                                .update(cx, |store, cx| {
                                                                    store
                                                                        .complete_task(&task_id, cx)
                                                                })
                                                                .detach_and_log_err(cx);
                                                        }
                                                    })),
                                            )
                                            .child(
                                                Button::new(reject_id, "Reject & retry")
                                                    .style(ButtonStyle::Subtle)
                                                    .on_click(cx.listener({
                                                        move |this, _event, _window, cx| {
                                                            let store = this.store.clone();
                                                            store
                                                                .update(cx, |store, cx| {
                                                                    store.fail_task(
                                                                        &task_id,
                                                                        "human rejected",
                                                                        cx,
                                                                    )
                                                                })
                                                                .detach_and_log_err(cx);
                                                        }
                                                    })),
                                            ),
                                    )
                                })
                        })),
                )
            })
            .when(!detail.events_tail.is_empty(), |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(Label::new("Event History:").size(LabelSize::Small))
                        .child(render_task_timeline(&detail.events_tail, cx)),
                )
            })
            .when(status == AgentTaskStatus::Completed, |this| {
                this.child(
                    v_flex()
                        .gap_2()
                        .p_2()
                        .rounded_md()
                        .bg(cx.theme().status().created_background)
                        .child(
                            Label::new("Task Completed — Ready to Merge")
                                .size(LabelSize::Small)
                                .color(Color::Success),
                        )
                        .when(show_conflict_indicator, |this| {
                            this.child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Icon::new(IconName::Warning)
                                            .size(IconSize::Small)
                                            .color(Color::Warning),
                                    )
                                    .child(
                                        Label::new("Check the branch diff for conflicts")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                            )
                        })
                        .child(
                            Button::new("merge_view_diff", "View Task Diff")
                                .style(ButtonStyle::Subtle)
                                .on_click(cx.listener(|this, _event, window, cx| {
                                    this.view_task_diff(window, cx);
                                })),
                        ),
                )
            })
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
        AgentSettings::get_global(cx).dock.into()
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
        settings::update_settings_file(self.file_system.clone(), cx, move |settings, _| {
            settings
                .agent
                .get_or_insert_default()
                .set_dock(position.into());
        });
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(320.0)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Check)
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let is_offline = store.is_offline();
        let last_error = store.last_error().map(|error| error.to_string());

        let mut key_context = KeyContext::new_with_defaults();
        key_context.add("AgentTaskPanel");

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
            )
            .when(is_offline, |this| {
                this.child(
                    h_flex()
                        .px_3()
                        .py_2()
                        .bg(cx.theme().status().warning_background)
                        .items_center()
                        .justify_between()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    Icon::new(IconName::Warning)
                                        .size(IconSize::Small)
                                        .color(Color::Warning),
                                )
                                .child(
                                    Label::new(
                                        last_error
                                            .unwrap_or_else(|| "Task server offline".to_string()),
                                    )
                                    .size(LabelSize::Small)
                                    .color(Color::Warning),
                                ),
                        )
                        .child(
                            Button::new("retry_tasks", "Retry")
                                .style(ButtonStyle::Subtle)
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    let store = this.store.clone();
                                    store
                                        .update(cx, |store, cx| store.refresh(cx))
                                        .detach_and_log_err(cx);
                                })),
                        ),
                )
            })
            .child(
                v_flex()
                    .id("agent_tasks_scroll_container")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(self.render_task_tree(cx))
                    .child(self.render_task_detail(window, cx)),
            )
    }
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
                    tasks: vec![AgentTaskSummary {
                        id: AgentTaskId::from("TASK-1"),
                        parent_id: None,
                        title: "Test Task".to_string(),
                        status: AgentTaskStatus::Ready,
                        attempt: 1,
                        assignee: None,
                        write_scopes: vec![],
                    }],
                }))
            }
        }

        fn get_task(
            &self,
            _id: &AgentTaskId,
            _cx: &mut App,
        ) -> Task<anyhow::Result<AgentTaskDetail>> {
            Task::ready(Ok(AgentTaskDetail {
                summary: AgentTaskSummary {
                    id: AgentTaskId::from("TASK-1"),
                    parent_id: None,
                    title: "Test Task".to_string(),
                    status: AgentTaskStatus::Ready,
                    attempt: 1,
                    assignee: None,
                    write_scopes: vec![],
                },
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
            Task::ready(Ok(vec![]))
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
        let _project = Project::test(file_system.clone(), [], cx).await;
        let provider = Arc::new(TestProvider { offline: true });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        // Opening a window with the panel as its root view exercises
        // `render`: the offline banner must draw without panicking.
        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(store, WeakEntity::new_invalid(), file_system, cx)
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
        let _project = Project::test(file_system.clone(), [], cx).await;
        let provider = Arc::new(TestProvider { offline: false });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            AgentTaskPanel::new(store, WeakEntity::new_invalid(), file_system, cx)
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            let store = panel.store.read(cx);
            assert!(!store.is_offline());
            assert_eq!(store.graph().tasks.len(), 1);
            assert_eq!(store.graph().tasks[0].id.0.as_ref(), "TASK-1");
        });

        // Selecting the task fetches the detail, then the panel redraws the
        // tree plus the detail view (description, criteria, toolbar).
        panel.update(cx, |panel, cx| {
            panel.select_task(AgentTaskId::from("TASK-1"), cx);
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
}
