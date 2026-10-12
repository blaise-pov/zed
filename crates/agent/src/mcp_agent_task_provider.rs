use anyhow::{Context as _, Result};
use context_server::ContextServerId;
use gpui::{App, Entity, Task};
use project::context_server_store::ContextServerStore;

use crate::agent_task::{
    AgentGoalStatus, AgentTaskArtifact, AgentTaskDetail, AgentTaskEvent, AgentTaskGraph,
    AgentTaskId, AgentTaskProvider, AgentTaskStatus, AgentUnifiedItem,
};

pub struct McpAgentTaskProvider {
    store: Entity<ContextServerStore>,
    server_id: ContextServerId,
}

impl McpAgentTaskProvider {
    pub fn new(store: Entity<ContextServerStore>, server_id: ContextServerId) -> Self {
        Self { store, server_id }
    }

    fn call_tool(
        &self,
        tool_name: &'static str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
        cx: &mut App,
    ) -> Task<Result<serde_json::Value>> {
        let Some(server) = self.store.read(cx).get_running_server(&self.server_id) else {
            return Task::ready(Err(anyhow::anyhow!(
                "Task server '{}' is not running",
                self.server_id
            )));
        };

        let server_id = self.server_id.clone();
        cx.spawn(async move |_cx| {
            let Some(client) = server.client() else {
                anyhow::bail!("Task server '{server_id}' is not initialized");
            };

            let response = client
                .request::<context_server::types::requests::CallTool>(
                    context_server::types::CallToolParams {
                        name: tool_name.into(),
                        arguments: arguments.map(serde_json::Value::Object),
                        meta: None,
                    },
                )
                .await?;

            if response.is_error == Some(true) {
                let error_message: String = response
                    .content
                    .iter()
                    .filter_map(|content| content.text())
                    .collect();
                anyhow::bail!("MCP tool '{tool_name}' returned error: {error_message}");
            }

            let text_content: String = response
                .content
                .iter()
                .filter_map(|content| content.text())
                .collect();

            if text_content.is_empty() {
                Ok(serde_json::Value::Null)
            } else {
                let parsed: serde_json::Value = serde_json::from_str(&text_content)
                    .unwrap_or_else(|_| serde_json::Value::String(text_content));
                Ok(parsed)
            }
        })
    }
}

impl AgentTaskProvider for McpAgentTaskProvider {
    fn server_id(&self) -> ContextServerId {
        self.server_id.clone()
    }

    fn fetch_graph(&self, cx: &mut App) -> Task<Result<AgentTaskGraph>> {
        let task = self.call_tool("task_graph", None, cx);
        cx.spawn(async move |_cx| {
            let value = task.await?;
            let graph: AgentTaskGraph = serde_json::from_value(value)
                .context("failed to parse AgentTaskGraph from task_graph response")?;
            Ok(graph)
        })
    }

    fn get_task(&self, id: &AgentTaskId, cx: &mut App) -> Task<Result<AgentTaskDetail>> {
        let mut args = serde_json::Map::new();
        args.insert(
            "task_id".to_string(),
            serde_json::Value::String(id.to_string()),
        );
        let task = self.call_tool("task_get", Some(args), cx);
        cx.spawn(async move |_cx| {
            let value = task.await?;
            let detail: AgentTaskDetail = serde_json::from_value(value)
                .context("failed to parse AgentTaskDetail from task_get response")?;
            Ok(detail)
        })
    }

