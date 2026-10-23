#[cfg(any(test, feature = "test-support"))]
use std::collections::HashSet;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(not(any(test, feature = "test-support")))]
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context as _, Result};
use fs::Fs;
use futures::FutureExt as _;
use git::repository::{AskPassDelegate, CommitOptions, CreateWorktreeTarget, RepoPath};
use agent_client_protocol::schema::v1 as acp;
use gpui::{App, AsyncApp, Entity, Task, WeakEntity};
use language_model::LanguageModelToolUseId;
use project::{
    Project, git_store::Repository, git_store::worktrees_directory_for_repo,
    project_settings::ProjectSettings,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::Settings;
use util::ResultExt;
use util::paths::PathStyle;

use crate::agent_task::AgentTaskProvider;
use crate::{AgentTaskId, AgentTaskSummary};

#[derive(Debug, thiserror::Error)]
#[error("no git repository found in project")]
pub struct NoGitRepositoryError;

pub(crate) fn system_git_binary() -> Option<PathBuf> {
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

pub fn ensure_task_worktree_interactive(
    project: Entity<Project>,
    task: &AgentTaskSummary,
    goal_id: Option<String>,
    base_branch: Option<String>,
    on_branch: Option<String>,
    cx: &mut App,
) -> Task<Result<PathBuf>> {
    ensure_task_worktree_with_options(project, task, goal_id, base_branch, on_branch, true, cx)
}

pub fn ensure_task_worktree_with_policy(
    project: Entity<Project>,
    task: &AgentTaskSummary,
    goal_id: Option<String>,
    base_branch: Option<String>,
    on_branch: Option<String>,
    cx: &mut App,
) -> Task<Result<PathBuf>> {
    ensure_task_worktree_with_options(project, task, goal_id, base_branch, on_branch, false, cx)
}

pub fn ensure_task_worktree_with_options(
    project: Entity<Project>,
    task: &AgentTaskSummary,
    goal_id: Option<String>,
    base_branch: Option<String>,
    on_branch: Option<String>,
    allow_overflow: bool,
    cx: &mut App,
) -> Task<Result<PathBuf>> {
    let task_id = task.id.clone();
    let task_title = task.title.clone();
    let goal_id = goal_id.or_else(|| task.goal_id.clone());
    let write_scopes = task.write_scopes.clone();
    cx.spawn(async move |cx| {
        record_task_title(&task_id, &task_title);
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
                    unregister_subagent_session(&self.task_id);
                    let mut reg = REGISTRY.write();
                    reg.active_guards.remove(&self.task_id);
                    reg.entries.remove(&self.task_id);
                }
            }
        }

        let mut claim_guard = ClaimBranchGuard {
            task_id: task_id.clone(),
            defused: false,
        };

        let pool_key = ProjectPoolKey::new(&anchor_path, &file_system);
        {
            let mut reg = REGISTRY.write();
            if let Some(entry) = reg.entries.get_mut(&task_id) {
                entry.pool_key = Some(pool_key.clone());
            }
        }

        let stale_tasks = stale_worktree_tasks_for_branch(&branch_to_claim, &task_id);
        for (stale_task_id, stale_path, safety, commit_error) in stale_tasks {
            match safety {
                WorktreeReleaseSafety::CommitError => {
                    let err = commit_error.as_deref().unwrap_or("unknown error");
                    anyhow::bail!(
                        "cannot release worktree for task {stale_task_id} holding branch {branch_to_claim} due to unrecovered commit error: {err}; inspect or resolve that worktree manually"
                    );
                }
                WorktreeReleaseSafety::ActiveSession => {
                    anyhow::bail!(
                        "cannot release worktree for task {stale_task_id} holding branch {branch_to_claim}: active subagent session running"
                    );
                }
                WorktreeReleaseSafety::MergeConflict | WorktreeReleaseSafety::Safe => {
                    // Allowed per design B7: merge conflicts are safe to take over at claim-time.
                }
            }

            let log_reason = format!("holding branch {branch_to_claim}");
            release_stale_task_worktree(
                &project,
                &repository,
                &file_system,
                &anchor_path,
                &stale_task_id,
                &stale_path,
                &log_reason,
                false,
                cx,
            )
            .await?;
        }

        let base_dir =
            worktrees_directory_for_repo(&anchor_path, &worktree_setting, path_style)?;
        let worktree_path =
            task_worktree_path(&anchor_path, &worktree_setting, path_style, &task_id)?;

        let path_exists = file_system.is_dir(&worktree_path).await;
        let mut slot_guard = None;
        if !path_exists {
            let guard = acquire_task_worktree_slot(
                &project,
                &repository,
                &file_system,
                &anchor_path,
                &base_dir,
                &task_id,
                allow_overflow,
                cx,
            )
            .await?;
            slot_guard = Some(guard);

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
                let exists = resolve_ref_to_sha(&git_repo, &branch_name)
                    .await?
                    .is_some();
                let target = if exists {
                    CreateWorktreeTarget::ExistingBranch {
                        branch_name: branch_name.clone(),
                    }
                } else {
                    CreateWorktreeTarget::NewBranch {
                        branch_name: branch_name.clone(),
                        base_sha: base_sha_for_worktree.clone(),
                    }
                };
                let create_task = repository.update(cx, |repository, _| {
                    repository.create_worktree(target, worktree_path.clone())
                });

                let mut res = create_task.await?;
                if let Err(error) = res {
                    let error_message = error.to_string();
                    if error_message.contains("already exists") {
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
            project.find_or_create_worktree(&worktree_path, false, cx)
        });
        let (worktree_entity, _) = match find_task.await {
            Ok(res) => res,
            Err(e) => {
                if !path_exists {
                    let remove_task = repository.update(cx, |repository, _| {
                        repository.remove_worktree(worktree_path.clone(), true)
                    });
                    remove_task.await.log_err();
                }
                unregister_task_worktree(&task_id);
                return Err(e);
            }
        };

        let worktree_id = cx.update(|cx| worktree_entity.read(cx).id());
        let suppressed = is_task_lsp_suppressed(&task_id);
        cx.update(|cx| {
            project.update(cx, |project, cx| {
                project.set_worktree_language_servers_suppressed(worktree_id, suppressed, cx);
            });
        });

        let project_id = project.entity_id();
        {
            let mut map = PROJECT_TASK_WORKTREES.write();
            map.entry(project_id)
                .or_default()
                .insert(task_id.clone(), worktree_entity);
        }

        cx.update(|cx| {
            let pool_key = pool_key.clone();
            cx.observe_release(&project, move |_, _cx| {
                let removed_tasks: Vec<AgentTaskId> = {
                    let mut map = PROJECT_TASK_WORKTREES.write();
                    map.remove(&project_id)
                        .map(|tasks| tasks.into_keys().collect())
                        .unwrap_or_default()
                };
                let mut reg = REGISTRY.write();
                for removed_task_id in &removed_tasks {
                    reg.entries.remove(removed_task_id);
                    #[cfg(any(test, feature = "test-support"))]
                    reg.task_files.remove(removed_task_id);
                }
                reg.entries
                    .retain(|_, state| state.pool_key.as_ref() != Some(&pool_key));
                IN_FLIGHT_CREATIONS.lock().remove(&pool_key);
                TASK_WORKTREE_WAITING_CREATIONS.lock().remove(&pool_key);
                NOTIFIED_WAITERS.lock().remove(&pool_key);
            })
            .detach();
        });

        finish_slot_and_register_policy(
            slot_guard,
            &task_id,
            worktree_path.clone(),
            goal_branch.clone(),
            base_ref,
            base_sha_for_worktree.clone(),
            target_checkout_branch.clone(),
            Some(pool_key),
            Some(write_scopes),
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
            } else {
                let recorded = get_task_recorded_files(&task_id);
                if !recorded.is_empty() {
                    initial_files = recorded;
                } else if let Some(ref g_branch) = goal_branch {
                    let g_id = g_branch.strip_prefix("agent-goal/").unwrap_or(g_branch);
                    initial_files = get_goal_files(g_id);
                }
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
    active_guards: HashMap<AgentTaskId, HashMap<SubagentSessionGuardId, SubagentSessionOwner>>,
    goal_history: HashMap<String, Vec<GoalMergeEntry>>,
    #[cfg(any(test, feature = "test-support"))]
    goal_files: HashMap<String, HashMap<PathBuf, Vec<u8>>>,
    #[cfg(any(test, feature = "test-support"))]
    task_files: HashMap<AgentTaskId, HashMap<PathBuf, Vec<u8>>>,
    goal_tip_shas: HashMap<String, String>,
    goal_graduations: HashMap<String, GoalGraduationSummary>,
    task_titles: HashMap<AgentTaskId, String>,
    task_sessions: HashMap<AgentTaskId, String>,
}

#[derive(Clone, Debug)]
struct GoalMergeEntry {
    #[allow(dead_code)]
    commit_sha: String,
    task_id: AgentTaskId,
    #[allow(dead_code)]
    changed_files: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ProjectPoolKey {
    anchor_path: PathBuf,
    fs_ptr: usize,
}

impl ProjectPoolKey {
    fn new(anchor_path: &Path, file_system: &Arc<dyn Fs>) -> Self {
        Self {
            anchor_path: anchor_path.to_path_buf(),
            fs_ptr: Arc::as_ptr(file_system) as *const () as usize,
        }
    }
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
    last_session_ended_at: Option<std::time::Instant>,
    pool_key: Option<ProjectPoolKey>,
    slot_notified: bool,
    is_cleaning: bool,
    pub session_id: Option<String>,
    pub write_scopes: Vec<String>,
}

impl TaskWorktreeState {
    fn checked_out_branch(&self, task_id: &AgentTaskId) -> String {
        self.checkout_target
            .clone()
            .unwrap_or_else(|| format!("agent-task/{}", task_id))
    }
}

/// Indicates whether and under what conditions a task worktree may be safely released or evicted.
///
/// Semantics:
/// - `CommitError`: NEVER release or evict; manual inspection is required.
/// - `ActiveSession`: NEVER release or evict while any subagent session is active.
/// - `MergeConflict`: Claim-time branch takeover is ALLOWED (per design В7: conflict
///   state is re-derivable, commits are safe), but startup sweep and LRU pool eviction
///   MUST skip it (retained for inspection).
/// - `Safe`: Fully safe to release, take over, or evict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeReleaseSafety {
    Safe,
    ActiveSession,
    CommitError,
    MergeConflict,
}

impl WorktreeReleaseSafety {
    pub fn is_eviction_safe(&self) -> bool {
        matches!(self, Self::Safe)
    }

    pub fn is_claim_takeover_safe(&self) -> bool {
        matches!(self, Self::Safe | Self::MergeConflict)
    }
}

fn is_non_empty_error(err: &str) -> bool {
    let trimmed = err.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed == "commit failed:" || trimmed == "staging failed:" {
        return false;
    }
    true
}

fn worktree_release_safety_for_state(
    _task_id: &AgentTaskId,
    state: &TaskWorktreeState,
    active_sessions: usize,
) -> WorktreeReleaseSafety {
    if active_sessions > 0 || state.is_cleaning {
        WorktreeReleaseSafety::ActiveSession
    } else if state.commit_error.as_deref().map_or(false, is_non_empty_error) {
        WorktreeReleaseSafety::CommitError
    } else if state.merge_conflict.is_some() {
        WorktreeReleaseSafety::MergeConflict
    } else {
        WorktreeReleaseSafety::Safe
    }
}

impl TaskWorktreeRegistry {
    pub fn live_active_sessions_count(&self, task_id: &AgentTaskId) -> usize {
        let Some(guards) = self.active_guards.get(task_id) else {
            return 0;
        };
        guards
            .values()
            .filter(|owner| is_owner_alive(owner))
            .count()
    }

    pub fn has_active_session(&self, task_id: &AgentTaskId) -> bool {
        self.live_active_sessions_count(task_id) > 0
    }
}

pub fn worktree_release_safety(task_id: &AgentTaskId) -> WorktreeReleaseSafety {
    let reg = REGISTRY.read();
    let active_sessions = reg.live_active_sessions_count(task_id);
    reg.entries.get(task_id).map_or(
        if active_sessions > 0 {
            WorktreeReleaseSafety::ActiveSession
        } else {
            WorktreeReleaseSafety::Safe
        },
        |state| worktree_release_safety_for_state(task_id, state, active_sessions),
    )
}

#[cfg(any(test, feature = "test-support"))]
struct TestThreadLocal<T: 'static>(fn() -> &'static T);

#[cfg(any(test, feature = "test-support"))]
impl<T: 'static> std::ops::Deref for TestThreadLocal<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        (self.0)()
    }
}

#[cfg(any(test, feature = "test-support"))]
macro_rules! task_worktree_static {
    ($name:ident: $ty:ty = $init:expr) => {
        static $name: TestThreadLocal<$ty> = TestThreadLocal(|| {
            thread_local! {
                static INSTANCE: &'static $ty = Box::leak(Box::new($init));
            }
            INSTANCE.with(|instance| *instance)
        });
    };
}

#[cfg(not(any(test, feature = "test-support")))]
macro_rules! task_worktree_static {
    ($name:ident: $ty:ty = $init:expr) => {
        static $name: LazyLock<$ty> = LazyLock::new(|| $init);
    };
}

task_worktree_static!(
    REGISTRY: parking_lot::RwLock<TaskWorktreeRegistry> =
        parking_lot::RwLock::new(TaskWorktreeRegistry::default())
);

task_worktree_static!(
    PROJECT_TASK_WORKTREES: parking_lot::RwLock<
        HashMap<gpui::EntityId, HashMap<AgentTaskId, Entity<project::Worktree>>>,
    > = parking_lot::RwLock::new(HashMap::default())
);

task_worktree_static!(
    TASK_LSP_SUPPRESSION: parking_lot::RwLock<HashMap<AgentTaskId, bool>> =
        parking_lot::RwLock::new(HashMap::default())
);

task_worktree_static!(
    TASK_PROVIDER: parking_lot::RwLock<Option<Arc<dyn AgentTaskProvider>>> =
        parking_lot::RwLock::new(None)
);

pub fn register_task_provider(provider: Arc<dyn AgentTaskProvider>) {
    *TASK_PROVIDER.write() = Some(provider);
}

pub fn unregister_task_provider() {
    *TASK_PROVIDER.write() = None;
}

pub fn task_provider() -> Option<Arc<dyn AgentTaskProvider>> {
    TASK_PROVIDER.read().clone()
}

pub fn set_task_lsp_suppressed(task_id: &AgentTaskId, suppressed: bool) {
    let mut map = TASK_LSP_SUPPRESSION.write();
    if suppressed {
        map.insert(task_id.clone(), true);
    } else {
        map.remove(task_id);
    }
}

pub fn is_task_lsp_suppressed(task_id: &AgentTaskId) -> bool {
    TASK_LSP_SUPPRESSION
        .read()
        .get(task_id)
        .copied()
        .unwrap_or(false)
}

pub fn clear_task_lsp_suppressed(task_id: &AgentTaskId) {
    TASK_LSP_SUPPRESSION.write().remove(task_id);
}

task_worktree_static!(
    GOAL_MERGE_LOCKS: parking_lot::Mutex<HashMap<String, Arc<futures::lock::Mutex<()>>>> =
        parking_lot::Mutex::new(HashMap::default())
);

pub(crate) fn get_goal_merge_lock(goal_id: &str) -> Arc<futures::lock::Mutex<()>> {
    let mut locks = GOAL_MERGE_LOCKS.lock();
    locks.entry(goal_id.to_string()).or_default().clone()
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_registry_for_tests() {
    let mut reg = REGISTRY.write();
    *reg = TaskWorktreeRegistry::default();
    let mut locks = GOAL_MERGE_LOCKS.lock();
    locks.clear();
    let mut project_worktrees = PROJECT_TASK_WORKTREES.write();
    project_worktrees.clear();
    let mut in_flight = IN_FLIGHT_CREATIONS.lock();
    in_flight.clear();
    let mut waiters = TASK_WORKTREE_WAITING_CREATIONS.lock();
    waiters.clear();
    let mut lsp_suppression = TASK_LSP_SUPPRESSION.write();
    lsp_suppression.clear();
    let mut notified = NOTIFIED_WAITERS.lock();
    notified.clear();
    unregister_task_provider();
}

task_worktree_static!(
    TASK_WORKTREE_WAITING_CREATIONS: parking_lot::Mutex<
        HashMap<ProjectPoolKey, VecDeque<futures::channel::oneshot::Sender<()>>>,
    > = parking_lot::Mutex::new(HashMap::default())
);

task_worktree_static!(
    IN_FLIGHT_CREATIONS: parking_lot::Mutex<HashMap<ProjectPoolKey, usize>> =
        parking_lot::Mutex::new(HashMap::default())
);

task_worktree_static!(
    NOTIFIED_WAITERS: parking_lot::Mutex<HashMap<ProjectPoolKey, usize>> =
        parking_lot::Mutex::new(HashMap::default())
);

fn notify_task_worktree_slot_available_for_pool(pool_key: &ProjectPoolKey) {
    let mut all_waiters = TASK_WORKTREE_WAITING_CREATIONS.lock();
    if let Some(waiters) = all_waiters.get_mut(pool_key) {
        while let Some(sender) = waiters.pop_front() {
            if sender.send(()).is_ok() {
                let mut notified = NOTIFIED_WAITERS.lock();
                *notified.entry(pool_key.clone()).or_insert(0) += 1;
                break;
            }
        }
        if waiters.is_empty() {
            all_waiters.remove(pool_key);
        }
    }
}

pub fn notify_task_worktree_slot_available() {
    let mut all_waiters = TASK_WORKTREE_WAITING_CREATIONS.lock();
    let mut empty_keys = Vec::new();
    for (pool_key, waiters) in all_waiters.iter_mut() {
        while let Some(sender) = waiters.pop_front() {
            if sender.send(()).is_ok() {
                let mut notified = NOTIFIED_WAITERS.lock();
                *notified.entry(pool_key.clone()).or_insert(0) += 1;
                break;
            }
        }
        if waiters.is_empty() {
            empty_keys.push(pool_key.clone());
        }
    }
    for key in empty_keys {
        all_waiters.remove(&key);
    }
}

#[derive(Debug)]
pub struct TaskWorktreeSlotGuard {
    pool_key: ProjectPoolKey,
    active: bool,
}

impl TaskWorktreeSlotGuard {
    fn new(pool_key: ProjectPoolKey) -> Self {
        Self {
            pool_key,
            active: true,
        }
    }

    pub fn finish(mut self) {
        if self.active {
            self.active = false;
            let mut in_flight = IN_FLIGHT_CREATIONS.lock();
            if let Some(count) = in_flight.get_mut(&self.pool_key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    in_flight.remove(&self.pool_key);
                }
            }
        }
    }
}

impl Drop for TaskWorktreeSlotGuard {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            let mut in_flight = IN_FLIGHT_CREATIONS.lock();
            if let Some(count) = in_flight.get_mut(&self.pool_key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    in_flight.remove(&self.pool_key);
                }
            }
            drop(in_flight);
            notify_task_worktree_slot_available_for_pool(&self.pool_key);
        }
    }
}

struct NotifiedWaiterGuard {
    pool_key: ProjectPoolKey,
    active: bool,
}

impl NotifiedWaiterGuard {
    fn new(pool_key: ProjectPoolKey) -> Self {
        Self {
            pool_key,
            active: true,
        }
    }

    fn defuse(&mut self) {
        if self.active {
            self.active = false;
            let mut notified = NOTIFIED_WAITERS.lock();
            if let Some(count) = notified.get_mut(&self.pool_key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    notified.remove(&self.pool_key);
                }
            }
        }
    }
}

impl Drop for NotifiedWaiterGuard {
    fn drop(&mut self) {
        self.defuse();
    }
}

const MAX_REMOVAL_ATTEMPTS: usize = 5;
const INITIAL_REMOVAL_BACKOFF: Duration = Duration::from_millis(50);
const MAX_REMOVAL_BACKOFF: Duration = Duration::from_millis(800);
const DEFAULT_SLOT_TIMEOUT: Duration = Duration::from_secs(45);

fn is_sharing_violation(error: &anyhow::Error) -> bool {
    for cause in error.chain() {
        if let Some(io_error) = cause.downcast_ref::<std::io::Error>() {
            if io_error.raw_os_error() == Some(32) {
                return true;
            }
        }
    }
    let message = format!("{error:#}").to_lowercase();
    message.contains("sharing violation")
}

fn removal_backoff_duration(attempt: usize) -> Duration {
    let mut backoff = INITIAL_REMOVAL_BACKOFF;
    for _ in 0..attempt {
        backoff = (backoff * 2).min(MAX_REMOVAL_BACKOFF);
    }
    backoff
}

