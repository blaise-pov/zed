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
    Cancelled,
    Archived,
    Other(SharedString),
}

impl AgentTaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Archived | Self::Cancelled
        )
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
            Self::Cancelled => "cancelled",
            Self::Archived => "archived",
            Self::Other(custom) => custom.as_ref(),
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "ready" => Self::Ready,
            "blocked" => Self::Blocked,
            "running" => Self::Running,
            "stale" => Self::Stale,
            "review" => Self::Review,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "archived" => Self::Archived,
            _ => Self::Other(SharedString::from(s.to_string())),
        }
    }
}

impl std::fmt::Display for AgentTaskStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.as_str())
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
        Ok(Self::from_str(&s))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AgentGoalStatus {
    Running,
    Blocked,
    Failed,
    Completed,
    Cancelled,
    Archived,
    Other(SharedString),
}

impl AgentGoalStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Failed | Self::Completed | Self::Cancelled | Self::Archived
        )
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Archived => "archived",
            Self::Other(custom) => custom.as_ref(),
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "running" | "active" => Self::Running,
            "blocked" => Self::Blocked,
            "failed" => Self::Failed,
            "completed" => Self::Completed,
            "cancelled" => Self::Cancelled,
            "archived" => Self::Archived,
            _ => Self::Other(SharedString::from(s.to_string())),
        }
    }
}

impl std::fmt::Display for AgentGoalStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.as_str())
    }
}

impl Serialize for AgentGoalStatus {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AgentGoalStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(Self::from_str(&s))
    }
}

pub fn deserialize_created_at<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct CreatedAtVisitor;

    impl<'de> serde::de::Visitor<'de> for CreatedAtVisitor {
        type Value = Option<u64>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a timestamp number in epoch millis, RFC3339 string, or null")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(Some(value))
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if value >= 0 {
                Ok(Some(value as u64))
            } else {
                Ok(None)
            }
        }

        fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if value >= 0.0 {
                Ok(Some(value as u64))
            } else {
                Ok(None)
            }
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            let trimmed = value.trim();
            if let Ok(millis) = trimmed.parse::<u64>() {
                return Ok(Some(millis));
            }
            if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(trimmed) {
                let millis = parsed.timestamp_millis();
                if millis >= 0 {
                    return Ok(Some(millis as u64));
                }
            }
            Ok(None)
        }
    }

    deserializer.deserialize_any(CreatedAtVisitor)
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDiffShortstat {
    pub files_changed: u64,
    pub insertions: u64,
    pub deletions: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentTaskMetrics {
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_tokens: Option<u64>,
    #[serde(default)]
    pub total_tokens: Option<u64>,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default, alias = "turns")]
    pub turns_count: Option<u32>,
    #[serde(default)]
    pub diff: Option<AgentDiffShortstat>,
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
    #[serde(default, deserialize_with = "deserialize_created_at")]
    pub created_at: Option<u64>,
    #[serde(default)]
    pub assigned_profile: Option<SharedString>,
    #[serde(default)]
    pub model: Option<SharedString>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub metrics: Option<AgentTaskMetrics>,
}

