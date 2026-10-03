use std::collections::HashMap;
#[cfg(any(test, feature = "test-support"))]
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use anyhow::{Context as _, Result};
use fs::Fs;
use git::repository::{AskPassDelegate, CommitOptions, CreateWorktreeTarget, RepoPath};
use gpui::{App, AsyncApp, Entity, Task};
use project::{
    Project, git_store::Repository, git_store::worktrees_directory_for_repo,
    project_settings::ProjectSettings,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::Settings;
use util::ResultExt;
use util::paths::PathStyle;

use crate::{AgentTaskId, AgentTaskSummary};

#[derive(Debug, thiserror::Error)]
#[error("no git repository found in project")]
pub struct NoGitRepositoryError;

fn system_git_binary() -> Option<PathBuf> {
    static CACHED_GIT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    CACHED_GIT.get_or_init(|| which::which("git").ok()).clone()
}

/// Resolves the repository a task worktree should be linked to, plus the data
/// needed to compute its path. Deterministic: prefer the repository backing
/// the project's primary worktree, then any non-linked (main) repository,
/// then any repository at all.
pub(crate) fn task_worktree_context(
    project: &Project,
    cx: &App,
) -> Result<(Entity<Repository>, PathBuf, PathStyle, String, Arc<dyn Fs>)> {
    let file_system = project.fs().clone();
    let repositories: Vec<Entity<Repository>> =
        project.repositories(cx).values().cloned().collect();

    let repository = project
        .visible_worktrees(cx)
        .next()
        .and_then(|worktree| {
            let primary_root = worktree.read(cx).abs_path();
            repositories.iter().find(|repository| {
                let work_directory = repository.read(cx).snapshot().work_directory_abs_path;
                primary_root.starts_with(work_directory.as_ref())
            })
        })
        .or_else(|| {
            repositories
                .iter()
                .find(|repository| !repository.read(cx).snapshot().is_linked_worktree())
        })
        .or_else(|| repositories.first())
        .ok_or_else(|| anyhow::anyhow!(NoGitRepositoryError))?
        .clone();

    let snapshot = repository.read(cx).snapshot();
    // Anchor the worktrees directory at the main checkout, mirroring
    // `Repository::path_for_new_linked_worktree`, so the base directory does
    // not depend on which linked worktree happens to be open.
    let anchor_path = snapshot
        .main_worktree_abs_path()
        .unwrap_or(snapshot.work_directory_abs_path.as_ref())
        .to_path_buf();
    let path_style = snapshot.path_style;
    let worktree_setting = ProjectSettings::get_global(cx)
        .git
        .worktree_directory
        .clone();

    Ok((
        repository,
        anchor_path,
        path_style,
        worktree_setting,
        file_system,
    ))
}

pub(crate) fn task_worktree_path(
    anchor_path: &PathBuf,
    worktree_setting: &str,
    path_style: PathStyle,
    task_id: &AgentTaskId,
) -> Result<PathBuf> {
    let worktrees_base = worktrees_directory_for_repo(anchor_path, worktree_setting, path_style)?;
    Ok(worktrees_base.join(format!("agent-task-{}", task_id)))
}

pub async fn resolve_ref_to_sha(
    git_repo: &Arc<dyn git::repository::GitRepository>,
    ref_name: &str,
) -> Result<Option<String>> {
    let revs = vec![
        ref_name.to_string(),
        format!("refs/heads/{ref_name}"),
        format!("refs/remotes/{ref_name}"),
    ];
    let shas = git_repo.revparse_batch(revs).await?;
    if let Some(sha) = shas.into_iter().flatten().next() {
        return Ok(Some(sha));
    }

    if let Ok(branches) = git_repo.branches().await {
        for branch in branches.branches {
            let matches = branch.ref_name.as_ref() == ref_name
                || branch
                    .ref_name
                    .strip_prefix("refs/heads/")
                    .unwrap_or(&branch.ref_name)
                    == ref_name;
            if matches {
                if let Some(commit) = branch.most_recent_commit {
                    return Ok(Some(commit.sha.to_string()));
                }
                return Ok(None);
            }
        }
    }

    Ok(None)
}

pub fn ensure_goal_branch(
    project: Entity<Project>,
    goal_id: &str,
    cx: &mut App,
) -> Task<Result<String>> {
    let goal_id = goal_id.to_string();
    cx.spawn(async move |cx| {
        let (repository, anchor_path, _, _, file_system) = project
            .update(cx, |project, cx| {
                anyhow::Ok(task_worktree_context(project, cx))
            })??;
        let dot_git = anchor_path.join(".git");
        let git_binary = system_git_binary();
        let git_repo = file_system
            .open_repo(&dot_git, git_binary.as_deref())
            .with_context(|| format!("opening repo at {}", dot_git.display()))?;

        let goal_branch = format!("agent-goal/{}", goal_id);
        let goal_ref = format!("refs/heads/{}", goal_branch);

        if let Ok(Some(sha)) = resolve_ref_to_sha(&git_repo, &goal_branch).await {
            record_goal_tip_sha(&goal_id, sha);
            return Ok(goal_branch);
        }

        // Branch doesn't exist: create ref from current HEAD of main repository
        let head_sha = match resolve_ref_to_sha(&git_repo, "HEAD").await? {
            Some(sha) => sha,
            None => git_repo
                .head_sha()
                .await
                .unwrap_or_else(|| "HEAD".to_string()),
        };

        let update_task = repository.update(cx, |repository, _| {
            repository.update_ref(goal_ref.clone(), head_sha.clone())
        });
        update_task.await??;
        record_goal_tip_sha(&goal_id, head_sha.clone());

        #[cfg(any(test, feature = "test-support"))]
        if file_system.is_fake() {
            file_system
                .as_fake()
                .with_git_state(&dot_git, true, |state| {
                    state.branches.insert(goal_branch.clone());
                    state.refs.insert(goal_ref.clone(), head_sha.clone());
                    state.refs.insert(goal_branch.clone(), head_sha.clone());
                })
                .ok();
        }

        log::info!("Created goal branch {goal_branch} at {head_sha}");
        Ok(goal_branch)
    })
}

#[cfg(any(test, feature = "test-support"))]
pub fn ensure_task_worktree(
    project: Entity<Project>,
    task: &AgentTaskSummary,
    cx: &mut App,
) -> Task<Result<PathBuf>> {
    ensure_task_worktree_with_policy(project, task, None, None, None, cx)
}

pub fn ensure_task_worktree_with_policy(
    project: Entity<Project>,
    task: &AgentTaskSummary,
    goal_id: Option<String>,
    base_branch: Option<String>,
    on_branch: Option<String>,
    cx: &mut App,
) -> Task<Result<PathBuf>> {
    let task_id = task.id.clone();
    let goal_id = goal_id.or_else(|| task.goal_id.clone());
    cx.spawn(async move |cx| {
        if on_branch.is_some() && base_branch.is_some() {
            anyhow::bail!("on_branch and base_branch are mutually exclusive");
        }
        if on_branch.as_deref() == Some("goal") && goal_id.is_none() {
            anyhow::bail!("on_branch 'goal' requires goal_id");
        }

        let (repository, anchor_path, path_style, worktree_setting, file_system) = project
            .update(cx, |project, cx| {
                anyhow::Ok(task_worktree_context(project, cx))
            })??;

        let dot_git = anchor_path.join(".git");
        let git_binary = system_git_binary();
        let git_repo = file_system
            .open_repo(&dot_git, git_binary.as_deref())
            .with_context(|| format!("opening repo at {}", dot_git.display()))?;

        let (target_checkout_branch, base_ref, base_sha_for_worktree, goal_branch) =
            if let Some(ref on_b) = on_branch {
                let branch_name = if on_b == "goal" {
                    let g_id = goal_id
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("on_branch 'goal' requires goal_id"))?;
                    let ensure_task =
                        project.update(cx, |_, cx| ensure_goal_branch(project.clone(), g_id, cx));
                    ensure_task.await?
                } else {
                    on_b.clone()
                };

                let sha = resolve_ref_to_sha(&git_repo, &branch_name).await?;
                let sha = sha.ok_or_else(|| anyhow::anyhow!("branch not found: {branch_name}"))?;

                let g_branch = if branch_name.starts_with("agent-goal/") {
                    Some(branch_name.clone())
                } else {
                    goal_id.as_ref().map(|g| format!("agent-goal/{g}"))
                };

                (Some(branch_name), None, Some(sha), g_branch)
            } else {
                // Base selection priority: explicit base_branch > goal-tip > HEAD main
                let (b_ref, b_sha, g_branch) = if let Some(ref base) = base_branch {
                    if let Some(extracted_goal_id) = base.strip_prefix("agent-goal/") {
                        let ensure_task = project.update(cx, |_, cx| {
                            ensure_goal_branch(project.clone(), extracted_goal_id, cx)
                        });
                        ensure_task.await?;
                    }
                    let sha = resolve_ref_to_sha(&git_repo, base).await?;
                    let base_sha =
                        sha.ok_or_else(|| anyhow::anyhow!("branch not found: {base}"))?;
                    let g_branch =
                        goal_id
                            .as_ref()
                            .map(|g| format!("agent-goal/{g}"))
                            .or_else(|| {
                                if base.starts_with("agent-goal/") {
                                    Some(base.clone())
                                } else {
                                    None
                                }
                            });
                    (Some(base.clone()), Some(base_sha), g_branch)
                } else if let Some(ref g_id) = goal_id {
                    let ensure_task =
                        project.update(cx, |_, cx| ensure_goal_branch(project.clone(), g_id, cx));
                    let g_branch = ensure_task.await?;
                    let sha = resolve_ref_to_sha(&git_repo, &g_branch)
                        .await?
                        .unwrap_or_else(|| "HEAD".to_string());
                    (Some(g_branch.clone()), Some(sha), Some(g_branch))
                } else {
                    (None, None, None)
                };
                (None, b_ref, b_sha, g_branch)
            };

        let branch_to_claim = target_checkout_branch
            .clone()
            .unwrap_or_else(|| format!("agent-task/{}", task_id));

        claim_branch_or_error(&branch_to_claim, &task_id, target_checkout_branch.clone())?;

        struct ClaimBranchGuard {
            task_id: AgentTaskId,
            defused: bool,
        }

        impl Drop for ClaimBranchGuard {
            fn drop(&mut self) {
                if !self.defused {
                    let mut reg = REGISTRY.write();
                    reg.entries.remove(&self.task_id);
                }
            }
        }

        let mut claim_guard = ClaimBranchGuard {
            task_id: task_id.clone(),
            defused: false,
        };

        let worktree_path =
            task_worktree_path(&anchor_path, &worktree_setting, path_style, &task_id)?;

        let path_exists = file_system.is_dir(&worktree_path).await;
        if !path_exists {
            let create_res = if let Some(ref branch_name) = target_checkout_branch {
                let branch_name = branch_name.clone();
                let create_task = repository.update(cx, |repository, _| {
                    repository.create_worktree(
                        CreateWorktreeTarget::ExistingBranch { branch_name },
                        worktree_path.clone(),
                    )
                });
                create_task.await?
            } else {
                let branch_name = format!("agent-task/{}", task_id);
                let create_task = repository.update(cx, |repository, _| {
                    repository.create_worktree(
                        CreateWorktreeTarget::NewBranch {
                            branch_name: branch_name.clone(),
                            base_sha: base_sha_for_worktree.clone(),
                        },
                        worktree_path.clone(),
                    )
                });

                let mut res = create_task.await?;
                if let Err(error) = res {
                    let error_message = error.to_string();
                    if error_message.contains("already exists") {
                        // The branch survives an earlier crashed run; check it
                        // out instead of creating it again.
                        let retry_task = repository.update(cx, |repository, _| {
                            repository.create_worktree(
                                CreateWorktreeTarget::ExistingBranch { branch_name },
                                worktree_path.clone(),
                            )
                        });
                        res = retry_task.await?;
                    } else {
                        res = Err(error);
                    }
                }
                res
            };

            if let Err(error) = create_res {
                unregister_task_worktree(&task_id);
                return Err(error);
            }
        }

        let find_task = project.update(cx, |project, cx| {
            project.find_or_create_worktree(&worktree_path, true, cx)
        });
        if let Err(e) = find_task.await {
            unregister_task_worktree(&task_id);
            return Err(e);
        }

        register_task_worktree_policy(
            &task_id,
            worktree_path.clone(),
            goal_branch.clone(),
            base_ref,
            base_sha_for_worktree.clone(),
            target_checkout_branch.clone(),
        );
        claim_guard.defused = true;

        #[cfg(any(test, feature = "test-support"))]
        if file_system.is_fake() {
            let mut initial_files = HashMap::new();
            if let Some(ref target_branch) = target_checkout_branch {
                if target_branch.starts_with("agent-goal/") {
                    let g_id = target_branch
                        .strip_prefix("agent-goal/")
                        .unwrap_or(target_branch);
                    initial_files = get_goal_files(g_id);
                } else if let Some(stripped) = target_branch.strip_prefix("agent-task/") {
                    let tid = AgentTaskId::from(stripped);
                    initial_files = get_task_recorded_files(&tid);
                    if initial_files.is_empty() {
                        if let Some(other_path) = task_worktree_path_for_id(&tid) {
                            if let Ok(items) =
                                fs::read_dir_items(file_system.as_ref(), &other_path).await
                            {
                                for (item_path, is_dir) in items {
                                    if !is_dir {
                                        if let Ok(rel) = item_path.strip_prefix(&other_path) {
                                            if !rel.starts_with(".git") {
                                                if let Ok(bytes) =
                                                    file_system.load_bytes(&item_path).await
                                                {
                                                    initial_files.insert(rel.to_path_buf(), bytes);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            } else if let Some(ref g_branch) = goal_branch {
                let g_id = g_branch.strip_prefix("agent-goal/").unwrap_or(g_branch);
                initial_files = get_goal_files(g_id);
            }

            for (rel_path, content) in &initial_files {
                let dest = worktree_path.join(rel_path);
                if let Some(parent) = dest.parent() {
                    file_system.create_dir(parent).await.log_err();
                }
                file_system.write(&dest, content).await.log_err();
            }

            if !initial_files.is_empty() {
                let worktree_dot_git = worktree_path.join(".git");
                file_system
                    .as_fake()
                    .with_git_state(&worktree_dot_git, true, |state| {
                        for (rel_path, content) in &initial_files {
                            if let Ok(rel) = util::rel_path::RelPath::new(rel_path, path_style) {
                                let repo_path = RepoPath::from_rel_path(&rel);
                                state
                                    .head_contents
                                    .insert(repo_path.clone(), content.clone());
                                state.index_contents.insert(repo_path, content.clone());
                            }
                        }
                    })
                    .log_err();
            }
        }

        Ok(worktree_path)
    })
}

#[derive(Default)]
struct TaskWorktreeRegistry {
    entries: HashMap<AgentTaskId, TaskWorktreeState>,
    active_sessions: HashMap<AgentTaskId, usize>,
    goal_history: HashMap<String, Vec<GoalMergeEntry>>,
    #[cfg(any(test, feature = "test-support"))]
    goal_files: HashMap<String, HashMap<PathBuf, Vec<u8>>>,
    #[cfg(any(test, feature = "test-support"))]
    task_files: HashMap<AgentTaskId, HashMap<PathBuf, Vec<u8>>>,
    goal_tip_shas: HashMap<String, String>,
    goal_graduations: HashMap<String, GoalGraduationSummary>,
}

#[derive(Clone, Debug)]
struct GoalMergeEntry {
    commit_sha: String,
    task_id: AgentTaskId,
    changed_files: Vec<String>,
}

#[derive(Default)]
struct TaskWorktreeState {
    worktree_path: PathBuf,
    commit_error: Option<String>,
    cleanup_error: Option<String>,
    is_terminal: bool,
    goal_branch: Option<String>,
    base_ref: Option<String>,
    base_sha: Option<String>,
    merge_conflict: Option<Vec<String>>,
    changed_files: Vec<String>,
    isolation_details: Option<SubagentIsolationDetails>,
    checkout_target: Option<String>,
}

impl TaskWorktreeState {
    fn checked_out_branch(&self, task_id: &AgentTaskId) -> String {
        self.checkout_target
            .clone()
            .unwrap_or_else(|| format!("agent-task/{}", task_id))
    }
}

static REGISTRY: LazyLock<parking_lot::RwLock<TaskWorktreeRegistry>> =
    LazyLock::new(|| parking_lot::RwLock::new(TaskWorktreeRegistry::default()));

static GOAL_MERGE_LOCKS: LazyLock<
    parking_lot::Mutex<HashMap<String, Arc<futures::lock::Mutex<()>>>>,
> = LazyLock::new(|| parking_lot::Mutex::new(HashMap::default()));

pub(crate) fn get_goal_merge_lock(goal_id: &str) -> Arc<futures::lock::Mutex<()>> {
    let mut locks = GOAL_MERGE_LOCKS.lock();
    locks.entry(goal_id.to_string()).or_default().clone()
}

pub fn reset_registry_for_tests() {
    let mut reg = REGISTRY.write();
    *reg = TaskWorktreeRegistry::default();
    let mut locks = GOAL_MERGE_LOCKS.lock();
    locks.clear();
}

pub fn register_task_worktree(task_id: &AgentTaskId, worktree_path: PathBuf) {
    register_task_worktree_policy(task_id, worktree_path, None, None, None, None);
}

pub fn register_task_worktree_policy(
    task_id: &AgentTaskId,
    worktree_path: PathBuf,
    goal_branch: Option<String>,
    base_ref: Option<String>,
    base_sha: Option<String>,
    checkout_target: Option<String>,
) {
    let mut reg = REGISTRY.write();
    let entry = reg
        .entries
        .entry(task_id.clone())
        .or_insert_with(|| TaskWorktreeState {
            worktree_path: worktree_path.clone(),
            commit_error: None,
            cleanup_error: None,
            is_terminal: false,
            goal_branch: goal_branch.clone(),
            base_ref: base_ref.clone(),
            base_sha: base_sha.clone(),
            merge_conflict: None,
            changed_files: Vec::new(),
            isolation_details: None,
            checkout_target: checkout_target.clone(),
        });
    entry.worktree_path = worktree_path;
    entry.goal_branch = goal_branch;
    entry.base_ref = base_ref;
    entry.base_sha = base_sha;
    entry.checkout_target = checkout_target;
}

pub fn claim_branch_or_error(
    branch: &str,
    task_id: &AgentTaskId,
    checkout_target: Option<String>,
) -> Result<()> {
    let mut reg = REGISTRY.write();
    for (other_task_id, entry) in &reg.entries {
        if other_task_id != task_id && !entry.is_terminal {
            let occupied = entry.checked_out_branch(other_task_id);
            if occupied == branch {
                anyhow::bail!(
                    "branch {branch} is checked out by task {other_task_id}; serialize or use on_branch after it completes"
                );
            }
        }
    }
    let entry = reg.entries.entry(task_id.clone()).or_default();
    *entry = TaskWorktreeState {
        checkout_target,
        is_terminal: false,
        ..Default::default()
    };
    Ok(())
}

pub fn task_worktree_checkout_target(task_id: &AgentTaskId) -> Option<String> {
    let reg = REGISTRY.read();
    reg.entries
        .get(task_id)
        .and_then(|e| e.checkout_target.clone())
}

pub fn task_worktree_goal_branch(task_id: &AgentTaskId) -> Option<String> {
    let reg = REGISTRY.read();
    reg.entries.get(task_id).and_then(|e| e.goal_branch.clone())
}

pub fn task_worktree_base_ref(task_id: &AgentTaskId) -> Option<String> {
    let reg = REGISTRY.read();
    reg.entries.get(task_id).and_then(|e| e.base_ref.clone())
}

pub fn task_worktree_base_sha(task_id: &AgentTaskId) -> Option<String> {
    let reg = REGISTRY.read();
    reg.entries.get(task_id).and_then(|e| e.base_sha.clone())
}

pub fn task_worktree_path_for_id(task_id: &AgentTaskId) -> Option<PathBuf> {
    let reg = REGISTRY.read();
    reg.entries.get(task_id).map(|e| e.worktree_path.clone())
}

pub fn record_task_worktree_changed_files(task_id: &AgentTaskId, files: Vec<String>) {
    let mut reg = REGISTRY.write();
    if let Some(entry) = reg.entries.get_mut(task_id) {
        if !files.is_empty() || entry.changed_files.is_empty() {
            entry.changed_files = files;
        }
    }
}

pub fn task_worktree_changed_files(task_id: &AgentTaskId) -> Vec<String> {
    let reg = REGISTRY.read();
    reg.entries
        .get(task_id)
        .map(|e| e.changed_files.clone())
        .unwrap_or_default()
}

pub fn record_task_worktree_merge_conflict(task_id: &AgentTaskId, conflict: Vec<String>) {
    let mut reg = REGISTRY.write();
    if let Some(entry) = reg.entries.get_mut(task_id) {
        entry.merge_conflict = Some(conflict);
    } else {
        reg.entries.insert(
            task_id.clone(),
            TaskWorktreeState {
                merge_conflict: Some(conflict),
                ..Default::default()
            },
        );
    }
}

pub fn clear_task_worktree_merge_conflict(task_id: &AgentTaskId) {
    let mut reg = REGISTRY.write();
    if let Some(entry) = reg.entries.get_mut(task_id) {
        entry.merge_conflict = None;
    }
}

pub fn task_worktree_merge_conflict(task_id: &AgentTaskId) -> Option<Vec<String>> {
    let reg = REGISTRY.read();
    reg.entries
        .get(task_id)
        .and_then(|e| e.merge_conflict.clone())
}

pub fn record_task_worktree_isolation_details(
    task_id: &AgentTaskId,
    details: SubagentIsolationDetails,
) {
    let mut reg = REGISTRY.write();
    if let Some(entry) = reg.entries.get_mut(task_id) {
        entry.isolation_details = Some(details);
    }
}

pub fn task_worktree_isolation_details(task_id: &AgentTaskId) -> Option<SubagentIsolationDetails> {
    let reg = REGISTRY.read();
    reg.entries
        .get(task_id)
        .and_then(|e| e.isolation_details.clone())
}

#[cfg(any(test, feature = "test-support"))]
pub fn store_goal_files(goal_id: &str, files: HashMap<PathBuf, Vec<u8>>) {
    let mut reg = REGISTRY.write();
    let entry = reg.goal_files.entry(goal_id.to_string()).or_default();
    for (path, content) in files {
        entry.insert(path, content);
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn get_goal_files(goal_id: &str) -> HashMap<PathBuf, Vec<u8>> {
    let reg = REGISTRY.read();
    reg.goal_files.get(goal_id).cloned().unwrap_or_default()
}

#[cfg(any(test, feature = "test-support"))]
pub fn record_task_recorded_files(task_id: &AgentTaskId, files: HashMap<PathBuf, Vec<u8>>) {
    let mut reg = REGISTRY.write();
    let entry = reg.task_files.entry(task_id.clone()).or_default();
    for (path, content) in files {
        entry.insert(path, content);
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn get_task_recorded_files(task_id: &AgentTaskId) -> HashMap<PathBuf, Vec<u8>> {
    let reg = REGISTRY.read();
    reg.task_files.get(task_id).cloned().unwrap_or_default()
}

#[cfg(test)]
pub fn get_goal_history_entries(goal_id: &str) -> Vec<(String, AgentTaskId, Vec<String>)> {
    let reg = REGISTRY.read();
    reg.goal_history
        .get(goal_id)
        .map(|entries| {
            entries
                .iter()
                .map(|e| {
                    (
                        e.commit_sha.clone(),
                        e.task_id.clone(),
                        e.changed_files.clone(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn record_goal_merge_entry(goal_id: &str, entry: GoalMergeEntry) {
    let mut reg = REGISTRY.write();
    reg.goal_history
        .entry(goal_id.to_string())
        .or_default()
        .push(entry);
}

#[cfg(any(test, feature = "test-support"))]
fn get_goal_changed_files_since(goal_id: &str, base_sha: Option<&str>) -> HashSet<String> {
    let reg = REGISTRY.read();
    let mut result = HashSet::new();
    if let Some(history) = reg.goal_history.get(goal_id) {
        let mut include = base_sha.is_none();
        for entry in history {
            if !include {
                if entry.commit_sha == base_sha.unwrap_or_default() {
                    include = true;
                    continue;
                }
            }
            if include {
                for f in &entry.changed_files {
                    result.insert(f.clone());
                }
            }
        }
        if !include {
            for entry in history {
                for f in &entry.changed_files {
                    result.insert(f.clone());
                }
            }
        }
    }
    result
}

pub fn unregister_task_worktree(task_id: &AgentTaskId) {
    let mut reg = REGISTRY.write();
    reg.entries.remove(task_id);
}

pub fn record_task_worktree_commit_error(task_id: &AgentTaskId, error: String) {
    let mut reg = REGISTRY.write();
    if let Some(entry) = reg.entries.get_mut(task_id) {
        entry.commit_error = Some(error);
    } else {
        reg.entries.insert(
            task_id.clone(),
            TaskWorktreeState {
                commit_error: Some(error),
                ..Default::default()
            },
        );
    }
}

pub fn clear_task_worktree_commit_error(task_id: &AgentTaskId) {
    let mut reg = REGISTRY.write();
    if let Some(entry) = reg.entries.get_mut(task_id) {
        entry.commit_error = None;
    }
}

pub fn task_worktree_commit_error(task_id: &AgentTaskId) -> Option<String> {
    let reg = REGISTRY.read();
    reg.entries
        .get(task_id)
        .and_then(|e| e.commit_error.clone())
}

pub fn record_task_worktree_cleanup_error(task_id: &AgentTaskId, error: String) {
    let mut reg = REGISTRY.write();
    if let Some(entry) = reg.entries.get_mut(task_id) {
        entry.cleanup_error = Some(error);
    } else {
        reg.entries.insert(
            task_id.clone(),
            TaskWorktreeState {
                cleanup_error: Some(error),
                ..Default::default()
            },
        );
    }
}

pub fn clear_task_worktree_cleanup_error(task_id: &AgentTaskId) {
    let mut reg = REGISTRY.write();
    if let Some(entry) = reg.entries.get_mut(task_id) {
        entry.cleanup_error = None;
    }
}

pub fn task_worktree_cleanup_error(task_id: &AgentTaskId) -> Option<String> {
    let reg = REGISTRY.read();
    reg.entries
        .get(task_id)
        .and_then(|e| e.cleanup_error.clone())
}

pub fn mark_task_terminal(task_id: &AgentTaskId) {
    let mut reg = REGISTRY.write();
    if let Some(entry) = reg.entries.get_mut(task_id) {
        entry.is_terminal = true;
    } else {
        reg.entries.insert(
            task_id.clone(),
            TaskWorktreeState {
                is_terminal: true,
                ..Default::default()
            },
        );
    }
}

pub fn is_task_terminal(task_id: &AgentTaskId) -> bool {
    let reg = REGISTRY.read();
    reg.entries.get(task_id).map_or(false, |e| e.is_terminal)
}

pub fn register_subagent_session(task_id: &AgentTaskId) {
    let mut reg = REGISTRY.write();
    *reg.active_sessions.entry(task_id.clone()).or_insert(0) += 1;
}

pub fn unregister_subagent_session(task_id: &AgentTaskId) {
    let mut reg = REGISTRY.write();
    if let Some(count) = reg.active_sessions.get_mut(task_id) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            reg.active_sessions.remove(task_id);
        }
    }
}

pub fn has_active_subagent_session(task_id: &AgentTaskId) -> bool {
    let reg = REGISTRY.read();
    reg.active_sessions.get(task_id).copied().unwrap_or(0) > 0
}

pub struct SubagentSessionGuard(AgentTaskId);

impl SubagentSessionGuard {
    pub fn new(task_id: AgentTaskId) -> Self {
        register_subagent_session(&task_id);
        Self(task_id)
    }
}

impl Drop for SubagentSessionGuard {
    fn drop(&mut self) {
        unregister_subagent_session(&self.0);
    }
}

fn remove_task_worktree_internal(
    project: Entity<Project>,
    task_id: &AgentTaskId,
    force: bool,
    cx: &mut App,
) -> Task<Result<()>> {
    let task_id = task_id.clone();
    cx.spawn(async move |cx| {
        let (repository, anchor_path, path_style, worktree_setting, file_system) = project
            .update(cx, |project, cx| {
                anyhow::Ok(task_worktree_context(project, cx))
            })??;
        let worktree_path =
            task_worktree_path(&anchor_path, &worktree_setting, path_style, &task_id)?;

        if file_system.is_dir(&worktree_path).await {
            let remove_task = repository.update(cx, |repository, _| {
                repository.remove_worktree(worktree_path.clone(), force)
            });
            remove_task.await??;
        }

        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .find(|w| {
                    let w_abs = w.read(cx).abs_path();
                    w_abs.as_ref() == worktree_path
                        || util::paths::normalize_lexically(w_abs.as_ref())
                            .ok()
                            .as_deref()
                            == util::paths::normalize_lexically(&worktree_path)
                                .ok()
                                .as_deref()
                })
                .map(|w| w.read(cx).id())
        });
        if let Some(id) = worktree_id {
            project.update(cx, |project, cx| {
                project.remove_worktree(id, cx);
            });
        }

        Ok(())
    })
}

pub fn remove_task_worktree(
    project: Entity<Project>,
    task_id: &AgentTaskId,
    cx: &mut App,
) -> Task<Result<()>> {
    clear_task_worktree_commit_error(task_id);
    clear_task_worktree_cleanup_error(task_id);
    clear_task_worktree_merge_conflict(task_id);
    let task_id_clone = task_id.clone();
    let task = remove_task_worktree_internal(project, task_id, false, cx);
    cx.spawn(async move |_cx| {
        task.await?;
        unregister_task_worktree(&task_id_clone);
        Ok(())
    })
}

async fn prune_git_worktrees(anchor_path: &std::path::Path) {
    if let Some(git_binary) = system_git_binary() {
        let mut command = util::command::new_command(git_binary);
        command.args(["worktree", "prune"]);
        command.current_dir(anchor_path);
        match command.output().await {
            Ok(output) if !output.status.success() => {
                log::warn!(
                    "git worktree prune failed in {}: {}",
                    anchor_path.display(),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Err(error) => {
                log::warn!(
                    "failed to execute git worktree prune in {}: {error}",
                    anchor_path.display()
                );
            }
            _ => {}
        }
    }
}

pub fn remove_orphan_worktree(
    project: Entity<Project>,
    path: std::path::PathBuf,
    cx: &mut App,
) -> Task<Result<()>> {
    cx.spawn(async move |cx| {
        let (repository, anchor_path, _path_style, _worktree_setting, file_system) = project
            .update(cx, |project, cx| {
                anyhow::Ok(task_worktree_context(project, cx))
            })??;

        if file_system.is_dir(&path).await {
            let remove_task = repository.update(cx, |repository, _| {
                repository.remove_worktree(path.clone(), true)
            });
            let git_remove_result = match remove_task.await {
                Ok(inner_result) => inner_result,
                Err(canceled) => Err(anyhow::anyhow!("remove_worktree task canceled: {canceled}")),
            };

            let directory_still_exists = file_system.is_dir(&path).await;
            if let Err(git_error) = git_remove_result {
                log::warn!(
                    "git remove_worktree for orphan at {} failed: {git_error:#}; attempting fallback filesystem removal",
                    path.display()
                );
                if directory_still_exists {
                    file_system
                        .remove_dir(
                            &path,
                            fs::RemoveOptions {
                                recursive: true,
                                ignore_if_not_exists: true,
                            },
                        )
                        .await
                        .with_context(|| {
                            format!(
                                "failed to remove orphan worktree directory at {}: git remove failed ({:#}) and filesystem removal failed",
                                path.display(),
                                git_error
                            )
                        })?;

                    prune_git_worktrees(&anchor_path).await;
                }
            } else if directory_still_exists {
                file_system
                    .remove_dir(
                        &path,
                        fs::RemoveOptions {
                            recursive: true,
                            ignore_if_not_exists: true,
                        },
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "failed to remove orphan worktree directory at {}",
                            path.display()
                        )
                    })?;

                prune_git_worktrees(&anchor_path).await;
            }
        }

        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .find(|w| {
                    let w_abs = w.read(cx).abs_path();
                    w_abs.as_ref() == path
                        || util::paths::normalize_lexically(w_abs.as_ref())
                            .ok()
                            .as_deref()
                            == util::paths::normalize_lexically(&path).ok().as_deref()
                })
                .map(|w| w.read(cx).id())
        });
        if let Some(id) = worktree_id {
            project.update(cx, |project, cx| {
                project.remove_worktree(id, cx);
            });
        }

        Ok(())
    })
}

pub fn auto_cleanup_task_worktree(
    project: Entity<Project>,
    task_id: &AgentTaskId,
    cx: &mut App,
) -> Task<Result<bool>> {
    if let Some(err) = task_worktree_commit_error(task_id) {
        log::info!("Safeguard: retaining worktree for {task_id} due to commit error: {err}");
        return Task::ready(Ok(false));
    }

    if let Some(conflicts) = task_worktree_merge_conflict(task_id) {
        log::info!(
            "Safeguard: retaining worktree for {task_id} due to merge conflict: {conflicts:?}"
        );
        return Task::ready(Ok(false));
    }

    if has_active_subagent_session(task_id) {
        log::info!("Safeguard: retaining worktree for {task_id} due to active subagent session");
        return Task::ready(Ok(false));
    }

    let task_id_clone = task_id.clone();
    let remove_task = remove_task_worktree_internal(project, task_id, false, cx);
    cx.spawn(async move |_cx| {
        if let Err(e) = remove_task.await {
            let err_msg = e.to_string();
            record_task_worktree_cleanup_error(&task_id_clone, err_msg);
            return Err(e);
        }
        unregister_task_worktree(&task_id_clone);
        log::info!("Auto-cleaned worktree for terminal task {task_id_clone}");
        Ok(true)
    })
}

pub fn startup_sweep(
    project: Entity<Project>,
    tasks: Option<&[AgentTaskSummary]>,
    cx: &mut App,
) -> Task<Result<Vec<AgentTaskId>>> {
    let Some(tasks) = tasks else {
        log::info!("Startup sweep: task status unavailable (TGS not connected), fail-open skip");
        return Task::ready(Ok(Vec::new()));
    };

    let tasks = tasks.to_vec();
    cx.spawn(async move |cx| {
        let (_, anchor_path, path_style, worktree_setting, file_system) = project
            .update(cx, |project, cx| {
                anyhow::Ok(task_worktree_context(project, cx))
            })??;

        let base_dir = worktrees_directory_for_repo(&anchor_path, &worktree_setting, path_style)?;
        if !file_system.is_dir(&base_dir).await {
            return Ok(Vec::new());
        }

        let mut swept = Vec::new();
        for task in tasks {
            if !task.status.is_terminal() {
                continue;
            }

            let task_id = &task.id;

            if task_worktree_commit_error(task_id).is_some() {
                log::info!("Startup sweep: skipping {task_id} due to commit error");
                continue;
            }
            if task_worktree_merge_conflict(task_id).is_some() {
                log::info!("Startup sweep: skipping {task_id} due to merge conflict");
                continue;
            }
            if has_active_subagent_session(task_id) {
                log::info!("Startup sweep: skipping {task_id} due to active session");
                continue;
            }

            let worktree_path =
                task_worktree_path(&anchor_path, &worktree_setting, path_style, task_id)?;
            if file_system.is_dir(&worktree_path).await {
                let remove_task = project.update(cx, |_, cx| {
                    remove_task_worktree_internal(project.clone(), task_id, false, cx)
                });
                if let Err(e) = remove_task.await {
                    log::warn!("Startup sweep: failed to remove {task_id}: {e}");
                } else {
                    unregister_task_worktree(task_id);
                    swept.push(task_id.clone());
                }
            }
        }

        Ok(swept)
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct MergedInto {
    #[serde(rename = "ref")]
    pub target_ref: String,
    pub sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct MergeConflict {
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    NoGoal,
    FastForward { target_ref: String, sha: String },
    Merged { target_ref: String, sha: String },
    Conflict(MergeConflict),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct TaskRefArtifact {
    pub branch: String,
    pub head_sha: Option<String>,
    #[serde(default)]
    pub merged_into: Option<MergedInto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_conflict: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct SubagentIsolationDetails {
    pub mode: String,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    pub head_sha: Option<String>,
    pub changed_files: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_error: Option<String>,
    #[serde(default)]
    pub merged_into: Option<MergedInto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_conflict: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct GoalRefs {
    pub goal_branch: String,
    pub goal_ref: String,
    pub base_branch: String,
    pub base_ref: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct UnmergedConflict {
    pub task_id: String,
    pub branch: String,
    pub conflict_files: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct PrProposal {
    pub title: String,
    pub body: String,
    pub source_branch: String,
    pub target_branch: String,
    pub command: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct GoalGraduationSummary {
    pub goal_id: String,
    pub goal_branch: String,
    pub base_branch: String,
    pub refs: GoalRefs,
    pub tip_sha: Option<String>,
    pub commits_count: usize,
    pub merged_tasks: Vec<String>,
    pub unmerged_conflicts: Vec<UnmergedConflict>,
    pub graduation_blocked: bool,
    pub pr_proposal: PrProposal,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct GoalBranchSummary {
    pub goal_id: String,
    pub branch: String,
    pub tip_sha: Option<String>,
    pub merged_tasks_count: usize,
    pub conflicts: Option<Vec<String>>,
    pub has_conflict: bool,
}

pub async fn commit_task_worktree(
    fs: Arc<dyn Fs>,
    task_worktree: PathBuf,
    task_id: String,
    is_error: bool,
    cx: &mut AsyncApp,
) -> Result<SubagentIsolationDetails> {
    let dot_git = task_worktree.join(".git");
    let git_binary = system_git_binary();
    let git_repo = fs
        .open_repo(&dot_git, git_binary.as_deref())
        .with_context(|| format!("opening repo at {}", dot_git.display()))?;

    let task_id_typed = AgentTaskId::from(task_id.clone());
    let checkout_target = task_worktree_checkout_target(&task_id_typed);
    let is_checkout = checkout_target.is_some();
    let branch = checkout_target
        .clone()
        .unwrap_or_else(|| format!("agent-task/{}", task_id));
    let base_branch = if is_checkout {
        None
    } else {
        task_worktree_base_ref(&task_id_typed)
    };
    let mode = if is_checkout { "checkout" } else { "worktree" };

    let prefix = RepoPath::from_rel_path(&util::rel_path::RelPath::empty());

    let status = match git_repo.status(&[prefix]).await {
        Ok(status) => status,
        Err(e) => {
            let head_sha = git_repo.head_sha().await;
            return Ok(SubagentIsolationDetails {
                mode: mode.to_string(),
                branch,
                base_branch,
                head_sha,
                changed_files: 0,
                commit_error: Some(e.to_string()),
                merged_into: None,
                merge_conflict: None,
            });
        }
    };

    if status.entries.is_empty() {
        let head_sha = git_repo.head_sha().await;
        // Snapshot existing worktree files if any
        #[cfg(any(test, feature = "test-support"))]
        if let Ok(items) = fs::read_dir_items(fs.as_ref(), &task_worktree).await {
            let mut scanned = HashMap::new();
            for (item_path, is_dir) in items {
                if !is_dir {
                    if let Ok(rel) = item_path.strip_prefix(&task_worktree) {
                        if !rel.starts_with(".git") {
                            if let Ok(bytes) = fs.load_bytes(&item_path).await {
                                scanned.insert(rel.to_path_buf(), bytes);
                            }
                        }
                    }
                }
            }
            if !scanned.is_empty() {
                record_task_recorded_files(&task_id_typed, scanned);
            }
        }
        return Ok(SubagentIsolationDetails {
            mode: mode.to_string(),
            branch,
            base_branch,
            head_sha,
            changed_files: 0,
            commit_error: None,
            merged_into: None,
            merge_conflict: None,
        });
    }

    let changed_files = status.entries.len();
    let paths: Vec<RepoPath> = status
        .entries
        .iter()
        .map(|(path, _)| path.clone())
        .collect();

    let file_strings: Vec<String> = paths.iter().map(|p| p.as_unix_str().to_string()).collect();
    record_task_worktree_changed_files(&task_id_typed, file_strings);

    #[cfg(any(test, feature = "test-support"))]
    {
        let mut files_snapshot = HashMap::new();
        for p in &paths {
            let full_path = task_worktree.join(p.as_std_path());
            if let Ok(bytes) = fs.load_bytes(&full_path).await {
                files_snapshot.insert(p.as_std_path().to_path_buf(), bytes);
            }
        }
        if !files_snapshot.is_empty() {
            record_task_recorded_files(&task_id_typed, files_snapshot.clone());
            if is_checkout && branch.starts_with("agent-goal/") {
                let g_id = branch.strip_prefix("agent-goal/").unwrap_or(&branch);
                store_goal_files(g_id, files_snapshot);
            }
        }
    }

    if let Err(e) = git_repo
        .stage_paths(paths, Arc::new(HashMap::default()))
        .await
    {
        let head_sha = git_repo.head_sha().await;
        return Ok(SubagentIsolationDetails {
            mode: mode.to_string(),
            branch,
            base_branch,
            head_sha,
            changed_files: 0,
            commit_error: Some(format!("staging failed: {e}")),
            merged_into: None,
            merge_conflict: None,
        });
    }

    let commit_message = if is_error {
        format!("WIP: {branch} error checkpoint")
    } else {
        format!("{branch}: completed task work")
    };

    let askpass = AskPassDelegate::new(cx, |_, _, _| {});

    if let Err(e) = git_repo
        .commit(
            commit_message.into(),
            None,
            CommitOptions::default(),
            askpass,
            Arc::new(HashMap::default()),
        )
        .await
    {
        let head_sha = git_repo.head_sha().await;
        return Ok(SubagentIsolationDetails {
            mode: mode.to_string(),
            branch,
            base_branch,
            head_sha,
            changed_files: 0,
            commit_error: Some(format!("commit failed: {e}")),
            merged_into: None,
            merge_conflict: None,
        });
    }

    let head_sha = git_repo.head_sha().await;
    #[cfg(any(test, feature = "test-support"))]
    let head_sha = if fs.is_fake() {
        head_sha.map(|sha| {
            if sha.starts_with("fake-commit-") {
                format!("{sha}-{task_id}")
            } else {
                sha
            }
        })
    } else {
        head_sha
    };

    #[cfg(any(test, feature = "test-support"))]
    if let Some(ref sha) = head_sha {
        if fs.is_fake() {
            {
                let task_ref = format!("refs/heads/{}", branch);
                fs.as_fake()
                    .with_git_state(&dot_git, true, |state| {
                        state.refs.insert("HEAD".into(), sha.clone());
                        state.refs.insert(task_ref.clone(), sha.clone());
                        state.refs.insert(branch.clone(), sha.clone());
                    })
                    .ok();

                if let Ok(content) = fs.load(&dot_git).await {
                    if let Some(gitdir) = content.trim().strip_prefix("gitdir:") {
                        let worktrees_entry_dir = PathBuf::from(gitdir.trim());
                        let commondir_file = worktrees_entry_dir.join("commondir");
                        if let Ok(commondir_content) = fs.load(&commondir_file).await {
                            let common_dot_git = PathBuf::from(commondir_content.trim());
                            fs.as_fake()
                                .with_git_state(&common_dot_git, true, |state| {
                                    state.refs.insert(task_ref.clone(), sha.clone());
                                    state.refs.insert(branch.clone(), sha.clone());
                                })
                                .ok();
                        }
                    }
                }
            }
        }
    }

    if let Some(ref sha) = head_sha {
        if let Some(goal_id) = branch.strip_prefix("agent-goal/") {
            record_goal_tip_sha(goal_id, sha.clone());
        }
    }

    Ok(SubagentIsolationDetails {
        mode: mode.to_string(),
        branch,
        base_branch,
        head_sha,
        changed_files,
        commit_error: None,
        merged_into: None,
        merge_conflict: None,
    })
}

pub fn merge_task_branch_into_goal(
    project: Entity<Project>,
    task_id: &AgentTaskId,
    cx: &mut App,
) -> Task<Result<MergeOutcome>> {
    let task_id = task_id.clone();
    cx.spawn(async move |cx| {
        let goal_branch = task_worktree_goal_branch(&task_id);
        let Some(goal_branch) = goal_branch else {
            return Ok(MergeOutcome::NoGoal);
        };

        let goal_id = goal_branch
            .strip_prefix("agent-goal/")
            .unwrap_or(&goal_branch)
            .to_string();

        let goal_lock = get_goal_merge_lock(&goal_id);
        let _guard = goal_lock.lock().await;

        let (repository, anchor_path, _, _, file_system) = project
            .update(cx, |project, cx| {
                anyhow::Ok(task_worktree_context(project, cx))
            })??;

        let dot_git = anchor_path.join(".git");
        let git_binary = system_git_binary();
        let git_repo = file_system
            .open_repo(&dot_git, git_binary.as_deref())
            .with_context(|| format!("opening repo at {}", dot_git.display()))?;

        let goal_ref = format!("refs/heads/{}", goal_branch);
        let task_branch = format!("agent-task/{}", task_id);

        let task_sha = resolve_ref_to_sha(&git_repo, &task_branch)
            .await?
            .ok_or_else(|| anyhow::anyhow!("task branch {} not found", task_branch))?;

        let goal_sha = resolve_ref_to_sha(&git_repo, &goal_branch)
            .await?
            .ok_or_else(|| anyhow::anyhow!("goal branch {} not found", goal_branch))?;

        if task_sha == goal_sha {
            return Ok(MergeOutcome::FastForward {
                target_ref: goal_ref,
                sha: task_sha,
            });
        }

        let base_sha = task_worktree_base_sha(&task_id);
        let task_files = task_worktree_changed_files(&task_id);

        let is_fast_forward = match &base_sha {
            Some(base) => base == &goal_sha,
            None => {
                let reg = REGISTRY.read();
                reg.goal_history
                    .get(&goal_id)
                    .map_or(true, |h| h.is_empty())
            }
        };

        if is_fast_forward {
            // Fast-forward case: merge-base == goal-tip.
            // Atomic update_ref without checkout.
            let update_task = repository.update(cx, |repository, _| {
                repository.update_ref(goal_ref.clone(), task_sha.clone())
            });
            update_task.await??;

            #[cfg(any(test, feature = "test-support"))]
            if file_system.is_fake() {
                file_system
                    .as_fake()
                    .with_git_state(&dot_git, true, |state| {
                        state.refs.insert(goal_ref.clone(), task_sha.clone());
                        state.refs.insert(goal_branch.clone(), task_sha.clone());
                    })
                    .log_err();
            }

            #[cfg(any(test, feature = "test-support"))]
            {
                let files = get_task_recorded_files(&task_id);
                if !files.is_empty() {
                    store_goal_files(&goal_id, files);
                }
            }

            record_goal_tip_sha(&goal_id, task_sha.clone());
            record_goal_merge_entry(
                &goal_id,
                GoalMergeEntry {
                    commit_sha: task_sha.clone(),
                    task_id: task_id.clone(),
                    changed_files: task_files.clone(),
                },
            );

            log::info!(
                "Fast-forward merged {} into {} (tip: {})",
                task_branch,
                goal_branch,
                task_sha
            );

            return Ok(MergeOutcome::FastForward {
                target_ref: goal_ref,
                sha: task_sha,
            });
        }

        // Non-fast-forward: another task merged into goal branch earlier.
        #[cfg(any(test, feature = "test-support"))]
        if file_system.is_fake() {
            let goal_changed_files_since_base =
                get_goal_changed_files_since(&goal_id, base_sha.as_deref());
            let mut detected_conflicts: Vec<String> = Vec::new();
            for file in &task_files {
                if goal_changed_files_since_base.contains(file) {
                    detected_conflicts.push(file.clone());
                }
            }

            if !detected_conflicts.is_empty() {
                file_system
                    .as_fake()
                    .with_git_state(&dot_git, true, |state| {
                        state
                            .simulated_merge_conflicts
                            .insert((goal_sha.clone(), task_sha.clone()), detected_conflicts);
                    })
                    .log_err();
            }
        }

        let plumbing_outcome = git_repo
            .merge_tree(goal_sha.clone(), task_sha.clone())
            .await?;

        let merge_sha = match plumbing_outcome {
            git::repository::MergeTreeResult::Conflict { conflicts } => {
                log::warn!(
                    "Merge conflict detected merging {} into {}: {:?}",
                    task_branch,
                    goal_branch,
                    conflicts
                );
                record_task_worktree_merge_conflict(&task_id, conflicts.clone());
                return Ok(MergeOutcome::Conflict(MergeConflict { files: conflicts }));
            }
            git::repository::MergeTreeResult::Clean { tree_sha } => {
                let commit_message = format!("Merge task {} into goal {}", task_id, goal_id);
                git_repo
                    .commit_tree(
                        tree_sha,
                        vec![goal_sha.clone(), task_sha.clone()],
                        commit_message,
                    )
                    .await?
            }
        };

        let update_task = repository.update(cx, |repository, _| {
            repository.update_ref(goal_ref.clone(), merge_sha.clone())
        });
        update_task.await??;

        #[cfg(any(test, feature = "test-support"))]
        if file_system.is_fake() {
            file_system
                .as_fake()
                .with_git_state(&dot_git, true, |state| {
                    state.refs.insert(goal_ref.clone(), merge_sha.clone());
                    state.refs.insert(goal_branch.clone(), merge_sha.clone());
                })
                .log_err();
        }

        #[cfg(any(test, feature = "test-support"))]
        {
            let files = get_task_recorded_files(&task_id);
            if !files.is_empty() {
                store_goal_files(&goal_id, files);
            }
        }

        record_goal_tip_sha(&goal_id, merge_sha.clone());
        record_goal_merge_entry(
            &goal_id,
            GoalMergeEntry {
                commit_sha: merge_sha.clone(),
                task_id: task_id.clone(),
                changed_files: task_files.clone(),
            },
        );

        log::info!(
            "Non-FF merged {} into {} with merge commit {} (parents: [{}, {}])",
            task_branch,
            goal_branch,
            merge_sha,
            goal_sha,
            task_sha
        );

        Ok(MergeOutcome::Merged {
            target_ref: goal_ref,
            sha: merge_sha,
        })
    })
}

pub fn publish_task_ref_artifact_to_tgs(
    project: Entity<Project>,
    task_id: &AgentTaskId,
    details: &SubagentIsolationDetails,
    cx: &mut App,
) -> Task<Result<Option<String>>> {
    let artifact = TaskRefArtifact {
        branch: details.branch.clone(),
        head_sha: details.head_sha.clone(),
        merged_into: details.merged_into.clone(),
        merge_conflict: details.merge_conflict.clone(),
    };
    let task_id_str = task_id.to_string();
    let content = match serde_json::to_string(&artifact) {
        Ok(c) => c,
        Err(e) => {
            return Task::ready(Err(anyhow::anyhow!(
                "failed to serialize TaskRefArtifact: {e}"
            )));
        }
    };

    let server_store = project.read(cx).context_server_store();
    let server_id = agent_settings::AgentSettings::get_for_project(project.read(cx), cx)
        .task_graph_server_id
        .clone();
    let server = server_store
        .read(cx)
        .get_running_server(&context_server::ContextServerId(server_id.clone().into()));

    let Some(server) = server else {
        return Task::ready(Err(anyhow::anyhow!(
            "No running task graph server found with ID '{server_id}' to publish TaskRefArtifact for {task_id_str}"
        )));
    };

    cx.spawn(async move |_cx| {
        let Some(client) = server.client() else {
            return Err(anyhow::anyhow!(
                "Task graph server '{server_id}' has no client connected to publish TaskRefArtifact for {task_id_str}"
            ));
        };

        let mut args = serde_json::Map::new();
        args.insert(
            "task_id".to_string(),
            serde_json::Value::String(task_id_str.clone()),
        );
        args.insert(
            "kind".to_string(),
            serde_json::Value::String("task_ref".to_string()),
        );
        args.insert(
            "summary".to_string(),
            serde_json::Value::String(format!("Task ref for {task_id_str}")),
        );
        args.insert("content".to_string(), serde_json::Value::String(content));

        let response = client
            .request::<context_server::types::requests::CallTool>(
                context_server::types::CallToolParams {
                    name: "artifact_publish".into(),
                    arguments: Some(serde_json::Value::Object(args)),
                    meta: None,
                },
            )
            .await;

        match response {
            Ok(resp) => {
                if resp.is_error == Some(true) {
                    let err_msg: String = resp.content.iter().filter_map(|c| c.text()).collect();
                    log::warn!("TGS artifact_publish returned error for {task_id_str}: {err_msg}");
                    Err(anyhow::anyhow!(
                        "TGS artifact_publish returned error for {task_id_str}: {err_msg}"
                    ))
                } else {
                    let text: String = resp.content.iter().filter_map(|c| c.text()).collect();
                    log::info!("Published TaskRefArtifact for {task_id_str}: {text}");
                    Ok(Some(text))
                }
            }
            Err(e) => {
                log::warn!("Failed to publish TaskRefArtifact for {task_id_str}: {e}");
                Err(e)
            }
        }
    })
}

pub fn fallback_isolation_details(
    task_id: &AgentTaskId,
    default_mode: &str,
    commit_error: Option<String>,
    merge_conflict: Option<Vec<String>>,
) -> SubagentIsolationDetails {
    let checkout_target = task_worktree_checkout_target(task_id);
    let is_checkout = checkout_target.is_some();
    let branch = checkout_target.unwrap_or_else(|| format!("agent-task/{task_id}"));
    let base_branch = if is_checkout {
        None
    } else {
        task_worktree_base_ref(task_id)
    };
    SubagentIsolationDetails {
        mode: if is_checkout {
            "checkout".to_string()
        } else {
            default_mode.to_string()
        },
        branch,
        base_branch,
        head_sha: None,
        changed_files: 0,
        commit_error,
        merged_into: None,
        merge_conflict,
    }
}

pub fn commit_and_merge_task_worktree(
    project: Entity<Project>,
    task_id: &AgentTaskId,
    is_error: bool,
    cx: &mut App,
) -> Task<Result<SubagentIsolationDetails>> {
    let task_id = task_id.clone();
    cx.spawn(async move |cx| {
        let (worktree_path, fs) = {
            let path_opt = task_worktree_path_for_id(&task_id);
            let fs = cx.update(|cx| project.read(cx).fs().clone());
            match (path_opt, fs) {
                (Some(p), fs) => (p, fs),
                _ => {
                    return Ok(fallback_isolation_details(
                        &task_id,
                        "none",
                        task_worktree_commit_error(&task_id),
                        task_worktree_merge_conflict(&task_id),
                    ));
                }
            }
        };

        if let Some(err) = task_worktree_commit_error(&task_id) {
            return Ok(fallback_isolation_details(
                &task_id,
                "worktree",
                Some(err),
                None,
            ));
        }

        if !fs.is_dir(&worktree_path).await {
            return Ok(fallback_isolation_details(
                &task_id,
                "none",
                None,
                task_worktree_merge_conflict(&task_id),
            ));
        }

        let mut details = commit_task_worktree(
            fs.clone(),
            worktree_path.clone(),
            task_id.to_string(),
            is_error,
            cx,
        )
        .await?;

        if let Some(ref err) = details.commit_error {
            record_task_worktree_commit_error(&task_id, err.clone());
            return Ok(details);
        } else {
            clear_task_worktree_commit_error(&task_id);
        }

        let checkout_target = task_worktree_checkout_target(&task_id);
        let is_checkout = checkout_target.is_some();

        if !is_error {
            if is_checkout {
                details.merged_into = None;
                details.merge_conflict = None;
                clear_task_worktree_merge_conflict(&task_id);
            } else {
                let merge_task =
                    cx.update(|cx| merge_task_branch_into_goal(project.clone(), &task_id, cx));
                match merge_task.await? {
                    MergeOutcome::NoGoal => {
                        details.merged_into = None;
                        details.merge_conflict = None;
                    }
                    MergeOutcome::FastForward { target_ref, sha }
                    | MergeOutcome::Merged { target_ref, sha } => {
                        details.merged_into = Some(MergedInto { target_ref, sha });
                        details.merge_conflict = None;
                        clear_task_worktree_merge_conflict(&task_id);
                    }
                    MergeOutcome::Conflict(conflict) => {
                        details.merged_into = None;
                        details.merge_conflict = Some(conflict.files.clone());
                        record_task_worktree_merge_conflict(&task_id, conflict.files);
                    }
                }
            }
        }

        record_task_worktree_isolation_details(&task_id, details.clone());
        let project_clone = project.clone();
        let publish_task =
            cx.update(|cx| publish_task_ref_artifact_to_tgs(project_clone, &task_id, &details, cx));
        publish_task.await.log_err();

        Ok(details)
    })
}

pub fn record_goal_tip_sha(goal_id: &str, sha: String) {
    let mut reg = REGISTRY.write();
    reg.goal_tip_shas.insert(goal_id.to_string(), sha);
}

pub fn goal_tip_sha(goal_id: &str) -> Option<String> {
    let reg = REGISTRY.read();
    reg.goal_tip_shas.get(goal_id).cloned()
}

pub fn get_goal_graduation_summary(goal_id: &str) -> Option<GoalGraduationSummary> {
    let reg = REGISTRY.read();
    reg.goal_graduations.get(goal_id).cloned()
}

pub fn assert_safe_graduation_ref(ref_or_branch: &str) -> Result<()> {
    let clean = ref_or_branch
        .strip_prefix("refs/heads/")
        .unwrap_or(ref_or_branch);
    if clean == "main" || clean == "master" || clean == "HEAD" || clean.starts_with("refs/remotes/")
    {
        anyhow::bail!(
            "ref '{ref_or_branch}' is protected; direct mutation or deletion of main/master is forbidden"
        );
    }
    if clean.contains("..") {
        anyhow::bail!("ref '{ref_or_branch}' cannot contain path traversals");
    }
    if !(clean.starts_with("agent-goal/") || clean.starts_with("agent-task/")) {
        anyhow::bail!(
            "ref '{ref_or_branch}' is not a valid agent branch (must start with agent-goal/ or agent-task/)"
        );
    }
    Ok(())
}

pub fn prepare_goal_graduation_summary(goal_id: &str) -> Result<GoalGraduationSummary> {
    let goal_id_str = goal_id.to_string();
    let goal_branch = format!("agent-goal/{goal_id_str}");
    assert_safe_graduation_ref(&goal_branch)?;

    let reg = REGISTRY.read();
    let tip_sha = reg.goal_tip_shas.get(&goal_id_str).cloned();

    let history = reg
        .goal_history
        .get(&goal_id_str)
        .cloned()
        .unwrap_or_default();
    let commits_count = history.len();
    let merged_tasks: Vec<String> = history.iter().map(|e| e.task_id.to_string()).collect();

    let mut unmerged_conflicts = Vec::new();
    for (task_id, entry) in &reg.entries {
        let matches_goal = entry.goal_branch.as_deref() == Some(&goal_branch)
            || entry.goal_branch.as_deref() == Some(&goal_id_str);
        if matches_goal {
            if let Some(conflict_files) = &entry.merge_conflict {
                unmerged_conflicts.push(UnmergedConflict {
                    task_id: task_id.to_string(),
                    branch: entry.checked_out_branch(task_id),
                    conflict_files: conflict_files.clone(),
                });
            }
        }
    }

    let graduation_blocked = !unmerged_conflicts.is_empty();

    let title = format!("Graduate goal {goal_id_str}");
    let mut body = String::new();
    body.push_str(&format!(
        "## Summary\n\nGoal `{goal_id_str}` graduation into `main`.\n\n"
    ));
    body.push_str(&format!("- Source branch: `{goal_branch}`\n"));
    body.push_str("- Target branch: `main`\n");
    if let Some(sha) = &tip_sha {
        let short_sha = &sha[..7.min(sha.len())];
        body.push_str(&format!("- Tip SHA: `{short_sha}` ({sha})\n"));
    }
    body.push_str(&format!("- Commits merged: {commits_count}\n"));
    if !merged_tasks.is_empty() {
        body.push_str("- Merged tasks:\n");
        for t in &merged_tasks {
            body.push_str(&format!("  - `{t}`\n"));
        }
    }
    if graduation_blocked {
        body.push_str("\n⚠️ **Graduation is currently blocked by merge conflicts:**\n");
        for c in &unmerged_conflicts {
            body.push_str(&format!(
                "- Task `{}` on `{}`: {}\n",
                c.task_id,
                c.branch,
                c.conflict_files.join(", ")
            ));
        }
    }
    body.push_str("\nRelease Notes:\n\n- N/A\n");

    let pr_cmd = format!(
        "gh pr create --base main --head {} --title \"{}\" --body \"{}\"",
        goal_branch,
        title,
        body.replace('\"', "\\\"").replace('\n', "\\n")
    );

    let pr_proposal = PrProposal {
        title,
        body,
        source_branch: goal_branch.clone(),
        target_branch: "main".to_string(),
        command: pr_cmd,
    };

    let summary = GoalGraduationSummary {
        goal_id: goal_id_str,
        goal_branch: goal_branch.clone(),
        base_branch: "main".to_string(),
        refs: GoalRefs {
            goal_branch: goal_branch.clone(),
            goal_ref: format!("refs/heads/{goal_branch}"),
            base_branch: "main".to_string(),
            base_ref: "refs/heads/main".to_string(),
        },
        tip_sha,
        commits_count,
        merged_tasks,
        unmerged_conflicts,
        graduation_blocked,
        pr_proposal,
    };

    Ok(summary)
}

pub fn prepare_goal_graduation(
    project: Entity<Project>,
    goal_id: &str,
    cx: &mut App,
) -> Task<Result<GoalGraduationSummary>> {
    let goal_id = goal_id.to_string();
    cx.spawn(async move |cx| {
        let (_repository, anchor_path, _, _, file_system) = project
            .update(cx, |project, cx| {
                anyhow::Ok(task_worktree_context(project, cx))
            })??;
        let dot_git = anchor_path.join(".git");
        let git_binary = system_git_binary();
        let git_repo = file_system
            .open_repo(&dot_git, git_binary.as_deref())
            .with_context(|| format!("opening repo at {}", dot_git.display()))?;

        let goal_branch = format!("agent-goal/{}", goal_id);
        assert_safe_graduation_ref(&goal_branch)?;

        if let Ok(Some(sha)) = resolve_ref_to_sha(&git_repo, &goal_branch).await {
            record_goal_tip_sha(&goal_id, sha);
        }

        let summary = prepare_goal_graduation_summary(&goal_id)?;
        {
            let mut reg = REGISTRY.write();
            reg.goal_graduations
                .insert(goal_id.clone(), summary.clone());
        }

        Ok(summary)
    })
}

pub fn publish_goal_ref_artifact_to_tgs(
    project: Entity<Project>,
    goal_id: &str,
    summary: &GoalGraduationSummary,
    cx: &mut App,
) -> Task<Result<Option<String>>> {
    let goal_id_str = goal_id.to_string();
    let content = match serde_json::to_string(summary) {
        Ok(c) => c,
        Err(e) => {
            return Task::ready(Err(anyhow::anyhow!(
                "failed to serialize GoalGraduationSummary: {e}"
            )));
        }
    };

    let server_store = project.read(cx).context_server_store();
    let server_id = agent_settings::AgentSettings::get_for_project(project.read(cx), cx)
        .task_graph_server_id
        .clone();
    let server = server_store
        .read(cx)
        .get_running_server(&context_server::ContextServerId(server_id.clone().into()));

    let Some(server) = server else {
        return Task::ready(Err(anyhow::anyhow!(
            "No running task graph server found with ID '{server_id}' to publish GoalGraduationSummary for {goal_id_str}"
        )));
    };

    cx.spawn(async move |_cx| {
        let Some(client) = server.client() else {
            return Err(anyhow::anyhow!(
                "Task graph server '{server_id}' has no client connected to publish GoalGraduationSummary for {goal_id_str}"
            ));
        };

        let mut args = serde_json::Map::new();
        args.insert(
            "task_id".to_string(),
            serde_json::Value::String(goal_id_str.clone()),
        );
        args.insert(
            "goal_id".to_string(),
            serde_json::Value::String(goal_id_str.clone()),
        );
        args.insert(
            "kind".to_string(),
            serde_json::Value::String("goal_ref".to_string()),
        );
        args.insert(
            "summary".to_string(),
            serde_json::Value::String(format!("Goal ref graduation summary for {goal_id_str}")),
        );
        args.insert("content".to_string(), serde_json::Value::String(content));

        let response = client
            .request::<context_server::types::requests::CallTool>(
                context_server::types::CallToolParams {
                    name: "artifact_publish".into(),
                    arguments: Some(serde_json::Value::Object(args)),
                    meta: None,
                },
            )
            .await;

        match response {
            Ok(resp) => {
                if resp.is_error == Some(true) {
                    let err_msg: String = resp.content.iter().filter_map(|c| c.text()).collect();
                    log::warn!(
                        "TGS artifact_publish returned error for goal {goal_id_str}: {err_msg}"
                    );
                    Err(anyhow::anyhow!(
                        "TGS artifact_publish returned error for goal {goal_id_str}: {err_msg}"
                    ))
                } else {
                    let text: String = resp.content.iter().filter_map(|c| c.text()).collect();
                    log::info!("Published GoalRefArtifact for {goal_id_str}: {text}");
                    Ok(Some(text))
                }
            }
            Err(e) => {
                log::warn!("Failed to publish GoalRefArtifact for {goal_id_str}: {e}");
                Err(e)
            }
        }
    })
}

pub fn prepare_and_publish_goal_graduation(
    project: Entity<Project>,
    goal_id: &str,
    cx: &mut App,
) -> Task<Result<GoalGraduationSummary>> {
    let goal_id = goal_id.to_string();
    let prep_task = prepare_goal_graduation(project.clone(), &goal_id, cx);
    cx.spawn(async move |cx| {
        let summary = prep_task.await?;
        let pub_task =
            cx.update(|cx| publish_goal_ref_artifact_to_tgs(project, &goal_id, &summary, cx));
        pub_task.await.log_err();
        Ok(summary)
    })
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct GoalCleanupResult {
    pub goal_id: String,
    pub deleted_branches: Vec<String>,
    pub failed_branches: Vec<(String, String)>,
    pub cleaned_tasks: Vec<AgentTaskId>,
}

pub fn goal_has_active_tasks(goal_id: &str) -> bool {
    let reg = REGISTRY.read();
    let goal_branch = format!("agent-goal/{}", goal_id);
    for (_task_id, entry) in &reg.entries {
        let matches_goal = entry.goal_branch.as_deref() == Some(&goal_branch)
            || entry.goal_branch.as_deref() == Some(goal_id);
        if matches_goal && !entry.is_terminal {
            return true;
        }
    }
    false
}

pub fn cleanup_graduated_goal(
    project: Entity<Project>,
    goal_id: &str,
    cx: &mut App,
) -> Task<Result<GoalCleanupResult>> {
    let goal_id = goal_id.to_string();
    cx.spawn(async move |cx| {
        let (repository, anchor_path, _, _, file_system) = project
            .update(cx, |project, cx| {
                anyhow::Ok(task_worktree_context(project, cx))
            })??;
        let dot_git = anchor_path.join(".git");
        let git_binary = system_git_binary();
        let git_repo = file_system
            .open_repo(&dot_git, git_binary.as_deref())
            .with_context(|| format!("opening repo at {}", dot_git.display()))?;

        let goal_branch = format!("agent-goal/{}", goal_id);
        assert_safe_graduation_ref(&goal_branch)?;

        let mut branches_to_delete = vec![goal_branch.clone()];
        let mut tasks_to_cleanup = Vec::new();

        {
            let reg = REGISTRY.read();
            for (task_id, entry) in &reg.entries {
                let matches_goal = entry.goal_branch.as_deref() == Some(&goal_branch)
                    || entry.goal_branch.as_deref() == Some(&goal_id);
                if matches_goal && !entry.is_terminal {
                    anyhow::bail!(
                        "cannot clean up goal {goal_id}: sibling task {task_id} is still active"
                    );
                }
            }
            if let Some(history) = reg.goal_history.get(&goal_id) {
                for entry in history {
                    tasks_to_cleanup.push(entry.task_id.clone());
                }
            }
            for (task_id, entry) in &reg.entries {
                let matches_goal = entry.goal_branch.as_deref() == Some(&goal_branch)
                    || entry.goal_branch.as_deref() == Some(&goal_id);
                if matches_goal {
                    tasks_to_cleanup.push(task_id.clone());
                }
            }
        }

        tasks_to_cleanup.sort();
        tasks_to_cleanup.dedup();

        for task_id in &tasks_to_cleanup {
            let task_branch = format!("agent-task/{}", task_id);
            assert_safe_graduation_ref(&task_branch)?;
            branches_to_delete.push(task_branch);
        }

        branches_to_delete.sort();
        branches_to_delete.dedup();

        let mut deleted_branches = Vec::new();
        let mut failed_branches = Vec::new();

        for branch in branches_to_delete {
            assert_safe_graduation_ref(&branch)?;
            let mut failed = false;
            let git_del_res = git_repo.delete_branch(false, branch.clone(), true).await;
            if let Err(e) = git_del_res {
                log::warn!(
                    "Failed to delete branch {} from git repository: {}",
                    branch,
                    e
                );
                failed_branches.push((branch.clone(), e.to_string()));
                failed = true;
            }

            let del_task =
                repository.update(cx, |r, _| r.delete_branch(false, branch.clone(), true));
            match del_task.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    log::warn!(
                        "Failed to delete branch {} from repository entity: {}",
                        branch,
                        e
                    );
                    if !failed {
                        failed_branches.push((branch.clone(), e.to_string()));
                        failed = true;
                    }
                }
                Err(e) => {
                    log::warn!("Failed to wait for branch deletion of {}: {}", branch, e);
                    if !failed {
                        failed_branches.push((branch.clone(), e.to_string()));
                        failed = true;
                    }
                }
            }

            if !failed {
                deleted_branches.push(branch.clone());
            }

            #[cfg(any(test, feature = "test-support"))]
            if file_system.is_fake() {
                file_system
                    .as_fake()
                    .with_git_state(&dot_git, true, |state| {
                        state.branches.remove(&branch);
                        state.refs.remove(&branch);
                        state.refs.remove(&format!("refs/heads/{}", branch));
                    })
                    .log_err();
            }
        }

        {
            let mut reg = REGISTRY.write();
            reg.goal_history.remove(&goal_id);
            #[cfg(any(test, feature = "test-support"))]
            reg.goal_files.remove(&goal_id);
            reg.goal_tip_shas.remove(&goal_id);
            reg.goal_graduations.remove(&goal_id);
            for task_id in &tasks_to_cleanup {
                reg.entries.remove(task_id);
                #[cfg(any(test, feature = "test-support"))]
                reg.task_files.remove(task_id);
            }
        }

        log::info!("Cleaned up graduated goal {goal_id} and associated task branches");
        Ok(GoalCleanupResult {
            goal_id,
            deleted_branches,
            failed_branches,
            cleaned_tasks: tasks_to_cleanup,
        })
    })
}

pub fn goal_branch_summary_for_task(task_id: &AgentTaskId) -> Option<GoalBranchSummary> {
    let reg = REGISTRY.read();
    let entry = reg.entries.get(task_id);

    let goal_id = entry
        .and_then(|e| e.goal_branch.as_ref())
        .map(|gb| {
            gb.strip_prefix("agent-goal/")
                .unwrap_or(gb.as_str())
                .to_string()
        })
        .or_else(|| {
            let task_str = task_id.to_string();
            if reg.goal_history.contains_key(&task_str) || reg.goal_tip_shas.contains_key(&task_str)
            {
                Some(task_str)
            } else {
                for (gid, history) in &reg.goal_history {
                    if history.iter().any(|e| &e.task_id == task_id) {
                        return Some(gid.clone());
                    }
                }
                None
            }
        })?;

    let branch = format!("agent-goal/{}", goal_id);
    let tip_sha = reg.goal_tip_shas.get(&goal_id).cloned();
    let merged_tasks_count = reg.goal_history.get(&goal_id).map_or(0, |h| h.len());
    let mut conflicts = None;
    let mut has_conflict = false;

    if let Some(entry) = entry {
        if let Some(ref c) = entry.merge_conflict {
            conflicts = Some(c.clone());
            has_conflict = true;
        }
    }

    if !has_conflict {
        for (_other_id, other_entry) in &reg.entries {
            let matches_goal = other_entry.goal_branch.as_deref() == Some(&branch)
                || other_entry.goal_branch.as_deref() == Some(&goal_id);
            if matches_goal && other_entry.merge_conflict.is_some() {
                has_conflict = true;
                if conflicts.is_none() {
                    conflicts = other_entry.merge_conflict.clone();
                }
                break;
            }
        }
    }

    Some(GoalBranchSummary {
        goal_id,
        branch,
        tip_sha,
        merged_tasks_count,
        conflicts,
        has_conflict,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentTaskStatus;
    use context_server::{ContextServer, ContextServerCommand, ContextServerId};
    use fs::FakeFs;
    use futures::{StreamExt as _, channel::mpsc};
    use gpui::TestAppContext;
    use project::project_settings::ProjectSettings;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        // reset_registry_for_tests();
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            ProjectSettings::register(cx);
            agent_settings::AgentSettings::register(cx);
        });
    }

    #[test]
    fn test_subagent_session_guard_drop_unregisters() {
        let task_id = AgentTaskId::from("TASK-GUARD-TEST");
        assert!(!has_active_subagent_session(&task_id));

        {
            let _guard = SubagentSessionGuard::new(task_id.clone());
            assert!(has_active_subagent_session(&task_id));
        }

        // Drop of guard must automatically unregister the session
        assert!(!has_active_subagent_session(&task_id));
    }

    #[gpui::test]
    async fn test_ensure_task_worktree_idempotent(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            json!({
                ".git": {}
            }),
        )
        .await;

        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-100"),
            parent_id: None,
            goal_id: None,
            title: "Test worktree task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec!["src/".to_string()],
        };

        let path1 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await;
        assert!(
            path1.is_ok(),
            "ensure_task_worktree failed: {:?}",
            path1.err()
        );
        let path1 = path1.unwrap();
        assert!(path1.to_string_lossy().contains("agent-task-TASK-100"));

        let path2 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await;
        assert!(path2.is_ok(), "idempotent call failed: {:?}", path2.err());
        assert_eq!(path1, path2.unwrap());
    }

    #[gpui::test]
    async fn test_two_tasks_get_distinct_worktrees(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            json!({
                ".git": {}
            }),
        )
        .await;

        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
        let first_task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-DIST-1"),
            parent_id: None,
            goal_id: None,
            title: "First task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec!["src/".to_string()],
        };
        let second_task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-DIST-2"),
            parent_id: None,
            goal_id: None,
            title: "Second task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec!["docs/".to_string()],
        };

        let first_path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &first_task, cx))
            .await
            .expect("first worktree should be created");
        let second_path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &second_task, cx))
            .await
            .expect("second worktree should be created");

        assert_ne!(first_path, second_path);
        assert!(
            first_path
                .to_string_lossy()
                .contains("agent-task-TASK-DIST-1")
        );
        assert!(
            second_path
                .to_string_lossy()
                .contains("agent-task-TASK-DIST-2")
        );

        // Both directories exist in the project's filesystem and the main
        // worktree root is still present.
        assert!(fs.is_dir(&first_path).await);
        assert!(fs.is_dir(&second_path).await);
        assert!(fs.is_dir("/root".as_ref()).await);
    }

    #[gpui::test]
    async fn test_ensure_goal_branch_lazy_idempotent_no_worktree(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            json!({
                ".git": {
                    "HEAD": "ref: refs/heads/main\n"
                }
            }),
        )
        .await;

        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;

        // Before ensure_goal_branch, agent-goal/GOAL-1 does not exist
        let dot_git = std::path::Path::new("/root/.git");
        let has_goal_initially = fs
            .with_git_state(dot_git, false, |state| {
                state.branches.contains("agent-goal/GOAL-1")
            })
            .unwrap();
        assert!(!has_goal_initially);

        // First call: lazily creates ref without worktree checkout
        let branch = cx
            .update(|cx| ensure_goal_branch(project.clone(), "GOAL-1", cx))
            .await
            .expect("ensure_goal_branch should succeed");
        assert_eq!(branch, "agent-goal/GOAL-1");

        // Verify ref exists in git
        let has_goal_after = fs
            .with_git_state(dot_git, false, |state| {
                state.branches.contains("agent-goal/GOAL-1")
                    || state.refs.contains_key("refs/heads/agent-goal/GOAL-1")
            })
            .unwrap();
        assert!(has_goal_after, "agent-goal/GOAL-1 must exist in git");

        // Verify NO worktree directory was created for the goal branch
        assert!(
            !fs.is_dir(std::path::Path::new("/root/agent-goal-GOAL-1"))
                .await
        );
        assert!(
            !fs.is_dir(std::path::Path::new("/worktrees/root/agent-goal-GOAL-1"))
                .await
        );

        // Second call: idempotent, returns same branch
        let branch2 = cx
            .update(|cx| ensure_goal_branch(project.clone(), "GOAL-1", cx))
            .await
            .expect("second ensure_goal_branch should succeed");
        assert_eq!(branch2, "agent-goal/GOAL-1");
    }

    #[gpui::test]
    async fn test_ensure_task_worktree_with_goal_id_and_base_branch(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            json!({
                ".git": {
                    "HEAD": "ref: refs/heads/main\n"
                }
            }),
        )
        .await;

        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
        let dot_git = std::path::Path::new("/root/.git");
        fs.with_git_state(dot_git, true, |state| {
            state
                .refs
                .insert("refs/heads/main".into(), "main-sha-123".into());
            state.refs.insert("HEAD".into(), "main-sha-123".into());
            state.branches.insert("main".into());
            state
                .refs
                .insert("refs/heads/feature-prev".into(), "prev-sha-456".into());
            state.branches.insert("feature-prev".into());
        })
        .unwrap();

        let task1 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-ENS-1"),
            parent_id: None,
            goal_id: None,
            title: "Task 1".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };

        // Case 1: task with goal_id forks from goal-tip
        let path1 = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task1,
                    Some("GOAL-42".to_string()),
                    None,
                    None,
                    cx,
                )
            })
            .await
            .unwrap();
        assert!(fs.is_dir(&path1).await);
        assert_eq!(
            task_worktree_goal_branch(&task1.id),
            Some("agent-goal/GOAL-42".to_string())
        );
        assert_eq!(
            task_worktree_base_ref(&task1.id),
            Some("agent-goal/GOAL-42".to_string())
        );

        // Case 2: task with both goal_id and base_branch -> base_branch prioritized as fork base
        let task2 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-ENS-2"),
            parent_id: None,
            goal_id: None,
            title: "Task 2".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let path2 = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task2,
                    Some("GOAL-42".to_string()),
                    Some("feature-prev".to_string()),
                    None,
                    cx,
                )
            })
            .await
            .unwrap();
        assert!(fs.is_dir(&path2).await);
        assert_eq!(
            task_worktree_goal_branch(&task2.id),
            Some("agent-goal/GOAL-42".to_string())
        );
        assert_eq!(
            task_worktree_base_ref(&task2.id),
            Some("feature-prev".to_string())
        );

        // Case 3: base_branch matches agent-goal/* -> auto-creates goal branch
        let task3 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-3"),
            parent_id: None,
            goal_id: None,
            title: "Task 3".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let path3 = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task3,
                    None,
                    Some("agent-goal/AUTO-GOAL".to_string()),
                    None,
                    cx,
                )
            })
            .await
            .unwrap();
        assert!(fs.is_dir(&path3).await);
        assert_eq!(
            task_worktree_base_ref(&task3.id),
            Some("agent-goal/AUTO-GOAL".to_string())
        );

        // Case 4: unknown base_branch -> error
        let task4 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-4"),
            parent_id: None,
            goal_id: None,
            title: "Task 4".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let result4 = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task4,
                    None,
                    Some("nonexistent-branch".to_string()),
                    None,
                    cx,
                )
            })
            .await;
        assert!(result4.is_err());
        let err_msg = result4.unwrap_err().to_string();
        assert!(err_msg.contains("branch not found"));

        // Case 5: task_id without goal_id/base_branch -> previous behavior (from HEAD main)
        let task5 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-5"),
            parent_id: None,
            goal_id: None,
            title: "Task 5".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let path5 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task5, cx))
            .await
            .unwrap();
        assert!(fs.is_dir(&path5).await);
        assert_eq!(task_worktree_goal_branch(&task5.id), None);
        assert_eq!(task_worktree_base_ref(&task5.id), None);
    }

    #[gpui::test]
    async fn test_acceptance_fast_forward_merge_moves_goal_ref_without_checkout(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".git": {
                    "HEAD": "ref: refs/heads/main\n"
                },
                "initial.txt": "initial base"
            }),
        )
        .await;

        let dot_git = std::path::Path::new(path!("/root/.git"));
        fs.with_git_state(dot_git, true, |state| {
            state
                .refs
                .insert("refs/heads/main".into(), "main-sha-100".into());
            state.refs.insert("HEAD".into(), "main-sha-100".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-FF-1"),
            parent_id: None,
            goal_id: None,
            title: "FF Task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };

        let worktree_path = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task,
                    Some("GOAL-FF".to_string()),
                    None,
                    None,
                    cx,
                )
            })
            .await
            .unwrap();

        fs.save(
            &worktree_path.join("file_ff.txt"),
            &"fast forward content".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details = cx
            .update(|cx| commit_and_merge_task_worktree(project.clone(), &task.id, false, cx))
            .await
            .unwrap();

        assert_eq!(details.commit_error, None);
        assert_eq!(details.merge_conflict, None);
        let merged_into = details
            .merged_into
            .expect("must be merged into goal branch");
        assert_eq!(merged_into.target_ref, "refs/heads/agent-goal/GOAL-FF");
        assert_ne!(merged_into.sha, "main-sha-100");

        let goal_sha = fs
            .with_git_state(dot_git, false, |state| {
                state.refs.get("refs/heads/agent-goal/GOAL-FF").cloned()
            })
            .unwrap();
        assert_eq!(goal_sha, Some(merged_into.sha));

        let main_sha = fs
            .with_git_state(dot_git, false, |state| {
                state.refs.get("refs/heads/main").cloned()
            })
            .unwrap();
        assert_eq!(main_sha, Some("main-sha-100".to_string()));
    }

    #[gpui::test]
    async fn test_acceptance_non_fast_forward_two_tasks_serialized_dag_parents(
        cx: &mut TestAppContext,
    ) {
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
            state
                .refs
                .insert("refs/heads/main".into(), "main-sha-200".into());
            state.refs.insert("HEAD".into(), "main-sha-200".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let task_a = AgentTaskSummary {
            id: AgentTaskId::from("TASK-NFF-A"),
            parent_id: None,
            goal_id: None,
            title: "Task A".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let task_b = AgentTaskSummary {
            id: AgentTaskId::from("TASK-NFF-B"),
            parent_id: None,
            goal_id: None,
            title: "Task B".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };

        let path_a = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task_a,
                    Some("GOAL-NFF".to_string()),
                    None,
                    None,
                    cx,
                )
            })
            .await
            .unwrap();

        let path_b = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task_b,
                    Some("GOAL-NFF".to_string()),
                    None,
                    None,
                    cx,
                )
            })
            .await
            .unwrap();

        fs.save(
            &path_a.join("file_a.txt"),
            &"task a content".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        fs.save(
            &path_b.join("file_b.txt"),
            &"task b content".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details_a = cx
            .update(|cx| commit_and_merge_task_worktree(project.clone(), &task_a.id, false, cx))
            .await
            .unwrap();
        let sha_a = details_a.merged_into.expect("task a merged").sha;

        let details_b = cx
            .update(|cx| commit_and_merge_task_worktree(project.clone(), &task_b.id, false, cx))
            .await
            .unwrap();
        assert_eq!(details_b.merge_conflict, None);
        let merged_b = details_b.merged_into.expect("task b merged");
        let merge_sha = merged_b.sha;

        let history = get_goal_history_entries("GOAL-NFF");
        assert_eq!(history.len(), 2, "history should contain 2 merge entries");

        assert_eq!(history[0].0, sha_a);
        assert_eq!(history[0].1, task_a.id);
        assert_eq!(history[0].2, vec!["file_a.txt".to_string()]);

        assert_eq!(history[1].0, merge_sha);
        assert_eq!(history[1].1, task_b.id);
        assert_eq!(history[1].2, vec!["file_b.txt".to_string()]);

        let current_goal_sha = fs
            .with_git_state(dot_git, false, |state| {
                state.refs.get("refs/heads/agent-goal/GOAL-NFF").cloned()
            })
            .unwrap();
        assert_eq!(current_goal_sha, Some(merge_sha));
    }

    #[gpui::test]
    async fn test_acceptance_merge_conflict_fail_open_retention(cx: &mut TestAppContext) {
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
            state
                .refs
                .insert("refs/heads/main".into(), "main-sha-300".into());
            state.refs.insert("HEAD".into(), "main-sha-300".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let task_c1 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-CONF-1"),
            parent_id: None,
            goal_id: None,
            title: "Task C1".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let task_c2 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-CONF-2"),
            parent_id: None,
            goal_id: None,
            title: "Task C2".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };

        let path_c1 = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task_c1,
                    Some("GOAL-CONF".to_string()),
                    None,
                    None,
                    cx,
                )
            })
            .await
            .unwrap();

        let path_c2 = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task_c2,
                    Some("GOAL-CONF".to_string()),
                    None,
                    None,
                    cx,
                )
            })
            .await
            .unwrap();

        fs.save(
            &path_c1.join("conflict.txt"),
            &"c1 edits".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        fs.save(
            &path_c2.join("conflict.txt"),
            &"c2 edits".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details_c1 = cx
            .update(|cx| commit_and_merge_task_worktree(project.clone(), &task_c1.id, false, cx))
            .await
            .unwrap();
        let sha_c1 = details_c1.merged_into.unwrap().sha;

        let details_c2 = cx
            .update(|cx| commit_and_merge_task_worktree(project.clone(), &task_c2.id, false, cx))
            .await
            .unwrap();

        assert_eq!(details_c2.merged_into, None);
        assert_eq!(
            details_c2.merge_conflict,
            Some(vec!["conflict.txt".to_string()])
        );

        let serialized = serde_json::to_value(&details_c2).unwrap();
        assert_eq!(serialized["merged_into"], serde_json::Value::Null);
        assert_eq!(
            serialized["merge_conflict"],
            serde_json::json!(["conflict.txt"])
        );

        let goal_sha_after = fs
            .with_git_state(dot_git, false, |state| {
                state.refs.get("refs/heads/agent-goal/GOAL-CONF").cloned()
            })
            .unwrap();
        assert_eq!(goal_sha_after, Some(sha_c1));
        assert!(details_c2.head_sha.is_some());

        mark_task_terminal(&task_c2.id);
        let cleaned = cx
            .update(|cx| auto_cleanup_task_worktree(project.clone(), &task_c2.id, cx))
            .await
            .unwrap();
        assert!(!cleaned, "worktree must be retained on merge conflict");
        assert!(
            fs.is_dir(&path_c2).await,
            "worktree directory must survive auto_cleanup on conflict"
        );
        assert_eq!(
            task_worktree_merge_conflict(&task_c2.id),
            Some(vec!["conflict.txt".to_string()])
        );
    }

    #[gpui::test]
    async fn test_acceptance_merge_invariant_before_task_complete(cx: &mut TestAppContext) {
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
            state
                .refs
                .insert("refs/heads/main".into(), "main-sha-400".into());
            state.refs.insert("HEAD".into(), "main-sha-400".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-INV-1"),
            parent_id: None,
            goal_id: None,
            title: "Invariant Task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };

        let worktree_path = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task,
                    Some("GOAL-INV".to_string()),
                    None,
                    None,
                    cx,
                )
            })
            .await
            .unwrap();

        fs.save(
            &worktree_path.join("inv.txt"),
            &"invariant verified".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        assert!(!is_task_terminal(&task.id));
        assert!(task_worktree_isolation_details(&task.id).is_none());

        let details = cx
            .update(|cx| commit_and_merge_task_worktree(project.clone(), &task.id, false, cx))
            .await
            .unwrap();

        assert!(!is_task_terminal(&task.id));
        assert!(task_worktree_isolation_details(&task.id).is_some());
        let merged_sha = details.merged_into.unwrap().sha;

        let goal_sha = fs
            .with_git_state(dot_git, false, |state| {
                state.refs.get("refs/heads/agent-goal/GOAL-INV").cloned()
            })
            .unwrap();
        assert_eq!(goal_sha, Some(merged_sha));

        mark_task_terminal(&task.id);
        assert!(is_task_terminal(&task.id));
        let cleaned = cx
            .update(|cx| auto_cleanup_task_worktree(project.clone(), &task.id, cx))
            .await
            .unwrap();
        assert!(cleaned);
        assert!(!fs.is_dir(&worktree_path).await);
    }

    #[gpui::test]
    async fn test_acceptance_e2e_goal_transport_task_a_to_task_b(cx: &mut TestAppContext) {
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
            state
                .refs
                .insert("refs/heads/main".into(), "main-sha-500".into());
            state.refs.insert("HEAD".into(), "main-sha-500".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let task_a = AgentTaskSummary {
            id: AgentTaskId::from("TASK-E2E-A"),
            parent_id: None,
            goal_id: None,
            title: "Task A".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };

        let path_a = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task_a,
                    Some("GOAL-E2E".to_string()),
                    None,
                    None,
                    cx,
                )
            })
            .await
            .unwrap();

        fs.save(
            &path_a.join("module_a.rs"),
            &"pub fn a() -> usize { 42 }".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details_a = cx
            .update(|cx| commit_and_merge_task_worktree(project.clone(), &task_a.id, false, cx))
            .await
            .unwrap();
        assert!(details_a.merged_into.is_some());

        mark_task_terminal(&task_a.id);
        let cleaned_a = cx
            .update(|cx| auto_cleanup_task_worktree(project.clone(), &task_a.id, cx))
            .await
            .unwrap();
        assert!(cleaned_a);
        assert!(!fs.is_dir(&path_a).await);

        let task_b = AgentTaskSummary {
            id: AgentTaskId::from("TASK-E2E-B"),
            parent_id: None,
            goal_id: None,
            title: "Task B".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };

        let path_b = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task_b,
                    Some("GOAL-E2E".to_string()),
                    None,
                    None,
                    cx,
                )
            })
            .await
            .unwrap();

        let file_a_in_b = path_b.join("module_a.rs");
        assert!(
            fs.is_file(&file_a_in_b).await,
            "TASK-B worktree must contain module_a.rs from TASK-A"
        );
        let content_in_b = fs.load(&file_a_in_b).await.unwrap();
        assert_eq!(content_in_b, "pub fn a() -> usize { 42 }");

        fs.save(
            &path_b.join("module_b.rs"),
            &"pub fn b() -> usize { 84 }".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details_b = cx
            .update(|cx| commit_and_merge_task_worktree(project.clone(), &task_b.id, false, cx))
            .await
            .unwrap();
        assert!(details_b.merged_into.is_some());

        let goal_files = get_goal_files("GOAL-E2E");
        assert!(goal_files.contains_key(PathBuf::from("module_a.rs").as_path()));
        assert!(goal_files.contains_key(PathBuf::from("module_b.rs").as_path()));
    }

    #[gpui::test]
    async fn test_acceptance_reviewer_on_branch_goal_reads_integrated_state(
        cx: &mut TestAppContext,
    ) {
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
            state
                .refs
                .insert("refs/heads/main".into(), "main-sha-rev".into());
            state.refs.insert("HEAD".into(), "main-sha-rev".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let task_a = AgentTaskSummary {
            id: AgentTaskId::from("TASK-REV-A"),
            parent_id: None,
            goal_id: None,
            title: "Task A".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };

        let path_a = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task_a,
                    Some("GOAL-REV".to_string()),
                    None,
                    None,
                    cx,
                )
            })
            .await
            .unwrap();

        fs.save(
            &path_a.join("module_a.rs"),
            &"pub fn a() -> usize { 42 }".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details_a = cx
            .update(|cx| commit_and_merge_task_worktree(project.clone(), &task_a.id, false, cx))
            .await
            .unwrap();
        assert!(details_a.merged_into.is_some());

        mark_task_terminal(&task_a.id);
        let cleaned_a = cx
            .update(|cx| auto_cleanup_task_worktree(project.clone(), &task_a.id, cx))
            .await
            .unwrap();
        assert!(cleaned_a);
        assert!(!fs.is_dir(&path_a).await);

        // Reviewer task: on_branch: "goal"
        let reviewer_task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-REV-REVIEWER"),
            parent_id: None,
            goal_id: None,
            title: "Reviewer Task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };

        let path_reviewer = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &reviewer_task,
                    Some("GOAL-REV".to_string()),
                    None,
                    Some("goal".to_string()),
                    cx,
                )
            })
            .await
            .unwrap();

        // Read integrated state: module_a.rs from TASK-A must be present
        let file_a_in_rev = path_reviewer.join("module_a.rs");
        assert!(
            fs.is_file(&file_a_in_rev).await,
            "reviewer on on_branch:goal must see module_a.rs from merged TASK-A"
        );
        let content = fs.load(&file_a_in_rev).await.unwrap();
        assert_eq!(content, "pub fn a() -> usize { 42 }");

        // Read-only profile makes no edits -> changed_files: 0
        let details_rev = cx
            .update(|cx| {
                commit_and_merge_task_worktree(project.clone(), &reviewer_task.id, false, cx)
            })
            .await
            .unwrap();

        assert_eq!(details_rev.mode, "checkout");
        assert_eq!(details_rev.branch, "agent-goal/GOAL-REV");
        assert_eq!(details_rev.base_branch, None);
        assert_eq!(details_rev.changed_files, 0);
        assert_eq!(details_rev.merged_into, None);
        assert_eq!(details_rev.commit_error, None);
        assert_eq!(details_rev.merge_conflict, None);
    }

    #[gpui::test]
    async fn test_acceptance_inspector_on_branch_agent_task_x(cx: &mut TestAppContext) {
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
            state
                .refs
                .insert("refs/heads/main".into(), "main-sha-insp".into());
            state.refs.insert("HEAD".into(), "main-sha-insp".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let task_x = AgentTaskSummary {
            id: AgentTaskId::from("TASK-X"),
            parent_id: None,
            goal_id: None,
            title: "Task X".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };

        let path_x = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task_x,
                    Some("GOAL-INSP".to_string()),
                    None,
                    None,
                    cx,
                )
            })
            .await
            .unwrap();

        fs.save(
            &path_x.join("task_x_file.txt"),
            &"task x artifact content".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details_x = cx
            .update(|cx| commit_and_merge_task_worktree(project.clone(), &task_x.id, false, cx))
            .await
            .unwrap();
        assert!(details_x.merged_into.is_some());

        mark_task_terminal(&task_x.id);
        let cleaned_x = cx
            .update(|cx| auto_cleanup_task_worktree(project.clone(), &task_x.id, cx))
            .await
            .unwrap();
        assert!(cleaned_x);

        // Inspector task: on_branch: "agent-task/TASK-X"
        let inspector_task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-INSPECTOR"),
            parent_id: None,
            goal_id: None,
            title: "Inspector Task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };

        let path_insp = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &inspector_task,
                    None,
                    None,
                    Some("agent-task/TASK-X".to_string()),
                    cx,
                )
            })
            .await
            .unwrap();

        let file_x_in_insp = path_insp.join("task_x_file.txt");
        assert!(
            fs.is_file(&file_x_in_insp).await,
            "inspector on on_branch:agent-task/TASK-X must see task_x_file.txt"
        );
        let content = fs.load(&file_x_in_insp).await.unwrap();
        assert_eq!(content, "task x artifact content");

        let details_insp = cx
            .update(|cx| {
                commit_and_merge_task_worktree(project.clone(), &inspector_task.id, false, cx)
            })
            .await
            .unwrap();

        assert_eq!(details_insp.mode, "checkout");
        assert_eq!(details_insp.branch, "agent-task/TASK-X");
        assert_eq!(details_insp.base_branch, None);
        assert_eq!(details_insp.merged_into, None);
    }

    #[gpui::test]
    async fn test_acceptance_double_checkout_error_and_cleanup_releases(cx: &mut TestAppContext) {
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
            state
                .refs
                .insert("refs/heads/main".into(), "main-sha-exc".into());
            state.refs.insert("HEAD".into(), "main-sha-exc".into());
            state.branches.insert("main".into());
            state
                .refs
                .insert("refs/heads/shared-branch".into(), "shared-sha".into());
            state.branches.insert("shared-branch".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let task1 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-EXC-1"),
            parent_id: None,
            goal_id: None,
            title: "Task 1".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };
        let task2 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-EXC-2"),
            parent_id: None,
            goal_id: None,
            title: "Task 2".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };

        // Task 1 checks out shared-branch
        let path1 = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task1,
                    None,
                    None,
                    Some("shared-branch".to_string()),
                    cx,
                )
            })
            .await
            .unwrap();
        assert!(fs.is_dir(&path1).await);

        // Task 2 concurrently attempts to check out the same shared-branch
        let result2 = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task2,
                    None,
                    None,
                    Some("shared-branch".to_string()),
                    cx,
                )
            })
            .await;
        assert!(result2.is_err());
        let err_msg = result2.unwrap_err().to_string();
        assert_eq!(
            err_msg,
            "branch shared-branch is checked out by task TASK-EXC-1; serialize or use on_branch after it completes"
        );

        // Task 1 completes and cleans up
        mark_task_terminal(&task1.id);
        let cleaned1 = cx
            .update(|cx| auto_cleanup_task_worktree(project.clone(), &task1.id, cx))
            .await
            .unwrap();
        assert!(cleaned1);
        assert!(!fs.is_dir(&path1).await);

        // Task 2 retry now succeeds after Task 1 is terminal/cleaned up
        let path2 = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task2,
                    None,
                    None,
                    Some("shared-branch".to_string()),
                    cx,
                )
            })
            .await
            .unwrap();
        assert!(fs.is_dir(&path2).await);
    }

    #[gpui::test]
    async fn test_acceptance_on_branch_validation_errors(cx: &mut TestAppContext) {
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
            state
                .refs
                .insert("refs/heads/main".into(), "main-sha-val".into());
            state.refs.insert("HEAD".into(), "main-sha-val".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-VAL"),
            parent_id: None,
            goal_id: None,
            title: "Validation Task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };

        // 1. on_branch + base_branch mutually exclusive
        let err1 = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task,
                    None,
                    Some("base-b".to_string()),
                    Some("on-b".to_string()),
                    cx,
                )
            })
            .await
            .unwrap_err();
        assert!(err1.to_string().contains("mutually exclusive"));

        // 2. on_branch: "goal" requires goal_id
        let err2 = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task,
                    None,
                    None,
                    Some("goal".to_string()),
                    cx,
                )
            })
            .await
            .unwrap_err();
        assert_eq!(err2.to_string(), "on_branch 'goal' requires goal_id");

        // 3. missing branch -> branch not found
        let err3 = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task,
                    None,
                    None,
                    Some("nonexistent-branch".to_string()),
                    cx,
                )
            })
            .await
            .unwrap_err();
        assert_eq!(err3.to_string(), "branch not found: nonexistent-branch");
    }

    #[gpui::test]
    async fn test_acceptance_on_branch_write_commits_directly_without_merge_back(
        cx: &mut TestAppContext,
    ) {
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
            state
                .refs
                .insert("refs/heads/main".into(), "main-sha-dir".into());
            state.refs.insert("HEAD".into(), "main-sha-dir".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let task_merge_agent = AgentTaskSummary {
            id: AgentTaskId::from("TASK-MERGE-AGENT"),
            parent_id: None,
            goal_id: None,
            title: "Merge Agent".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };

        let path = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task_merge_agent,
                    Some("GOAL-DIRECT".to_string()),
                    None,
                    Some("goal".to_string()),
                    cx,
                )
            })
            .await
            .unwrap();

        fs.save(
            &path.join("direct_fix.rs"),
            &"pub fn resolved() -> bool { true }".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details = cx
            .update(|cx| {
                commit_and_merge_task_worktree(project.clone(), &task_merge_agent.id, false, cx)
            })
            .await
            .unwrap();

        assert_eq!(details.mode, "checkout");
        assert_eq!(details.branch, "agent-goal/GOAL-DIRECT");
        assert_eq!(details.base_branch, None);
        assert_eq!(details.changed_files, 1);
        assert_eq!(details.merged_into, None);
        assert_eq!(details.commit_error, None);
        assert_eq!(details.merge_conflict, None);

        let goal_files = get_goal_files("GOAL-DIRECT");
        assert!(goal_files.contains_key(PathBuf::from("direct_fix.rs").as_path()));
    }

    #[gpui::test]
    async fn test_acceptance_goal_graduation_summary_and_tgs_publication(cx: &mut TestAppContext) {
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
            state
                .refs
                .insert("refs/heads/main".into(), "main-sha-grad-100".into());
            state.refs.insert("HEAD".into(), "main-sha-grad-100".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        // Set up fake TGS context server that receives artifact_publish
        let (tool_calls_tx, mut tool_calls_rx) = mpsc::unbounded();
        let fake_transport = context_server::test::create_fake_transport("tgs", cx.executor())
            .on_request::<context_server::types::requests::Initialize, _>(
                move |_params| async move {
                    context_server::types::InitializeResponse {
                        protocol_version: context_server::types::ProtocolVersion(
                            context_server::types::LATEST_PROTOCOL_VERSION.to_string(),
                        ),
                        server_info: context_server::types::Implementation {
                            name: "tgs".into(),
                            title: None,
                            version: "1.0.0".to_string(),
                            description: None,
                        },
                        capabilities: context_server::types::ServerCapabilities {
                            tools: Some(context_server::types::ToolsCapabilities {
                                list_changed: Some(true),
                            }),
                            ..Default::default()
                        },
                        meta: None,
                    }
                },
            )
            .on_request::<context_server::types::requests::CallTool, _>(move |params| {
                let tool_calls_tx = tool_calls_tx.clone();
                async move {
                    tool_calls_tx.unbounded_send(params).unwrap();
                    context_server::types::CallToolResponse {
                        content: vec![context_server::types::ToolResponseContent::Text {
                            text: "artifact published ok".into(),
                        }],
                        is_error: None,
                        meta: None,
                        structured_content: None,
                    }
                }
            });

        cx.update(|cx| {
            let mut settings = ProjectSettings::get_global(cx).clone();
            settings.context_servers.insert(
                "tgs".into(),
                project::project_settings::ContextServerSettings::Stdio {
                    enabled: true,
                    remote: false,
                    command: ContextServerCommand {
                        path: "somebinary".into(),
                        args: Vec::new(),
                        env: None,
                        timeout: None,
                        platforms: Default::default(),
                    },
                },
            );
            ProjectSettings::override_global(settings, cx);
        });

        let context_server_store = project.read_with(cx, |p, _| p.context_server_store());
        context_server_store.update(cx, |store, cx| {
            store.start_server(
                Arc::new(ContextServer::new(
                    ContextServerId("tgs".into()),
                    Arc::new(fake_transport),
                )),
                cx,
            );
        });
        cx.run_until_parked();

        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-GRAD-1"),
            parent_id: None,
            goal_id: None,
            title: "Graduation Task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
        };

        let worktree_path = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task,
                    Some("GOAL-GRAD-1".to_string()),
                    None,
                    None,
                    cx,
                )
            })
            .await
            .unwrap();

        fs.save(
            &worktree_path.join("feature.rs"),
            &"pub fn feature() -> bool { true }".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details = cx
            .update(|cx| commit_and_merge_task_worktree(project.clone(), &task.id, false, cx))
            .await
            .unwrap();
        assert!(details.merged_into.is_some());
        let tip_sha = details.merged_into.as_ref().unwrap().sha.clone();

        // Trigger graduation publication
        let summary = cx
            .update(|cx| prepare_and_publish_goal_graduation(project.clone(), "GOAL-GRAD-1", cx))
            .await
            .unwrap();

        assert_eq!(summary.goal_id, "GOAL-GRAD-1");
        assert_eq!(summary.goal_branch, "agent-goal/GOAL-GRAD-1");
        assert_eq!(summary.base_branch, "main");
        assert_eq!(summary.tip_sha, Some(tip_sha));
        assert_eq!(summary.commits_count, 1);
        assert_eq!(summary.merged_tasks, vec!["TASK-GRAD-1".to_string()]);
        assert!(!summary.graduation_blocked);
        assert_eq!(summary.pr_proposal.title, "Graduate goal GOAL-GRAD-1");
        assert!(
            summary
                .pr_proposal
                .body
                .contains("Release Notes:\n\n- N/A\n")
        );
        assert!(summary.pr_proposal.command.contains("gh pr create"));

        // Verify task_ref artifact was published from task commit/merge
        let task_call = tool_calls_rx
            .next()
            .await
            .expect("must receive task artifact_publish call");
        assert_eq!(task_call.name, "artifact_publish");
        let task_args = task_call.arguments.expect("must have task tool arguments");
        assert_eq!(task_args["task_id"], "TASK-GRAD-1");
        assert_eq!(task_args["kind"], "task_ref");

        // Verify goal_ref artifact was published from goal graduation
        let goal_call = tool_calls_rx
            .next()
            .await
            .expect("must receive goal artifact_publish call");
        assert_eq!(goal_call.name, "artifact_publish");
        let goal_args = goal_call.arguments.expect("must have goal tool arguments");
        assert_eq!(goal_args["task_id"], "GOAL-GRAD-1");
        assert_eq!(goal_args["kind"], "goal_ref");
        let content_str = goal_args["content"]
            .as_str()
            .expect("must have content string");
        let published_summary: GoalGraduationSummary = serde_json::from_str(content_str).unwrap();
        assert_eq!(published_summary.goal_id, "GOAL-GRAD-1");
        assert_eq!(
            published_summary.merged_tasks,
            vec!["TASK-GRAD-1".to_string()]
        );

        // Verify summary stored in registry
        let stored = get_goal_graduation_summary("GOAL-GRAD-1");
        assert!(stored.is_some());
        assert_eq!(stored.unwrap().goal_id, "GOAL-GRAD-1");
    }

    #[gpui::test]
    async fn test_acceptance_goal_graduation_blocked_by_unmerged_conflicts(
        cx: &mut TestAppContext,
    ) {
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
            state
                .refs
                .insert("refs/heads/main".into(), "main-sha-blk".into());
            state.refs.insert("HEAD".into(), "main-sha-blk".into());
            state.branches.insert("main".into());
            state.refs.insert(
                "refs/heads/agent-goal/GOAL-BLK".into(),
                "goal-sha-blk".into(),
            );
            state.branches.insert("agent-goal/GOAL-BLK".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let task_ok = AgentTaskId::from("TASK-OK");
        let task_conflict = AgentTaskId::from("TASK-CONF");

        register_task_worktree_policy(
            &task_ok,
            PathBuf::from("/fake/ok"),
            Some("agent-goal/GOAL-BLK".to_string()),
            None,
            None,
            None,
        );
        register_task_worktree_policy(
            &task_conflict,
            PathBuf::from("/fake/conf"),
            Some("agent-goal/GOAL-BLK".to_string()),
            None,
            None,
            None,
        );

        record_goal_tip_sha("GOAL-BLK", "goal-sha-blk".to_string());
        record_task_worktree_merge_conflict(
            &task_conflict,
            vec!["src/conflict.rs".to_string(), "docs/readme.md".to_string()],
        );

        let summary = cx
            .update(|cx| prepare_goal_graduation(project.clone(), "GOAL-BLK", cx))
            .await
            .unwrap();

        assert!(
            summary.graduation_blocked,
            "must be blocked by unmerged conflict"
        );
        assert_eq!(summary.unmerged_conflicts.len(), 1);
        assert_eq!(summary.unmerged_conflicts[0].task_id, "TASK-CONF");
        assert_eq!(
            summary.unmerged_conflicts[0].conflict_files,
            vec!["src/conflict.rs".to_string(), "docs/readme.md".to_string()]
        );
        assert!(
            summary
                .pr_proposal
                .body
                .contains("⚠️ **Graduation is currently blocked by merge conflicts:**")
        );
        assert!(summary.pr_proposal.body.contains("src/conflict.rs"));
        assert!(summary.pr_proposal.body.contains("docs/readme.md"));
        assert!(summary.pr_proposal.body.contains("TASK-CONF"));
    }

    #[gpui::test]
    async fn test_acceptance_cleanup_graduated_goal_removes_branches_and_preserves_main(
        cx: &mut TestAppContext,
    ) {
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
            state.refs.insert(
                "refs/heads/main".into(),
                "main-sha-preserved-forever".into(),
            );
            state
                .refs
                .insert("HEAD".into(), "main-sha-preserved-forever".into());
            state.branches.insert("main".into());

            state.refs.insert(
                "refs/heads/agent-goal/GOAL-CLEANUP".into(),
                "goal-sha-1".into(),
            );
            state.branches.insert("agent-goal/GOAL-CLEANUP".into());

            state.refs.insert(
                "refs/heads/agent-task/TASK-CLN-1".into(),
                "task-1-sha".into(),
            );
            state.branches.insert("agent-task/TASK-CLN-1".into());

            state.refs.insert(
                "refs/heads/agent-task/TASK-CLN-2".into(),
                "task-2-sha".into(),
            );
            state.branches.insert("agent-task/TASK-CLN-2".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let task1 = AgentTaskId::from("TASK-CLN-1");
        let task2 = AgentTaskId::from("TASK-CLN-2");

        register_task_worktree_policy(
            &task1,
            PathBuf::from("/fake/cln1"),
            Some("agent-goal/GOAL-CLEANUP".to_string()),
            None,
            None,
            None,
        );
        register_task_worktree_policy(
            &task2,
            PathBuf::from("/fake/cln2"),
            Some("agent-goal/GOAL-CLEANUP".to_string()),
            None,
            None,
            None,
        );
        record_goal_tip_sha("GOAL-CLEANUP", "goal-sha-1".to_string());
        mark_task_terminal(&task1);
        mark_task_terminal(&task2);

        // Prepare graduation summary so goal has graduation summary
        let _ = cx
            .update(|cx| prepare_goal_graduation(project.clone(), "GOAL-CLEANUP", cx))
            .await
            .unwrap();
        assert!(get_goal_graduation_summary("GOAL-CLEANUP").is_some());

        // Perform cleanup of graduated goal
        cx.update(|cx| cleanup_graduated_goal(project.clone(), "GOAL-CLEANUP", cx))
            .await
            .unwrap();

        // 1. Goal branch and task branches MUST be deleted
        let (has_goal, has_t1, has_t2, main_sha, head_sha) = fs
            .with_git_state(dot_git, false, |state| {
                let g = state.branches.contains("agent-goal/GOAL-CLEANUP")
                    || state
                        .refs
                        .contains_key("refs/heads/agent-goal/GOAL-CLEANUP");
                let t1 = state.branches.contains("agent-task/TASK-CLN-1")
                    || state.refs.contains_key("refs/heads/agent-task/TASK-CLN-1");
                let t2 = state.branches.contains("agent-task/TASK-CLN-2")
                    || state.refs.contains_key("refs/heads/agent-task/TASK-CLN-2");
                let m = state.refs.get("refs/heads/main").cloned();
                let h = state.refs.get("HEAD").cloned();
                (g, t1, t2, m, h)
            })
            .unwrap();

        assert!(!has_goal, "goal branch must be deleted");
        assert!(!has_t1, "task 1 branch must be deleted");
        assert!(!has_t2, "task 2 branch must be deleted");

        // 2. Guarantee: main ref and HEAD MUST BE UNCHANGED
        assert_eq!(
            main_sha,
            Some("main-sha-preserved-forever".to_string()),
            "refs/heads/main MUST remain untouched"
        );
        assert_eq!(
            head_sha,
            Some("main-sha-preserved-forever".to_string()),
            "HEAD MUST remain untouched"
        );

        // 3. Registry cleaned up
        assert!(get_goal_graduation_summary("GOAL-CLEANUP").is_none());
        assert!(goal_tip_sha("GOAL-CLEANUP").is_none());
        assert!(goal_branch_summary_for_task(&task1).is_none());
        assert!(goal_branch_summary_for_task(&task2).is_none());
    }

    #[gpui::test]
    async fn test_acceptance_cleanup_graduated_goal_blocked_by_active_sibling(
        cx: &mut TestAppContext,
    ) {
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
            state
                .refs
                .insert("refs/heads/main".into(), "main-sha".into());
            state.refs.insert("HEAD".into(), "main-sha".into());
            state.branches.insert("main".into());
            state.refs.insert(
                "refs/heads/agent-goal/GOAL-ACTIVE".into(),
                "goal-sha-1".into(),
            );
            state.branches.insert("agent-goal/GOAL-ACTIVE".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let task1 = AgentTaskId::from("TASK-ACT-1");
        let task2 = AgentTaskId::from("TASK-ACT-2");

        register_task_worktree_policy(
            &task1,
            PathBuf::from("/fake/act1"),
            Some("agent-goal/GOAL-ACTIVE".to_string()),
            None,
            None,
            None,
        );
        register_task_worktree_policy(
            &task2,
            PathBuf::from("/fake/act2"),
            Some("agent-goal/GOAL-ACTIVE".to_string()),
            None,
            None,
            None,
        );
        record_goal_tip_sha("GOAL-ACTIVE", "goal-sha-1".to_string());

        mark_task_terminal(&task1);
        // task2 is still active (not terminal)!
        assert!(goal_has_active_tasks("GOAL-ACTIVE"));

        let res = cx
            .update(|cx| cleanup_graduated_goal(project.clone(), "GOAL-ACTIVE", cx))
            .await;
        assert!(
            res.is_err(),
            "cleanup must fail when sibling task is active"
        );
        let err_msg = res.unwrap_err().to_string();
        assert!(err_msg.contains("sibling task TASK-ACT-2 is still active"));

        // Now mark task2 terminal
        mark_task_terminal(&task2);
        assert!(!goal_has_active_tasks("GOAL-ACTIVE"));

        let res = cx
            .update(|cx| cleanup_graduated_goal(project.clone(), "GOAL-ACTIVE", cx))
            .await;
        assert!(
            res.is_ok(),
            "cleanup succeeds when all sibling tasks are terminal"
        );
    }

    #[test]
    fn test_acceptance_main_branch_mutation_invariant() {
        // Direct mutations / deletions targeting main or master or HEAD are strictly rejected
        assert!(assert_safe_graduation_ref("main").is_err());
        assert!(assert_safe_graduation_ref("refs/heads/main").is_err());
        assert!(assert_safe_graduation_ref("master").is_err());
        assert!(assert_safe_graduation_ref("refs/heads/master").is_err());
        // Graduation summary creation forbids targeting main
        let bad_summary = prepare_goal_graduation_summary("../main");
        assert!(bad_summary.is_err());
    }
}