async fn remove_worktree_from_git_and_fs(
    repository: &Entity<Repository>,
    file_system: &Arc<dyn Fs>,
    worktree_path: &Path,
    anchor_path: &Path,
    force: bool,
    description: &str,
    cx: &mut AsyncApp,
) -> Result<()> {
    let mut git_remove_result = Ok(());
    for attempt in 0..MAX_REMOVAL_ATTEMPTS {
        let remove_task = repository.update(cx, |repository, _| {
            repository.remove_worktree(worktree_path.to_path_buf(), force)
        });
        let result = match remove_task.await {
            Ok(inner_result) => inner_result,
            Err(canceled) => Err(anyhow::anyhow!("remove_worktree task canceled: {canceled}")),
        };

        match result {
            Ok(()) => {
                git_remove_result = Ok(());
                break;
            }
            Err(error) => {
                let is_sharing = is_sharing_violation(&error);
                git_remove_result = Err(error);
                if attempt + 1 < MAX_REMOVAL_ATTEMPTS && is_sharing {
                    let backoff = removal_backoff_duration(attempt);
                    cx.background_executor().timer(backoff).await;
                } else {
                    break;
                }
            }
        }
    }

    let directory_still_exists = file_system.is_dir(worktree_path).await;
    if let Err(git_error) = git_remove_result {
        log::warn!(
            "git remove_worktree for {description} at {} failed: {git_error:#}; attempting fallback filesystem removal",
            worktree_path.display()
        );
        if directory_still_exists {
            for attempt in 0..MAX_REMOVAL_ATTEMPTS {
                let remove_result = file_system
                    .remove_dir(
                        worktree_path,
                        fs::RemoveOptions {
                            recursive: true,
                            ignore_if_not_exists: true,
                        },
                    )
                    .await;
                match remove_result {
                    Ok(()) => break,
                    Err(file_system_error) => {
                        let is_sharing = is_sharing_violation(&file_system_error);
                        if attempt + 1 < MAX_REMOVAL_ATTEMPTS && is_sharing {
                            let backoff = removal_backoff_duration(attempt);
                            cx.background_executor().timer(backoff).await;
                        } else {
                            return Err(file_system_error).with_context(|| {
                                format!(
                                    "failed to remove {description} directory at {}: git remove failed ({:#}) and filesystem removal failed",
                                    worktree_path.display(),
                                    git_error
                                )
                            });
                        }
                    }
                }
            }
            prune_git_worktrees(anchor_path).await;
        }
    } else if directory_still_exists {
        for attempt in 0..MAX_REMOVAL_ATTEMPTS {
            let remove_result = file_system
                .remove_dir(
                    worktree_path,
                    fs::RemoveOptions {
                        recursive: true,
                        ignore_if_not_exists: true,
                    },
                )
                .await;
            match remove_result {
                Ok(()) => break,
                Err(file_system_error) => {
                    let is_sharing = is_sharing_violation(&file_system_error);
                    if attempt + 1 < MAX_REMOVAL_ATTEMPTS && is_sharing {
                        let backoff = removal_backoff_duration(attempt);
                        cx.background_executor().timer(backoff).await;
                    } else {
                        return Err(file_system_error).with_context(|| {
                            format!(
                                "failed to remove {description} directory at {}",
                                worktree_path.display()
                            )
                        });
                    }
                }
            }
        }
        prune_git_worktrees(anchor_path).await;
    }

    Ok(())
}

async fn release_stale_task_worktree(
    project: &Entity<Project>,
    repository: &Entity<Repository>,
    file_system: &Arc<dyn Fs>,
    anchor_path: &Path,
    stale_task_id: &AgentTaskId,
    stale_path: &Path,
    reason: &str,
    already_marked_cleaning: bool,
    cx: &mut AsyncApp,
) -> Result<()> {
    if !already_marked_cleaning {
        let mut reg = REGISTRY.write();
        if let Some(entry) = reg.entries.get_mut(stale_task_id) {
            if entry.is_cleaning {
                anyhow::bail!("task worktree for {stale_task_id} is already being cleaned up");
            }
            entry.is_cleaning = true;
        }
    }

    let perform_release = async {
        if !stale_path.as_os_str().is_empty() {
            if file_system.is_dir(stale_path).await {
                let dot_git = stale_path.join(".git");
                let has_git = file_system.is_file(&dot_git).await || file_system.is_dir(&dot_git).await;
                if has_git {
                    let salvage_details = commit_task_worktree(
                        file_system.clone(),
                        stale_path.to_path_buf(),
                        stale_task_id.to_string(),
                        false,
                        cx,
                    )
                    .await;

                    match salvage_details {
                        Ok(details) => {
                            if let Some(err) = details.commit_error {
                                anyhow::bail!(
                                    "failed to salvage uncommitted changes from stale worktree for task {stale_task_id} {reason}: {err}; inspect or resolve that worktree manually"
                                );
                            }
                        }
                        Err(err) => {
                            anyhow::bail!(
                                "failed to salvage uncommitted changes from stale worktree for task {stale_task_id} {reason}: {err}; inspect or resolve that worktree manually"
                            );
                        }
                    }
                }
            }

            let stale_worktree_id = project.read_with(cx, |project, cx| {
                project
                    .worktrees(cx)
                    .find(|w| {
                        let w_abs = w.read(cx).abs_path();
                        w_abs.as_ref() == stale_path
                            || util::paths::normalize_lexically(w_abs.as_ref())
                                .ok()
                                .as_deref()
                                == util::paths::normalize_lexically(stale_path).ok().as_deref()
                    })
                    .map(|w| w.read(cx).id())
            });
            {
                let mut project_worktrees = PROJECT_TASK_WORKTREES.write();
                for map in project_worktrees.values_mut() {
                    map.remove(stale_task_id);
                }
            }
            if let Some(id) = stale_worktree_id {
                let wait = project.update(cx, |project, cx| {
                    let wait = project.wait_for_worktree_release(id, cx);
                    project.remove_worktree(id, cx);
                    wait
                });
                wait.await.log_err();
            }

            if file_system.is_dir(stale_path).await {
                remove_worktree_from_git_and_fs(
                    repository,
                    file_system,
                    stale_path,
                    anchor_path,
                    true,
                    "stale worktree",
                    cx,
                )
                .await?;

                if !file_system.is_fake() {
                    prune_git_worktrees(anchor_path).await;
                }
            }
        }

        unregister_task_worktree(stale_task_id);
        log::info!("Released stale worktree of task {stale_task_id} {reason}");
        Ok(())
    };

    let result = perform_release.await;
    if result.is_err() {
        let mut reg = REGISTRY.write();
        if let Some(entry) = reg.entries.get_mut(stale_task_id) {
            entry.is_cleaning = false;
        }
    }
    result
}

async fn acquire_task_worktree_slot(
    project: &Entity<Project>,
    repository: &Entity<Repository>,
    file_system: &Arc<dyn Fs>,
    anchor_path: &Path,
    base_dir: &Path,
    current_task_id: &AgentTaskId,
    allow_overflow: bool,
    cx: &mut AsyncApp,
) -> Result<TaskWorktreeSlotGuard> {
    let pool_key = ProjectPoolKey::new(anchor_path, file_system);
    let mut is_retry = false;
    let mut notified_guard: Option<NotifiedWaiterGuard> = None;

    loop {
        let (limit, cleanup_enabled) = project.read_with(cx, |project, cx| {
            let settings = agent_settings::AgentSettings::get_for_project(project, cx);
            (settings.task_worktree_limit, settings.cleanup)
        });

        let mut snapshotted_ids = collections::HashSet::default();
        let entries: Vec<(
            AgentTaskId,
            PathBuf,
            WorktreeReleaseSafety,
            Option<std::time::Instant>,
        )> = {
            let reg = REGISTRY.read();
            reg.entries
                .iter()
                .filter(|(_, state)| {
                    state.pool_key.as_ref() == Some(&pool_key)
                        && (state.worktree_path.as_os_str().is_empty()
                            || state.worktree_path.starts_with(&base_dir))
                })
                .map(|(id, state)| {
                    snapshotted_ids.insert(id.clone());
                    let active = reg.live_active_sessions_count(id);
                    let safety = worktree_release_safety_for_state(id, state, active);
                    (
                        id.clone(),
                        state.worktree_path.clone(),
                        safety,
                        state.last_session_ended_at,
                    )
                })
                .collect()
        };

        let mut confirmed_existing_ids = collections::HashSet::default();
        let mut candidates = Vec::new();
        for (id, path, safety, last_session_ended_at) in &entries {
            if id == current_task_id {
                continue;
            }
            if !path.as_os_str().is_empty() && file_system.is_dir(path).await {
                confirmed_existing_ids.insert(id.clone());
                if cleanup_enabled && safety.is_eviction_safe() {
                    candidates.push((id.clone(), path.clone(), *last_session_ended_at));
                }
            }
        }

        let holder_info: Vec<(AgentTaskId, &'static str)> = entries
            .iter()
            .filter(|(id, _, _, _)| id != current_task_id && confirmed_existing_ids.contains(id))
            .map(|(id, _, safety, _)| {
                let reason = match safety {
                    WorktreeReleaseSafety::ActiveSession => "active session",
                    WorktreeReleaseSafety::CommitError => "commit error",
                    WorktreeReleaseSafety::MergeConflict => "merge conflict",
                    WorktreeReleaseSafety::Safe => "other",
                };
                (id.clone(), reason)
            })
            .collect();

        // Re-validate existing worktrees and in-flight count under lock to prevent over-limit races.
        let slot_allocated = {
            let reg = REGISTRY.read();
            let mut in_flight_map = IN_FLIGHT_CREATIONS.lock();
            let confirmed_count = confirmed_existing_ids
                .iter()
                .filter(|id| reg.entries.contains_key(*id))
                .count();
            let newly_registered = reg
                .entries
                .iter()
                .filter(|(id, state)| {
                    !snapshotted_ids.contains(id)
                        && state.pool_key.as_ref() == Some(&pool_key)
                        && !state.worktree_path.as_os_str().is_empty()
                })
                .count();
            let in_flight = in_flight_map.get(&pool_key).copied().unwrap_or(0);
            if confirmed_count + newly_registered + in_flight < limit {
                *in_flight_map.entry(pool_key.clone()).or_insert(0) += 1;
                true
            } else {
                false
            }
        };

        if slot_allocated {
            drop(notified_guard);
            return Ok(TaskWorktreeSlotGuard::new(pool_key));
        }

        candidates.sort_by(|a, b| match (a.2, b.2) {
            (Some(time_a), Some(time_b)) => time_a.cmp(&time_b),
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });

        let mut evicted = false;
        for (candidate_id, candidate_path, _) in candidates {
            log::info!("LRU eviction attempting release of worktree for task {candidate_id}");
            let evict_result = release_stale_task_worktree(
                project,
                repository,
                file_system,
                anchor_path,
                &candidate_id,
                &candidate_path,
                "due to LRU pool limit",
                false,
                cx,
            )
            .await;

            match evict_result {
                Ok(()) => {
                    confirmed_existing_ids.remove(&candidate_id);
                    let allocated_after_evict = {
                        let reg = REGISTRY.read();
                        let mut in_flight_map = IN_FLIGHT_CREATIONS.lock();
                        let confirmed_count = confirmed_existing_ids
                            .iter()
                            .filter(|id| reg.entries.contains_key(*id))
                            .count();
                        let newly_registered = reg
                            .entries
                            .iter()
                            .filter(|(id, state)| {
                                !snapshotted_ids.contains(id)
                                    && state.pool_key.as_ref() == Some(&pool_key)
                                    && !state.worktree_path.as_os_str().is_empty()
                            })
                            .count();
                        let in_flight = in_flight_map.get(&pool_key).copied().unwrap_or(0);
                        if confirmed_count + newly_registered + in_flight < limit {
                            *in_flight_map.entry(pool_key.clone()).or_insert(0) += 1;
                            true
                        } else {
                            false
                        }
                    };
                    if allocated_after_evict {
                        evicted = true;
                        break;
                    }
                }
                Err(err) => {
                    log::warn!(
                        "LRU eviction: failed to salvage/release worktree for task {candidate_id}: {err:#}; skipping"
                    );
                }
            }
        }

        if evicted {
            drop(notified_guard);
            return Ok(TaskWorktreeSlotGuard::new(pool_key));
        }

        if allow_overflow {
            drop(notified_guard);
            {
                let mut in_flight_map = IN_FLIGHT_CREATIONS.lock();
                *in_flight_map.entry(pool_key.clone()).or_insert(0) += 1;
            }
            return Ok(TaskWorktreeSlotGuard::new(pool_key));
        }

        drop(notified_guard.take());

        let (sender, receiver) = futures::channel::oneshot::channel();
        {
            let mut all_waiters = TASK_WORKTREE_WAITING_CREATIONS.lock();
            let waiters = all_waiters.entry(pool_key.clone()).or_default();
            if is_retry {
                waiters.push_front(sender);
            } else {
                waiters.push_back(sender);
            }
        }
        is_retry = true;

        let mut receiver = receiver.fuse();
        let mut timer = cx.background_executor().timer(DEFAULT_SLOT_TIMEOUT).fuse();

        let slot_available = futures::select! {
            _ = receiver => true,
            _ = timer => false,
        };

        if !slot_available {
            {
                let mut all_waiters = TASK_WORKTREE_WAITING_CREATIONS.lock();
                if let Some(waiters) = all_waiters.get_mut(&pool_key) {
                    waiters.retain(|sender| !sender.is_canceled());
                    if waiters.is_empty() {
                        all_waiters.remove(&pool_key);
                    }
                }
            }

            unregister_subagent_session(current_task_id);
            {
                let mut reg = REGISTRY.write();
                reg.active_guards.remove(current_task_id);
                reg.entries.remove(current_task_id);
            }

            let mut holder_descriptions: Vec<String> = holder_info
                .iter()
                .map(|(id, reason)| format!("{id} ({reason})"))
                .collect();
            holder_descriptions.sort();
            let holders_summary = if holder_descriptions.is_empty() {
                "none".to_string()
            } else {
                holder_descriptions.join(", ")
            };

            anyhow::bail!(
                "timed out waiting for task worktree slot after 45s (pool limit: {limit}, holders: [{holders_summary}]); consider increasing `task_worktree_limit` setting"
            );
        }

        notified_guard = Some(NotifiedWaiterGuard::new(pool_key.clone()));
    }
}

pub fn register_task_worktree(task_id: &AgentTaskId, worktree_path: PathBuf) {
    register_task_worktree_policy(task_id, worktree_path, None, None, None, None);
}

fn finish_slot_and_register_policy(
    guard: Option<TaskWorktreeSlotGuard>,
    task_id: &AgentTaskId,
    worktree_path: PathBuf,
    goal_branch: Option<String>,
    base_ref: Option<String>,
    base_sha: Option<String>,
    checkout_target: Option<String>,
    pool_key: Option<ProjectPoolKey>,
    write_scopes: Option<Vec<String>>,
) {
    let mut reg = REGISTRY.write();
    let mut in_flight = IN_FLIGHT_CREATIONS.lock();

    if let Some(mut guard) = guard {
        guard.active = false;
        if let Some(count) = in_flight.get_mut(&guard.pool_key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                in_flight.remove(&guard.pool_key);
            }
        }
    }

    let has_active = reg.has_active_session(task_id);
    let initial_ended_at = if has_active {
        None
    } else {
        Some(std::time::Instant::now())
    };
    let session_from_map = reg.task_sessions.get(task_id).cloned();
    let entry = reg.entries.entry(task_id.clone()).or_default();
    let preserved_pool_key = pool_key.or_else(|| {
        if entry.worktree_path == worktree_path {
            entry.pool_key.clone()
        } else {
            None
        }
    });
    let preserved_session_id = entry.session_id.clone().or(session_from_map);
    let preserved_write_scopes = match write_scopes {
        Some(scopes) if !scopes.is_empty() => scopes,
        Some(scopes) => {
            if entry.write_scopes.is_empty() {
                scopes
            } else {
                entry.write_scopes.clone()
            }
        }
        None => entry.write_scopes.clone(),
    };
    *entry = TaskWorktreeState {
        worktree_path,
        commit_error: None,
        cleanup_error: None,
        is_terminal: false,
        goal_branch,
        base_ref,
        base_sha,
        merge_conflict: None,
        changed_files: Vec::new(),
        isolation_details: None,
        checkout_target,
        last_session_ended_at: initial_ended_at,
        pool_key: preserved_pool_key,
        slot_notified: false,
        is_cleaning: false,
        session_id: preserved_session_id,
        write_scopes: preserved_write_scopes,
    };
}

pub fn register_task_worktree_policy(
    task_id: &AgentTaskId,
    worktree_path: PathBuf,
    goal_branch: Option<String>,
    base_ref: Option<String>,
    base_sha: Option<String>,
    checkout_target: Option<String>,
) {
    register_task_worktree_policy_with_scopes(
        task_id,
        worktree_path,
        goal_branch,
        base_ref,
        base_sha,
        checkout_target,
        None,
    );
}

pub fn register_task_worktree_policy_with_scopes(
    task_id: &AgentTaskId,
    worktree_path: PathBuf,
    goal_branch: Option<String>,
    base_ref: Option<String>,
    base_sha: Option<String>,
    checkout_target: Option<String>,
    write_scopes: Option<Vec<String>>,
) {
    finish_slot_and_register_policy(
        None,
        task_id,
        worktree_path,
        goal_branch,
        base_ref,
        base_sha,
        checkout_target,
        None,
        write_scopes,
    );
}

pub fn claim_branch_or_error(
    branch: &str,
    task_id: &AgentTaskId,
    checkout_target: Option<String>,
) -> Result<()> {
    let mut reg = REGISTRY.write();
    let active_sessions = reg.live_active_sessions_count(task_id);
    if active_sessions > 1 {
        anyhow::bail!(
            "task {task_id} already has an active session; wait for it to finish or resume via session_id later"
        );
    }
    for (other_task_id, entry) in &reg.entries {
        if other_task_id != task_id {
            let occupied = entry.checked_out_branch(other_task_id);
            if occupied == branch {
                let active = reg.live_active_sessions_count(other_task_id);
                let safety = worktree_release_safety_for_state(other_task_id, entry, active);
                match safety {
                    WorktreeReleaseSafety::ActiveSession => {
                        anyhow::bail!(
                            "branch {branch} is checked out by task {other_task_id}; serialize or use on_branch after it completes"
                        );
                    }
                    WorktreeReleaseSafety::CommitError => {
                        let err = entry.commit_error.as_deref().unwrap_or("unknown error");
                        anyhow::bail!(
                            "cannot release worktree for task {other_task_id} holding branch {branch} due to unrecovered commit error: {err}; inspect or resolve that worktree manually"
                        );
                    }
                    WorktreeReleaseSafety::MergeConflict | WorktreeReleaseSafety::Safe => {
                        // Allowed per design B7: merge conflicts are safe to take over at claim-time.
                    }
                }
            }
        }
    }
    let initial_ended_at = if active_sessions == 0 {
        Some(std::time::Instant::now())
    } else {
        None
    };
    let session_from_map = reg.task_sessions.get(task_id).cloned();
    let entry = reg.entries.entry(task_id.clone()).or_default();
    let preserved_session_id = entry.session_id.clone().or(session_from_map);
    let preserved_write_scopes = entry.write_scopes.clone();
    *entry = TaskWorktreeState {
        checkout_target,
        is_terminal: false,
        last_session_ended_at: initial_ended_at,
        session_id: preserved_session_id,
        write_scopes: preserved_write_scopes,
        ..Default::default()
    };
    Ok(())
}

fn stale_worktree_tasks_for_branch(
    branch: &str,
    current_task_id: &AgentTaskId,
) -> Vec<(AgentTaskId, PathBuf, WorktreeReleaseSafety, Option<String>)> {
    let reg = REGISTRY.read();
    reg.entries
        .iter()
        .filter(|(other_id, entry)| {
            *other_id != current_task_id && entry.checked_out_branch(other_id) == branch
        })
        .map(|(other_id, entry)| {
            let active = reg.live_active_sessions_count(other_id);
            let safety = worktree_release_safety_for_state(other_id, entry, active);
            (
                other_id.clone(),
                entry.worktree_path.clone(),
                safety,
                entry.commit_error.clone(),
            )
        })
        .collect()
}

pub fn record_task_title(task_id: &AgentTaskId, title: &str) {
    // Store only the first non-empty trimmed line so commit subjects stay single-line.
    if let Some(first_line) = title.lines().map(str::trim).find(|line| !line.is_empty()) {
        REGISTRY
            .write()
            .task_titles
            .insert(task_id.clone(), first_line.to_string());
    }
}

pub fn task_worktree_title(task_id: &AgentTaskId) -> Option<String> {
    REGISTRY.read().task_titles.get(task_id).cloned()
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
    reg.entries
        .get(task_id)
        .map(|e| e.worktree_path.clone())
        .filter(|p| !p.as_os_str().is_empty())
}

pub fn task_worktree_anchor_path(task_worktree: &Path) -> Option<PathBuf> {
    let reg = REGISTRY.read();
    reg.entries.values().find_map(|state| {
        if state.worktree_path == task_worktree {
            state.pool_key.as_ref().map(|key| key.anchor_path.clone())
        } else {
            None
        }
    })
}

pub fn register_task_session(task_id: &AgentTaskId, session_id: String) {
    let mut reg = REGISTRY.write();
    reg.task_sessions
        .insert(task_id.clone(), session_id.clone());
    if let Some(entry) = reg.entries.get_mut(task_id) {
        entry.session_id = Some(session_id);
    } else {
        reg.entries.insert(
            task_id.clone(),
            TaskWorktreeState {
                session_id: Some(session_id),
                ..Default::default()
            },
        );
    }
}

pub fn task_session_id_for_id(task_id: &AgentTaskId) -> Option<String> {
    let reg = REGISTRY.read();
    reg.entries
        .get(task_id)
        .and_then(|e| e.session_id.clone())
        .or_else(|| reg.task_sessions.get(task_id).cloned())
}

