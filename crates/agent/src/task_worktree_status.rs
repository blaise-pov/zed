use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use futures::StreamExt;
use gpui::{App, Context, Entity, Task};
use project::Project;
use project::git_store::worktrees_directory_for_repo;
use serde::{Deserialize, Serialize};

use crate::agent_task::{AgentGoalSummary, AgentTaskId, AgentTaskSummary};
use crate::task_worktree::{resolve_ref_to_sha, task_worktree_context, task_worktree_path};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalGitState {
    pub branch: String,
    pub branch_exists: bool,
    pub tip_sha: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskGitState {
    pub worktree_path: PathBuf,
    pub exists_on_disk: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrphanWorktree {
    pub task_id_hint: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskGitSnapshot {
    pub goals: HashMap<String, GoalGitState>,
    pub tasks: HashMap<AgentTaskId, TaskGitState>,
    pub orphan_worktrees: Vec<OrphanWorktree>,
}

pub fn snapshot(
    project: &Entity<Project>,
    tasks: &[AgentTaskSummary],
    goals: &[AgentGoalSummary],
    cx: &mut App,
) -> Task<Result<TaskGitSnapshot>> {
    let tasks = tasks.to_vec();
    let goals = goals.to_vec();
    let project = project.clone();

    cx.spawn(async move |cx| {
        let (_repository, anchor_path, path_style, worktree_setting, file_system) = project
            .update(cx, |project, cx| {
                anyhow::Ok(task_worktree_context(project, cx))
            })??;

        let dot_git = anchor_path.join(".git");
        let git_repo = file_system
            .open_repo(&dot_git, None)
            .with_context(|| format!("opening repo at {}", dot_git.display()))?;

        let branches_scan = git_repo.branches().await?;
        let mut goals_map = HashMap::new();

        let mut existing_branches = HashMap::new();
        for branch in branches_scan.branches {
            let ref_name = branch.ref_name.as_ref();
            let branch_name = ref_name
                .strip_prefix("refs/heads/")
                .or_else(|| ref_name.strip_prefix("refs/remotes/"))
                .unwrap_or(ref_name);
            let branch_name = branch_name.strip_prefix("origin/").unwrap_or(branch_name);
            if let Some(goal_id) = branch_name.strip_prefix("agent-goal/") {
                let canonical_branch = format!("agent-goal/{goal_id}");
                let tip_sha = if let Some(commit) = branch.most_recent_commit {
                    Some(commit.sha.to_string())
                } else {
                    match resolve_ref_to_sha(&git_repo, &canonical_branch).await {
                        Ok(sha) => sha,
                        Err(error) => {
                            log::warn!(
                                "failed to resolve tip sha for goal branch {canonical_branch}: {error:#}"
                            );
                            None
                        }
                    }
                };
                existing_branches.insert(goal_id.to_string(), (canonical_branch, tip_sha));
            }
        }

        for goal in &goals {
            let branch = format!("agent-goal/{}", goal.goal_id);
            if let Some((_, tip_sha)) = existing_branches.get(&goal.goal_id) {
                goals_map.insert(
                    goal.goal_id.clone(),
                    GoalGitState {
                        branch,
                        branch_exists: true,
                        tip_sha: tip_sha.clone(),
                    },
                );
            } else {
                goals_map.insert(
                    goal.goal_id.clone(),
                    GoalGitState {
                        branch,
                        branch_exists: false,
                        tip_sha: None,
                    },
                );
            }
        }

        for (goal_id, (branch_name, tip_sha)) in existing_branches {
            goals_map.entry(goal_id).or_insert_with(|| GoalGitState {
                branch: branch_name,
                branch_exists: true,
                tip_sha,
            });
        }

        let base_dir = worktrees_directory_for_repo(&anchor_path, &worktree_setting, path_style)?;
        let mut tasks_map = HashMap::new();
        let mut orphan_worktrees = Vec::new();

        let known_task_ids: HashSet<String> = tasks
            .iter()
            .map(|task| task.id.as_str().to_string())
            .collect();

        if file_system.is_dir(&base_dir).await {
            let mut disk_task_entries = HashMap::new();
            let mut dir_stream = file_system.read_dir(&base_dir).await?;
            while let Some(entry_result) = dir_stream.next().await {
                let entry_path = entry_result?;
                if let Some(file_name) = entry_path.file_name().and_then(|name| name.to_str()) {
                    if let Some(task_id_hint) = file_name.strip_prefix("agent-task-") {
                        disk_task_entries.insert(task_id_hint.to_string(), entry_path);
                    }
                }
            }

            for task in &tasks {
                let worktree_path =
                    task_worktree_path(&anchor_path, &worktree_setting, path_style, &task.id)?;
                let exists_on_disk = disk_task_entries.contains_key(task.id.as_str());
                tasks_map.insert(
                    task.id.clone(),
                    TaskGitState {
                        worktree_path,
                        exists_on_disk,
                    },
                );
            }

            for (task_id_hint, path) in disk_task_entries {
                if !known_task_ids.contains(&task_id_hint) {
                    orphan_worktrees.push(OrphanWorktree { task_id_hint, path });
                }
            }
            orphan_worktrees.sort_by(|left, right| left.task_id_hint.cmp(&right.task_id_hint));
        } else {
            for task in &tasks {
                let worktree_path =
                    task_worktree_path(&anchor_path, &worktree_setting, path_style, &task.id)?;
                tasks_map.insert(
                    task.id.clone(),
                    TaskGitState {
                        worktree_path,
                        exists_on_disk: false,
                    },
                );
            }
        }

        Ok(TaskGitSnapshot {
            goals: goals_map,
            tasks: tasks_map,
            orphan_worktrees,
        })
    })
}

pub struct TaskWorktreeStatus {
    project: Entity<Project>,
    snapshot: Option<TaskGitSnapshot>,
    is_loading: bool,
    last_error: Option<String>,
    current_epoch: u64,
}

impl TaskWorktreeStatus {
    pub fn new(project: Entity<Project>, _cx: &mut Context<Self>) -> Self {
        Self {
            project,
            snapshot: None,
            is_loading: false,
            last_error: None,
            current_epoch: 0,
        }
    }

    pub fn snapshot(&self) -> Option<&TaskGitSnapshot> {
        self.snapshot.as_ref()
    }

    pub fn is_loading(&self) -> bool {
        self.is_loading
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn refresh(
        &mut self,
        tasks: Vec<AgentTaskSummary>,
        goals: Vec<AgentGoalSummary>,
        cx: &mut Context<Self>,
    ) -> Task<Result<TaskGitSnapshot>> {
        self.current_epoch = self.current_epoch.wrapping_add(1);
        let request_epoch = self.current_epoch;
        self.is_loading = true;
        self.last_error = None;
        cx.notify();

        let snapshot_task = snapshot(&self.project, &tasks, &goals, cx);
        cx.spawn(async move |this, cx| {
            let result = snapshot_task.await;
            this.update(cx, |status, cx| {
                if status.current_epoch == request_epoch {
                    status.is_loading = false;
                    match &result {
                        Ok(snapshot) => {
                            status.snapshot = Some(snapshot.clone());
                            status.last_error = None;
                        }
                        Err(err) => {
                            status.last_error = Some(err.to_string());
                        }
                    }
                    cx.notify();
                } else {
                    log::debug!(
                        "ignoring outdated task worktree snapshot result from epoch {request_epoch} (current: {})",
                        status.current_epoch
                    );
                }
            })?;
            result
        })
    }

    pub fn set_snapshot_for_test(&mut self, snapshot: TaskGitSnapshot, cx: &mut Context<Self>) {
        self.snapshot = Some(snapshot);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use fs::FakeFs;
    use gpui::{AppContext, TestAppContext};
    use project::Project;
    use project::project_settings::ProjectSettings;
    use serde_json::json;
    use settings::{Settings, SettingsStore};
    use util::path;

    use super::*;
    use crate::agent_task::AgentTaskStatus;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            ProjectSettings::register(cx);
            agent_settings::AgentSettings::register(cx);
        });
    }

    #[gpui::test]
    async fn test_snapshot_with_goal_branch_and_task_worktree(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".git": {
                    "HEAD": "ref: refs/heads/main\n"
                }
            }),
        )
        .await;

        fs.insert_tree(
            path!("/worktrees/root"),
            json!({
                "agent-task-TASK-1": {
                    "file.rs": "fn main() {}"
                },
                "agent-task-TASK-ORPHAN": {
                    "orphan.rs": "fn orphan() {}"
                }
            }),
        )
        .await;

        let dot_git = std::path::Path::new(path!("/root/.git"));
        fs.with_git_state(dot_git, true, |state| {
            state.branches.insert("main".into());
            state
                .refs
                .insert("refs/heads/main".into(), "main-sha-100".into());
            state.refs.insert("HEAD".into(), "main-sha-100".into());

            state.branches.insert("agent-goal/GOAL-1".into());
            state.refs.insert(
                "refs/heads/agent-goal/GOAL-1".into(),
                "goal-1-sha-200".into(),
            );
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let tasks = vec![
            AgentTaskSummary {
                id: AgentTaskId::from("TASK-1"),
                parent_id: None,
                goal_id: Some("GOAL-1".to_string()),
                title: "Task 1".to_string(),
                status: AgentTaskStatus::Running,
                attempt: 1,
                assignee: None,
                write_scopes: vec![],
            },
            AgentTaskSummary {
                id: AgentTaskId::from("TASK-2"),
                parent_id: None,
                goal_id: Some("GOAL-1".to_string()),
                title: "Task 2".to_string(),
                status: AgentTaskStatus::Ready,
                attempt: 1,
                assignee: None,
                write_scopes: vec![],
            },
        ];

        let goals = vec![
            AgentGoalSummary {
                goal_id: "GOAL-1".to_string(),
                title: "Goal 1".to_string(),
                status: "active".to_string(),
                priority: 1,
                tasks_total: 2,
                tasks_done: 0,
            },
            AgentGoalSummary {
                goal_id: "GOAL-2".to_string(),
                title: "Goal 2".to_string(),
                status: "active".to_string(),
                priority: 2,
                tasks_total: 1,
                tasks_done: 0,
            },
        ];

        let snapshot = cx
            .update(|cx| snapshot(&project, &tasks, &goals, cx))
            .await
            .unwrap();

        // GOAL-1 branch exists with tip_sha
        let goal_1_state = snapshot.goals.get("GOAL-1").unwrap();
        assert_eq!(goal_1_state.branch, "agent-goal/GOAL-1");
        assert!(goal_1_state.branch_exists);
        assert_eq!(goal_1_state.tip_sha.as_deref(), Some("goal-1-sha-200"));

        // GOAL-2 branch does not exist
        let goal_2_state = snapshot.goals.get("GOAL-2").unwrap();
        assert_eq!(goal_2_state.branch, "agent-goal/GOAL-2");
        assert!(!goal_2_state.branch_exists);
        assert_eq!(goal_2_state.tip_sha, None);

        // TASK-1 exists on disk
        let task_1_state = snapshot.tasks.get(&AgentTaskId::from("TASK-1")).unwrap();
        assert!(task_1_state.exists_on_disk);
        assert!(
            task_1_state
                .worktree_path
                .to_string_lossy()
                .contains("agent-task-TASK-1")
        );

        // TASK-2 does not exist on disk
        let task_2_state = snapshot.tasks.get(&AgentTaskId::from("TASK-2")).unwrap();
        assert!(!task_2_state.exists_on_disk);
        assert!(
            task_2_state
                .worktree_path
                .to_string_lossy()
                .contains("agent-task-TASK-2")
        );

        // TASK-ORPHAN is in orphan_worktrees
        assert_eq!(snapshot.orphan_worktrees.len(), 1);
        assert_eq!(snapshot.orphan_worktrees[0].task_id_hint, "TASK-ORPHAN");
        assert!(
            snapshot.orphan_worktrees[0]
                .path
                .to_string_lossy()
                .contains("agent-task-TASK-ORPHAN")
        );
    }

    #[gpui::test]
    async fn test_snapshot_empty_directory_and_no_goals(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".git": {
                    "HEAD": "ref: refs/heads/main\n"
                }
            }),
        )
        .await;

        let dot_git = std::path::Path::new(path!("/root/.git"));
        fs.with_git_state(dot_git, true, |state| {
            state.branches.insert("main".into());
            state
                .refs
                .insert("refs/heads/main".into(), "main-sha-100".into());
            state.refs.insert("HEAD".into(), "main-sha-100".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let snapshot = cx
            .update(|cx| snapshot(&project, &[], &[], cx))
            .await
            .unwrap();

        assert!(snapshot.goals.is_empty());
        assert!(snapshot.tasks.is_empty());
        assert!(snapshot.orphan_worktrees.is_empty());
    }

    #[gpui::test]
    async fn test_task_worktree_status_entity(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".git": {
                    "HEAD": "ref: refs/heads/main\n"
                }
            }),
        )
        .await;

        let dot_git = std::path::Path::new(path!("/root/.git"));
        fs.with_git_state(dot_git, true, |state| {
            state.branches.insert("main".into());
            state
                .refs
                .insert("refs/heads/main".into(), "main-sha-100".into());
            state.refs.insert("HEAD".into(), "main-sha-100".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let status_entity =
            cx.update(|cx| cx.new(|cx| TaskWorktreeStatus::new(project.clone(), cx)));

        assert!(!status_entity.read_with(cx, |s, _| s.is_loading()));
        assert!(status_entity.read_with(cx, |s, _| s.snapshot().is_none()));

        let refresh_task = status_entity.update(cx, |s, cx| s.refresh(vec![], vec![], cx));
        let snapshot = refresh_task.await.unwrap();

        assert!(snapshot.goals.is_empty());
        assert!(snapshot.tasks.is_empty());
        assert!(snapshot.orphan_worktrees.is_empty());

        assert!(!status_entity.read_with(cx, |s, _| s.is_loading()));
        assert!(status_entity.read_with(cx, |s, _| s.snapshot().is_some()));
        assert_eq!(
            status_entity.read_with(cx, |s, _| s.snapshot().cloned().unwrap()),
            snapshot
        );
    }

    #[gpui::test]
    async fn test_snapshot_no_git_repository_populates_last_error(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/empty_root"),
            json!({
                "some_file.txt": "hello"
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/empty_root").as_ref()], cx).await;

        // Calling snapshot directly returns Err
        let snapshot_result = cx.update(|cx| snapshot(&project, &[], &[], cx)).await;
        assert!(snapshot_result.is_err());
        assert!(
            snapshot_result
                .unwrap_err()
                .to_string()
                .contains("no git repository found")
        );

        // Refreshing TaskWorktreeStatus entity stores error in last_error without panicking
        let status_entity =
            cx.update(|cx| cx.new(|cx| TaskWorktreeStatus::new(project.clone(), cx)));
        let refresh_result = status_entity
            .update(cx, |status, cx| status.refresh(vec![], vec![], cx))
            .await;
        assert!(refresh_result.is_err());

        assert!(!status_entity.read_with(cx, |s, _| s.is_loading()));
        assert!(status_entity.read_with(cx, |s, _| s.snapshot().is_none()));
        let last_error = status_entity.read_with(cx, |s, _| s.last_error().map(|s| s.to_string()));
        assert!(last_error.is_some());
        assert!(last_error.unwrap().contains("no git repository found"));
    }
}
