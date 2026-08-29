use std::sync::Arc;

use anyhow::Result;
use context_server::ContextServerId;
use gpui::{App, SharedString, Task};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentTaskId(pub Arc<str>);

impl std::fmt::Display for AgentTaskId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl From<&str> for AgentTaskId {
    fn from(value: &str) -> Self {
        Self(Arc::from(value))
    }
}

impl From<String> for AgentTaskId {
    fn from(value: String) -> Self {
        Self(Arc::from(value))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTaskStatus {
    Ready,
    Blocked,
    Running,
    Stale,
    Review,
    Completed,
    Failed,
}

impl AgentTaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentTaskSummary {
    pub id: AgentTaskId,
    pub parent_id: Option<AgentTaskId>,
    pub title: String,
    pub status: AgentTaskStatus,
    pub attempt: u32,
    pub assignee: Option<SharedString>,
    pub write_scopes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct AgentTaskGraph {
    pub tasks: Vec<AgentTaskSummary>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentTaskDetail {
    pub summary: AgentTaskSummary,
    pub description: String,
    pub acceptance_criteria: Vec<String>,
    pub events_tail: Vec<AgentTaskEvent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTaskEventKind {
    Info,
    ToolCall,
    PolicyDenied,
    StatusChanged,
    ReviewVerdict,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentTaskEvent {
    pub seq: u64,
    pub timestamp_millis: u64,
    pub task_id: Option<AgentTaskId>,
    pub kind: AgentTaskEventKind,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentTaskArtifact {
    pub id: String,
    pub task_id: AgentTaskId,
    pub kind: String,
    pub content: String,
}

pub trait AgentTaskProvider: 'static + Send + Sync {
    fn server_id(&self) -> ContextServerId;
    fn fetch_graph(&self, cx: &mut App) -> Task<Result<AgentTaskGraph>>;
    fn get_task(&self, id: &AgentTaskId, cx: &mut App) -> Task<Result<AgentTaskDetail>>;
    fn complete_task(&self, id: &AgentTaskId, cx: &mut App) -> Task<Result<()>>;
    fn fail_task(&self, id: &AgentTaskId, reason: &str, cx: &mut App) -> Task<Result<()>>;
    fn list_events(&self, limit: u32, cx: &mut App) -> Task<Result<Vec<AgentTaskEvent>>>;
    fn list_artifacts(
        &self,
        task_id: &AgentTaskId,
        cx: &mut App,
    ) -> Task<Result<Vec<AgentTaskArtifact>>>;
    fn get_artifact(&self, artifact_id: &str, cx: &mut App) -> Task<Result<AgentTaskArtifact>>;
}
