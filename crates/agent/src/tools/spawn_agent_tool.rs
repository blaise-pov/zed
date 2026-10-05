use acp_thread::{SUBAGENT_SESSION_INFO_META_KEY, SubagentSessionInfo};
use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use gpui::{App, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use std::rc::Rc;
use std::sync::Arc;

use crate::task_worktree::SubagentIsolationDetails;
use crate::{AgentTool, ThreadEnvironment, ToolCallEventStream, ToolInput};
use acp_thread::AgentModelId;
use settings::Settings as _;

/// Spawn a sub-agent for a well-scoped task.
///
/// ### Designing delegated subtasks
/// - An agent does not see your conversation history. Include all relevant context (file paths, requirements, constraints) in the message.
/// - Subtasks must be concrete, well-defined, and self-contained.
/// - Delegated subtasks must materially advance the main task.
/// - Do not duplicate work between your work and delegated subtasks.
/// - Do not use this tool for tasks you could accomplish directly with one or two tool calls. For example, don't ask the agent to read a single file and return the contents, you can do this yourself.
/// - When you delegate work, focus on coordinating and synthesizing results instead of duplicating the same work yourself.
/// - Avoid issuing multiple delegate calls for the same unresolved subproblem unless the new delegated task is genuinely different and necessary.
/// - Narrow the delegated ask to the concrete output you need next.
/// - For code-edit subtasks, decompose work so each delegated task has a disjoint write set.
/// - When sending a follow-up using an existing agent session_id, the agent already has the context from the previous turn. Send only a short, direct message. Do NOT repeat the original task or context.
///
/// ### Parallel delegation patterns
/// - Run multiple independent information-seeking subtasks in parallel when you have distinct questions that can be answered independently.
/// - Split implementation into disjoint codebase slices and spawn multiple agents for them in parallel when the write scopes do not overlap.
/// - When a plan has multiple independent steps, prefer delegating those steps in parallel rather than serializing them unnecessarily.
/// - Reuse the returned session_id when you want to follow up on the same delegated subproblem instead of creating a duplicate session.
///
/// ### Model selection
/// - When the user requests a particular model or asks you to choose based on cost or capability, call `list_agents_and_models` first, then pass the exact `models[].id` from the native Zed agent entry (`is_native: true`) in `model`.
/// - Omit `model` to use the user's configured subagent model, or the parent model when no subagent model is configured.
/// - Do not silently choose a different model when an explicit model is unavailable unless the user allowed fallback.
/// - A resumed session keeps its existing model, so `model` cannot be combined with `session_id`.
///
/// ### Output
/// - You will receive only the agent's final message as output.
/// - Successful calls return a session_id that you can use for follow-up messages.
/// - Error results may also include a session_id if a session was already created.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct SpawnAgentToolInput {
    /// Short label displayed in the UI while the agent runs (e.g., "Researching alternatives")
    pub label: String,
    /// The prompt for the agent. For new sessions, include full context needed for the task. For follow-ups (with session_id), you can rely on the agent already having the previous message.
    pub message: String,
    /// Session ID of an existing agent session to continue instead of creating a new one. Omit to create a new agent.
    #[serde(default, deserialize_with = "deserialize_session_id")]
    pub session_id: Option<acp::SessionId>,
    /// Optional reference to a task managed in the Task Graph Service (e.g. `TASK-42`).
    /// The runtime registers or links this task in the task graph service (get-or-create,
    /// best-effort) before spawning, and the spawned agent is expected to fetch details
    /// and drive the task to completion via the task tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Optional profile ID to use for this subagent. If not specified, the subagent will use the default profile.
    /// The profile controls which tools are available and can specify a default model.
    #[serde(default)]
    pub profile: Option<agent_settings::AgentProfileId>,
    /// Goal identifier for goal-scoped task coordination (§8.2).
    /// When specified, the task worktree forks from the goal's integration branch
    /// `agent-goal/{goal_id}` (created lazily from HEAD of main if absent).
    #[serde(default)]
    pub goal_id: Option<String>,
    /// Base branch or ref to fork the task worktree from (§8.2).
    /// Overrides goal-tip as the fork base when both are specified. If the ref
    /// matches `agent-goal/*`, it is auto-created lazily if absent; otherwise,
    /// it must exist in the repository.
    #[serde(default)]
    pub base_branch: Option<String>,
    /// Existing branch to check out into the subagent's task worktree (§8.2).
    /// Used for review, CI, inspection, or conflict resolution without creating a new branch.
    /// The subagent gets its own worktree on this existing branch, and writes are restricted
    /// to this worktree. Mutually exclusive with `base_branch`. Accepts a branch name or the
    /// special value "goal", which resolves to `agent-goal/{goal_id}`.
    #[serde(default)]
    pub on_branch: Option<String>,
    /// Optional model override. Pass the exact `models[].id` returned for the
    /// native Zed agent (`is_native: true`) by `list_agents_and_models`.
    /// Omit to preserve default behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

fn deserialize_session_id<'de, D>(deserializer: D) -> Result<Option<acp::SessionId>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(value) = Option::<serde_json::Value>::deserialize(deserializer)? else {
        return Ok(None);
    };

    if value
        .as_str()
        .is_some_and(|session_id| session_id.trim().is_empty())
    {
        return Ok(None);
    }

    serde_json::from_value(value)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

