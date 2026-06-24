use acp_thread::{AcpThread, UiRequest, UiRequestKind, UiRequestScope, UiResponse};
use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Window};
use ui::{ButtonStyle, prelude::*};

/// Renders one normalized [`UiRequest`] surfaced by a runtime (approval, input,
/// select, editor, open-url) adjacent to the agent thread.
///
/// Deny is the default: the deny/cancel control holds focus on first render and
/// `Esc` routes to it, so a stray keypress can never approve. Approving, opening
/// a URL, submitting input, or picking an option each require an explicit
/// action. Every request is answered exactly once — the first response wins and
/// subsequent interactions are ignored — and the answer is routed back to the
/// runtime by request id via [`AcpThread::respond_to_ui_request`].
pub struct UiRequestPrompt {
    thread: Entity<AcpThread>,
    request: UiRequest,
    focus_handle: FocusHandle,
    deny_focus_handle: FocusHandle,
    input_editor: Option<Entity<Editor>>,
    answered: bool,
    focused_default: bool,
}

impl UiRequestPrompt {
    pub fn new(
        thread: Entity<AcpThread>,
        request: UiRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input_editor = matches!(request.kind, UiRequestKind::Input | UiRequestKind::Editor)
            .then(|| {
                let default_value = request.default_value.clone();
                cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    if let Some(default_value) = default_value {
                        editor.set_text(default_value, window, cx);
                    }
                    editor
                })
            });
        Self {
            thread,
            request,
            focus_handle: cx.focus_handle(),
            deny_focus_handle: cx.focus_handle(),
            input_editor,
            answered: false,
            focused_default: false,
        }
    }

    pub fn request_id(&self) -> SharedString {
        self.request.id.clone()
    }

    fn respond(&mut self, response: UiResponse, cx: &mut Context<Self>) {
        if self.answered {
            return;
        }
        self.answered = true;
        let request_id = self.request.id.clone();
        self.thread.update(cx, |thread, cx| {
            thread.respond_to_ui_request(request_id, response, cx)
        });
        cx.notify();
    }

    fn approve(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.respond(UiResponse::Approve, cx);
    }

    /// Negative answer: deny for approval/open-url, cancel for input/select/
    /// editor. This is also what `Esc` routes to.
    fn deny(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let response = match self.request.kind {
            UiRequestKind::Approval | UiRequestKind::OpenUrl => UiResponse::Deny,
            UiRequestKind::Input | UiRequestKind::Select | UiRequestKind::Editor => {
                UiResponse::Cancel
            }
        };
        self.respond(response, cx);
    }

    fn submit_input(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.input_editor.clone() else {
            return;
        };
        let value = editor.read(cx).text(cx);
        self.respond(UiResponse::Input(value), cx);
    }

    fn select(&mut self, option_id: SharedString, cx: &mut Context<Self>) {
        self.respond(UiResponse::Select(option_id), cx);
    }

    fn open_url(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // Explicit user action only — never auto-navigated. We open the URL and
        // record the approval so the runtime can continue.
        if let Some(url) = self.request.url.clone() {
            cx.open_url(&url);
        }
        self.respond(UiResponse::Approve, cx);
    }

    fn badge_label(&self) -> &'static str {
        match self.request.kind {
            UiRequestKind::Approval => "Needs approval",
            UiRequestKind::OpenUrl => "Open link",
            UiRequestKind::Input | UiRequestKind::Editor => "Needs input",
            UiRequestKind::Select => "Needs a choice",
        }
    }

    fn render_scope(scope: &UiRequestScope) -> Option<impl IntoElement> {
        if scope.is_empty() {
            return None;
        }
        let mut parts: Vec<String> = Vec::new();
        if let Some(tool) = &scope.tool {
            parts.push(format!("tool: {tool}"));
        }
        if let Some(action) = &scope.action {
            parts.push(format!("action: {action}"));
        }
        if let Some(path) = &scope.path {
            parts.push(format!("path: {path}"));
        }
        if let Some(workspace) = &scope.workspace {
            parts.push(format!("workspace: {workspace}"));
        }
        if let Some(session) = &scope.session {
            parts.push(format!("session: {session}"));
        }
        Some(
            Label::new(parts.join("  ·  "))
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
    }

    fn render_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        let deny = div().track_focus(&self.deny_focus_handle).child(
            Button::new("ui-request-deny", self.deny_button_label())
                .on_click(cx.listener(|this, _, window, cx| this.deny(window, cx))),
        );

        match self.request.kind {
            UiRequestKind::Approval => h_flex()
                .gap_1()
                .child(deny)
                .child(
                    Button::new("ui-request-approve", "Approve")
                        .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                        .on_click(cx.listener(|this, _, window, cx| this.approve(window, cx))),
                )
                .into_any_element(),
            UiRequestKind::OpenUrl => h_flex()
                .gap_1()
                .child(deny)
                .child(
                    Button::new("ui-request-open-url", "Open link")
                        .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                        .on_click(cx.listener(|this, _, window, cx| this.open_url(window, cx))),
                )
                .into_any_element(),
            UiRequestKind::Input | UiRequestKind::Editor => v_flex()
                .gap_2()
                .when_some(self.input_editor.clone(), |this, editor| this.child(editor))
                .child(
                    h_flex().gap_1().child(deny).child(
                        Button::new("ui-request-submit", "Submit")
                            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.submit_input(window, cx)),
                            ),
                    ),
                )
                .into_any_element(),
            UiRequestKind::Select => {
                let options = self.request.options.clone();
                v_flex()
                    .gap_1()
                    .children(options.into_iter().map(|option| {
                        let option_id = option.id.clone();
                        let element_id =
                            SharedString::from(format!("ui-request-option-{}", option.id));
                        Button::new(element_id, option.label).on_click(
                            cx.listener(move |this, _, _, cx| this.select(option_id.clone(), cx)),
                        )
                    }))
                    .child(deny)
                    .into_any_element()
            }
        }
    }

    fn deny_button_label(&self) -> &'static str {
        match self.request.kind {
            UiRequestKind::Approval | UiRequestKind::OpenUrl => "Deny",
            UiRequestKind::Input | UiRequestKind::Select | UiRequestKind::Editor => "Cancel",
        }
    }
}

