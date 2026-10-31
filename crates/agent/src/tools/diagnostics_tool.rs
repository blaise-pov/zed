use crate::tools::tool_permissions::{
    ResolvedProjectPath, canonicalize_worktree_roots, is_inside_task_worktree, resolve_tool_path,
};
use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v2 as acp;
use futures::{Future, FutureExt as _};
use gpui::{App, AsyncApp, Entity, Task};
use language::{DiagnosticSeverity, OffsetRangeExt};
use project::{Project, ProjectPath};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{fmt::Write, sync::Arc};
use ui::SharedString;
use util::markdown::MarkdownInlineCode;

type Result<T, E = String> = core::result::Result<T, E>;

/// Get errors and warnings for the project or a specific file.
///
/// This tool can be invoked after a series of edits to determine if further edits are necessary, or if the user asks to fix errors or warnings in their codebase.
///
/// When a path is provided, shows all diagnostics for that specific file.
/// When no path is provided, shows a summary of error and warning counts for all files in the project.
///
/// This tool attempts to refresh diagnostics before returning.
/// If refreshing diagnostics fails (for example, if the language server does not support pull-based diagnostics), it will return any diagnostics already present.
/// Note that, in this case, the results may be out-of-date, and may or may not reflect the most recent edits.
/// If this happens, do not attempt to re-run this tool in the hope that refreshing will later succeed. Failures are typically persistent.
///
/// <example>
/// To get diagnostics for a specific file:
/// {
///     "path": "src/main.rs"
/// }
///
/// To get a project-wide diagnostic summary:
/// {}
/// </example>
///
/// <guidelines>
/// - If you think you can fix a diagnostic, make 1-2 attempts and then give up.
/// - Don't remove code you've generated just because you can't fix an error. The user can help you fix it.
/// </guidelines>
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DiagnosticsToolInput {
    /// The path to get diagnostics for. If not provided, returns a project-wide summary.
    ///
    /// This path should never be absolute, and the first component
    /// of the path should always be a root directory in a project.
    ///
    /// <example>
    /// If the project has the following root directories:
    ///
    /// - lorem
    /// - ipsum
    ///
    /// If you wanna access diagnostics for `dolor.txt` in `ipsum`, you should use the path `ipsum/dolor.txt`.
    /// </example>
    pub path: Option<String>,
}

pub struct DiagnosticsTool {
    project: Entity<Project>,
}

impl DiagnosticsTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

async fn with_cancellation<T>(f: impl Future<Output = T>, s: &ToolCallEventStream) -> Result<T> {
    futures::select! {
        result = f.fuse() => Ok(result),
        _ = s.cancelled_by_user().fuse() => {
            Err("Diagnostics cancelled by user".to_string())
        }
    }
}

fn freshness_message(refreshed: bool) -> &'static str {
    if refreshed {
        "Diagnostics successfully refreshed."
    } else {
        "Failed to refresh diagnostics. Diagnostics may be stale."
    }
}

/// Attempt to pull fresh diagnostics from the LSP before reading them.
///
/// Returns `Ok(true)` if diagnostics were successfully refreshed,
/// `Ok(false)` if the pull failed (callers should fall through to
/// read cached diagnostics), or `Err` if cancelled by the user.
async fn pull_diagnostics(
    project: &Entity<Project>,
    project_path: Option<ProjectPath>,
    event_stream: &ToolCallEventStream,
    cx: &mut AsyncApp,
) -> Result<bool, String> {
    match project_path {
        Some(project_path) => {
            let open_buffer_task = project.update(cx, |project, cx| {
                project.open_buffer(project_path, cx)
            });

            let buffer = with_cancellation(open_buffer_task, event_stream)
                .await?
                .map_err(|e| e.to_string())?;

            let lsp_store = project.read_with(cx, |project, _cx| project.lsp_store());
            let pull_task = lsp_store.update(cx, |lsp_store, cx| {
                lsp_store.pull_diagnostics_for_buffer(buffer, cx)
            });
            let pull_result = with_cancellation(pull_task, event_stream).await?;
            if let Err(error) = &pull_result {
                log::warn!("Failed to pull diagnostics, using cached: {error:#}");
            }
            Ok(pull_result.is_ok())
        }
        None => {
            let lsp_store = project.read_with(cx, |project, _cx| project.lsp_store());
            let pull_task = lsp_store.update(cx, |lsp_store, cx| {
                lsp_store.pull_workspace_diagnostics_once(cx)
            });
            let succeeded = with_cancellation(pull_task, event_stream).await?;
            if !succeeded {
                log::warn!("Failed to pull workspace diagnostics, using cached");
            }
            Ok(succeeded)
        }
    }
}

