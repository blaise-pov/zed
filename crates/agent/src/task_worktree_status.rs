use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use futures::StreamExt;
use gpui::{App, Context, Entity, Task};
use parking_lot::Mutex;
use project::Project;
use project::git_store::worktrees_directory_for_repo;
use serde::{Deserialize, Serialize};

use crate::agent_task::{AgentGoalSummary, AgentTaskId, AgentTaskSummary};
use crate::task_worktree::{system_git_binary, task_worktree_context, task_worktree_path};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffShortStat {
    pub files: u32,
    pub added: u32,
    pub removed: u32,
}

impl DiffShortStat {
    pub fn from_numstat(output: &str) -> Self {
        let diff_stat = git::status::parse_numstat(output);
        let files = u32::try_from(diff_stat.entries.len()).unwrap_or(u32::MAX);
        let mut added = 0u32;
        let mut removed = 0u32;
        for (_, stat) in diff_stat.entries.iter() {
            added = added.saturating_add(stat.added);
            removed = removed.saturating_add(stat.deleted);
        }
        Self {
            files,
            added,
            removed,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalGitState {
    pub branch: String,
    pub branch_exists: bool,
    pub tip_sha: Option<String>,
    pub diff: Option<DiffShortStat>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskGitState {
    pub worktree_path: PathBuf,
    pub exists_on_disk: bool,
    pub branch: Option<String>,
    pub branch_exists: bool,
    pub diff: Option<DiffShortStat>,
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

pub type DiffCache = Arc<Mutex<HashMap<(String, String), DiffShortStat>>>;

#[allow(dead_code)]
pub const SPEC_DIFF_CACHE_TTL: Duration = Duration::from_secs(30);

#[allow(dead_code)]
static TIMED_DIFF_CACHE: std::sync::OnceLock<
    Mutex<HashMap<(PathBuf, String), (Instant, Option<DiffShortStat>)>>,
> = std::sync::OnceLock::new();

#[allow(dead_code)]
fn timed_diff_cache() -> &'static Mutex<HashMap<(PathBuf, String), (Instant, Option<DiffShortStat>)>>
{
    TIMED_DIFF_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[allow(dead_code)]
fn get_timed_cached_diff(working_dir: &Path, diff_spec: &str) -> Option<Option<DiffShortStat>> {
    let key = (working_dir.to_path_buf(), diff_spec.to_string());
    let cache = timed_diff_cache().lock();
    if let Some((timestamp, stat)) = cache.get(&key) {
        if timestamp.elapsed() < SPEC_DIFF_CACHE_TTL {
            return Some(*stat);
        }
    }
    None
}

#[allow(dead_code)]
fn set_timed_cached_diff(working_dir: &Path, diff_spec: &str, stat: Option<DiffShortStat>) {
    let key = (working_dir.to_path_buf(), diff_spec.to_string());
    timed_diff_cache()
        .lock()
        .insert(key, (Instant::now(), stat));
}

#[allow(dead_code)]
static GLOBAL_DIFF_CACHE: std::sync::OnceLock<Mutex<HashMap<(String, String), DiffShortStat>>> =
    std::sync::OnceLock::new();

#[allow(dead_code)]
fn global_diff_cache() -> &'static Mutex<HashMap<(String, String), DiffShortStat>> {
    GLOBAL_DIFF_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[allow(dead_code)]
fn get_cached_diff(
    cache: Option<&DiffCache>,
    base_sha: &str,
    head_sha: &str,
) -> Option<DiffShortStat> {
    let key = (base_sha.to_string(), head_sha.to_string());
    if let Some(cache) = cache {
        cache.lock().get(&key).copied()
    } else {
        global_diff_cache().lock().get(&key).copied()
    }
}

#[allow(dead_code)]
fn set_cached_diff(
    cache: Option<&DiffCache>,
    base_sha: String,
    head_sha: String,
    stat: DiffShortStat,
) {
    let key = (base_sha, head_sha);
    if let Some(cache) = cache {
        cache.lock().insert(key, stat);
    } else {
        global_diff_cache().lock().insert(key, stat);
    }
}

#[allow(dead_code)]
async fn compute_short_stat_for_diff(
    working_dir: &Path,
    diff_spec: &str,
    base_sha: Option<&str>,
    head_sha: Option<&str>,
    cache: Option<&DiffCache>,
) -> Option<DiffShortStat> {
    if let (Some(base), Some(head)) = (base_sha, head_sha) {
        if let Some(cached) = get_cached_diff(cache, base, head) {
            return Some(cached);
        }
    } else if let Some(cached) = get_timed_cached_diff(working_dir, diff_spec) {
        return cached;
    }

    if !working_dir.is_dir() {
        return None;
    }

    let git_binary = system_git_binary()?;
    let mut command = util::command::new_command(git_binary);
    command.args(["diff", "--numstat", "--no-renames", diff_spec]);
    command.current_dir(working_dir);

    let stat = match command.output().await {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stat = DiffShortStat::from_numstat(&stdout);
            if let (Some(base), Some(head)) = (base_sha, head_sha) {
                set_cached_diff(cache, base.to_string(), head.to_string(), stat);
            }
            Some(stat)
        }
        Ok(output) => {
            log::warn!(
                "git diff {diff_spec} failed in {}: {}",
                working_dir.display(),
                String::from_utf8_lossy(&output.stderr)
            );
            None
        }
        Err(error) => {
            log::warn!(
                "failed to execute git diff {diff_spec} in {}: {error}",
                working_dir.display()
            );
            None
        }
    };

    if base_sha.is_none() || head_sha.is_none() {
        set_timed_cached_diff(working_dir, diff_spec, stat);
    }

    stat
}

pub fn snapshot(
    project: &Entity<Project>,
    tasks: &[AgentTaskSummary],
    goals: &[AgentGoalSummary],
    cx: &mut App,
) -> Task<Result<TaskGitSnapshot>> {
    snapshot_with_cache(project, tasks, goals, None, cx)
}

pub fn snapshot_with_cache(
    project: &Entity<Project>,
    tasks: &[AgentTaskSummary],
    goals: &[AgentGoalSummary],
    _diff_cache: Option<DiffCache>,
    cx: &mut App,
) -> Task<Result<TaskGitSnapshot>> {
    let tasks = tasks.to_vec();
    let goals = goals.to_vec();
    let project = project.clone();

    cx.spawn(async move |cx| {
        let (anchor_path, path_style, worktree_setting, file_system) =
            project.update(cx, |project, cx| {
                let (_repository, anchor_path, path_style, worktree_setting, file_system) =
                    task_worktree_context(project, cx)?;
                anyhow::Ok((anchor_path, path_style, worktree_setting, file_system))
            })?;

        let dot_git = anchor_path.join(".git");
        let git_binary = system_git_binary();
        let git_repo = file_system
            .open_repo(&dot_git, git_binary.as_deref())
            .with_context(|| format!("opening repo at {}", dot_git.display()))?;

        let branches_scan = git_repo.branches().await?;
        let mut goals_map = HashMap::new();

        let mut existing_goal_branches = HashMap::new();
        let mut existing_task_branches = HashMap::new();
        for branch in branches_scan.branches {
            let ref_name = branch.ref_name.as_ref();
            let branch_name = ref_name
                .strip_prefix("refs/heads/")
                .or_else(|| ref_name.strip_prefix("refs/remotes/"))
                .unwrap_or(ref_name);
            let branch_name = branch_name.strip_prefix("origin/").unwrap_or(branch_name);
            if let Some(goal_id) = branch_name.strip_prefix("agent-goal/") {
                let canonical_branch = format!("agent-goal/{goal_id}");
                let tip_sha = branch
                    .most_recent_commit
                    .map(|commit| commit.sha.to_string());
                let entry = existing_goal_branches.entry(goal_id.to_string());
                match entry {
                    std::collections::hash_map::Entry::Vacant(vacant) => {
                        vacant.insert((canonical_branch, tip_sha));
                    }
                    std::collections::hash_map::Entry::Occupied(mut occupied) => {
                        if occupied.get().1.is_none() && tip_sha.is_some() {
                            occupied.insert((canonical_branch, tip_sha));
                        }
                    }
                }
            } else if let Some(task_id) = branch_name.strip_prefix("agent-task/") {
                let canonical_branch = format!("agent-task/{task_id}");
                let tip_sha = branch
                    .most_recent_commit
                    .map(|commit| commit.sha.to_string());
                let entry = existing_task_branches.entry(task_id.to_string());
                match entry {
                    std::collections::hash_map::Entry::Vacant(vacant) => {
                        vacant.insert((canonical_branch, tip_sha));
                    }
                    std::collections::hash_map::Entry::Occupied(mut occupied) => {
                        if occupied.get().1.is_none() && tip_sha.is_some() {
                            occupied.insert((canonical_branch, tip_sha));
                        }
                    }
                }
            }
        }

        for goal in &goals {
            let branch = format!("agent-goal/{}", goal.goal_id);
            if let Some((_, tip_sha)) = existing_goal_branches.get(&goal.goal_id) {
                goals_map.insert(
                    goal.goal_id.clone(),
                    GoalGitState {
                        branch,
                        branch_exists: true,
                        tip_sha: tip_sha.clone(),
                        diff: None,
                    },
                );
            } else {
                goals_map.insert(
                    goal.goal_id.clone(),
                    GoalGitState {
                        branch,
                        branch_exists: false,
                        tip_sha: None,
                        diff: None,
                    },
                );
            }
        }

        for (goal_id, (branch_name, tip_sha)) in existing_goal_branches {
            goals_map.entry(goal_id).or_insert_with(|| GoalGitState {
                branch: branch_name,
                branch_exists: true,
                tip_sha,
                diff: None,
            });
        }

        let base_dir = worktrees_directory_for_repo(&anchor_path, &worktree_setting, path_style)?;
        let mut tasks_map = HashMap::new();
        let mut orphan_worktrees = Vec::new();

        let known_task_ids: HashSet<String> = tasks
            .iter()
            .map(|task| task.id.as_str().to_string())
            .collect();

        let disk_task_entries = if file_system.is_dir(&base_dir).await {
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

            for (task_id_hint, path) in &disk_task_entries {
                if !known_task_ids.contains(task_id_hint) {
                    orphan_worktrees.push(OrphanWorktree {
                        task_id_hint: task_id_hint.clone(),
                        path: path.clone(),
                    });
                }
            }
            orphan_worktrees.sort_by(|left, right| left.task_id_hint.cmp(&right.task_id_hint));
            disk_task_entries
        } else {
            HashMap::new()
        };

        for task in &tasks {
            let worktree_path =
                task_worktree_path(&anchor_path, &worktree_setting, path_style, &task.id)?;
            let exists_on_disk = disk_task_entries.contains_key(task.id.as_str());

            let (branch, branch_exists) = if let Some((canonical_branch, _tip_sha)) =
                existing_task_branches.get(task.id.as_str())
            {
                (Some(canonical_branch.clone()), true)
            } else {
                (None, false)
            };

            tasks_map.insert(
                task.id.clone(),
                TaskGitState {
                    worktree_path,
                    exists_on_disk,
                    branch,
                    branch_exists,
                    diff: None,
                },
            );
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
    diff_cache: DiffCache,
    last_tasks: Option<Vec<AgentTaskSummary>>,
    last_goals: Option<Vec<AgentGoalSummary>>,
}

impl TaskWorktreeStatus {
    pub fn new(project: Entity<Project>, _cx: &mut Context<Self>) -> Self {
        Self {
            project,
            snapshot: None,
            is_loading: false,
            last_error: None,
            current_epoch: 0,
            diff_cache: Arc::new(Mutex::new(HashMap::new())),
            last_tasks: None,
            last_goals: None,
        }
    }

    pub fn diff_cache(&self) -> &DiffCache {
        &self.diff_cache
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

    pub fn refresh_if_needed(
        &mut self,
        tasks: Vec<AgentTaskSummary>,
        goals: Vec<AgentGoalSummary>,
        cx: &mut Context<Self>,
    ) -> Task<Result<TaskGitSnapshot>> {
        if self.snapshot.is_some() && self.last_error.is_none() {
            if let (Some(last_tasks), Some(last_goals)) = (&self.last_tasks, &self.last_goals) {
                if last_tasks.len() == tasks.len() && last_goals.len() == goals.len() {
                    let task_ids_match = last_tasks
                        .iter()
                        .map(|t| (&t.id, t.goal_id.as_deref()))
                        .collect::<HashSet<_>>()
                        == tasks
                            .iter()
                            .map(|t| (&t.id, t.goal_id.as_deref()))
                            .collect::<HashSet<_>>();
                    let goal_ids_match = last_goals
                        .iter()
                        .map(|g| &g.goal_id)
                        .collect::<HashSet<_>>()
                        == goals.iter().map(|g| &g.goal_id).collect::<HashSet<_>>();

                    if task_ids_match && goal_ids_match {
                        if let Some(snapshot) = self.snapshot.clone() {
                            return Task::ready(Ok(snapshot));
                        }
                    }
                }
            }
        }

        self.refresh(tasks, goals, cx)
    }

    pub fn refresh(
        &mut self,
        tasks: Vec<AgentTaskSummary>,
        goals: Vec<AgentGoalSummary>,
        cx: &mut Context<Self>,
    ) -> Task<Result<TaskGitSnapshot>> {
        if self.snapshot.is_some()
            && self.last_error.is_none()
            && self.last_tasks.as_ref() == Some(&tasks)
            && self.last_goals.as_ref() == Some(&goals)
        {
            if let Some(snapshot) = self.snapshot.clone() {
                return Task::ready(Ok(snapshot));
            }
        }

        self.current_epoch = self.current_epoch.wrapping_add(1);
        let request_epoch = self.current_epoch;
        self.is_loading = true;
        self.last_error = None;

        let tasks_clone = tasks.clone();
        let goals_clone = goals.clone();
        let snapshot_task = snapshot_with_cache(
            &self.project,
            &tasks,
            &goals,
            Some(self.diff_cache.clone()),
            cx,
        );
        cx.spawn(async move |this, cx| {
            let result = snapshot_task.await;
            this.update(cx, |status, cx| {
                if status.current_epoch == request_epoch {
                    status.is_loading = false;
                    match &result {
                        Ok(snapshot) => {
                            status.snapshot = Some(snapshot.clone());
                            status.last_tasks = Some(tasks_clone);
                            status.last_goals = Some(goals_clone);
                            status.last_error = None;
                        }
                        Err(err) => {
                            status.last_error = Some(format!("{err:#}"));
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
        self.last_tasks = None;
        self.last_goals = None;
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
    use crate::agent_task::{AgentGoalStatus, AgentTaskStatus};

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
                created_at: None,
                assigned_profile: None,
                model: None,
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
                created_at: None,
                assigned_profile: None,
                model: None,
            },
        ];

        let goals = vec![
            AgentGoalSummary {
                goal_id: "GOAL-1".to_string(),
                title: "Goal 1".to_string(),
                status: AgentGoalStatus::Running,
                priority: 1,
                tasks_total: 2,
                tasks_done: 0,
                created_at: None,
            },
            AgentGoalSummary {
                goal_id: "GOAL-2".to_string(),
                title: "Goal 2".to_string(),
                status: AgentGoalStatus::Running,
                priority: 2,
                tasks_total: 1,
                tasks_done: 0,
                created_at: None,
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
        assert_eq!(goal_1_state.diff, None);

        // GOAL-2 branch does not exist
        let goal_2_state = snapshot.goals.get("GOAL-2").unwrap();
        assert_eq!(goal_2_state.branch, "agent-goal/GOAL-2");
        assert!(!goal_2_state.branch_exists);
        assert_eq!(goal_2_state.tip_sha, None);
        assert_eq!(goal_2_state.diff, None);

        // TASK-1 exists on disk
        let task_1_state = snapshot.tasks.get(&AgentTaskId::from("TASK-1")).unwrap();
        assert!(task_1_state.exists_on_disk);
        assert!(
            task_1_state
                .worktree_path
                .to_string_lossy()
                .contains("agent-task-TASK-1")
        );
        assert_eq!(task_1_state.branch, None);
        assert!(!task_1_state.branch_exists);
        assert_eq!(task_1_state.diff, None);

        // TASK-2 does not exist on disk
        let task_2_state = snapshot.tasks.get(&AgentTaskId::from("TASK-2")).unwrap();
        assert!(!task_2_state.exists_on_disk);
        assert!(
            task_2_state
                .worktree_path
                .to_string_lossy()
                .contains("agent-task-TASK-2")
        );
        assert_eq!(task_2_state.branch, None);
        assert!(!task_2_state.branch_exists);
        assert_eq!(task_2_state.diff, None);

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

    #[gpui::test]
    async fn test_snapshot_detects_task_branch(cx: &mut TestAppContext) {
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

            state.branches.insert("agent-goal/GOAL-1".into());
            state.refs.insert(
                "refs/heads/agent-goal/GOAL-1".into(),
                "goal-1-sha-200".into(),
            );

            state.branches.insert("agent-task/TASK-1".into());
            state.refs.insert(
                "refs/heads/agent-task/TASK-1".into(),
                "task-1-sha-300".into(),
            );
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let tasks = vec![AgentTaskSummary {
            id: AgentTaskId::from("TASK-1"),
            parent_id: None,
            goal_id: Some("GOAL-1".to_string()),
            title: "Task 1".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
        }];

        let goals = vec![AgentGoalSummary {
            goal_id: "GOAL-1".to_string(),
            title: "Goal 1".to_string(),
            status: AgentGoalStatus::Running,
            priority: 1,
            tasks_total: 1,
            tasks_done: 0,
            created_at: None,
        }];

        let snapshot = cx
            .update(|cx| snapshot(&project, &tasks, &goals, cx))
            .await
            .unwrap();

        let task_1_state = snapshot.tasks.get(&AgentTaskId::from("TASK-1")).unwrap();
        assert_eq!(task_1_state.branch, Some("agent-task/TASK-1".to_string()));
        assert!(task_1_state.branch_exists);
        assert_eq!(task_1_state.diff, None);
    }

    #[test]
    fn test_diff_short_stat_from_numstat() {
        let sample = "10\t5\tcrates/agent/src/lib.rs\n\
                      3\t0\tREADME.md\n\
                      -\t-\tassets/image.png\n\
                      0\t12\tsrc/old.rs\n";
        let stat = DiffShortStat::from_numstat(sample);
        assert_eq!(stat.files, 3);
        assert_eq!(stat.added, 13);
        assert_eq!(stat.removed, 17);

        // Empty output
        let empty_stat = DiffShortStat::from_numstat("");
        assert_eq!(empty_stat.files, 0);
        assert_eq!(empty_stat.added, 0);
        assert_eq!(empty_stat.removed, 0);

        // Binary files only
        let binary_stat = DiffShortStat::from_numstat("-\t-\tfoo.png\n-\t-\tbar.jpg\n");
        assert_eq!(binary_stat.files, 0);
        assert_eq!(binary_stat.added, 0);
        assert_eq!(binary_stat.removed, 0);
    }

    #[test]
    fn test_diff_cache_lookup_and_insert() {
        let cache = Arc::new(Mutex::new(HashMap::new()));
        let base = "sha-base-1";
        let head = "sha-head-1";

        assert_eq!(get_cached_diff(Some(&cache), base, head), None);

        let stat = DiffShortStat {
            files: 2,
            added: 15,
            removed: 4,
        };
        set_cached_diff(Some(&cache), base.to_string(), head.to_string(), stat);

        assert_eq!(get_cached_diff(Some(&cache), base, head), Some(stat));
    }

    #[gpui::test]
    async fn test_compute_diff_short_stat_cached() {
        let cache = Arc::new(Mutex::new(HashMap::new()));
        let base_sha = "base123";
        let head_sha = "head456";
        let expected = DiffShortStat {
            files: 5,
            added: 100,
            removed: 20,
        };
        set_cached_diff(
            Some(&cache),
            base_sha.to_string(),
            head_sha.to_string(),
            expected,
        );

        let non_existent_dir = std::path::Path::new("/does/not/exist");
        let result = compute_short_stat_for_diff(
            non_existent_dir,
            "base...head",
            Some(base_sha),
            Some(head_sha),
            Some(&cache),
        )
        .await;

        assert_eq!(result, Some(expected));

        // When not cached and dir does not exist, returns None
        let uncached = compute_short_stat_for_diff(
            non_existent_dir,
            "base...head",
            Some("other_base"),
            Some("other_head"),
            Some(&cache),
        )
        .await;
        assert_eq!(uncached, None);
    }

    #[test]
    fn test_refresh_error_chain_formatting() {
        let inner_err = anyhow::anyhow!("no git binary available");
        let outer_err = inner_err.context("opening repo at /test/.git");
        let formatted = format!("{outer_err:#}");
        assert!(formatted.contains("opening repo at /test/.git"));
        assert!(formatted.contains("no git binary available"));
    }

    #[gpui::test]
    async fn test_task_worktree_status_refresh_dirty_checking(cx: &mut TestAppContext) {
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

        let notify_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let notify_count_clone = notify_count.clone();
        cx.update(|cx| {
            cx.observe(&status_entity, move |_, _| {
                notify_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
            .detach();
        });

        let tasks = vec![AgentTaskSummary {
            id: AgentTaskId::from("TASK-1"),
            parent_id: None,
            goal_id: None,
            title: "Task 1".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
        }];

        let snap1 = status_entity
            .update(cx, |s, cx| s.refresh(tasks.clone(), vec![], cx))
            .await
            .unwrap();
        let notifies_after_first = notify_count.load(std::sync::atomic::Ordering::SeqCst);
        assert!(notifies_after_first > 0);

        let snap2 = status_entity
            .update(cx, |s, cx| s.refresh(tasks.clone(), vec![], cx))
            .await
            .unwrap();
        assert_eq!(snap1, snap2);
        assert_eq!(
            notify_count.load(std::sync::atomic::Ordering::SeqCst),
            notifies_after_first
        );
    }

    #[gpui::test]
    async fn test_task_worktree_status_refresh_if_needed(cx: &mut TestAppContext) {
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

        let tasks = vec![AgentTaskSummary {
            id: AgentTaskId::from("TASK-1"),
            parent_id: None,
            goal_id: Some("GOAL-1".to_string()),
            title: "Task 1".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
        }];
        let goals = vec![AgentGoalSummary {
            goal_id: "GOAL-1".to_string(),
            title: "Goal 1".to_string(),
            status: AgentGoalStatus::Running,
            priority: 1,
            tasks_total: 1,
            tasks_done: 0,
            created_at: None,
        }];

        let snap1 = status_entity
            .update(cx, |s, cx| s.refresh(tasks.clone(), goals.clone(), cx))
            .await
            .unwrap();

        // Mutate task status and title, but keep (task.id, task.goal_id) and goal.goal_id identical
        let mut modified_tasks = tasks.clone();
        modified_tasks[0].title = "Task 1 renamed".to_string();
        modified_tasks[0].status = AgentTaskStatus::Completed;

        let mut modified_goals = goals.clone();
        modified_goals[0].tasks_done = 1;

        // refresh_if_needed should return cached snapshot because IDs did not change
        let snap2 = status_entity
            .update(cx, |s, cx| {
                s.refresh_if_needed(modified_tasks.clone(), modified_goals.clone(), cx)
            })
            .await
            .unwrap();
        assert_eq!(snap1, snap2);

        // Adding a new task changes IDs, so refresh_if_needed should refresh
        modified_tasks.push(AgentTaskSummary {
            id: AgentTaskId::from("TASK-2"),
            parent_id: None,
            goal_id: Some("GOAL-1".to_string()),
            title: "Task 2".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
        });

        let snap3 = status_entity
            .update(cx, |s, cx| {
                s.refresh_if_needed(modified_tasks, modified_goals, cx)
            })
            .await
            .unwrap();
        assert_eq!(snap3.tasks.len(), 2);
    }

    #[gpui::test]
    async fn test_compute_diff_short_stat_timed_cache_when_sha_missing() {
        let test_dir = std::path::Path::new("/test/timed_cache_dir");
        let diff_spec = "main...feature";
        let expected = DiffShortStat {
            files: 3,
            added: 12,
            removed: 5,
        };

        set_timed_cached_diff(test_dir, diff_spec, Some(expected));

        let result =
            compute_short_stat_for_diff(test_dir, diff_spec, None, Some("head_sha"), None).await;

        assert_eq!(result, Some(expected));
    }
}
