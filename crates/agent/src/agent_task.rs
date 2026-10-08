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
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AgentItemKind {
    Goal,
    Task,
    Other(SharedString),
}

impl AgentItemKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Goal => "goal",
            Self::Task => "task",
            Self::Other(custom) => custom.as_ref(),
        }
    }
}

impl Serialize for AgentItemKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AgentItemKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(match s.to_ascii_lowercase().as_str() {
            "goal" => Self::Goal,
            "task" => Self::Task,
            _ => Self::Other(SharedString::from(s)),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AgentGoalProgress {
    pub done: u64,
    pub total: u64,
}

impl<'de> Deserialize<'de> for AgentGoalProgress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let done = value
            .get("done")
            .or_else(|| value.get("tasks_done"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let total = value
            .get("total")
            .or_else(|| value.get("tasks_total"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        Ok(Self { done, total })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentUnifiedItem {
    pub kind: AgentItemKind,
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub priority: i64,
    #[serde(default, deserialize_with = "deserialize_created_at")]
    pub created_at: Option<u64>,
    #[serde(default)]
    pub failure_reason: Option<String>,
    #[serde(default)]
    pub progress: Option<AgentGoalProgress>,
    #[serde(default)]
    pub assignee: Option<SharedString>,
    #[serde(default)]
    pub assigned_profile: Option<SharedString>,
    #[serde(default)]
    pub model: Option<SharedString>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub goal_id: Option<String>,
}

impl AgentUnifiedItem {
    pub fn task_status(&self) -> AgentTaskStatus {
        AgentTaskStatus::from_str(&self.status)
    }

    pub fn goal_status(&self) -> AgentGoalStatus {
        AgentGoalStatus::from_str(&self.status)
    }
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
    fn ensure_task(&self, id: &AgentTaskId, title: &str, cx: &mut App) -> Task<Result<()>> {
        let _ = id;
        let _ = title;
        let _ = cx;
        Task::ready(Ok(()))
    }
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
    fn fetch_items(&self, cx: &mut App) -> Task<Result<Vec<AgentUnifiedItem>>> {
        let _ = cx;
        Task::ready(Err(anyhow::anyhow!("item_list not supported")))
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
    fn test_unified_item_tolerant_parsing() {
        let json_data = json!({
            "kind": "task",
            "id": "TASK-1",
            "title": "A task item",
            "status": "cancelled",
            "priority": 10,
            "created_at": 1710000000000u64,
            "failure_reason": "something went wrong",
            "progress": {
                "done": 1,
                "total": 2
            },
            "assigned_profile": "coder",
            "model": "gpt-4",
            "parent_id": "TASK-0",
            "goal_id": "GOAL-1"
        });

        let item: AgentUnifiedItem = serde_json::from_value(json_data).unwrap();
        assert_eq!(item.kind, AgentItemKind::Task);
        assert_eq!(item.id, "TASK-1");
        assert_eq!(item.title, "A task item");
        assert_eq!(item.status, "cancelled");
        assert_eq!(item.task_status(), AgentTaskStatus::Cancelled);
        assert_eq!(item.priority, 10);
        assert_eq!(item.created_at, Some(1710000000000));
        assert_eq!(
            item.failure_reason,
            Some("something went wrong".to_string())
        );
        assert_eq!(item.progress, Some(AgentGoalProgress { done: 1, total: 2 }));
        assert_eq!(item.assignee, None);
        assert_eq!(item.assigned_profile, Some(SharedString::from("coder")));
        assert_eq!(item.model, Some(SharedString::from("gpt-4")));
        assert_eq!(item.parent_id, Some("TASK-0".to_string()));
        assert_eq!(item.goal_id, Some("GOAL-1".to_string()));

        // RFC3339 string created_at and tasks_done/tasks_total progress shape
        let json_rfc3339 = json!({
            "kind": "goal",
            "id": "GOAL-2",
            "title": "A goal item",
            "status": "active",
            "created_at": "2024-03-09T12:00:00Z",
            "progress": {
                "tasks_done": 3,
                "tasks_total": 5
            },
            "extra_field_should_be_ignored": true
        });

        let item2: AgentUnifiedItem = serde_json::from_value(json_rfc3339).unwrap();
        assert_eq!(item2.kind, AgentItemKind::Goal);
        assert_eq!(item2.id, "GOAL-2");
        assert_eq!(item2.goal_status(), AgentGoalStatus::Running);
        assert_eq!(item2.priority, 0); // default
        assert!(item2.created_at.is_some());
        assert_eq!(
            item2.progress,
            Some(AgentGoalProgress { done: 3, total: 5 })
        );
        assert_eq!(item2.failure_reason, None);
        assert_eq!(item2.assignee, None);
        assert_eq!(item2.assigned_profile, None);
        assert_eq!(item2.model, None);

        // Minimal item with null/absent fields
        let json_minimal = json!({
            "kind": "other_kind",
            "id": "ITEM-3",
            "created_at": null,
            "progress": null
        });

        let item3: AgentUnifiedItem = serde_json::from_value(json_minimal).unwrap();
        assert_eq!(
            item3.kind,
            AgentItemKind::Other(SharedString::from("other_kind"))
        );
        assert_eq!(item3.id, "ITEM-3");
        assert_eq!(item3.title, "");
        assert_eq!(item3.created_at, None);
        assert_eq!(item3.progress, None);
    }

    #[test]
    fn test_parse_real_item_list_payload() {
        let sample = r#"{
  "items": [
    {
      "id": "TASK-2", "kind": "task", "project_id": "my-project", "parent_id": "TASK-1", "goal_id": "GOAL-1",
      "title": "Реализация репозитория БД", "description": "Создать схемы и миграции", "type": "feature",
      "status": "running", "priority": 2, "assignee": "repository", "model": "claude-3-7-sonnet",
      "created_at": "2026-10-07T12:05:00Z", "updated_at": "2026-10-07T12:06:00Z", "metadata": {}
    },
    {
      "id": "TASK-1", "kind": "task", "project_id": "my-project", "goal_id": "GOAL-1",
      "title": "Архитектура бекенда", "description": "Спроектировать сервис", "type": "architecture",
      "status": "ready", "priority": 1, "assignee": "backend", "model": "gemini-2.5-pro",
      "created_at": "2026-10-07T12:00:00Z", "updated_at": "2026-10-07T12:00:00Z", "metadata": {}
    },
    {
      "id": "GOAL-1", "kind": "goal", "project_id": "my-project", "title": "Релиз модуля оплаты",
      "description": "Запуск платежного шлюза", "status": "running", "priority": 1,
      "progress": { "tasks_total": 2, "tasks_done": 0 },
      "created_at": "2026-10-07T11:50:00Z", "updated_at": "2026-10-07T11:50:00Z", "metadata": {}
    }
  ]
}"#;

        #[derive(Deserialize)]
        struct ItemListResponse {
            items: Vec<AgentUnifiedItem>,
        }

        let parsed: ItemListResponse =
            serde_json::from_str(sample).expect("failed to parse real item_list payload");
        assert_eq!(parsed.items.len(), 3);

        let task2 = &parsed.items[0];
        assert_eq!(task2.id, "TASK-2");
        assert_eq!(task2.kind, AgentItemKind::Task);
        assert_eq!(task2.parent_id.as_deref(), Some("TASK-1"));
        assert_eq!(task2.goal_id.as_deref(), Some("GOAL-1"));
        assert_eq!(task2.assignee, Some(SharedString::from("repository")));
        assert_eq!(task2.model, Some(SharedString::from("claude-3-7-sonnet")));
        assert_eq!(task2.created_at, Some(1791374700000));

        let task1 = &parsed.items[1];
        assert_eq!(task1.id, "TASK-1");
        assert_eq!(task1.kind, AgentItemKind::Task);
        assert_eq!(task1.goal_id.as_deref(), Some("GOAL-1"));
        assert_eq!(task1.assignee, Some(SharedString::from("backend")));
        assert_eq!(task1.model, Some(SharedString::from("gemini-2.5-pro")));
        assert_eq!(task1.created_at, Some(1791374400000));

        let goal1 = &parsed.items[2];
        assert_eq!(goal1.id, "GOAL-1");
        assert_eq!(goal1.kind, AgentItemKind::Goal);
        assert_eq!(goal1.title, "Релиз модуля оплаты");
        let progress = goal1.progress.as_ref().expect("missing goal progress");
        assert_eq!(progress.total, 2);
        assert_eq!(progress.done, 0);
        assert_eq!(goal1.created_at, Some(1791373800000));
    }

    #[test]
    fn test_created_at_rfc3339_with_offset() {
        let json_with_offset = r#"{
            "id": "TASK-OFFSET",
            "kind": "task",
            "title": "Offset Task",
            "status": "ready",
            "priority": 1,
            "created_at": "2026-10-07T15:05:00+03:00"
        }"#;

        let item: AgentUnifiedItem = serde_json::from_str(json_with_offset)
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
            }
        );

        // Roundtrip serialize/deserialize
        let serialized = serde_json::to_string(&graph_new).unwrap();
        let deserialized: AgentTaskGraph = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, graph_new);
    }
}