impl Focusable for UiRequestPrompt {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for UiRequestPrompt {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Deny-default focus: foreground the deny/cancel control on first paint.
        if !self.focused_default {
            self.focused_default = true;
            self.deny_focus_handle.focus(window, cx);
        }

        let message = self.request.message.clone();

        v_flex()
            .key_context("OmpUiRequest")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                this.deny(window, cx);
                cx.stop_propagation();
            }))
            .gap_2()
            .p_3()
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .bg(cx.theme().colors().editor_background)
            .child(
                Label::new(self.badge_label())
                    .size(LabelSize::Small)
                    .color(Color::Accent),
            )
            .when(!message.is_empty(), |this| this.child(Label::new(message)))
            .children(Self::render_scope(&self.request.scope))
            .child(self.render_controls(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_thread::{AcpThread, StubAgentConnection, UiRequest, UiRequestKind, UiRequestScope};
    use agent_client_protocol::schema::v1 as acp;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use std::rc::Rc;
    use util::path;

    async fn setup(
        kind: UiRequestKind,
        options: Vec<acp_thread::UiRequestOption>,
        cx: &mut TestAppContext,
    ) -> (StubAgentConnection, Entity<AcpThread>, UiRequest) {
        crate::test_support::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), serde_json::json!({"a.txt": ""}))
            .await;
        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        let action_log = cx.new(|_| action_log::ActionLog::new(project.clone()));
        let connection = StubAgentConnection::new();
        let connection_rc: Rc<dyn acp_thread::AgentConnection> = Rc::new(connection.clone());
        let thread = cx.new(|cx| {
            AcpThread::new(
                None,
                None,
                None,
                connection_rc,
                project,
                action_log,
                acp::SessionId::new("test-session"),
                watch::Receiver::constant(acp::PromptCapabilities::new()),
                cx,
            )
        });
        let request = UiRequest {
            id: "ui-1".into(),
            kind,
            message: "Proceed?".into(),
            scope: UiRequestScope {
                tool: Some("write".into()),
                path: Some("a.txt".into()),
                ..Default::default()
            },
            options,
            default_value: None,
            url: (kind == UiRequestKind::OpenUrl).then(|| "https://example.com".into()),
        };
        thread.update(cx, |thread, cx| thread.push_ui_request(request.clone(), cx));
        (connection, thread, request)
    }

    #[gpui::test]
    async fn deny_control_holds_focus_by_default(cx: &mut TestAppContext) {
        let (_connection, thread, request) = setup(UiRequestKind::Approval, Vec::new(), cx).await;
        let (prompt, cx) =
            cx.add_window_view(|window, cx| UiRequestPrompt::new(thread, request, window, cx));
        cx.run_until_parked();
        let deny = prompt.read_with(cx, |prompt, _| prompt.deny_focus_handle.clone());
        let focused = cx.update(|window, _| deny.is_focused(window));
        assert!(focused, "the deny control must hold focus by default");
    }

    #[gpui::test]
    async fn escape_denies_the_request(cx: &mut TestAppContext) {
        let (connection, thread, request) = setup(UiRequestKind::Approval, Vec::new(), cx).await;
        cx.update(|cx| {
            cx.bind_keys([gpui::KeyBinding::new("escape", menu::Cancel, None)]);
        });
        let (_prompt, cx) =
            cx.add_window_view(|window, cx| UiRequestPrompt::new(thread, request, window, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(
            connection.ui_responses(),
            vec![("ui-1".to_owned(), UiResponse::Deny)]
        );
    }

    #[gpui::test]
    async fn approve_sends_approve_exactly_once(cx: &mut TestAppContext) {
        let (connection, thread, request) = setup(UiRequestKind::Approval, Vec::new(), cx).await;
        let (prompt, cx) =
            cx.add_window_view(|window, cx| UiRequestPrompt::new(thread, request, window, cx));
        prompt.update_in(cx, |prompt, window, cx| prompt.approve(window, cx));
        // A second interaction must be ignored (answer-once at the widget too).
        prompt.update_in(cx, |prompt, window, cx| prompt.deny(window, cx));
        cx.run_until_parked();
        assert_eq!(
            connection.ui_responses(),
            vec![("ui-1".to_owned(), UiResponse::Approve)]
        );
    }

    #[gpui::test]
    async fn select_routes_the_chosen_option_id(cx: &mut TestAppContext) {
        let options = vec![
            acp_thread::UiRequestOption {
                id: "a".into(),
                label: "Option A".into(),
            },
            acp_thread::UiRequestOption {
                id: "b".into(),
                label: "Option B".into(),
            },
        ];
        let (connection, thread, request) = setup(UiRequestKind::Select, options, cx).await;
        let (prompt, cx) =
            cx.add_window_view(|window, cx| UiRequestPrompt::new(thread, request, window, cx));
        prompt.update(cx, |prompt, cx| prompt.select("b".into(), cx));
        cx.run_until_parked();
        assert_eq!(
            connection.ui_responses(),
            vec![("ui-1".to_owned(), UiResponse::Select("b".into()))]
        );
    }

    #[gpui::test]
    async fn open_url_requires_explicit_action(cx: &mut TestAppContext) {
        let (connection, thread, request) = setup(UiRequestKind::OpenUrl, Vec::new(), cx).await;
        let (prompt, cx) =
            cx.add_window_view(|window, cx| UiRequestPrompt::new(thread, request, window, cx));
        cx.run_until_parked();
        // Nothing is sent until the user acts: no auto-navigation, no response.
        assert!(connection.ui_responses().is_empty());
        prompt.update_in(cx, |prompt, window, cx| prompt.open_url(window, cx));
        cx.run_until_parked();
        assert_eq!(
            connection.ui_responses(),
            vec![("ui-1".to_owned(), UiResponse::Approve)]
        );
    }
}