pub fn record_task_worktree_changed_files(task_id: &AgentTaskId, files: Vec<String>) {
    let mut reg = REGISTRY.write();
    if let Some(entry) = reg.entries.get_mut(task_id) {
        entry.changed_files = files;
    }
}

pub fn task_worktree_changed_files(task_id: &AgentTaskId) -> Vec<String> {
    let reg = REGISTRY.read();
    reg.entries
        .get(task_id)
        .map(|e| e.changed_files.clone())
        .unwrap_or_default()
}

pub fn task_worktree_write_scopes(task_id: &AgentTaskId) -> Vec<String> {
    let reg = REGISTRY.read();
    reg.entries
        .get(task_id)
        .map(|e| e.write_scopes.clone())
        .unwrap_or_default()
}

pub fn record_task_worktree_write_scopes(task_id: &AgentTaskId, scopes: Vec<String>) {
    let mut reg = REGISTRY.write();
    if let Some(entry) = reg.entries.get_mut(task_id) {
        entry.write_scopes = scopes;
    }
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
    clear_task_lsp_suppressed(task_id);
    let mut reg = REGISTRY.write();
    let entry = reg.entries.remove(task_id);
    reg.active_guards.remove(task_id);
    let mut project_worktrees = PROJECT_TASK_WORKTREES.write();
    for map in project_worktrees.values_mut() {
        map.remove(task_id);
    }
    drop(reg);
    drop(project_worktrees);
    if let Some(pool_key) = entry.and_then(|e| e.pool_key) {
        let already_in_flight = {
            let notified = NOTIFIED_WAITERS.lock();
            notified.get(&pool_key).copied().unwrap_or(0)
        };
        if already_in_flight == 0 {
            notify_task_worktree_slot_available_for_pool(&pool_key);
        }
    } else {
        notify_task_worktree_slot_available();
    }
}

