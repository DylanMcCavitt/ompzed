use acp_thread::{AcpThread, Subagent};
use collections::HashSet;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Subscription,
    Window,
};
use ui::prelude::*;

/// Renders the runtime's child-agent telemetry as a default-collapsed, nested
/// tree tied to the active thread, with a drill-in inspector that tails one
/// subagent's streamed transcript.
///
/// The tree reads [`AcpThread::subagents`] live: live progress updates a node in
/// place (the bridge dedupes by id), so re-rendering never duplicates rows.
/// Expansion and the selected (inspected) node persist across re-renders;
/// because the widget is bound to one thread, switching sessions rebinds it and
/// the inspector follows the active thread.
pub struct SubagentTree {
    thread: Entity<AcpThread>,
    focus_handle: FocusHandle,
    /// Subagent ids whose children are revealed. Empty by default, so the tree
    /// opens collapsed to its roots (status-first).
    expanded: HashSet<SharedString>,
    /// The subagent whose transcript the inspector is tailing, if any.
    selected: Option<SharedString>,
    /// Repaints the tree when the thread emits a change (a subagent frame
    /// upsert/transcript append calls `cx.notify()` on the thread); a view only
    /// re-renders on its own notify, so this observation keeps live telemetry
    /// flowing.
    _thread_subscription: Subscription,
}

impl SubagentTree {
    pub fn new(thread: Entity<AcpThread>, cx: &mut Context<Self>) -> Self {
        let _thread_subscription = cx.observe(&thread, |_, _, cx| cx.notify());
        Self {
            thread,
            focus_handle: cx.focus_handle(),
            expanded: HashSet::default(),
            selected: None,
            _thread_subscription,
        }
    }

    /// The thread this tree is bound to, so the panel can rebind on session
    /// switch.
    pub fn thread(&self) -> &Entity<AcpThread> {
        &self.thread
    }

