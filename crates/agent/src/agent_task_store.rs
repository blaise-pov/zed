use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use gpui::{App, Context, Task};
use util::ResultExt;

use crate::agent_task::{
    AgentGoalStatus, AgentGoalSummary, AgentItemKind, AgentTaskArtifact, AgentTaskDetail,
    AgentTaskEvent, AgentTaskEventKind, AgentTaskGraph, AgentTaskId, AgentTaskProvider,
    AgentTaskStatus, AgentTaskSummary, AgentUnifiedItem,
};

fn merge_items_into_graph(graph: &mut AgentTaskGraph, items: &[AgentUnifiedItem]) {
    let items_by_id: std::collections::HashMap<&str, &AgentUnifiedItem> =
        items.iter().map(|item| (item.id.as_str(), item)).collect();

    for task in &mut graph.tasks {
        if let Some(item) = items_by_id.get(task.id.as_str()) {
            if task.created_at.is_none() {
                task.created_at = item.created_at;
            }
            if task.parent_id.is_none() {
                task.parent_id = item.parent_id.as_deref().map(AgentTaskId::from);
            }
            if task.goal_id.is_none() {
                task.goal_id = item.goal_id.clone();
            }
            if task.title.is_empty() {
                task.title = item.title.clone();
            }
            let item_profile = item
                .assigned_profile
                .clone()
                .or_else(|| item.assignee.clone());
            if (task.assigned_profile.is_none()
                || task.assigned_profile.as_ref().is_some_and(|p| p.is_empty()))
                && item_profile.is_some()
            {
                task.assigned_profile = item_profile;
            }
            if task.model.is_none() {
                task.model = item.model.clone();
            }
        }
    }

    let mut existing_task_ids: std::collections::HashSet<AgentTaskId> =
        graph.tasks.iter().map(|t| t.id.clone()).collect();

    for item in items.iter().filter(|item| item.kind == AgentItemKind::Task) {
        let task_id = AgentTaskId::from(item.id.as_str());
        if !existing_task_ids.contains(&task_id) {
            existing_task_ids.insert(task_id.clone());
            let item_profile = item
                .assigned_profile
                .clone()
                .or_else(|| item.assignee.clone());
            graph.tasks.push(AgentTaskSummary {
                id: task_id,
                parent_id: item.parent_id.as_deref().map(AgentTaskId::from),
                goal_id: item.goal_id.clone(),
                title: item.title.clone(),
                status: item.task_status(),
                attempt: 1,
                assignee: item.assignee.clone(),
                write_scopes: Vec::new(),
                created_at: item.created_at,
                assigned_profile: item_profile,
                model: item.model.clone(),
            });
        }
    }

    for goal in &mut graph.goals {
        if let Some(item) = items_by_id.get(goal.goal_id.as_str()) {
            if goal.created_at.is_none() {
                goal.created_at = item.created_at;
            }
            if goal.tasks_total == 0 {
                if let Some(progress) = &item.progress {
                    goal.tasks_total = progress.total;
                    goal.tasks_done = progress.done;
                }
            }
        }
    }

    if graph.goals.is_empty() {
        for item in items.iter().filter(|item| item.kind == AgentItemKind::Goal) {
            let (tasks_done, tasks_total) = match &item.progress {
                Some(progress) => (progress.done, progress.total),
                None => (0, 0),
            };
            graph.goals.push(AgentGoalSummary {
                goal_id: item.id.clone(),
                title: item.title.clone(),
                status: item.goal_status(),
                priority: item.priority,
                tasks_total,
                tasks_done,
                created_at: item.created_at,
            });
        }
    }
}

pub struct AgentTaskStore {
    provider: Arc<dyn AgentTaskProvider>,
    graph: AgentTaskGraph,
    events: VecDeque<AgentTaskEvent>,
    is_offline: bool,
    last_error: Option<String>,
    _poll_task: Option<Task<()>>,
}

impl AgentTaskStore {
    pub fn new(provider: Arc<dyn AgentTaskProvider>, cx: &mut Context<Self>) -> Self {
        let mut store = Self {
            provider,
            graph: AgentTaskGraph::default(),
            events: VecDeque::new(),
            is_offline: false,
            last_error: None,
            _poll_task: None,
        };
        store.start_polling(cx);
        store
    }

    pub fn provider(&self) -> &Arc<dyn AgentTaskProvider> {
        &self.provider
    }

    /// Swap the backing provider (e.g. when the configured task server id
    /// changes) and refresh from the new server.
    pub fn set_provider(&mut self, provider: Arc<dyn AgentTaskProvider>, cx: &mut Context<Self>) {
        self.provider = provider;
        self.graph = AgentTaskGraph::default();
        self.events.clear();
        self.is_offline = false;
        self.last_error = None;
        self.refresh(cx).detach();
        cx.notify();
    }

    pub fn graph(&self) -> &AgentTaskGraph {
        &self.graph
    }