/// Ensures the profile requested via `spawn_agent` exists, so a subagent is
/// never silently spawned with an empty toolset (`enabled_tools` fails closed
/// for unknown profiles). Returns a model-facing error message on failure.
fn validate_profile(
    profile: Option<&agent_settings::AgentProfileId>,
    location: Option<settings::SettingsLocation>,
    cx: &App,
) -> Option<String> {
    let profile = profile?;
    let settings = agent_settings::AgentSettings::get(location, cx);
    if settings.profiles.contains_key(profile) {
        return None;
    }
    let available_profiles = settings
        .profiles
        .keys()
        .map(|id| id.as_str().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "Unknown profile '{}'. Available profiles: {}",
        profile.as_str(),
        available_profiles
    ))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[serde(rename_all = "snake_case")]
pub enum SpawnAgentToolOutput {
    Success {
        session_id: acp::SessionId,
        output: String,
        session_info: SubagentSessionInfo,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        isolation: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        isolation_details: Option<SubagentIsolationDetails>,
    },
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        session_id: Option<acp::SessionId>,
        error: String,
        session_info: Option<SubagentSessionInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        isolation: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        isolation_details: Option<SubagentIsolationDetails>,
    },
}

impl From<SpawnAgentToolOutput> for LanguageModelToolResultContent {
    fn from(output: SpawnAgentToolOutput) -> Self {
        match output {
            SpawnAgentToolOutput::Success {
                session_id,
                output,
                session_info: _, // Don't show this to the model
                isolation,
                isolation_details,
            } => {
                let mut map = serde_json::json!({ "session_id": session_id, "output": output });
                if let Some(isolation) = isolation {
                    map["isolation"] = serde_json::Value::String(isolation);
                }
                if let Some(details) = isolation_details {
                    map["isolation_details"] = serde_json::to_value(details).unwrap_or_default();
                }
                serde_json::to_string(&map)
                    .unwrap_or_else(|e| format!("Failed to serialize spawn_agent output: {e}"))
                    .into()
            }
            SpawnAgentToolOutput::Error {
                session_id,
                error,
                session_info: _, // Don't show this to the model
                isolation,
                isolation_details,
            } => {
                let mut map = serde_json::json!({ "session_id": session_id, "error": error });
                if let Some(isolation) = isolation {
                    map["isolation"] = serde_json::Value::String(isolation);
                }
                if let Some(details) = isolation_details {
                    map["isolation_details"] = serde_json::to_value(details).unwrap_or_default();
                }
                serde_json::to_string(&map)
                    .unwrap_or_else(|e| format!("Failed to serialize spawn_agent output: {e}"))
                    .into()
            }
        }
    }
}

/// Tool that spawns an agent thread to work on a task.
pub struct SpawnAgentTool {
    environment: Rc<dyn ThreadEnvironment>,
}

