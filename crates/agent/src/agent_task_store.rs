use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use gpui::{App, Context, Task};
use util::ResultExt;

use crate::agent_task::{
    AgentTaskArtifact, AgentTaskDetail, AgentTaskEvent, AgentTaskEventKind, AgentTaskGraph,
    AgentTaskId, AgentTaskProvider,
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
            let graph_result = graph_task.await;
            let events_result = events_task.await;

            this.update(cx, |store, cx| {
                match (graph_result, events_result) {
                    (Ok(graph), Ok(events)) => {
                        store.graph = graph;
                        store.events = events.into();
                        store.is_offline = false;
                        store.last_error = None;
                    }
                    (Ok(graph), Err(err)) => {
                        log::error!("failed to fetch events: {err:?}");
                        store.graph = graph;
                        store.is_offline = false;
                        store.last_error = None;
                    }
                    (Err(err), _) => {
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
    use super::*;
    use crate::agent_task::AgentTaskStatus;
    use crate::agent_task::AgentTaskSummary;
    use context_server::ContextServerId;
    use gpui::{App, AppContext, TestAppContext};

    struct TestProvider {
        should_fail: bool,
    }

    impl AgentTaskProvider for TestProvider {
        fn server_id(&self) -> ContextServerId {
            ContextServerId("test".into())
        }

        fn fetch_graph(&self, _cx: &mut App) -> Task<Result<AgentTaskGraph>> {
            if self.should_fail {
                Task::ready(Err(anyhow::anyhow!("offline")))
            } else {
                Task::ready(Ok(AgentTaskGraph {
                    tasks: vec![AgentTaskSummary {
                        id: AgentTaskId::from("TASK-1"),
                        parent_id: None,
                        title: "Test Task".to_string(),
                        status: AgentTaskStatus::Ready,
                        attempt: 1,
                        assignee: None,
                        write_scopes: vec![],
                    }],
                }))
            }
        }

        fn get_task(&self, _id: &AgentTaskId, _cx: &mut App) -> Task<Result<AgentTaskDetail>> {
            Task::ready(Err(anyhow::anyhow!("not implemented")))
        }

        fn complete_task(&self, _id: &AgentTaskId, _cx: &mut App) -> Task<Result<()>> {
            Task::ready(Ok(()))
        }

        fn fail_task(&self, _id: &AgentTaskId, _reason: &str, _cx: &mut App) -> Task<Result<()>> {
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
        let provider = Arc::new(TestProvider { should_fail: false });
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
        let provider = Arc::new(TestProvider { should_fail: true });
        let store = cx.update(|cx| cx.new(|cx| AgentTaskStore::new(provider, cx)));

        cx.run_until_parked();

        store.update(cx, |store, _cx| {
            assert!(store.is_offline());
            assert_eq!(store.last_error(), Some("offline"));
        });
    }
}