    fn ensure_task(
        &self,
        id: &AgentTaskId,
        title: &str,
        goal_id: Option<&str>,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let mut get_arguments = serde_json::Map::new();
        get_arguments.insert(
            "task_id".to_string(),
            serde_json::Value::String(id.to_string()),
        );
        let get_task = self.call_tool("task_get", Some(get_arguments), cx);

        let mut create_arguments = serde_json::Map::new();
        let title_string = if title.trim().is_empty() {
            id.to_string()
        } else {
            title.to_string()
        };
        create_arguments.insert("title".to_string(), serde_json::Value::String(title_string));
        create_arguments.insert(
            "task_id".to_string(),
            serde_json::Value::String(id.to_string()),
        );
        create_arguments.insert(
            "idempotency_key".to_string(),
            serde_json::Value::String(id.to_string()),
        );
        if let Some(goal_id) = goal_id {
            create_arguments.insert(
                "goal_id".to_string(),
                serde_json::Value::String(goal_id.to_string()),
            );
        }

        let store = self.store.clone();
        let server_id = self.server_id.clone();

        cx.spawn(async move |cx| {
            if get_task.await.is_ok() {
                return Ok(());
            }

            let create_task = cx.update(|cx| {
                let provider = McpAgentTaskProvider::new(store, server_id);
                provider.call_tool("task_create", Some(create_arguments), cx)
            });
            create_task.await?;
            Ok(())
        })
    }

    fn update_task_metadata(
        &self,
        id: &AgentTaskId,
        model: Option<&str>,
        assignee: Option<&str>,
        session_id: Option<&str>,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let mut args = serde_json::Map::new();
        args.insert(
            "task_id".to_string(),
            serde_json::Value::String(id.to_string()),
        );
        if let Some(m) = model.map(str::trim).filter(|s| !s.is_empty()) {
            args.insert(
                "model".to_string(),
                serde_json::Value::String(m.to_string()),
            );
            args.insert(
                "assigned_model".to_string(),
                serde_json::Value::String(m.to_string()),
            );
        }
        if let Some(a) = assignee.map(str::trim).filter(|s| !s.is_empty()) {
            args.insert(
                "assignee".to_string(),
                serde_json::Value::String(a.to_string()),
            );
        }
        let has_session_id = if let Some(s) = session_id.map(str::trim).filter(|s| !s.is_empty()) {
            args.insert(
                "session_id".to_string(),
                serde_json::Value::String(s.to_string()),
            );
            true
        } else {
            false
        };

        if args.len() <= 1 {
            return Task::ready(Ok(()));
        }

        let update_task = self.call_tool("task_update", Some(args.clone()), cx);
        let store = self.store.clone();
        let server_id = self.server_id.clone();

        cx.spawn(async move |cx| match update_task.await {
            Ok(_) => Ok(()),
            Err(err) => {
                if has_session_id {
                    args.remove("session_id");
                    if args.len() <= 1 {
                        return Ok(());
                    }
                    let fallback_task = cx.update(|cx| {
                        let provider = McpAgentTaskProvider::new(store, server_id);
                        provider.call_tool("task_update", Some(args), cx)
                    });
                    fallback_task.await?;
                    Ok(())
                } else {
                    Err(err)
                }
            }
        })
    }

    fn complete_task(&self, id: &AgentTaskId, cx: &mut App) -> Task<Result<()>> {
        let mut args = serde_json::Map::new();
        args.insert(
            "task_id".to_string(),
            serde_json::Value::String(id.to_string()),
        );
        let task = self.call_tool("task_complete", Some(args), cx);
        cx.spawn(async move |_cx| {
            task.await?;
            Ok(())
        })
    }

    fn fail_task(&self, id: &AgentTaskId, reason: &str, cx: &mut App) -> Task<Result<()>> {
        let mut args = serde_json::Map::new();
        args.insert(
            "task_id".to_string(),
            serde_json::Value::String(id.to_string()),
        );
        args.insert(
            "reason".to_string(),
            serde_json::Value::String(reason.to_string()),
        );
        let task = self.call_tool("task_fail", Some(args), cx);
        cx.spawn(async move |_cx| {
            task.await?;
            Ok(())
        })
    }

    fn archive_task(&self, id: &AgentTaskId, cx: &mut App) -> Task<Result<()>> {
        let mut args = serde_json::Map::new();
        args.insert(
            "task_id".to_string(),
            serde_json::Value::String(id.to_string()),
        );
        let task = self.call_tool("task_archive", Some(args), cx);
        cx.spawn(async move |_cx| {
            task.await?;
            Ok(())
        })
    }

