use acp_thread::{SUBAGENT_SESSION_INFO_META_KEY, SubagentSessionInfo};
use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use futures::FutureExt;
use gpui::{App, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crate::agent_task::AgentDiffShortstat;
use crate::task_worktree::SubagentIsolationDetails;
use crate::{AgentTool, Template as _, ThreadEnvironment, ToolCallEventStream, ToolInput};
use acp_thread::AgentModelId;
use settings::Settings as _;
use util::ResultExt as _;

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
    /// Resumed sessions inherit isolation and fork base from session creation; branch params, worktree_of, and goal_id are forbidden on resume.
    #[serde(default, deserialize_with = "deserialize_session_id")]
    pub session_id: Option<acp::SessionId>,
    /// Optional reference to a task managed in the Task Graph Service (e.g. `TASK-42`).
    /// Required when task isolation is required by profile, or when base_branch, on_branch, or worktree_of is specified.
    /// If passed alone with an optional-isolation profile and no branch params, the agent runs in shared mode (isolation: shared:task-linked).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Optional profile ID to use for this subagent. If not specified, the subagent will use the default profile.
    /// The profile controls which tools are available, default model, and whether task isolation (dedicated git worktree) is required, optional, or disabled.
    #[serde(default)]
    pub profile: Option<agent_settings::AgentProfileId>,
    /// Goal identifier for goal-scoped task coordination (§8.2).
    /// Requires task isolation (dedicated worktree or guest mode). In dedicated worktree mode, forks from the goal's integration branch
    /// `agent-goal/{goal_id}` (created lazily from HEAD of main if absent). In guest mode (`worktree_of`), serves as task graph linkage without forking.
    #[serde(default)]
    pub goal_id: Option<String>,
    /// Base branch or ref to fork the task worktree from (§8.2).
    /// Requires task_id. Mutually exclusive with on_branch and worktree_of.
    /// Overrides goal-tip as the fork base when both are specified. If the ref
    /// matches `agent-goal/*`, it is auto-created lazily if absent; otherwise,
    /// it must exist in the repository.
    #[serde(default)]
    pub base_branch: Option<String>,
    /// Existing branch to check out into the subagent's task worktree (§8.2).
    /// Requires task_id. Used for review, CI, inspection, or conflict resolution without creating a new branch.
    /// The subagent gets its own worktree on this existing branch, and writes are restricted
    /// to this worktree. Mutually exclusive with base_branch and worktree_of. Branch names must be explicit (e.g. `agent-goal/GOAL-21`).
    #[serde(default)]
    pub on_branch: Option<String>,
    /// Host task ID whose git worktree this subagent will share in guest mode (§8.2).
    /// Requires task_id. Mutually exclusive with base_branch and on_branch.
    /// The subagent runs inside the host's existing worktree (borrowing its working directory),
    /// but cannot commit, merge, or cleanup the host worktree.
    /// Forbidden if profile task_isolation is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_of: Option<String>,
    /// Optional model override. Pass the exact `models[].id` returned for the
    /// native Zed agent (`is_native: true`) by `list_agents_and_models`.
    /// Omit to preserve default behavior. Cannot be changed when resuming an existing session.
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

#[derive(Debug)]
pub(crate) enum StepOutcome<T> {
    Ok(T),
    Err(anyhow::Error),
    Timeout,
}

pub(crate) fn log_lifecycle_step(
    task_id: Option<&str>,
    step: &str,
    duration_ms: u64,
    outcome: &str,
) {
    log::info!(
        "[cancel-audit] task_id={} step={} duration_ms={} outcome={}",
        task_id.unwrap_or("-"),
        step,
        duration_ms,
        outcome
    );
}

pub(crate) fn maybe_timer(
    executor: &gpui::BackgroundExecutor,
    timeout: Duration,
) -> futures::future::Either<futures::future::Pending<()>, Task<()>> {
    if timeout.is_zero() {
        futures::future::Either::Left(futures::future::pending::<()>())
    } else {
        futures::future::Either::Right(executor.timer(timeout))
    }
}

pub(crate) async fn timeout_step<F, T>(
    task_id: Option<&str>,
    step: &str,
    timeout: Duration,
    cx: &gpui::AsyncApp,
    future: F,
) -> StepOutcome<T>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    let start = std::time::Instant::now();
    let timer = maybe_timer(cx.background_executor(), timeout).fuse();
    futures::pin_mut!(timer);
    let future = future.fuse();
    futures::pin_mut!(future);

    futures::select_biased! {
        result = future => match result {
            Ok(value) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                log_lifecycle_step(task_id, step, duration_ms, "success");
                StepOutcome::Ok(value)
            }
            Err(err) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                log_lifecycle_step(task_id, step, duration_ms, "error");
                StepOutcome::Err(err)
            }
        },
        _ = timer => {
            let duration_ms = start.elapsed().as_millis() as u64;
            log_lifecycle_step(task_id, step, duration_ms, "timeout");
            StepOutcome::Timeout
        }
    }
}

/// Tool that spawns an agent thread to work on a task.
fn cancelled_error(
    stage: &'static str,
    session_id: Option<acp::SessionId>,
    session_info: Option<SubagentSessionInfo>,
    isolation: Option<String>,
    isolation_details: Option<SubagentIsolationDetails>,
) -> SpawnAgentToolOutput {
    SpawnAgentToolOutput::Error {
        session_id,
        error: format!("spawn_agent cancelled at {stage}"),
        session_info,
        isolation,
        isolation_details,
    }
}

struct WorktreeHolderGuard {
    host: crate::AgentTaskId,
    session_id: String,
}

impl WorktreeHolderGuard {
    fn new(host: crate::AgentTaskId, session_id: String) -> Self {
        crate::task_worktree::register_worktree_holder(&host, &session_id);
        Self { host, session_id }
    }
}

impl Drop for WorktreeHolderGuard {
    fn drop(&mut self) {
        crate::task_worktree::unregister_worktree_holder(&self.host, &self.session_id);
    }
}

pub struct SpawnAgentTool {
    environment: Rc<dyn ThreadEnvironment>,
    templates: Arc<crate::Templates>,
}

impl SpawnAgentTool {
    pub fn new(environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self {
            environment,
            templates: crate::Templates::new(),
        }
    }

