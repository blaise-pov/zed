use super::tool_permissions::{
    SensitiveSettingsKind, authorize_symlink_escapes, canonicalize_worktree_roots,
    check_profile_write_scope, collect_symlink_escapes, is_path_in_profile_write_scope,
    resolve_creatable_global_skill_descendant_path, resolve_global_skill_descendant_path,
    resolves_to_global_skills_dir, sensitive_settings_kind,
};
use crate::{
    AgentTool, ToolCallEventStream, ToolInput, ToolPermissionDecision,
    authorize_with_sensitive_settings, decide_permission_for_paths_with_profile,
};
use agent_client_protocol::schema::v1 as acp;
use agent_settings::{AgentPermissionMode, AgentSettings};
use futures::FutureExt as _;
use gpui::{App, Entity, SharedString, Task};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::Settings;
use std::{path::Path, sync::Arc};
use util::markdown::MarkdownInlineCode;

/// Moves or rename a file or directory in the project, and returns confirmation that the move succeeded.
///
/// If the source and destination directories are the same, but the filename is different, this performs a rename. Otherwise, it performs a move.
///
/// This tool should be used when it's desirable to move or rename a file or directory without changing its contents at all.
/// The only supported paths outside the project are descendants of `~/.agents/skills`, for global agent skills.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct MovePathToolInput {
    /// The source path of the file or directory to move/rename.
    ///
    /// <example>
    /// If the project has the following files:
    ///
    /// - directory1/a/something.txt
    /// - directory2/a/things.txt
    /// - directory3/a/other.txt
    ///
    /// You can move the first file by providing a source_path of "directory1/a/something.txt"
    /// </example>
    pub source_path: String,

    /// The destination path where the file or directory should be moved/renamed to.
    /// If the paths are the same except for the filename, then this will be a rename.
    ///
    /// <example>
    /// To move "directory1/a/something.txt" to "directory2/b/renamed.txt",
    /// provide a destination_path of "directory2/b/renamed.txt"
    /// </example>
    pub destination_path: String,
}

pub struct MovePathTool {
    project: Entity<Project>,
}

impl MovePathTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for MovePathTool {
    type Input = MovePathToolInput;
    type Output = String;

    const NAME: &'static str = "move_path";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Move
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Ok(input) = input {
            let src = MarkdownInlineCode(&input.source_path);
            let dest = MarkdownInlineCode(&input.destination_path);
            let src_path = Path::new(&input.source_path);
            let dest_path = Path::new(&input.destination_path);

            match dest_path
                .file_name()
                .and_then(|os_str| os_str.to_os_string().into_string().ok())
            {
                Some(filename) if src_path.parent() == dest_path.parent() => {
                    let filename = MarkdownInlineCode(&filename);
                    format!("Rename {src} to {filename}").into()
                }
                _ => format!("Move {src} to {dest}").into(),
            }
        } else {
            "Move path".into()
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|e| e.to_string())?;
            let paths = vec![input.source_path.clone(), input.destination_path.clone()];
            let profile = cx.update(|cx| event_stream.profile_settings(cx));
            let decision = cx.update(|cx| {
                let location = project
                    .read(cx)
                    .visible_worktrees(cx)
                    .next()
                    .map(|w| settings::SettingsLocation {
                        worktree_id: w.read(cx).id(),
                        path: util::rel_path::RelPath::empty(),
                    });
                decide_permission_for_paths_with_profile(
                    Self::NAME,
                    &paths,
                    AgentSettings::get(location, cx),
                    profile.as_ref(),
                )
            });
            if let ToolPermissionDecision::Deny(reason) = decision {
                return Err(reason);
            }

            let fs = project.read_with(cx, |project, _cx| project.fs().clone());
            let canonical_roots = canonicalize_worktree_roots(&project, &fs, cx).await;

