//! Off-by-default, **user-initiated** terminal/task integration for OMP
//! workflows (AGE-648 / ZED-12).
//!
//! Security model — three boundaries, all asserted by the tests below:
//!
//! 1. **User-initiated only.** [`spawn_workspace_task`] refuses to spawn unless
//!    `enabled` is set (the off-by-default `omp.terminal_integration` setting),
//!    and it is only ever called from an explicit user click in the panel. The
//!    OMP agent has **no** path to this function — driving the terminal from
//!    the agent is an explicit non-goal.
//! 2. **No input channel.** The task is spawned with `stdin` set to `null`.
//!    There is no writer handle anywhere — not on [`OmpTask`], not on the agent
//!    bridge — so agent frames (or anything else) cannot write to a task's
//!    input. Tasks are output-only.
//! 3. **Cleanup on close/shutdown.** The child is spawned `kill_on_drop`, so
//!    dropping the [`OmpTask`] (panel closed, thread switched, app quit, or the
//!    awaiting task cancelled) signals the child.
//!
//! The runner executes one configured shell command to completion and returns
//! its captured output; it is the "task" half of terminal/tasks (run-to-
//! completion), deliberately not an interactive PTY the agent could type into.

use anyhow::{Context as _, Result, ensure};
use std::ffi::OsStr;
use std::path::Path;
use std::process::Output;
use util::command::{Stdio, new_command};

/// A user-initiated workspace task: a spawned child the panel awaits for output.
/// Holds no stdin writer (input is `null`) and kills the child on drop.
pub struct OmpTask {
    child: util::command::Child,
}

impl OmpTask {
    /// OS process id of the spawned child.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Run the task to completion and return its captured output. Consumes the
    /// task; if this future is dropped before it resolves, the child is killed
    /// (kill-on-drop).
    pub async fn output(self) -> Result<Output> {
        Ok(self.child.output().await?)
    }
}

/// Spawn a user-initiated workspace task.
///
/// `enabled` is the resolved `omp.terminal_integration` setting; when false
/// this returns an error and **spawns nothing** — the gate that keeps the
/// feature off by default and user-initiated. The child runs `program args` in
/// `cwd` with **no stdin** (output-only, no agent-writable input) and is killed
/// on drop.
pub fn spawn_workspace_task(
    enabled: bool,
    program: impl AsRef<OsStr>,
    args: &[String],
    cwd: &Path,
) -> Result<OmpTask> {
    ensure!(
        enabled,
        "OMP terminal integration is disabled (set `omp.terminal_integration`)"
    );
    let child = new_command(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawning OMP workspace task")?;
    Ok(OmpTask { child })
}

/// The user's shell, falling back to `/bin/sh`.
pub fn default_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.trim().is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string())
}

/// Build a `(program, args)` shell invocation for a one-line command:
/// `<shell> -lc <command>`.
pub fn shell_invocation(command: &str) -> (String, Vec<String>) {
    (
        default_shell(),
        vec!["-lc".to_string(), command.to_string()],
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    fn process_is_alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Write an executable shell script into a tempdir and return its path.
    fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[test]
    fn disabled_spawns_nothing() {
        let dir = tempfile::tempdir().unwrap();
        // With the feature off, the gate refuses before any spawn happens.
        let result = spawn_workspace_task(
            false,
            "/bin/echo",
            &["should-not-run".to_string()],
            dir.path(),
        );
        assert!(result.is_err());
    }

    #[gpui::test]
    async fn task_has_no_writable_stdin(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let recorded = dir.path().join("stdin.txt");
        // The script copies whatever it receives on stdin into a file. With
        // stdin = null it reads EOF immediately, so the file must be empty —
        // there is no input channel for anyone (including the agent) to write.
        let path = script(
            dir.path(),
            "task.sh",
            &format!("#!/bin/sh\ncat > \"{}\"\n", recorded.display()),
        );
        let task = spawn_workspace_task(true, &path, &[], dir.path()).unwrap();
        let output = task.output().await.unwrap();
        assert!(output.status.success());
        assert_eq!(fs::read_to_string(&recorded).unwrap(), "");
    }

    #[gpui::test]
    async fn output_is_captured(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let path = script(dir.path(), "echo.sh", "#!/bin/sh\necho hello-omp\n");
        let task = spawn_workspace_task(true, &path, &[], dir.path()).unwrap();
        let output = task.output().await.unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello-omp");
    }

    #[gpui::test]
    async fn dropping_task_kills_the_child(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        // A long-running task that would outlive the panel if not cleaned up.
        let path = script(
            dir.path(),
            "loop.sh",
            "#!/bin/sh\nwhile true; do sleep 1; done\n",
        );
        let task = spawn_workspace_task(true, &path, &[], dir.path()).unwrap();
        let pid = task.pid() as i32;
        assert!(process_is_alive(pid));

        // Dropping the task must kill the child (close/shutdown cleanup).
        drop(task);

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while process_is_alive(pid) && std::time::Instant::now() < deadline {
            cx.executor().timer(Duration::from_millis(20)).await;
        }
        assert!(
            !process_is_alive(pid),
            "child process should be killed when the task is dropped"
        );
    }

    #[test]
    fn shell_invocation_builds_login_command() {
        let (_shell, args) = shell_invocation("echo hi");
        assert_eq!(args, vec!["-lc".to_string(), "echo hi".to_string()]);
    }
}
