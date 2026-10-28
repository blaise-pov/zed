use std::collections::HashMap;
use std::sync::Arc;
#[cfg(not(any(test, feature = "test-support")))]
use std::sync::LazyLock;

use anyhow::Result;
use gpui::{App, Context, Entity, Task, TaskExt, WeakEntity};
use project::Project;
use util::ResultExt;

use crate::agent_task::{
    AgentGoalStatus, AgentTaskArtifact, AgentTaskDetail, AgentTaskGraph, AgentTaskId,
    AgentTaskProvider, AgentTaskStatus,
};

#[cfg(any(test, feature = "test-support"))]
struct TestThreadLocal<T: 'static>(fn() -> &'static T);

#[cfg(any(test, feature = "test-support"))]
impl<T: 'static> std::ops::Deref for TestThreadLocal<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        (self.0)()
    }
}

#[cfg(any(test, feature = "test-support"))]
macro_rules! task_store_static {
    ($name:ident: $ty:ty = $init:expr) => {
        static $name: TestThreadLocal<$ty> = TestThreadLocal(|| {
            thread_local! {
                static INSTANCE: &'static $ty = Box::leak(Box::new($init));
            }
            INSTANCE.with(|instance| *instance)
        });
    };
}

#[cfg(not(any(test, feature = "test-support")))]
macro_rules! task_store_static {
    ($name:ident: $ty:ty = $init:expr) => {
        static $name: LazyLock<$ty> = LazyLock::new(|| $init);
    };
}

task_store_static!(
    PROJECT_TASK_STORES: parking_lot::RwLock<HashMap<gpui::EntityId, WeakEntity<AgentTaskStore>>> =
        parking_lot::RwLock::new(HashMap::default())
);