            cx.update(|cx| {
                check_profile_write_scope(
                    Self::NAME,
                    Path::new(&input.source_path),
                    &project,
                    &canonical_roots,
                    profile.as_ref(),
                    cx,
                )?;
                check_profile_write_scope(
                    Self::NAME,
                    Path::new(&input.destination_path),
                    &project,
                    &canonical_roots,
                    profile.as_ref(),
                    cx,
                )
            })
            .map_err(|e| e.to_string())?;

            if resolves_to_global_skills_dir(Path::new(&input.source_path), fs.as_ref()).await
                || resolves_to_global_skills_dir(
                    Path::new(&input.destination_path),
                    fs.as_ref(),
                )
                .await
            {
                return Err(
                    "Cannot move the global agent skills directory itself. Move a skill directory or file beneath it instead."
                        .to_string(),
                );
            }

            let global_source_path =
                resolve_global_skill_descendant_path(Path::new(&input.source_path), fs.as_ref())
                    .await;
            let global_destination_path = resolve_creatable_global_skill_descendant_path(
                Path::new(&input.destination_path),
                fs.as_ref(),
            )
            .await;

            let mode = profile
                .as_ref()
                .map_or(AgentPermissionMode::Interactive, |p| {
                    p.effective_permission_mode()
                });

            if let Some(profile) = &profile {
                if mode == AgentPermissionMode::Autonomous
                    && (global_source_path.is_some() || global_destination_path.is_some())
                {
                    return Err(format!(
                        "PolicyDenied: Operating on global skills path is outside project write scopes for profile '{}'",
                        profile.name
                    ));
                }
            }

            let symlink_escapes: Vec<(&str, std::path::PathBuf)> =
                project.read_with(cx, |project, cx| {
                    collect_symlink_escapes(
                        project,
                        &input.source_path,
                        &input.destination_path,
                        &canonical_roots,
                        cx,
                    )
                });

            let source_sensitive = sensitive_settings_kind(
                Path::new(&input.source_path),
                &canonical_roots,
                fs.as_ref(),
            )
            .await;
            let dest_sensitive = sensitive_settings_kind(
                Path::new(&input.destination_path),
                &canonical_roots,
                fs.as_ref(),
            )
            .await;
            let sensitive_kind = source_sensitive.or(dest_sensitive);

            let source_in_write_scope = if source_sensitive.is_some() {
                cx.update(|cx| {
                    is_path_in_profile_write_scope(
                        Self::NAME,
                        Path::new(&input.source_path),
                        &project,
                        &canonical_roots,
                        profile.as_ref(),
                        cx,
                    )
                })
            } else {
                true
            };

            let dest_in_write_scope = if dest_sensitive.is_some() {
                cx.update(|cx| {
                    is_path_in_profile_write_scope(
                        Self::NAME,
                        Path::new(&input.destination_path),
                        &project,
                        &canonical_roots,
                        profile.as_ref(),
                        cx,
                    )
                })
            } else {
                true
            };

            let in_write_scope = source_in_write_scope && dest_in_write_scope;

            if let Some(profile) = &profile {
                if mode == AgentPermissionMode::Autonomous {
                    if !symlink_escapes.is_empty() {
                        return Err(format!(
                            "PolicyDenied: Moving path '{}' escapes project boundaries via symlink (disallowed for autonomous profile '{}')",
                            input.source_path, profile.name
                        ));
                    }
                    if source_sensitive.is_some() && !source_in_write_scope {
                        if source_sensitive == Some(SensitiveSettingsKind::Hidden) {
                            return Err(format!(
                                "PolicyDenied: Editing hidden path '{}' is disallowed for autonomous profile '{}' without explicit write_scope",
                                input.source_path, profile.name
                            ));
                        }
                        return Err(format!(
                            "PolicyDenied: Accessing sensitive settings is disallowed for autonomous profile '{}' without explicit write_scope",
                            profile.name
                        ));
                    }
                    if dest_sensitive.is_some() && !dest_in_write_scope {
                        if dest_sensitive == Some(SensitiveSettingsKind::Hidden) {
                            return Err(format!(
                                "PolicyDenied: Editing hidden path '{}' is disallowed for autonomous profile '{}' without explicit write_scope",
                                input.destination_path, profile.name
                            ));
                        }
                        return Err(format!(
                            "PolicyDenied: Accessing sensitive settings is disallowed for autonomous profile '{}' without explicit write_scope",
                            profile.name
                        ));
                    }
                }
            }

