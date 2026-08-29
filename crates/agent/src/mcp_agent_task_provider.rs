use anyhow::{Context as _, Result};
use context_server::ContextServerId;
use gpui::{App, Entity, Task};
use project::context_server_store::ContextServerStore;

use crate::agent_task::{
    AgentTaskArtifact, AgentTaskDetail, AgentTaskEvent, AgentTaskGraph, AgentTaskId,
    AgentTaskProvider,
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
}
