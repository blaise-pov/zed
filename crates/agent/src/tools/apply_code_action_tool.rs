use std::fmt::Write;
use std::path::Path;
use std::sync::Arc;

use agent_client_protocol::schema::v1 as acp;
use agent_settings::{AgentPermissionMode, AgentProfileSettings};
use gpui::{App, Entity, SharedString, Task};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::symbol_locator::CodeActionStore;
use crate::{AgentTool, ToolCallEventStream, ToolInput};

/// Applies a code action previously retrieved by get_code_actions.
///
/// You must call get_code_actions first to get the list of available actions,
/// then use the number from that list to choose which action to apply.
///
/// After applying a code action, the list is cleared. If you want to apply
/// another action, call get_code_actions again.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ApplyCodeActionToolInput {
    /// The 1-based index of the code action to apply, from the list
    /// returned by get_code_actions.
    pub index: u32,
}

pub struct ApplyCodeActionTool {
    project: Entity<Project>,
    code_action_store: CodeActionStore,
}

impl ApplyCodeActionTool {
    pub fn new(project: Entity<Project>, code_action_store: CodeActionStore) -> Self {
        Self {
            project,
            code_action_store,
        }
    }
}

impl AgentTool for ApplyCodeActionTool {
    type Input = ApplyCodeActionToolInput;
    type Output = String;

    const NAME: &'static str = "apply_code_action";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        cx: &mut App,
    ) -> SharedString {
        if let Ok(input) = input {
            let title = self
                .code_action_store
                .read(cx)
                .as_ref()
                .and_then(|pending| {
                    let index = input.index.checked_sub(1)? as usize;
                    Some(pending.actions.get(index)?.lsp_action.title().to_string())
                });
            if let Some(title) = title {
                format!("Apply code action: {title}").into()
            } else {
                format!("Apply code action #{}", input.index).into()
            }
        } else {
            "Apply code action".into()
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let project = self.project.clone();
        let store = self.code_action_store.clone();
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|e| format!("Failed to receive tool input: {e}"))?;

            let task_worktree = cx.update(|cx| event_stream.task_worktree(cx));
            let profile = cx.update(|cx| event_stream.profile_settings(cx));
            check_apply_code_action_permissions(profile.as_ref(), task_worktree.as_deref())?;

            let pending = store.update(cx, |store, _cx| store.take()).ok_or_else(|| {
                "No code actions available. Call get_code_actions first.".to_string()
            })?;

            let zero_based_index = input
                .index
                .checked_sub(1)
                .ok_or_else(|| "Index must be 1 or greater.".to_string())?;

            let action = pending
                .actions
                .get(zero_based_index as usize)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "Index {} is out of range. There were {} code action(s) available.",
                        input.index,
                        pending.actions.len()
                    )
                })?;

            let title = action.lsp_action.title().to_string();
            let buffer = pending.buffer.clone();

            let apply_task = project.update(cx, |project, cx| {
                project.apply_code_action(buffer, action, true, cx)
            });

            let transaction = apply_task
                .await
                .map_err(|e| format!("Failed to apply code action '{title}': {e}"))?;

            if transaction.0.is_empty() {
                return Ok(format!(
                    "Code action '{title}' was applied but made no changes.",
                ));
            }

            let mut output = format!(
                "Applied code action '{title}'. Modified {} file(s):\n",
                transaction.0.len()
            );

            for (buffer, _) in &transaction.0 {
                buffer.read_with(cx, |buffer, cx| {
                    let path = buffer
                        .file()
                        .map(|f| f.full_path(cx).display().to_string())
                        .unwrap_or_else(|| "<untitled>".to_string());
                    writeln!(output, "- {path}").ok();
                });
            }

            Ok(output)
        })
    }
}

fn check_apply_code_action_permissions(
    profile: Option<&AgentProfileSettings>,
    task_worktree: Option<&Path>,
) -> Result<(), String> {
    if let Some(task_worktree) = task_worktree {
        return Err(format!(
            "PolicyDenied: apply_code_action performs language-server edits across an \
             unbounded set of files, so it cannot be confined to isolated task worktree '{}'. \
             Use edit_file instead.",
            task_worktree.display()
        ));
    }

    if let Some(profile) = profile
        && profile.effective_permission_mode() == AgentPermissionMode::Autonomous
    {
        return Err(format!(
            "PolicyDenied: apply_code_action performs language-server edits across an \
             unbounded set of files, so it cannot be confined to write_scopes and is \
             disallowed for autonomous profile '{}'. Use edit_file instead.",
            profile.name
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_settings::ToolPermissions;

    fn autonomous_profile() -> AgentProfileSettings {
        AgentProfileSettings {
            name: "backend_engineer".into(),
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
            tool_permissions: Some(ToolPermissions::default()),
            permission_mode: None,
            terminal_wrapper_command: None,
        }
    }

    #[test]
    fn test_apply_code_action_denied_for_worktree_bound() {
        let worktree = Path::new("/worktrees/agent-task-TASK-1");
        let error = check_apply_code_action_permissions(None, Some(worktree)).unwrap_err();
        assert!(error.contains("PolicyDenied"));
        assert!(error.contains("agent-task-TASK-1"));
    }

    #[test]
    fn test_apply_code_action_denied_for_autonomous_profile() {
        let profile = autonomous_profile();
        let error = check_apply_code_action_permissions(Some(&profile), None).unwrap_err();
        assert!(error.contains("PolicyDenied"));
        assert!(error.contains("backend_engineer"));
    }

    #[test]
    fn test_apply_code_action_allowed_without_profile_or_worktree() {
        assert!(check_apply_code_action_permissions(None, None).is_ok());
    }
}