            let sensitive_needs_confirmation = match sensitive_kind {
                Some(SensitiveSettingsKind::Hidden) => !in_write_scope,
                Some(_) => true,
                None => false,
            };

            let needs_confirmation = (mode == AgentPermissionMode::Interactive)
                && (matches!(decision, ToolPermissionDecision::Confirm)
                    || (matches!(decision, ToolPermissionDecision::Allow)
                        && sensitive_needs_confirmation));

            let authorize = if !symlink_escapes.is_empty() {
                // Symlink escape authorization replaces (rather than supplements)
                // the normal tool-permission prompt. The symlink prompt already
                // requires explicit user approval with the canonical target shown,
                // which is strictly more security-relevant than a generic confirm.
                Some(cx.update(|cx| {
                    authorize_symlink_escapes(Self::NAME, &symlink_escapes, &event_stream, cx)
                }))
            } else if needs_confirmation {
                Some(cx.update(|cx| {
                    let src = MarkdownInlineCode(&input.source_path);
                    let dest = MarkdownInlineCode(&input.destination_path);
                    let context = crate::ToolPermissionContext::new(
                        Self::NAME,
                        vec![input.source_path.clone(), input.destination_path.clone()],
                    );
                    let title = format!("Move {src} to {dest}");
                    authorize_with_sensitive_settings(
                        sensitive_kind,
                        context,
                        &title,
                        &event_stream,
                        cx,
                    )
                }))
            } else {
                None
            };

            if let Some(authorize) = authorize {
                authorize.await.map_err(|e| e.to_string())?;
            }

            if global_source_path.is_some() || global_destination_path.is_some() {
                let source_path = if let Some(global_source_path) = global_source_path {
                    global_source_path
                } else {
                    project.read_with(cx, |project, cx| {
                        let project_path = project.find_project_path(&input.source_path, cx).ok_or_else(|| {
                            format!("Source path {} was not found in the project.", input.source_path)
                        })?;
                        project.entry_for_path(&project_path, cx).ok_or_else(|| {
                            format!("Source path {} was not found in the project.", input.source_path)
                        })?;
                        project.absolute_path(&project_path, cx).ok_or_else(|| {
                            format!("Source path {} could not be resolved.", input.source_path)
                        })
                    })?
                };

                let destination_path = if let Some(global_destination_path) = global_destination_path
                {
                    global_destination_path
                } else {
                    project.read_with(cx, |project, cx| {
                        let project_path = project.find_project_path(&input.destination_path, cx).ok_or_else(|| {
                            format!(
                                "Destination path {} was outside the project.",
                                input.destination_path
                            )
                        })?;
                        project.absolute_path(&project_path, cx).ok_or_else(|| {
                            format!(
                                "Destination path {} could not be resolved.",
                                input.destination_path
                            )
                        })
                    })?
                };

                futures::select! {
                    result = fs.rename(
                        &source_path,
                        &destination_path,
                        fs::RenameOptions {
                            create_parents: true,
                            ..fs::RenameOptions::default()
                        },
                    ).fuse() => {
                        result.map_err(|e| format!("Moving {} to {}: {e}", input.source_path, input.destination_path))?;
                    }
                    _ = event_stream.cancelled_by_user().fuse() => {
                        return Err("Move cancelled by user".to_string());
                    }
                }

                return Ok(format!(
                    "Moved {} to {}",
                    input.source_path, input.destination_path
                ));
            }

            let rename_task = project.update(cx, |project, cx| {
                match project
                    .find_project_path(&input.source_path, cx)
                    .and_then(|project_path| project.entry_for_path(&project_path, cx))
                {
                    Some(entity) => match project.find_project_path(&input.destination_path, cx) {
                        Some(project_path) => Ok(project.rename_entry(entity.id, project_path, cx)),
                        None => Err(format!(
                            "Destination path {} was outside the project.",
                            input.destination_path
                        )),
                    },
                    None => Err(format!(
                        "Source path {} was not found in the project.",
                        input.source_path
                    )),
                }
            })?;