    pub fn events(&self) -> &VecDeque<AgentTaskEvent> {
        &self.events
    }

    pub fn is_offline(&self) -> bool {
        self.is_offline
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn start_polling(&mut self, cx: &mut Context<Self>) {
        let poll_task = cx.spawn(async move |this, cx| {
            loop {
                let refresh_task = this.update(cx, |store, cx| store.refresh(cx));
                if let Ok(task) = refresh_task {
                    task.await.log_err();
                }
                cx.background_executor().timer(Duration::from_secs(5)).await;
            }
        });
        self._poll_task = Some(poll_task);
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let provider = self.provider.clone();
        let graph_task = provider.fetch_graph(cx);
        let items_task = provider.fetch_items(cx);
        let events_task = provider.list_events(200, cx);

        cx.spawn(async move |this, cx| {
            let (graph_result, items_result, events_result) =
                futures::join!(graph_task, items_task, events_task);

            this.update(cx, |store, cx| {
                match graph_result {
                    Ok(mut graph) => {
                        match items_result {
                            Ok(items) => {
                                merge_items_into_graph(&mut graph, &items);
                            }
                            Err(err) => {
                                log::warn!("failed to fetch unified items: {err:?}");
                            }
                        }

                        match events_result {
                            Ok(events) => {
                                store.events = events.into();
                            }
                            Err(err) => {
                                log::error!("failed to fetch events: {err:?}");
                            }
                        }

                        store.graph = graph;
                        store.is_offline = false;
                        store.last_error = None;
                    }
                    Err(err) => {
                        store.is_offline = true;
                        store.last_error = Some(err.to_string());
                    }
                }
                cx.notify();
            })
            .log_err();
            Ok(())
        })
    }

    pub fn policy_denied_event_for_task(&self, task_id: &AgentTaskId) -> Option<&AgentTaskEvent> {
        self.events.iter().find(|e| {
            e.kind == AgentTaskEventKind::PolicyDenied && e.task_id.as_ref() == Some(task_id)
        })
    }

    pub fn complete_task(&mut self, id: &AgentTaskId, cx: &mut Context<Self>) -> Task<Result<()>> {
        let provider = self.provider.clone();
        let task = provider.complete_task(id, cx);
        cx.spawn(async move |this, cx| {
            task.await?;
            let refresh_task = this.update(cx, |store, cx| store.refresh(cx))?;
            refresh_task.await?;
            Ok(())
        })
    }

    pub fn ensure_task(
        &mut self,
        id: &AgentTaskId,
        title: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let provider = self.provider.clone();
        let task = provider.ensure_task(id, title, cx);
        cx.spawn(async move |this, cx| {
            task.await?;
            let refresh_task = this.update(cx, |store, cx| store.refresh(cx))?;
            refresh_task.await?;
            Ok(())
        })
    }

    pub fn fail_task(
        &mut self,
        id: &AgentTaskId,
        reason: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let provider = self.provider.clone();
        let task = provider.fail_task(id, reason, cx);
        cx.spawn(async move |this, cx| {
            task.await?;
            let refresh_task = this.update(cx, |store, cx| store.refresh(cx))?;
            refresh_task.await?;
            Ok(())
        })
    }

    pub fn archive_task(&mut self, id: &AgentTaskId, cx: &mut Context<Self>) -> Task<Result<()>> {
        let provider = self.provider.clone();
        let task = provider.archive_task(id, cx);
        cx.spawn(async move |this, cx| {
            task.await?;
            let refresh_task = this.update(cx, |store, cx| store.refresh(cx))?;
            refresh_task.await?;
            Ok(())
        })
    }

    pub fn unarchive_task(&mut self, id: &AgentTaskId, cx: &mut Context<Self>) -> Task<Result<()>> {
        let provider = self.provider.clone();
        let task = provider.unarchive_task(id, cx);
        cx.spawn(async move |this, cx| {
            task.await?;
            let refresh_task = this.update(cx, |store, cx| store.refresh(cx))?;
            refresh_task.await?;
            Ok(())
        })
    }

    pub fn delete_task(&mut self, id: &AgentTaskId, cx: &mut Context<Self>) -> Task<Result<()>> {
        let provider = self.provider.clone();
        let task = provider.delete_task(id, cx);
        cx.spawn(async move |this, cx| {
            task.await?;
            let refresh_task = this.update(cx, |store, cx| store.refresh(cx))?;
            refresh_task.await?;
            Ok(())
        })
    }

    pub fn set_task_status(
        &mut self,
        id: &AgentTaskId,
        status: &AgentTaskStatus,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let provider = self.provider.clone();
        let task = provider.set_task_status(id, status, cx);
        cx.spawn(async move |this, cx| {
            task.await?;
            let refresh_task = this.update(cx, |store, cx| store.refresh(cx))?;
            refresh_task.await?;
            Ok(())
        })
    }

    pub fn set_goal_status(
        &mut self,
        goal_id: &str,
        status: &AgentGoalStatus,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let provider = self.provider.clone();
        let task = provider.set_goal_status(goal_id, status, cx);
        cx.spawn(async move |this, cx| {
            task.await?;
            let refresh_task = this.update(cx, |store, cx| store.refresh(cx))?;
            refresh_task.await?;
            Ok(())
        })
    }

    pub fn archive_goal(&mut self, goal_id: &str, cx: &mut Context<Self>) -> Task<Result<()>> {
        let provider = self.provider.clone();
        let task = provider.archive_goal(goal_id, cx);
        cx.spawn(async move |this, cx| {
            task.await?;
            let refresh_task = this.update(cx, |store, cx| store.refresh(cx))?;
            refresh_task.await?;
            Ok(())
        })
    }

    pub fn unarchive_goal(&mut self, goal_id: &str, cx: &mut Context<Self>) -> Task<Result<()>> {
        let provider = self.provider.clone();
        let task = provider.unarchive_goal(goal_id, cx);
        cx.spawn(async move |this, cx| {
            task.await?;
            let refresh_task = this.update(cx, |store, cx| store.refresh(cx))?;
            refresh_task.await?;
            Ok(())
        })
    }

    pub fn delete_goal(&mut self, goal_id: &str, cx: &mut Context<Self>) -> Task<Result<()>> {
        let provider = self.provider.clone();
        let task = provider.delete_goal(goal_id, cx);
        cx.spawn(async move |this, cx| {
            task.await?;
            let refresh_task = this.update(cx, |store, cx| store.refresh(cx))?;
            refresh_task.await?;
            Ok(())
        })
    }

    pub fn get_task_detail(&self, id: &AgentTaskId, cx: &mut App) -> Task<Result<AgentTaskDetail>> {
        self.provider.get_task(id, cx)
    }

    pub fn list_artifacts(
        &self,
        task_id: &AgentTaskId,
        cx: &mut App,
    ) -> Task<Result<Vec<AgentTaskArtifact>>> {
        self.provider.list_artifacts(task_id, cx)
    }

    pub fn get_artifact(&self, artifact_id: &str, cx: &mut App) -> Task<Result<AgentTaskArtifact>> {
        self.provider.get_artifact(artifact_id, cx)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::agent_task::AgentTaskStatus;
    use crate::agent_task::AgentTaskSummary;
    use context_server::ContextServerId;
    use gpui::{App, AppContext, SharedString, TestAppContext};

    #[derive(Default)]
    struct CallCounts {
        archive_calls: Vec<AgentTaskId>,
        unarchive_calls: Vec<AgentTaskId>,
        delete_calls: Vec<AgentTaskId>,
        ensure_task_calls: Vec<(AgentTaskId, String)>,
        set_task_status_calls: Vec<(AgentTaskId, AgentTaskStatus)>,
        set_goal_status_calls: Vec<(String, AgentGoalStatus)>,
        archive_goal_calls: Vec<String>,
        unarchive_goal_calls: Vec<String>,
        delete_goal_calls: Vec<String>,
        fetch_graph_count: usize,
        fetch_items_count: usize,
    }

    #[derive(Default)]
    struct TestProvider {
        should_fail: bool,
        items_fail: bool,
        items: Vec<AgentUnifiedItem>,
        tasks: Vec<AgentTaskSummary>,
        goals: Vec<AgentGoalSummary>,
        calls: Arc<Mutex<CallCounts>>,
    }

    impl AgentTaskProvider for TestProvider {
        fn server_id(&self) -> ContextServerId {
            ContextServerId("test".into())
        }

        fn fetch_graph(&self, _cx: &mut App) -> Task<Result<AgentTaskGraph>> {
            self.calls.lock().unwrap().fetch_graph_count += 1;
            if self.should_fail {
                Task::ready(Err(anyhow::anyhow!("offline")))
            } else {
                let tasks = if self.tasks.is_empty() {
                    vec![AgentTaskSummary {
                        id: AgentTaskId::from("TASK-1"),
                        parent_id: None,
                        goal_id: None,
                        title: "Test Task".to_string(),
                        status: AgentTaskStatus::Ready,
                        attempt: 1,
                        assignee: None,
                        write_scopes: vec![],
                        created_at: None,
                        assigned_profile: None,
                        model: None,
                    }]
                } else {
                    self.tasks.clone()
                };
                Task::ready(Ok(AgentTaskGraph {
                    tasks,
                    goals: self.goals.clone(),
                }))
            }
        }

        fn fetch_items(&self, _cx: &mut App) -> Task<Result<Vec<AgentUnifiedItem>>> {
            self.calls.lock().unwrap().fetch_items_count += 1;
            if self.items_fail {
                Task::ready(Err(anyhow::anyhow!("items failed")))
            } else {
                Task::ready(Ok(self.items.clone()))
            }
        }

        fn set_task_status(
            &self,
            id: &AgentTaskId,
            status: &AgentTaskStatus,
            _cx: &mut App,
        ) -> Task<Result<()>> {
            self.calls
                .lock()
                .unwrap()
                .set_task_status_calls
                .push((id.clone(), status.clone()));
            Task::ready(Ok(()))
        }

        fn set_goal_status(
            &self,
            goal_id: &str,
            status: &AgentGoalStatus,
            _cx: &mut App,
        ) -> Task<Result<()>> {
            self.calls
                .lock()
                .unwrap()
                .set_goal_status_calls
                .push((goal_id.to_string(), status.clone()));
            Task::ready(Ok(()))
        }

        fn archive_goal(&self, goal_id: &str, _cx: &mut App) -> Task<Result<()>> {
            self.calls
                .lock()
                .unwrap()
                .archive_goal_calls
                .push(goal_id.to_string());
            Task::ready(Ok(()))
        }

        fn unarchive_goal(&self, goal_id: &str, _cx: &mut App) -> Task<Result<()>> {
            self.calls
                .lock()
                .unwrap()
                .unarchive_goal_calls
                .push(goal_id.to_string());
            Task::ready(Ok(()))
        }

        fn delete_goal(&self, goal_id: &str, _cx: &mut App) -> Task<Result<()>> {
            self.calls
                .lock()
                .unwrap()
                .delete_goal_calls
                .push(goal_id.to_string());
            Task::ready(Ok(()))
        }

        fn get_task(&self, _id: &AgentTaskId, _cx: &mut App) -> Task<Result<AgentTaskDetail>> {
            Task::ready(Err(anyhow::anyhow!("not implemented")))
        }

        fn complete_task(&self, _id: &AgentTaskId, _cx: &mut App) -> Task<Result<()>> {
            Task::ready(Ok(()))
        }

        fn ensure_task(&self, id: &AgentTaskId, title: &str, _cx: &mut App) -> Task<Result<()>> {
            self.calls
                .lock()
                .unwrap()
                .ensure_task_calls
                .push((id.clone(), title.to_string()));
            if self.should_fail {
                Task::ready(Err(anyhow::anyhow!("offline")))
            } else {
                Task::ready(Ok(()))
            }
        }

        fn fail_task(&self, _id: &AgentTaskId, _reason: &str, _cx: &mut App) -> Task<Result<()>> {
            Task::ready(Ok(()))
        }

        fn archive_task(&self, id: &AgentTaskId, _cx: &mut App) -> Task<Result<()>> {
            self.calls.lock().unwrap().archive_calls.push(id.clone());
            Task::ready(Ok(()))
        }

        fn unarchive_task(&self, id: &AgentTaskId, _cx: &mut App) -> Task<Result<()>> {
            self.calls.lock().unwrap().unarchive_calls.push(id.clone());
            Task::ready(Ok(()))
        }

        fn delete_task(&self, id: &AgentTaskId, _cx: &mut App) -> Task<Result<()>> {
            self.calls.lock().unwrap().delete_calls.push(id.clone());
            Task::ready(Ok(()))
        }

        fn list_events(&self, _limit: u32, _cx: &mut App) -> Task<Result<Vec<AgentTaskEvent>>> {
            Task::ready(Ok(vec![]))
        }

        fn list_artifacts(
            &self,
            _task_id: &AgentTaskId,
            _cx: &mut App,
        ) -> Task<Result<Vec<AgentTaskArtifact>>> {
            Task::ready(Ok(vec![]))
        }

        fn get_artifact(
            &self,
            _artifact_id: &str,
            _cx: &mut App,
        ) -> Task<Result<AgentTaskArtifact>> {
            Task::ready(Err(anyhow::anyhow!("not implemented")))
        }
    }

    #[gpui::test]
    async fn test_agent_task_store_success(cx: &mut TestAppContext) {
        let provider = Arc::new(TestProvider {
            should_fail: false,
            ..Default::default()
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        cx.run_until_parked();

        store.update(cx, |store, _cx| {
            assert!(!store.is_offline());
            assert_eq!(store.graph().tasks.len(), 1);
            assert_eq!(store.graph().tasks[0].id.0.as_ref(), "TASK-1");
        });
    }

    #[gpui::test]
    async fn test_agent_task_store_offline(cx: &mut TestAppContext) {
        let provider = Arc::new(TestProvider {
            should_fail: true,
            ..Default::default()
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        cx.run_until_parked();

        store.update(cx, |store, _cx| {
            assert!(store.is_offline());
            assert_eq!(store.last_error(), Some("offline"));
        });
    }

    #[gpui::test]
    async fn test_agent_task_store_archive_task(cx: &mut TestAppContext) {
        let calls = Arc::new(Mutex::new(CallCounts::default()));
        let provider = Arc::new(TestProvider {
            should_fail: false,
            calls: calls.clone(),
            ..Default::default()
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));
        cx.run_until_parked();

        let initial_refreshes = calls.lock().unwrap().fetch_graph_count;
        let task_id = AgentTaskId::from("TASK-1");
        let result = store
            .update(cx, |store, cx| store.archive_task(&task_id, cx))
            .await;
        assert!(result.is_ok());

        let counts = calls.lock().unwrap();
        assert_eq!(counts.archive_calls, vec![task_id]);
        assert_eq!(counts.fetch_graph_count, initial_refreshes + 1);
    }

    #[gpui::test]
    async fn test_agent_task_store_unarchive_task(cx: &mut TestAppContext) {
        let calls = Arc::new(Mutex::new(CallCounts::default()));
        let provider = Arc::new(TestProvider {
            should_fail: false,
            calls: calls.clone(),
            ..Default::default()
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));
        cx.run_until_parked();

        let initial_refreshes = calls.lock().unwrap().fetch_graph_count;
        let task_id = AgentTaskId::from("TASK-1");
        let result = store
            .update(cx, |store, cx| store.unarchive_task(&task_id, cx))
            .await;
        assert!(result.is_ok());

        let counts = calls.lock().unwrap();
        assert_eq!(counts.unarchive_calls, vec![task_id]);
        assert_eq!(counts.fetch_graph_count, initial_refreshes + 1);
    }

    #[gpui::test]
    async fn test_agent_task_store_delete_task(cx: &mut TestAppContext) {
        let calls = Arc::new(Mutex::new(CallCounts::default()));
        let provider = Arc::new(TestProvider {
            should_fail: false,
            calls: calls.clone(),
            ..Default::default()
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));
        cx.run_until_parked();

        let initial_refreshes = calls.lock().unwrap().fetch_graph_count;
        let task_id = AgentTaskId::from("TASK-1");
        let result = store
            .update(cx, |store, cx| store.delete_task(&task_id, cx))
            .await;
        assert!(result.is_ok());

        let counts = calls.lock().unwrap();
        assert_eq!(counts.delete_calls, vec![task_id]);
        assert_eq!(counts.fetch_graph_count, initial_refreshes + 1);
    }

    #[gpui::test]
    async fn test_agent_task_store_ensure_task(cx: &mut TestAppContext) {
        let calls = Arc::new(Mutex::new(CallCounts::default()));
        let provider = Arc::new(TestProvider {
            should_fail: false,
            calls: calls.clone(),
            ..Default::default()
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));
        cx.run_until_parked();

        let initial_refreshes = calls.lock().unwrap().fetch_graph_count;
        let task_id = AgentTaskId::from("TASK-1");
        let result = store
            .update(cx, |store, cx| {
                store.ensure_task(&task_id, "Test Title", cx)
            })
            .await;
        assert!(result.is_ok());

        let counts = calls.lock().unwrap();
        assert_eq!(
            counts.ensure_task_calls,
            vec![(task_id, "Test Title".to_string())]
        );
        assert_eq!(counts.fetch_graph_count, initial_refreshes + 1);
    }

    #[gpui::test]
    async fn test_agent_task_store_set_task_status(cx: &mut TestAppContext) {
        let calls = Arc::new(Mutex::new(CallCounts::default()));
        let provider = Arc::new(TestProvider {
            calls: calls.clone(),
            ..Default::default()
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));
        cx.run_until_parked();

        let initial_refreshes = calls.lock().unwrap().fetch_graph_count;
        let task_id = AgentTaskId::from("TASK-1");
        let result = store
            .update(cx, |store, cx| {
                store.set_task_status(&task_id, &AgentTaskStatus::Running, cx)
            })
            .await;
        assert!(result.is_ok());

        let counts = calls.lock().unwrap();
        assert_eq!(
            counts.set_task_status_calls,
            vec![(task_id, AgentTaskStatus::Running)]
        );
        assert_eq!(counts.fetch_graph_count, initial_refreshes + 1);
    }

    #[gpui::test]
    async fn test_agent_task_store_goal_actions(cx: &mut TestAppContext) {
        let calls = Arc::new(Mutex::new(CallCounts::default()));
        let provider = Arc::new(TestProvider {
            calls: calls.clone(),
            ..Default::default()
        });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));
        cx.run_until_parked();

        let initial_refreshes = calls.lock().unwrap().fetch_graph_count;

        let result = store
            .update(cx, |store, cx| {
                store.set_goal_status("GOAL-1", &AgentGoalStatus::Completed, cx)
            })
            .await;
        assert!(result.is_ok());

        let result = store
            .update(cx, |store, cx| store.archive_goal("GOAL-1", cx))
            .await;
        assert!(result.is_ok());

        let result = store
            .update(cx, |store, cx| store.unarchive_goal("GOAL-1", cx))
            .await;
        assert!(result.is_ok());

        let result = store
            .update(cx, |store, cx| store.delete_goal("GOAL-1", cx))
            .await;
        assert!(result.is_ok());

        let counts = calls.lock().unwrap();
        assert_eq!(
            counts.set_goal_status_calls,
            vec![("GOAL-1".to_string(), AgentGoalStatus::Completed)]
        );
        assert_eq!(counts.archive_goal_calls, vec!["GOAL-1".to_string()]);
        assert_eq!(counts.unarchive_goal_calls, vec!["GOAL-1".to_string()]);
        assert_eq!(counts.delete_goal_calls, vec!["GOAL-1".to_string()]);
        assert_eq!(counts.fetch_graph_count, initial_refreshes + 4);
    }

    #[gpui::test]
    async fn test_agent_task_store_merge_items(cx: &mut TestAppContext) {
        use crate::agent_task::AgentGoalProgress;
        use gpui::SharedString;

        let initial_task = AgentTaskSummary {
            id: AgentTaskId::from("TASK-1"),
            parent_id: None,
            goal_id: Some("GOAL-1".to_string()),
            title: "Task Title From Graph".to_string(),
            status: AgentTaskStatus::Running,
            attempt: 1,
            assignee: None,
            write_scopes: vec![],
            created_at: None,
            assigned_profile: None,
            model: None,
        };

        let initial_goal = AgentGoalSummary {
            goal_id: "GOAL-1".to_string(),
            title: "Goal Title From Graph".to_string(),
            status: AgentGoalStatus::Running,
            priority: 2,
            tasks_total: 0,
            tasks_done: 0,
            created_at: None,
        };

        let items = vec![
            AgentUnifiedItem {
                kind: AgentItemKind::Task,
                id: "TASK-1".to_string(),
                title: "Task Title From Item".to_string(),
                status: "completed".to_string(), // graph status should win
                priority: 5,
                created_at: Some(1710000000000),
                failure_reason: None,
                progress: None,
                assignee: None,
                assigned_profile: Some(SharedString::from("code_mechanic")),
                model: Some(SharedString::from("gpt-4o")),
                parent_id: None,
                goal_id: None,
            },
            AgentUnifiedItem {
                kind: AgentItemKind::Goal,
                id: "GOAL-1".to_string(),
                title: "Goal Title From Item".to_string(),
                status: "blocked".to_string(),
                priority: 1,
                created_at: Some(1709999999000),
                failure_reason: None,
                progress: Some(AgentGoalProgress { done: 3, total: 5 }),
                assignee: None,
                assigned_profile: None,
                model: None,
                parent_id: None,
                goal_id: None,
            },
        ];

        let provider = Arc::new(TestProvider {
            tasks: vec![initial_task],
            goals: vec![initial_goal],
            items,
            ..Default::default()
        });

        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));
        cx.run_until_parked();

        store.update(cx, |store, _cx| {
            let task = &store.graph().tasks[0];
            // item fields filled
            assert_eq!(task.created_at, Some(1710000000000));
            assert_eq!(
                task.assigned_profile,
                Some(SharedString::from("code_mechanic"))
            );
            assert_eq!(task.model, Some(SharedString::from("gpt-4o")));
            // graph source of truth preserved
            assert_eq!(task.status, AgentTaskStatus::Running);
            assert_eq!(task.title, "Task Title From Graph");

            let goal = &store.graph().goals[0];
            assert_eq!(goal.created_at, Some(1709999999000));
            assert_eq!(goal.tasks_done, 3);
            assert_eq!(goal.tasks_total, 5);
            assert_eq!(goal.status, AgentGoalStatus::Running);
            assert_eq!(goal.title, "Goal Title From Graph");
        });
    }

    #[gpui::test]
    async fn test_agent_task_store_synthesize_goals_when_graph_goals_empty(
        cx: &mut TestAppContext,
    ) {
        use crate::agent_task::AgentGoalProgress;

        let items = vec![AgentUnifiedItem {
            kind: AgentItemKind::Goal,
            id: "GOAL-SYNTH".to_string(),
            title: "Synthesized Goal".to_string(),
            status: "running".to_string(),
            priority: 3,
            created_at: Some(1710000050000),
            failure_reason: None,
            progress: Some(AgentGoalProgress { done: 1, total: 2 }),
            assignee: None,
            assigned_profile: None,
            model: None,
            parent_id: None,
            goal_id: None,
        }];

        let provider = Arc::new(TestProvider {
            tasks: vec![],
            goals: vec![], // empty goals in graph
            items,
            ..Default::default()
        });

        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));
        cx.run_until_parked();

        store.update(cx, |store, _cx| {
            assert_eq!(store.graph().goals.len(), 1);
            let goal = &store.graph().goals[0];
            assert_eq!(goal.goal_id, "GOAL-SYNTH");
            assert_eq!(goal.title, "Synthesized Goal");
            assert_eq!(goal.status, AgentGoalStatus::Running);
            assert_eq!(goal.priority, 3);
            assert_eq!(goal.tasks_done, 1);
            assert_eq!(goal.tasks_total, 2);
            assert_eq!(goal.created_at, Some(1710000050000));
        });
    }