impl Default for AgentTaskSummary {
    fn default() -> Self {
        Self {
            id: AgentTaskId::from(""),
            parent_id: None,
            goal_id: None,
            title: String::new(),
            status: AgentTaskStatus::Ready,
            attempt: 0,
            assignee: None,
            write_scopes: Vec::new(),
            created_at: None,
            assigned_profile: None,
            model: None,
            session_id: None,
            metrics: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentGoalSummary {
    pub goal_id: String,
    pub title: String,
    pub status: AgentGoalStatus,
    pub priority: i64,
    pub tasks_total: u64,
    pub tasks_done: u64,
    #[serde(default, deserialize_with = "deserialize_created_at")]
    pub created_at: Option<u64>,
    #[serde(default)]
    pub metrics: Option<AgentTaskMetrics>,
    #[serde(default)]
    pub session_id: Option<String>,
}

impl Default for AgentGoalSummary {
    fn default() -> Self {
        Self {
            goal_id: String::new(),
            title: String::new(),
            status: AgentGoalStatus::Running,
            priority: 0,
            tasks_total: 0,
            tasks_done: 0,
            created_at: None,
            metrics: None,
            session_id: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentTurnRecord {
    pub duration_ms: u64,
    pub tokens: Option<language_model::TokenUsage>,
    pub model: Option<SharedString>,
    pub session_id: Option<SharedString>,
    pub error: Option<SharedString>,
    pub branch: Option<SharedString>,
    pub commit_sha: Option<SharedString>,
    pub output_summary: Option<SharedString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<AgentDiffShortstat>,
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
    fn ensure_task(
        &self,
        id: &AgentTaskId,
        title: &str,
        goal_id: Option<&str>,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let _ = id;
        let _ = title;
        let _ = goal_id;
        let _ = cx;
        Task::ready(Ok(()))
    }
    fn update_task_metadata(
        &self,
        id: &AgentTaskId,
        model: Option<&str>,
        assignee: Option<&str>,
        session_id: Option<&str>,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let _ = (id, model, assignee, session_id, cx);
        Task::ready(Ok(()))
    }
    fn complete_task(&self, id: &AgentTaskId, cx: &mut App) -> Task<Result<()>>;
    fn fail_task(&self, id: &AgentTaskId, reason: &str, cx: &mut App) -> Task<Result<()>>;
    fn archive_task(&self, id: &AgentTaskId, cx: &mut App) -> Task<Result<()>>;
    fn unarchive_task(&self, id: &AgentTaskId, cx: &mut App) -> Task<Result<()>>;
    fn delete_task(&self, id: &AgentTaskId, cx: &mut App) -> Task<Result<()>>;
    fn list_artifacts(
        &self,
        task_id: &AgentTaskId,
        cx: &mut App,
    ) -> Task<Result<Vec<AgentTaskArtifact>>>;
    fn get_artifact(&self, artifact_id: &str, cx: &mut App) -> Task<Result<AgentTaskArtifact>>;
    fn record_turn(
        &self,
        task_id: &AgentTaskId,
        record: AgentTurnRecord,
        cx: &mut App,
    ) -> Task<Result<u32>> {
        let _ = (task_id, record, cx);
        Task::ready(Ok(0))
    }
    fn publish_artifact(
        &self,
        task_id: &AgentTaskId,
        kind: &str,
        content: &str,
        summary: Option<&str>,
        cx: &mut App,
    ) -> Task<Result<String>> {
        let _ = (task_id, kind, content, summary, cx);
        Task::ready(Ok(String::new()))
    }
    fn update_task_title(&self, id: &AgentTaskId, title: &str, cx: &mut App) -> Task<Result<()>> {
        let _ = (id, title, cx);
        Task::ready(Err(anyhow::anyhow!("update_task_title not supported")))
    }
    fn update_goal_title(&self, goal_id: &str, title: &str, cx: &mut App) -> Task<Result<()>> {
        let _ = (goal_id, title, cx);
        Task::ready(Err(anyhow::anyhow!("update_goal_title not supported")))
    }
    fn set_task_status(
        &self,
        id: &AgentTaskId,
        status: &AgentTaskStatus,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let _ = id;
        let _ = status;
        let _ = cx;
        Task::ready(Err(anyhow::anyhow!("set_task_status not supported")))
    }
    fn set_goal_status(
        &self,
        goal_id: &str,
        status: &AgentGoalStatus,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let _ = goal_id;
        let _ = status;
        let _ = cx;
        Task::ready(Err(anyhow::anyhow!("set_goal_status not supported")))
    }
    fn archive_goal(&self, goal_id: &str, cx: &mut App) -> Task<Result<()>> {
        let _ = goal_id;
        let _ = cx;
        Task::ready(Err(anyhow::anyhow!("archive_goal not supported")))
    }
    fn unarchive_goal(&self, goal_id: &str, cx: &mut App) -> Task<Result<()>> {
        let _ = goal_id;
        let _ = cx;
        Task::ready(Err(anyhow::anyhow!("unarchive_goal not supported")))
    }
    fn delete_goal(&self, goal_id: &str, cx: &mut App) -> Task<Result<()>> {
        let _ = goal_id;
        let _ = cx;
        Task::ready(Err(anyhow::anyhow!("delete_goal not supported")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_tolerant_task_status_parsing() {
        // Known status "cancelled"
        let cancelled: AgentTaskStatus = serde_json::from_str("\"cancelled\"").unwrap();
        assert_eq!(cancelled, AgentTaskStatus::Cancelled);
        assert!(cancelled.is_terminal());
        assert_eq!(cancelled.as_str(), "cancelled");

        // Roundtrip serialization for Cancelled
        let serialized = serde_json::to_string(&cancelled).unwrap();
        assert_eq!(serialized, "\"cancelled\"");
        let deserialized: AgentTaskStatus = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, cancelled);

        // Unknown status "paused"
        let status: AgentTaskStatus = serde_json::from_str("\"paused\"").unwrap();
        assert_eq!(status, AgentTaskStatus::Other(SharedString::from("paused")));
        assert!(!status.is_terminal());
        assert_eq!(status.as_str(), "paused");

        // Roundtrip serialization preserves unknown status string
        let serialized = serde_json::to_string(&status).unwrap();
        assert_eq!(serialized, "\"paused\"");
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

        let archived: AgentTaskStatus = serde_json::from_str("\"archived\"").unwrap();
        assert_eq!(archived, AgentTaskStatus::Archived);
        assert!(archived.is_terminal());
        assert_eq!(archived.as_str(), "archived");
    }

    #[test]
    fn test_tolerant_goal_status_parsing() {
        let running: AgentGoalStatus = serde_json::from_str("\"running\"").unwrap();
        assert_eq!(running, AgentGoalStatus::Running);
        assert!(!running.is_terminal());
        assert_eq!(running.as_str(), "running");
        assert_eq!(format!("{running}"), "running");

        let active: AgentGoalStatus = serde_json::from_str("\"active\"").unwrap();
        assert_eq!(active, AgentGoalStatus::Running);
        assert!(!active.is_terminal());
        assert_eq!(active.as_str(), "running");

        let blocked: AgentGoalStatus = serde_json::from_str("\"blocked\"").unwrap();
        assert_eq!(blocked, AgentGoalStatus::Blocked);
        assert!(!blocked.is_terminal());

        let failed: AgentGoalStatus = serde_json::from_str("\"failed\"").unwrap();
        assert_eq!(failed, AgentGoalStatus::Failed);
        assert!(failed.is_terminal());

        let completed: AgentGoalStatus = serde_json::from_str("\"completed\"").unwrap();
        assert_eq!(completed, AgentGoalStatus::Completed);
        assert!(completed.is_terminal());

        let cancelled: AgentGoalStatus = serde_json::from_str("\"cancelled\"").unwrap();
        assert_eq!(cancelled, AgentGoalStatus::Cancelled);
        assert!(cancelled.is_terminal());

        let archived: AgentGoalStatus = serde_json::from_str("\"archived\"").unwrap();
        assert_eq!(archived, AgentGoalStatus::Archived);
        assert!(archived.is_terminal());

        let other: AgentGoalStatus = serde_json::from_str("\"paused\"").unwrap();
        assert_eq!(other, AgentGoalStatus::Other(SharedString::from("paused")));
        assert!(!other.is_terminal());
        assert_eq!(other.as_str(), "paused");
        assert_eq!(format!("{other}"), "paused");
    }

    #[test]
    fn test_created_at_rfc3339_with_offset() {
        let json_with_offset = r#"{
            "id": "TASK-OFFSET",
            "title": "Offset Task",
            "status": "ready",
            "attempt": 1,
            "write_scopes": [],
            "created_at": "2026-10-07T15:05:00+03:00"
        }"#;

        let item: AgentTaskSummary = serde_json::from_str(json_with_offset)
            .expect("failed to parse item with +03:00 offset");
        assert_eq!(item.created_at, Some(1791374700000));
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
        assert_eq!(graph_old.tasks[0].status, AgentTaskStatus::Cancelled);
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
                status: AgentGoalStatus::Running,
                priority: 1,
                tasks_total: 2,
                tasks_done: 1,
                created_at: None,
                metrics: None,
                session_id: None,
            }
        );

        // Roundtrip serialize/deserialize
        let serialized = serde_json::to_string(&graph_new).unwrap();
        let deserialized: AgentTaskGraph = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, graph_new);
    }

    #[test]
    fn test_task_metrics_deserialization() {
        let json_data = json!({
            "duration_ms": 12500,
            "input_tokens": 1500,
            "output_tokens": 250,
            "cache_read_tokens": 300,
            "cache_creation_tokens": 50,
            "total_tokens": 2100,
            "cost_usd": 0.015,
            "turns_count": 3
        });
        let metrics: AgentTaskMetrics = serde_json::from_value(json_data).unwrap();
        assert_eq!(metrics.duration_ms, Some(12500));
        assert_eq!(metrics.input_tokens, Some(1500));
        assert_eq!(metrics.output_tokens, Some(250));
        assert_eq!(metrics.cache_read_tokens, Some(300));
        assert_eq!(metrics.cache_creation_tokens, Some(50));
        assert_eq!(metrics.total_tokens, Some(2100));
        assert_eq!(metrics.cost_usd, Some(0.015));
        assert_eq!(metrics.turns_count, Some(3));

        // Alias "turns" -> turns_count
        let json_alias = json!({
            "turns": 5
        });
        let metrics_alias: AgentTaskMetrics = serde_json::from_value(json_alias).unwrap();
        assert_eq!(metrics_alias.turns_count, Some(5));

        // Task summary with metrics
        let task_json = json!({
            "id": "TASK-METRICS",
            "title": "Task with metrics",
            "status": "completed",
            "attempt": 1,
            "write_scopes": [],
            "metrics": {
                "duration_ms": 5000,
                "input_tokens": 100,
                "output_tokens": 20
            }
        });
        let task: AgentTaskSummary = serde_json::from_value(task_json).unwrap();
        assert!(task.metrics.is_some());
        let m = task.metrics.unwrap();
        assert_eq!(m.duration_ms, Some(5000));
        assert_eq!(m.input_tokens, Some(100));
        assert_eq!(m.output_tokens, Some(20));
    }

    #[gpui::test]
    async fn test_default_agent_task_provider_update_title_unsupported(
        cx: &mut gpui::TestAppContext,
    ) {
        struct DefaultProvider;
        impl AgentTaskProvider for DefaultProvider {
            fn server_id(&self) -> ContextServerId {
                ContextServerId("dummy".into())
            }
            fn fetch_graph(&self, _cx: &mut App) -> Task<Result<AgentTaskGraph>> {
                Task::ready(Err(anyhow::anyhow!("not implemented")))
            }
            fn get_task(&self, _id: &AgentTaskId, _cx: &mut App) -> Task<Result<AgentTaskDetail>> {
                Task::ready(Err(anyhow::anyhow!("not implemented")))
            }
            fn complete_task(&self, _id: &AgentTaskId, _cx: &mut App) -> Task<Result<()>> {
                Task::ready(Err(anyhow::anyhow!("not implemented")))
            }
            fn fail_task(
                &self,
                _id: &AgentTaskId,
                _reason: &str,
                _cx: &mut App,
            ) -> Task<Result<()>> {
                Task::ready(Err(anyhow::anyhow!("not implemented")))
            }
            fn archive_task(&self, _id: &AgentTaskId, _cx: &mut App) -> Task<Result<()>> {
                Task::ready(Err(anyhow::anyhow!("not implemented")))
            }
            fn unarchive_task(&self, _id: &AgentTaskId, _cx: &mut App) -> Task<Result<()>> {
                Task::ready(Err(anyhow::anyhow!("not implemented")))
            }
            fn delete_task(&self, _id: &AgentTaskId, _cx: &mut App) -> Task<Result<()>> {
                Task::ready(Err(anyhow::anyhow!("not implemented")))
            }
            fn list_artifacts(
                &self,
                _task_id: &AgentTaskId,
                _cx: &mut App,
            ) -> Task<Result<Vec<AgentTaskArtifact>>> {
                Task::ready(Err(anyhow::anyhow!("not implemented")))
            }
            fn get_artifact(
                &self,
                _artifact_id: &str,
                _cx: &mut App,
            ) -> Task<Result<AgentTaskArtifact>> {
                Task::ready(Err(anyhow::anyhow!("not implemented")))
            }
        }

        let provider = DefaultProvider;
        let task_res = cx
            .update(|cx| provider.update_task_title(&AgentTaskId::from("TASK-1"), "New Title", cx))
            .await;
        assert_eq!(
            task_res.unwrap_err().to_string(),
            "update_task_title not supported"
        );

        let goal_res = cx
            .update(|cx| provider.update_goal_title("GOAL-1", "New Title", cx))
            .await;
        assert_eq!(
            goal_res.unwrap_err().to_string(),
            "update_goal_title not supported"
        );
    }
}