    pub fn with_templates(mut self, templates: Arc<crate::Templates>) -> Self {
        self.templates = templates;
        self
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
            if event_stream.was_cancelled_by_user() {
                return Err(cancelled_error("input receive", None, None, None, None));
            }

            let input = futures::select_biased! {
                _ = event_stream.cancelled_by_user().fuse() => {
                    return Err(cancelled_error("input receive", None, None, None, None));
                }
                input_res = input.recv().fuse() => {
                    input_res.map_err(|e| SpawnAgentToolOutput::Error {
                        session_id: None,
                        error: e.to_string(),
                        session_info: None,
                        isolation: None,
                        isolation_details: None,
                    })?
                }
            };

            if input.session_id.is_some() && input.model.is_some() {
                return Err(SpawnAgentToolOutput::Error {
                    session_id: input.session_id.clone(),
                    error: "model cannot be changed when resuming a subagent session".to_string(),
                    session_info: None,
                    isolation: None,
                    isolation_details: None,
                });
            }

            if input.session_id.is_some()
                && (input.base_branch.is_some()
                    || input.on_branch.is_some()
                    || input.worktree_of.is_some()
                    || input.goal_id.is_some())
            {
                return Err(SpawnAgentToolOutput::Error {
                    session_id: input.session_id.clone(),
                    error: "isolation and fork base are fixed at session creation".to_string(),
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

            if input.worktree_of.is_some()
                && (input.base_branch.is_some() || input.on_branch.is_some())
            {
                return Err(SpawnAgentToolOutput::Error {
                    session_id: input.session_id.clone(),
                    error: "worktree_of is mutually exclusive with base_branch and on_branch: guest mode runs in the host worktree".to_string(),
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

            if (input.base_branch.is_some()
                || input.on_branch.is_some()
                || input.worktree_of.is_some())
                && raw_task_id.is_none()
            {
                return Err(SpawnAgentToolOutput::Error {
                    session_id: input.session_id.clone(),
                    error: "base_branch/on_branch/worktree_of requires task_id: git isolation is task-scoped".to_string(),
                    session_info: None,
                    isolation: None,
                    isolation_details: None,
                });
            }

            if let Some(ref session_id) = input.session_id {
                let load_fut = self.environment.ensure_subagent_loaded(session_id, &mut cx);
                futures::select_biased! {
                    _ = event_stream.cancelled_by_user().fuse() => {
                        return Err(cancelled_error(
                            "input receive",
                            input.session_id.clone(),
                            None,
                            None,
                            None,
                        ));
                    }
                    load_res = load_fut.fuse() => {
                        if let Err(err) = load_res {
                            return Err(SpawnAgentToolOutput::Error {
                                session_id: input.session_id.clone(),
                                error: format!("Failed to load subagent session: {err}"),
                                session_info: None,
                                isolation: None,
                                isolation_details: None,
                            });
                        }
                    }
                }
            }

            let task_id = if raw_task_id.is_some() {
                raw_task_id
            } else if let Some(session_id) = &input.session_id {
                cx.update(|cx| self.environment.subagent_task_id(session_id, cx))
            } else {
                None
            };

            let (
                effective_profile_id,
                task_isolation,
                enable_lsp,
                is_agent_mode,
                should_register_metadata,
                should_set_task_status,
                should_commit,
                should_cleanup,
                step_timeouts,
            ) = cx.update(|cx| {
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
                let step_timeouts = settings.spawn_step_timeout_seconds;
                let (effective_profile_id, task_isolation, enable_lsp) =
                    if let Some(ref session_id) = input.session_id {
                        let session_profile_id =
                            self.environment.subagent_profile_id(session_id, cx);
                        let session_profile_settings = session_profile_id
                            .as_ref()
                            .and_then(|id| settings.profiles.get(id));
                        let task_isolation = session_profile_settings
                            .map(|p| p.task_isolation)
                            .unwrap_or(agent_settings::TaskIsolation::Optional);

                        let effective_profile_id = input
                            .profile
                            .clone()
                            .or(session_profile_id)
                            .unwrap_or_else(|| settings.default_profile.clone());

                        let profile_settings = settings.profiles.get(&effective_profile_id);
                        let enable_lsp = profile_settings
                            .and_then(|p| p.task_worktree_language_servers)
                            .unwrap_or(settings.task_worktree_language_servers);

                        (effective_profile_id, task_isolation, enable_lsp)
                    } else {
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

                        (effective_profile_id, task_isolation, enable_lsp)
                    };

                let is_agent_mode =
                    settings.task_lifecycle_mode == agent_settings::TaskLifecycleMode::Agent;
                let should_register_metadata = !is_agent_mode && settings.register_metadata;
                let should_set_task_status = !is_agent_mode;
                let should_commit = !is_agent_mode && settings.commit_on_session_end;
                let should_cleanup = !is_agent_mode && settings.cleanup;

                Ok((
                    effective_profile_id,
                    task_isolation,
                    enable_lsp,
                    is_agent_mode,
                    should_register_metadata,
                    should_set_task_status,
                    should_commit,
                    should_cleanup,
                    step_timeouts,
                ))
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

            if task_isolation.is_required() && input.worktree_of.is_some() {
                return Err(SpawnAgentToolOutput::Error {
                    session_id: input.session_id.clone(),
                    error: format!(
                        "profile '{effective_profile_id}' requires dedicated task isolation; worktree_of guest mode is not allowed"
                    ),
                    session_info: None,
                    isolation: None,
                    isolation_details: None,
                });
            }

            if task_isolation.is_disabled()
                && (input.base_branch.is_some()
                    || input.on_branch.is_some()
                    || input.worktree_of.is_some())
            {
                return Err(SpawnAgentToolOutput::Error {
                    session_id: input.session_id.clone(),
                    error: format!(
                        "profile '{effective_profile_id}' has isolation disabled; base_branch/on_branch/worktree_of are not applicable"
                    ),
                    session_info: None,
                    isolation: None,
                    isolation_details: None,
                });
            }

            let is_guest = input.worktree_of.is_some();
            let wants_worktree = task_isolation.is_required()
                || input.base_branch.is_some()
                || input.on_branch.is_some();
            let is_shared = !is_guest && !wants_worktree;

            if input.goal_id.is_some() && is_shared {
                return Err(SpawnAgentToolOutput::Error {
                    session_id: input.session_id.clone(),
                    error: "goal_id requires task isolation".to_string(),
                    session_info: None,
                    isolation: None,
                    isolation_details: None,
                });
            }

            let task_detail = if let Some(ref task_id) = task_id {
                let get_fut = self.environment.get_task_registered(task_id, &mut cx);
                let detail_res = futures::select_biased! {
                    _ = event_stream.cancelled_by_user().fuse() => {
                        return Err(cancelled_error(
                            "input receive",
                            input.session_id.clone(),
                            None,
                            None,
                            None,
                        ));
                    }
                    res = get_fut.fuse() => res,
                };
                match detail_res {
                    Ok(detail) => Some(detail),
                    Err(error) => {
                        return Err(SpawnAgentToolOutput::Error {
                            session_id: input.session_id.clone(),
                            error: format!(
                                "Task '{task_id}' does not exist in Task Graph Service: {error:#}. Tasks must be created first via task_create before spawning an agent."
                            ),
                            session_info: None,
                            isolation: None,
                            isolation_details: None,
                        });
                    }
                }
            } else {
                None
            };

            let task_status_str = task_detail
                .as_ref()
                .map(|d| format!("{:?}", d.summary.status))
                .unwrap_or_else(|| "-".to_string());
            let owner_session_id = cx.update(|cx| {
                event_stream
                    .session_id(cx)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "-".to_string())
            });
            let tool_use_id = event_stream.tool_use_id().clone();
            log::info!(
                "[cancel-audit] task_id={} owner_session_id={} tool_use_id={} task_status={} source=spawn_start",
                task_id.as_deref().unwrap_or("-"),
                owner_session_id,
                tool_use_id,
                task_status_str
            );

            let session_guard = if let Some(ref host_task_id_str) = input.worktree_of {
                let host_task_id = crate::AgentTaskId::from(host_task_id_str.clone());
                if crate::task_worktree::has_active_subagent_session(&host_task_id) {
                    return Err(SpawnAgentToolOutput::Error {
                        session_id: input.session_id.clone(),
                        error: format!(
                            "host task '{host_task_id_str}' already has an active session; wait for it to finish"
                        ),
                        session_info: None,
                        isolation: None,
                        isolation_details: None,
                    });
                }
                let owner = cx.update(|cx| {
                    let thread_weak = event_stream.thread().cloned();
                    let thread_id = event_stream
                        .session_id(cx)
                        .unwrap_or_else(|| acp::SessionId::new("standalone"));
                    let tool_use_id = event_stream.tool_use_id().clone();
                    let _is_test = thread_weak.is_none();
                    crate::task_worktree::SubagentSessionOwner {
                        thread_id,
                        tool_use_id,
                        thread: thread_weak,
                        #[cfg(any(test, feature = "test-support"))]
                        is_test_live: _is_test,
                    }
                });
                Some(crate::task_worktree::SubagentSessionGuard::new(
                    host_task_id,
                    owner,
                ))
            } else if let Some(ref task_id) = task_id {
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
                let owner = cx.update(|cx| {
                    let thread_weak = event_stream.thread().cloned();
                    let thread_id = event_stream
                        .session_id(cx)
                        .unwrap_or_else(|| acp::SessionId::new("standalone"));
                    let tool_use_id = event_stream.tool_use_id().clone();
                    let _is_test = thread_weak.is_none();
                    crate::task_worktree::SubagentSessionOwner {
                        thread_id,
                        tool_use_id,
                        thread: thread_weak,
                        #[cfg(any(test, feature = "test-support"))]
                        is_test_live: _is_test,
                    }
                });
                Some(crate::task_worktree::SubagentSessionGuard::new(
                    task_id_typed,
                    owner,
                ))
            } else {
                None
            };

            if event_stream.was_cancelled_by_user() {
                drop(session_guard);
                return Err(cancelled_error(
                    "worktree acquisition",
                    input.session_id.clone(),
                    None,
                    None,
                    None,
                ));
            }

            let (worktree_path, isolation) = if task_isolation.is_disabled() {
                (None, Some("shared:profile-disabled".to_string()))
            } else if let Some(ref host_id) = input.worktree_of {
                let worktree_fut = self.environment.ensure_subagent_worktree(
                    task_id.as_deref().unwrap_or(""),
                    input.goal_id.clone(),
                    input.base_branch.clone(),
                    input.on_branch.clone(),
                    input.worktree_of.clone(),
                    &mut cx,
                );
                let worktree_res = futures::select_biased! {
                    _ = event_stream.cancelled_by_user().fuse() => {
                        drop(session_guard);
                        return Err(cancelled_error(
                            "worktree acquisition",
                            input.session_id.clone(),
                            None,
                            None,
                            None,
                        ));
                    }
                    res = worktree_fut.fuse() => res,
                };
                match worktree_res {
                    Ok(path) => (Some(path), Some(format!("guest:{host_id}"))),
                    Err(err) => {
                        let err_msg = err.to_string();
                        drop(session_guard);
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
            } else if wants_worktree {
                let task_id_ref = task_id.as_deref().unwrap_or("");
                let task_id_typed = crate::AgentTaskId::from(task_id_ref);
                crate::task_worktree::set_task_lsp_suppressed(&task_id_typed, !enable_lsp);
                let worktree_fut = self.environment.ensure_subagent_worktree(
                    task_id_ref,
                    input.goal_id.clone(),
                    input.base_branch.clone(),
                    input.on_branch.clone(),
                    None,
                    &mut cx,
                );
                let worktree_res = futures::select_biased! {
                    _ = event_stream.cancelled_by_user().fuse() => {
                        drop(session_guard);
                        return Err(cancelled_error(
                            "worktree acquisition",
                            input.session_id.clone(),
                            None,
                            None,
                            None,
                        ));
                    }
                    res = worktree_fut.fuse() => res,
                };
                match worktree_res {
                    Ok(path) => {
                        let isolation_str = if let Some(target) = crate::task_worktree::task_worktree_checkout_target(&task_id_typed) {
                            format!("checkout:{target}")
                        } else if let Some(ref on_b) = input.on_branch {
                            format!("checkout:{on_b}")
                        } else {
                            format!("worktree:agent-task/{task_id_ref}")
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
                            drop(session_guard);
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
                (
                    None,
                    if task_id.is_some() {
                        Some("shared:task-linked".to_string())
                    } else {
                        None
                    },
                )
            };

            if event_stream.was_cancelled_by_user() {
                drop(session_guard);
                return Err(cancelled_error(
                    "subagent creation",
                    input.session_id.clone(),
                    None,
                    isolation,
                    None,
                ));
            }

            let explicit_profile_str = input.profile.as_ref().map(|p| p.0.to_string());
            let explicit_model_str = input.model.clone();
            let is_initial_turn = input.session_id.is_none();

            let (subagent, mut session_info, resolved_model) = cx.update(|cx| {
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
                let subagent = if let Some(session_id) = input.session_id.clone() {
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
                let resolved_model = subagent.model_id(cx).or(explicit_model_str);
                let session_info = SubagentSessionInfo {
                    session_id: subagent.id(),
                    message_start_index: subagent.num_entries(cx),
                    message_end_index: None,
                };
                if is_initial_turn {
                    subagent.set_goal_id(input.goal_id.clone(), cx);
                }
                if let Some(ref task_id) = task_id {
                    subagent.set_task_id(Some(task_id.clone()), cx);
                    let task_id_typed = crate::AgentTaskId::from(task_id.clone());
                    if !is_guest {
                        if let Some(ref worktree_path) = worktree_path {
                            if crate::task_worktree::task_worktree_path_for_id(&task_id_typed).is_none() {
                                crate::task_worktree::register_task_worktree(
                                    &task_id_typed,
                                    worktree_path.clone(),
                                );
                            }
                        }
                    }
                    crate::task_worktree::register_task_session(
                        &task_id_typed,
                        session_info.session_id.to_string(),
                    );
                }

                event_stream.subagent_spawned(subagent.id());
                event_stream.update_fields_with_meta(
                    acp::ToolCallUpdateFields::new(),
                    Some(acp::Meta::from_iter([(
                        SUBAGENT_SESSION_INFO_META_KEY.into(),
                        serde_json::json!(&session_info),
                    )])),
                );

                Ok((subagent, session_info, resolved_model))
            })?;

            let _holder_guard = input.worktree_of.as_ref().map(|host| {
                WorktreeHolderGuard::new(
                    crate::AgentTaskId::from(host.clone()),
                    session_info.session_id.to_string(),
                )
            });

            if event_stream.was_cancelled_by_user() {
                drop(session_guard);
                return Err(cancelled_error(
                    "subagent creation",
                    Some(session_info.session_id.clone()),
                    Some(session_info),
                    isolation,
                    None,
                ));
            }

            if let Some(ref task_id) = task_id {
                let session_id_str = session_info.session_id.to_string();
                if should_register_metadata {
                    if event_stream.was_cancelled_by_user() {
                        drop(session_guard);
                        return Err(cancelled_error(
                            "task-graph metadata/status updates",
                            Some(session_info.session_id.clone()),
                            Some(session_info),
                            isolation,
                            None,
                        ));
                    }
                    let update_fut = self.environment.update_task_registered(
                        task_id,
                        resolved_model.as_deref(),
                        explicit_profile_str.as_deref(),
                        Some(&session_id_str),
                        &mut cx,
                    );
                    let step_fut = timeout_step(
                        Some(task_id),
                        "task_metadata_update",
                        step_timeouts.task_graph,
                        &cx,
                        update_fut,
                    );
                    let step_res = futures::select_biased! {
                        _ = event_stream.cancelled_by_user().fuse() => {
                            drop(session_guard);
                            return Err(cancelled_error(
                                "task-graph metadata/status updates",
                                Some(session_info.session_id.clone()),
                                Some(session_info),
                                isolation,
                                None,
                            ));
                        }
                        res = step_fut.fuse() => res,
                    };
                    match step_res {
                        StepOutcome::Ok(()) => {}
                        StepOutcome::Err(error) => {
                            log::warn!(
                                "failed to update task {task_id} metadata in task graph service: {error:#}"
                            );
                        }
                        StepOutcome::Timeout => {
                            log::warn!(
                                "task_metadata_update timed out after {}s for task {task_id}",
                                step_timeouts.task_graph.as_secs()
                            );
                        }
                    }
                }

                if should_set_task_status {
                    if event_stream.was_cancelled_by_user() {
                        drop(session_guard);
                        return Err(cancelled_error(
                            "task-graph metadata/status updates",
                            Some(session_info.session_id.clone()),
                            Some(session_info),
                            isolation,
                            None,
                        ));
                    }
                    let status_fut = self.environment.set_task_status_registered(
                        task_id,
                        crate::AgentTaskStatus::Running,
                        &mut cx,
                    );
                    let step_fut = timeout_step(
                        Some(task_id),
                        "status_running",
                        step_timeouts.task_graph,
                        &cx,
                        status_fut,
                    );
                    let step_res = futures::select_biased! {
                        _ = event_stream.cancelled_by_user().fuse() => {
                            drop(session_guard);
                            return Err(cancelled_error(
                                "task-graph metadata/status updates",
                                Some(session_info.session_id.clone()),
                                Some(session_info),
                                isolation,
                                None,
                            ));
                        }
                        res = step_fut.fuse() => res,
                    };
                    match step_res {
                        StepOutcome::Ok(()) => {}
                        StepOutcome::Err(error) => {
                            log::warn!(
                                "failed to set task {task_id} status to running in task graph service: {error:#}"
                            );
                        }
                        StepOutcome::Timeout => {
                            log::warn!(
                                "status_running timed out after {}s for task {task_id}",
                                step_timeouts.task_graph.as_secs()
                            );
                        }
                    }
                }
            }

            let session_id_str = session_info.session_id.to_string();
            let mut contract_block = None;

            if let Some(ref task_id) = task_id {
                if is_initial_turn {
                    if let Some(ref detail) = task_detail {
                        let (custom_template_path, worktree_root, templates) = cx.update(|cx| {
                            let settings = agent_settings::AgentSettings::get_global(cx);
                            let template_path = settings.task_contract_template.clone();
                            let worktree_root = self.environment.worktree_root_path(cx);
                            let templates = self
                                .environment
                                .templates(cx)
                                .unwrap_or_else(|| self.templates.clone());
                            (template_path, worktree_root, templates)
                        });
                        contract_block = build_task_contract(
                            task_id,
                            detail,
                            &templates,
                            custom_template_path.as_deref(),
                            worktree_root.as_deref(),
                        );
                    }
                }
            }

            let start_instant = std::time::Instant::now();
            let start_tokens = cx.update(|cx| subagent.cumulative_token_usage(cx));

            let message = if let Some(ref contract) = contract_block {
                format!("{contract}\n\n{message}", message = input.message)
            } else {
                input.message.clone()
            };

            let send_fut = subagent.send(message, cx);
            let (was_cancelled_mid_send, send_result) = futures::select_biased! {
                _ = event_stream.cancelled_by_user().fuse() => (true, None),
                res = send_fut.fuse() => (false, Some(res)),
            };

            if was_cancelled_mid_send {
                let error_message = "spawn_agent cancelled at subagent.send".to_string();
                session_info.message_end_index =
                    cx.update(|cx| Some(subagent.num_entries(cx).saturating_sub(1)));

                let isolation_details = if should_commit && !is_guest {
                    if let (Some(worktree_path), Some(task_id)) = (&worktree_path, &task_id) {
                        let commit_fut = self.environment.commit_subagent_worktree(
                            task_id,
                            worktree_path,
                            true,
                            &mut cx,
                        );
                        let details = match timeout_step(
                            Some(task_id),
                            "salvage_commit",
                            step_timeouts.commit,
                            &cx,
                            commit_fut,
                        )
                        .await
                        {
                            StepOutcome::Ok(details) => details,
                            StepOutcome::Err(e) => {
                                log::warn!("salvage commit failed for task {task_id}: {e:#}");
                                let task_id_typed = crate::AgentTaskId::from(task_id.clone());
                                crate::task_worktree::fallback_isolation_details(
                                    &task_id_typed,
                                    "worktree",
                                    Some(e.to_string()),
                                    None,
                                )
                            }
                            StepOutcome::Timeout => {
                                log::warn!(
                                    "salvage commit timed out after {}s for task {task_id}",
                                    step_timeouts.commit.as_secs()
                                );
                                let task_id_typed = crate::AgentTaskId::from(task_id.clone());
                                crate::task_worktree::fallback_isolation_details(
                                    &task_id_typed,
                                    "worktree",
                                    Some(format!(
                                        "commit timed out after {}s",
                                        step_timeouts.commit.as_secs()
                                    )),
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
                    }
                } else {
                    None
                };

                if !is_agent_mode {
                    if let Some(ref task_id) = task_id {
                        let fail_fut = self
                            .environment
                            .fail_task_registered(task_id, "cancelled", &mut cx);
                        match timeout_step(
                            Some(task_id),
                            "salvage_fail_task",
                            step_timeouts.task_graph,
                            &cx,
                            fail_fut,
                        )
                        .await
                        {
                            StepOutcome::Ok(()) => {}
                            StepOutcome::Err(failure_error) => {
                                log::warn!(
                                    "failed to fail task {task_id} in task graph service: {failure_error:#}"
                                );
                            }
                            StepOutcome::Timeout => {
                                log::warn!(
                                    "fail task timed out after {}s for task {task_id}",
                                    step_timeouts.task_graph.as_secs()
                                );
                            }
                        }
                    }
                }

                drop(session_guard);

                let meta = Some(acp::Meta::from_iter([(
                    SUBAGENT_SESSION_INFO_META_KEY.into(),
                    serde_json::json!(&session_info),
                )]));
                event_stream.update_fields_with_meta(
                    acp::ToolCallUpdateFields::new().content(vec![error_message.clone().into()]),
                    meta,
                );

                return Err(SpawnAgentToolOutput::Error {
                    session_id: Some(session_info.session_id.clone()),
                    error: error_message,
                    session_info: Some(session_info),
                    isolation,
                    isolation_details,
                });
            }

            let send_result = match send_result {
                Some(send_result) => send_result,
                None => {
                    return Err(cancelled_error(
                        "subagent.send",
                        Some(session_info.session_id.clone()),
                        Some(session_info),
                        isolation,
                        None,
                    ));
                }
            };

            let duration_ms = start_instant.elapsed().as_millis() as u64;
            let end_tokens = cx.update(|cx| subagent.cumulative_token_usage(cx));
            let delta_tokens = match (start_tokens, end_tokens) {
                (Some(start), Some(end)) => Some(language_model::TokenUsage {
                    input_tokens: end.input_tokens.saturating_sub(start.input_tokens),
                    output_tokens: end.output_tokens.saturating_sub(start.output_tokens),
                    cache_creation_input_tokens: end
                        .cache_creation_input_tokens
                        .saturating_sub(start.cache_creation_input_tokens),
                    cache_read_input_tokens: end
                        .cache_read_input_tokens
                        .saturating_sub(start.cache_read_input_tokens),
                }),
                (None, Some(end)) => Some(end),
                _ => None,
            };

            let turn_error_str = send_result.as_ref().err().map(|e| e.to_string());

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
                if !is_agent_mode {
                    if let Some(ref task_id) = task_id {
                        let error_message = error.to_string();
                        if crate::classify_send_error(&error_message) == crate::SendErrorKind::Definitive {
                            let fail_fut = self
                                .environment
                                .fail_task_registered(task_id, &error_message, &mut cx);
                            match timeout_step(
                                Some(task_id),
                                "status_failed",
                                step_timeouts.task_graph,
                                &cx,
                                fail_fut,
                            )
                            .await
                            {
                                StepOutcome::Ok(()) => {}
                                StepOutcome::Err(failure_error) => {
                                    log::warn!(
                                        "failed to auto-fail task {task_id} in task graph service: {failure_error:#}"
                                    );
                                }
                                StepOutcome::Timeout => {
                                    log::warn!(
                                        "status_failed timed out after {}s for task {task_id}",
                                        step_timeouts.task_graph.as_secs()
                                    );
                                }
                            }
                        }
                    }
                }
            }

            let is_error = send_result.is_err();
            if event_stream.was_cancelled_by_user() {
                drop(session_guard);
                return Err(cancelled_error(
                    "commit",
                    Some(session_info.session_id.clone()),
                    Some(session_info),
                    isolation,
                    None,
                ));
            }
            let isolation_details = if should_commit && !is_guest {
                if let (Some(worktree_path), Some(task_id)) = (&worktree_path, &task_id) {
                    let commit_fut = self
                        .environment
                        .commit_subagent_worktree(task_id, worktree_path, is_error, &mut cx);
                    let step_fut = timeout_step(
                        Some(task_id),
                        "commit",
                        step_timeouts.commit,
                        &cx,
                        commit_fut,
                    );
                    let step_res = futures::select_biased! {
                        _ = event_stream.cancelled_by_user().fuse() => {
                            drop(session_guard);
                            return Err(cancelled_error(
                                "commit",
                                Some(session_info.session_id.clone()),
                                Some(session_info),
                                isolation,
                                None,
                            ));
                        }
                        res = step_fut.fuse() => res,
                    };
                    let details = match step_res {
                        StepOutcome::Ok(details) => details,
                        StepOutcome::Err(e) => {
                            log::warn!("commit failed for task {task_id}: {e:#}");
                            let task_id_typed = crate::AgentTaskId::from(task_id.clone());
                            crate::task_worktree::fallback_isolation_details(
                                &task_id_typed,
                                "worktree",
                                Some(e.to_string()),
                                None,
                            )
                        }
                        StepOutcome::Timeout => {
                            log::warn!(
                                "commit timed out after {}s for task {task_id}",
                                step_timeouts.commit.as_secs()
                            );
                            let task_id_typed = crate::AgentTaskId::from(task_id.clone());
                            crate::task_worktree::fallback_isolation_details(
                                &task_id_typed,
                                "worktree",
                                Some(format!(
                                    "commit timed out after {}s",
                                    step_timeouts.commit.as_secs()
                                )),
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
                    if let Some(task_id) = task_id.as_deref() {
                        log_lifecycle_step(Some(task_id), "commit", 0, "skipped");
                    }
                    None
                }
            } else {
                if let Some(task_id) = task_id.as_deref() {
                    log_lifecycle_step(Some(task_id), "commit", 0, "skipped");
                }
                None
            };

            if event_stream.was_cancelled_by_user() {
                drop(session_guard);
                return Err(cancelled_error(
                    "turn record",
                    Some(session_info.session_id.clone()),
                    Some(session_info),
                    isolation,
                    isolation_details,
                ));
            }

            if let Some(ref task_id) = task_id {
                let branch_str = isolation_details.as_ref().map(|d| d.branch.as_str());
                let commit_sha_str = isolation_details.as_ref().and_then(|d| d.head_sha.as_deref());
                let output_summary = send_result
                    .as_ref()
                    .ok()
                    .map(|out| out.lines().take(20).collect::<Vec<_>>().join("\n"))
                    .map(SharedString::from);
                let diff = if is_guest {
                    None
                } else {
                    let task_id_typed = crate::AgentTaskId::from(task_id.clone());
                    let base_sha = crate::task_worktree::task_worktree_base_sha(&task_id_typed);
                    let worktree_path =
                        crate::task_worktree::task_worktree_path_for_id(&task_id_typed);
                    if let (Some(base_sha), Some(worktree_path)) = (base_sha, worktree_path) {
                        let timer = maybe_timer(cx.background_executor(), step_timeouts.diff).fuse();
                        futures::pin_mut!(timer);
                        let diff_fut = compute_turn_diff(&base_sha, &worktree_path).fuse();
                        futures::pin_mut!(diff_fut);
                        futures::select_biased! {
                            _ = event_stream.cancelled_by_user().fuse() => {
                                drop(session_guard);
                                return Err(cancelled_error(
                                    "turn record",
                                    Some(session_info.session_id.clone()),
                                    Some(session_info),
                                    isolation,
                                    isolation_details,
                                ));
                            }
                            diff = diff_fut => diff,
                            _ = timer => {
                                log::warn!(
                                    "compute_turn_diff timed out after {}s for task {task_id}",
                                    step_timeouts.diff.as_secs()
                                );
                                None
                            }
                        }
                    } else {
                        None
                    }
                };
                let record = crate::AgentTurnRecord {
                    duration_ms,
                    tokens: delta_tokens.or(end_tokens),
                    model: resolved_model.map(SharedString::from),
                    session_id: Some(session_id_str.into()),
                    error: turn_error_str.map(SharedString::from),
                    branch: branch_str.map(SharedString::from),
                    commit_sha: commit_sha_str.map(SharedString::from),
                    output_summary,
                    diff,
                };
                let record_fut = self.environment.record_turn(task_id, record, &mut cx);
                let step_fut = timeout_step(
                    Some(task_id),
                    "turn_record",
                    step_timeouts.task_graph,
                    &cx,
                    record_fut,
                );
                let step_res = futures::select_biased! {
                    _ = event_stream.cancelled_by_user().fuse() => {
                        drop(session_guard);
                        return Err(cancelled_error(
                            "turn record",
                            Some(session_info.session_id.clone()),
                            Some(session_info),
                            isolation,
                            isolation_details,
                        ));
                    }
                    res = step_fut.fuse() => res,
                };
                match step_res {
                    StepOutcome::Ok(_) => {}
                    StepOutcome::Err(err) => {
                        log::warn!("failed to record turn for task {task_id}: {err:#}");
                    }
                    StepOutcome::Timeout => {
                        log::warn!(
                            "turn_record timed out after {}s for task {task_id}",
                            step_timeouts.task_graph.as_secs()
                        );
                    }
                }
            }

            if send_result.is_ok() && should_set_task_status {
                if event_stream.was_cancelled_by_user() {
                    drop(session_guard);
                    return Err(cancelled_error(
                        "turn record",
                        Some(session_info.session_id.clone()),
                        Some(session_info),
                        isolation,
                        isolation_details,
                    ));
                }
                if let Some(ref task_id) = task_id {
                    let status_fut = self.environment.set_task_status_registered(
                        task_id,
                        crate::AgentTaskStatus::Completed,
                        &mut cx,
                    );
                    match timeout_step(
                        Some(task_id),
                        "status_completed",
                        step_timeouts.task_graph,
                        &cx,
                        status_fut,
                    )
                    .await
                    {
                        StepOutcome::Ok(()) => {
                            if !is_guest {
                                let task_id_typed = crate::AgentTaskId::from(task_id.clone());
                                crate::task_worktree::mark_task_terminal(&task_id_typed);
                            }
                        }
                        StepOutcome::Err(error) => {
                            log::warn!(
                                "failed to set task {task_id} status to completed in task graph service: {error:#}"
                            );
                        }
                        StepOutcome::Timeout => {
                            log::warn!(
                                "status_completed timed out after {}s for task {task_id}",
                                step_timeouts.task_graph.as_secs()
                            );
                        }
                    }
                }
            } else if let Some(ref task_id) = task_id {
                log_lifecycle_step(Some(task_id), "status_completed", 0, "skipped");
            }
            drop(session_guard);

            if event_stream.was_cancelled_by_user() {
                return Err(cancelled_error(
                    "cleanup",
                    Some(session_info.session_id.clone()),
                    Some(session_info),
                    isolation,
                    isolation_details,
                ));
            }

            if should_cleanup && !is_guest {
                if let Some(ref task_id) = task_id {
                    let task_id_typed = crate::AgentTaskId::from(task_id.clone());
                    if crate::task_worktree::task_worktree_commit_error(&task_id_typed).is_none()
                        && crate::task_worktree::task_worktree_merge_conflict(&task_id_typed).is_none()
                    {
                        self.environment.flush_diff_stats(&mut cx).await.log_err();
                        let cleanup_fut = self
                            .environment
                            .auto_cleanup_subagent_worktree(task_id, &mut cx);
                        let step_fut = timeout_step(
                            Some(task_id),
                            "cleanup",
                            step_timeouts.cleanup,
                            &cx,
                            cleanup_fut,
                        );
                        let step_res = futures::select_biased! {
                            _ = event_stream.cancelled_by_user().fuse() => {
                                return Err(cancelled_error(
                                    "cleanup",
                                    Some(session_info.session_id.clone()),
                                    Some(session_info),
                                    isolation,
                                    isolation_details,
                                ));
                            }
                            res = step_fut.fuse() => res,
                        };
                        match step_res {
                            StepOutcome::Ok(_) => {}
                            StepOutcome::Err(err) => {
                                log::error!("auto cleanup failed for subagent task {task_id}: {err:?}");
                            }
                            StepOutcome::Timeout => {
                                log::error!(
                                    "cleanup timed out after {}s for subagent task {task_id}",
                                    step_timeouts.cleanup.as_secs()
                                );
                            }
                        }
                    } else {
                        log_lifecycle_step(Some(task_id), "cleanup", 0, "skipped");
                    }
                }
            } else if let Some(ref task_id) = task_id {
                log_lifecycle_step(Some(task_id), "cleanup", 0, "skipped");
            }

            if event_stream.was_cancelled_by_user() {
                return Err(cancelled_error(
                    "cleanup",
                    Some(session_info.session_id.clone()),
                    Some(session_info),
                    isolation,
                    isolation_details,
                ));
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

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    let needle_bytes = needle.as_bytes();
    if needle_bytes.is_empty() || haystack.len() < needle_bytes.len() {
        return None;
    }
    haystack
        .as_bytes()
        .windows(needle_bytes.len())
        .position(|window| window.eq_ignore_ascii_case(needle_bytes))
        .map(|pos| (pos, pos + needle_bytes.len()))
}

fn extract_contract_parts(desc: &str) -> (String, Vec<String>) {
    let marker_pos = find_ascii_case_insensitive(desc, "strict constraints:")
        .or_else(|| find_ascii_case_insensitive(desc, "constraints:"));

    if let Some((start_pos, content_pos)) = marker_pos {
        let objective = desc[..start_pos].trim().to_string();
        let remainder = &desc[content_pos..];
        let mut constraints = Vec::new();
        for line in remainder.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with('#')
                || (trimmed.ends_with(':')
                    && !trimmed.starts_with('-')
                    && !trimmed.starts_with('*'))
            {
                break;
            }
            if let Some(item) = trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("* "))
            {
                constraints.push(item.trim().to_string());
            } else if let Some(dot_pos) = trimmed.find(". ") {
                if trimmed[..dot_pos].chars().all(|c| c.is_ascii_digit()) {
                    constraints.push(trimmed[dot_pos + 2..].trim().to_string());
                }
            } else {
                constraints.push(trimmed.to_string());
            }
        }
        (objective, constraints)
    } else {
        (desc.trim().to_string(), Vec::new())
    }
}

fn build_task_contract(
    task_id: &str,
    detail: &crate::AgentTaskDetail,
    templates: &crate::Templates,
    custom_template_path: Option<&str>,
    worktree_root: Option<&Path>,
) -> Option<String> {
    let has_description = !detail.description.trim().is_empty();
    let has_criteria = !detail.acceptance_criteria.is_empty();
    if !has_description && !has_criteria {
        return None;
    }

    let (objective_part, constraints) = extract_contract_parts(&detail.description);
    let objective = if !objective_part.is_empty() {
        objective_part
    } else if has_description {
        detail.description.trim().to_string()
    } else {
        detail.summary.title.clone()
    };

    let write_scopes = detail.summary.write_scopes.join(", ");
    let data = crate::TaskContractTemplate {
        task_id,
        title: &detail.summary.title,
        objective: &objective,
        acceptance_criteria: &detail.acceptance_criteria,
        constraints: &constraints,
        write_scopes: &write_scopes,
    };

    let render_default = || match data.render(templates) {
        Ok(rendered) => Some(rendered),
        Err(err) => {
            log::warn!("failed to render default task contract template: {err}");
            None
        }
    };

    if let Some(path) = custom_template_path.filter(|p| !p.trim().is_empty()) {
        match agent_settings::read_prompt_file(path, worktree_root) {
            Some(custom_template) => {
                match templates.render_custom_template(&custom_template, &data, worktree_root) {
                    Ok(rendered) => Some(rendered),
                    Err(err) => {
                        log::warn!(
                            "failed to render custom task contract template from {path}: {err}"
                        );
                        render_default()
                    }
                }
            }
            None => {
                log::warn!("failed to read custom task contract template file from {path}");
                render_default()
            }
        }
    } else {
        render_default()
    }
}

pub(crate) fn parse_git_shortstat(output: &str) -> Option<AgentDiffShortstat> {
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let mut files_changed: Option<u64> = None;
        let mut insertions: u64 = 0;
        let mut deletions: u64 = 0;
        let mut valid = true;

        for part in line.split(',') {
            let part = part.trim();
            let mut tokens = part.split_whitespace();
            let Some(number_str) = tokens.next() else {
                valid = false;
                break;
            };
            let Ok(number) = number_str.parse::<u64>() else {
                valid = false;
                break;
            };
            let Some(label) = tokens.next() else {
                valid = false;
                break;
            };

            if label.starts_with("file") {
                files_changed = Some(number);
            } else if label.starts_with("insertion") {
                insertions = number;
            } else if label.starts_with("deletion") {
                deletions = number;
            } else {
                valid = false;
                break;
            }
        }

        if valid {
            if let Some(files_changed) = files_changed {
                return Some(AgentDiffShortstat {
                    files_changed,
                    insertions,
                    deletions,
                });
            }
        }
    }

    None
}

async fn compute_turn_diff(
    base_sha: &str,
    worktree_path: &std::path::Path,
) -> Option<AgentDiffShortstat> {
    let git_binary = crate::task_worktree::system_git_binary()?;
    let mut command = util::command::new_command(git_binary);
    command.args(["diff", "--shortstat", base_sha]);
    command.current_dir(worktree_path);
    let output = command.output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_git_shortstat(&stdout)
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

    #[test]
    fn extract_contract_parts_handles_case_folding_unicode_safely() {
        let desc = "İstanbul İı objective.
Strict Constraints:
- Keep İ intact
1. No panics";
        let (objective, constraints) = extract_contract_parts(desc);
        assert_eq!(objective, "İstanbul İı objective.");
        assert_eq!(
            constraints,
            vec!["Keep İ intact".to_string(), "No panics".to_string()]
        );
    }

    #[test]
    fn test_parse_git_shortstat() {
        let full = " 3 files changed, 10 insertions(+), 5 deletions(-)";
        let result = parse_git_shortstat(full).unwrap();
        assert_eq!(
            result,
            AgentDiffShortstat {
                files_changed: 3,
                insertions: 10,
                deletions: 5,
            }
        );

        let missing_deletions = " 1 file changed, 2 insertions(+)";
        let result = parse_git_shortstat(missing_deletions).unwrap();
        assert_eq!(
            result,
            AgentDiffShortstat {
                files_changed: 1,
                insertions: 2,
                deletions: 0,
            }
        );

        let missing_insertions = " 2 files changed, 4 deletions(-)";
        let result = parse_git_shortstat(missing_insertions).unwrap();
        assert_eq!(
            result,
            AgentDiffShortstat {
                files_changed: 2,
                insertions: 0,
                deletions: 4,
            }
        );

        let files_only = " 1 file changed";
        let result = parse_git_shortstat(files_only).unwrap();
        assert_eq!(
            result,
            AgentDiffShortstat {
                files_changed: 1,
                insertions: 0,
                deletions: 0,
            }
        );

        assert_eq!(parse_git_shortstat(""), None);
        assert_eq!(parse_git_shortstat("   \n\t  "), None);
        assert_eq!(parse_git_shortstat("not a valid shortstat"), None);
        assert_eq!(parse_git_shortstat("fatal: bad revision 'HEAD'"), None);
        assert_eq!(parse_git_shortstat("something, else, here"), None);
    }

    #[gpui::test]
    async fn test_compute_turn_diff_real_git(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let Some(git_binary) = crate::task_worktree::system_git_binary() else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path();

        let init_status = util::command::new_command(&git_binary)
            .args(["init"])
            .current_dir(path)
            .output()
            .await
            .unwrap();
        assert!(init_status.status.success());

        util::command::new_command(&git_binary)
            .args(["config", "user.email", "test@test.com"])
            .current_dir(path)
            .output()
            .await
            .unwrap();
        util::command::new_command(&git_binary)
            .args(["config", "user.name", "Test"])
            .current_dir(path)
            .output()
            .await
            .unwrap();

        std::fs::write(path.join("file1.txt"), "hello\n").unwrap();
        util::command::new_command(&git_binary)
            .args(["add", "."])
            .current_dir(path)
            .output()
            .await
            .unwrap();
        util::command::new_command(&git_binary)
            .args(["commit", "-m", "initial"])
            .current_dir(path)
            .output()
            .await
            .unwrap();

        let base_sha_output = util::command::new_command(&git_binary)
            .args(["rev-parse", "HEAD"])
            .current_dir(path)
            .output()
            .await
            .unwrap();
        let base_sha = String::from_utf8_lossy(&base_sha_output.stdout)
            .trim()
            .to_string();

        let diff_none = compute_turn_diff(&base_sha, path).await;
        assert_eq!(diff_none, None);

        std::fs::write(path.join("file1.txt"), "hello\nworld\nagain\n").unwrap();
        std::fs::write(path.join("file2.txt"), "new file\n").unwrap();
        util::command::new_command(&git_binary)
            .args(["add", "."])
            .current_dir(path)
            .output()
            .await
            .unwrap();
        util::command::new_command(&git_binary)
            .args(["commit", "-m", "second"])
            .current_dir(path)
            .output()
            .await
            .unwrap();

        let diff_some = compute_turn_diff(&base_sha, path).await;
        assert_eq!(
            diff_some,
            Some(AgentDiffShortstat {
                files_changed: 2,
                insertions: 3,
                deletions: 0,
            })
        );

        let bad_sha = compute_turn_diff("nonexistent_sha", path).await;
        assert_eq!(bad_sha, None);

        let bad_path = compute_turn_diff(&base_sha, std::path::Path::new("/nonexistent")).await;
        assert_eq!(bad_path, None);
    }

    #[gpui::test]
    async fn test_timeout_step_outcomes(cx: &mut gpui::TestAppContext) {
        let async_cx = cx.to_async();

        // 1. Success
        let outcome = timeout_step(
            Some("TASK-TEST"),
            "test_step_success",
            Duration::from_secs(5),
            &async_cx,
            async { Ok("success_val") },
        )
        .await;
        assert!(matches!(outcome, StepOutcome::Ok("success_val")));

        // 2. Error
        let outcome: StepOutcome<&str> = timeout_step(
            Some("TASK-TEST"),
            "test_step_error",
            Duration::from_secs(5),
            &async_cx,
            async { Err(anyhow::anyhow!("fail")) },
        )
        .await;
        assert!(matches!(outcome, StepOutcome::Err(_)));

        // 3. Timeout
        let timeout_task = cx.spawn(|cx| async move {
            timeout_step(
                Some("TASK-TEST"),
                "test_step_timeout",
                Duration::from_millis(50),
                &cx,
                async {
                    cx.background_executor()
                        .timer(Duration::from_millis(500))
                        .await;
                    Ok("late")
                },
            )
            .await
        });

        cx.executor().advance_clock(Duration::from_millis(100));
        let outcome = timeout_task.await;
        assert!(matches!(outcome, StepOutcome::Timeout));

        // 4. Timeout Duration::ZERO (disabled timer, waits for future without timing out)
        let disabled_timeout_task = cx.spawn(|cx| async move {
            timeout_step(
                Some("TASK-TEST"),
                "test_step_zero_timeout",
                Duration::ZERO,
                &cx,
                async {
                    cx.background_executor()
                        .timer(Duration::from_millis(500))
                        .await;
                    Ok("zero_timeout_ok")
                },
            )
            .await
        });

        cx.executor().advance_clock(Duration::from_millis(600));
        let outcome = disabled_timeout_task.await;
        assert!(matches!(outcome, StepOutcome::Ok("zero_timeout_ok")));

        // 5. maybe_timer helper returns Left for zero timeout, Right for non-zero
        let zero_timer = maybe_timer(&cx.background_executor, Duration::ZERO);
        assert!(matches!(zero_timer, futures::future::Either::Left(_)));
        let non_zero_timer = maybe_timer(&cx.background_executor, Duration::from_secs(1));
        assert!(matches!(non_zero_timer, futures::future::Either::Right(_)));
    }

    struct TestSubagentHandle {
        session_id: acp::SessionId,
        send_started_tx: Arc<parking_lot::Mutex<Option<futures::channel::oneshot::Sender<()>>>>,
    }

    impl crate::thread::SubagentHandle for TestSubagentHandle {
        fn id(&self) -> acp::SessionId {
            self.session_id.clone()
        }

        fn num_entries(&self, _cx: &App) -> usize {
            1
        }

        fn send(&self, _message: String, cx: &gpui::AsyncApp) -> Task<Result<String>> {
            if let Some(tx) = self.send_started_tx.lock().take() {
                tx.send(()).ok();
            }
            cx.spawn(async move |_| futures::future::pending::<Result<String>>().await)
        }
    }

    struct CancelMockEnvironment {
        send_started_tx: Arc<parking_lot::Mutex<Option<futures::channel::oneshot::Sender<()>>>>,
        slot_wait_started_tx:
            Arc<parking_lot::Mutex<Option<futures::channel::oneshot::Sender<()>>>>,
        hang_slot_wait: bool,
        commit_called: Arc<std::sync::atomic::AtomicBool>,
        commit_is_error: Arc<std::sync::atomic::AtomicBool>,
        cleanup_called: Arc<std::sync::atomic::AtomicBool>,
        failed_task_reason: Arc<parking_lot::Mutex<Option<String>>>,
    }

    impl crate::thread::ThreadEnvironment for CancelMockEnvironment {
        fn create_terminal(
            &self,
            _command: String,
            _extra_env: Vec<acp::EnvVariable>,
            _cwd: Option<std::path::PathBuf>,
            _output_byte_limit: Option<u64>,
            _sandbox_wrap: Option<acp_thread::SandboxWrap>,
            _cx: &mut gpui::AsyncApp,
        ) -> Task<Result<Rc<dyn crate::thread::TerminalHandle>>> {
            Task::ready(Err(anyhow::anyhow!("not implemented")))
        }

        fn create_subagent(
            &self,
            _label: String,
            _profile: Option<agent_settings::AgentProfileId>,
            _model: Option<crate::AgentModelId>,
            _task_worktree: Option<std::path::PathBuf>,
            _cx: &mut App,
        ) -> Result<Rc<dyn crate::thread::SubagentHandle>> {
            Ok(Rc::new(TestSubagentHandle {
                session_id: acp::SessionId::new("mock-subagent-session"),
                send_started_tx: self.send_started_tx.clone(),
            }))
        }

        fn ensure_subagent_worktree(
            &self,
            _task_id: &str,
            _goal_id: Option<String>,
            _base_branch: Option<String>,
            _on_branch: Option<String>,
            _worktree_of: Option<String>,
            cx: &mut gpui::AsyncApp,
        ) -> Task<Result<std::path::PathBuf>> {
            if self.hang_slot_wait {
                if let Some(tx) = self.slot_wait_started_tx.lock().take() {
                    tx.send(()).ok();
                }
                return cx.spawn(async move |_| {
                    futures::future::pending::<Result<std::path::PathBuf>>().await
                });
            }
            Task::ready(Ok(std::path::PathBuf::from("/mock/worktree")))
        }

        fn get_task_registered(
            &self,
            task_id: &str,
            _cx: &mut gpui::AsyncApp,
        ) -> Task<Result<crate::AgentTaskDetail>> {
            Task::ready(Ok(crate::AgentTaskDetail {
                summary: crate::AgentTaskSummary {
                    id: crate::AgentTaskId::from(task_id),
                    title: task_id.to_string(),
                    status: crate::AgentTaskStatus::Ready,
                    attempt: 1,
                    ..Default::default()
                },
                description: String::new(),
                acceptance_criteria: Vec::new(),
                events_tail: Vec::new(),
            }))
        }

        fn commit_subagent_worktree(
            &self,
            task_id: &str,
            _task_worktree: &std::path::Path,
            is_error: bool,
            _cx: &mut gpui::AsyncApp,
        ) -> Task<Result<crate::task_worktree::SubagentIsolationDetails>> {
            self.commit_called
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.commit_is_error
                .store(is_error, std::sync::atomic::Ordering::SeqCst);
            let task_id_typed = crate::AgentTaskId::from(task_id);
            Task::ready(Ok(crate::task_worktree::fallback_isolation_details(
                &task_id_typed,
                "worktree",
                None,
                None,
            )))
        }

        fn fail_task_registered(
            &self,
            _task_id: &str,
            reason: &str,
            _cx: &mut gpui::AsyncApp,
        ) -> Task<Result<()>> {
            *self.failed_task_reason.lock() = Some(reason.to_string());
            Task::ready(Ok(()))
        }

        fn auto_cleanup_subagent_worktree(
            &self,
            _task_id: &str,
            _cx: &mut gpui::AsyncApp,
        ) -> Task<Result<bool>> {
            self.cleanup_called
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Task::ready(Ok(true))
        }

        fn update_task_registered(
            &self,
            _task_id: &str,
            _model: Option<&str>,
            _assignee: Option<&str>,
            _session_id: Option<&str>,
            _cx: &mut gpui::AsyncApp,
        ) -> Task<Result<()>> {
            Task::ready(Ok(()))
        }

        fn set_task_status_registered(
            &self,
            _task_id: &str,
            _status: crate::AgentTaskStatus,
            _cx: &mut gpui::AsyncApp,
        ) -> Task<Result<()>> {
            Task::ready(Ok(()))
        }
    }

    #[gpui::test]
    async fn test_cancel_during_send_skips_commit_and_cleanup_and_releases_guard(
        cx: &mut gpui::TestAppContext,
    ) {
        crate::task_worktree::reset_registry_for_tests();
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
        });

        let (send_started_tx, send_started_rx) = futures::channel::oneshot::channel();
        let env = Rc::new(CancelMockEnvironment {
            send_started_tx: Arc::new(parking_lot::Mutex::new(Some(send_started_tx))),
            slot_wait_started_tx: Arc::new(parking_lot::Mutex::new(None)),
            hang_slot_wait: false,
            commit_called: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            commit_is_error: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cleanup_called: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            failed_task_reason: Arc::new(parking_lot::Mutex::new(None)),
        });

        #[allow(clippy::arc_with_non_send_sync)]
        let tool = Arc::new(SpawnAgentTool::new(env.clone()));
        let task_id = "TASK-CANCEL-SEND".to_string();
        let task_id_typed = crate::AgentTaskId::from(task_id.clone());

        let (event_stream, _rx, mut cancellation_tx) =
            crate::thread::ToolCallEventStream::test_with_cancellation();

        let run_task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(SpawnAgentToolInput {
                    label: "subtask".to_string(),
                    message: "do something".to_string(),
                    task_id: Some(task_id.clone()),
                    base_branch: Some("main".to_string()),
                    ..Default::default()
                }),
                event_stream,
                cx,
            )
        });

        send_started_rx.await.unwrap();

        assert!(
            crate::task_worktree::has_active_subagent_session(&task_id_typed),
            "session guard must be held while subagent.send is running"
        );

        crate::thread::ToolCallEventStream::signal_cancellation_with_sender(&mut cancellation_tx);

        let result = run_task.await;
        let Err(SpawnAgentToolOutput::Error {
            error,
            session_id,
            session_info,
            ..
        }) = result
        else {
            panic!("expected Err(SpawnAgentToolOutput::Error), got {result:?}");
        };

        assert!(
            error.contains("spawn_agent cancelled at subagent.send"),
            "expected cancel error message to name subagent.send stage, got: {error}"
        );
        assert_eq!(
            session_id.as_ref().map(|s| s.to_string()),
            Some("mock-subagent-session".to_string())
        );
        assert!(session_info.is_some(), "session_info must be preserved");

        assert!(
            env.commit_called.load(std::sync::atomic::Ordering::SeqCst),
            "salvage commit must run on mid-send cancel"
        );
        assert!(
            env.commit_is_error
                .load(std::sync::atomic::Ordering::SeqCst),
            "salvage commit must be error-checkpoint (is_error = true)"
        );
        assert_eq!(
            env.failed_task_reason.lock().as_deref(),
            Some("cancelled"),
            "task must be failed in taskgraph with reason cancelled"
        );
        assert!(
            !env.cleanup_called.load(std::sync::atomic::Ordering::SeqCst),
            "cleanup must be skipped on cancel"
        );
        assert!(
            !crate::task_worktree::has_active_subagent_session(&task_id_typed),
            "session guard must be released after cancel"
        );
    }

    #[gpui::test]
    async fn test_cancel_during_slot_wait_leaves_no_holder(cx: &mut gpui::TestAppContext) {
        crate::task_worktree::reset_registry_for_tests();
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
        });

        let (slot_wait_started_tx, slot_wait_started_rx) = futures::channel::oneshot::channel();
        let env = Rc::new(CancelMockEnvironment {
            send_started_tx: Arc::new(parking_lot::Mutex::new(None)),
            slot_wait_started_tx: Arc::new(parking_lot::Mutex::new(Some(slot_wait_started_tx))),
            hang_slot_wait: true,
            commit_called: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            commit_is_error: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cleanup_called: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            failed_task_reason: Arc::new(parking_lot::Mutex::new(None)),
        });

        #[allow(clippy::arc_with_non_send_sync)]
        let tool = Arc::new(SpawnAgentTool::new(env.clone()));
        let task_id = "TASK-CANCEL-SLOT".to_string();
        let task_id_typed = crate::AgentTaskId::from(task_id.clone());

        let (event_stream, _rx, mut cancellation_tx) =
            crate::thread::ToolCallEventStream::test_with_cancellation();

        let run_task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(SpawnAgentToolInput {
                    label: "subtask".to_string(),
                    message: "do something".to_string(),
                    task_id: Some(task_id.clone()),
                    base_branch: Some("main".to_string()),
                    ..Default::default()
                }),
                event_stream,
                cx,
            )
        });

        slot_wait_started_rx.await.unwrap();

        assert!(
            crate::task_worktree::has_active_subagent_session(&task_id_typed),
            "session guard must be held during worktree slot wait"
        );

        crate::thread::ToolCallEventStream::signal_cancellation_with_sender(&mut cancellation_tx);

        let result = run_task.await;
        let Err(SpawnAgentToolOutput::Error { error, .. }) = result else {
            panic!("expected Err(SpawnAgentToolOutput::Error), got {result:?}");
        };

        assert!(
            error.contains("spawn_agent cancelled at worktree acquisition"),
            "expected error to name worktree acquisition, got: {error}"
        );
        assert!(
            !crate::task_worktree::has_active_subagent_session(&task_id_typed),
            "session guard must be released so slot wait leaves no holder"
        );
    }
}
