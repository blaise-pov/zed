use std::path::PathBuf;
use std::sync::Arc;

use agent::{AgentTaskId, AgentTaskSummary};
use anyhow::{Context as _, Result};
use fs::Fs;
use git::repository::CreateWorktreeTarget;
use gpui::{App, Entity, Task};
use project::{
    Project, git_store::Repository, git_store::worktrees_directory_for_repo,
    project_settings::ProjectSettings,
};
use settings::Settings;
use util::paths::PathStyle;

/// Resolves the repository a task worktree should be linked to, plus the data
/// needed to compute its path. Deterministic: prefer the repository backing
/// the project's primary worktree, then any non-linked (main) repository,
/// then any repository at all.
fn task_worktree_context(
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
        .context("no git repository found in project")?
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

fn task_worktree_path(
    anchor_path: &PathBuf,
    worktree_setting: &str,
    path_style: PathStyle,
    task_id: &AgentTaskId,
) -> Result<PathBuf> {
    let worktrees_base = worktrees_directory_for_repo(anchor_path, worktree_setting, path_style)?;
    Ok(worktrees_base.join(format!("agent-task-{}", task_id)))
}

pub fn ensure_task_worktree(
    project: Entity<Project>,
    task: &AgentTaskSummary,
    cx: &mut App,
) -> Task<Result<PathBuf>> {
    let task_id = task.id.clone();
    cx.spawn(async move |cx| {
        let (repository, anchor_path, path_style, worktree_setting, file_system) = project
            .update(cx, |project, cx| {
                anyhow::Ok(task_worktree_context(project, cx))
            })??;
        let worktree_path =
            task_worktree_path(&anchor_path, &worktree_setting, path_style, &task_id)?;

        let path_exists = file_system.is_dir(&worktree_path).await;
        if !path_exists {
            let branch_name = format!("agent-task/{}", task_id);
            let create_task = repository.update(cx, |repository, _| {
                repository.create_worktree(
                    CreateWorktreeTarget::NewBranch {
                        branch_name: branch_name.clone(),
                        base_sha: None,
                    },
                    worktree_path.clone(),
                )
            });

            if let Err(error) = create_task.await? {
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
                    retry_task.await??;
                } else {
                    return Err(error);
                }
            }
        }

        let find_task = project.update(cx, |project, cx| {
            project.find_or_create_worktree(&worktree_path, false, cx)
        });
        find_task.await?;

        Ok(worktree_path)
    })
}

pub fn remove_task_worktree(
    project: Entity<Project>,
    task_id: &AgentTaskId,
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
                repository.remove_worktree(worktree_path, true)
            });
            remove_task.await??;
        }

        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::AgentTaskStatus;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;
    use settings::SettingsStore;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            ProjectSettings::register(cx);
            agent_settings::AgentSettings::register(cx);
        });
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
            id: AgentTaskId::from("TASK-1"),
            parent_id: None,
            title: "First task".to_string(),
            status: AgentTaskStatus::Ready,
            attempt: 1,
            assignee: None,
            write_scopes: vec!["src/".to_string()],
        };
        let second_task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-2"),
            parent_id: None,
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
        assert!(first_path.to_string_lossy().contains("agent-task-TASK-1"));
        assert!(second_path.to_string_lossy().contains("agent-task-TASK-2"));

        // Both directories exist in the project's filesystem and the main
        // worktree root is still present.
        assert!(fs.is_dir(&first_path).await);
        assert!(fs.is_dir(&second_path).await);
        assert!(fs.is_dir("/root".as_ref()).await);
    }
}
