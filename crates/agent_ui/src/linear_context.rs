use acp_thread::AcpThread;
use agent_servers::{LINEAR_CONTEXT_DEFAULT_LIMIT, LinearContext, LinearIssue, LinearTeam};
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Task, Window,
};
use http_client::HttpClient;
use std::sync::Arc;
use ui::prelude::*;

/// Read-only Linear context surface for the active OMP thread (AGE-646).
///
/// On creation (and on connect/disconnect) it resolves the Linear API key and
/// fetches context through [`agent_servers::load_linear_context`], which keeps
/// the key inside a background task and returns only mapped data — this view
/// never holds or renders the credential. It shows a collapsed summary header
/// that expands to the viewer's teams, projects, and open issues, or a muted
/// "not connected" status with a Connect action. Bound to one thread; the panel
/// rebinds it on session switch.
pub struct LinearContextView {
    thread: Entity<AcpThread>,
    focus_handle: FocusHandle,
    state: LoadState,
    expanded: bool,
    /// Holds the in-flight load so it is cancelled if the view is dropped before
    /// the result lands.
    _task: Task<()>,
}

enum LoadState {
    Loading,
    Loaded(LinearContext),
}

impl LinearContextView {
    pub fn new(thread: Entity<AcpThread>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            thread,
            focus_handle: cx.focus_handle(),
            state: LoadState::Loading,
            expanded: false,
            _task: Task::ready(()),
        };
        this.reload(cx);
        this
    }

    /// The thread this view is bound to, so the panel can rebind on session
    /// switch.
    pub fn thread(&self) -> &Entity<AcpThread> {
        &self.thread
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.state = LoadState::Loading;
        let http: Arc<dyn HttpClient> = self
            .thread
            .read(cx)
            .project()
            .read(cx)
            .client()
            .http_client();
        // The key is resolved and used entirely inside this task; only the
        // mapped LinearContext returns to the view.
        let load = agent_servers::load_linear_context(cx, http, LINEAR_CONTEXT_DEFAULT_LIMIT);
        self._task = cx.spawn(async move |this, cx| {
            let context = load.await;
            this.update(cx, |this, cx| {
                this.state = LoadState::Loaded(context);
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    /// Seed the keychain from `LINEAR_API_KEY` (when present) so the connection
    /// persists, then reload. The key never enters this view.
    fn connect(&mut self, cx: &mut Context<Self>) {
        if let Some(store) = agent_servers::connect_linear_from_env(cx) {
            store.detach_and_log_err(cx);
        }
        self.reload(cx);
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        agent_servers::clear_linear_api_key(cx).detach_and_log_err(cx);
        self.reload(cx);
    }

    fn toggle_expanded(&mut self, cx: &mut Context<Self>) {
        self.expanded = !self.expanded;
        cx.notify();
    }

    fn is_expandable(context: &LinearContext) -> bool {
        context.authenticated
            && (!context.teams.is_empty()
                || !context.projects.is_empty()
                || !context.issues.is_empty())
    }

    fn team_label(team: &LinearTeam) -> String {
        format!("{} {}", team.key, team.name)
    }

    fn issue_label(issue: &LinearIssue) -> String {
        format!("{} {}", issue.identifier, issue.title)
    }

    fn render_rows(title: &str, rows: Vec<String>) -> Option<AnyElement> {
        if rows.is_empty() {
            return None;
        }
        Some(
            v_flex()
                .gap_0p5()
                .pl(px(20.))
                .child(
                    Label::new(SharedString::from(title.to_string()))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .children(rows.into_iter().map(|row| {
                    Label::new(row)
                        .size(LabelSize::Small)
                        .into_any_element()
                }))
                .into_any_element(),
        )
    }

    fn render_loading() -> AnyElement {
        Label::new("Loading Linear context…")
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element()
    }

    fn render_loaded(&self, context: &LinearContext, cx: &mut Context<Self>) -> AnyElement {
        let expandable = Self::is_expandable(context);
        let text_color = if context.authenticated {
            Color::Default
        } else {
            Color::Muted
        };

        let toggle = if expandable {
            IconButton::new(
                "linear-context-toggle",
                if self.expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                },
            )
            .icon_size(IconSize::Small)
            .on_click(cx.listener(|this, _, _, cx| this.toggle_expanded(cx)))
            .into_any_element()
        } else {
            div().w(px(16.)).into_any_element()
        };

        let action = if context.authenticated {
            Button::new("linear-disconnect", "Disconnect")
                .on_click(cx.listener(|this, _, _, cx| this.disconnect(cx)))
        } else {
            Button::new("linear-connect", "Connect")
                .on_click(cx.listener(|this, _, _, cx| this.connect(cx)))
        };

        let header = h_flex()
            .w_full()
            .justify_between()
            .child(
                h_flex()
                    .gap_1()
                    .child(toggle)
                    .child(
                        Icon::new(IconName::ListTodo)
                            .size(IconSize::Small)
                            .color(text_color),
                    )
                    .child(
                        Label::new(context.summary_label())
                            .size(LabelSize::Small)
                            .color(text_color),
                    ),
            )
            .child(action);

        let mut column = v_flex().gap_0p5().child(header);
        if expandable && self.expanded {
            column = column
                .children(Self::render_rows(
                    "Teams",
                    context.teams.iter().map(Self::team_label).collect(),
                ))
                .children(Self::render_rows(
                    "Projects",
                    context
                        .projects
                        .iter()
                        .map(|project| project.to_string())
                        .collect(),
                ))
                .children(Self::render_rows(
                    "Open issues",
                    context.issues.iter().map(Self::issue_label).collect(),
                ));
        }
        column.into_any_element()
    }
}

impl Focusable for LinearContextView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for LinearContextView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let context = match &self.state {
            LoadState::Loading => None,
            LoadState::Loaded(context) => Some(context.clone()),
        };
        let content = match context {
            None => Self::render_loading(),
            Some(context) => self.render_loaded(&context, cx),
        };
        v_flex().gap_0p5().p_2().child(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_servers::{LinearIssue, LinearTeam};

    #[test]
    fn row_labels_format_key_and_title() {
        assert_eq!(
            LinearContextView::team_label(&LinearTeam {
                key: "ENG".into(),
                name: "Engineering".into()
            }),
            "ENG Engineering"
        );
        assert_eq!(
            LinearContextView::issue_label(&LinearIssue {
                identifier: "AGE-646".into(),
                title: "Linear bridge".into()
            }),
            "AGE-646 Linear bridge"
        );
    }

    #[test]
    fn expandable_only_when_authenticated_with_rows() {
        let mut context = LinearContext {
            authenticated: true,
            status: None,
            viewer: Some("Ada".into()),
            teams: vec![LinearTeam {
                key: "ENG".into(),
                name: "Engineering".into(),
            }],
            projects: Vec::new(),
            issues: Vec::new(),
        };
        assert!(LinearContextView::is_expandable(&context));

        context.teams.clear();
        assert!(!LinearContextView::is_expandable(&context));

        // Unauthenticated (default): never expandable.
        assert!(!LinearContextView::is_expandable(&LinearContext::default()));
    }
}
