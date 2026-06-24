use acp_thread::AcpThread;
use agent_servers::{GITHUB_CONTEXT_DEFAULT_LIMIT, GithubContext, GithubRef, fetch_github_context};
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Task, Window,
};
use std::path::PathBuf;
use ui::prelude::*;

/// Read-only GitHub context surface for the active OMP thread (AGE-649).
///
/// On creation it kicks off a one-shot `gh` fetch (off the foreground thread)
/// for the thread's first worktree, then renders a collapsed summary header:
/// `owner/repo · N open issues · M open PRs`. Expanding lists the open issues
/// and PRs. When `gh` is missing, unauthenticated, or the directory is not a
/// GitHub repo, the bridge fails closed and the header shows a muted, degraded
/// status line that cannot be expanded. Bound to one thread; the panel rebinds
/// it on session switch.
pub struct GithubContextView {
    thread: Entity<AcpThread>,
    focus_handle: FocusHandle,
    state: LoadState,
    expanded: bool,
    /// Holds the in-flight fetch so it is cancelled if the view is dropped
    /// (e.g. the thread changed) before the result lands.
    _fetch_task: Task<()>,
}

enum LoadState {
    Loading,
    Loaded(GithubContext),
}

impl GithubContextView {
    pub fn new(thread: Entity<AcpThread>, cx: &mut Context<Self>) -> Self {
        let work_dir = Self::work_dir(&thread, cx);
        let fetch = cx.background_spawn(async move {
            match work_dir {
                Some(work_dir) => {
                    fetch_github_context(None, work_dir, GITHUB_CONTEXT_DEFAULT_LIMIT).await
                }
                None => GithubContext::default(),
            }
        });
        let _fetch_task = cx.spawn(async move |this, cx| {
            let context = fetch.await;
            this.update(cx, |this, cx| {
                this.state = LoadState::Loaded(context);
                cx.notify();
            })
            .ok();
        });
        Self {
            thread,
            focus_handle: cx.focus_handle(),
            state: LoadState::Loading,
            expanded: false,
            _fetch_task,
        }
    }

    /// The thread this view is bound to, so the panel can rebind on session
    /// switch.
    pub fn thread(&self) -> &Entity<AcpThread> {
        &self.thread
    }

    fn work_dir(thread: &Entity<AcpThread>, cx: &App) -> Option<PathBuf> {
        thread
            .read(cx)
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
    }

    fn toggle_expanded(&mut self, cx: &mut Context<Self>) {
        self.expanded = !self.expanded;
        cx.notify();
    }

    /// A context is worth expanding only when it resolved a repo and has at
    /// least one open issue or PR to list.
    fn is_expandable(context: &GithubContext) -> bool {
        context.available && (!context.issues.is_empty() || !context.pull_requests.is_empty())
    }

    fn ref_label(reference: &GithubRef) -> String {
        format!("#{} {}", reference.number, reference.title)
    }

    fn render_ref_section(title: &str, refs: &[GithubRef]) -> Option<AnyElement> {
        if refs.is_empty() {
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
                .children(refs.iter().map(|reference| {
                    Label::new(Self::ref_label(reference))
                        .size(LabelSize::Small)
                        .into_any_element()
                }))
                .into_any_element(),
        )
    }

    fn render_loading() -> AnyElement {
        Label::new("Loading GitHub context…")
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element()
    }

    fn render_loaded(&self, context: &GithubContext, cx: &mut Context<Self>) -> AnyElement {
        let expandable = Self::is_expandable(context);
        let text_color = if context.available {
            Color::Default
        } else {
            Color::Muted
        };

        let toggle = if expandable {
            IconButton::new(
                "github-context-toggle",
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

        let header = h_flex()
            .w_full()
            .gap_1()
            .child(toggle)
            .child(
                Icon::new(IconName::Github)
                    .size(IconSize::Small)
                    .color(text_color),
            )
            .child(
                Label::new(context.summary_label())
                    .size(LabelSize::Small)
                    .color(text_color),
            );

        let mut column = v_flex().gap_0p5().child(header);
        if expandable && self.expanded {
            column = column
                .children(Self::render_ref_section("Open issues", &context.issues))
                .children(Self::render_ref_section(
                    "Open pull requests",
                    &context.pull_requests,
                ));
        }
        column.into_any_element()
    }
}

impl Focusable for GithubContextView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GithubContextView {
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
    use agent_servers::GithubRepo;

    fn reference(number: u64, title: &str) -> GithubRef {
        GithubRef {
            number,
            title: title.into(),
        }
    }

    #[test]
    fn ref_label_formats_number_and_title() {
        assert_eq!(
            GithubContextView::ref_label(&reference(42, "Fix it")),
            "#42 Fix it"
        );
    }

    #[test]
    fn expandable_only_when_available_with_rows() {
        let repo = GithubRepo {
            name_with_owner: "octo/demo".into(),
            default_branch: None,
        };
        let with_rows = GithubContext {
            available: true,
            status: None,
            repo: Some(repo.clone()),
            issues: vec![reference(1, "a")],
            pull_requests: Vec::new(),
        };
        assert!(GithubContextView::is_expandable(&with_rows));

        let available_but_empty = GithubContext {
            available: true,
            status: None,
            repo: Some(repo),
            issues: Vec::new(),
            pull_requests: Vec::new(),
        };
        assert!(!GithubContextView::is_expandable(&available_but_empty));

        // Degraded (no `gh` / no repo): never expandable.
        assert!(!GithubContextView::is_expandable(&GithubContext::default()));
    }
}