    fn unarchive_task(&self, id: &AgentTaskId, cx: &mut App) -> Task<Result<()>> {
        let mut args = serde_json::Map::new();
        args.insert(
            "task_id".to_string(),
            serde_json::Value::String(id.to_string()),
        );
        let task = self.call_tool("task_unarchive", Some(args), cx);
        cx.spawn(async move |_cx| {
            task.await?;
            Ok(())
        })
    }

    fn delete_task(&self, id: &AgentTaskId, cx: &mut App) -> Task<Result<()>> {
        let mut args = serde_json::Map::new();
        args.insert(
            "task_id".to_string(),
            serde_json::Value::String(id.to_string()),
        );
        let task = self.call_tool("task_delete", Some(args), cx);
        cx.spawn(async move |_cx| {
            task.await?;
            Ok(())
        })
    }

    fn list_events(&self, limit: u32, cx: &mut App) -> Task<Result<Vec<AgentTaskEvent>>> {
        let mut args = serde_json::Map::new();
        args.insert("limit".to_string(), serde_json::Value::Number(limit.into()));
        let task = self.call_tool("events_list", Some(args), cx);
        cx.spawn(async move |_cx| {
            let value = task.await?;
            let events: Vec<AgentTaskEvent> = serde_json::from_value(value)
                .context("failed to parse Vec<AgentTaskEvent> from events_list response")?;
            Ok(events)
        })
    }

    fn list_artifacts(
        &self,
        task_id: &AgentTaskId,
        cx: &mut App,
    ) -> Task<Result<Vec<AgentTaskArtifact>>> {
        let mut args = serde_json::Map::new();
        args.insert(
            "task_id".to_string(),
            serde_json::Value::String(task_id.to_string()),
        );
        let task = self.call_tool("artifact_list", Some(args), cx);
        cx.spawn(async move |_cx| {
            let value = task.await?;
            let artifacts: Vec<AgentTaskArtifact> = serde_json::from_value(value)
                .context("failed to parse Vec<AgentTaskArtifact> from artifact_list response")?;
            Ok(artifacts)
        })
    }

    fn get_artifact(&self, artifact_id: &str, cx: &mut App) -> Task<Result<AgentTaskArtifact>> {
        let mut args = serde_json::Map::new();
        args.insert(
            "artifact_id".to_string(),
            serde_json::Value::String(artifact_id.to_string()),
        );
        let task = self.call_tool("artifact_get", Some(args), cx);
        cx.spawn(async move |_cx| {
            let value = task.await?;
            let artifact: AgentTaskArtifact = serde_json::from_value(value)
                .context("failed to parse AgentTaskArtifact from artifact_get response")?;
            Ok(artifact)
        })
    }