    #[gpui::test]
    async fn test_agent_task_store_items_failure_non_fatal(cx: &mut TestAppContext) {
        let provider = Arc::new(TestProvider {
            should_fail: false,
            items_fail: true,
            ..Default::default()
        });

        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));
        cx.run_until_parked();

        store.update(cx, |store, _cx| {
            assert!(!store.is_offline());
            assert_eq!(store.last_error(), None);
            assert_eq!(store.graph().tasks.len(), 1);
            assert_eq!(store.graph().tasks[0].id.as_str(), "TASK-1");
        });
    }

    #[test]
    fn test_agent_task_status_archived() {
        assert!(AgentTaskStatus::Archived.is_terminal());
        assert!(AgentTaskStatus::Completed.is_terminal());
        assert!(AgentTaskStatus::Failed.is_terminal());
        assert!(AgentTaskStatus::Cancelled.is_terminal());
        assert!(!AgentTaskStatus::Ready.is_terminal());
        assert!(!AgentTaskStatus::Running.is_terminal());

        let serialized = serde_json::to_string(&AgentTaskStatus::Archived).unwrap();
        assert_eq!(serialized, "\"archived\"");
        let deserialized: AgentTaskStatus = serde_json::from_str("\"archived\"").unwrap();
        assert_eq!(deserialized, AgentTaskStatus::Archived);
    }

    #[test]
    fn test_merge_items_assigned_profile_priority() {
        let mut graph = AgentTaskGraph {
            tasks: vec![
                AgentTaskSummary {
                    id: AgentTaskId::from("TASK-WITHOUT-PROFILE"),
                    parent_id: None,
                    goal_id: None,
                    title: "No Profile Task".to_string(),
                    status: AgentTaskStatus::Ready,
                    attempt: 0,
                    assignee: None,
                    write_scopes: vec![],
                    created_at: None,
                    assigned_profile: None,
                    model: None,
                },
                AgentTaskSummary {
                    id: AgentTaskId::from("TASK-WITH-EXISTING-PROFILE"),
                    parent_id: None,
                    goal_id: None,
                    title: "Existing Profile Task".to_string(),
                    status: AgentTaskStatus::Running,
                    attempt: 1,
                    assignee: None,
                    write_scopes: vec![],
                    created_at: None,
                    assigned_profile: Some(SharedString::from("senior_engineer")),
                    model: None,
                },
            ],
            goals: vec![],
        };

        let items = vec![
            AgentUnifiedItem {
                kind: AgentItemKind::Task,
                id: "TASK-WITHOUT-PROFILE".to_string(),
                title: "No Profile Task".to_string(),
                status: "ready".to_string(),
                priority: 1,
                created_at: Some(1791374700000),
                failure_reason: None,
                progress: None,
                assignee: Some(SharedString::from("repository")),
                assigned_profile: None,
                model: Some(SharedString::from("claude-3-7-sonnet")),
                parent_id: None,
                goal_id: None,
            },
            AgentUnifiedItem {
                kind: AgentItemKind::Task,
                id: "TASK-WITH-EXISTING-PROFILE".to_string(),
                title: "Existing Profile Task".to_string(),
                status: "running".to_string(),
                priority: 2,
                created_at: Some(1791374400000),
                failure_reason: None,
                progress: None,
                assignee: None,
                assigned_profile: None,
                model: None,
                parent_id: None,
                goal_id: None,
            },
        ];

        merge_items_into_graph(&mut graph, &items);

        let task_without_profile = &graph.tasks[0];
        assert_eq!(
            task_without_profile.assigned_profile,
            Some(SharedString::from("repository"))
        );
        assert_eq!(task_without_profile.created_at, Some(1791374700000));
        assert_eq!(
            task_without_profile.model,
            Some(SharedString::from("claude-3-7-sonnet"))
        );

        let task_with_profile = &graph.tasks[1];
        assert_eq!(
            task_with_profile.assigned_profile,
            Some(SharedString::from("senior_engineer"))
        );
    }

    #[test]
    fn test_merge_items_synthesizes_missing_tasks_and_syncs_parent_and_goal_id() {
        let mut graph = AgentTaskGraph {
            tasks: vec![AgentTaskSummary {
                id: AgentTaskId::from("TASK-EXISTING"),
                parent_id: None,
                goal_id: None,
                title: "Existing Task".to_string(),
                status: AgentTaskStatus::Running,
                attempt: 1,
                assignee: None,
                write_scopes: vec![],
                created_at: None,
                assigned_profile: None,
                model: None,
            }],
            goals: vec![],
        };

        let items = vec![
            AgentUnifiedItem {
                kind: AgentItemKind::Task,
                id: "TASK-EXISTING".to_string(),
                title: "Existing Task Title".to_string(),
                status: "running".to_string(),
                priority: 1,
                created_at: Some(100),
                failure_reason: None,
                progress: None,
                assignee: None,
                assigned_profile: None,
                model: None,
                parent_id: Some("TASK-PARENT".to_string()),
                goal_id: Some("GOAL-42".to_string()),
            },
            AgentUnifiedItem {
                kind: AgentItemKind::Task,
                id: "TASK-SYNTHESIZED".to_string(),
                title: "Synthesized Task".to_string(),
                status: "ready".to_string(),
                priority: 2,
                created_at: Some(200),
                failure_reason: None,
                progress: None,
                assignee: Some(SharedString::from("coder")),
                assigned_profile: Some(SharedString::from("code_mechanic")),
                model: Some(SharedString::from("claude-3-7-sonnet")),
                parent_id: Some("TASK-EXISTING".to_string()),
                goal_id: Some("GOAL-42".to_string()),
            },
        ];

        merge_items_into_graph(&mut graph, &items);

        let existing = &graph.tasks[0];
        assert_eq!(existing.parent_id, Some(AgentTaskId::from("TASK-PARENT")));
        assert_eq!(existing.goal_id, Some("GOAL-42".to_string()));

        assert_eq!(graph.tasks.len(), 2);
        let synthesized = &graph.tasks[1];
        assert_eq!(synthesized.id, AgentTaskId::from("TASK-SYNTHESIZED"));
        assert_eq!(synthesized.title, "Synthesized Task");
        assert_eq!(synthesized.status, AgentTaskStatus::Ready);
        assert_eq!(
            synthesized.parent_id,
            Some(AgentTaskId::from("TASK-EXISTING"))
        );
        assert_eq!(synthesized.goal_id, Some("GOAL-42".to_string()));
        assert_eq!(synthesized.created_at, Some(200));
        assert_eq!(
            synthesized.assigned_profile,
            Some(SharedString::from("code_mechanic"))
        );
        assert_eq!(
            synthesized.model,
            Some(SharedString::from("claude-3-7-sonnet"))
        );
    }

    #[test]
    fn test_merge_items_parent_id_goal_id_and_synthesize_tasks() {
        let mut graph = AgentTaskGraph {
            tasks: vec![AgentTaskSummary {
                id: AgentTaskId::from("TASK-EXISTING"),
                parent_id: None,
                goal_id: None,
                title: "".to_string(),
                status: AgentTaskStatus::Ready,
                attempt: 1,
                assignee: None,
                write_scopes: vec![],
                created_at: None,
                assigned_profile: None,
                model: None,
            }],
            goals: vec![],
        };

        let items = vec![
            AgentUnifiedItem {
                kind: AgentItemKind::Task,
                id: "TASK-EXISTING".to_string(),
                title: "Existing Task Title".to_string(),
                status: "running".to_string(), // graph status Ready should win
                priority: 2,
                created_at: Some(1791500000000),
                failure_reason: None,
                progress: None,
                assignee: Some(SharedString::from("assignee_fallback")),
                assigned_profile: None,
                model: Some(SharedString::from("gpt-4o")),
                parent_id: Some("TASK-PARENT-1".to_string()),
                goal_id: Some("GOAL-10".to_string()),
            },
            AgentUnifiedItem {
                kind: AgentItemKind::Task,
                id: "TASK-SYNTHESIZED".to_string(),
                title: "Synthesized Task".to_string(),
                status: "running".to_string(),
                priority: 1,
                created_at: Some(1791500001000),
                failure_reason: None,
                progress: None,
                assignee: None,
                assigned_profile: Some(SharedString::from("agent_engineer")),
                model: Some(SharedString::from("claude-3-7-sonnet")),
                parent_id: Some("TASK-EXISTING".to_string()),
                goal_id: Some("GOAL-10".to_string()),
            },
        ];

        merge_items_into_graph(&mut graph, &items);

        assert_eq!(graph.tasks.len(), 2);
        let existing = &graph.tasks[0];
        assert_eq!(existing.parent_id, Some(AgentTaskId::from("TASK-PARENT-1")));
        assert_eq!(existing.goal_id, Some("GOAL-10".to_string()));
        assert_eq!(existing.title, "Existing Task Title");
        assert_eq!(existing.status, AgentTaskStatus::Ready);
        assert_eq!(existing.created_at, Some(1791500000000));
        assert_eq!(
            existing.assigned_profile,
            Some(SharedString::from("assignee_fallback"))
        );
        assert_eq!(existing.model, Some(SharedString::from("gpt-4o")));

        let synth = &graph.tasks[1];
        assert_eq!(synth.id, AgentTaskId::from("TASK-SYNTHESIZED"));
        assert_eq!(synth.parent_id, Some(AgentTaskId::from("TASK-EXISTING")));
        assert_eq!(synth.goal_id, Some("GOAL-10".to_string()));
        assert_eq!(synth.title, "Synthesized Task");
        assert_eq!(synth.status, AgentTaskStatus::Running);
        assert_eq!(synth.attempt, 1);
        assert_eq!(synth.created_at, Some(1791500001000));
        assert_eq!(
            synth.assigned_profile,
            Some(SharedString::from("agent_engineer"))
        );
        assert_eq!(synth.model, Some(SharedString::from("claude-3-7-sonnet")));
    }
}