    fn toggle_expanded(&mut self, id: SharedString, cx: &mut Context<Self>) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        cx.notify();
    }

    fn toggle_selected(&mut self, id: SharedString, cx: &mut Context<Self>) {
        if self.selected.as_ref() == Some(&id) {
            self.selected = None;
        } else {
            self.selected = Some(id);
        }
        cx.notify();
    }

    /// Flatten the subagents into pre-order `(node, depth)` rows, descending
    /// into a node's children only when it is expanded. Children link to parents
    /// by [`Subagent::parent_id`]; roots have no parent.
    fn visible_rows(
        subagents: &[Subagent],
        expanded: &HashSet<SharedString>,
    ) -> Vec<(Subagent, usize)> {
        fn walk(
            subagents: &[Subagent],
            parent: Option<&str>,
            depth: usize,
            expanded: &HashSet<SharedString>,
            out: &mut Vec<(Subagent, usize)>,
        ) {
            for node in subagents
                .iter()
                .filter(|node| node.parent_id.as_deref() == parent)
            {
                out.push((node.clone(), depth));
                if expanded.contains(&node.id) {
                    walk(subagents, Some(node.id.as_ref()), depth + 1, expanded, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(subagents, None, 0, expanded, &mut out);
        out
    }

    fn has_children(subagents: &[Subagent], id: &str) -> bool {
        subagents
            .iter()
            .any(|node| node.parent_id.as_deref() == Some(id))
    }

    fn status_color(status: &str) -> Color {
        match status {
            "completed" | "done" | "finished" | "succeeded" => Color::Success,
            "failed" | "error" | "errored" | "cancelled" | "canceled" => Color::Error,
            "" => Color::Muted,
            _ => Color::Accent,
        }
    }

    fn counters_label(node: &Subagent) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(tool_count) = node.tool_count.filter(|count| *count > 0) {
            parts.push(format!("{tool_count} tools"));
        }
        if let Some(tokens) = node.tokens.filter(|tokens| *tokens > 0) {
            parts.push(format!("{tokens} tok"));
        }
        if let Some(cost) = node.cost.filter(|cost| *cost > 0.0) {
            parts.push(format!("${cost:.2}"));
        }
        if let Some(model) = &node.model {
            parts.push(model.to_string());
        }
        (!parts.is_empty()).then(|| parts.join("  ·  "))
    }

    fn render_node(
        &self,
        node: &Subagent,
        depth: usize,
        has_children: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = node.id.clone();
        let is_expanded = self.expanded.contains(&id);
        let counters = Self::counters_label(node);

        let toggle = if has_children {
            let toggle_id = id.clone();
            IconButton::new(
                SharedString::from(format!("subagent-toggle-{id}")),
                if is_expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                },
            )
            .icon_size(IconSize::Small)
            .on_click(
                cx.listener(move |this, _, _, cx| this.toggle_expanded(toggle_id.clone(), cx)),
            )
            .into_any_element()
        } else {
            div().w(px(16.)).into_any_element()
        };

        let select_id = id.clone();
        h_flex()
            .w_full()
            .gap_1()
            .pl(px(4. + depth as f32 * 14.))
            .child(toggle)
            .child(
                h_flex()
                    .id(SharedString::from(format!("subagent-row-{id}")))
                    .flex_1()
                    .gap_1p5()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_selected(select_id.clone(), cx)
                    }))
                    .child(Label::new(id).size(LabelSize::Small))
                    .child(
                        Label::new(node.agent.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .when(!node.status.is_empty(), |row| {
                        row.child(
                            Label::new(node.status.clone())
                                .size(LabelSize::Small)
                                .color(Self::status_color(&node.status)),
                        )
                    })
                    .when_some(counters, |row, counters| {
                        row.child(
                            Label::new(counters)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_inspector(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = self.selected.clone()?;
        let lines = self
            .thread
            .read(cx)
            .subagent_transcript(id.as_ref())
            .to_vec();
        let body = if lines.is_empty() {
            vec![
                Label::new("No messages yet")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
            ]
        } else {
            lines
                .into_iter()
                .map(|line| Label::new(line).size(LabelSize::Small).into_any_element())
                .collect()
        };
        Some(
            v_flex()
                .gap_0p5()
                .mt_1()
                .pl(px(8.))
                .child(
                    h_flex()
                        .w_full()
                        .justify_between()
                        .child(
                            Label::new(format!("Transcript · {id}"))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(Button::new("subagent-inspector-close", "Close").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.selected = None;
                                cx.notify();
                            }),
                        )),
                )
                .children(body)
                .into_any_element(),
        )
    }
}

impl Focusable for SubagentTree {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SubagentTree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let subagents = self.thread.read(cx).subagents().to_vec();
        if subagents.is_empty() {
            return div().into_any_element();
        }
        let rows = Self::visible_rows(&subagents, &self.expanded);
        v_flex()
            .gap_0p5()
            .p_2()
            .child(
                Label::new(format!("Subagents ({})", subagents.len()))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .children(rows.into_iter().map(|(node, depth)| {
                let has_children = Self::has_children(&subagents, node.id.as_ref());
                self.render_node(&node, depth, has_children, cx)
            }))
            .children(self.render_inspector(cx))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, parent: Option<&str>, index: u32) -> Subagent {
        Subagent {
            id: id.into(),
            agent: "quick_task".into(),
            parent_id: parent.map(Into::into),
            index,
            status: "running".into(),
            ..Default::default()
        }
    }

    fn rows(subagents: &[Subagent], expanded: &[&str]) -> Vec<(String, usize)> {
        let expanded: HashSet<SharedString> = expanded.iter().map(|id| (*id).into()).collect();
        SubagentTree::visible_rows(subagents, &expanded)
            .into_iter()
            .map(|(node, depth)| (node.id.to_string(), depth))
            .collect()
    }

    #[test]
    fn collapses_to_roots_by_default() {
        let subagents = [
            node("TaskA", None, 0),
            node("TaskB", None, 1),
            node("TaskA1", Some("TaskA"), 0),
        ];
        // No ids expanded: only the roots show, children stay hidden.
        assert_eq!(
            rows(&subagents, &[]),
            vec![("TaskA".to_string(), 0), ("TaskB".to_string(), 0)]
        );
        assert!(SubagentTree::has_children(&subagents, "TaskA"));
        assert!(!SubagentTree::has_children(&subagents, "TaskB"));
    }

    #[test]
    fn expanding_a_root_reveals_its_children_in_place() {
        let subagents = [
            node("TaskA", None, 0),
            node("TaskB", None, 1),
            node("TaskA1", Some("TaskA"), 0),
        ];
        assert_eq!(
            rows(&subagents, &["TaskA"]),
            vec![
                ("TaskA".to_string(), 0),
                ("TaskA1".to_string(), 1),
                ("TaskB".to_string(), 0),
            ]
        );
    }

    #[test]
    fn nests_grandchildren_at_increasing_depth() {
        let subagents = [
            node("TaskA", None, 0),
            node("TaskA1", Some("TaskA"), 0),
            node("TaskA1a", Some("TaskA1"), 0),
        ];
        // Collapsed grandparent hides the whole subtree.
        assert_eq!(rows(&subagents, &[]), vec![("TaskA".to_string(), 0)]);
        // Expanding only the root reveals one level.
        assert_eq!(
            rows(&subagents, &["TaskA"]),
            vec![("TaskA".to_string(), 0), ("TaskA1".to_string(), 1)]
        );
        // Expanding both levels reveals the grandchild at depth 2.
        assert_eq!(
            rows(&subagents, &["TaskA", "TaskA1"]),
            vec![
                ("TaskA".to_string(), 0),
                ("TaskA1".to_string(), 1),
                ("TaskA1a".to_string(), 2),
            ]
        );
    }

    #[test]
    fn counters_label_shows_only_present_nonzero_fields() {
        let mut node = node("TaskA", None, 0);
        node.tool_count = Some(3);
        node.tokens = Some(0);
        node.cost = Some(0.0);
        node.model = Some("gpt".into());
        assert_eq!(
            SubagentTree::counters_label(&node).as_deref(),
            Some("3 tools  ·  gpt")
        );

        let bare = node_bare();
        assert_eq!(SubagentTree::counters_label(&bare), None);
    }

    fn node_bare() -> Subagent {
        Subagent {
            id: "TaskZ".into(),
            ..Default::default()
        }
    }

    #[test]
    fn status_color_maps_lifecycle_states() {
        assert_eq!(SubagentTree::status_color("completed"), Color::Success);
        assert_eq!(SubagentTree::status_color("failed"), Color::Error);
        assert_eq!(SubagentTree::status_color("running"), Color::Accent);
        assert_eq!(SubagentTree::status_color(""), Color::Muted);
    }
}