    fn record_turn_start(
        &self,
        task_id: &AgentTaskId,
        prompt: &str,
        model: Option<&str>,
        session_id: Option<&str>,
        cx: &mut App,
    ) -> Task<Result<Option<u32>>> {
        let mut args = serde_json::Map::new();
        args.insert(
            "task_id".to_string(),
            serde_json::Value::String(task_id.to_string()),
        );
        args.insert(
            "prompt".to_string(),
            serde_json::Value::String(prompt.to_string()),
        );
        if let Some(m) = model.map(str::trim).filter(|s| !s.is_empty()) {
            args.insert(
                "model".to_string(),
                serde_json::Value::String(m.to_string()),
            );
        }
        if let Some(s) = session_id.map(str::trim).filter(|s| !s.is_empty()) {
            args.insert(
                "session_id".to_string(),
                serde_json::Value::String(s.to_string()),
            );
        }
        let task = self.call_tool("task_turn_start", Some(args), cx);
        cx.spawn(async move |_cx| {
            let value = task.await?;
            let turn_index = if let Some(n) = value.as_u64() {
                Some(n as u32)
            } else if let Some(obj) = value.as_object() {
                obj.get("turn_index")
                    .or_else(|| obj.get("turn"))
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32)
            } else {
                None
            };
            Ok(turn_index)
        })
    }

    fn record_turn_finish(
        &self,
        task_id: &AgentTaskId,
        turn_index: Option<u32>,
        duration_ms: u64,
        tokens: Option<language_model::TokenUsage>,
        error: Option<&str>,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let mut args = serde_json::Map::new();
        args.insert(
            "task_id".to_string(),
            serde_json::Value::String(task_id.to_string()),
        );
        if let Some(turn_index) = turn_index {
            args.insert(
                "turn_index".to_string(),
                serde_json::Value::Number(turn_index.into()),
            );
        }
        args.insert(
            "duration_ms".to_string(),
            serde_json::Value::Number(duration_ms.into()),
        );
        if let Some(tokens) = tokens {
            let tokens_obj = serde_json::json!({
                "input": tokens.input_tokens,
                "output": tokens.output_tokens,
                "cache_read": tokens.cache_read_input_tokens,
                "cache_creation": tokens.cache_creation_input_tokens,
            });
            args.insert("tokens".to_string(), tokens_obj);
        }
        if let Some(error) = error.map(str::trim).filter(|s| !s.is_empty()) {
            args.insert(
                "error".to_string(),
                serde_json::Value::String(error.to_string()),
            );
        }
        let task = self.call_tool("task_turn_finish", Some(args), cx);
        cx.spawn(async move |_cx| {
            task.await?;
            Ok(())
        })
    }

    fn publish_artifact(
        &self,
        task_id: &AgentTaskId,
        kind: &str,
        content: &str,
        summary: Option<&str>,
        cx: &mut App,
    ) -> Task<Result<String>> {
        let mut args = serde_json::Map::new();
        args.insert(
            "task_id".to_string(),
            serde_json::Value::String(task_id.to_string()),
        );
        args.insert(
            "kind".to_string(),
            serde_json::Value::String(kind.to_string()),
        );
        args.insert(
            "content".to_string(),
            serde_json::Value::String(content.to_string()),
        );
        if let Some(summary) = summary.map(str::trim).filter(|s| !s.is_empty()) {
            args.insert(
                "summary".to_string(),
                serde_json::Value::String(summary.to_string()),
            );
        }
        let task = self.call_tool("artifact_publish", Some(args), cx);
        cx.spawn(async move |_cx| {
            let value = task.await?;
            let artifact_id = if let Some(s) = value.as_str() {
                s.to_string()
            } else if let Some(obj) = value.as_object() {
                obj.get("id")
                    .or_else(|| obj.get("artifact_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            } else {
                String::new()
            };
            Ok(artifact_id)
        })
    }

    fn fetch_items(&self, cx: &mut App) -> Task<Result<Vec<AgentUnifiedItem>>> {
        let task = self.call_tool("item_list", Some(serde_json::Map::new()), cx);
        cx.spawn(async move |_cx| {
            let value = task.await?;
            let items_value = match value {
                serde_json::Value::Array(_) => value,
                serde_json::Value::Object(mut map) => map
                    .remove("items")
                    .unwrap_or(serde_json::Value::Array(Vec::new())),
                _ => serde_json::Value::Array(Vec::new()),
            };
            let mut items: Vec<AgentUnifiedItem> = serde_json::from_value(items_value)
                .context("failed to parse Vec<AgentUnifiedItem> from item_list response")?;
            for item in &mut items {
                if item.metrics.is_none() {
                    if let Some(metadata) = &item.metadata {
                        if let Some(m) = metadata.get("metrics") {
                            if let Ok(parsed) = serde_json::from_value(m.clone()) {
                                item.metrics = Some(parsed);
                            }
                        }
                    }
                }
            }
            Ok(items)
        })
    }

    fn set_task_status(
        &self,
        id: &AgentTaskId,
        status: &AgentTaskStatus,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let (tool_name, extra_args) = match status {
            AgentTaskStatus::Ready => ("task_retry", None),
            AgentTaskStatus::Running => ("task_start", None),
            AgentTaskStatus::Completed => ("task_complete", None),
            AgentTaskStatus::Failed => (
                "task_fail",
                Some(("reason", "marked failed from Agent Task Panel".to_string())),
            ),
            AgentTaskStatus::Cancelled => ("task_cancel", None),
            AgentTaskStatus::Archived => ("task_archive", None),
            other => {
                return Task::ready(Err(anyhow::anyhow!(
                    "cannot set task status {:?} via MCP",
                    other
                )));
            }
        };

        let mut args = serde_json::Map::new();
        args.insert(
            "task_id".to_string(),
            serde_json::Value::String(id.to_string()),
        );
        if let Some((key, val)) = extra_args {
            args.insert(key.to_string(), serde_json::Value::String(val));
        }

        let is_running = *status == AgentTaskStatus::Running;
        let task_id_str = id.to_string();
        let store = self.store.clone();
        let server_id = self.server_id.clone();

        let task = self.call_tool(tool_name, Some(args), cx);
        cx.spawn(async move |cx| match task.await {
            Ok(_) => Ok(()),
            Err(first_err) if is_running => {
                let mut retry_args = serde_json::Map::new();
                retry_args.insert(
                    "task_id".to_string(),
                    serde_json::Value::String(task_id_str.clone()),
                );
                let retry_task = cx.update(|cx| {
                    let provider = McpAgentTaskProvider::new(store.clone(), server_id.clone());
                    provider.call_tool("task_retry", Some(retry_args.clone()), cx)
                });
                if retry_task.await.is_ok() {
                    let start_again_task = cx.update(|cx| {
                        let provider = McpAgentTaskProvider::new(store.clone(), server_id.clone());
                        provider.call_tool("task_start", Some(retry_args), cx)
                    });
                    if start_again_task.await.is_ok() {
                        return Ok(());
                    }
                }

                let mut update_args = serde_json::Map::new();
                update_args.insert(
                    "task_id".to_string(),
                    serde_json::Value::String(task_id_str),
                );
                update_args.insert(
                    "status".to_string(),
                    serde_json::Value::String("running".to_string()),
                );
                let update_task = cx.update(|cx| {
                    let provider = McpAgentTaskProvider::new(store, server_id);
                    provider.call_tool("task_update", Some(update_args), cx)
                });
                if update_task.await.is_ok() {
                    Ok(())
                } else {
                    Err(first_err)
                }
            }
            Err(err) => Err(err),
        })
    }

    fn set_goal_status(
        &self,
        goal_id: &str,
        status: &AgentGoalStatus,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let mut args = serde_json::Map::new();
        args.insert(
            "goal_id".to_string(),
            serde_json::Value::String(goal_id.to_string()),
        );
        args.insert(
            "status".to_string(),
            serde_json::Value::String(status.as_str().to_string()),
        );
        let task = self.call_tool("goal_update", Some(args), cx);
        cx.spawn(async move |_cx| {
            task.await?;
            Ok(())
        })
    }

    fn archive_goal(&self, goal_id: &str, cx: &mut App) -> Task<Result<()>> {
        let mut args = serde_json::Map::new();
        args.insert(
            "goal_id".to_string(),
            serde_json::Value::String(goal_id.to_string()),
        );
        let task = self.call_tool("goal_archive", Some(args), cx);
        cx.spawn(async move |_cx| {
            task.await?;
            Ok(())
        })
    }

    fn unarchive_goal(&self, goal_id: &str, cx: &mut App) -> Task<Result<()>> {
        let mut args = serde_json::Map::new();
        args.insert(
            "goal_id".to_string(),
            serde_json::Value::String(goal_id.to_string()),
        );
        let task = self.call_tool("goal_unarchive", Some(args), cx);
        cx.spawn(async move |_cx| {
            task.await?;
            Ok(())
        })
    }

    fn delete_goal(&self, goal_id: &str, cx: &mut App) -> Task<Result<()>> {
        let mut args = serde_json::Map::new();
        args.insert(
            "goal_id".to_string(),
            serde_json::Value::String(goal_id.to_string()),
        );
        let task = self.call_tool("goal_delete", Some(args), cx);
        cx.spawn(async move |_cx| {
            task.await?;
            Ok(())
        })
    }
}
