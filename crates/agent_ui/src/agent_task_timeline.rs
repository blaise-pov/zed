//! Agent Task Timeline
//!
//! Renders the event tail of a task (and the store-wide event feed) as a
//! vertical timeline: sequence number, a colored kind badge, the owning task
//! and the message. Policy-denied events are highlighted.

use agent::{AgentTaskEvent, AgentTaskEventKind};
use gpui::{App, Div, SharedString, prelude::*};
use ui::{Color, Label, LabelSize, prelude::*};

fn event_kind_label(kind: AgentTaskEventKind) -> (&'static str, Color) {
    match kind {
        AgentTaskEventKind::Info => ("INFO", Color::Muted),
        AgentTaskEventKind::ToolCall => ("TOOL", Color::Info),
        AgentTaskEventKind::PolicyDenied => ("DENIED", Color::Warning),
        AgentTaskEventKind::StatusChanged => ("STATUS", Color::Accent),
        AgentTaskEventKind::ReviewVerdict => ("REVIEW", Color::Success),
    }
}

fn render_event_kind_badge(kind: AgentTaskEventKind) -> Div {
    let (label, color) = event_kind_label(kind);
    h_flex()
        .px_1p5()
        .py_0p5()
        .rounded_sm()
        .child(Label::new(label).size(LabelSize::Small).color(color))
}

fn render_timeline_event(event: &AgentTaskEvent, cx: &App) -> Div {
    let is_denied = event.kind == AgentTaskEventKind::PolicyDenied;
    let message = SharedString::from(event.message.clone());

    h_flex()
        .w_full()
        .gap_2()
        .items_start()
        .p_2()
        .rounded_md()
        .when(is_denied, |this| {
            this.bg(cx.theme().status().warning_background)
        })
        .child(
            v_flex()
                .gap_0p5()
                .items_start()
                .child(
                    Label::new(format!("#{}", event.seq))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(render_event_kind_badge(event.kind)),
        )
        .child(
            v_flex()
                .flex_1()
                .gap_0p5()
                .child(
                    Label::new(message)
                        .size(LabelSize::Small)
                        .color(if is_denied {
                            Color::Warning
                        } else {
                            Color::Default
                        }),
                )
                .when_some(event.task_id.as_ref(), |this, task_id| {
                    this.child(
                        Label::new(format!("Task: {}", task_id))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                }),
        )
}

pub fn render_task_timeline(events: &[AgentTaskEvent], cx: &App) -> Div {
    if events.is_empty() {
        return v_flex().child(
            Label::new("No event history available")
                .size(LabelSize::Small)
                .color(Color::Muted),
        );
    }

    v_flex().gap_1p5().children(
        events
            .iter()
            .map(|event| render_timeline_event(event, cx))
            .collect::<Vec<_>>(),
    )
}
