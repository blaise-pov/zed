use std::fmt::Write;
use std::sync::Arc;

use agent_client_protocol::schema::v1 as acp;
use collections::HashSet;
use gpui::{App, Entity, SharedString, Task};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::symbol_locator::SymbolLocator;
use crate::{
    AgentTool, ToolCallEventStream, ToolInput, ToolPermissionDecision,
    decide_permission_for_path_with_profile,
};
use agent_settings::{AgentProfileSettings, AgentSettings};
use settings::Settings as _;

/// Renames a symbol across the project using the language server.
///
/// This performs a semantic rename, updating all references to the symbol across all files in the project. The language server determines which occurrences to rename based on the symbol's type and scope.
///
/// Before using this tool, use read_file or grep to find the exact symbol name and line number.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RenameToolInput {
    /// The symbol to rename.
    pub symbol: SymbolLocator,

    /// The new name for the symbol.
    pub new_name: String,
}

pub struct RenameTool {
    project: Entity<Project>,
}

impl RenameTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for RenameTool {
    type Input = RenameToolInput;
    type Output = String;

    const NAME: &'static str = "rename_symbol";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Ok(input) = input {
            format!(
                "Rename `{}` to `{}`",
                input.symbol.symbol_name, input.new_name
            )
            .into()
        } else {
            "Rename symbol".into()
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|e| format!("Failed to receive tool input: {e}"))?;

            let resolved = input.symbol.resolve(&project, cx).await?;

            let profile = cx.update(|cx| event_stream.profile_settings(cx));
            let path_str = resolved.buffer.read_with(cx, |buffer, cx| {
                buffer
                    .file()
                    .map(|file| file.full_path(cx).display().to_string())
                    .unwrap_or_default()
            });
            let decision = cx.update(|cx| {
                decide_permission_for_path_with_profile(
                    Self::NAME,
                    &path_str,
                    &AgentSettings::get_global(cx),
                    profile.as_ref(),
                )
            });
            check_rename_permissions(profile.as_ref(), decision)?;

            let rename_task = project.update(cx, |project, cx| {
                project.perform_rename(
                    resolved.buffer.clone(),
                    resolved.position,
                    input.new_name.clone(),
                    cx,
                )
            });

            let transaction = rename_task
                .await
                .map_err(|e| format!("Rename failed: {e}"))?;

            if transaction.0.is_empty() {
                return Ok(format!(
                    "No changes were made. The language server could not rename '{}'.",
                    input.symbol.symbol_name
                ));
            }

            let buffers = transaction.0.keys().cloned().collect::<HashSet<_>>();
            project
                .update(cx, |project, cx| project.save_buffers(buffers, cx))
                .await
                .map_err(|e| format!("Rename succeeded, but failed to save renamed files: {e}"))?;

            let mut output = format!(
                "Renamed `{}` to `{}` in {} file(s):\n",
                input.symbol.symbol_name,
                input.new_name,
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

/// Permission gate for `rename_symbol`.
///
/// Unlike file tools, a language-server rename edits an unbounded set of
/// project files chosen by the server, so `write_scopes` cannot be enforced
/// per path. Autonomous profiles (those with `tool_permissions`) therefore
/// fail closed: the tool is denied outright rather than risking writes
/// outside the profile's scopes.
fn check_rename_permissions(
    profile: Option<&AgentProfileSettings>,
    decision: ToolPermissionDecision,
) -> Result<(), String> {
    if let Some(profile) = profile
        && profile.tool_permissions.is_some()
    {
        return Err(format!(
            "PolicyDenied: rename_symbol performs language-server-wide edits across an \
             unbounded set of files, so it cannot be confined to write_scopes and is \
             disallowed for autonomous profile '{}'. Use edit_file instead.",
            profile.name
        ));
    }

    if let ToolPermissionDecision::Deny(reason) = decision {
        return Err(reason);
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
            custom_prompt: None,
            description: None,
            skills: None,
            delegation: None,
            tool_permissions: Some(ToolPermissions::default()),
        }
    }

    #[test]
    fn test_rename_denied_for_autonomous_profile() {
        let profile = autonomous_profile();
        let error =
            check_rename_permissions(Some(&profile), ToolPermissionDecision::Allow).unwrap_err();
        assert!(error.contains("PolicyDenied"));
        assert!(error.contains("backend_engineer"));
    }

    #[test]
    fn test_rename_denied_by_deny_decision() {
        let error =
            check_rename_permissions(None, ToolPermissionDecision::Deny("blocked by rule".into()))
                .unwrap_err();
        assert_eq!(error, "blocked by rule");
    }

    #[test]
    fn test_rename_allowed_without_profile_or_deny() {
        assert!(check_rename_permissions(None, ToolPermissionDecision::Allow).is_ok());
        // A non-autonomous profile (no tool_permissions) still allows rename.
        let mut profile = autonomous_profile();
        profile.tool_permissions = None;
        assert!(check_rename_permissions(Some(&profile), ToolPermissionDecision::Allow).is_ok());
    }
}