pub fn record_task_worktree_commit_error(task_id: &AgentTaskId, error: String) {
    if !is_non_empty_error(&error) {
        clear_task_worktree_commit_error(task_id);
        return;
    }
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
        .filter(|e| is_non_empty_error(e))
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

static NEXT_GUARD_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SubagentSessionGuardId(usize);

#[derive(Clone, Debug)]
pub struct SubagentSessionOwner {
    pub thread_id: acp::SessionId,
    pub tool_use_id: LanguageModelToolUseId,
    pub thread: Option<WeakEntity<crate::Thread>>,
    #[cfg(any(test, feature = "test-support"))]
    pub is_test_live: bool,
}

pub fn is_owner_alive(owner: &SubagentSessionOwner) -> bool {
    if let Some(ref thread) = owner.thread {
        if thread.upgrade().is_none() {
            return false;
        }
        crate::thread::is_thread_turn_running(&owner.thread_id)
    } else {
        #[cfg(any(test, feature = "test-support"))]
        {
            owner.is_test_live || crate::thread::is_thread_turn_running(&owner.thread_id)
        }
        #[cfg(not(any(test, feature = "test-support")))]
        {
            crate::thread::is_thread_turn_running(&owner.thread_id)
        }
    }
}

pub struct SubagentSessionGuard {
    task_id: AgentTaskId,
    guard_id: SubagentSessionGuardId,
}

impl SubagentSessionGuard {
    pub fn new(task_id: AgentTaskId, owner: SubagentSessionOwner) -> Self {
        register_subagent_session_guard(&task_id, owner)
    }

    pub fn task_id(&self) -> &AgentTaskId {
        &self.task_id
    }

    pub fn id(&self) -> SubagentSessionGuardId {
        self.guard_id
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test_new(task_id: AgentTaskId) -> Self {
        let owner = SubagentSessionOwner {
            thread_id: acp::SessionId::new("test-thread"),
            tool_use_id: LanguageModelToolUseId::from("test-tool-use"),
            thread: None,
            is_test_live: true,
        };
        register_subagent_session_guard(&task_id, owner)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test_new_stale(task_id: AgentTaskId) -> Self {
        let owner = SubagentSessionOwner {
            thread_id: acp::SessionId::new("test-stale-thread"),
            tool_use_id: LanguageModelToolUseId::from("test-stale-tool-use"),
            thread: None,
            is_test_live: false,
        };
        register_subagent_session_guard(&task_id, owner)
    }
}

impl Drop for SubagentSessionGuard {
    fn drop(&mut self) {
        unregister_subagent_session_guard(&self.task_id, self.guard_id);
    }
}

pub fn register_subagent_session_guard(
    task_id: &AgentTaskId,
    owner: SubagentSessionOwner,
) -> SubagentSessionGuard {
    let guard_id = SubagentSessionGuardId(NEXT_GUARD_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let mut reg = REGISTRY.write();
    let guards = reg.active_guards.entry(task_id.clone()).or_default();
    guards.insert(guard_id, owner);
    if let Some(entry) = reg.entries.get_mut(task_id) {
        entry.last_session_ended_at = None;
        entry.slot_notified = false;
    }
    SubagentSessionGuard {
        task_id: task_id.clone(),
        guard_id,
    }
}

pub fn unregister_subagent_session_guard(
    task_id: &AgentTaskId,
    guard_id: SubagentSessionGuardId,
) {
    let mut reg = REGISTRY.write();
    let mut session_ended = false;
    let mut pool_key = None;
    let (live_count, is_empty) = if let Some(guards) = reg.active_guards.get_mut(task_id) {
        guards.remove(&guard_id);
        (
            guards.values().filter(|o| is_owner_alive(o)).count(),
            guards.is_empty(),
        )
    } else {
        (0, false)
    };
    if is_empty {
        reg.active_guards.remove(task_id);
    }
    if live_count == 0 {
        if let Some(entry) = reg.entries.get_mut(task_id) {
            entry.last_session_ended_at = Some(std::time::Instant::now());
            session_ended = true;
            pool_key = entry.pool_key.clone();
        }
    }
    drop(reg);

    if session_ended {
        if let Some(pool_key) = pool_key {
            let already_in_flight = {
                let notified = NOTIFIED_WAITERS.lock();
                notified.get(&pool_key).copied().unwrap_or(0)
            };
            if already_in_flight == 0 {
                notify_task_worktree_slot_available_for_pool(&pool_key);
            }
        } else {
            notify_task_worktree_slot_available();
        }
    }
}

pub fn sweep_guards_for_thread(thread_id: &acp::SessionId) {
    let mut reg = REGISTRY.write();
    let mut ended_pools = Vec::new();
    let mut tasks_to_clean = Vec::new();
    let mut ended_tasks = Vec::new();

    for (task_id, guards) in reg.active_guards.iter_mut() {
        let before_live = guards.values().filter(|o| is_owner_alive(o)).count();
        guards.retain(|_, owner| owner.thread_id != *thread_id);
        let after_live = guards.values().filter(|o| is_owner_alive(o)).count();

        if before_live > 0 && after_live == 0 {
            ended_tasks.push(task_id.clone());
        }
        if guards.is_empty() {
            tasks_to_clean.push(task_id.clone());
        }
    }

    for task_id in tasks_to_clean {
        reg.active_guards.remove(&task_id);
    }

    for task_id in ended_tasks {
        if let Some(entry) = reg.entries.get_mut(&task_id) {
            entry.last_session_ended_at = Some(std::time::Instant::now());
            if let Some(ref pk) = entry.pool_key {
                ended_pools.push(pk.clone());
            }
        }
    }

    drop(reg);

    for pool_key in ended_pools {
        let already_in_flight = {
            let notified = NOTIFIED_WAITERS.lock();
            notified.get(&pool_key).copied().unwrap_or(0)
        };
        if already_in_flight == 0 {
            notify_task_worktree_slot_available_for_pool(&pool_key);
        }
    }
}

pub fn sweep_guards_for_tool_use(
    thread_id: &acp::SessionId,
    tool_use_id: &LanguageModelToolUseId,
) {
    let mut reg = REGISTRY.write();
    let mut ended_pools = Vec::new();
    let mut tasks_to_clean = Vec::new();
    let mut ended_tasks = Vec::new();

    for (task_id, guards) in reg.active_guards.iter_mut() {
        let before_live = guards.values().filter(|o| is_owner_alive(o)).count();
        guards.retain(|_, owner| !(owner.thread_id == *thread_id && owner.tool_use_id == *tool_use_id));
        let after_live = guards.values().filter(|o| is_owner_alive(o)).count();

        if before_live > 0 && after_live == 0 {
            ended_tasks.push(task_id.clone());
        }
        if guards.is_empty() {
            tasks_to_clean.push(task_id.clone());
        }
    }

    for task_id in tasks_to_clean {
        reg.active_guards.remove(&task_id);
    }

    for task_id in ended_tasks {
        if let Some(entry) = reg.entries.get_mut(&task_id) {
            entry.last_session_ended_at = Some(std::time::Instant::now());
            if let Some(ref pk) = entry.pool_key {
                ended_pools.push(pk.clone());
            }
        }
    }

    drop(reg);

    for pool_key in ended_pools {
        let already_in_flight = {
            let notified = NOTIFIED_WAITERS.lock();
            notified.get(&pool_key).copied().unwrap_or(0)
        };
        if already_in_flight == 0 {
            notify_task_worktree_slot_available_for_pool(&pool_key);
        }
    }
}

pub fn register_subagent_session(task_id: &AgentTaskId) {
    let owner = SubagentSessionOwner {
        thread_id: acp::SessionId::new("legacy-test-thread"),
        tool_use_id: LanguageModelToolUseId::from("legacy-test-tool-use"),
        thread: None,
        #[cfg(any(test, feature = "test-support"))]
        is_test_live: true,
    };
    register_subagent_session_guard(task_id, owner);
}

pub fn unregister_subagent_session(task_id: &AgentTaskId) {
    let mut reg = REGISTRY.write();
    let mut session_ended = false;
    let mut pool_key = None;
    let (live_count, is_empty) = if let Some(guards) = reg.active_guards.get_mut(task_id) {
        if let Some(&guard_id) = guards.keys().next() {
            guards.remove(&guard_id);
        }
        (
            guards.values().filter(|o| is_owner_alive(o)).count(),
            guards.is_empty(),
        )
    } else {
        (0, false)
    };
    if is_empty {
        reg.active_guards.remove(task_id);
    }
    if live_count == 0 {
        if let Some(entry) = reg.entries.get_mut(task_id) {
            entry.last_session_ended_at = Some(std::time::Instant::now());
            session_ended = true;
            pool_key = entry.pool_key.clone();
        }
    }
    drop(reg);

    if session_ended {
        if let Some(pool_key) = pool_key {
            let already_in_flight = {
                let notified = NOTIFIED_WAITERS.lock();
                notified.get(&pool_key).copied().unwrap_or(0)
            };
            if already_in_flight == 0 {
                notify_task_worktree_slot_available_for_pool(&pool_key);
            }
        } else {
            notify_task_worktree_slot_available();
        }
    }
}

pub fn has_active_subagent_session(task_id: &AgentTaskId) -> bool {
    let reg = REGISTRY.read();
    reg.has_active_session(task_id)
}

#[cfg(any(test, feature = "test-support"))]
pub fn active_guards_count(task_id: &AgentTaskId) -> usize {
    let reg = REGISTRY.read();
    reg.active_guards.get(task_id).map_or(0, |m| m.len())
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
        {
            let mut project_worktrees = PROJECT_TASK_WORKTREES.write();
            for map in project_worktrees.values_mut() {
                map.remove(&task_id);
            }
        }
        if let Some(id) = worktree_id {
            let wait = project.update(cx, |project, cx| {
                let wait = project.wait_for_worktree_release(id, cx);
                project.remove_worktree(id, cx);
                wait
            });
            wait.await.log_err();
        }

        if file_system.is_dir(&worktree_path).await {
            remove_worktree_from_git_and_fs(
                &repository,
                &file_system,
                &worktree_path,
                &anchor_path,
                force,
                "task worktree",
                cx,
            )
            .await?;
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
    let task = remove_task_worktree_internal(project, task_id, true, cx);
    cx.spawn(async move |_cx| {
        task.await?;
        unregister_task_worktree(&task_id_clone);
        Ok(())
    })
}

async fn prune_git_worktrees(anchor_path: &std::path::Path) {
    if anchor_path.is_relative() || !anchor_path.exists() {
        return;
    }
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
    let cleanup_enabled = project.read_with(cx, |project, cx| {
        let s = agent_settings::AgentSettings::get_for_project(project, cx);
        s.task_lifecycle_mode != agent_settings::TaskLifecycleMode::Agent && s.cleanup
    });
    if !cleanup_enabled {
        return Task::ready(Ok(false));
    }

    if task_worktree_path_for_id(task_id).is_none() {
        return Task::ready(Ok(false));
    }

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

    {
        let mut reg = REGISTRY.write();
        if let Some(entry) = reg.entries.get_mut(task_id) {
            if entry.is_cleaning {
                return Task::ready(Ok(false));
            }
            entry.is_cleaning = true;
        }
    }

    let task_id_clone = task_id.clone();
    cx.spawn(async move |cx| {
        let (repository, anchor_path, _path_style, _worktree_setting, file_system) = project
            .update(cx, |project, cx| {
                anyhow::Ok(task_worktree_context(project, cx))
            })??;
        let worktree_path = task_worktree_path_for_id(&task_id_clone)
            .unwrap_or_else(|| PathBuf::new());
        if let Err(e) = release_stale_task_worktree(
            &project,
            &repository,
            &file_system,
            &anchor_path,
            &task_id_clone,
            &worktree_path,
            "auto-cleanup",
            true,
            cx,
        )
        .await
        {
            let err_msg = e.to_string();
            record_task_worktree_cleanup_error(&task_id_clone, err_msg);
            return Err(e);
        }
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
        for task in &tasks {
            record_task_title(&task.id, &task.title);
        }
        let (repository, anchor_path, path_style, worktree_setting, file_system) = project
            .update(cx, |project, cx| {
                anyhow::Ok(task_worktree_context(project, cx))
            })??;

        let base_dir = worktrees_directory_for_repo(&anchor_path, &worktree_setting, path_style)?;
        if !file_system.is_dir(&base_dir).await {
            return Ok(Vec::new());
        }

        let mut swept = Vec::new();
        for task in &tasks {
            if !task.status.is_terminal() {
                continue;
            }

            let task_id = &task.id;

            let safety = worktree_release_safety(task_id);
            if !safety.is_eviction_safe() {
                log::info!("Startup sweep: skipping {task_id} due to release safety: {safety:?}");
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

        let pool_key = ProjectPoolKey::new(&anchor_path, &file_system);
        let limit = project.read_with(cx, |project, cx| {
            agent_settings::AgentSettings::get_for_project(project, cx).task_worktree_limit
        });

        let mut existing_tasks = Vec::new();
        let mut seen_ids = collections::HashSet::default();

        let reg_entries: Vec<(
            AgentTaskId,
            PathBuf,
            WorktreeReleaseSafety,
            Option<std::time::Instant>,
        )> = {
            let reg = REGISTRY.read();
            reg.entries
                .iter()
                .filter(|(_, state)| {
                    state
                        .pool_key
                        .as_ref()
                        .map_or(true, |pk| pk == &pool_key)
                })
                .map(|(id, state)| {
                    let active = reg.live_active_sessions_count(id);
                    let safety = worktree_release_safety_for_state(id, state, active);
                    (
                        id.clone(),
                        state.worktree_path.clone(),
                        safety,
                        state.last_session_ended_at,
                    )
                })
                .collect()
        };

        for (id, path, safety, last_session_ended_at) in reg_entries {
            if !path.as_os_str().is_empty() && file_system.is_dir(&path).await {
                seen_ids.insert(id.clone());
                existing_tasks.push((id, path, safety, last_session_ended_at));
            }
        }

        for task in &tasks {
            if seen_ids.contains(&task.id) {
                continue;
            }
            if let Ok(worktree_path) =
                task_worktree_path(&anchor_path, &worktree_setting, path_style, &task.id)
            {
                if file_system.is_dir(&worktree_path).await {
                    seen_ids.insert(task.id.clone());
                    let safety = worktree_release_safety(&task.id);
                    let last_ended = {
                        let mut reg = REGISTRY.write();
                        let entry = reg.entries.entry(task.id.clone()).or_default();
                        entry.pool_key = Some(pool_key.clone());
                        entry.worktree_path = worktree_path.clone();
                        entry.last_session_ended_at
                    };
                    existing_tasks.push((
                        task.id.clone(),
                        worktree_path,
                        safety,
                        last_ended,
                    ));
                }
            }
        }

        if existing_tasks.len() > limit {
            let num_to_evict = existing_tasks.len() - limit;
            let mut candidates: Vec<_> = existing_tasks
                .into_iter()
                .filter(|(_, _, safety, _)| safety.is_eviction_safe())
                .collect();

            candidates.sort_by(|a, b| match (a.3, b.3) {
                (Some(time_a), Some(time_b)) => time_a.cmp(&time_b),
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            });

            let mut evicted_count = 0;
            for (candidate_id, candidate_path, _, _) in candidates {
                if evicted_count >= num_to_evict {
                    break;
                }
                log::info!("Startup sweep: LRU evicting stale worktree for task {candidate_id}");
                let evict_res = release_stale_task_worktree(
                    &project,
                    &repository,
                    &file_system,
                    &anchor_path,
                    &candidate_id,
                    &candidate_path,
                    "startup sweep LRU eviction",
                    false,
                    cx,
                )
                .await;

                match evict_res {
                    Ok(()) => {
                        swept.push(candidate_id);
                        evicted_count += 1;
                    }
                    Err(err) => {
                        log::warn!(
                            "Startup sweep: failed to LRU evict worktree for task {candidate_id}: {err:#}; skipping"
                        );
                    }
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

fn is_path_in_write_scopes(path: &RepoPath, scopes: &[String]) -> bool {
    let valid_scopes: Vec<Arc<str>> = scopes
        .iter()
        .map(|scope| scope.trim())
        .filter(|scope| !scope.is_empty())
        .map(Arc::from)
        .collect();

    if valid_scopes.is_empty() {
        return false;
    }

    let Ok(write_scopes) = agent_settings::WriteScopes::new(valid_scopes) else {
        return false;
    };

    if write_scopes.is_match(path) {
        return true;
    }

    for scope in scopes {
        let trimmed_scope = scope.trim().trim_start_matches("./");
        if let Some((_root_name, relative_pattern)) = trimmed_scope.split_once('/') {
            let relative_pattern = relative_pattern.trim_matches('/');
            if !relative_pattern.is_empty() {
                if let Ok(sub_scopes) =
                    agent_settings::WriteScopes::new(vec![Arc::from(relative_pattern)])
                {
                    if sub_scopes.is_match(path) {
                        return true;
                    }
                }
            }
        }
    }

    false
}

pub const GIT_LOCK_RETRY_DELAY: Duration = Duration::from_millis(500);

pub fn is_git_lock_contention(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("index.lock")
        || lower.contains(".lock")
        || (lower.contains("unable to create") && lower.contains("lock"))
        || lower.contains("file is locked")
        || lower.contains("being used by another process")
        || lower.contains("could not lock")
        || lower.contains("cannot lock")
        || lower.contains("failed to lock")
        || lower.contains("resource temporarily unavailable")
        || lower.contains("another git process seems to be running")
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

    let status = match git_repo.status(std::slice::from_ref(&prefix)).await {
        Ok(status) => status,
        Err(e) => {
            let err_str = e.to_string();
            if is_git_lock_contention(&err_str) {
                cx.background_executor().timer(GIT_LOCK_RETRY_DELAY).await;
                match git_repo.status(&[prefix]).await {
                    Ok(status) => status,
                    Err(retry_err) => {
                        let head_sha = git_repo.head_sha().await;
                        return Ok(SubagentIsolationDetails {
                            mode: mode.to_string(),
                            branch,
                            base_branch,
                            head_sha,
                            changed_files: 0,
                            commit_error: Some(format!("git lock contention: {retry_err}")),
                            merged_into: None,
                            merge_conflict: None,
                        });
                    }
                }
            } else {
                let head_sha = git_repo.head_sha().await;
                return Ok(SubagentIsolationDetails {
                    mode: mode.to_string(),
                    branch,
                    base_branch,
                    head_sha,
                    changed_files: 0,
                    commit_error: Some(err_str),
                    merged_into: None,
                    merge_conflict: None,
                });
            }
        }
    };

    let write_scopes = task_worktree_write_scopes(&task_id_typed);
    let paths: Vec<RepoPath> = status
        .entries
        .iter()
        .filter_map(|(path, _)| {
            let unix_str = path.as_unix_str().replace('\\', "/");
            let clean_path = unix_str.trim_start_matches("./").trim_start_matches('/');
            let is_injected_settings = clean_path == ".zed/settings.json";
            if is_injected_settings && !is_path_in_write_scopes(path, &write_scopes) {
                None
            } else {
                Some(path.clone())
            }
        })
        .collect();

    if paths.is_empty() {
        record_task_worktree_changed_files(&task_id_typed, Vec::new());
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

    let changed_files = paths.len();
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
        .stage_paths(paths.clone(), Arc::new(HashMap::default()))
        .await
    {
        let err_str = e.to_string();
        let retry_res = if is_git_lock_contention(&err_str) {
            cx.background_executor().timer(GIT_LOCK_RETRY_DELAY).await;
            git_repo.stage_paths(paths, Arc::new(HashMap::default())).await
        } else {
            Err(e)
        };
        if let Err(final_err) = retry_res {
            let head_sha = git_repo.head_sha().await;
            let err_trimmed = final_err.to_string();
            let commit_error = if is_non_empty_error(&err_trimmed) {
                Some(format!("staging failed: {final_err}"))
            } else {
                None
            };
            return Ok(SubagentIsolationDetails {
                mode: mode.to_string(),
                branch,
                base_branch,
                head_sha,
                changed_files: 0,
                commit_error,
                merged_into: None,
                merge_conflict: None,
            });
        }
    }

    let commit_message = if is_error {
        format!("WIP: {branch} error checkpoint")
    } else {
        let recorded_title = task_worktree_title(&task_id_typed);
        if let Some(title) = recorded_title {
            title
        } else {
            let mut fetched_title = None;
            if let Some(provider) = task_provider() {
                let task = cx.update(|cx| provider.get_task(&task_id_typed, cx));
                if let Ok(detail) = task.await {
                    let summary_title = detail.summary.title.trim().to_string();
                    if !summary_title.is_empty() {
                        record_task_title(&task_id_typed, &summary_title);
                        fetched_title = task_worktree_title(&task_id_typed);
                    }
                }
            }
            fetched_title.unwrap_or_else(|| format!("{branch}: completed task work"))
        }
    };

    let askpass = AskPassDelegate::new(cx, |_, _, _| {});

    if let Err(e) = git_repo
        .commit(
            commit_message.clone().into(),
            None,
            CommitOptions::default(),
            askpass,
            Arc::new(HashMap::default()),
        )
        .await
    {
        let err_str = e.to_string();
        let retry_res = if is_git_lock_contention(&err_str) {
            cx.background_executor().timer(GIT_LOCK_RETRY_DELAY).await;
            let retry_askpass = AskPassDelegate::new(cx, |_, _, _| {});
            git_repo
                .commit(
                    commit_message.into(),
                    None,
                    CommitOptions::default(),
                    retry_askpass,
                    Arc::new(HashMap::default()),
                )
                .await
        } else {
            Err(e)
        };
        if let Err(final_err) = retry_res {
            let head_sha = git_repo.head_sha().await;
            let err_trimmed = final_err.to_string();
            let commit_error = if is_non_empty_error(&err_trimmed) {
                Some(format!("commit failed: {final_err}"))
            } else {
                None
            };
            return Ok(SubagentIsolationDetails {
                mode: mode.to_string(),
                branch,
                base_branch,
                head_sha,
                changed_files: 0,
                commit_error,
                merged_into: None,
                merge_conflict: None,
            });
        }
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
                let merge_result = match merge_task.await {
                    Ok(outcome) => Ok(outcome),
                    Err(e) => {
                        let err_str = e.to_string();
                        if is_git_lock_contention(&err_str) {
                            cx.background_executor().timer(GIT_LOCK_RETRY_DELAY).await;
                            let retry_task = cx.update(|cx| {
                                merge_task_branch_into_goal(project.clone(), &task_id, cx)
                            });
                            retry_task.await
                        } else {
                            Err(e)
                        }
                    }
                };
                match merge_result {
                    Ok(MergeOutcome::NoGoal) => {
                        details.merged_into = None;
                        details.merge_conflict = None;
                    }
                    Ok(MergeOutcome::FastForward { target_ref, sha })
                    | Ok(MergeOutcome::Merged { target_ref, sha }) => {
                        details.merged_into = Some(MergedInto { target_ref, sha });
                        details.merge_conflict = None;
                        clear_task_worktree_merge_conflict(&task_id);
                    }
                    Ok(MergeOutcome::Conflict(conflict)) => {
                        details.merged_into = None;
                        details.merge_conflict = Some(conflict.files.clone());
                        record_task_worktree_merge_conflict(&task_id, conflict.files);
                    }
                    Err(err) => {
                        let err_str = format!("merge failed: {err:#}");
                        let commit_error = if is_non_empty_error(&err.to_string()) {
                            Some(err_str)
                        } else {
                            None
                        };
                        details.merged_into = None;
                        details.merge_conflict = None;
                        details.commit_error = commit_error.clone();
                        if let Some(err_to_record) = commit_error {
                            record_task_worktree_commit_error(&task_id, err_to_record);
                        } else {
                            clear_task_worktree_commit_error(&task_id);
                        }
                    }
                }
            }
        }

        record_task_worktree_isolation_details(&task_id, details.clone());
        let should_publish = project.read_with(cx, |project, cx| {
            let s = agent_settings::AgentSettings::get_for_project(project, cx);
            s.task_lifecycle_mode != agent_settings::TaskLifecycleMode::Agent
                && s.publish_ref_artifact
        });
        if should_publish {
            let project_clone = project.clone();
            let publish_task = cx
                .update(|cx| publish_task_ref_artifact_to_tgs(project_clone, &task_id, &details, cx));
            publish_task.await.log_err();
        }

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

pub fn prepare_goal_graduation_summary(
    goal_id: &str,
    base_branch: &str,
) -> Result<GoalGraduationSummary> {
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
        "## Summary\n\nGoal `{goal_id_str}` graduation into `{base_branch}`.\n\n"
    ));
    body.push_str(&format!("- Source branch: `{goal_branch}`\n"));
    body.push_str(&format!("- Target branch: `{base_branch}`\n"));
    if let Some(sha) = &tip_sha {
        let short_sha: String = sha.chars().take(7).collect();
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
        "gh pr create --base {} --head {} --title \"{}\" --body \"{}\"",
        base_branch,
        goal_branch,
        title,
        body.replace('\"', "\\\"").replace('\n', "\\n")
    );

    let pr_proposal = PrProposal {
        title,
        body,
        source_branch: goal_branch.clone(),
        target_branch: base_branch.to_string(),
        command: pr_cmd,
    };

    let summary = GoalGraduationSummary {
        goal_id: goal_id_str,
        goal_branch: goal_branch.clone(),
        base_branch: base_branch.to_string(),
        refs: GoalRefs {
            goal_branch: goal_branch.clone(),
            goal_ref: format!("refs/heads/{goal_branch}"),
            base_branch: base_branch.to_string(),
            base_ref: format!("refs/heads/{base_branch}"),
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
    let configured_base_branch = project.read_with(cx, |project, cx| {
        agent_settings::AgentSettings::get_for_project(project, cx)
            .goal_graduation_base_branch
            .clone()
    });
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

        let base_branch = if let Some(base) = configured_base_branch {
            base
        } else if let Ok(Some(default_branch)) = git_repo.default_branch(false).await {
            default_branch.to_string()
        } else {
            "main".to_string()
        };

        let summary = prepare_goal_graduation_summary(&goal_id, &base_branch)?;
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
    pub skipped_branches: Vec<String>,
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
    let (delete_goal_branch, delete_task_branches) = project.read_with(cx, |project, cx| {
        let s = agent_settings::AgentSettings::get_for_project(project, cx);
        (
            s.delete_goal_branches_on_graduation,
            s.delete_task_branches_on_graduation,
        )
    });
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

        let mut branches_to_delete = Vec::new();
        let mut skipped_branches = Vec::new();
        if delete_goal_branch {
            branches_to_delete.push(goal_branch.clone());
        } else {
            skipped_branches.push(goal_branch.clone());
        }
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
            if delete_task_branches {
                branches_to_delete.push(task_branch);
            } else {
                skipped_branches.push(task_branch);
            }
        }

        branches_to_delete.sort();
        branches_to_delete.dedup();
        skipped_branches.sort();
        skipped_branches.dedup();

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
            skipped_branches,
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
    use gpui::{BorrowAppContext, TestAppContext};
    use project::project_settings::ProjectSettings;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        reset_registry_for_tests();
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            ProjectSettings::register(cx);
            agent_settings::AgentSettings::register(cx);
        });
    }

    #[test]
    fn test_subagent_session_guard_drop_unregisters() {
        reset_registry_for_tests();
        let task_id = AgentTaskId::from("TASK-GUARD-TEST");
        assert!(!has_active_subagent_session(&task_id));

        {
            let _guard = SubagentSessionGuard::test_new(task_id.clone());
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
    async fn test_task_worktree_is_invisible_in_project(cx: &mut TestAppContext) {
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
            id: AgentTaskId::from("TASK-INVIS-1"),
            parent_id: None,
            goal_id: None,
            title: "Invisible worktree task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec!["src/".to_string()],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        let initial_visible_count =
            project.read_with(cx, |project, cx| project.visible_worktrees(cx).count());
        assert_eq!(initial_visible_count, 1);

        let worktree_path = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(project.clone(), &task, None, None, None, cx)
            })
            .await
            .expect("task worktree should be created");

        // After ensuring task worktree, it exists on disk
        assert!(fs.is_dir(&worktree_path).await);

        // The task worktree must NOT be in visible_worktrees
        let visible_worktrees = project.read_with(cx, |project, cx| {
            project
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            visible_worktrees.len(),
            1,
            "task worktree should not increase visible worktrees count"
        );
        assert!(
            !visible_worktrees.iter().any(|path| {
                path == &worktree_path
                    || util::paths::normalize_lexically(path).ok()
                        == util::paths::normalize_lexically(&worktree_path).ok()
            }),
            "task worktree must not be among project visible worktrees"
        );

        // Invisible task worktrees are retained in project worktrees (worktree_store) for buffers/LSP/tools
        let all_worktrees = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                .collect::<Vec<_>>()
        });
        assert!(
            all_worktrees.iter().any(|path| {
                path == &worktree_path
                    || util::paths::normalize_lexically(path).ok()
                        == util::paths::normalize_lexically(&worktree_path).ok()
            }),
            "task worktree must be present in project worktrees"
        );

        let cleaned = cx
            .update(|cx| auto_cleanup_task_worktree(project.clone(), &task.id, cx))
            .await
            .expect("auto cleanup should succeed");
        assert!(cleaned, "worktree should be cleaned up");

        // After cleanup, worktree is removed from project worktrees
        let remaining_worktrees = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                .collect::<Vec<_>>()
        });
        assert!(
            !remaining_worktrees.iter().any(|path| {
                path == &worktree_path
                    || util::paths::normalize_lexically(path).ok()
                        == util::paths::normalize_lexically(&worktree_path).ok()
            }),
            "task worktree should be removed from project worktrees after auto-cleanup"
        );

        // After cleanup, worktree directory is removed from disk
        assert!(!fs.is_dir(&worktree_path).await);
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            id: AgentTaskId::from("TASK-ENS-3"),
            parent_id: None,
            goal_id: None,
            title: "Task 3".to_string(),
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
            id: AgentTaskId::from("TASK-ENS-4"),
            parent_id: None,
            goal_id: None,
            title: "Task 4".to_string(),
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
            id: AgentTaskId::from("TASK-ENS-5"),
            parent_id: None,
            goal_id: None,
            title: "Task 5".to_string(),
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        // Task 1 checks out shared-branch
        let guard1 = SubagentSessionGuard::test_new(task1.id.clone());
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

        drop(guard1);

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
    async fn test_regression_merge_conflict_on_branch_takeover_releases_stale_worktree(
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
                .insert("refs/heads/main".into(), "main-sha-reg".into());
            state.refs.insert("HEAD".into(), "main-sha-reg".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let task_b = AgentTaskSummary {
            id: AgentTaskId::from("TASK-B"),
            parent_id: None,
            goal_id: Some("GOAL-REG".to_string()),
            title: "Task B".to_string(),
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

        let task_a = AgentTaskSummary {
            id: AgentTaskId::from("TASK-A"),
            parent_id: None,
            goal_id: Some("GOAL-REG".to_string()),
            title: "Task A".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        let task_rec = AgentTaskSummary {
            id: AgentTaskId::from("TASK-REC"),
            parent_id: None,
            goal_id: None,
            title: "Recovery Task".to_string(),
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

        // Both Task B and Task A create worktrees branched from the same initial goal state
        let path_b = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task_b,
                    Some("GOAL-REG".to_string()),
                    None,
                    None,
                    cx,
                )
            })
            .await
            .unwrap();

        let guard_a = SubagentSessionGuard::test_new(task_a.id.clone());
        let path_a = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task_a,
                    Some("GOAL-REG".to_string()),
                    None,
                    None,
                    cx,
                )
            })
            .await
            .unwrap();

        fs.save(
            &path_b.join("conflict.txt"),
            &"task b goal edits".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        fs.save(
            &path_a.join("conflict.txt"),
            &"task a conflicting edits".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        // Task B commits and merges first, advancing the goal branch
        let details_b = cx
            .update(|cx| commit_and_merge_task_worktree(project.clone(), &task_b.id, false, cx))
            .await
            .unwrap();
        assert!(details_b.merged_into.is_some());

        // Task A commits and attempts to merge, producing a merge conflict
        let details_a = cx
            .update(|cx| commit_and_merge_task_worktree(project.clone(), &task_a.id, false, cx))
            .await
            .unwrap();
        assert_eq!(details_a.merged_into, None);
        assert_eq!(
            details_a.merge_conflict,
            Some(vec!["conflict.txt".to_string()])
        );

        // Session guard drops, but task A is NOT marked terminal
        drop(guard_a);
        assert!(!is_task_terminal(&task_a.id));
        assert!(fs.is_dir(&path_a).await);

        // A new spawn with on_branch "agent-task/TASK-A" succeeds!
        let path_rec = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task_rec,
                    None,
                    None,
                    Some(format!("agent-task/{}", task_a.id)),
                    cx,
                )
            })
            .await
            .unwrap();

        // Task A's worktree was released, and TASK-REC's worktree now exists
        assert!(!fs.is_dir(&path_a).await);
        assert!(fs.is_dir(&path_rec).await);
    }

    #[gpui::test]
    async fn test_branch_busy_by_active_sessions_not_terminal_status(cx: &mut TestAppContext) {
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
                .insert("refs/heads/main".into(), "main-sha-busy".into());
            state.refs.insert("HEAD".into(), "main-sha-busy".into());
            state.branches.insert("main".into());
            state
                .refs
                .insert("refs/heads/branch-c".into(), "branch-c-sha".into());
            state.branches.insert("branch-c".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let task1 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-C-1"),
            parent_id: None,
            goal_id: None,
            title: "Task C1".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        let task2 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-C-2"),
            parent_id: None,
            goal_id: None,
            title: "Task C2".to_string(),
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

        // Task 1 checks out branch-c with an active session guard
        let guard1 = SubagentSessionGuard::test_new(task1.id.clone());
        let path1 = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task1,
                    None,
                    None,
                    Some("branch-c".to_string()),
                    cx,
                )
            })
            .await
            .unwrap();
        assert!(fs.is_dir(&path1).await);

        // While Task 1's session is active, Task 2 is blocked
        let err2 = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task2,
                    None,
                    None,
                    Some("branch-c".to_string()),
                    cx,
                )
            })
            .await
            .unwrap_err();
        assert_eq!(
            err2.to_string(),
            "branch branch-c is checked out by task TASK-C-1; serialize or use on_branch after it completes"
        );

        // Task 1's session guard drops, but Task 1 is still non-terminal
        drop(guard1);
        assert!(!is_task_terminal(&task1.id));

        // Now Task 2 can claim branch-c because Task 1 is inactive
        let path2 = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task2,
                    None,
                    None,
                    Some("branch-c".to_string()),
                    cx,
                )
            })
            .await
            .unwrap();
        assert!(fs.is_dir(&path2).await);
        assert!(!fs.is_dir(&path1).await);
    }

    #[gpui::test]
    async fn test_commit_error_worktree_never_released_on_claim(cx: &mut TestAppContext) {
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
                .insert("refs/heads/main".into(), "main-sha-err".into());
            state.refs.insert("HEAD".into(), "main-sha-err".into());
            state.branches.insert("main".into());
            state
                .refs
                .insert("refs/heads/branch-d".into(), "branch-d-sha".into());
            state.branches.insert("branch-d".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let task1 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-D-1"),
            parent_id: None,
            goal_id: None,
            title: "Task D1".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        let task2 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-D-2"),
            parent_id: None,
            goal_id: None,
            title: "Task D2".to_string(),
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

        let path1 = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task1,
                    None,
                    None,
                    Some("branch-d".to_string()),
                    cx,
                )
            })
            .await
            .unwrap();
        assert!(fs.is_dir(&path1).await);

        // Record a commit error for Task 1
        record_task_worktree_commit_error(&task1.id, "disk full during commit".to_string());

        // Task 1 has zero active sessions, but has a commit error.
        // Task 2 attempts to claim branch-d: must fail with explicit error and NOT evict Task 1.
        let result = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task2,
                    None,
                    None,
                    Some("branch-d".to_string()),
                    cx,
                )
            })
            .await;

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("cannot release worktree for task TASK-D-1 holding branch branch-d due to unrecovered commit error: disk full during commit; inspect or resolve that worktree manually"),
            "unexpected error message: {err_msg}"
        );

        // Task 1's worktree directory must NOT be evicted
        assert!(fs.is_dir(&path1).await);
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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

        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.context_servers.insert(
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
                        }
                        .into(),
                    );
                });
            });
        });

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
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
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
    async fn test_acceptance_cleanup_graduated_goal_skips_branches_per_settings(
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
                "refs/heads/agent-goal/GOAL-SKIP-CLEANUP".into(),
                "goal-sha-skip".into(),
            );
            state.branches.insert("agent-goal/GOAL-SKIP-CLEANUP".into());

            state.refs.insert(
                "refs/heads/agent-task/TASK-SKIP-1".into(),
                "task-skip-1-sha".into(),
            );
            state.branches.insert("agent-task/TASK-SKIP-1".into());

            state.refs.insert(
                "refs/heads/agent-task/TASK-SKIP-2".into(),
                "task-skip-2-sha".into(),
            );
            state.branches.insert("agent-task/TASK-SKIP-2".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let task1 = AgentTaskId::from("TASK-SKIP-1");
        let task2 = AgentTaskId::from("TASK-SKIP-2");

        register_task_worktree_policy(
            &task1,
            PathBuf::from("/fake/skip1"),
            Some("agent-goal/GOAL-SKIP-CLEANUP".to_string()),
            None,
            None,
            None,
        );
        register_task_worktree_policy(
            &task2,
            PathBuf::from("/fake/skip2"),
            Some("agent-goal/GOAL-SKIP-CLEANUP".to_string()),
            None,
            None,
            None,
        );
        record_goal_tip_sha("GOAL-SKIP-CLEANUP", "goal-sha-skip".to_string());
        mark_task_terminal(&task1);
        mark_task_terminal(&task2);

        let _ = cx
            .update(|cx| prepare_goal_graduation(project.clone(), "GOAL-SKIP-CLEANUP", cx))
            .await
            .unwrap();
        assert!(get_goal_graduation_summary("GOAL-SKIP-CLEANUP").is_some());

        // Configure both branch deletion settings to false
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    let agent = settings.agent.get_or_insert_default();
                    agent.delete_goal_branches_on_graduation = Some(false);
                    agent.delete_task_branches_on_graduation = Some(false);
                });
            });
        });

        let cleanup_result = cx
            .update(|cx| cleanup_graduated_goal(project.clone(), "GOAL-SKIP-CLEANUP", cx))
            .await
            .unwrap();

        // 1. Result reports skipped branches and no deleted branches
        assert!(cleanup_result.deleted_branches.is_empty());
        assert_eq!(
            cleanup_result.skipped_branches,
            vec![
                "agent-goal/GOAL-SKIP-CLEANUP".to_string(),
                "agent-task/TASK-SKIP-1".to_string(),
                "agent-task/TASK-SKIP-2".to_string(),
            ]
        );

        // 2. Branches survived in the fake repository
        let (has_goal, has_t1, has_t2, main_sha, head_sha) = fs
            .with_git_state(dot_git, false, |state| {
                let g = state.branches.contains("agent-goal/GOAL-SKIP-CLEANUP")
                    && state
                        .refs
                        .contains_key("refs/heads/agent-goal/GOAL-SKIP-CLEANUP");
                let t1 = state.branches.contains("agent-task/TASK-SKIP-1")
                    && state.refs.contains_key("refs/heads/agent-task/TASK-SKIP-1");
                let t2 = state.branches.contains("agent-task/TASK-SKIP-2")
                    && state.refs.contains_key("refs/heads/agent-task/TASK-SKIP-2");
                let m = state.refs.get("refs/heads/main").cloned();
                let h = state.refs.get("HEAD").cloned();
                (g, t1, t2, m, h)
            })
            .unwrap();

        assert!(has_goal, "goal branch must survive when deletion is disabled");
        assert!(has_t1, "task 1 branch must survive when deletion is disabled");
        assert!(has_t2, "task 2 branch must survive when deletion is disabled");
        assert_eq!(main_sha, Some("main-sha-preserved-forever".to_string()));
        assert_eq!(head_sha, Some("main-sha-preserved-forever".to_string()));

        // 3. REGISTRY entries are still removed unconditionally
        assert!(get_goal_graduation_summary("GOAL-SKIP-CLEANUP").is_none());
        assert!(goal_tip_sha("GOAL-SKIP-CLEANUP").is_none());
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
        let bad_summary = prepare_goal_graduation_summary("../main", "main");
        assert!(bad_summary.is_err());
    }

    #[gpui::test]
    async fn test_pool_limit_and_fifo_wait_queue(cx: &mut TestAppContext) {
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
                .insert("refs/heads/main".into(), "main-sha-pool".into());
            state.refs.insert("HEAD".into(), "main-sha-pool".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        // Set limit = 2
        cx.update(|cx| {
            cx.update_global(|store: &mut settings::SettingsStore, cx| {
                store
                    .set_user_settings(r#"{ "agent": { "task_worktree_limit": 2 } }"#, cx)
                    .unwrap();
            });
        });

        let task1 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-POOL-1"),
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
            session_id: None,
            metrics: None,
        };
        let task2 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-POOL-2"),
            parent_id: None,
            goal_id: None,
            title: "Task 2".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        let task3 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-POOL-3"),
            parent_id: None,
            goal_id: None,
            title: "Task 3".to_string(),
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
        let task4 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-POOL-4"),
            parent_id: None,
            goal_id: None,
            title: "Task 4".to_string(),
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

        // Task 1 and Task 2 active sessions
        let guard1 = SubagentSessionGuard::test_new(task1.id.clone());
        let path1 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task1, cx))
            .await
            .unwrap();
        assert!(fs.is_dir(&path1).await);

        let guard2 = SubagentSessionGuard::test_new(task2.id.clone());
        let path2 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task2, cx))
            .await
            .unwrap();
        assert!(fs.is_dir(&path2).await);

        // Pool is saturated (2 worktrees with active sessions, limit=2).
        // Task 3 ensure is spawned in background — should block!
        let mut ensure_task3 = cx.spawn({
            let project = project.clone();
            let task3 = task3.clone();
            |cx| async move {
                cx.update(|cx| ensure_task_worktree(project, &task3, cx))
                    .await
            }
        });

        // Run until parked — task3 cannot progress because pool is full
        cx.run_until_parked();
        assert!(
            (&mut ensure_task3).now_or_never().is_none(),
            "task3 must block waiting for slot"
        );

        // Spawn Task 4 — should also block behind Task 3 (FIFO order)
        let mut ensure_task4 = cx.spawn({
            let project = project.clone();
            let task4 = task4.clone();
            |cx| async move {
                cx.update(|cx| ensure_task_worktree(project, &task4, cx))
                    .await
            }
        });

        cx.run_until_parked();
        assert!(
            (&mut ensure_task4).now_or_never().is_none(),
            "task4 must also block"
        );

        // Now Task 1 drops session guard and cleans up (freeing a slot)
        drop(guard1);
        mark_task_terminal(&task1.id);
        let cleaned1 = cx
            .update(|cx| auto_cleanup_task_worktree(project.clone(), &task1.id, cx))
            .await
            .unwrap();
        assert!(cleaned1);
        assert!(!fs.is_dir(&path1).await);

        // Run until parked — Task 3 (first waiter) should proceed!
        cx.run_until_parked();
        let path3 = (&mut ensure_task3)
            .now_or_never()
            .expect("task3 should have completed after slot freed")
            .unwrap();
        assert!(fs.is_dir(&path3).await);

        // Task 4 must STILL be blocked because pool is at capacity again (Task 2 + Task 3 = 2)
        assert!(
            (&mut ensure_task4).now_or_never().is_none(),
            "task4 must still block after task3 took slot"
        );

        // Now Task 2 finishes and cleans up
        drop(guard2);
        mark_task_terminal(&task2.id);
        let cleaned2 = cx
            .update(|cx| auto_cleanup_task_worktree(project.clone(), &task2.id, cx))
            .await
            .unwrap();
        assert!(cleaned2);

        // Now Task 4 should proceed
        cx.run_until_parked();
        let path4 = (&mut ensure_task4)
            .now_or_never()
            .expect("task4 should have completed after second slot freed")
            .unwrap();
        assert!(fs.is_dir(&path4).await);
    }

    #[gpui::test]
    async fn test_pool_lru_eviction(cx: &mut TestAppContext) {
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
                .insert("refs/heads/main".into(), "main-sha-lru".into());
            state.refs.insert("HEAD".into(), "main-sha-lru".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        // Set limit = 2
        cx.update(|cx| {
            cx.update_global(|store: &mut settings::SettingsStore, cx| {
                store
                    .set_user_settings(r#"{ "agent": { "task_worktree_limit": 2 } }"#, cx)
                    .unwrap();
            });
        });

        let task1 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-LRU-1"),
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
            session_id: None,
            metrics: None,
        };
        let task2 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-LRU-2"),
            parent_id: None,
            goal_id: None,
            title: "Task 2".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        let task3 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-LRU-3"),
            parent_id: None,
            goal_id: None,
            title: "Task 3".to_string(),
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

        // Create Task 1 with guard
        let guard1 = SubagentSessionGuard::test_new(task1.id.clone());
        let path1 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task1, cx))
            .await
            .unwrap();

        fs.save(
            &path1.join("file1.txt"),
            &"content 1".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let commit_res1 = cx
            .update(|cx| commit_and_merge_task_worktree(project.clone(), &task1.id, false, cx))
            .await
            .unwrap();
        assert!(commit_res1.head_sha.is_some());

        let guard2 = SubagentSessionGuard::test_new(task2.id.clone());
        let path2 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task2, cx))
            .await
            .unwrap();

        drop(guard1);
        drop(guard2);

        assert!(!has_active_subagent_session(&task1.id));
        assert!(!has_active_subagent_session(&task2.id));
        assert!(fs.is_dir(&path1).await);
        assert!(fs.is_dir(&path2).await);

        let path3 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task3, cx))
            .await
            .unwrap();

        // Task 1 worktree was removed from disk, entry unregistered
        assert!(
            !fs.is_dir(&path1).await,
            "task 1 worktree must be evicted from disk"
        );
        assert!(
            task_worktree_path_for_id(&task1.id).is_none(),
            "task 1 must be unregistered from registry"
        );

        // Task 2 worktree is preserved (more recently used)
        assert!(fs.is_dir(&path2).await, "task 2 worktree must be preserved");

        // Task 3 worktree is created
        assert!(fs.is_dir(&path3).await, "task 3 worktree must be created");

        // Task 1's branch and commits are preserved in git!
        let dot_git = std::path::Path::new(path!("/root/.git"));
        let branch1_preserved = fs
            .with_git_state(dot_git, false, |state| {
                state.branches.contains("agent-task/TASK-LRU-1")
                    || state.refs.contains_key("refs/heads/agent-task/TASK-LRU-1")
            })
            .unwrap();
        assert!(
            branch1_preserved,
            "task 1 git branch must be preserved upon eviction"
        );
    }

    #[gpui::test]
    async fn test_pool_eviction_safety_rules(cx: &mut TestAppContext) {
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
                .insert("refs/heads/main".into(), "main-sha-safe".into());
            state.refs.insert("HEAD".into(), "main-sha-safe".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        // Set limit = 2
        cx.update(|cx| {
            cx.update_global(|store: &mut settings::SettingsStore, cx| {
                store
                    .set_user_settings(r#"{ "agent": { "task_worktree_limit": 2 } }"#, cx)
                    .unwrap();
            });
        });

        let task_err = AgentTaskSummary {
            id: AgentTaskId::from("TASK-SAFE-ERR"),
            parent_id: None,
            goal_id: None,
            title: "Task Error".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        let task_dirty = AgentTaskSummary {
            id: AgentTaskId::from("TASK-SAFE-DIRTY"),
            parent_id: None,
            goal_id: None,
            title: "Task Dirty".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        let task_new = AgentTaskSummary {
            id: AgentTaskId::from("TASK-SAFE-NEW"),
            parent_id: None,
            goal_id: None,
            title: "Task New".to_string(),
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

        // Task Err is created, guard dropped, but has commit_error
        let guard_err = SubagentSessionGuard::test_new(task_err.id.clone());
        let path_err = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task_err, cx))
            .await
            .unwrap();
        drop(guard_err);
        record_task_worktree_commit_error(&task_err.id, "disk quota exceeded".to_string());

        // Task Dirty is created, guard dropped, has uncommitted dirty file
        let guard_dirty = SubagentSessionGuard::test_new(task_dirty.id.clone());
        let path_dirty = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task_dirty, cx))
            .await
            .unwrap();
        drop(guard_dirty);

        fs.save(
            &path_dirty.join("uncommitted_salvage.txt"),
            &"precious edits".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        // Now spawn Task New:
        // Pool is full (task_err + task_dirty = 2, limit = 2).
        // Candidate 1 (task_err) has commit_error -> SKIPPED (never evicted)!
        // Candidate 2 (task_dirty) is dirty -> salvage-committed before eviction, then evicted!
        let path_new = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task_new, cx))
            .await
            .unwrap();

        // Task Err must NOT have been evicted
        assert!(
            fs.is_dir(&path_err).await,
            "worktree with commit_error must never be evicted"
        );
        assert!(task_worktree_commit_error(&task_err.id).is_some());

        // Task Dirty was evicted
        assert!(
            !fs.is_dir(&path_dirty).await,
            "dirty worktree should be evicted after salvage"
        );

        // Task Dirty's uncommitted edits were salvaged to its branch!
        let recorded = get_task_recorded_files(&task_dirty.id);
        assert!(
            recorded.contains_key(std::path::Path::new("uncommitted_salvage.txt")),
            "dirty edits must be salvage-committed to task branch before eviction"
        );
        assert_eq!(
            recorded[std::path::Path::new("uncommitted_salvage.txt")],
            b"precious edits"
        );

        let dirty_dot_git = std::path::Path::new(path!("/root/.git"));
        let branch_preserved = fs
            .with_git_state(dirty_dot_git, false, |state| {
                state.branches.contains("agent-task/TASK-SAFE-DIRTY")
                    || state
                        .refs
                        .contains_key("refs/heads/agent-task/TASK-SAFE-DIRTY")
            })
            .unwrap();
        assert!(
            branch_preserved,
            "task branch must be preserved upon salvage eviction"
        );

        // Task New worktree is successfully created
        assert!(fs.is_dir(&path_new).await);
    }

    #[gpui::test]
    async fn test_pool_active_session_never_evicted(cx: &mut TestAppContext) {
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
                .insert("refs/heads/main".into(), "main-sha-act".into());
            state.refs.insert("HEAD".into(), "main-sha-act".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        // Set limit = 1
        cx.update(|cx| {
            cx.update_global(|store: &mut settings::SettingsStore, cx| {
                store
                    .set_user_settings(r#"{ "agent": { "task_worktree_limit": 1 } }"#, cx)
                    .unwrap();
            });
        });

        let task1 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-ACT-SAFE-1"),
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
            session_id: None,
            metrics: None,
        };
        let task2 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-ACT-SAFE-2"),
            parent_id: None,
            goal_id: None,
            title: "Task 2".to_string(),
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

        // Task 1 has active session guard
        let guard1 = SubagentSessionGuard::test_new(task1.id.clone());
        let path1 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task1, cx))
            .await
            .unwrap();
        assert!(fs.is_dir(&path1).await);

        // Task 2 attempts to ensure: pool limit is 1, Task 1 is active -> CANNOT be evicted -> Task 2 blocks!
        let mut ensure_task2 = cx.spawn({
            let project = project.clone();
            let task2 = task2.clone();
            |cx| async move {
                cx.update(|cx| ensure_task_worktree(project, &task2, cx))
                    .await
            }
        });

        cx.run_until_parked();
        assert!(
            (&mut ensure_task2).now_or_never().is_none(),
            "task2 must block because active task1 cannot be evicted"
        );
        assert!(fs.is_dir(&path1).await, "active task1 must not be evicted");

        drop(guard1);
        mark_task_terminal(&task1.id);
        let cleaned1 = cx
            .update(|cx| auto_cleanup_task_worktree(project.clone(), &task1.id, cx))
            .await
            .unwrap();
        assert!(cleaned1);

        cx.run_until_parked();
        let path2 = (&mut ensure_task2)
            .now_or_never()
            .expect("task2 must proceed after cleanup")
            .unwrap();
        assert!(fs.is_dir(&path2).await);
    }

    #[gpui::test]
    async fn test_pool_allow_overflow_bypasses_slot_wait(cx: &mut TestAppContext) {
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
                .insert("refs/heads/main".into(), "main-sha-overflow".into());
            state.refs.insert("HEAD".into(), "main-sha-overflow".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        cx.update(|cx| {
            cx.update_global(|store: &mut settings::SettingsStore, cx| {
                store
                    .set_user_settings(r#"{ "agent": { "task_worktree_limit": 1 } }"#, cx)
                    .unwrap();
            });
        });

        let task1 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-OVERFLOW-1"),
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
            session_id: None,
            metrics: None,
        };
        let task2 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-OVERFLOW-2"),
            parent_id: None,
            goal_id: None,
            title: "Task 2".to_string(),
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
        let task3 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-OVERFLOW-3"),
            parent_id: None,
            goal_id: None,
            title: "Task 3".to_string(),
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

        let _guard1 = SubagentSessionGuard::test_new(task1.id.clone());
        let path1 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task1, cx))
            .await
            .unwrap();
        assert!(fs.is_dir(&path1).await);

        let path2 = cx
            .update(|cx| {
                ensure_task_worktree_with_options(
                    project.clone(),
                    &task2,
                    None,
                    None,
                    None,
                    true,
                    cx,
                )
            })
            .await
            .unwrap();
        assert!(fs.is_dir(&path2).await);
        assert_ne!(path1, path2);
        assert!(fs.is_dir(&path1).await);

        let mut ensure_task3 = cx.spawn({
            let project = project.clone();
            let task3 = task3.clone();
            |cx| async move {
                cx.update(|cx| {
                    ensure_task_worktree_with_options(project, &task3, None, None, None, false, cx)
                })
                .await
            }
        });
        cx.run_until_parked();
        assert!(
            (&mut ensure_task3).now_or_never().is_none(),
            "task3 with allow_overflow=false must block when pool is full"
        );
    }

    #[gpui::test]
    async fn test_ensure_task_worktree_restores_existing_task_branch(cx: &mut TestAppContext) {
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
                .insert("refs/heads/main".into(), "main-sha-restore".into());
            state.refs.insert("HEAD".into(), "main-sha-restore".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-RESTORE-1"),
            parent_id: None,
            goal_id: None,
            title: "Task Restore".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        let path1 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await
            .unwrap();
        assert!(fs.is_dir(&path1).await);

        fs.save(
            &path1.join("task_file.txt"),
            &"task file content".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        cx.spawn({
            let fs = fs.clone();
            let path1 = path1.clone();
            let task_id = task.id.to_string();
            |mut cx| async move { commit_task_worktree(fs, path1, task_id, false, &mut cx).await }
        })
        .await
        .unwrap();

        cx.update(|cx| remove_task_worktree(project.clone(), &task.id, cx))
            .await
            .unwrap();
        assert!(!fs.is_dir(&path1).await);

        let path2 = cx
            .update(|cx| {
                ensure_task_worktree_with_options(
                    project.clone(),
                    &task,
                    None,
                    None,
                    None,
                    false,
                    cx,
                )
            })
            .await
            .unwrap();
        assert!(fs.is_dir(&path2).await);

        let content = fs.load(&path2.join("task_file.txt")).await.unwrap();
        assert_eq!(content, "task file content");
    }

    #[gpui::test]
    async fn test_regression_waiter_woken_on_non_terminal_session_end(cx: &mut TestAppContext) {
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
                .insert("refs/heads/main".into(), "main-sha-non-terminal".into());
            state
                .refs
                .insert("HEAD".into(), "main-sha-non-terminal".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        // Set pool limit = 2
        // Set limit = 2
        cx.update(|cx| {
            cx.update_global(|store: &mut settings::SettingsStore, cx| {
                store
                    .set_user_settings(r#"{ "agent": { "task_worktree_limit": 2 } }"#, cx)
                    .unwrap();
            });
        });

        let task1 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-NONTERM-1"),
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
            session_id: None,
            metrics: None,
        };
        let task2 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-NONTERM-2"),
            parent_id: None,
            goal_id: None,
            title: "Task 2".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        let task3 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-NONTERM-3"),
            parent_id: None,
            goal_id: None,
            title: "Task 3".to_string(),
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

        // Tasks 1 and 2 occupy the entire pool (limit = 2) with active sessions.
        let guard1 = SubagentSessionGuard::test_new(task1.id.clone());
        let path1 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task1, cx))
            .await
            .unwrap();
        assert!(fs.is_dir(&path1).await);

        let guard2 = SubagentSessionGuard::test_new(task2.id.clone());
        let path2 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task2, cx))
            .await
            .unwrap();
        assert!(fs.is_dir(&path2).await);

        // Task 3 ensure is spawned in background — must block waiting for slot.
        let mut ensure_task3 = cx.spawn({
            let project = project.clone();
            let task3 = task3.clone();
            |cx| async move {
                cx.update(|cx| ensure_task_worktree(project, &task3, cx))
                    .await
            }
        });

        cx.run_until_parked();
        assert!(
            (&mut ensure_task3).now_or_never().is_none(),
            "task 3 must block waiting for an available slot"
        );

        // Task 1 session ends NON-terminally (e.g. merge conflict, user canceled, error).
        // Guard drops without marking terminal or cleaning up worktree.
        drop(guard1);

        // Dropping guard1 decrements session count to 0 and notifies waiters.
        // Task 3 wakes up, re-runs eviction loop, sees Task 1 now has 0 active sessions,
        // and LRU-evicts Task 1 without requiring terminal transition or auto-cleanup.
        cx.run_until_parked();
        let path3 = (&mut ensure_task3)
            .now_or_never()
            .expect("task 3 must proceed after session count reached 0 on task 1")
            .unwrap();

        assert!(fs.is_dir(&path3).await, "task 3 worktree must now exist");
        assert!(
            !fs.is_dir(&path1).await,
            "task 1 worktree must have been evicted to make room"
        );
        assert!(
            fs.is_dir(&path2).await,
            "task 2 worktree must remain untouched since it has an active session"
        );

        drop(guard2);
    }

    #[gpui::test]
    async fn test_startup_sweep_lru_eviction_down_to_limit(cx: &mut TestAppContext) {
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
                .insert("refs/heads/main".into(), "main-sha-swp".into());
            state.refs.insert("HEAD".into(), "main-sha-swp".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        // Create 3 worktrees with higher limit
        cx.update(|cx| {
            cx.update_global(|store: &mut settings::SettingsStore, cx| {
                store
                    .set_user_settings(r#"{ "agent": { "task_worktree_limit": 5 } }"#, cx)
                    .unwrap();
            });
        });

        let task1 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-SWP-1"),
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
            session_id: None,
            metrics: None,
        };
        let task2 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-SWP-2"),
            parent_id: None,
            goal_id: None,
            title: "Task 2".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        let task3 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-SWP-3"),
            parent_id: None,
            goal_id: None,
            title: "Task 3".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        let g1 = SubagentSessionGuard::test_new(task1.id.clone());
        let path1 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task1, cx))
            .await
            .unwrap();
        let g2 = SubagentSessionGuard::test_new(task2.id.clone());
        let path2 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task2, cx))
            .await
            .unwrap();
        let g3 = SubagentSessionGuard::test_new(task3.id.clone());
        let path3 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task3, cx))
            .await
            .unwrap();

        // End session on task 1 first (oldest LRU), then task 2, then task 3
        drop(g1);
        drop(g2);
        drop(g3);

        // Reset limit to 2
        // Set limit = 2
        cx.update(|cx| {
            cx.update_global(|store: &mut settings::SettingsStore, cx| {
                store
                    .set_user_settings(r#"{ "agent": { "task_worktree_limit": 2 } }"#, cx)
                    .unwrap();
            });
        });

        // Run startup sweep with all 3 tasks (non-terminal).
        // Since count (3) > limit (2), startup sweep should LRU evict task 1 (oldest LRU) down to limit 2!
        let swept = cx
            .update(|cx| {
                startup_sweep(
                    project.clone(),
                    Some(&[task1.clone(), task2.clone(), task3.clone()]),
                    cx,
                )
            })
            .await
            .unwrap();

        assert_eq!(swept, vec![task1.id.clone()]);
        assert!(
            !fs.is_dir(&path1).await,
            "task 1 must be evicted down to limit"
        );
        assert!(fs.is_dir(&path2).await, "task 2 must be preserved");
        assert!(fs.is_dir(&path3).await, "task 3 must be preserved");
    }

    #[test]
    fn test_task_title_recording_and_sanitization() {
        reset_registry_for_tests();
        let task_1 = AgentTaskId::from("TASK-TITLES-UNIT-1");
        record_task_title(&task_1, "  Add login page\nsecond line");
        assert_eq!(
            task_worktree_title(&task_1),
            Some("Add login page".to_string())
        );

        let task_2 = AgentTaskId::from("TASK-TITLES-UNIT-2");
        record_task_title(&task_2, "\n\n   Fix memory leak   \nmore text");
        assert_eq!(
            task_worktree_title(&task_2),
            Some("Fix memory leak".to_string())
        );

        let task_empty = AgentTaskId::from("TASK-TITLES-UNIT-EMPTY");
        record_task_title(&task_empty, "");
        assert_eq!(task_worktree_title(&task_empty), None);

        let task_whitespace = AgentTaskId::from("TASK-TITLES-UNIT-WS");
        record_task_title(&task_whitespace, "   \n\t  \n  ");
        assert_eq!(task_worktree_title(&task_whitespace), None);

        let task_unknown = AgentTaskId::from("TASK-TITLES-UNIT-UNKNOWN");
        assert_eq!(task_worktree_title(&task_unknown), None);
    }

    #[gpui::test]
    async fn test_commit_task_worktree_uses_task_title(cx: &mut TestAppContext) {
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
                .insert("refs/heads/main".into(), "main-sha-title".into());
            state.refs.insert("HEAD".into(), "main-sha-title".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-TITLES-E2E-1"),
            parent_id: None,
            goal_id: None,
            title: "  Meaningful task title\nsecond line description".to_string(),
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

        let worktree_path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await
            .unwrap();

        fs.save(
            &worktree_path.join("task_file.txt"),
            &"worktree edit content".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details = cx
            .spawn({
                let fs = fs.clone();
                let worktree_path = worktree_path.clone();
                let task_id = task.id.to_string();
                |mut cx| async move {
                    commit_task_worktree(fs, worktree_path, task_id, false, &mut cx).await
                }
            })
            .await
            .unwrap();

        assert_eq!(details.commit_error, None);

        let snapshot = fs
            .with_git_state(&worktree_path.join(".git"), false, |state| {
                state.commit_history.last().cloned()
            })
            .unwrap()
            .expect("commit snapshot must exist in commit history");

        assert_eq!(snapshot.message, "Meaningful task title");

        // Error checkpoint path continues to use WIP message format
        fs.save(
            &worktree_path.join("error_file.txt"),
            &"error checkpoint content".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let err_details = cx
            .spawn({
                let fs = fs.clone();
                let worktree_path = worktree_path.clone();
                let task_id = task.id.to_string();
                |mut cx| async move {
                    commit_task_worktree(fs, worktree_path, task_id, true, &mut cx).await
                }
            })
            .await
            .unwrap();

        assert_eq!(err_details.commit_error, None);

        let err_snapshot = fs
            .with_git_state(&worktree_path.join(".git"), false, |state| {
                state.commit_history.last().cloned()
            })
            .unwrap()
            .expect("error commit snapshot must exist in commit history");

        let branch = format!("agent-task/{}", task.id);
        assert_eq!(
            err_snapshot.message,
            format!("WIP: {branch} error checkpoint")
        );
    }

    #[gpui::test]
    async fn test_commit_task_worktree_fallback_when_no_title_recorded(cx: &mut TestAppContext) {
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
                .insert("refs/heads/main".into(), "main-sha-fallback".into());
            state.refs.insert("HEAD".into(), "main-sha-fallback".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-TITLES-FALLBACK-1"),
            parent_id: None,
            goal_id: None,
            title: "Temporary Title".to_string(),
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

        let worktree_path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await
            .unwrap();

        // Clear recorded title to simulate missing title (e.g. post-restart without TGS summary)
        REGISTRY.write().task_titles.remove(&task.id);
        assert_eq!(task_worktree_title(&task.id), None);

        fs.save(
            &worktree_path.join("fallback_file.txt"),
            &"some edits".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let branch = format!("agent-task/{}", task.id);
        let expected_message = format!("{branch}: completed task work");

        let details = cx
            .spawn({
                let fs = fs.clone();
                let worktree_path = worktree_path.clone();
                let task_id = task.id.to_string();
                |mut cx| async move {
                    commit_task_worktree(fs, worktree_path, task_id, false, &mut cx).await
                }
            })
            .await
            .unwrap();

        assert_eq!(details.commit_error, None);

        let snapshot = fs
            .with_git_state(&worktree_path.join(".git"), false, |state| {
                state.commit_history.last().cloned()
            })
            .unwrap()
            .expect("commit snapshot must exist in commit history");

        assert_eq!(snapshot.message, expected_message);
    }

    #[gpui::test]
    async fn test_startup_sweep_seeds_task_titles(cx: &mut TestAppContext) {
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
                .insert("refs/heads/main".into(), "main-sha-swp-title".into());
            state
                .refs
                .insert("HEAD".into(), "main-sha-swp-title".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-TITLES-SWEEP-1"),
            parent_id: None,
            goal_id: None,
            title: "  Swept task title \n Details".to_string(),
            status: AgentTaskStatus::Completed,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        assert_eq!(task_worktree_title(&task.id), None);

        let swept = cx
            .update(|cx| startup_sweep(project.clone(), Some(std::slice::from_ref(&task)), cx))
            .await
            .expect("startup_sweep should succeed");
        assert!(swept.is_empty());

        assert_eq!(
            task_worktree_title(&task.id),
            Some("Swept task title".to_string())
        );
    }

    struct MockTgsProvider {
        task_title: Option<String>,
        should_fail: bool,
    }

    impl AgentTaskProvider for MockTgsProvider {
        fn server_id(&self) -> context_server::ContextServerId {
            context_server::ContextServerId("test_tgs".into())
        }

        fn fetch_graph(&self, _cx: &mut App) -> Task<Result<crate::agent_task::AgentTaskGraph>> {
            Task::ready(Err(anyhow::anyhow!("not implemented")))
        }

        fn get_task(
            &self,
            id: &AgentTaskId,
            _cx: &mut App,
        ) -> Task<Result<crate::agent_task::AgentTaskDetail>> {
            if self.should_fail {
                return Task::ready(Err(anyhow::anyhow!("TGS service offline")));
            }
            let title = self.task_title.clone().unwrap_or_default();
            Task::ready(Ok(crate::agent_task::AgentTaskDetail {
                summary: AgentTaskSummary {
                    id: id.clone(),
                    parent_id: None,
                    goal_id: None,
                    title,
                    status: AgentTaskStatus::Ready,
                    attempt: 1,
                    assignee: None,
                    write_scopes: vec![],
                    created_at: None,
                    assigned_profile: None,
                    model: None,
                    session_id: None,
                    metrics: None,
                },
                description: String::new(),
                acceptance_criteria: Vec::new(),
                events_tail: Vec::new(),
            }))
        }

        fn complete_task(&self, _id: &AgentTaskId, _cx: &mut App) -> Task<Result<()>> {
            Task::ready(Ok(()))
        }

        fn fail_task(&self, _id: &AgentTaskId, _reason: &str, _cx: &mut App) -> Task<Result<()>> {
            Task::ready(Ok(()))
        }

        fn archive_task(&self, _id: &AgentTaskId, _cx: &mut App) -> Task<Result<()>> {
            Task::ready(Ok(()))
        }

        fn unarchive_task(&self, _id: &AgentTaskId, _cx: &mut App) -> Task<Result<()>> {
            Task::ready(Ok(()))
        }

        fn delete_task(&self, _id: &AgentTaskId, _cx: &mut App) -> Task<Result<()>> {
            Task::ready(Ok(()))
        }

        fn list_artifacts(
            &self,
            _task_id: &AgentTaskId,
            _cx: &mut App,
        ) -> Task<Result<Vec<crate::agent_task::AgentTaskArtifact>>> {
            Task::ready(Ok(Vec::new()))
        }

        fn get_artifact(
            &self,
            _artifact_id: &str,
            _cx: &mut App,
        ) -> Task<Result<crate::agent_task::AgentTaskArtifact>> {
            Task::ready(Err(anyhow::anyhow!("not implemented")))
        }
    }

    #[gpui::test]
    async fn test_commit_task_worktree_fallback_fetches_tgs_title_when_in_memory_empty(
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
                .insert("refs/heads/main".into(), "main-sha-fallback-tgs".into());
            state.refs.insert("HEAD".into(), "main-sha-fallback-tgs".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let provider = Arc::new(MockTgsProvider {
            task_title: Some("Persisted TGS Title".to_string()),
            should_fail: false,
        });
        register_task_provider(provider);

        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-TITLES-FALLBACK-TGS"),
            parent_id: None,
            goal_id: None,
            title: "Temporary Title".to_string(),
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

        let worktree_path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await
            .unwrap();

        REGISTRY.write().task_titles.remove(&task.id);
        assert_eq!(task_worktree_title(&task.id), None);

        fs.save(
            &worktree_path.join("file.txt"),
            &"some edits".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details = cx
            .spawn({
                let fs = fs.clone();
                let worktree_path = worktree_path.clone();
                let task_id = task.id.to_string();
                |mut cx| async move {
                    commit_task_worktree(fs, worktree_path, task_id, false, &mut cx).await
                }
            })
            .await
            .unwrap();

        assert_eq!(details.commit_error, None);

        let snapshot = fs
            .with_git_state(&worktree_path.join(".git"), false, |state| {
                state.commit_history.last().cloned()
            })
            .unwrap()
            .expect("commit snapshot must exist in commit history");

        assert_eq!(snapshot.message, "Persisted TGS Title");
        assert_eq!(
            task_worktree_title(&task.id),
            Some("Persisted TGS Title".to_string())
        );
    }

    #[gpui::test]
    async fn test_commit_task_worktree_fallback_placeholder_when_tgs_has_no_title(
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
                .insert("refs/heads/main".into(), "main-sha-fallback-empty".into());
            state.refs.insert("HEAD".into(), "main-sha-fallback-empty".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let provider = Arc::new(MockTgsProvider {
            task_title: Some("   ".to_string()),
            should_fail: false,
        });
        register_task_provider(provider);

        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-TITLES-FALLBACK-NOTITLE"),
            parent_id: None,
            goal_id: None,
            title: "Temporary Title".to_string(),
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

        let worktree_path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await
            .unwrap();

        REGISTRY.write().task_titles.remove(&task.id);
        assert_eq!(task_worktree_title(&task.id), None);

        fs.save(
            &worktree_path.join("file.txt"),
            &"some edits".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let branch = format!("agent-task/{}", task.id);
        let expected_message = format!("{branch}: completed task work");

        let details = cx
            .spawn({
                let fs = fs.clone();
                let worktree_path = worktree_path.clone();
                let task_id = task.id.to_string();
                |mut cx| async move {
                    commit_task_worktree(fs, worktree_path, task_id, false, &mut cx).await
                }
            })
            .await
            .unwrap();

        assert_eq!(details.commit_error, None);

        let snapshot = fs
            .with_git_state(&worktree_path.join(".git"), false, |state| {
                state.commit_history.last().cloned()
            })
            .unwrap()
            .expect("commit snapshot must exist in commit history");

        assert_eq!(snapshot.message, expected_message);
    }

    #[gpui::test]
    async fn test_fresh_task_worktree_has_zero_local_modifications(cx: &mut TestAppContext) {
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
                .insert("refs/heads/main".into(), "main-sha-fresh".into());
            state.refs.insert("HEAD".into(), "main-sha-fresh".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-FRESH-1"),
            parent_id: None,
            goal_id: None,
            title: "Fresh task worktree".to_string(),
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

        let worktree_path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await
            .unwrap();

        assert!(!fs.is_file(&worktree_path.join(".zed/settings.json")).await);
        assert!(!fs.is_dir(&worktree_path.join(".zed")).await);

        let git_repo = fs
            .open_repo(&worktree_path.join(".git"), None)
            .unwrap();
        let prefix = RepoPath::from_rel_path(&util::rel_path::RelPath::empty());
        let status = git_repo
            .status(std::slice::from_ref(&prefix))
            .await
            .unwrap();
        assert!(
            status.entries.is_empty(),
            "expected zero local modifications right after creation, found: {:?}",
            status.entries
        );
        assert!(task_worktree_changed_files(&task.id).is_empty());
    }

    #[gpui::test]
    async fn test_commit_task_worktree_excludes_gitignored_file(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".git": {
                    "HEAD": "ref: refs/heads/main\n"
                },
                ".gitignore": "*.log\n",
            }),
        )
        .await;

        let dot_git = std::path::Path::new(path!("/root/.git"));
        fs.with_git_state(dot_git, true, |state| {
            state
                .refs
                .insert("refs/heads/main".into(), "main-sha-ignore".into());
            state.refs.insert("HEAD".into(), "main-sha-ignore".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-GITIGNORE-1"),
            parent_id: None,
            goal_id: None,
            title: "Task with ignored files".to_string(),
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

        let worktree_path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await
            .unwrap();

        fs.save(
            &worktree_path.join(".gitignore"),
            &"*.log\n".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        fs.save(
            &worktree_path.join("src/lib.rs"),
            &"pub fn run() {}".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();
        fs.save(
            &worktree_path.join("debug.log"),
            &"debug logs that should be ignored".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details = cx
            .spawn({
                let fs = fs.clone();
                let worktree_path = worktree_path.clone();
                let task_id = task.id.to_string();
                |mut cx| async move {
                    commit_task_worktree(fs, worktree_path, task_id, false, &mut cx).await
                }
            })
            .await
            .unwrap();

        assert_eq!(details.commit_error, None);
        let changed = task_worktree_changed_files(&task.id);
        assert!(
            !changed.contains(&"debug.log".to_string()),
            "gitignored file debug.log must not be committed, changed: {changed:?}"
        );
        assert!(
            changed.contains(&"src/lib.rs".to_string()),
            "tracked file must be committed"
        );
    }

    #[gpui::test]
    async fn test_main_invariant_goalless_task_main_untouched_after_commit_and_merge(
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
                "initial.txt": "initial content",
            }),
        )
        .await;

        let dot_git = std::path::Path::new(path!("/root/.git"));
        let initial_main_sha = "main-sha-invariant-init";
        fs.with_git_state(dot_git, true, |state| {
            state
                .refs
                .insert("refs/heads/main".into(), initial_main_sha.into());
            state.refs.insert("HEAD".into(), initial_main_sha.into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-GOALLESS-INVARIANT"),
            parent_id: None,
            goal_id: None,
            title: "Goalless task invariant".to_string(),
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

        let worktree_path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await
            .unwrap();

        fs.save(
            &worktree_path.join("task_only_file.txt"),
            &"task only content".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details = cx
            .update(|cx| commit_and_merge_task_worktree(project.clone(), &task.id, false, cx))
            .await
            .unwrap();

        assert_eq!(details.commit_error, None);
        assert_eq!(details.merged_into, None);
        assert_eq!(details.merge_conflict, None);

        let main_ref = fs
            .with_git_state(dot_git, false, |state| {
                state.refs.get("refs/heads/main").cloned()
            })
            .unwrap();
        assert_eq!(
            main_ref,
            Some(initial_main_sha.to_string()),
            "main ref must remain unchanged for goal-less task"
        );

        let task_branch_ref = fs
            .with_git_state(dot_git, false, |state| {
                state
                    .refs
                    .get(&format!("refs/heads/agent-task/{}", task.id))
                    .cloned()
            })
            .unwrap();
        assert!(task_branch_ref.is_some());
        assert_ne!(
            task_branch_ref,
            Some(initial_main_sha.to_string()),
            "changes must be committed on task branch"
        );

        assert!(!fs.is_file(std::path::Path::new(path!("/root/task_only_file.txt"))).await);
    }

    #[gpui::test]
    async fn test_main_invariant_goal_task_auto_merges_into_goal_branch_and_leaves_main_untouched(
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
                "initial.txt": "initial content",
            }),
        )
        .await;

        let dot_git = std::path::Path::new(path!("/root/.git"));
        let initial_main_sha = "main-sha-goal-invariant-init";
        fs.with_git_state(dot_git, true, |state| {
            state
                .refs
                .insert("refs/heads/main".into(), initial_main_sha.into());
            state.refs.insert("HEAD".into(), initial_main_sha.into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let goal_id = "GOAL-INVARIANT-TEST";
        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-GOAL-INVARIANT"),
            parent_id: None,
            goal_id: Some(goal_id.to_string()),
            title: "Goal task invariant".to_string(),
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

        let worktree_path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await
            .unwrap();

        fs.save(
            &worktree_path.join("goal_merged_file.txt"),
            &"goal merged content".into(),
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
            .expect("must be auto-merged into goal branch");
        assert_eq!(
            merged_into.target_ref,
            format!("refs/heads/agent-goal/{goal_id}")
        );
        assert_ne!(merged_into.sha, initial_main_sha);

        let main_ref = fs
            .with_git_state(dot_git, false, |state| {
                state.refs.get("refs/heads/main").cloned()
            })
            .unwrap();
        assert_eq!(
            main_ref,
            Some(initial_main_sha.to_string()),
            "main ref must remain unchanged for goal task"
        );

        let goal_branch_ref = fs
            .with_git_state(dot_git, false, |state| {
                state
                    .refs
                    .get(&format!("refs/heads/agent-goal/{goal_id}"))
                    .cloned()
            })
            .unwrap();
        assert_eq!(goal_branch_ref, Some(merged_into.sha));

        assert!(!fs.is_file(std::path::Path::new(path!("/root/goal_merged_file.txt"))).await);
    }

    #[test]
    fn test_prepare_goal_graduation_summary_multibyte_utf8_sha() {
        reset_registry_for_tests();
        record_goal_tip_sha("GOAL-UTF8", "🦀🦀🦀abc".to_string());
        let summary = prepare_goal_graduation_summary("GOAL-UTF8", "main")
            .expect("prepare_goal_graduation_summary should succeed for multibyte UTF-8 SHA");
        assert!(
            summary
                .pr_proposal
                .body
                .contains("- Tip SHA: `🦀🦀🦀abc` (🦀🦀🦀abc)")
        );
    }

    #[gpui::test]
    async fn test_prepare_goal_graduation_base_branch_resolution(cx: &mut TestAppContext) {
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
            state.refs.insert("refs/heads/main".into(), "main-sha-base".into());
            state.refs.insert("HEAD".into(), "main-sha-base".into());
            state.branches.insert("main".into());
            state.refs.insert("refs/heads/agent-goal/GOAL-BASE-1".into(), "goal-base-sha-1".into());
            state.branches.insert("agent-goal/GOAL-BASE-1".into());
            state.refs.insert("refs/heads/agent-goal/GOAL-BASE-2".into(), "goal-base-sha-2".into());
            state.branches.insert("agent-goal/GOAL-BASE-2".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        // Absent setting falls back to git default branch / "main"
        let summary_fallback = cx
            .update(|cx| prepare_goal_graduation(project.clone(), "GOAL-BASE-1", cx))
            .await
            .unwrap();
        assert_eq!(summary_fallback.base_branch, "main");
        assert_eq!(summary_fallback.refs.base_branch, "main");
        assert_eq!(summary_fallback.refs.base_ref, "refs/heads/main");
        assert_eq!(summary_fallback.pr_proposal.target_branch, "main");
        assert!(summary_fallback.pr_proposal.command.contains("--base main"));
        assert!(summary_fallback.pr_proposal.body.contains("graduation into `main`"));
        assert!(summary_fallback.pr_proposal.body.contains("- Target branch: `main`"));

        // Explicit setting wins
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.agent.get_or_insert_default().goal_graduation_base_branch =
                        Some("release/v2.0".to_string());
                });
            });
        });

        let summary_explicit = cx
            .update(|cx| prepare_goal_graduation(project.clone(), "GOAL-BASE-2", cx))
            .await
            .unwrap();
        assert_eq!(summary_explicit.base_branch, "release/v2.0");
        assert_eq!(summary_explicit.refs.base_branch, "release/v2.0");
        assert_eq!(summary_explicit.refs.base_ref, "refs/heads/release/v2.0");
        assert_eq!(summary_explicit.pr_proposal.target_branch, "release/v2.0");
        assert!(summary_explicit.pr_proposal.command.contains("--base release/v2.0"));
        assert!(summary_explicit.pr_proposal.body.contains("graduation into `release/v2.0`"));
        assert!(summary_explicit.pr_proposal.body.contains("- Target branch: `release/v2.0`"));
    }

    #[test]
    fn test_register_task_worktree_policy_clears_stale_overlay_and_reset() {
        reset_registry_for_tests();
        let task_id = AgentTaskId::from("TASK-OVERLAY-1");
        register_task_worktree_policy(
            &task_id,
            PathBuf::from("/worktree/first"),
            Some("agent-goal/GOAL-STALE".to_string()),
            Some("agent-goal/GOAL-STALE".to_string()),
            Some("stale-sha".to_string()),
            Some("agent-goal/GOAL-STALE".to_string()),
        );
        record_task_worktree_commit_error(&task_id, "stale error".to_string());
        record_task_worktree_merge_conflict(&task_id, vec!["conflict.rs".to_string()]);
        mark_task_terminal(&task_id);

        assert_eq!(
            task_worktree_goal_branch(&task_id),
            Some("agent-goal/GOAL-STALE".to_string())
        );

        register_task_worktree(&task_id, PathBuf::from("/worktree/second"));
        assert_eq!(task_worktree_goal_branch(&task_id), None);
        assert_eq!(task_worktree_base_ref(&task_id), None);
        assert_eq!(task_worktree_base_sha(&task_id), None);
        assert_eq!(task_worktree_checkout_target(&task_id), None);
        assert_eq!(task_worktree_commit_error(&task_id), None);
        assert_eq!(task_worktree_merge_conflict(&task_id), None);
        assert!(!is_task_terminal(&task_id));

        reset_registry_for_tests();
        assert_eq!(task_worktree_path_for_id(&task_id), None);
    }

    #[gpui::test]
    async fn test_release_stale_task_worktree_detaches_project_before_removal(
        cx: &mut TestAppContext,
    ) {
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
            id: AgentTaskId::from("TASK-ORDER-1"),
            parent_id: None,
            goal_id: None,
            title: "Order assertion task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec!["src/".to_string()],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        let path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await
            .expect("ensure worktree should succeed");

        let has_worktree_before = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .any(|worktree| worktree.read(cx).abs_path().as_ref() == path)
        });
        assert!(
            has_worktree_before,
            "worktree must be in project before release"
        );

        fs.set_remove_dir_error(&path, "permanent removal failure".to_string());

        let (repository, anchor_path, _, _, _) = project
            .update(cx, |project, cx| task_worktree_context(project, cx))
            .unwrap();

        let fs_trait: Arc<dyn Fs> = fs.clone();
        let result = release_stale_task_worktree(
            &project,
            &repository,
            &fs_trait,
            &anchor_path,
            &task.id,
            &path,
            "for order testing",
            false,
            &mut cx.to_async(),
        )
        .await;

        assert!(result.is_err(), "removal should fail due to injected error");

        let has_worktree_after = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .any(|worktree| worktree.read(cx).abs_path().as_ref() == path)
        });
        assert!(
            !has_worktree_after,
            "project worktree must be detached before git/fs removal"
        );
    }

    #[gpui::test]
    async fn test_remove_task_worktree_internal_detaches_project_before_removal(
        cx: &mut TestAppContext,
    ) {
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
            id: AgentTaskId::from("TASK-ORDER-2"),
            parent_id: None,
            goal_id: None,
            title: "Internal removal order assertion task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec!["src/".to_string()],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        let path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await
            .expect("ensure worktree should succeed");

        fs.set_remove_dir_error(&path, "fatal removal error".to_string());

        let remove_task =
            cx.update(|cx| remove_task_worktree_internal(project.clone(), &task.id, false, cx));
        let result = remove_task.await;
        assert!(result.is_err());

        let has_worktree = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .any(|worktree| worktree.read(cx).abs_path().as_ref() == path)
        });
        assert!(
            !has_worktree,
            "worktree must be detached from project before removal failure"
        );
    }

    #[test]
    fn test_is_sharing_violation_helper() {
        let os_sharing_error = anyhow::Error::new(std::io::Error::from_raw_os_error(32));
        assert!(is_sharing_violation(&os_sharing_error));

        let string_sharing_error =
            anyhow::anyhow!("The process cannot access the file: sharing violation");
        assert!(is_sharing_violation(&string_sharing_error));

        let uppercase_sharing_error = anyhow::anyhow!("ERROR: SHARING VIOLATION DETECTED");
        assert!(is_sharing_violation(&uppercase_sharing_error));

        let unrelated_io_error = anyhow::Error::new(std::io::Error::from_raw_os_error(5));
        assert!(!is_sharing_violation(&unrelated_io_error));

        let unrelated_string_error = anyhow::anyhow!("file not found");
        assert!(!is_sharing_violation(&unrelated_string_error));
    }

    #[test]
    fn test_removal_backoff_schedule() {
        assert_eq!(removal_backoff_duration(0), Duration::from_millis(50));
        assert_eq!(removal_backoff_duration(1), Duration::from_millis(100));
        assert_eq!(removal_backoff_duration(2), Duration::from_millis(200));
        assert_eq!(removal_backoff_duration(3), Duration::from_millis(400));
        assert_eq!(removal_backoff_duration(4), Duration::from_millis(800));
        assert_eq!(removal_backoff_duration(5), Duration::from_millis(800));
    }

    #[gpui::test]
    async fn test_removal_retry_fake_fs_sharing_violation_succeeds(cx: &mut TestAppContext) {
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
            id: AgentTaskId::from("TASK-RETRY-SUCCESS"),
            parent_id: None,
            goal_id: None,
            title: "Retry success task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec!["src/".to_string()],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        let path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await
            .expect("ensure worktree should succeed");

        fs.set_remove_dir_error(&path, "sharing violation: file locked".to_string());

        let (repository, anchor_path, _, _, _) = project
            .update(cx, |project, cx| task_worktree_context(project, cx))
            .unwrap();

        // Spawn background task to clear error after two failed attempts (50ms + 100ms backoff)
        let fs_clone = fs.clone();
        let path_clone = path.clone();
        let background_executor = cx.background_executor.clone();
        cx.background_executor
            .spawn(async move {
                background_executor.timer(Duration::from_millis(120)).await;
                fs_clone.clear_remove_dir_error(&path_clone);
            })
            .detach();

        let fs_trait: Arc<dyn Fs> = fs.clone();
        let mut remove_task = cx.spawn({
            let project = project.clone();
            let task_id = task.id.clone();
            let path = path.clone();
            |mut cx| async move {
                release_stale_task_worktree(
                    &project,
                    &repository,
                    &fs_trait,
                    &anchor_path,
                    &task_id,
                    &path,
                    "testing retry",
                    false,
                    &mut cx,
                )
                .await
            }
        });

        // Run until attempt 0 fails and waits 50ms
        cx.run_until_parked();
        // Advance 50ms: attempt 1 runs, fails, and waits 100ms
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
        // Advance 70ms (total 120ms): background task clears error
        cx.executor().advance_clock(Duration::from_millis(70));
        cx.run_until_parked();
        // Advance 30ms (total 150ms): attempt 2 runs with error cleared, succeeds
        cx.executor().advance_clock(Duration::from_millis(30));
        cx.run_until_parked();

        let result = (&mut remove_task)
            .now_or_never()
            .expect("removal should have completed after sharing violation cleared");
        assert!(result.is_ok());
        assert!(!fs.is_dir(&path).await);
    }

    #[gpui::test]
    async fn test_removal_retry_exhaustion_records_cleanup_error(cx: &mut TestAppContext) {
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
            id: AgentTaskId::from("TASK-RETRY-FAIL"),
            parent_id: None,
            goal_id: None,
            title: "Retry exhaustion task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec!["src/".to_string()],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        let path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await
            .expect("ensure worktree should succeed");

        fs.set_remove_dir_error(&path, "sharing violation: locked file".to_string());

        let mut cleanup_task = cx.spawn({
            let project = project.clone();
            let task_id = task.id.clone();
            |cx| async move {
                cx.update(|cx| auto_cleanup_task_worktree(project, &task_id, cx))
                    .await
            }
        });

        cx.run_until_parked();
        // Advance past all backoffs (50 + 100 + 200 + 400 = 750ms for git, then again for fs fallback)
        cx.executor().advance_clock(Duration::from_secs(5));
        cx.run_until_parked();

        let result = (&mut cleanup_task)
            .now_or_never()
            .expect("cleanup should complete after retries exhaust");
        assert!(result.is_err());

        let cleanup_error = task_worktree_cleanup_error(&task.id);
        assert!(
            cleanup_error.is_some(),
            "cleanup error must be recorded after final failure"
        );
        let error_message = cleanup_error.unwrap().to_lowercase();
        assert!(error_message.contains("sharing violation"));
    }

    #[gpui::test]
    async fn test_acquire_slot_timeout_lists_holders(cx: &mut TestAppContext) {
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

        cx.update(|cx| {
            cx.update_global(|store: &mut settings::SettingsStore, cx| {
                store
                    .set_user_settings(r#"{ "agent": { "task_worktree_limit": 3 } }"#, cx)
                    .unwrap();
            });
        });

        for index in 1..=3 {
            let task = AgentTaskSummary {
                id: AgentTaskId::from(format!("TASK-HOLDER-{index}")),
                parent_id: None,
                goal_id: None,
                title: format!("Holder task {index}"),
                status: AgentTaskStatus::Ready,
                attempt: 1,
                assignee: None,
                write_scopes: vec!["src/".to_string()],
                created_at: None,
                assigned_profile: None,
                model: None,
                session_id: None,
                metrics: None,
            };
            cx.update(|cx| ensure_task_worktree(project.clone(), &task, cx))
                .await
                .expect("ensure holder worktree should succeed");
            record_task_worktree_commit_error(&task.id, "uncommitted changes".to_string());
        }

        let task4 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-WAIT-4"),
            parent_id: None,
            goal_id: None,
            title: "Waiting task 4".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec!["src/".to_string()],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        let mut acquire_task = cx.spawn({
            let project = project.clone();
            |cx| async move {
                cx.update(|cx| ensure_task_worktree(project, &task4, cx))
                    .await
            }
        });

        cx.run_until_parked();
        assert!(
            (&mut acquire_task).now_or_never().is_none(),
            "slot acquisition must be waiting in queue"
        );

        cx.executor().advance_clock(Duration::from_secs(45));
        cx.run_until_parked();

        let result = (&mut acquire_task)
            .now_or_never()
            .expect("slot acquisition should fail with timeout");
        assert!(
            result.is_err(),
            "acquisition must return an error on timeout"
        );

        let error_message = result.unwrap_err().to_string();
        assert!(
            error_message.contains("pool limit: 3"),
            "error message must include pool limit, got: {error_message}"
        );
        assert!(
            error_message.contains("task_worktree_limit"),
            "error message must include task_worktree_limit setting name, got: {error_message}"
        );
        assert!(
            error_message.contains("TASK-HOLDER-1")
                && error_message.contains("TASK-HOLDER-2")
                && error_message.contains("TASK-HOLDER-3"),
            "error message must list holder task ids, got: {error_message}"
        );
        assert!(
            error_message.contains("commit error"),
            "error message must list hold reason 'commit error', got: {error_message}"
        );
    }

    #[gpui::test]
    async fn test_commit_task_worktree_excludes_settings_json_when_out_of_scope(
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
                .insert("refs/heads/main".into(), "main-sha-exclude".into());
            state.refs.insert("HEAD".into(), "main-sha-exclude".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-OUT-OF-SCOPE-1"),
            parent_id: None,
            goal_id: None,
            title: "Out of scope task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec!["src/".to_string()],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        let worktree_path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await
            .unwrap();

        let head_before = fs
            .with_git_state(&worktree_path.join(".git"), false, |state| {
                state.refs.get("HEAD").cloned()
            })
            .unwrap();

        assert!(!fs.is_file(&worktree_path.join(".zed/settings.json")).await);
        fs.save(
            &worktree_path.join(".zed/settings.json"),
            &r#"{"file_scan_exclusions":["**/target"]}"#.into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details = cx
            .spawn({
                let fs = fs.clone();
                let worktree_path = worktree_path.clone();
                let task_id = task.id.to_string();
                |mut cx| async move {
                    commit_task_worktree(fs, worktree_path, task_id, false, &mut cx).await
                }
            })
            .await
            .unwrap();

        assert_eq!(details.changed_files, 0);
        assert_eq!(details.commit_error, None);
        assert_eq!(details.head_sha, head_before);

        let head_after = fs
            .with_git_state(&worktree_path.join(".git"), false, |state| {
                state.refs.get("HEAD").cloned()
            })
            .unwrap();
        assert_eq!(head_after, head_before);
        assert!(task_worktree_changed_files(&task.id).is_empty());
    }

    #[gpui::test]
    async fn test_commit_task_worktree_commits_settings_json_when_in_scope(
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
                .insert("refs/heads/main".into(), "main-sha-in-scope".into());
            state.refs.insert("HEAD".into(), "main-sha-in-scope".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-IN-SCOPE-1"),
            parent_id: None,
            goal_id: None,
            title: "In scope task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec![".zed/".to_string()],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        let worktree_path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await
            .unwrap();

        let head_before = fs
            .with_git_state(&worktree_path.join(".git"), false, |state| {
                state.refs.get("HEAD").cloned()
            })
            .unwrap();

        assert!(!fs.is_file(&worktree_path.join(".zed/settings.json")).await);
        fs.save(
            &worktree_path.join(".zed/settings.json"),
            &r#"{"file_scan_exclusions":["**/target"]}"#.into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details = cx
            .spawn({
                let fs = fs.clone();
                let worktree_path = worktree_path.clone();
                let task_id = task.id.to_string();
                |mut cx| async move {
                    commit_task_worktree(fs, worktree_path, task_id, false, &mut cx).await
                }
            })
            .await
            .unwrap();

        assert_eq!(details.changed_files, 1);
        assert_eq!(details.commit_error, None);
        assert_ne!(details.head_sha, head_before);

        let head_after = fs
            .with_git_state(&worktree_path.join(".git"), false, |state| {
                state.refs.get("HEAD").cloned()
            })
            .unwrap();
        assert_ne!(head_after, head_before);
        assert_eq!(
            task_worktree_changed_files(&task.id),
            vec![".zed/settings.json".to_string()]
        );
    }

    #[gpui::test]
    async fn test_commit_task_worktree_excludes_settings_json_when_scopes_empty(
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
                .insert("refs/heads/main".into(), "main-sha-empty".into());
            state.refs.insert("HEAD".into(), "main-sha-empty".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-EMPTY-SCOPES-1"),
            parent_id: None,
            goal_id: None,
            title: "Empty scopes task".to_string(),
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

        let worktree_path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await
            .unwrap();

        let head_before = fs
            .with_git_state(&worktree_path.join(".git"), false, |state| {
                state.refs.get("HEAD").cloned()
            })
            .unwrap();

        assert!(!fs.is_file(&worktree_path.join(".zed/settings.json")).await);
        fs.save(
            &worktree_path.join(".zed/settings.json"),
            &r#"{"file_scan_exclusions":["**/target"]}"#.into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details = cx
            .spawn({
                let fs = fs.clone();
                let worktree_path = worktree_path.clone();
                let task_id = task.id.to_string();
                |mut cx| async move {
                    commit_task_worktree(fs, worktree_path, task_id, false, &mut cx).await
                }
            })
            .await
            .unwrap();

        assert_eq!(details.changed_files, 0);
        assert_eq!(details.commit_error, None);
        assert_eq!(details.head_sha, head_before);

        let head_after = fs
            .with_git_state(&worktree_path.join(".git"), false, |state| {
                state.refs.get("HEAD").cloned()
            })
            .unwrap();
        assert_eq!(head_after, head_before);
        assert!(task_worktree_changed_files(&task.id).is_empty());
    }

    #[gpui::test]
    async fn test_commit_task_worktree_excludes_settings_json_while_committing_in_scope_changes(
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
                .insert("refs/heads/main".into(), "main-sha-mixed".into());
            state.refs.insert("HEAD".into(), "main-sha-mixed".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-MIXED-1"),
            parent_id: None,
            goal_id: None,
            title: "Mixed changes task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec!["src/".to_string()],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        let worktree_path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await
            .unwrap();

        let head_before = fs
            .with_git_state(&worktree_path.join(".git"), false, |state| {
                state.refs.get("HEAD").cloned()
            })
            .unwrap();

        assert!(!fs.is_file(&worktree_path.join(".zed/settings.json")).await);
        fs.save(
            &worktree_path.join(".zed/settings.json"),
            &r#"{"file_scan_exclusions":["**/target"]}"#.into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        fs.save(
            &worktree_path.join("src/lib.rs"),
            &"pub fn hello() {}".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details = cx
            .spawn({
                let fs = fs.clone();
                let worktree_path = worktree_path.clone();
                let task_id = task.id.to_string();
                |mut cx| async move {
                    commit_task_worktree(fs, worktree_path, task_id, false, &mut cx).await
                }
            })
            .await
            .unwrap();

        assert_eq!(details.changed_files, 1);
        assert_eq!(details.commit_error, None);
        assert_ne!(details.head_sha, head_before);

        assert_eq!(
            task_worktree_changed_files(&task.id),
            vec!["src/lib.rs".to_string()]
        );

        let snapshot = fs
            .with_git_state(&worktree_path.join(".git"), false, |state| {
                state.commit_history.last().cloned()
            })
            .unwrap()
            .expect("commit snapshot must exist");

        let committed_paths: Vec<String> = snapshot
            .index_contents
            .keys()
            .map(|p| p.as_unix_str().to_string())
            .collect();
        assert!(committed_paths.contains(&"src/lib.rs".to_string()));
        assert!(!committed_paths.contains(&".zed/settings.json".to_string()));
    }

    #[test]
    fn test_task_worktree_write_scopes_preserved_across_reregistration() {
        reset_registry_for_tests();
        let task_id = AgentTaskId::from("TASK-SCOPES-PRESERVE-1");

        register_task_worktree_policy_with_scopes(
            &task_id,
            PathBuf::from("/worktree/path"),
            None,
            None,
            None,
            None,
            Some(vec!["src/".to_string(), "docs/".to_string()]),
        );

        assert_eq!(
            task_worktree_write_scopes(&task_id),
            vec!["src/".to_string(), "docs/".to_string()]
        );

        // Re-register without scopes preserves existing write_scopes
        register_task_worktree_policy(
            &task_id,
            PathBuf::from("/worktree/path"),
            None,
            None,
            None,
            None,
        );

        assert_eq!(
            task_worktree_write_scopes(&task_id),
            vec!["src/".to_string(), "docs/".to_string()]
        );

        // Re-register with empty scopes preserves existing non-empty write_scopes
        register_task_worktree_policy_with_scopes(
            &task_id,
            PathBuf::from("/worktree/path"),
            None,
            None,
            None,
            None,
            Some(vec![]),
        );

        assert_eq!(
            task_worktree_write_scopes(&task_id),
            vec!["src/".to_string(), "docs/".to_string()]
        );

        // Re-register with new non-empty scopes updates them
        register_task_worktree_policy_with_scopes(
            &task_id,
            PathBuf::from("/worktree/path"),
            None,
            None,
            None,
            None,
            Some(vec!["crates/".to_string()]),
        );

        assert_eq!(
            task_worktree_write_scopes(&task_id),
            vec!["crates/".to_string()]
        );
    }

    #[gpui::test]
    async fn test_auto_cleanup_gated_by_settings(cx: &mut TestAppContext) {
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

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-CLEANUP-GATE"),
            parent_id: None,
            goal_id: None,
            title: "Test".to_string(),
            status: AgentTaskStatus::Completed,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        let worktree_path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await
            .unwrap();
        assert!(fs.is_dir(&worktree_path).await);

        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.agent.get_or_insert_default().task_lifecycle_mode =
                        Some(agent_settings::TaskLifecycleMode::Agent);
                });
            });
        });

        let cleaned = cx
            .update(|cx| auto_cleanup_task_worktree(project.clone(), &task.id, cx))
            .await
            .unwrap();
        assert!(!cleaned);
        assert!(fs.is_dir(&worktree_path).await);

        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    let agent = settings.agent.get_or_insert_default();
                    agent.task_lifecycle_mode =
                        Some(agent_settings::TaskLifecycleMode::Runtime);
                    agent.cleanup = Some(false);
                });
            });
        });

        let cleaned = cx
            .update(|cx| auto_cleanup_task_worktree(project.clone(), &task.id, cx))
            .await
            .unwrap();
        assert!(!cleaned);
        assert!(fs.is_dir(&worktree_path).await);

        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.agent.get_or_insert_default().cleanup = Some(true);
                });
            });
        });

        let cleaned = cx
            .update(|cx| auto_cleanup_task_worktree(project.clone(), &task.id, cx))
            .await
            .unwrap();
        assert!(cleaned);
        assert!(!fs.is_dir(&worktree_path).await);
    }

    #[gpui::test]
    async fn test_publish_ref_artifact_gated_by_settings(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".git": {
                    "HEAD": "ref: refs/heads/main\n"
                },
                "initial.txt": "initial"
            }),
        )
        .await;

        let (tool_calls_tx, mut tool_calls_rx) = futures::channel::mpsc::unbounded();
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
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.context_servers.insert(
                        "tgs".into(),
                        project::project_settings::ContextServerSettings::Stdio {
                            enabled: true,
                            remote: false,
                            command: context_server::ContextServerCommand {
                                path: "somebinary".into(),
                                args: Vec::new(),
                                env: None,
                                timeout: None,
                                platforms: Default::default(),
                            },
                        }
                        .into(),
                    );
                    settings.agent.get_or_insert_default().publish_ref_artifact = Some(false);
                });
            });
        });

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let context_server_store = project.read_with(cx, |p, _| p.context_server_store());
        context_server_store.update(cx, |store, cx| {
            store.start_server(
                Arc::new(context_server::ContextServer::new(
                    context_server::ContextServerId("tgs".into()),
                    Arc::new(fake_transport),
                )),
                cx,
            );
        });
        cx.run_until_parked();

        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-NO-REF-PUB"),
            parent_id: None,
            goal_id: None,
            title: "Test".to_string(),
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

        let worktree_path = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task,
                    Some("GOAL-NO-REF-PUB".to_string()),
                    None,
                    None,
                    cx,
                )
            })
            .await
            .unwrap();

        fs.save(
            &worktree_path.join("file.txt"),
            &"change".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details = cx
            .update(|cx| commit_and_merge_task_worktree(project.clone(), &task.id, false, cx))
            .await
            .unwrap();
        assert!(details.merged_into.is_some());

        // With publish_ref_artifact = false, no TGS call should be received.
        assert!(tool_calls_rx.try_recv().is_err());

        // Now enable publish_ref_artifact and verify that publication occurs.
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.agent.get_or_insert_default().publish_ref_artifact = Some(true);
                });
            });
        });

        let task2 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-WITH-REF-PUB"),
            parent_id: None,
            goal_id: None,
            title: "Test 2".to_string(),
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

        let worktree_path2 = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task2,
                    Some("GOAL-NO-REF-PUB".to_string()),
                    None,
                    None,
                    cx,
                )
            })
            .await
            .unwrap();

        fs.save(
            &worktree_path2.join("file2.txt"),
            &"change 2".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details2 = cx
            .update(|cx| commit_and_merge_task_worktree(project.clone(), &task2.id, false, cx))
            .await
            .unwrap();
        assert!(details2.merged_into.is_some());

        let call = tool_calls_rx
            .try_recv()
            .expect("should receive artifact_publish call when enabled");
        assert_eq!(call.name, "artifact_publish");
    }

    #[gpui::test]
    async fn test_regression_waiters_do_not_reserve_slots_against_themselves(
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
                .insert("refs/heads/main".into(), "main-sha-waiters".into());
            state.refs.insert("HEAD".into(), "main-sha-waiters".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        cx.update(|cx| {
            cx.update_global(|store: &mut settings::SettingsStore, cx| {
                store
                    .set_user_settings(r#"{ "agent": { "task_worktree_limit": 2 } }"#, cx)
                    .unwrap();
            });
        });

        let task1 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-WAIT-REG-1"),
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
            session_id: None,
            metrics: None,
        };
        let task2 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-WAIT-REG-2"),
            parent_id: None,
            goal_id: None,
            title: "Task 2".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        let task3 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-WAIT-REG-3"),
            parent_id: None,
            goal_id: None,
            title: "Task 3".to_string(),
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
        let task4 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-WAIT-REG-4"),
            parent_id: None,
            goal_id: None,
            title: "Task 4".to_string(),
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

        let guard1 = SubagentSessionGuard::test_new(task1.id.clone());
        let path1 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task1, cx))
            .await
            .unwrap();
        assert!(fs.is_dir(&path1).await);

        let guard2 = SubagentSessionGuard::test_new(task2.id.clone());
        let path2 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task2, cx))
            .await
            .unwrap();
        assert!(fs.is_dir(&path2).await);

        let mut ensure_task3 = cx.spawn({
            let project = project.clone();
            let task3 = task3.clone();
            |cx| async move {
                cx.update(|cx| ensure_task_worktree(project, &task3, cx))
                    .await
            }
        });

        cx.run_until_parked();
        assert!((&mut ensure_task3).now_or_never().is_none());

        let mut ensure_task4 = cx.spawn({
            let project = project.clone();
            let task4 = task4.clone();
            |cx| async move {
                cx.update(|cx| ensure_task_worktree(project, &task4, cx))
                    .await
            }
        });

        cx.run_until_parked();
        assert!((&mut ensure_task4).now_or_never().is_none());

        drop(guard1);
        mark_task_terminal(&task1.id);
        let cleaned1 = cx
            .update(|cx| auto_cleanup_task_worktree(project.clone(), &task1.id, cx))
            .await
            .unwrap();
        assert!(cleaned1);
        assert!(!fs.is_dir(&path1).await);

        cx.run_until_parked();
        let path3 = (&mut ensure_task3)
            .now_or_never()
            .expect("task3 should succeed after slot 1 freed")
            .unwrap();
        assert!(fs.is_dir(&path3).await);

        drop(guard2);
        mark_task_terminal(&task2.id);
        let cleaned2 = cx
            .update(|cx| auto_cleanup_task_worktree(project.clone(), &task2.id, cx))
            .await
            .unwrap();
        assert!(cleaned2);
        assert!(!fs.is_dir(&path2).await);

        cx.run_until_parked();
        let path4 = (&mut ensure_task4)
            .now_or_never()
            .expect("task4 should succeed after slot 2 freed")
            .unwrap();
        assert!(fs.is_dir(&path4).await);
    }

    #[gpui::test]
    async fn test_slot_wait_timeout_unregisters_holder_and_session_guard(
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
                .insert("refs/heads/main".into(), "main-sha-timeout".into());
            state.refs.insert("HEAD".into(), "main-sha-timeout".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        cx.update(|cx| {
            cx.update_global(|store: &mut settings::SettingsStore, cx| {
                store
                    .set_user_settings(r#"{ "agent": { "task_worktree_limit": 1 } }"#, cx)
                    .unwrap();
            });
        });

        let holder_task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-OCCUPIER-1"),
            parent_id: None,
            goal_id: None,
            title: "Occupier Task 1".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec!["src/".to_string()],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        cx.update(|cx| ensure_task_worktree(project.clone(), &holder_task, cx))
            .await
            .expect("ensure holder worktree should succeed");
        record_task_worktree_commit_error(&holder_task.id, "real disk failure".to_string());

        for i in 1..=3 {
            let failed_task = AgentTaskSummary {
                id: AgentTaskId::from(format!("TASK-FAILED-SPAWN-{i}")),
                parent_id: None,
                goal_id: None,
                title: format!("Failed spawn task {i}"),
                status: AgentTaskStatus::Ready,
                attempt: 1,
                assignee: None,
                write_scopes: vec!["src/".to_string()],
                created_at: None,
                assigned_profile: None,
                model: None,
                session_id: None,
                metrics: None,
            };

            let _guard = SubagentSessionGuard::test_new(failed_task.id.clone());
            let mut spawn_task = cx.spawn({
                let project = project.clone();
                let failed_task = failed_task.clone();
                |cx| async move {
                    cx.update(|cx| ensure_task_worktree(project, &failed_task, cx))
                        .await
                }
            });

            cx.run_until_parked();
            assert!((&mut spawn_task).now_or_never().is_none());

            cx.executor().advance_clock(Duration::from_secs(45));
            cx.run_until_parked();

            let res = (&mut spawn_task)
                .now_or_never()
                .expect("spawn must finish with error on timeout");
            assert!(res.is_err());

            assert!(
                !has_active_subagent_session(&failed_task.id),
                "failed task session must be unregistered"
            );
            assert!(
                !REGISTRY.read().entries.contains_key(&failed_task.id),
                "failed task entry must be removed from registry"
            );
        }

        let next_waiter = AgentTaskSummary {
            id: AgentTaskId::from("TASK-NEXT-WAITER"),
            parent_id: None,
            goal_id: None,
            title: "Next Waiter Task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec!["src/".to_string()],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        let mut next_spawn = cx.spawn({
            let project = project.clone();
            |cx| async move {
                cx.update(|cx| ensure_task_worktree(project, &next_waiter, cx))
                    .await
            }
        });

        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(45));
        cx.run_until_parked();

        let err_msg = (&mut next_spawn)
            .now_or_never()
            .expect("must time out")
            .unwrap_err()
            .to_string();

        assert!(
            err_msg.contains("TASK-OCCUPIER-1"),
            "holders list must contain actual occupier, got: {err_msg}"
        );
        assert!(
            !err_msg.contains("TASK-FAILED-SPAWN-1")
                && !err_msg.contains("TASK-FAILED-SPAWN-2")
                && !err_msg.contains("TASK-FAILED-SPAWN-3"),
            "holders list must not contain timed-out failed spawns, got: {err_msg}"
        );
    }

    #[gpui::test]
    async fn test_empty_commit_error_does_not_hold_worktree_and_allows_eviction_or_cleanup(
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
                .insert("refs/heads/main".into(), "main-sha-empty-err".into());
            state.refs.insert("HEAD".into(), "main-sha-empty-err".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-EMPTY-ERR"),
            parent_id: None,
            goal_id: None,
            title: "Task with empty error".to_string(),
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

        let worktree_path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task, cx))
            .await
            .unwrap();

        record_task_worktree_commit_error(&task.id, "".to_string());
        assert_eq!(task_worktree_commit_error(&task.id), None);
        assert_eq!(worktree_release_safety(&task.id), WorktreeReleaseSafety::Safe);

        record_task_worktree_commit_error(&task.id, "   ".to_string());
        assert_eq!(task_worktree_commit_error(&task.id), None);
        assert_eq!(worktree_release_safety(&task.id), WorktreeReleaseSafety::Safe);

        record_task_worktree_commit_error(&task.id, "commit failed: ".to_string());
        assert_eq!(task_worktree_commit_error(&task.id), None);
        assert_eq!(worktree_release_safety(&task.id), WorktreeReleaseSafety::Safe);

        record_task_worktree_commit_error(&task.id, "staging failed: ".to_string());
        assert_eq!(task_worktree_commit_error(&task.id), None);
        assert_eq!(worktree_release_safety(&task.id), WorktreeReleaseSafety::Safe);

        mark_task_terminal(&task.id);
        let cleaned = cx
            .update(|cx| auto_cleanup_task_worktree(project.clone(), &task.id, cx))
            .await
            .unwrap();
        assert!(cleaned, "empty commit error must not block auto-cleanup");
        assert!(!fs.is_dir(&worktree_path).await);
    }

    #[gpui::test]
    async fn test_checkout_mode_commit_with_filtered_settings_returns_clean_details_no_commit_error(
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
                .insert("refs/heads/main".into(), "main-sha-checkout".into());
            state.refs.insert("HEAD".into(), "main-sha-checkout".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let task_setup = AgentTaskSummary {
            id: AgentTaskId::from("TASK-SETUP-1"),
            parent_id: None,
            goal_id: None,
            title: "Setup task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec!["src/".to_string()],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };

        let path_setup = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &task_setup,
                    Some("GOAL-10".to_string()),
                    None,
                    None,
                    cx,
                )
            })
            .await
            .unwrap();

        fs.save(
            &path_setup.join("src/lib.rs"),
            &"pub fn hello() {}".into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details_setup = cx
            .update(|cx| {
                commit_and_merge_task_worktree(project.clone(), &task_setup.id, false, cx)
            })
            .await
            .unwrap();
        assert!(details_setup.merged_into.is_some());
        mark_task_terminal(&task_setup.id);
        cx.update(|cx| auto_cleanup_task_worktree(project.clone(), &task_setup.id, cx))
            .await
            .unwrap();

        // TASK-178 scenario: checkout-mode reviewer session on agent-goal/GOAL-10, read-only
        let reviewer_task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-178"),
            parent_id: None,
            goal_id: None,
            title: "Reviewer TASK-178".to_string(),
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

        let path_reviewer = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &reviewer_task,
                    Some("GOAL-10".to_string()),
                    None,
                    Some("agent-goal/GOAL-10".to_string()),
                    cx,
                )
            })
            .await
            .unwrap();

        fs.save(
            &path_reviewer.join(".zed/settings.json"),
            &r#"{"file_scan_exclusions":["**/target"]}"#.into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();

        let details = cx
            .update(|cx| {
                commit_and_merge_task_worktree(project.clone(), &reviewer_task.id, false, cx)
            })
            .await
            .unwrap();

        assert_eq!(details.mode, "checkout");
        assert_eq!(details.branch, "agent-goal/GOAL-10");
        assert_eq!(details.changed_files, 0);
        assert_eq!(
            details.commit_error, None,
            "commit_error must be None for fully filtered changes"
        );
        assert_eq!(task_worktree_commit_error(&reviewer_task.id), None);
        assert_eq!(
            worktree_release_safety(&reviewer_task.id),
            WorktreeReleaseSafety::Safe
        );
    }

    #[gpui::test]
    async fn test_failing_spawn_frees_slot_for_next_task(cx: &mut TestAppContext) {
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
                .insert("refs/heads/main".into(), "main-sha-fail-spawn".into());
            state.refs.insert("HEAD".into(), "main-sha-fail-spawn".into());
            state.branches.insert("main".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        cx.update(|cx| {
            cx.update_global(|store: &mut settings::SettingsStore, cx| {
                store
                    .set_user_settings(r#"{ "agent": { "task_worktree_limit": 1 } }"#, cx)
                    .unwrap();
            });
        });

        let failing_task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-FAIL-EARLY"),
            parent_id: None,
            goal_id: None,
            title: "Failing Task".to_string(),
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

        let fail_res = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &failing_task,
                    None,
                    Some("main".to_string()),
                    Some("main".to_string()),
                    cx,
                )
            })
            .await;
        assert!(fail_res.is_err(), "mutually exclusive on_branch and base_branch must fail");
        assert!(!has_active_subagent_session(&failing_task.id));
        assert!(!REGISTRY.read().entries.contains_key(&failing_task.id));

        let missing_branch_task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-FAIL-MISSING-BRANCH"),
            parent_id: None,
            goal_id: None,
            title: "Missing branch task".to_string(),
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

        let fail_branch_res = cx
            .update(|cx| {
                ensure_task_worktree_with_policy(
                    project.clone(),
                    &missing_branch_task,
                    None,
                    None,
                    Some("nonexistent-branch".to_string()),
                    cx,
                )
            })
            .await;
        assert!(fail_branch_res.is_err(), "missing branch must fail");
        assert!(!has_active_subagent_session(&missing_branch_task.id));
        assert!(!REGISTRY.read().entries.contains_key(&missing_branch_task.id));

        let succeeding_task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-SUCCEED-AFTER-FAIL"),
            parent_id: None,
            goal_id: None,
            title: "Succeeding Task".to_string(),
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

        let succeed_path = cx
            .update(|cx| ensure_task_worktree(project.clone(), &succeeding_task, cx))
            .await
            .expect("slot must be free and available for next task");
        assert!(fs.is_dir(&succeed_path).await);
    }

    #[gpui::test]
    async fn test_pool_stale_guard_with_dead_owner_salvaged_and_evicted(cx: &mut TestAppContext) {
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
                .insert("refs/heads/main".into(), "main-sha-stale".into());
            state.refs.insert("HEAD".into(), "main-sha-stale".into());
            state.branches.insert("main".into());
        })
        .expect("git state setup");

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        cx.update(|cx| {
            cx.update_global(|store: &mut settings::SettingsStore, cx| {
                store
                    .set_user_settings(r#"{ "agent": { "task_worktree_limit": 1 } }"#, cx)
                    .expect("settings update");
            });
        });

        let task1 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-STALE-OWNER-1"),
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
            session_id: None,
            metrics: None,
        };
        let task2 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-STALE-OWNER-2"),
            parent_id: None,
            goal_id: None,
            title: "Task 2".to_string(),
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

        // Task 1 gets created with a stale guard (owner is gone/dead)
        let _guard1 = SubagentSessionGuard::test_new_stale(task1.id.clone());
        let path1 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task1, cx))
            .await
            .expect("ensure task 1");
        assert!(fs.is_dir(&path1).await);

        // Add dirty uncommitted file in task 1 worktree
        fs.save(
            &path1.join("salvage_file.txt"),
            &"important dirty changes".into(),
            text::LineEnding::Unix,
        )
        .await
        .expect("save dirty file");

        // Stale guard means no active session, and safety is Safe
        assert!(!has_active_subagent_session(&task1.id));
        assert_eq!(worktree_release_safety(&task1.id), WorktreeReleaseSafety::Safe);

        // Task 2 ensures worktree: pool limit is 1, Task 1 is evicted via salvage path
        let path2 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task2, cx))
            .await
            .expect("task 2 acquires slot by evicting stale task 1");

        assert!(!fs.is_dir(&path1).await, "stale task 1 worktree must be removed");
        assert!(fs.is_dir(&path2).await, "task 2 worktree must exist");

        // Verify task 1 uncommitted changes were salvaged to branch
        let branch_name = format!("agent-task/{}", task1.id);
        fs.with_git_state(dot_git, true, |state| {
            assert!(state.branches.contains(&branch_name), "salvaged branch must exist");
        })
        .expect("git state verification");
    }

    #[gpui::test]
    async fn test_pool_live_resumed_session_not_evicted(cx: &mut TestAppContext) {
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
                .insert("refs/heads/main".into(), "main-sha-resume".into());
            state.refs.insert("HEAD".into(), "main-sha-resume".into());
            state.branches.insert("main".into());
        })
        .expect("git state setup");

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        cx.update(|cx| {
            cx.update_global(|store: &mut settings::SettingsStore, cx| {
                store
                    .set_user_settings(r#"{ "agent": { "task_worktree_limit": 1 } }"#, cx)
                    .expect("settings update");
            });
        });

        let task1 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-RESUME-1"),
            parent_id: None,
            goal_id: None,
            title: "Task 1".to_string(),
            status: AgentTaskStatus::Completed,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        let task2 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-RESUME-2"),
            parent_id: None,
            goal_id: None,
            title: "Task 2".to_string(),
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

        // Create Task 1 worktree initially
        let path1 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task1, cx))
            .await
            .expect("ensure task 1");
        assert!(fs.is_dir(&path1).await);
        mark_task_terminal(&task1.id);

        // Resume session for completed Task 1 with live owner
        let resumed_guard = SubagentSessionGuard::test_new(task1.id.clone());
        assert!(has_active_subagent_session(&task1.id));
        assert_eq!(
            worktree_release_safety(&task1.id),
            WorktreeReleaseSafety::ActiveSession
        );

        // Task 2 attempts to ensure: pool limit is 1, Task 1 is live resumed session -> must NOT evict -> Task 2 blocks!
        let mut ensure_task2 = cx.spawn({
            let project = project.clone();
            let task2 = task2.clone();
            |cx| async move {
                cx.update(|cx| ensure_task_worktree(project, &task2, cx))
                    .await
            }
        });

        cx.run_until_parked();
        assert!(
            (&mut ensure_task2).now_or_never().is_none(),
            "task 2 must block because resumed task 1 has live owner"
        );
        assert!(fs.is_dir(&path1).await, "task 1 must remain");

        // End the resumed session
        drop(resumed_guard);
        assert!(!has_active_subagent_session(&task1.id));

        // Now task 2 can proceed
        cx.run_until_parked();
        let path2 = (&mut ensure_task2)
            .now_or_never()
            .expect("task 2 must unblock")
            .expect("task 2 worktree path");
        assert!(fs.is_dir(&path2).await);
    }

    #[test]
    fn test_timeout_path_leaves_zero_phantoms() {
        reset_registry_for_tests();
        let task_id = AgentTaskId::from("TASK-TIMEOUT-PHANTOM");

        let guard = SubagentSessionGuard::test_new(task_id.clone());
        assert_eq!(active_guards_count(&task_id), 1);
        assert!(has_active_subagent_session(&task_id));

        drop(guard);
        assert_eq!(active_guards_count(&task_id), 0);
        assert!(!has_active_subagent_session(&task_id));
    }

    #[test]
    fn test_turn_end_sweep_releases_guards() {
        reset_registry_for_tests();
        let task_a = AgentTaskId::from("TASK-SWEEP-A");
        let task_b = AgentTaskId::from("TASK-SWEEP-B");

        let thread_1 = acp::SessionId::new("thread-turn-1");
        let thread_2 = acp::SessionId::new("thread-turn-2");

        let tool_use_1 = LanguageModelToolUseId::from("tool-call-1");
        let tool_use_2 = LanguageModelToolUseId::from("tool-call-2");

        let owner_1a = SubagentSessionOwner {
            thread_id: thread_1.clone(),
            tool_use_id: tool_use_1.clone(),
            thread: None,
            is_test_live: true,
        };
        let owner_1b = SubagentSessionOwner {
            thread_id: thread_1.clone(),
            tool_use_id: tool_use_2,
            thread: None,
            is_test_live: true,
        };
        let owner_2 = SubagentSessionOwner {
            thread_id: thread_2,
            tool_use_id: LanguageModelToolUseId::from("tool-call-other"),
            thread: None,
            is_test_live: true,
        };

        let _guard_1a = register_subagent_session_guard(&task_a, owner_1a);
        let _guard_1b = register_subagent_session_guard(&task_a, owner_1b);
        let _guard_2 = register_subagent_session_guard(&task_b, owner_2);

        assert_eq!(active_guards_count(&task_a), 2);
        assert_eq!(active_guards_count(&task_b), 1);

        // Sweeping a single tool use removes only that tool's guard
        sweep_guards_for_tool_use(&thread_1, &tool_use_1);
        assert_eq!(active_guards_count(&task_a), 1);
        assert!(has_active_subagent_session(&task_a));

        // Sweeping thread_1 removes all remaining guards for thread_1
        sweep_guards_for_thread(&thread_1);
        assert_eq!(active_guards_count(&task_a), 0);
        assert!(!has_active_subagent_session(&task_a));
        assert_eq!(active_guards_count(&task_b), 1);
        assert!(has_active_subagent_session(&task_b));
    }

    #[gpui::test]
    async fn test_lru_eviction_skipped_when_cleanup_disabled(cx: &mut TestAppContext) {
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
                .insert("refs/heads/main".into(), "main-sha-nocleanup".into());
            state.refs.insert("HEAD".into(), "main-sha-nocleanup".into());
            state.branches.insert("main".into());
        })
        .expect("git state setup");

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        // Set limit = 1 and cleanup = false
        cx.update(|cx| {
            cx.update_global(|store: &mut settings::SettingsStore, cx| {
                store
                    .set_user_settings(
                        r#"{ "agent": { "task_worktree_limit": 1, "cleanup": false } }"#,
                        cx,
                    )
                    .expect("settings update");
            });
        });

        let task1 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-NOCLEANUP-1"),
            parent_id: None,
            goal_id: None,
            title: "Task 1".to_string(),
            status: AgentTaskStatus::Completed,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        };
        let task2 = AgentTaskSummary {
            id: AgentTaskId::from("TASK-NOCLEANUP-2"),
            parent_id: None,
            goal_id: None,
            title: "Task 2".to_string(),
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

        // Create Task 1 worktree and commit it so it is Safe for eviction
        let path1 = cx
            .update(|cx| ensure_task_worktree(project.clone(), &task1, cx))
            .await
            .expect("ensure task 1");
        assert!(fs.is_dir(&path1).await);
        let commit_res = commit_task_worktree(
            fs.clone(),
            path1.clone(),
            task1.id.to_string(),
            false,
            &mut cx.to_async(),
        )
        .await
        .expect("commit task 1");
        assert!(commit_res.head_sha.is_some());
        assert_eq!(worktree_release_safety(&task1.id), WorktreeReleaseSafety::Safe);

        // Task 2 attempts to acquire a slot: because cleanup is disabled, Task 1 must NOT be evicted!
        let mut ensure_task2 = cx.spawn({
            let project = project.clone();
            let task2 = task2.clone();
            |cx| async move {
                cx.update(|cx| ensure_task_worktree(project, &task2, cx))
                    .await
            }
        });

        cx.run_until_parked();
        assert!(
            (&mut ensure_task2).now_or_never().is_none(),
            "task 2 must block because cleanup is disabled and eviction is skipped"
        );
        assert!(fs.is_dir(&path1).await, "task 1 must remain intact");
    }
}
