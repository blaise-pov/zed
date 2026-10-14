use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use gpui::{App, Context, Task};
use util::ResultExt;

use crate::agent_task::{
    AgentGoalStatus, AgentTaskArtifact, AgentTaskDetail, AgentTaskEvent,
    AgentTaskEventKind, AgentTaskGraph, AgentTaskId, AgentTaskProvider, AgentTaskStatus,
};

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
        let events_task = provider.list_events(200, cx);

        cx.spawn(async move |this, cx| {
            let (graph_result, events_result) = futures::join!(graph_task, events_task);

            this.update(cx, |store, cx| {
                let mut changed = false;

                match graph_result {
                    Ok(graph) => {
                        for task in &graph.tasks {
                            if let Some(session_id) = &task.session_id {
                                crate::task_worktree::register_task_session(
                                    &task.id,
                                    session_id.clone(),
                                );
                            }
                        }

                        match events_result {
                            Ok(events) => {
                                if store.events.make_contiguous() != events.as_slice() {
                                    store.events = events.into();
                                    changed = true;
                                }
                            }
                            Err(err) => {
                                log::error!("failed to fetch events: {err:?}");
                            }
                        }

                        if store.graph != graph {
                            store.graph = graph;
                            changed = true;
                        }

                        if store.is_offline {
                            store.is_offline = false;
                            changed = true;
                        }

                        if store.last_error.is_some() {
                            store.last_error = None;
                            changed = true;
                        }
                    }
                    Err(err) => {
                        if !store.is_offline {
                            store.is_offline = true;
                            changed = true;
                        }

                        let err_message = err.to_string();
                        if store.last_error.as_deref() != Some(&err_message) {
                            store.last_error = Some(err_message);
                            changed = true;
                        }
                    }
                }

                if changed {
                    cx.notify();
                }
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
        goal_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let provider = self.provider.clone();
        let task = provider.ensure_task(id, title, goal_id, cx);
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
    use crate::agent_task::{AgentGoalSummary, AgentTaskStatus, AgentTaskSummary};
    use context_server::ContextServerId;
    use gpui::{App, AppContext, TestAppContext};

    #[derive(Default)]
    struct CallCounts {
        archive_calls: Vec<AgentTaskId>,
        unarchive_calls: Vec<AgentTaskId>,
        delete_calls: Vec<AgentTaskId>,
        ensure_task_calls: Vec<(AgentTaskId, String, Option<String>)>,
        set_task_status_calls: Vec<(AgentTaskId, AgentTaskStatus)>,
        set_goal_status_calls: Vec<(String, AgentGoalStatus)>,
        archive_goal_calls: Vec<String>,
        unarchive_goal_calls: Vec<String>,
        delete_goal_calls: Vec<String>,
        fetch_graph_count: usize,
    }

    #[derive(Default)]
    struct TestProvider {
        should_fail: bool,
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
                        session_id: None,
                        metrics: None,
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

        fn ensure_task(
            &self,
            id: &AgentTaskId,
            title: &str,
            goal_id: Option<&str>,
            _cx: &mut App,
        ) -> Task<Result<()>> {
            self.calls.lock().unwrap().ensure_task_calls.push((
                id.clone(),
                title.to_string(),
                goal_id.map(str::to_string),
            ));
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
                store.ensure_task(&task_id, "Test Title", Some("GOAL-1"), cx)
            })
            .await;
        assert!(result.is_ok());

        let counts = calls.lock().unwrap();
        assert_eq!(
            counts.ensure_task_calls,
            vec![(
                task_id,
                "Test Title".to_string(),
                Some("GOAL-1".to_string())
            )]
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

    #[gpui::test]
    async fn test_agent_task_store_refresh_dirty_checking(cx: &mut TestAppContext) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct DynamicProvider {
            graph: parking_lot::Mutex<AgentTaskGraph>,
            events: parking_lot::Mutex<Vec<AgentTaskEvent>>,
        }

        impl AgentTaskProvider for DynamicProvider {
            fn server_id(&self) -> ContextServerId {
                ContextServerId("dynamic_test".into())
            }

            fn fetch_graph(&self, _cx: &mut App) -> Task<Result<AgentTaskGraph>> {
                Task::ready(Ok(self.graph.lock().clone()))
            }

            fn get_task(&self, _id: &AgentTaskId, _cx: &mut App) -> Task<Result<AgentTaskDetail>> {
                Task::ready(Err(anyhow::anyhow!("not implemented")))
            }

            fn complete_task(&self, _id: &AgentTaskId, _cx: &mut App) -> Task<Result<()>> {
                Task::ready(Ok(()))
            }

            fn fail_task(
                &self,
                _id: &AgentTaskId,
                _reason: &str,
                _cx: &mut App,
            ) -> Task<Result<()>> {
                Task::ready(Ok(()))
            }

            fn archive_task(&self, _id: &AgentTaskId, _cx: &mut App) -> Task<Result<()>> {
                Task::ready(Ok(()))
            }

            fn unarchive_task(&self, _id: &AgentTaskId, _cx: &mut App) -> Task<Result<()>> {
                Task::ready(Ok(()))
            }

            fn delete_task(&self, _id: &AgentTaskId, _cx: &mut App) -> Task<Result<()>> {
                Task::ready(Ok(()))
            }

            fn list_events(&self, _limit: u32, _cx: &mut App) -> Task<Result<Vec<AgentTaskEvent>>> {
                Task::ready(Ok(self.events.lock().clone()))
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

        let initial_graph = AgentTaskGraph {
            tasks: vec![AgentTaskSummary {
                id: AgentTaskId::from("TASK-1"),
                parent_id: None,
                goal_id: None,
                title: "Task 1".to_string(),
                status: AgentTaskStatus::Ready,
                attempt: 1,
                assignee: None,
                write_scopes: vec![],
                created_at: None,
                assigned_profile: None,
                model: None,
                session_id: None,
                metrics: None,
            }],
            goals: vec![],
        };

        let provider = Arc::new(DynamicProvider {
            graph: parking_lot::Mutex::new(initial_graph.clone()),
            events: parking_lot::Mutex::new(Vec::new()),
        });

        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider.clone(), cx)));

        let notify_count = Arc::new(AtomicUsize::new(0));
        let notify_count_clone = notify_count.clone();
        cx.update(|cx| {
            cx.observe(&store, move |_, _| {
                notify_count_clone.fetch_add(1, Ordering::SeqCst);
            })
            .detach();
        });

        cx.run_until_parked();
        let initial_notifies = notify_count.load(Ordering::SeqCst);
        assert!(initial_notifies >= 1);

        let refresh_task = store.update(cx, |s, cx| s.refresh(cx));
        refresh_task.await.ok();
        assert_eq!(notify_count.load(Ordering::SeqCst), initial_notifies);

        let mut updated_graph = initial_graph.clone();
        updated_graph.tasks[0].status = AgentTaskStatus::Running;
        *provider.graph.lock() = updated_graph;

        let refresh_task = store.update(cx, |s, cx| s.refresh(cx));
        refresh_task.await.ok();
        assert_eq!(notify_count.load(Ordering::SeqCst), initial_notifies + 1);
    }

    #[gpui::test]
    async fn test_agent_task_store_refresh_rehydrates_session_id(cx: &mut TestAppContext) {
        crate::task_worktree::reset_registry_for_tests();

        let initial_graph = AgentTaskGraph {
            tasks: vec![AgentTaskSummary {
                id: AgentTaskId::from("TASK-REHYDRATE-1"),
                parent_id: None,
                goal_id: None,
                title: "Task with session".to_string(),
                status: AgentTaskStatus::Running,
                attempt: 1,
                assignee: None,
                write_scopes: vec![],
                created_at: None,
                assigned_profile: None,
                model: None,
                session_id: Some("session-rehydrated-42".to_string()),
                metrics: None,
            }],
            goals: vec![],
        };

        let provider = Arc::new(TestProvider {
            tasks: initial_graph.tasks,
            ..Default::default()
        });

        assert_eq!(
            crate::task_worktree::task_session_id_for_id(&AgentTaskId::from("TASK-REHYDRATE-1")),
            None
        );

        let _store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider.clone(), cx)));
        cx.run_until_parked();

        assert_eq!(
            crate::task_worktree::task_session_id_for_id(&AgentTaskId::from("TASK-REHYDRATE-1")),
            Some("session-rehydrated-42".to_string())
        );
    }
}
