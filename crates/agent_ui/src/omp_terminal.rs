use acp_thread::AcpThread;
use agent_servers::{OmpSettings, shell_invocation, spawn_workspace_task};
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Task, Window,
};
use settings::Settings as _;
use std::path::PathBuf;
use ui::prelude::*;

/// Maximum characters of captured stdout/stderr shown in the panel.
const MAX_OUTPUT_CHARS: usize = 4000;

/// User-initiated terminal/task runner surface for the OMP panel (AGE-648).
///
/// Only shown when `omp.terminal_integration` is enabled (off by default). It
/// runs one configured shell command on explicit user click and shows the
/// captured output. It is output-only: there is no input field and the OMP
/// agent has no path to spawn or drive it. The running task is held in `_task`,
/// so closing the panel / switching threads drops it and kills the child
/// (kill-on-drop in `agent_servers::spawn_workspace_task`).
pub struct OmpTerminalView {
    thread: Entity<AcpThread>,
    focus_handle: FocusHandle,
    state: RunState,
    _task: Task<()>,
}

enum RunState {
    Idle,
    Running,
    Done(TaskOutput),
}

struct TaskOutput {
    exit_code: Option<i32>,
    stdout: SharedString,
    stderr: SharedString,
}

impl TaskOutput {
    fn from_output(output: std::process::Output) -> Self {
        Self {
            exit_code: output.status.code(),
            stdout: truncate_output(String::from_utf8_lossy(&output.stdout).into_owned()).into(),
            stderr: truncate_output(String::from_utf8_lossy(&output.stderr).into_owned()).into(),
        }
    }

    fn failure(message: String) -> Self {
        Self {
            exit_code: None,
            stdout: SharedString::default(),
            stderr: message.into(),
        }
    }

    fn status_label(&self) -> String {
        status_label(self.exit_code)
    }
}

fn status_label(exit_code: Option<i32>) -> String {
    match exit_code {
        Some(0) => "Exited 0".to_string(),
        Some(code) => format!("Exited {code}"),
        None => "Failed to run".to_string(),
    }
}

fn truncate_output(mut value: String) -> String {
    if value.chars().count() > MAX_OUTPUT_CHARS {
        let end = value
            .char_indices()
            .nth(MAX_OUTPUT_CHARS)
            .map(|(index, _)| index)
            .unwrap_or(value.len());
        value.truncate(end);
        value.push_str("…");
    }
    value
}

impl OmpTerminalView {
    pub fn new(thread: Entity<AcpThread>, cx: &mut Context<Self>) -> Self {
        // Never auto-runs: the task starts only on explicit user action.
        Self {
            thread,
            focus_handle: cx.focus_handle(),
            state: RunState::Idle,
            _task: Task::ready(()),
        }
    }

    pub fn thread(&self) -> &Entity<AcpThread> {
        &self.thread
    }

    fn work_dir(&self, cx: &App) -> Option<PathBuf> {
        self.thread
            .read(cx)
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
    }

    /// Run the configured task. Invoked only from the user's click.
    fn run(&mut self, cx: &mut Context<Self>) {
        let settings = OmpSettings::get_global(cx);
        let enabled = settings.terminal_integration;
        let Some(command) = settings.terminal_task_command.clone() else {
            return;
        };
        let Some(cwd) = self.work_dir(cx) else {
            return;
        };
        let (program, args) = shell_invocation(&command);

        self.state = RunState::Running;
        cx.notify();

        // Spawn + capture entirely off the foreground thread. The OmpTask lives
        // inside this future, so cancelling it (panel closed) kills the child.
        let output_task = cx.background_spawn(async move {
            let task = spawn_workspace_task(enabled, &program, &args, &cwd)?;
            task.output().await
        });
        self._task = cx.spawn(async move |this, cx| {
            let result = output_task.await;
            this.update(cx, |this, cx| {
                this.state = RunState::Done(match result {
                    Ok(output) => TaskOutput::from_output(output),
                    Err(error) => TaskOutput::failure(error.to_string()),
                });
                cx.notify();
            })
            .ok();
        });
    }

    fn render_output(output: &TaskOutput) -> AnyElement {
        let mut column = v_flex().gap_0p5().pl(px(20.)).child(
            Label::new(output.status_label())
                .size(LabelSize::Small)
                .color(Color::Muted),
        );
        if !output.stdout.is_empty() {
            column = column.child(Label::new(output.stdout.clone()).size(LabelSize::Small));
        }
        if !output.stderr.is_empty() {
            column = column.child(
                Label::new(output.stderr.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error),
            );
        }
        column.into_any_element()
    }
}

impl Focusable for OmpTerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for OmpTerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let command = OmpSettings::get_global(cx).terminal_task_command.clone();
        let running = matches!(self.state, RunState::Running);

        let mut column = v_flex()
            .gap_0p5()
            .p_2()
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(IconName::Terminal)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new("OMP terminal tasks")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                Label::new("User-initiated, output-only — the agent never drives the terminal.")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );

        match command {
            Some(command) => {
                column = column.child(
                    h_flex()
                        .w_full()
                        .justify_between()
                        .child(
                            Label::new(format!("$ {command}"))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Button::new(
                                "omp-run-task",
                                if running { "Running…" } else { "Run task" },
                            )
                            .disabled(running)
                            .on_click(cx.listener(|this, _, _, cx| this.run(cx))),
                        ),
                );
                if let RunState::Done(output) = &self.state {
                    column = column.child(Self::render_output(output));
                }
            }
            None => {
                column = column.child(
                    Label::new("Set `omp.terminal_task_command` to enable a task.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                );
            }
        }
        column.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_label_maps_exit_codes() {
        assert_eq!(status_label(Some(0)), "Exited 0");
        assert_eq!(status_label(Some(2)), "Exited 2");
        assert_eq!(status_label(None), "Failed to run");
    }

    #[test]
    fn truncate_output_caps_long_text() {
        let short = "ok".to_string();
        assert_eq!(truncate_output(short.clone()), short);

        let long = "x".repeat(MAX_OUTPUT_CHARS + 10);
        let truncated = truncate_output(long);
        assert!(truncated.ends_with('…'));
        // MAX_OUTPUT_CHARS chars plus the ellipsis.
        assert_eq!(truncated.chars().count(), MAX_OUTPUT_CHARS + 1);
    }

    #[test]
    fn failure_output_has_no_exit_and_carries_message() {
        let output = TaskOutput::failure("boom".to_string());
        assert_eq!(output.exit_code, None);
        assert_eq!(output.stderr.as_ref(), "boom");
        assert!(output.stdout.is_empty());
        assert_eq!(output.status_label(), "Failed to run");
    }
}