            futures::select! {
                result = rename_task.fuse() => result.map_err(|e| format!("Moving {} to {}: {e}", input.source_path, input.destination_path))?,
                _ = event_stream.cancelled_by_user().fuse() => {
                    return Err("Move cancelled by user".to_string());
                }
            };
            Ok(format!(
                "Moved {} to {}",
                input.source_path, input.destination_path
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::Fs as _;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::PathBuf;
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        cx.update(|cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.tool_permissions.default = settings::ToolPermissionMode::Allow;
            AgentSettings::override_global(settings, cx);
        });
    }

    #[gpui::test]
    async fn test_move_path_global_skill_directory_to_project(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root/project"), json!({})).await;
        let skill_dir = agent_skills::global_skills_dir().join("my-skill");
        fs.insert_tree(&skill_dir, json!({ "SKILL.md": "content" }))
            .await;
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(MovePathTool::new(project));
        let input_path = PathBuf::from("~")
            .join(".agents")
            .join("skills")
            .join("my-skill")
            .to_string_lossy()
            .into_owned();
        let destination_path = path!("/root/project/my-skill").to_string();

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(MovePathToolInput {
                    source_path: input_path,
                    destination_path,
                }),
                event_stream,
                cx,
            )
        });

        let auth = event_rx.expect_authorization().await;
        let title = auth.tool_call.fields.title.as_deref().unwrap_or("");
        assert!(
            title.contains("agent skills"),
            "Authorization title should mention agent skills, got: {title}",
        );
        assert!(
            auth.options
                .first_option_of_kind(acp::PermissionOptionKind::AllowAlways)
                .is_none(),
            "agent skills prompt must not offer an \"Always allow\" option: {:?}",
            auth.options,
        );
        auth.response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .expect("authorization response should send");

        let result = task.await;
        assert!(result.is_ok(), "should move after approval: {result:?}");
        assert!(!fs.is_dir(&skill_dir).await);
        assert_eq!(
            fs.load(path!("/root/project/my-skill/SKILL.md").as_ref())
                .await
                .unwrap(),
            "content"
        );
    }

    #[gpui::test]
    async fn test_move_path_project_directory_to_global_skill_directory(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root/project"),
            json!({ "exported-skill": { "SKILL.md": "content" } }),
        )
        .await;
        let skills_dir = agent_skills::global_skills_dir();
        fs.create_dir(&skills_dir).await.unwrap();
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(MovePathTool::new(project));
        let destination_path = PathBuf::from("~")
            .join(".agents")
            .join("skills")
            .join("exported-skill")
            .to_string_lossy()
            .into_owned();

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(MovePathToolInput {
                    source_path: path!("/root/project/exported-skill").to_string(),
                    destination_path,
                }),
                event_stream,
                cx,
            )
        });

        let auth = event_rx.expect_authorization().await;
        let title = auth.tool_call.fields.title.as_deref().unwrap_or("");
        assert!(
            title.contains("agent skills"),
            "Authorization title should mention agent skills, got: {title}",
        );
        assert!(
            auth.options
                .first_option_of_kind(acp::PermissionOptionKind::AllowAlways)
                .is_none(),
            "agent skills prompt must not offer an \"Always allow\" option: {:?}",
            auth.options,
        );
        auth.response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .expect("authorization response should send");

        let result = task.await;
        assert!(result.is_ok(), "should move after approval: {result:?}");
        assert!(
            !fs.is_dir(path!("/root/project/exported-skill").as_ref())
                .await
        );
        assert_eq!(
            fs.load(skills_dir.join("exported-skill").join("SKILL.md").as_ref())
                .await
                .unwrap(),
            "content"
        );
    }

    #[gpui::test]
    async fn test_move_path_symlink_escape_source_requests_authorization(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    "src": { "file.txt": "content" }
                },
                "external": {
                    "secret.txt": "SECRET"
                }
            }),
        )
        .await;

        fs.create_symlink(
            path!("/root/project/link_to_external").as_ref(),
            PathBuf::from("../external"),
        )
        .await
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(MovePathTool::new(project));

        let input = MovePathToolInput {
            source_path: "project/link_to_external".into(),
            destination_path: "project/external_moved".into(),
        };

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx));

        let auth = event_rx.expect_authorization().await;
        let title = auth.tool_call.fields.title.as_deref().unwrap_or("");
        assert!(
            title.contains("points outside the project")
                || title.contains("symlinks outside project"),
            "Authorization title should mention symlink escape, got: {title}",
        );

        auth.response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .unwrap();

        let result = task.await;
        assert!(result.is_ok(), "should succeed after approval: {result:?}");
    }

    #[gpui::test]
    async fn test_move_path_symlink_escape_denied(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    "src": { "file.txt": "content" }
                },
                "external": {
                    "secret.txt": "SECRET"
                }
            }),
        )
        .await;

        fs.create_symlink(
            path!("/root/project/link_to_external").as_ref(),
            PathBuf::from("../external"),
        )
        .await
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(MovePathTool::new(project));

        let input = MovePathToolInput {
            source_path: "project/link_to_external".into(),
            destination_path: "project/external_moved".into(),
        };

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx));

        let auth = event_rx.expect_authorization().await;
        drop(auth);

        let result = task.await;
        assert!(result.is_err(), "should fail when denied");
    }

    #[gpui::test]
    async fn test_move_path_symlink_escape_confirm_requires_single_approval(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        cx.update(|cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.tool_permissions.default = settings::ToolPermissionMode::Confirm;
            AgentSettings::override_global(settings, cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    "src": { "file.txt": "content" }
                },
                "external": {
                    "secret.txt": "SECRET"
                }
            }),
        )
        .await;

        fs.create_symlink(
            path!("/root/project/link_to_external").as_ref(),
            PathBuf::from("../external"),
        )
        .await
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(MovePathTool::new(project));

        let input = MovePathToolInput {
            source_path: "project/link_to_external".into(),
            destination_path: "project/external_moved".into(),
        };

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx));

        let auth = event_rx.expect_authorization().await;
        let title = auth.tool_call.fields.title.as_deref().unwrap_or("");
        assert!(
            title.contains("points outside the project")
                || title.contains("symlinks outside project"),
            "Authorization title should mention symlink escape, got: {title}",
        );

        auth.response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .unwrap();

        assert!(
            !matches!(
                event_rx.try_recv(),
                Ok(Ok(crate::ThreadEvent::ToolCallAuthorization(_)))
            ),
            "Expected a single authorization prompt",
        );

        let result = task.await;
        assert!(
            result.is_ok(),
            "Tool should succeed after one authorization: {result:?}"
        );
    }

    #[gpui::test]
    async fn test_move_path_symlink_escape_honors_deny_policy(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.tool_permissions.tools.insert(
                "move_path".into(),
                agent_settings::ToolRules {
                    default: Some(settings::ToolPermissionMode::Deny),
                    ..Default::default()
                },
            );
            AgentSettings::override_global(settings, cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    "src": { "file.txt": "content" }
                },
                "external": {
                    "secret.txt": "SECRET"
                }
            }),
        )
        .await;

        fs.create_symlink(
            path!("/root/project/link_to_external").as_ref(),
            PathBuf::from("../external"),
        )
        .await
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(MovePathTool::new(project));

        let input = MovePathToolInput {
            source_path: "project/link_to_external".into(),
            destination_path: "project/external_moved".into(),
        };

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx))
            .await;

        assert!(result.is_err(), "Tool should fail when policy denies");
        assert!(
            !matches!(
                event_rx.try_recv(),
                Ok(Ok(crate::ThreadEvent::ToolCallAuthorization(_)))
            ),
            "Deny policy should not emit symlink authorization prompt",
        );
    }

    #[gpui::test]
    async fn test_move_path_hidden_destination_autonomous_without_scope_fails(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    "src": { "file.txt": "content" },
                    ".github": {}
                }
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let profile_id = agent_settings::AgentProfileId("autonomous_default_allow".into());
        let mut tools = collections::HashMap::default();
        tools.insert(
            Arc::from("move_path"),
            agent_settings::ToolRules {
                default: Some(settings::ToolPermissionMode::Allow),
                always_allow: vec![],
                always_deny: vec![],
                always_confirm: vec![],
                write_scopes: None,
                invalid_patterns: vec![],
            },
        );
        let profile = agent_settings::AgentProfileSettings {
            name: "autonomous_default_allow".into(),
            origin: Default::default(),
            tools: collections::IndexMap::default(),
            enable_all_context_servers: false,
            context_servers: collections::IndexMap::default(),
            default_model: None,
            custom_prompt_path: None,
            system_prompt_template: None,
            description: None,
            skills: None,
            delegation: None,
            tool_permissions: Some(agent_settings::ToolPermissions {
                default: settings::ToolPermissionMode::Allow,
                tools,
            }),
            permission_mode: Some(AgentPermissionMode::Autonomous),
        };

        cx.update(|cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.profiles.insert(profile_id.clone(), profile);
            AgentSettings::override_global(settings, cx);
        });

        let tool = Arc::new(MovePathTool::new(project));
        let input = MovePathToolInput {
            source_path: "project/src/file.txt".into(),
            destination_path: "project/.github/file.txt".into(),
        };

        let (event_stream, _rx) = ToolCallEventStream::test_with_profile(profile_id);
        let result = cx
            .update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx))
            .await;

        assert!(
            result.is_err(),
            "Tool should fail when moving to hidden dest without scope"
        );
        let err_str = result.unwrap_err();
        assert!(
            err_str.contains("PolicyDenied: Editing hidden path 'project/.github/file.txt' is disallowed for autonomous profile 'autonomous_default_allow' without explicit write_scope"),
            "expected PolicyDenied message for hidden destination, got: {err_str}"
        );
    }

    #[gpui::test]
    async fn test_move_path_interactive_confirm_prompts_for_ordinary_path(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    "src": { "foo.txt": "hello" }
                }
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let profile_id = agent_settings::AgentProfileId("interactive_confirm".into());
        let mut tools = collections::HashMap::default();
        tools.insert(
            Arc::from("move_path"),
            agent_settings::ToolRules {
                default: Some(settings::ToolPermissionMode::Confirm),
                always_allow: vec![],
                always_deny: vec![],
                always_confirm: vec![],
                write_scopes: None,
                invalid_patterns: vec![],
            },
        );
        let profile = agent_settings::AgentProfileSettings {
            name: "interactive_confirm".into(),
            origin: Default::default(),
            tools: collections::IndexMap::default(),
            enable_all_context_servers: false,
            context_servers: collections::IndexMap::default(),
            default_model: None,
            custom_prompt_path: None,
            system_prompt_template: None,
            description: None,
            skills: None,
            delegation: None,
            tool_permissions: Some(agent_settings::ToolPermissions {
                default: settings::ToolPermissionMode::Confirm,
                tools,
            }),
            permission_mode: Some(AgentPermissionMode::Interactive),
        };

        cx.update(|cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.profiles.insert(profile_id.clone(), profile);
            AgentSettings::override_global(settings, cx);
        });

        let tool = Arc::new(MovePathTool::new(project));
        let input = MovePathToolInput {
            source_path: "project/src/foo.txt".into(),
            destination_path: "project/src/bar.txt".into(),
        };

        let (event_stream, mut event_rx) = ToolCallEventStream::test_with_profile(profile_id);
        let task = cx.update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx));

        let auth = event_rx.expect_authorization().await;
        auth.response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .unwrap();

        let result = task.await;
        assert!(
            result.is_ok(),
            "Tool should succeed after user confirms: {:?}",
            result
        );
    }
}