impl SpawnAgentTool {
    pub fn new(environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self { environment }
    }
}

impl AgentTool for SpawnAgentTool {
    type Input = SpawnAgentToolInput;
    type Output = SpawnAgentToolOutput;

    const NAME: &'static str = "spawn_agent";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(i) => i.label.into(),
            Err(value) => value
                .get("label")
                .and_then(|v| v.as_str())
                .map(|s| SharedString::from(s.to_owned()))
                .unwrap_or_else(|| "Spawning agent".into()),
        }
    }

    #[allow(clippy::result_large_err)]
    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |mut cx| {
            let input = input
                .recv()
                .await
                .map_err(|e| SpawnAgentToolOutput::Error {
                    session_id: None,
                    error: e.to_string(),
                    session_info: None,
                    isolation: None,
                    isolation_details: None,
                })?;

            if input.session_id.is_some() && input.model.is_some() {
                return Err(SpawnAgentToolOutput::Error {
                    session_id: input.session_id.clone(),
                    error: "model cannot be changed when resuming a subagent session".to_string(),
                    session_info: None,
                    isolation: None,
                    isolation_details: None,
                });
            }

            if input.on_branch.is_some() && input.base_branch.is_some() {
                return Err(SpawnAgentToolOutput::Error {
                    session_id: input.session_id.clone(),
                    error: "on_branch and base_branch are mutually exclusive: on_branch checks out an existing branch directly, while base_branch creates a new branch forked from a base ref".to_string(),
                    session_info: None,
                    isolation: None,
                    isolation_details: None,
                });
            }

            if input.on_branch.as_deref() == Some("goal") && input.goal_id.is_none() {
                return Err(SpawnAgentToolOutput::Error {
                    session_id: input.session_id.clone(),
                    error: "on_branch 'goal' requires goal_id".to_string(),
                    session_info: None,
                    isolation: None,
                    isolation_details: None,
                });
            }

            let raw_task_id = input
                .task_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);

            if (input.base_branch.is_some() || input.on_branch.is_some())
                && raw_task_id.is_none()
                && input.session_id.is_none()
            {
                return Err(SpawnAgentToolOutput::Error {
                    session_id: input.session_id.clone(),
                    error: "base_branch/on_branch requires task_id: git isolation is task-scoped".to_string(),
                    session_info: None,
                    isolation: None,
                    isolation_details: None,
                });
            }

            let task_id = if raw_task_id.is_some() {
                raw_task_id
            } else if let Some(session_id) = &input.session_id {
                cx.update(|cx| self.environment.subagent_task_id(session_id, cx))
            } else {
                None
            };

            let (effective_profile_id, task_isolation, enable_lsp) = cx.update(|cx| {
                let location = event_stream.settings_location(cx);
                if let Some(error) = validate_profile(input.profile.as_ref(), location, cx) {
                    return Err(SpawnAgentToolOutput::Error {
                        session_id: input.session_id.clone(),
                        error,
                        session_info: None,
                        isolation: None,
                        isolation_details: None,
                    });
                }

                let settings = agent_settings::AgentSettings::get(location, cx);
                let effective_profile_id = if let Some(ref profile_id) = input.profile {
                    profile_id.clone()
                } else if let Some(caller_profile_id) = event_stream.profile_id(cx) {
                    caller_profile_id
                } else {
                    settings.default_profile.clone()
                };

                let profile_settings = settings.profiles.get(&effective_profile_id);
                let task_isolation = profile_settings
                    .map(|p| p.task_isolation)
                    .unwrap_or(agent_settings::TaskIsolation::Optional);

                let enable_lsp = profile_settings
                    .and_then(|p| p.task_worktree_language_servers)
                    .unwrap_or(settings.task_worktree_language_servers);

                Ok((effective_profile_id, task_isolation, enable_lsp))
            })?;

            if task_isolation.is_required() && task_id.is_none() {
                return Err(SpawnAgentToolOutput::Error {
                    session_id: input.session_id.clone(),
                    error: format!(
                        "profile '{effective_profile_id}' requires task-scoped isolation; pass task_id"
                    ),
                    session_info: None,
                    isolation: None,
                    isolation_details: None,
                });
            }

            if task_isolation.is_disabled() && (input.base_branch.is_some() || input.on_branch.is_some()) {
                return Err(SpawnAgentToolOutput::Error {
                    session_id: input.session_id.clone(),
                    error: format!(
                        "profile '{effective_profile_id}' has isolation disabled; base_branch/on_branch are not applicable"
                    ),
                    session_info: None,
                    isolation: None,
                    isolation_details: None,
                });
            }

            let session_guard = if let Some(ref task_id) = task_id {
                let task_id_typed = crate::AgentTaskId::from(task_id.clone());
                if crate::task_worktree::has_active_subagent_session(&task_id_typed) {
                    return Err(SpawnAgentToolOutput::Error {
                        session_id: input.session_id.clone(),
                        error: format!(
                            "task {task_id} already has an active session; wait for it to finish or resume via session_id later"
                        ),
                        session_info: None,
                        isolation: None,
                        isolation_details: None,
                    });
                }
                Some(crate::task_worktree::SubagentSessionGuard::new(
                    task_id_typed,
                ))
            } else {
                None
            };

            let (worktree_path, isolation) = if task_isolation.is_disabled() {
                (None, Some("shared:profile-disabled".to_string()))
            } else if let Some(ref task_id) = task_id {
                let task_id_typed = crate::AgentTaskId::from(task_id.clone());
                crate::task_worktree::set_task_lsp_suppressed(&task_id_typed, !enable_lsp);
                match self
                    .environment
                    .ensure_subagent_worktree(
                        task_id,
                        input.goal_id.clone(),
                        input.base_branch.clone(),
                        input.on_branch.clone(),
                        &mut cx,
                    )
                    .await
                {
                    Ok(path) => {
                        let isolation_str = if let Some(target) = crate::task_worktree::task_worktree_checkout_target(&task_id_typed) {
                            format!("checkout:{target}")
                        } else if let Some(ref on_b) = input.on_branch {
                            let resolved = if on_b == "goal" {
                                format!("agent-goal/{}", input.goal_id.as_deref().unwrap_or_default())
                            } else {
                                on_b.clone()
                            };
                            format!("checkout:{resolved}")
                        } else {
                            format!("worktree:agent-task/{task_id}")
                        };
                        (Some(path), Some(isolation_str))
                    }
                    Err(err) => {
                        if err
                            .downcast_ref::<crate::task_worktree::NoGitRepositoryError>()
                            .is_some()
                            || err
                                .root_cause()
                                .is::<crate::task_worktree::NoGitRepositoryError>()
                        {
                            (None, Some("shared:no-git-repository".to_string()))
                        } else {
                            let err_msg = err.to_string();
                            return Err(SpawnAgentToolOutput::Error {
                                session_id: input.session_id.clone(),
                                error: format!(
                                    "Failed to isolate subagent in git worktree: {err_msg}"
                                ),
                                session_info: None,
                                isolation: None,
                                isolation_details: None,
                            });
                        }
                    }
                }
            } else {
                (None, None)
            };

            let (subagent, mut session_info) = cx.update(|cx| {
                if let Some(ref path) = worktree_path {
                    if let Some(project) = event_stream.project() {
                        let worktree_id = project.read_with(cx, |project, cx| {
                            project.worktrees(cx).find(|w| {
                                let w_abs = w.read(cx).abs_path();
                                w_abs.as_ref() == path
                                    || util::paths::normalize_lexically(w_abs.as_ref()).ok().as_deref()
                                        == util::paths::normalize_lexically(path).ok().as_deref()
                            }).map(|w| w.read(cx).id())
                        });
                        if let Some(worktree_id) = worktree_id {
                            project.update(cx, |project, cx| {
                                project.set_worktree_language_servers_suppressed(worktree_id, !enable_lsp, cx);
                            });
                        }
                    }
                }
                let subagent = if let Some(session_id) = input.session_id {
                    self.environment.resume_subagent(
                        session_id,
                        input.profile.clone(),
                        worktree_path.clone(),
                        cx,
                    )
                } else {
                    self.environment.create_subagent(
                        input.label.clone(),
                        input.profile,
                        input.model.map(AgentModelId::from),
                        worktree_path.clone(),
                        cx,
                    )
                };
                let subagent = subagent.map_err(|err| SpawnAgentToolOutput::Error {
                    session_id: None,
                    error: err.to_string(),
                    session_info: None,
                    isolation: isolation.clone(),
                    isolation_details: None,
                })?;
                if let Some(ref task_id) = task_id {
                    subagent.set_task_id(Some(task_id.clone()), cx);
                    if let Some(ref worktree_path) = worktree_path {
                        let task_id_typed = crate::AgentTaskId::from(task_id.clone());
                        if crate::task_worktree::task_worktree_path_for_id(&task_id_typed).is_none() {
                            crate::task_worktree::register_task_worktree(
                                &task_id_typed,
                                worktree_path.clone(),
                            );
                        }
                    }
                }
                let session_info = SubagentSessionInfo {
                    session_id: subagent.id(),
                    message_start_index: subagent.num_entries(cx),
                    message_end_index: None,
                };

                event_stream.subagent_spawned(subagent.id());
                event_stream.update_fields_with_meta(
                    acp::ToolCallUpdateFields::new(),
                    Some(acp::Meta::from_iter([(
                        SUBAGENT_SESSION_INFO_META_KEY.into(),
                        serde_json::json!(&session_info),
                    )])),
                );

                Ok((subagent, session_info))
            })?;

            if let Some(ref task_id) = task_id {
                if let Err(error) = self
                    .environment
                    .ensure_task_registered(task_id, &input.label, &mut cx)
                    .await
                {
                    log::warn!(
                        "failed to ensure task {task_id} registered in task graph service: {error:#}"
                    );
                }
            }

            let send_result = {
                let message = match input.task_id.as_deref() {
                    Some(task_id) => format!(
                        "{message}\n\nTask reference: {task_id}. Fetch the details of this task \
                         yourself via the task-management tools available to you.",
                        message = input.message
                    ),
                    None => input.message.clone(),
                };
                subagent.send(message, cx).await
            };

            let status = if send_result.is_ok() {
                "completed"
            } else {
                "error"
            };
            telemetry::event!(
                "Subagent Completed",
                subagent_session = session_info.session_id.to_string(),
                status,
            );

            session_info.message_end_index =
                cx.update(|cx| Some(subagent.num_entries(cx).saturating_sub(1)));

            if let Err(ref error) = send_result {
                if let Some(ref task_id) = task_id {
                    let error_message = error.to_string();
                    if crate::classify_send_error(&error_message) == crate::SendErrorKind::Definitive {
                        if let Err(failure_error) = self
                            .environment
                            .fail_task_registered(task_id, &error_message, &mut cx)
                            .await
                        {
                            log::warn!(
                                "failed to auto-fail task {task_id} in task graph service: {failure_error:#}"
                            );
                        }
                    }
                }
            }

            let is_error = send_result.is_err();
            let isolation_details =
                if let (Some(worktree_path), Some(task_id)) = (&worktree_path, &task_id) {
                    let details = match self
                        .environment
                        .commit_subagent_worktree(task_id, worktree_path, is_error, &mut cx)
                        .await
                    {
                        Ok(details) => details,
                        Err(e) => {
                            let task_id_typed = crate::AgentTaskId::from(task_id.clone());
                            crate::task_worktree::fallback_isolation_details(
                                &task_id_typed,
                                "worktree",
                                Some(e.to_string()),
                                None,
                            )
                        }
                    };
                    if let Some(ref err) = details.commit_error {
                        crate::task_worktree::record_task_worktree_commit_error(
                            &crate::AgentTaskId::from(task_id.clone()),
                            err.clone(),
                        );
                    } else {
                        crate::task_worktree::clear_task_worktree_commit_error(
                            &crate::AgentTaskId::from(task_id.clone()),
                        );
                    }
                    Some(details)
                } else {
                    None
                };

            // Release active session guard after commit is done to close race window
            drop(session_guard);

            if let Some(ref task_id) = task_id {
                let task_id_typed = crate::AgentTaskId::from(task_id.clone());
                if crate::task_worktree::is_task_terminal(&task_id_typed)
                    && crate::task_worktree::task_worktree_commit_error(&task_id_typed).is_none()
                    && crate::task_worktree::task_worktree_merge_conflict(&task_id_typed).is_none()
                {
                    if let Err(err) = self
                        .environment
                        .auto_cleanup_subagent_worktree(task_id, &mut cx)
                        .await
                    {
                        log::error!("auto cleanup failed for subagent task {task_id}: {err:?}");
                    }
                }
            }

            let meta = Some(acp::Meta::from_iter([(
                SUBAGENT_SESSION_INFO_META_KEY.into(),
                serde_json::json!(&session_info),
            )]));

            let (output, result) = match send_result {
                Ok(output) => (
                    output.clone(),
                    Ok(SpawnAgentToolOutput::Success {
                        session_id: session_info.session_id.clone(),
                        session_info,
                        output,
                        isolation: isolation.clone(),
                        isolation_details: isolation_details.clone(),
                    }),
                ),
                Err(e) => {
                    let error = e.to_string();
                    (
                        error.clone(),
                        Err(SpawnAgentToolOutput::Error {
                            session_id: Some(session_info.session_id.clone()),
                            error,
                            session_info: Some(session_info),
                            isolation: isolation.clone(),
                            isolation_details: isolation_details.clone(),
                        }),
                    )
                }
            };
            event_stream.update_fields_with_meta(
                acp::ToolCallUpdateFields::new().content(vec![output.into()]),
                meta,
            );
            result
        })
    }

    fn replay(
        &self,
        _input: Self::Input,
        output: Self::Output,
        event_stream: ToolCallEventStream,
        _cx: &mut App,
    ) -> Result<()> {
        let (content, session_info) = match output {
            SpawnAgentToolOutput::Success {
                output,
                session_info,
                ..
            } => (output.into(), Some(session_info)),
            SpawnAgentToolOutput::Error {
                error,
                session_info,
                ..
            } => (error.into(), session_info),
        };

        let meta = session_info.map(|session_info| {
            acp::Meta::from_iter([(
                SUBAGENT_SESSION_INFO_META_KEY.into(),
                serde_json::json!(&session_info),
            )])
        });
        event_stream.update_fields_with_meta(
            acp::ToolCallUpdateFields::new().content(vec![content]),
            meta,
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserializes_blank_session_id_as_absent() {
        for session_id in [json!(null), json!(""), json!("   ")] {
            let input: SpawnAgentToolInput = serde_json::from_value(json!({
                "label": "label",
                "message": "message",
                "session_id": session_id,
            }))
            .unwrap();

            assert!(input.session_id.is_none());
        }

        let input: SpawnAgentToolInput = serde_json::from_value(json!({
            "label": "label",
            "message": "message",
        }))
        .unwrap();
        assert!(input.session_id.is_none());

        let input: SpawnAgentToolInput = serde_json::from_value(json!({
            "label": "label",
            "message": "message",
            "session_id": "existing-session",
        }))
        .unwrap();
        assert_eq!(input.session_id.unwrap().to_string(), "existing-session");
    }

    #[gpui::test]
    fn validates_profile_against_configured_profiles(cx: &mut gpui::App) {
        let store = settings::SettingsStore::test(cx);
        cx.set_global(store);

        assert_eq!(validate_profile(None, None, cx), None);
        assert_eq!(
            validate_profile(
                Some(&agent_settings::AgentProfileId("write".into())),
                None,
                cx,
            ),
            None
        );

        let error = validate_profile(
            Some(&agent_settings::AgentProfileId("nonexistent".into())),
            None,
            cx,
        )
        .expect("unknown profile should be rejected");
        assert!(
            error.starts_with("Unknown profile 'nonexistent'."),
            "unexpected error: {error}"
        );
    }
}
