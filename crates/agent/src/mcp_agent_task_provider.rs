use std::sync::Arc;

use anyhow::{Context as _, Result};
use context_server::ContextServerId;
use gpui::{App, Entity, Task};
use project::context_server_store::ContextServerStore;

use crate::agent_task::{
    AgentGoalStatus, AgentTaskArtifact, AgentTaskDetail, AgentTaskGraph, AgentTaskId,
    AgentTaskProvider, AgentTaskStatus, AgentTurnRecord,
};

#[derive(Clone)]
pub struct McpAgentTaskProvider {
    store: Entity<ContextServerStore>,
    server_id: ContextServerId,
}

impl McpAgentTaskProvider {
    pub fn new(store: Entity<ContextServerStore>, server_id: ContextServerId) -> Self {
        let provider = Self { store, server_id };
        crate::task_worktree::register_task_provider(Arc::new(provider.clone()));
        provider
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

    pub fn record_task_event(
        &self,
        id: &AgentTaskId,
        kind: &str,
        message: &str,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let mut event_args = serde_json::Map::new();
        event_args.insert(
            "task_id".to_string(),
            serde_json::Value::String(id.to_string()),
        );
        event_args.insert(
            "kind".to_string(),
            serde_json::Value::String(kind.to_string()),
        );
        event_args.insert(
            "message".to_string(),
            serde_json::Value::String(message.to_string()),
        );

        let event_task = self.call_tool("task_event", Some(event_args), cx);
        let store = self.store.clone();
        let server_id = self.server_id.clone();
        let id_clone = id.clone();
        let message_clone = message.to_string();

        cx.spawn(async move |cx| {
            if event_task.await.is_ok() {
                return Ok(());
            }

            let mut update_args = serde_json::Map::new();
            update_args.insert(
                "task_id".to_string(),
                serde_json::Value::String(id_clone.to_string()),
            );
            update_args.insert(
                "error".to_string(),
                serde_json::Value::String(message_clone),
            );

            let fallback_task = cx.update(|cx| {
                let provider = McpAgentTaskProvider::new(store, server_id);
                provider.call_tool("task_update", Some(update_args), cx)
            });
            fallback_task.await?;
            Ok(())
        })
    }

    pub fn record_task_error(
        &self,
        id: &AgentTaskId,
        error: &str,
        cx: &mut App,
    ) -> Task<Result<()>> {
        self.record_task_event(id, "error", error, cx)
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
        args.insert(
            "status".to_string(),
            serde_json::Value::String("completed".to_string()),
        );
        let task = self.call_tool("task_update", Some(args), cx);
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
            "status".to_string(),
            serde_json::Value::String("failed".to_string()),
        );
        args.insert(
            "error".to_string(),
            serde_json::Value::String(reason.to_string()),
        );
        let task = self.call_tool("task_update", Some(args), cx);
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
        args.insert(
            "status".to_string(),
            serde_json::Value::String("archived".to_string()),
        );
        let task = self.call_tool("task_update", Some(args), cx);
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
        args.insert(
            "status".to_string(),
            serde_json::Value::String("ready".to_string()),
        );
        let task = self.call_tool("task_update", Some(args), cx);
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

    fn record_turn(
        &self,
        task_id: &AgentTaskId,
        record: AgentTurnRecord,
        cx: &mut App,
    ) -> Task<Result<u32>> {
        let mut args = serde_json::Map::new();
        args.insert(
            "task_id".to_string(),
            serde_json::Value::String(task_id.to_string()),
        );
        args.insert(
            "duration_ms".to_string(),
            serde_json::Value::Number(record.duration_ms.into()),
        );
        if let Some(tokens) = record.tokens {
            let tokens_obj = serde_json::json!({
                "input": tokens.input_tokens,
                "output": tokens.output_tokens,
                "cache_read": tokens.cache_read_input_tokens,
                "cache_creation": tokens.cache_creation_input_tokens,
            });
            args.insert("tokens".to_string(), tokens_obj);
        }
        if let Some(model) = record
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            args.insert(
                "model".to_string(),
                serde_json::Value::String(model.to_string()),
            );
        }
        if let Some(session_id) = record
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            args.insert(
                "session_id".to_string(),
                serde_json::Value::String(session_id.to_string()),
            );
        }
        if let Some(error) = record
            .error
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            args.insert(
                "error".to_string(),
                serde_json::Value::String(error.to_string()),
            );
        }
        if let Some(branch) = record
            .branch
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            args.insert(
                "branch".to_string(),
                serde_json::Value::String(branch.to_string()),
            );
        }
        if let Some(commit_sha) = record
            .commit_sha
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            args.insert(
                "commit_sha".to_string(),
                serde_json::Value::String(commit_sha.to_string()),
            );
        }
        if let Some(output_summary) = record
            .output_summary
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            args.insert(
                "output_summary".to_string(),
                serde_json::Value::String(output_summary.to_string()),
            );
        }
        if let Some(diff) = record.diff {
            let diff_obj = serde_json::json!({
                "files_changed": diff.files_changed,
                "insertions": diff.insertions,
                "deletions": diff.deletions,
            });
            args.insert("diff".to_string(), diff_obj);
        }
        let task = self.call_tool("task_record_turn", Some(args), cx);
        cx.spawn(async move |_cx| {
            let value = task.await?;
            let turn_index = if let Some(n) = value.as_u64() {
                n as u32
            } else if let Some(obj) = value.as_object() {
                obj.get("turn_index")
                    .or_else(|| obj.get("turn"))
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32)
                    .unwrap_or(0)
            } else {
                0
            };
            Ok(turn_index)
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

    fn set_task_status(
        &self,
        id: &AgentTaskId,
        status: &AgentTaskStatus,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let mut args = serde_json::Map::new();
        args.insert(
            "task_id".to_string(),
            serde_json::Value::String(id.to_string()),
        );
        args.insert(
            "status".to_string(),
            serde_json::Value::String(status.as_str().to_string()),
        );
        let task = self.call_tool("task_update", Some(args), cx);
        cx.spawn(async move |_cx| {
            task.await?;
            Ok(())
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
        args.insert(
            "status".to_string(),
            serde_json::Value::String("archived".to_string()),
        );
        let task = self.call_tool("goal_update", Some(args), cx);
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
        args.insert(
            "status".to_string(),
            serde_json::Value::String("running".to_string()),
        );
        let task = self.call_tool("goal_update", Some(args), cx);
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

    fn update_task_title(&self, id: &AgentTaskId, title: &str, cx: &mut App) -> Task<Result<()>> {
        let mut args = serde_json::Map::new();
        args.insert(
            "task_id".to_string(),
            serde_json::Value::String(id.to_string()),
        );
        args.insert(
            "title".to_string(),
            serde_json::Value::String(title.to_string()),
        );
        let task = self.call_tool("task_update", Some(args), cx);
        cx.spawn(async move |_cx| {
            task.await?;
            Ok(())
        })
    }

    fn update_goal_title(&self, goal_id: &str, title: &str, cx: &mut App) -> Task<Result<()>> {
        let mut args = serde_json::Map::new();
        args.insert(
            "goal_id".to_string(),
            serde_json::Value::String(goal_id.to_string()),
        );
        args.insert(
            "title".to_string(),
            serde_json::Value::String(title.to_string()),
        );
        let task = self.call_tool("goal_update", Some(args), cx);
        cx.spawn(async move |_cx| {
            task.await?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::Fs as _;
    use futures::StreamExt as _;
    use futures::channel::mpsc;
    use gpui::TestAppContext;
    use project::Project;
    use settings::Settings as _;
    use std::sync::Arc;

    async fn setup_test_provider(
        cx: &mut TestAppContext,
        fake_transport: context_server::test::FakeTransport,
    ) -> McpAgentTaskProvider {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);

            let mut settings = project::project_settings::ProjectSettings::get_global(cx).clone();
            settings.context_servers.insert(
                "tgs".into(),
                project::project_settings::ContextServerSettings::Stdio {
                    enabled: true,
                    remote: false,
                    command: context_server::ContextServerCommand {
                        path: "mock".into(),
                        args: Vec::new(),
                        env: None,
                        timeout: None,
                        platforms: Default::default(),
                    },
                },
            );
            project::project_settings::ProjectSettings::override_global(settings, cx);
        });

        let fs = fs::FakeFs::new(cx.executor());
        fs.create_dir(std::path::Path::new("/root")).await.unwrap();
        let project = Project::test(fs.clone(), [std::path::Path::new("/root")], cx).await;

        let context_server_store = project.read_with(cx, |p, _| p.context_server_store());
        context_server_store.update(cx, |store, cx| {
            store.start_server(
                Arc::new(context_server::ContextServer::new(
                    ContextServerId("tgs".into()),
                    Arc::new(fake_transport),
                )),
                cx,
            );
        });
        cx.run_until_parked();

        McpAgentTaskProvider::new(
            context_server_store,
            ContextServerId("tgs".into()),
        )
    }

    #[gpui::test]
    async fn test_record_task_error_uses_task_event(cx: &mut TestAppContext) {
        let (calls_tx, mut calls_rx) = mpsc::unbounded();

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
                let calls_tx = calls_tx.clone();
                async move {
                    calls_tx.unbounded_send(params).ok();
                    context_server::types::CallToolResponse {
                        content: vec![context_server::types::ToolResponseContent::Text {
                            text: "ok".into(),
                        }],
                        is_error: None,
                        meta: None,
                        structured_content: None,
                    }
                }
            });

        let provider = setup_test_provider(cx, fake_transport).await;

        let task = cx.update(|cx| {
            provider.record_task_error(&AgentTaskId::from("TASK-42"), "simulated commit error", cx)
        });
        task.await.unwrap();

        let call = calls_rx.next().await.expect("expected task_event call");
        assert_eq!(call.name, "task_event");
        let args = call.arguments.expect("expected args");
        assert_eq!(args.get("task_id").and_then(|v| v.as_str()), Some("TASK-42"));
        assert_eq!(args.get("kind").and_then(|v| v.as_str()), Some("error"));
        assert_eq!(args.get("message").and_then(|v| v.as_str()), Some("simulated commit error"));
    }

    #[gpui::test]
    async fn test_record_task_error_falls_back_to_task_update(cx: &mut TestAppContext) {
        let (calls_tx, mut calls_rx) = mpsc::unbounded();

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
                let calls_tx = calls_tx.clone();
                async move {
                    let is_task_event = params.name == "task_event";
                    calls_tx.unbounded_send(params).ok();
                    if is_task_event {
                        context_server::types::CallToolResponse {
                            content: vec![context_server::types::ToolResponseContent::Text {
                                text: "task_event not supported".into(),
                            }],
                            is_error: Some(true),
                            meta: None,
                            structured_content: None,
                        }
                    } else {
                        context_server::types::CallToolResponse {
                            content: vec![context_server::types::ToolResponseContent::Text {
                                text: "ok".into(),
                            }],
                            is_error: None,
                            meta: None,
                            structured_content: None,
                        }
                    }
                }
            });

        let provider = setup_test_provider(cx, fake_transport).await;

        let task = cx.update(|cx| {
            provider.record_task_error(&AgentTaskId::from("TASK-42"), "simulated commit error", cx)
        });
        task.await.unwrap();

        let first_call = calls_rx.next().await.expect("expected task_event call");
        assert_eq!(first_call.name, "task_event");

        let second_call = calls_rx.next().await.expect("expected task_update fallback call");
        assert_eq!(second_call.name, "task_update");
        let args = second_call.arguments.expect("expected args");
        assert_eq!(args.get("task_id").and_then(|v| v.as_str()), Some("TASK-42"));
        assert_eq!(args.get("error").and_then(|v| v.as_str()), Some("simulated commit error"));
    }
}