impl AgentTool for DiagnosticsTool {
    type Input = DiagnosticsToolInput;
    type Output = String;

    const NAME: &'static str = "diagnostics";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Some(path) = input.ok().and_then(|input| match input.path {
            Some(path) if !path.is_empty() => Some(path),
            _ => None,
        }) {
            format!("Check diagnostics for {}", MarkdownInlineCode(&path)).into()
        } else {
            "Check project diagnostics".into()
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
            let input = input.recv().await.map_err(|e| e.to_string())?;
            let task_worktree = cx.update(|cx| event_stream.task_worktree(cx));

            match input.path {
                Some(ref path) if !path.is_empty() => {
                    let fs = project.read_with(cx, |project, _cx| project.fs().clone());
                    let canonical_roots = canonicalize_worktree_roots(&project, &fs, cx).await;
                    let project_path = project
                        .read_with(cx, |project, cx| {
                            let resolved = resolve_tool_path(
                                project,
                                task_worktree.as_deref(),
                                path,
                                &canonical_roots,
                                cx,
                            )?;
                            Ok(match resolved {
                                ResolvedProjectPath::Safe(p) => p,
                                ResolvedProjectPath::SymlinkEscape {
                                    project_path, ..
                                } => project_path,
                            })
                        })
                        .map_err(|e: anyhow::Error| e.to_string())?;

                    let refreshed =
                        pull_diagnostics(&project, Some(project_path.clone()), &event_stream, cx)
                            .await?;

                    let open_buffer_task = project.update(cx, |project, cx| {
                        project.open_buffer(project_path, cx)
                    });

                    let buffer = with_cancellation(open_buffer_task, &event_stream)
                        .await?
                        .map_err(|e| e.to_string())?;

                    let mut output = String::new();
                    let snapshot = buffer.read_with(cx, |buffer, _cx| buffer.snapshot());

                    for (_, group) in snapshot.diagnostic_groups(None) {
                        let entry = &group.entries[group.primary_ix];
                        let range = entry.range.to_point(&snapshot);
                        let severity = match entry.diagnostic.severity {
                            DiagnosticSeverity::ERROR => "error",
                            DiagnosticSeverity::WARNING => "warning",
                            _ => continue,
                        };

                        writeln!(
                            output,
                            "{} at line {}: {}",
                            severity,
                            range.start.row + 1,
                            entry.diagnostic.message
                        )
                        .ok();
                    }

                    let freshness = freshness_message(refreshed);
                    if output.is_empty() {
                        Ok(format!(
                            "{freshness}\n\nFile doesn't have errors or warnings!"
                        ))
                    } else {
                        Ok(format!("{freshness}\n\n{output}"))
                    }
                }
                _ => {
                    let refreshed = pull_diagnostics(&project, None, &event_stream, cx).await?;

                    let (output, has_diagnostics) = project.read_with(cx, |project, cx| {
                        let mut output = String::new();
                        let mut has_diagnostics = false;

                        for (project_path, _, summary) in project.diagnostic_summaries(true, cx) {
                            if summary.error_count > 0 || summary.warning_count > 0 {
                                let Some(worktree) =
                                    project.worktree_for_id(project_path.worktree_id, cx)
                                else {
                                    continue;
                                };

                                if let Some(ref task_worktree) = task_worktree {
                                    if !is_inside_task_worktree(
                                        worktree.read(cx).abs_path().as_ref(),
                                        task_worktree,
                                    ) {
                                        continue;
                                    }
                                }

                                has_diagnostics = true;
                                output.push_str(&format!(
                                    "{}: {} error(s), {} warning(s)\n",
                                    worktree.read(cx).absolutize(&project_path.path).display(),
                                    summary.error_count,
                                    summary.warning_count
                                ));
                            }
                        }

                        (output, has_diagnostics)
                    });

                    let freshness = freshness_message(refreshed);
                    if has_diagnostics {
                        Ok(format!("{freshness}\n\n{output}"))
                    } else {
                        Ok(format!(
                            "{freshness}\n\nNo errors or warnings found in the project."
                        ))
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use util::{path, rel_path::rel_path};

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    #[gpui::test]
    async fn test_diagnostics_isolated_task_worktree(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let host_root = path!("/root");
        let task_worktree_path = path!("/worktrees/agent-task-1");

        fs.insert_tree(
            host_root,
            json!({
                "host_file.rs": "fn host() {}",
                "shared.rs": "fn shared() {}"
            }),
        )
        .await;

        fs.insert_tree(
            task_worktree_path,
            json!({
                "task_file.rs": "fn task() {}",
                "shared.rs": "fn shared() {}"
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [host_root.as_ref()], cx).await;
        let (tree, _) = project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(Path::new(task_worktree_path), false, cx)
            })
        .await
        .unwrap();
        tree.read_with(cx, |tree, _| tree.as_local().unwrap().scan_complete())
        .await;
        cx.run_until_parked();

        let tool = Arc::new(DiagnosticsTool::new(project.clone()));

        let thread = cx.update(|cx| {
            let project_context = cx.new(|_cx| crate::ProjectContext::default());
            let context_server_store = project.read(cx).context_server_store();
            let context_server_registry =
                cx.new(|cx| crate::tools::ContextServerRegistry::new(context_server_store, cx));
            cx.new(|cx| {
                let mut t = crate::Thread::new(
                    project.clone(),
                    project_context,
                    context_server_registry,
                    crate::Templates::new(),
                    None,
                    cx,
                );
                t.set_task_worktree(Some(PathBuf::from(task_worktree_path)));
                t
            })
        });

        let (isolated_stream, _) = ToolCallEventStream::test_with_thread(&thread);
        let (non_isolated_stream, _) = ToolCallEventStream::test();

        let server_id = lsp::LanguageServerId(0);

        // Populate diagnostics in host shared.rs
        let host_buffer = project
            .update(cx, |project, cx| {
                let project_path = project
                    .find_project_path(Path::new(host_root).join("shared.rs"), cx)
                    .unwrap();
                project.open_buffer(project_path, cx)
            })
        .await
        .unwrap();
        host_buffer.update(cx, |buffer, cx| {
            buffer.update_diagnostics(
                server_id,
                language::DiagnosticSet::from_sorted_entries(
                    vec![language::DiagnosticEntry::new(
                        language::Anchor::min_max_range_for_buffer(buffer.remote_id()),
                        language::Diagnostic {
                            severity: DiagnosticSeverity::ERROR,
                            message: "host shared error".to_string().into(), is_primary: true,
                            ..Default::default()
                        },
                    )],
                    &buffer.snapshot(),
                ),
                cx,
            );
        });

        // Populate diagnostics in task worktree shared.rs
        let task_buffer = project
            .update(cx, |project, cx| {
                project.open_buffer(
                    project::ProjectPath {
                        worktree_id: tree.read(cx).id(),
                        path: rel_path("shared.rs").into(),
                    },
                    cx,
                )
            })
        .await
        .unwrap();
        task_buffer.update(cx, |buffer, cx| {
            buffer.update_diagnostics(
                server_id,
                language::DiagnosticSet::from_sorted_entries(
                    vec![language::DiagnosticEntry::new(
                        language::Anchor::min_max_range_for_buffer(buffer.remote_id()),
                        language::Diagnostic {
                            severity: DiagnosticSeverity::ERROR,
                            message: "task worktree shared error".to_string().into(), is_primary: true,
                            ..Default::default()
                        },
                    )],
                    &buffer.snapshot(),
                ),
                cx,
            );
        });
        cx.run_until_parked();

        // 1. Relative path in isolated thread reads task worktree diagnostics, never host main
        let result = cx
            .update(|cx| {
                let input = DiagnosticsToolInput {
                    path: Some("shared.rs".to_string()),
                };
                tool.clone().run(ToolInput::resolved(input), isolated_stream.clone(), cx)
            })
        .await
        .unwrap();
        assert!(
            result.contains("task worktree shared error"),
            "expected task worktree diagnostic, got: {result}"
        );
        assert!(
            !result.contains("host shared error"),
            "must never read host main diagnostic: {result}"
        );

        // 2. Relative path prefixed with worktree name also reads task worktree diagnostics
        let result = cx
            .update(|cx| {
                let input = DiagnosticsToolInput {
                    path: Some("agent-task-1/shared.rs".to_string()),
                };
                tool.clone().run(ToolInput::resolved(input), isolated_stream.clone(), cx)
            })
        .await
        .unwrap();
        assert!(
            result.contains("task worktree shared error"),
            "expected task worktree diagnostic for prefixed path, got: {result}"
        );

        // 3. Absolute host-worktree path is denied in isolated thread with clear message
        let host_file_abs = format!("{}/host_file.rs", host_root);
        let result = cx
            .update(|cx| {
                let input = DiagnosticsToolInput {
                    path: Some(host_file_abs.clone()),
                };
                tool.clone().run(ToolInput::resolved(input), isolated_stream.clone(), cx)
            })
        .await;
        assert!(
            result.is_err(),
            "host-worktree path should be denied in isolated thread"
        );
        let err_text = result.unwrap_err();
        assert!(
            err_text.contains("is outside the isolated task worktree"),
            "error message must name the allowed root: {err_text}"
        );

        // 4. In non-isolated thread, absolute host path still works
        let result = cx
            .update(|cx| {
                let input = DiagnosticsToolInput {
                    path: Some(host_file_abs.clone()),
                };
                tool.clone().run(ToolInput::resolved(input), non_isolated_stream.clone(), cx)
            })
        .await
        .unwrap();
        assert!(
            result.contains("File doesn't have errors or warnings!"),
            "expected non-isolated thread to succeed on host path: {result}"
        );

        // 5. Whole-project filter (is_inside_task_worktree):
        // Publish diagnostic summaries via LSP store for host_file.rs
        let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
        lsp_store.update(cx, |lsp_store, cx| {
            lsp_store
                .update_diagnostics(
                    server_id,
                    lsp::PublishDiagnosticsParams {
                        uri: lsp::Uri::from_file_path(path!("/root/host_file.rs")).unwrap(),
                        version: None,
                        diagnostics: vec![lsp::Diagnostic {
                            range: lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 5)),
                            severity: Some(lsp::DiagnosticSeverity::ERROR),
                            message: lsp::DiagnosticMessage::from("host error message"),
                            ..Default::default()
                        }],
                    },
                    None,
                    language::DiagnosticSourceKind::Pushed,
                    &[],
                    cx,
                )
            .unwrap();
        });
        cx.run_until_parked();

        // In a non-isolated thread, host diagnostics are visible in whole-project summary
        let non_isolated_summary = cx
            .update(|cx| {
                let input = DiagnosticsToolInput { path: None };
                tool.clone().run(ToolInput::resolved(input), non_isolated_stream, cx)
            })
        .await
        .unwrap();
        assert!(
            non_isolated_summary.contains("host_file.rs: 1 error(s), 0 warning(s)"),
            "non-isolated thread should see host diagnostics: {non_isolated_summary}"
        );

        // In an isolated thread, host diagnostics are filtered out via is_inside_task_worktree
        let isolated_summary = cx
            .update(|cx| {
                let input = DiagnosticsToolInput { path: None };
                tool.clone().run(ToolInput::resolved(input), isolated_stream, cx)
            })
        .await
        .unwrap();
        assert!(
            !isolated_summary.contains("host_file.rs"),
            "isolated thread must NOT see host diagnostics: {isolated_summary}"
        );
        assert!(
            isolated_summary.contains("No errors or warnings found in the project."),
            "isolated thread should report no errors when host diagnostics are confined: {isolated_summary}"
        );
    }
}