pub fn register_task_store_for_project(
    project_id: gpui::EntityId,
    store: WeakEntity<AgentTaskStore>,
) {
    let mut reg = PROJECT_TASK_STORES.write();
    reg.retain(|_, weak| weak.upgrade().is_some());
    reg.insert(project_id, store);
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_registry_for_tests() {
    let mut reg = PROJECT_TASK_STORES.write();
    reg.clear();
}

pub fn refresh_task_store_for_project(project: &Entity<Project>, cx: &mut App) {
    let store = {
        let mut reg = PROJECT_TASK_STORES.write();
        reg.retain(|_, weak| weak.upgrade().is_some());
        reg.get(&project.entity_id()).cloned()
    };
    if let Some(store) = store {
        if let Some(store) = store.upgrade() {
            store.update(cx, |store, cx| {
                store.refresh(cx).detach_and_log_err(cx);
            });
        }
    }
}

pub struct AgentTaskStore {
    provider: Arc<dyn AgentTaskProvider>,
    project: Option<Entity<Project>>,
    graph: AgentTaskGraph,
    has_initial_refresh: bool,
    is_offline: bool,
    last_error: Option<String>,
}

impl AgentTaskStore {
    pub fn new(provider: Arc<dyn AgentTaskProvider>, cx: &mut Context<Self>) -> Self {
        Self::new_with_project(provider, None, cx)
    }

    pub fn new_with_project(
        provider: Arc<dyn AgentTaskProvider>,
        project: Option<Entity<Project>>,
        cx: &mut Context<Self>,
    ) -> Self {
        if let Some(project) = &project {
            register_task_store_for_project(project.entity_id(), cx.weak_entity());
        }

        let mut store = Self {
            provider,
            project,
            graph: AgentTaskGraph::default(),
            has_initial_refresh: false,
            is_offline: false,
            last_error: None,
        };
        store.refresh(cx).detach_and_log_err(cx);
        store
    }

    pub fn provider(&self) -> &Arc<dyn AgentTaskProvider> {
        &self.provider
    }

    pub fn project(&self) -> Option<&Entity<Project>> {
        self.project.as_ref()
    }

    pub fn set_project(&mut self, project: Option<Entity<Project>>) {
        self.project = project;
    }

    /// Swap the backing provider (e.g. when the configured task server id
    /// changes) and refresh from the new server.
    pub fn set_provider(&mut self, provider: Arc<dyn AgentTaskProvider>, cx: &mut Context<Self>) {
        self.provider = provider;
        self.graph = AgentTaskGraph::default();
        self.has_initial_refresh = false;
        self.is_offline = false;
        self.last_error = None;
        self.refresh(cx).detach_and_log_err(cx);
        cx.notify();
    }

    pub fn graph(&self) -> &AgentTaskGraph {
        &self.graph
    }

    pub fn is_offline(&self) -> bool {
        self.is_offline
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let provider = self.provider.clone();
        let graph_task = provider.fetch_graph(cx);

        cx.spawn(async move |this, cx| {
            let graph_result = graph_task.await;

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

                        if store.has_initial_refresh {
                            if let Some(project) = store.project.clone() {
                                let mut retried_goals = std::collections::HashSet::new();

                                for new_task in &graph.tasks {
                                    let old_task =
                                        store.graph.tasks.iter().find(|t| t.id == new_task.id);
                                    let transitioned_to_terminal = old_task
                                        .map_or(false, |old| !old.status.is_terminal())
                                        && new_task.status.is_terminal();

                                    if transitioned_to_terminal {
                                        if crate::task_worktree::task_worktree_path_for_id(
                                            &new_task.id,
                                        )
                                        .is_some()
                                        && !crate::task_worktree::has_active_subagent_session(
                                            &new_task.id,
                                        )
                                        {
                                            let project = project.clone();
                                            let task_id = new_task.id.clone();
                                            crate::task_worktree::auto_cleanup_task_worktree(
                                                project, &task_id, cx,
                                            )
                                            .detach_and_log_err(cx);
                                        }

                                        if let Some(goal_id) = &new_task.goal_id {
                                            let goal_was_terminal = store
                                                .graph
                                                .goals
                                                .iter()
                                                .find(|g| &g.goal_id == goal_id)
                                                .is_some_and(|g| g.status.is_terminal());
                                            let goal_is_terminal = graph
                                                .goals
                                                .iter()
                                                .find(|g| &g.goal_id == goal_id)
                                                .map_or(goal_was_terminal, |g| {
                                                    g.status.is_terminal()
                                                });

                                            if goal_was_terminal
                                                && goal_is_terminal
                                                && retried_goals.insert(goal_id.clone())
                                            {
                                                crate::task_worktree::cleanup_graduated_goal(
                                                    project.clone(),
                                                    goal_id,
                                                    cx,
                                                )
                                                .detach_and_log_err(cx);
                                            }
                                        }
                                    }
                                }

                                for new_goal in &graph.goals {
                                    let old_goal = store
                                        .graph
                                        .goals
                                        .iter()
                                        .find(|g| g.goal_id == new_goal.goal_id);
                                    let transitioned_to_terminal = old_goal
                                        .map_or(false, |old| !old.status.is_terminal())
                                        && new_goal.status.is_terminal();

                                    if transitioned_to_terminal {
                                        crate::task_worktree::cleanup_graduated_goal(
                                            project.clone(),
                                            &new_goal.goal_id,
                                            cx,
                                        )
                                        .detach_and_log_err(cx);
                                    }
                                }
                            }
                        } else {
                            store.has_initial_refresh = true;
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

    pub fn update_task_title(
        &mut self,
        id: &AgentTaskId,
        title: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let provider = self.provider.clone();
        let task = provider.update_task_title(id, title, cx);
        cx.spawn(async move |this, cx| {
            task.await?;
            let refresh_task = this.update(cx, |store, cx| store.refresh(cx))?;
            refresh_task.await?;
            Ok(())
        })
    }

    pub fn update_goal_title(
        &mut self,
        goal_id: &str,
        title: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let provider = self.provider.clone();
        let task = provider.update_goal_title(goal_id, title, cx);
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
    use crate::agent_task::AgentTaskEvent;
    use fs::Fs;
    use settings::Settings;
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
        set_task_status_calls: Vec<(AgentTaskId, AgentTaskStatus)>,
        set_goal_status_calls: Vec<(String, AgentGoalStatus)>,
        archive_goal_calls: Vec<String>,
        unarchive_goal_calls: Vec<String>,
        delete_goal_calls: Vec<String>,
        update_task_title_calls: Vec<(AgentTaskId, String)>,
        update_goal_title_calls: Vec<(String, String)>,
        fetch_graph_count: usize,
    }

    #[derive(Default)]
    struct DynamicProvider {
        graph: parking_lot::Mutex<AgentTaskGraph>,
        #[allow(dead_code)]
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

        fn fail_task(&self, _id: &AgentTaskId, _reason: &str, _cx: &mut App) -> Task<Result<()>> {
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

        fn update_task_title(
            &self,
            id: &AgentTaskId,
            title: &str,
            _cx: &mut App,
        ) -> Task<Result<()>> {
            self.calls
                .lock()
                .unwrap()
                .update_task_title_calls
                .push((id.clone(), title.to_string()));
            Task::ready(Ok(()))
        }

        fn update_goal_title(&self, goal_id: &str, title: &str, _cx: &mut App) -> Task<Result<()>> {
            self.calls
                .lock()
                .unwrap()
                .update_goal_title_calls
                .push((goal_id.to_string(), title.to_string()));
            Task::ready(Ok(()))
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
    async fn test_agent_task_store_update_task_title(cx: &mut TestAppContext) {
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
                store.update_task_title(&task_id, "New Task Title", cx)
            })
            .await;
        assert!(result.is_ok());

        let counts = calls.lock().unwrap();
        assert_eq!(
            counts.update_task_title_calls,
            vec![(task_id, "New Task Title".to_string())]
        );
        assert_eq!(counts.fetch_graph_count, initial_refreshes + 1);
    }

    #[gpui::test]
    async fn test_agent_task_store_update_goal_title(cx: &mut TestAppContext) {
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
                store.update_goal_title("GOAL-1", "New Goal Title", cx)
            })
            .await;
        assert!(result.is_ok());

        let counts = calls.lock().unwrap();
        assert_eq!(
            counts.update_goal_title_calls,
            vec![("GOAL-1".to_string(), "New Goal Title".to_string())]
        );
        assert_eq!(counts.fetch_graph_count, initial_refreshes + 1);
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

    #[gpui::test]
    async fn test_terminal_reaction_flips_to_completed_cleans_worktree(cx: &mut TestAppContext) {
        crate::task_worktree::reset_registry_for_tests();
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            project::project_settings::ProjectSettings::register(cx);
            agent_settings::AgentSettings::register(cx);
        });

        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            serde_json::json!({
                ".git": {}
            }),
        )
        .await;

        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
        let task_id = AgentTaskId::from("TASK-TERM-1");
        let task = AgentTaskSummary {
            id: task_id.clone(),
            title: "Terminal task".to_string(),
            status: AgentTaskStatus::Ready,
            ..Default::default()
        };

        let path = cx
            .update(|cx| crate::task_worktree::ensure_task_worktree(project.clone(), &task, cx))
            .await
            .expect("worktree creation should succeed");
        assert!(crate::task_worktree::task_worktree_path_for_id(&task_id).is_some());

        let initial_graph = AgentTaskGraph {
            tasks: vec![task.clone()],
            goals: vec![],
        };
        let provider = Arc::new(DynamicProvider {
            graph: parking_lot::Mutex::new(initial_graph),
            events: parking_lot::Mutex::new(Vec::new()),
        });

        let store = cx.update(|cx| {
            cx.new(|cx| AgentTaskStore::new_with_project(provider.clone(), Some(project), cx))
        });
        cx.run_until_parked();

        assert!(crate::task_worktree::task_worktree_path_for_id(&task_id).is_some());

        let mut completed_task = task.clone();
        completed_task.status = AgentTaskStatus::Completed;
        *provider.graph.lock() = AgentTaskGraph {
            tasks: vec![completed_task],
            goals: vec![],
        };

        let refresh_task = store.update(cx, |s, cx| s.refresh(cx));
        refresh_task.await.ok();
        cx.run_until_parked();

        assert!(
            crate::task_worktree::task_worktree_path_for_id(&task_id).is_none(),
            "worktree should be unregistered after auto cleanup"
        );
        assert!(
            !fs.is_dir(&path).await,
            "worktree directory should be removed"
        );
    }

    #[gpui::test]
    async fn test_terminal_reaction_active_subagent_session_retains_worktree(
        cx: &mut TestAppContext,
    ) {
        crate::task_worktree::reset_registry_for_tests();
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            project::project_settings::ProjectSettings::register(cx);
            agent_settings::AgentSettings::register(cx);
        });

        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            serde_json::json!({
                ".git": {}
            }),
        )
        .await;

        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
        let task_id = AgentTaskId::from("TASK-TERM-2");
        let task = AgentTaskSummary {
            id: task_id.clone(),
            title: "Terminal task with active session".to_string(),
            status: AgentTaskStatus::Ready,
            ..Default::default()
        };

        let path = cx
            .update(|cx| crate::task_worktree::ensure_task_worktree(project.clone(), &task, cx))
            .await
            .expect("worktree creation should succeed");
        assert!(crate::task_worktree::task_worktree_path_for_id(&task_id).is_some());

        let initial_graph = AgentTaskGraph {
            tasks: vec![task.clone()],
            goals: vec![],
        };
        let provider = Arc::new(DynamicProvider {
            graph: parking_lot::Mutex::new(initial_graph),
            events: parking_lot::Mutex::new(Vec::new()),
        });

        let store = cx.update(|cx| {
            cx.new(|cx| AgentTaskStore::new_with_project(provider.clone(), Some(project), cx))
        });
        cx.run_until_parked();

        let guard = crate::task_worktree::SubagentSessionGuard::test_new(task_id.clone());

        let mut completed_task = task.clone();
        completed_task.status = AgentTaskStatus::Completed;
        *provider.graph.lock() = AgentTaskGraph {
            tasks: vec![completed_task],
            goals: vec![],
        };

        let refresh_task = store.update(cx, |s, cx| s.refresh(cx));
        refresh_task.await.ok();
        cx.run_until_parked();

        assert!(
            crate::task_worktree::task_worktree_path_for_id(&task_id).is_some(),
            "worktree should be retained when active session is present"
        );
        assert!(
            fs.is_dir(&path).await,
            "worktree directory should still exist"
        );

        drop(guard);
    }

    #[gpui::test]
    async fn test_terminal_reaction_terminal_status_unchanged_no_cleanup(cx: &mut TestAppContext) {
        crate::task_worktree::reset_registry_for_tests();
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            project::project_settings::ProjectSettings::register(cx);
            agent_settings::AgentSettings::register(cx);
        });

        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            serde_json::json!({
                ".git": {}
            }),
        )
        .await;

        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
        let task_id = AgentTaskId::from("TASK-TERM-3");
        let task = AgentTaskSummary {
            id: task_id.clone(),
            title: "Already completed task".to_string(),
            status: AgentTaskStatus::Completed,
            ..Default::default()
        };

        let path = cx
            .update(|cx| crate::task_worktree::ensure_task_worktree(project.clone(), &task, cx))
            .await
            .expect("worktree creation should succeed");

        let initial_graph = AgentTaskGraph {
            tasks: vec![task.clone()],
            goals: vec![],
        };
        let provider = Arc::new(DynamicProvider {
            graph: parking_lot::Mutex::new(initial_graph.clone()),
            events: parking_lot::Mutex::new(Vec::new()),
        });

        let store = cx.update(|cx| {
            cx.new(|cx| AgentTaskStore::new_with_project(provider.clone(), Some(project), cx))
        });
        cx.run_until_parked();

        let refresh_task = store.update(cx, |s, cx| s.refresh(cx));
        refresh_task.await.ok();
        cx.run_until_parked();

        assert!(
            crate::task_worktree::task_worktree_path_for_id(&task_id).is_some(),
            "worktree should not be cleaned when terminal status did not change"
        );
        assert!(fs.is_dir(&path).await);
    }

    #[gpui::test]
    async fn test_terminal_reaction_first_refresh_already_terminal_no_cleanup(
        cx: &mut TestAppContext,
    ) {
        crate::task_worktree::reset_registry_for_tests();
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            project::project_settings::ProjectSettings::register(cx);
            agent_settings::AgentSettings::register(cx);
        });

        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            serde_json::json!({
                ".git": {}
            }),
        )
        .await;

        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
        let task_id = AgentTaskId::from("TASK-TERM-4");
        let task = AgentTaskSummary {
            id: task_id.clone(),
            title: "Startup completed task".to_string(),
            status: AgentTaskStatus::Completed,
            ..Default::default()
        };

        let path = cx
            .update(|cx| crate::task_worktree::ensure_task_worktree(project.clone(), &task, cx))
            .await
            .expect("worktree creation should succeed");

        let initial_graph = AgentTaskGraph {
            tasks: vec![task.clone()],
            goals: vec![],
        };
        let provider = Arc::new(DynamicProvider {
            graph: parking_lot::Mutex::new(initial_graph),
            events: parking_lot::Mutex::new(Vec::new()),
        });

        let _store = cx.update(|cx| {
            cx.new(|cx| AgentTaskStore::new_with_project(provider.clone(), Some(project), cx))
        });
        cx.run_until_parked();

        assert!(
            crate::task_worktree::task_worktree_path_for_id(&task_id).is_some(),
            "worktree should not be cleaned on initial refresh bootstrap"
        );
        assert!(fs.is_dir(&path).await);
    }

    #[gpui::test]
    async fn test_refresh_task_store_for_project(cx: &mut TestAppContext) {
        reset_registry_for_tests();
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            project::project_settings::ProjectSettings::register(cx);
            agent_settings::AgentSettings::register(cx);
        });

        let fs = fs::FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;

        let calls = Arc::new(Mutex::new(CallCounts::default()));
        let provider = Arc::new(TestProvider {
            calls: calls.clone(),
            ..Default::default()
        });

        let store = cx.update(|cx| {
            cx.new(|cx| AgentTaskStore::new_with_project(provider, Some(project.clone()), cx))
        });
        cx.run_until_parked();

        let initial_refreshes = calls.lock().unwrap().fetch_graph_count;
        assert_eq!(initial_refreshes, 1);

        cx.update(|cx| {
            refresh_task_store_for_project(&project, cx);
        });
        cx.run_until_parked();

        assert_eq!(calls.lock().unwrap().fetch_graph_count, 2);

        drop(store);
        cx.update(|cx| {
            refresh_task_store_for_project(&project, cx);
        });
        cx.run_until_parked();

        assert_eq!(calls.lock().unwrap().fetch_graph_count, 2);

        let reg = PROJECT_TASK_STORES.read();
        assert!(
            !reg.contains_key(&project.entity_id()),
            "dead weak store handle should be pruned on refresh"
        );
    }

    #[gpui::test]
    async fn test_goal_transition_to_completed_triggers_cleanup(cx: &mut TestAppContext) {
        crate::task_worktree::reset_registry_for_tests();
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            project::project_settings::ProjectSettings::register(cx);
            agent_settings::AgentSettings::register(cx);
        });

        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            serde_json::json!({
                ".git": {
                    "HEAD": "ref: refs/heads/main\n"
                }
            }),
        )
        .await;

        let dot_git = std::path::Path::new("/root/.git");
        fs.with_git_state(dot_git, true, |state| {
            state.refs.insert("refs/heads/main".into(), "main-sha".into());
            state.refs.insert("HEAD".into(), "main-sha".into());
            state.branches.insert("main".into());
            state.refs.insert(
                "refs/heads/agent-goal/GOAL-1".into(),
                "goal-sha-1".into(),
            );
            state.branches.insert("agent-goal/GOAL-1".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
        let initial_goal = AgentGoalSummary {
            goal_id: "GOAL-1".to_string(),
            title: "Goal 1".to_string(),
            status: AgentGoalStatus::Running,
            priority: 0,
            tasks_total: 0,
            tasks_done: 0,
            created_at: None,
            metrics: None,
            session_id: None,
        };

        let initial_graph = AgentTaskGraph {
            tasks: vec![],
            goals: vec![initial_goal.clone()],
        };
        let provider = Arc::new(DynamicProvider {
            graph: parking_lot::Mutex::new(initial_graph),
            events: parking_lot::Mutex::new(Vec::new()),
        });

        let store = cx.update(|cx| {
            cx.new(|cx| AgentTaskStore::new_with_project(provider.clone(), Some(project), cx))
        });
        cx.run_until_parked();

        assert!(
            fs.with_git_state(dot_git, false, |state| state
                .branches
                .contains("agent-goal/GOAL-1"))
                .unwrap(),
            "goal branch must exist before completion"
        );

        let mut completed_goal = initial_goal;
        completed_goal.status = AgentGoalStatus::Completed;
        *provider.graph.lock() = AgentTaskGraph {
            tasks: vec![],
            goals: vec![completed_goal],
        };

        let refresh_task = store.update(cx, |s, cx| s.refresh(cx));
        refresh_task.await.ok();
        cx.run_until_parked();

        assert!(
            !fs.with_git_state(dot_git, false, |state| state
                .branches
                .contains("agent-goal/GOAL-1"))
                .unwrap(),
            "goal branch should be cleaned up after transitioning to Completed"
        );
    }

    #[gpui::test]
    async fn test_running_goal_triggers_no_cleanup(cx: &mut TestAppContext) {
        crate::task_worktree::reset_registry_for_tests();
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            project::project_settings::ProjectSettings::register(cx);
            agent_settings::AgentSettings::register(cx);
        });

        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            serde_json::json!({
                ".git": {
                    "HEAD": "ref: refs/heads/main\n"
                }
            }),
        )
        .await;

        let dot_git = std::path::Path::new("/root/.git");
        fs.with_git_state(dot_git, true, |state| {
            state.refs.insert("refs/heads/main".into(), "main-sha".into());
            state.refs.insert("HEAD".into(), "main-sha".into());
            state.branches.insert("main".into());
            state.refs.insert(
                "refs/heads/agent-goal/GOAL-RUNNING".into(),
                "goal-sha-1".into(),
            );
            state.branches.insert("agent-goal/GOAL-RUNNING".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
        let running_goal = AgentGoalSummary {
            goal_id: "GOAL-RUNNING".to_string(),
            title: "Running Goal".to_string(),
            status: AgentGoalStatus::Running,
            priority: 0,
            tasks_total: 1,
            tasks_done: 0,
            created_at: None,
            metrics: None,
            session_id: None,
        };

        let initial_graph = AgentTaskGraph {
            tasks: vec![],
            goals: vec![running_goal.clone()],
        };
        let provider = Arc::new(DynamicProvider {
            graph: parking_lot::Mutex::new(initial_graph),
            events: parking_lot::Mutex::new(Vec::new()),
        });

        let store = cx.update(|cx| {
            cx.new(|cx| AgentTaskStore::new_with_project(provider.clone(), Some(project), cx))
        });
        cx.run_until_parked();

        let refresh_task = store.update(cx, |s, cx| s.refresh(cx));
        refresh_task.await.ok();
        cx.run_until_parked();

        assert!(
            fs.with_git_state(dot_git, false, |state| state
                .branches
                .contains("agent-goal/GOAL-RUNNING"))
                .unwrap(),
            "goal branch must NOT be cleaned up while goal is Running"
        );
    }

    #[gpui::test]
    async fn test_task_terminal_transition_under_already_terminal_goal_triggers_retry(
        cx: &mut TestAppContext,
    ) {
        crate::task_worktree::reset_registry_for_tests();
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            project::project_settings::ProjectSettings::register(cx);
            agent_settings::AgentSettings::register(cx);
        });

        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            serde_json::json!({
                ".git": {
                    "HEAD": "ref: refs/heads/main\n"
                }
            }),
        )
        .await;

        let dot_git = std::path::Path::new("/root/.git");
        fs.with_git_state(dot_git, true, |state| {
            state.refs.insert("refs/heads/main".into(), "main-sha".into());
            state.refs.insert("HEAD".into(), "main-sha".into());
            state.branches.insert("main".into());
            state.refs.insert(
                "refs/heads/agent-goal/GOAL-RETRY".into(),
                "goal-sha-1".into(),
            );
            state.branches.insert("agent-goal/GOAL-RETRY".into());
            state.refs.insert(
                "refs/heads/agent-task/TASK-RETRY-1".into(),
                "task-sha-1".into(),
            );
            state.branches.insert("agent-task/TASK-RETRY-1".into());
        })
        .unwrap();

        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
        let task_id = AgentTaskId::from("TASK-RETRY-1");

        crate::task_worktree::register_task_worktree_policy(
            &task_id,
            std::path::PathBuf::from("/fake/retry1"),
            Some("agent-goal/GOAL-RETRY".to_string()),
            None,
            None,
            None,
        );

        let terminal_goal = AgentGoalSummary {
            goal_id: "GOAL-RETRY".to_string(),
            title: "Already completed goal".to_string(),
            status: AgentGoalStatus::Completed,
            priority: 0,
            tasks_total: 1,
            tasks_done: 0,
            created_at: None,
            metrics: None,
            session_id: None,
        };

        let running_task = AgentTaskSummary {
            id: task_id.clone(),
            parent_id: None,
            goal_id: Some("GOAL-RETRY".to_string()),
            title: "Running sibling task".to_string(),
            status: AgentTaskStatus::Running,
            ..Default::default()
        };

        let initial_graph = AgentTaskGraph {
            tasks: vec![running_task.clone()],
            goals: vec![terminal_goal.clone()],
        };
        let provider = Arc::new(DynamicProvider {
            graph: parking_lot::Mutex::new(initial_graph),
            events: parking_lot::Mutex::new(Vec::new()),
        });

        let store = cx.update(|cx| {
            cx.new(|cx| AgentTaskStore::new_with_project(provider.clone(), Some(project), cx))
        });
        cx.run_until_parked();

        assert!(fs
            .with_git_state(dot_git, false, |state| {
                state.branches.contains("agent-goal/GOAL-RETRY")
                    && state.branches.contains("agent-task/TASK-RETRY-1")
            })
            .unwrap());

        let mut completed_task = running_task;
        completed_task.status = AgentTaskStatus::Completed;
        crate::task_worktree::mark_task_terminal(&task_id);
        *provider.graph.lock() = AgentTaskGraph {
            tasks: vec![completed_task],
            goals: vec![terminal_goal],
        };

        let refresh_task = store.update(cx, |s, cx| s.refresh(cx));
        refresh_task.await.ok();
        cx.run_until_parked();

        assert!(
            !fs.with_git_state(dot_git, false, |state| state
                .branches
                .contains("agent-goal/GOAL-RETRY"))
                .unwrap(),
            "goal branch should be cleaned up by retry when task transitions to terminal"
        );
        assert!(
            !fs.with_git_state(dot_git, false, |state| state
                .branches
                .contains("agent-task/TASK-RETRY-1"))
                .unwrap(),
            "task branch should be cleaned up along with the goal"
        );
    }
}
