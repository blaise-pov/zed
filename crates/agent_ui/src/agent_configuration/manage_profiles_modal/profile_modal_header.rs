use agent_settings::ProfileOrigin;
use ui::prelude::*;

#[derive(IntoElement)]
pub struct ProfileModalHeader {
    label: SharedString,
    icon: Option<IconName>,
    origin: Option<ProfileOrigin>,
}

impl ProfileModalHeader {
    pub fn new(label: impl Into<SharedString>, icon: Option<IconName>) -> Self {
        Self {
            label: label.into(),
            icon,
            origin: None,
        }
    }

    pub fn with_origin(mut self, origin: Option<ProfileOrigin>) -> Self {
        self.origin = origin;
        self
    }
}

impl RenderOnce for ProfileModalHeader {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let mut container = h_flex()
            .w_full()
            .px(DynamicSpacing::Base12.rems(cx))
            .pt(DynamicSpacing::Base08.rems(cx))
            .pb(DynamicSpacing::Base04.rems(cx))
            .rounded_t_sm()
            .gap_1p5()
            .items_center();

        if let Some(icon) = self.icon {
            container = container.child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted));
        }

        container = container.child(
            h_flex().gap_1().overflow_x_hidden().child(
                div()
                    .max_w_96()
                    .overflow_x_hidden()
                    .text_ellipsis()
                    .child(Headline::new(self.label).size(HeadlineSize::XSmall)),
            ),
        );

        if let Some(origin) = self.origin {
            match origin {
                ProfileOrigin::Global => {
                    container = container.child(
                        div()
                            .px_1p5()
                            .py_0p5()
                            .rounded_md()
                            .bg(cx.theme().colors().element_hover)
                            .child(
                                Label::new("Global")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    );
                }
                ProfileOrigin::Project { path, .. } => {
                    container = container.child(
                        div()
                            .px_1p5()
                            .py_0p5()
                            .rounded_md()
                            .bg(cx.theme().colors().element_selected)
                            .child(
                                Label::new(format!("Project ({})", path.as_unix_str()))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Accent),
                            ),
                    );
                }
            }
        }

        container
    }
}
