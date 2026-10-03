use std::sync::Arc;

use anyhow::Result;
use context_server::ContextServerId;
use gpui::{App, SharedString, Task};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AgentTaskId(pub Arc<str>);

impl AgentTaskId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for AgentTaskId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<str> for AgentTaskId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AgentTaskStatus {
    Ready,
    Blocked,
    Running,
    Stale,
    Review,
    Completed,
    Failed,
    Archived,
    Other(SharedString),
}

impl AgentTaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Archived)
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Ready => "ready",
            Self::Blocked => "blocked",
            Self::Running => "running",
            Self::Stale => "stale",
            Self::Review => "review",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Archived => "archived",
            Self::Other(custom) => custom.as_ref(),
        }
    }
}

impl Serialize for AgentTaskStatus {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AgentTaskStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "ready" => Self::Ready,
            "blocked" => Self::Blocked,
            "running" => Self::Running,
            "stale" => Self::Stale,
            "review" => Self::Review,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "archived" => Self::Archived,
            _ => Self::Other(SharedString::from(s)),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentTaskSummary {
    pub id: AgentTaskId,
    pub parent_id: Option<AgentTaskId>,
    #[serde(default)]
    pub goal_id: Option<String>,
    pub title: String,
    pub status: AgentTaskStatus,
    pub attempt: u32,
    pub assignee: Option<SharedString>,
    pub write_scopes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentGoalSummary {
    pub goal_id: String,
    pub title: String,
    pub status: String,
    pub priority: i64,
    pub tasks_total: u64,
    pub tasks_done: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct AgentTaskGraph {
    pub tasks: Vec<AgentTaskSummary>,
    #[serde(default)]
    pub goals: Vec<AgentGoalSummary>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentTaskDetail {
    pub summary: AgentTaskSummary,
    pub description: String,
    pub acceptance_criteria: Vec<String>,
    pub events_tail: Vec<AgentTaskEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AgentTaskEventKind {
    Info,
    ToolCall,
    PolicyDenied,
    StatusChanged,
    ReviewVerdict,
    Other(SharedString),
}

impl AgentTaskEventKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Info => "info",
            Self::ToolCall => "tool_call",
            Self::PolicyDenied => "policy_denied",
            Self::StatusChanged => "status_changed",
            Self::ReviewVerdict => "review_verdict",
            Self::Other(custom) => custom.as_ref(),
        }
    }
}

impl Serialize for AgentTaskEventKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AgentTaskEventKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "info" => Self::Info,
            "tool_call" => Self::ToolCall,
            "policy_denied" => Self::PolicyDenied,
            "status_changed" => Self::StatusChanged,
            "review_verdict" => Self::ReviewVerdict,
            _ => Self::Other(SharedString::from(s)),
        })
    }
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
    fn archive_task(&self, id: &AgentTaskId, cx: &mut App) -> Task<Result<()>>;
    fn unarchive_task(&self, id: &AgentTaskId, cx: &mut App) -> Task<Result<()>>;
    fn delete_task(&self, id: &AgentTaskId, cx: &mut App) -> Task<Result<()>>;
    fn list_events(&self, limit: u32, cx: &mut App) -> Task<Result<Vec<AgentTaskEvent>>>;
    fn list_artifacts(
        &self,
        task_id: &AgentTaskId,
        cx: &mut App,
    ) -> Task<Result<Vec<AgentTaskArtifact>>>;
    fn get_artifact(&self, artifact_id: &str, cx: &mut App) -> Task<Result<AgentTaskArtifact>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_tolerant_task_status_parsing() {
        // Unknown status "cancelled"
        let status: AgentTaskStatus = serde_json::from_str("\"cancelled\"").unwrap();
        assert_eq!(
            status,
            AgentTaskStatus::Other(SharedString::from("cancelled"))
        );
        assert!(!status.is_terminal());
        assert_eq!(status.as_str(), "cancelled");

        // Roundtrip serialization preserves unknown status string
        let serialized = serde_json::to_string(&status).unwrap();
        assert_eq!(serialized, "\"cancelled\"");
        let deserialized: AgentTaskStatus = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, status);

        // Known statuses
        let ready: AgentTaskStatus = serde_json::from_str("\"ready\"").unwrap();
        assert_eq!(ready, AgentTaskStatus::Ready);
        assert!(!ready.is_terminal());
        assert_eq!(ready.as_str(), "ready");

        let running: AgentTaskStatus = serde_json::from_str("\"running\"").unwrap();
        assert_eq!(running, AgentTaskStatus::Running);
        assert!(!running.is_terminal());
        assert_eq!(running.as_str(), "running");

        let completed: AgentTaskStatus = serde_json::from_str("\"completed\"").unwrap();
        assert_eq!(completed, AgentTaskStatus::Completed);
        assert!(completed.is_terminal());
        assert_eq!(completed.as_str(), "completed");

        let failed: AgentTaskStatus = serde_json::from_str("\"failed\"").unwrap();
        assert_eq!(failed, AgentTaskStatus::Failed);
        assert!(failed.is_terminal());
        assert_eq!(failed.as_str(), "failed");
    }

    #[test]
    fn test_tolerant_task_event_kind_parsing() {
        let kind: AgentTaskEventKind = serde_json::from_str("\"custom_event_kind\"").unwrap();
        assert_eq!(
            kind,
            AgentTaskEventKind::Other(SharedString::from("custom_event_kind"))
        );
        assert_eq!(kind.as_str(), "custom_event_kind");

        // Roundtrip serialization
        let serialized = serde_json::to_string(&kind).unwrap();
        assert_eq!(serialized, "\"custom_event_kind\"");
        let deserialized: AgentTaskEventKind = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, kind);

        // Known kinds
        let status_changed: AgentTaskEventKind =
            serde_json::from_str("\"status_changed\"").unwrap();
        assert_eq!(status_changed, AgentTaskEventKind::StatusChanged);
        assert_eq!(status_changed.as_str(), "status_changed");
    }

    #[test]
    fn test_task_summary_goal_id_parsing() {
        // goal_id absent
        let json_absent = json!({
            "id": "T-1",
            "parent_id": null,
            "title": "Task without goal",
            "status": "ready",
            "attempt": 1,
            "assignee": null,
            "write_scopes": []
        });
        let summary_absent: AgentTaskSummary = serde_json::from_value(json_absent).unwrap();
        assert_eq!(summary_absent.goal_id, None);

        // goal_id null
        let json_null = json!({
            "id": "T-2",
            "parent_id": null,
            "goal_id": null,
            "title": "Task with null goal",
            "status": "running",
            "attempt": 1,
            "assignee": null,
            "write_scopes": []
        });
        let summary_null: AgentTaskSummary = serde_json::from_value(json_null).unwrap();
        assert_eq!(summary_null.goal_id, None);

        // goal_id present
        let json_present = json!({
            "id": "T-3",
            "parent_id": null,
            "goal_id": "GOAL-7",
            "title": "Task with goal",
            "status": "completed",
            "attempt": 1,
            "assignee": null,
            "write_scopes": []
        });
        let summary_present: AgentTaskSummary = serde_json::from_value(json_present).unwrap();
        assert_eq!(summary_present.goal_id, Some("GOAL-7".to_string()));
    }

    #[test]
    fn test_task_graph_goals_and_unknown_fields_roundtrip() {
        // Old task graph response without goals field
        let json_old = json!({
            "tasks": [
                {
                    "id": "T-42",
                    "parent_id": null,
                    "title": "Old Task",
                    "status": "cancelled",
                    "attempt": 1,
                    "assignee": null,
                    "write_scopes": []
                }
            ]
        });
        let graph_old: AgentTaskGraph = serde_json::from_value(json_old).unwrap();
        assert_eq!(graph_old.tasks.len(), 1);
        assert_eq!(
            graph_old.tasks[0].status,
            AgentTaskStatus::Other(SharedString::from("cancelled"))
        );
        assert_eq!(graph_old.tasks[0].goal_id, None);
        assert!(graph_old.goals.is_empty());

        // Empty goals array
        let json_empty_goals = json!({
            "tasks": [],
            "goals": []
        });
        let graph_empty: AgentTaskGraph = serde_json::from_value(json_empty_goals).unwrap();
        assert!(graph_empty.tasks.is_empty());
        assert!(graph_empty.goals.is_empty());

        // Populated task graph with goals and unknown status
        let json_new = json!({
            "tasks": [
                {
                    "id": "T-42",
                    "parent_id": null,
                    "goal_id": "GOAL-7",
                    "title": "Rework ATP data layer",
                    "status": "in_review",
                    "attempt": 1,
                    "assignee": "agent",
                    "write_scopes": ["src/"]
                }
            ],
            "goals": [
                {
                    "goal_id": "GOAL-7",
                    "title": "Rework Agent Task Panel",
                    "status": "active",
                    "priority": 1,
                    "tasks_total": 2,
                    "tasks_done": 1
                }
            ]
        });
        let graph_new: AgentTaskGraph = serde_json::from_value(json_new).unwrap();
        assert_eq!(graph_new.tasks.len(), 1);
        assert_eq!(graph_new.tasks[0].id, AgentTaskId::from("T-42"));
        assert_eq!(graph_new.tasks[0].goal_id, Some("GOAL-7".to_string()));
        assert_eq!(
            graph_new.tasks[0].status,
            AgentTaskStatus::Other(SharedString::from("in_review"))
        );
        assert_eq!(graph_new.goals.len(), 1);
        assert_eq!(
            graph_new.goals[0],
            AgentGoalSummary {
                goal_id: "GOAL-7".to_string(),
                title: "Rework Agent Task Panel".to_string(),
                status: "active".to_string(),
                priority: 1,
                tasks_total: 2,
                tasks_done: 1,
            }
        );

        // Roundtrip serialize/deserialize
        let serialized = serde_json::to_string(&graph_new).unwrap();
        let deserialized: AgentTaskGraph = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, graph_new);
    }
}
