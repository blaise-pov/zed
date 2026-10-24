use std::sync::Arc;
use std::time::Duration;

use acp_thread::ClientUserMessageId;
use agent::{ContextServerRegistry, Templates, Thread};
use gpui::{AppContext, TestAppContext};
use language_model::fake_provider::FakeLanguageModelProvider;
use language_model::{
    LanguageModelCompletionError, LanguageModelProviderName, LanguageModelRegistry,
};
use project::Project;

async fn setup_thread_for_test(
    cx: &mut TestAppContext,
) -> (gpui::Entity<Thread>, Arc<FakeLanguageModelProvider>) {
    let fake = cx.update(|cx| {
        let settings_store = settings::SettingsStore::test(cx);
        cx.set_global(settings_store);

        LanguageModelRegistry::test(cx)
    });

    let fs = fs::FakeFs::new(cx.background_executor.clone());
    let templates = Templates::new();
    let project = Project::test(fs.clone(), [], cx).await;

    let thread = cx.update(|cx| {
        let project_context = cx.new(|_cx| prompt_store::ProjectContext::default());
        let context_server_store = project.read(cx).context_server_store();
        let context_server_registry =
            cx.new(|cx| ContextServerRegistry::new(context_server_store, cx));

        cx.new(|cx| {
            Thread::new(
                project,
                project_context,
                context_server_registry,
                templates,
                None,
                cx,
            )
        })
    });

    (thread, fake)
}

#[gpui::test]
async fn test_watchdog_does_not_fire_during_retry(cx: &mut TestAppContext) {
    let (thread, fake) = setup_thread_for_test(cx).await;
    let model = fake.model("fake");

    cx.update(|cx| {
        thread.update(cx, |thread, cx| {
            thread.set_model(model.clone(), cx);
            thread.set_watchdog_timeout(Duration::from_millis(100));
        });
    });

    let _events = cx.update(|cx| {
        thread.update(cx, |thread, cx| {
            thread
                .send(ClientUserMessageId::new(), ["Start turn"], cx)
                .expect("send failed")
        })
    });

    cx.run_until_parked();

    thread.read_with(cx, |thread, _cx| {
        assert!(thread.is_turn_running());
        assert!(!thread.is_turn_complete());
    });

    fake.send_last_error(
        &model,
        LanguageModelCompletionError::from_http_status(
            LanguageModelProviderName::new("Anthropic"),
            http_client::StatusCode::SERVICE_UNAVAILABLE,
            "Service unavailable, retry later".to_string(),
            Some(Duration::from_millis(250)),
        ),
    );
    fake.end_last(&model);
    cx.run_until_parked();

    cx.background_executor.timer(Duration::from_millis(150)).await;
    cx.run_until_parked();

    thread.read_with(cx, |thread, _cx| {
        assert!(
            thread.is_turn_running(),
            "turn must still be running while retry is pending"
        );
        assert!(!thread.is_turn_complete());
    });

    cx.background_executor.timer(Duration::from_millis(150)).await;
    cx.run_until_parked();

    fake.send_last_text(&model, "Success after retry");
    fake.end_last(&model);
    cx.run_until_parked();

    let text = thread.read_with(cx, |thread, _cx| {
        assert!(!thread.is_turn_running());
        assert!(thread.is_turn_complete());

        let last_msg = thread.last_message().expect("expected at least one message");
        last_msg.to_markdown()
    });
    assert!(text.contains("Success after retry"));
    assert!(!text.contains("Turn timed out"));
}
