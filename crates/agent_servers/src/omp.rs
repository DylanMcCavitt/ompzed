use crate::{AgentServer, AgentServerDelegate};
use acp_thread::{
    AcpThread, AgentConnection, Subagent, UiRequest, UiRequestKind, UiRequestOption,
    UiRequestScope, UiResponse, UserMessageId, meta_with_tool_name,
};
use action_log::ActionLog;
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result, anyhow};
use collections::{HashMap, HashSet};
use futures::{
    AsyncBufReadExt as _, AsyncWriteExt as _, FutureExt as _, StreamExt as _, channel::oneshot,
    io::BufReader,
};
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, SharedString, Subscription, Task};
use project::{AgentId, Project};
use serde_json::{Value, json};
use settings::Settings as _;
use std::{
    any::Any,
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    process::Stdio,
    rc::{Rc, Weak},
    time::Duration,
};
use ui::IconName;
use util::ResultExt as _;
use util::{path_list::PathList, process::Child};

pub const OMP_AGENT_ID: &str = "omp";

pub struct OmpAgentServer;

impl AgentServer for OmpAgentServer {
    fn logo(&self) -> IconName {
        IconName::Terminal
    }

    fn agent_id(&self) -> AgentId {
        AgentId::new(OMP_AGENT_ID)
    }

    fn connect(
        &self,
        _delegate: AgentServerDelegate,
        _project: Entity<Project>,
        cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        let command = OmpCommand::from_settings(cx);
        cx.spawn(async move |cx| {
            let connection = Rc::new(OmpAgentConnection::new(command));
            cx.update(|cx| connection.register_app_quit(cx));
            Ok(connection as Rc<dyn AgentConnection>)
        })
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

#[derive(Clone)]
struct OmpCommand {
    program: PathBuf,
    prefix_args: Vec<String>,
}

impl Default for OmpCommand {
    fn default() -> Self {
        Self {
            program: resolve_omp_binary(),
            prefix_args: Vec::new(),
        }
    }
}

impl OmpCommand {
    /// Builds the launch command for a new session, honoring a user-configured
    /// `omp` binary path when one is set and points at an existing file.
    /// Otherwise resolves the binary the usual way.
    fn from_settings(cx: &App) -> Self {
        let configured = OmpSettings::try_get(cx)
            .and_then(|settings| settings.binary_path.clone())
            .filter(|path| path.exists());
        match configured {
            Some(program) => Self {
                program,
                prefix_args: Vec::new(),
            },
            None => Self::default(),
        }
    }
}

fn resolve_omp_binary() -> PathBuf {
    if let Some(binary) = std::env::var_os("OMP_BINARY") {
        return binary.into();
    }
    for candidate in common_tool_dirs().into_iter().map(|dir| dir.join("omp")) {
        if candidate.exists() {
            return candidate;
        }
    }
    "omp".into()
}

fn common_tool_dirs() -> Vec<PathBuf> {
    let mut dirs = vec!["/opt/homebrew/bin".into(), "/usr/local/bin".into()];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".bun/bin"));
        dirs.push(home.join(".local/bin"));
    }
    dirs
}

fn augmented_path() -> Option<std::ffi::OsString> {
    let mut paths = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    for dir in common_tool_dirs() {
        if !paths.iter().any(|path| path == &dir) {
            paths.push(dir);
        }
    }
    std::env::join_paths(paths).ok()
}

/// Workspace-scoped defaults for the built-in OMP agent. Registered with the
/// settings store, so values layer exactly like every other Zed setting: a
/// global default in the user settings, optionally overridden per-workspace in
/// a worktree's `.zed/settings.json`. These survive settings reloads because
/// they are re-derived from the merged settings content on every recompute.
#[derive(Clone, Debug, Default, PartialEq, Eq, settings::RegisterSetting)]
pub struct OmpSettings {
    /// User-configured path to the `omp` binary, if any.
    pub binary_path: Option<PathBuf>,
    /// User-configured workspace config directory, if any.
    pub config_dir: Option<PathBuf>,
}

impl settings::Settings for OmpSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let omp = content.omp.clone().unwrap_or_default();
        Self {
            binary_path: non_empty_path(omp.binary_path),
            config_dir: non_empty_path(omp.config_dir),
        }
    }
}

fn non_empty_path(value: Option<String>) -> Option<PathBuf> {
    value
        .map(|raw| raw.trim().to_owned())
        .filter(|raw| !raw.is_empty())
        .map(PathBuf::from)
}

/// Default workspace-scoped OMP config directory, relative to the workspace
/// root, when the user has not configured one.
const DEFAULT_WORKSPACE_CONFIG_DIR: &str = ".omp";

/// Result of probing a workspace for OMP availability. Every field degrades
/// gracefully when something is missing — nothing here spawns a child, blocks,
/// or panics — so a missing binary or config surfaces as a visible status
/// rather than a silent failure.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OmpDiscovery {
    /// Resolved `omp` binary, when one was found.
    pub binary: Option<PathBuf>,
    /// Workspace config directory, when it exists.
    pub config_dir: Option<PathBuf>,
    /// Number of workspace command files found under `<config>/commands`.
    pub command_count: usize,
}

impl OmpDiscovery {
    pub fn binary_available(&self) -> bool {
        self.binary.is_some()
    }

    pub fn config_available(&self) -> bool {
        self.config_dir.is_some()
    }

    /// A short, user-facing status line. Always non-empty: a missing binary or
    /// config is reported as a visible message instead of nothing.
    pub fn status_label(&self) -> SharedString {
        if self.binary.is_none() {
            return "OMP binary not found — install omp or set its path in settings".into();
        }
        match (&self.config_dir, self.command_count) {
            (None, _) => "OMP ready · no workspace config".into(),
            (Some(_), 0) => "OMP ready · workspace config found".into(),
            (Some(_), 1) => "OMP ready · 1 workspace command".into(),
            (Some(_), count) => format!("OMP ready · {count} workspace commands").into(),
        }
    }
}

/// Probe a workspace for OMP availability without spawning anything.
///
/// `binary_override` and `config_override` come from [`OmpSettings`]; when
/// unset, the binary is resolved the same way the launcher resolves it and the
/// config directory defaults to `.omp` under `work_dir`. Missing pieces degrade
/// to `None`/`0`.
pub fn discover_omp(
    binary_override: Option<&Path>,
    config_override: Option<&Path>,
    work_dir: &Path,
) -> OmpDiscovery {
    let binary = resolve_discovery_binary(binary_override);
    let config_dir = resolve_config_dir(config_override, work_dir).filter(|dir| dir.is_dir());
    let command_count = config_dir
        .as_deref()
        .map(count_workspace_commands)
        .unwrap_or(0);
    OmpDiscovery {
        binary,
        config_dir,
        command_count,
    }
}

fn resolve_discovery_binary(binary_override: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = binary_override
        && path.exists()
    {
        return Some(path.to_path_buf());
    }
    resolve_omp_binary_path()
}

/// Resolve the `omp` binary, returning `None` when no real binary exists — as
/// opposed to [`resolve_omp_binary`], which falls back to the bare name `omp`.
fn resolve_omp_binary_path() -> Option<PathBuf> {
    if let Some(binary) = std::env::var_os("OMP_BINARY") {
        let path = PathBuf::from(binary);
        return path.exists().then_some(path);
    }
    common_tool_dirs()
        .into_iter()
        .map(|dir| dir.join("omp"))
        .find(|candidate| candidate.exists())
}

fn resolve_config_dir(config_override: Option<&Path>, work_dir: &Path) -> Option<PathBuf> {
    let dir = match config_override {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => work_dir.join(path),
        None => work_dir.join(DEFAULT_WORKSPACE_CONFIG_DIR),
    };
    Some(dir)
}

fn count_workspace_commands(config_dir: &Path) -> usize {
    let commands_dir = config_dir.join("commands");
    let Ok(entries) = std::fs::read_dir(&commands_dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| entry.path().is_file())
        .count()
}

struct PendingPrompt {
    id: String,
    tx: oneshot::Sender<Result<acp::StopReason>>,
}

pub struct OmpAgentConnection {
    command: OmpCommand,
    sessions: RefCell<HashMap<acp::SessionId, OmpSession>>,
    next_session_id: Cell<u64>,
    quit_subscription: RefCell<Option<Subscription>>,
}

impl OmpAgentConnection {
    fn new(command: OmpCommand) -> Self {
        Self {
            command,
            sessions: RefCell::default(),
            next_session_id: Cell::new(0),
            quit_subscription: RefCell::default(),
        }
    }

    /// Kill every live OMP child when the app is about to quit so that no
    /// app-owned child outlives Zed.
    fn register_app_quit(self: &Rc<Self>, cx: &mut App) {
        let connection = Rc::downgrade(self);
        let subscription = cx.on_app_quit(move |cx| {
            if let Some(connection) = connection.upgrade() {
                connection.dispose_all(cx);
            }
            async {}
        });
        self.quit_subscription.borrow_mut().replace(subscription);
    }

    /// Tear down every session, killing its child. Used only on shutdown;
    /// closing a single session goes through `close_session` and never
    /// touches its siblings.
    fn dispose_all(&self, cx: &mut App) {
        for (_, session) in self.sessions.borrow_mut().drain() {
            session.state.cancel_active_turn();
            session
                .state
                .reject_pending_requests("OMP agent shutting down");
            session.state.kill(cx);
        }
    }

    /// Fallback id for the rare case where OMP never reports a session
    /// identity: the session works for the current run but cannot be resumed.
    fn synthesized_session_id(&self) -> acp::SessionId {
        let id = self.next_session_id.get();
        self.next_session_id.set(id + 1);
        acp::SessionId::new(format!("omp-{id}"))
    }

    /// Spawn a fresh or resumed OMP child, build the thread, and register the
    /// session.
    ///
    /// A fresh session adopts OMP's own reported session id as the persisted
    /// acp::SessionId, so Zed's existing history store can round-trip a later
    /// resume without a parallel store. A resume preserves the saved id
    /// verbatim and asks OMP to replay prior messages, rehydrating the
    /// transcript without mixing it with any other session.
    fn open_session(
        self: &Rc<Self>,
        resume: Option<acp::SessionId>,
        project: Entity<Project>,
        work_dirs: PathList,
        title: Option<SharedString>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        let Some(work_dir) = work_dirs.ordered_paths().next().cloned() else {
            return Task::ready(Err(anyhow!("OMP agent requires a workspace directory")));
        };
        let command = self.command.clone();
        let connection = self.clone();
        cx.spawn(async move |cx| {
            let resume_id = resume.as_ref().map(|session_id| session_id.0.to_string());
            let state = cx.update(|cx| {
                OmpSessionState::spawn(command, &work_dir, resume_id.as_deref(), cx)
            })?;

            // Learn OMP's own session identity/provenance. A fresh session
            // adopts it as the persisted id; a resume keeps the saved id.
            let descriptor = state.clone().fetch_descriptor(cx).await;
            let session_id = resume.clone().unwrap_or_else(|| {
                descriptor
                    .as_ref()
                    .and_then(OmpDescriptor::acp_session_id)
                    .unwrap_or_else(|| connection.synthesized_session_id())
            });

            let thread = cx.update(|cx| {
                let action_log = cx.new(|_| ActionLog::new(project.clone()));
                cx.new(|cx| {
                    AcpThread::new(
                        None,
                        title.clone(),
                        Some(work_dirs.clone()),
                        connection.clone(),
                        project.clone(),
                        action_log,
                        session_id.clone(),
                        watch::Receiver::constant(acp::PromptCapabilities::new()),
                        cx,
                    )
                })
            });
            state.set_thread(thread.downgrade());
            state.enable_subagent_telemetry();
            state.flush_available_commands(cx);
            if let Some(descriptor) = descriptor.as_ref() {
                state.apply_descriptor(descriptor, cx);
            }
            connection.sessions.borrow_mut().insert(
                session_id,
                OmpSession {
                    state: state.clone(),
                },
            );
            if resume.is_some() {
                state.clone().replay_history(cx).await;
            }
            Ok(thread)
        })
    }
}

impl AgentConnection for OmpAgentConnection {
    fn agent_id(&self) -> AgentId {
        AgentId::new(OMP_AGENT_ID)
    }

    fn telemetry_id(&self) -> SharedString {
        OMP_AGENT_ID.into()
    }

    fn new_session(
        self: Rc<Self>,
        project: Entity<Project>,
        work_dirs: PathList,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        self.open_session(None, project, work_dirs, None, cx)
    }

    fn supports_load_session(&self) -> bool {
        true
    }

    fn load_session(
        self: Rc<Self>,
        session_id: acp::SessionId,
        project: Entity<Project>,
        work_dirs: PathList,
        title: Option<SharedString>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        self.open_session(Some(session_id), project, work_dirs, title, cx)
    }

    fn supports_close_session(&self) -> bool {
        true
    }

    fn close_session(
        self: Rc<Self>,
        session_id: &acp::SessionId,
        cx: &mut App,
    ) -> Task<Result<()>> {
        if let Some(session) = self.sessions.borrow_mut().remove(session_id) {
            session.state.cancel_active_turn();
            session.state.reject_pending_requests("OMP session closed");
            session.state.reject_pending_ui_requests();
            session.state.clear_thread_ui_requests(cx);
            session.state.kill(cx);
        }
        Task::ready(Ok(()))
    }

    fn auth_methods(&self) -> &[acp::AuthMethod] {
        &[]
    }

    fn authenticate(&self, _method: acp::AuthMethodId, _cx: &mut App) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn prompt(
        &self,
        _user_message_id: UserMessageId,
        params: acp::PromptRequest,
        cx: &mut App,
    ) -> Task<Result<acp::PromptResponse>> {
        let Some(session) = self.sessions.borrow().get(&params.session_id).cloned() else {
            return Task::ready(Err(anyhow!("OMP session not found")));
        };
        let prompt = prompt_text(&params.prompt);
        match session.state.prompt(prompt) {
            Ok(rx) => cx.spawn(async move |_| {
                let stop_reason = rx.await.context("OMP prompt response dropped")??;
                Ok(acp::PromptResponse::new(stop_reason))
            }),
            Err(err) => Task::ready(Err(err)),
        }
    }

    fn cancel(&self, session_id: &acp::SessionId, cx: &mut App) {
        if let Some(session) = self.sessions.borrow().get(session_id).cloned() {
            session.state.send_abort();
            session.state.cancel_active_turn();
            session.state.reject_pending_ui_requests();
            session.state.clear_thread_ui_requests(cx);
        }
    }

    fn respond_to_ui_request(
        &self,
        session_id: &acp::SessionId,
        request_id: &str,
        response: UiResponse,
        _cx: &mut App,
    ) {
        if let Some(session) = self.sessions.borrow().get(session_id) {
            session.state.respond_ui_request(request_id, response);
        }
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

#[derive(Clone)]
struct OmpSession {
    state: Rc<OmpSessionState>,
}

/// OMP's own view of a session, parsed from a `get_state` response.
///
/// The `session_id` is OMP's stable session identity (adopted as the
/// acp::SessionId for a fresh session so resume round-trips through Zed's
/// history store); `session_file` is the JSONL trace path kept as provenance.
struct OmpDescriptor {
    session_id: Option<String>,
    session_file: Option<String>,
    title: Option<String>,
}

impl OmpDescriptor {
    fn acp_session_id(&self) -> Option<acp::SessionId> {
        self.session_id
            .as_deref()
            .or(self.session_file.as_deref())
            .map(|id| acp::SessionId::new(id.to_owned()))
    }
}

struct OmpSessionState {
    outgoing: async_channel::Sender<Value>,
    child: RefCell<Option<Child>>,
    thread: RefCell<Option<gpui::WeakEntity<AcpThread>>>,
    pending_prompt: RefCell<Option<PendingPrompt>>,
    pending_requests: RefCell<HashMap<String, oneshot::Sender<Result<Value, String>>>>,
    /// Ids of `extension_ui_request`s surfaced to the panel and not yet
    /// answered. Removed when answered, or cleared on cancel/exit so a request
    /// can never be answered twice or after the session is cancelled.
    pending_ui_requests: RefCell<HashSet<String>>,
    /// Maps a tool-call id to the subagent that made it, learned from streamed
    /// `subagent_event` tool executions. A child's `parentToolCallId` is
    /// resolved through this map to its parent subagent's id; an id absent here
    /// was a main-agent tool call, so that child is a tree root.
    subagent_tool_owners: RefCell<HashMap<String, SharedString>>,
    /// Latest slash commands OMP reported via `available_commands_update`,
    /// cached because the first update arrives at startup before the panel
    /// thread is attached; flushed to the thread on attach and updated on each
    /// later frame so the composer's command picker tracks the active session.
    available_commands: RefCell<Vec<acp::AvailableCommand>>,
    closed: Cell<bool>,
    next_request_id: Cell<u64>,
    _read_task: Task<()>,
    _write_task: Task<Result<()>>,
    _stderr_task: Task<Result<()>>,
}

impl OmpSessionState {
    fn spawn(
        command: OmpCommand,
        work_dir: &Path,
        resume: Option<&str>,
        cx: &mut App,
    ) -> Result<Rc<Self>> {
        let mut cmd = std::process::Command::new(&command.program);
        cmd.args(&command.prefix_args)
            .arg("--mode")
            .arg("rpc-ui")
            .arg("--cwd")
            .arg(work_dir)
            .arg("--approval-mode")
            .arg("always-ask");
        if let Some(resume) = resume {
            cmd.arg("--resume").arg(resume);
        }
        cmd.current_dir(work_dir);
        if let Some(path) = augmented_path() {
            cmd.env("PATH", path);
        }

        // Surface a clear, user-facing status when the agent can't start (most
        // often a missing binary). This error propagates up through
        // `new_session` and is rendered by the agent panel's "Failed to Launch"
        // surface, so a missing OMP degrades visibly instead of with a cryptic
        // OS error.
        let mut child = Child::spawn(cmd, Stdio::piped(), Stdio::piped(), Stdio::piped())
            .with_context(|| {
                format!(
                    "Could not start the OMP agent ({}). Install omp or set `omp.binary_path` in settings.",
                    command.program.display()
                )
            })?;
        let stdin = child.stdin.take().context("failed to take OMP stdin")?;
        let stdout = child.stdout.take().context("failed to take OMP stdout")?;
        let stderr = child.stderr.take().context("failed to take OMP stderr")?;
        let (outgoing, incoming) = async_channel::unbounded::<Value>();

        let write_task = cx.background_spawn(async move {
            let mut stdin = stdin;
            while let Ok(frame) = incoming.recv().await {
                stdin.write_all(frame.to_string().as_bytes()).await?;
                stdin.write_all(b"\n").await?;
                stdin.flush().await?;
            }
            Ok(())
        });

        let stderr_task = cx.background_spawn(async move {
            let mut stderr = BufReader::new(stderr);
            let mut line = String::new();
            while let Ok(n) = stderr.read_line(&mut line).await
                && n > 0
            {
                log::warn!("omp stderr: {}", line.trim_end_matches(['\n', '\r']));
                line.clear();
            }
            Ok(())
        });

        Ok(Rc::new_cyclic(|weak: &Weak<OmpSessionState>| {
            let mut lines = BufReader::new(stdout).lines();
            let weak = weak.clone();
            let read_task = cx.spawn(async move |cx| {
                while let Some(line) = lines.next().await {
                    let Ok(line) = line else {
                        continue;
                    };
                    let Some(state) = Weak::upgrade(&weak) else {
                        break;
                    };
                    state.dispatch_line(&line, cx);
                }
                if let Some(state) = Weak::upgrade(&weak) {
                    state.mark_child_exited(cx);
                }
            });

            Self {
                outgoing,
                child: RefCell::new(Some(child)),
                thread: RefCell::default(),
                pending_prompt: RefCell::default(),
                pending_requests: RefCell::default(),
                pending_ui_requests: RefCell::default(),
                subagent_tool_owners: RefCell::default(),
                available_commands: RefCell::default(),
                closed: Cell::new(false),
                next_request_id: Cell::new(0),
                _read_task: read_task,
                _write_task: write_task,
                _stderr_task: stderr_task,
            }
        }))
    }

    fn set_thread(&self, thread: gpui::WeakEntity<AcpThread>) {
        self.thread.borrow_mut().replace(thread);
    }

    fn prompt(&self, prompt: String) -> Result<oneshot::Receiver<Result<acp::StopReason>>> {
        if self.pending_prompt.borrow().is_some() {
            anyhow::bail!("OMP prompt already in progress");
        }
        let (tx, rx) = oneshot::channel();
        let id = self.next_request_id();
        self.pending_prompt
            .borrow_mut()
            .replace(PendingPrompt { id: id.clone(), tx });
        if let Err(err) = self.send_frame(json!({
            "type": "prompt",
            "id": id,
            "message": prompt,
        })) {
            self.pending_prompt.borrow_mut().take();
            return Err(err);
        }
        Ok(rx)
    }

    fn send_abort(&self) {
        self.send_frame(json!({
            "type": "abort",
            "id": self.next_request_id(),
        }))
        .log_err();
    }

    /// Turn on subagent telemetry for this session so OMP streams
    /// `subagent_lifecycle`/`subagent_progress`/`subagent_event` frames as the
    /// agent spawns child agents. Sent once after the session is established;
    /// fire-and-forget (OMP's ack arrives as a `response` we don't await).
    fn enable_subagent_telemetry(&self) {
        self.send_frame(json!({
            "type": "set_subagent_subscription",
            "id": self.next_request_id(),
            "level": "events",
        }))
        .log_err();
    }

    /// Send `get_state` and await OMP's reported session descriptor (id,
    /// trace-file provenance, and display title), bounded by a timeout so a
    /// silent child never wedges session creation.
    async fn fetch_descriptor(self: Rc<Self>, cx: &mut AsyncApp) -> Option<OmpDescriptor> {
        let rx = self
            .send_request("get_state", json!({ "type": "get_state" }))
            .ok()?;
        let mut rx = rx.fuse();
        let mut timer = cx
            .background_executor()
            .timer(Duration::from_secs(10))
            .fuse();
        futures::select_biased! {
            result = rx => match result {
                Ok(Ok(state)) => Some(parse_descriptor(&state)),
                _ => None,
            },
            _ = timer => {
                log::warn!("OMP get_state timed out; session will not be resumable");
                None
            }
        }
    }

    /// Ask a resumed OMP child to replay its saved transcript and await the
    /// terminating response. OMP streams the prior `history_message` frames
    /// before that response, so once it resolves the thread has rehydrated.
    async fn replay_history(self: Rc<Self>, cx: &mut AsyncApp) {
        let Ok(rx) = self.send_request("get_history", json!({ "type": "get_history" })) else {
            return;
        };
        let mut rx = rx.fuse();
        let mut timer = cx
            .background_executor()
            .timer(Duration::from_secs(10))
            .fuse();
        futures::select_biased! {
            _ = rx => {}
            _ = timer => log::warn!("OMP get_history (resume replay) timed out"),
        }
    }

    fn send_request(
        &self,
        command: &str,
        mut frame: Value,
    ) -> Result<oneshot::Receiver<Result<Value, String>>> {
        let id = self.next_request_id();
        frame["id"] = Value::String(id.clone());
        let (tx, rx) = oneshot::channel();
        self.pending_requests.borrow_mut().insert(id.clone(), tx);
        if let Err(err) = self.send_frame(frame) {
            self.pending_requests.borrow_mut().remove(&id);
            anyhow::bail!("failed to send OMP {command} request: {err}");
        }
        Ok(rx)
    }

    fn next_request_id(&self) -> String {
        let id = self.next_request_id.get();
        self.next_request_id.set(id + 1);
        format!("req_{id}")
    }

    fn send_frame(&self, frame: Value) -> Result<()> {
        if self.closed.get() {
            anyhow::bail!("OMP child exited");
        }
        self.outgoing
            .try_send(frame)
            .map_err(|_| anyhow!("OMP child stdin is closed"))
    }

    fn dispatch_line(&self, line: &str, cx: &mut AsyncApp) {
        let Ok(frame) = serde_json::from_str::<Value>(line) else {
            log::warn!("failed to parse OMP frame: {line}");
            return;
        };
        match frame.get("type").and_then(Value::as_str) {
            Some("response") => self.resolve_response(frame),
            Some("message_update") => self.handle_message_update(&frame, cx),
            Some("message_end") => self.handle_message_end(&frame, cx),
            Some("tool_execution_start") => self.handle_tool_execution_start(&frame, cx),
            Some("tool_execution_end") => self.handle_tool_execution_end(&frame, cx),
            Some("agent_end") => self.complete_prompt(acp::StopReason::EndTurn),
            Some("turn_end") => self.handle_turn_end(&frame),
            Some("history_message") => self.handle_history_message(&frame, cx),
            Some("extension_ui_request") => self.handle_ui_request(&frame, cx),
            Some("subagent_lifecycle") | Some("subagent_progress") => {
                self.handle_subagent_telemetry(&frame, cx)
            }
            Some("subagent_event") => self.handle_subagent_event(&frame, cx),
            Some("available_commands_update") => self.handle_available_commands(&frame, cx),
            Some("command_output") => self.handle_command_output(&frame, cx),
            Some("prompt_result") => self.handle_prompt_result(&frame),
            _ => {}
        }
    }

    fn resolve_response(&self, frame: Value) {
        let Some(id) = frame.get("id").and_then(Value::as_str) else {
            return;
        };
        let is_error = frame.get("success").and_then(Value::as_bool) == Some(false);
        let is_pending_prompt = self
            .pending_prompt
            .borrow()
            .as_ref()
            .is_some_and(|prompt| prompt.id == id);
        if is_pending_prompt {
            if is_error {
                if let Some(prompt) = self.pending_prompt.borrow_mut().take() {
                    prompt.tx.send(Err(anyhow!(response_error(&frame)))).ok();
                }
            } else if prompt_completed_locally(&frame) {
                // A local-only prompt (e.g. a slash command OMP runs without a
                // model turn) acks with `data.agentInvoked: false` and emits no
                // `agent_end`/`turn_end`, so resolve the turn now or it hangs.
                self.complete_prompt(acp::StopReason::EndTurn);
            }
            return;
        }
        let Some(tx) = self.pending_requests.borrow_mut().remove(id) else {
            return;
        };
        let result = if is_error {
            Err(response_error(&frame))
        } else {
            Ok(frame.get("data").cloned().unwrap_or(Value::Null))
        };
        tx.send(result).ok();
    }

    fn handle_message_update(&self, frame: &Value, cx: &mut AsyncApp) {
        let Some(event) = frame.get("assistantMessageEvent") else {
            return;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("text_delta") => {
                if let Some(delta) = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .filter(|delta| !delta.is_empty())
                {
                    self.send_session_update(
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                            delta.to_owned().into(),
                        )),
                        cx,
                    );
                }
            }
            Some("thinking_delta") => {
                if let Some(delta) = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .filter(|delta| !delta.is_empty())
                {
                    self.send_session_update(
                        acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(
                            delta.to_owned().into(),
                        )),
                        cx,
                    );
                }
            }
            Some("toolcall_end") => {
                if let Some(tool_call) = event.get("toolCall") {
                    self.send_tool_call(
                        tool_call,
                        acp::ToolCallStatus::Pending,
                        Vec::new(),
                        None,
                        cx,
                    );
                }
            }
            Some("error") => {
                if let Some(message) = event
                    .get("error")
                    .and_then(|error| error.get("errorMessage").or_else(|| error.get("message")))
                    .and_then(Value::as_str)
                    .filter(|message| !message.is_empty())
                {
                    self.send_session_update(
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                            message.to_owned().into(),
                        )),
                        cx,
                    );
                }
            }
            _ => {}
        }
    }

    fn handle_message_end(&self, frame: &Value, cx: &mut AsyncApp) {
        let Some(message) = frame.get("message") else {
            return;
        };
        match message.get("role").and_then(Value::as_str) {
            Some("toolResult") => {
                let status = if message.get("isError").and_then(Value::as_bool) == Some(true) {
                    acp::ToolCallStatus::Failed
                } else {
                    acp::ToolCallStatus::Completed
                };
                self.send_tool_result(message, status, cx);
            }
            Some("assistant") => {
                if let Some(error) = message
                    .get("errorMessage")
                    .and_then(Value::as_str)
                    .filter(|error| !error.is_empty())
                {
                    self.send_session_update(
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                            error.to_owned().into(),
                        )),
                        cx,
                    );
                }
            }
            _ => {}
        }
    }

    fn handle_tool_execution_start(&self, frame: &Value, cx: &mut AsyncApp) {
        self.send_tool_call(frame, acp::ToolCallStatus::InProgress, Vec::new(), None, cx);
    }

    fn handle_tool_execution_end(&self, frame: &Value, cx: &mut AsyncApp) {
        let status = if frame.get("isError").and_then(Value::as_bool) == Some(true) {
            acp::ToolCallStatus::Failed
        } else {
            acp::ToolCallStatus::Completed
        };
        self.send_tool_result(frame, status, cx);
    }

    fn send_tool_result(&self, frame: &Value, status: acp::ToolCallStatus, cx: &mut AsyncApp) {
        let content = tool_result_content(frame);
        let raw_output = Some(
            frame
                .get("result")
                .cloned()
                .unwrap_or_else(|| frame.clone()),
        );
        self.send_tool_call(frame, status, content, raw_output, cx);
    }

    fn send_tool_call(
        &self,
        frame: &Value,
        status: acp::ToolCallStatus,
        content: Vec<acp::ToolCallContent>,
        raw_output: Option<Value>,
        cx: &mut AsyncApp,
    ) {
        let Some(id) = frame
            .get("id")
            .or_else(|| frame.get("toolCallId"))
            .and_then(Value::as_str)
        else {
            return;
        };
        let Some(name) = frame
            .get("name")
            .or_else(|| frame.get("toolName"))
            .and_then(Value::as_str)
        else {
            return;
        };
        let arguments = frame
            .get("arguments")
            .or_else(|| frame.get("args"))
            .cloned();
        let title = tool_title(
            name,
            arguments.as_ref().unwrap_or(&Value::Null),
            frame.get("intent").and_then(Value::as_str),
        );
        let mut call = acp::ToolCall::new(id.to_owned(), title)
            .kind(tool_kind(name))
            .status(status)
            .meta(meta_with_tool_name(name));
        if let Some(arguments) = arguments {
            call = call.raw_input(arguments);
        }
        if !content.is_empty() {
            call = call.content(content);
        }
        if let Some(raw_output) = raw_output {
            call = call.raw_output(raw_output);
        }
        self.send_session_update(acp::SessionUpdate::ToolCall(call), cx);
    }

    fn send_session_update(&self, update: acp::SessionUpdate, cx: &mut AsyncApp) {
        if let Some(thread) = self
            .thread
            .borrow()
            .as_ref()
            .and_then(|thread| thread.upgrade())
        {
            thread
                .update(cx, |thread, cx| thread.handle_session_update(update, cx))
                .log_err();
        }
    }

    /// Surface OMP's `available_commands_update` to the panel composer's slash
    /// picker. Caches the latest list (the first update arrives at startup,
    /// before the thread attaches) and pushes it when a thread is present.
    fn handle_available_commands(&self, frame: &Value, cx: &mut AsyncApp) {
        let commands = parse_available_commands(frame);
        *self.available_commands.borrow_mut() = commands.clone();
        self.send_session_update(
            acp::SessionUpdate::AvailableCommandsUpdate(acp::AvailableCommandsUpdate::new(
                commands,
            )),
            cx,
        );
    }

    /// Replay the cached command list once a thread is attached, covering the
    /// startup `available_commands_update` that arrived before the panel.
    fn flush_available_commands(&self, cx: &mut AsyncApp) {
        let commands = self.available_commands.borrow().clone();
        if commands.is_empty() {
            return;
        }
        self.send_session_update(
            acp::SessionUpdate::AvailableCommandsUpdate(acp::AvailableCommandsUpdate::new(
                commands,
            )),
            cx,
        );
    }

    /// Surface the textual output of a local slash command (e.g. `/fast status`)
    /// as an agent message chunk so it lands in the transcript.
    fn handle_command_output(&self, frame: &Value, cx: &mut AsyncApp) {
        if let Some(text) = frame
            .get("text")
            .or_else(|| frame.get("output"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            self.send_session_update(
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    text.to_owned().into(),
                )),
                cx,
            );
        }
    }

    /// A prompt accepted immediately but resolving local-only emits
    /// `prompt_result` with `agentInvoked:false` and no `agent_end`; complete the
    /// matching turn so it doesn't hang waiting for events that never arrive.
    fn handle_prompt_result(&self, frame: &Value) {
        if frame.get("agentInvoked").and_then(Value::as_bool) != Some(false) {
            return;
        }
        let matches_pending = match frame.get("id").and_then(Value::as_str) {
            Some(id) => self
                .pending_prompt
                .borrow()
                .as_ref()
                .is_some_and(|prompt| prompt.id == id),
            None => self.pending_prompt.borrow().is_some(),
        };
        if matches_pending {
            self.complete_prompt(acp::StopReason::EndTurn);
        }
    }

    fn handle_turn_end(&self, frame: &Value) {
        match frame
            .get("message")
            .and_then(|message| message.get("stopReason"))
            .and_then(Value::as_str)
        {
            Some("toolUse") => {}
            Some("aborted") => self.complete_prompt(acp::StopReason::Cancelled),
            _ => self.complete_prompt(acp::StopReason::EndTurn),
        }
    }

    fn handle_ui_request(&self, frame: &Value, cx: &mut AsyncApp) {
        let Some(id) = frame.get("id").and_then(Value::as_str) else {
            return;
        };
        let method = frame.get("method").and_then(Value::as_str);
        let Some(kind) = method.and_then(ui_request_kind) else {
            // Unknown or explicit-cancel methods fail closed immediately so the
            // runtime is never left waiting on a request the panel can't show.
            self.send_frame(fail_closed_ui_response(id, None)).log_err();
            return;
        };

        let request = normalize_ui_request(id.to_owned(), kind, frame);

        let Some(thread) = self
            .thread
            .borrow()
            .as_ref()
            .and_then(|thread| thread.upgrade())
        else {
            // No panel is attached to surface the request: fail closed rather
            // than hang the runtime waiting for an answer that can't arrive.
            self.send_frame(fail_closed_ui_response(id, Some(kind)))
                .log_err();
            return;
        };

        self.pending_ui_requests.borrow_mut().insert(id.to_owned());
        thread.update(cx, |thread, cx| thread.push_ui_request(request, cx));
    }

    /// Upgrade the attached panel thread and run `f` against it, if a panel is
    /// still attached. Telemetry frames are best-effort: with no panel they are
    /// silently dropped (nothing to render into).
    fn update_thread(
        &self,
        cx: &mut AsyncApp,
        f: impl FnOnce(&mut AcpThread, &mut Context<AcpThread>),
    ) {
        let Some(thread) = self
            .thread
            .borrow()
            .as_ref()
            .and_then(|thread| thread.upgrade())
        else {
            return;
        };
        thread.update(cx, f);
    }

    /// Surface a `subagent_lifecycle`/`subagent_progress` frame as a telemetry
    /// node, merged into the tree by id so live progress never duplicates nodes.
    fn handle_subagent_telemetry(&self, frame: &Value, cx: &mut AsyncApp) {
        let Some(mut subagent) = frame.get("payload").and_then(parse_subagent) else {
            return;
        };
        // Resolve the raw parent tool-call id to the subagent that made it. An
        // id no subagent owns was a main-agent tool call, so this child is a
        // tree root (`parent_id` stays `None`).
        subagent.parent_id = subagent.parent_id.and_then(|tool_id| {
            self.subagent_tool_owners
                .borrow()
                .get(tool_id.as_ref())
                .cloned()
        });
        self.update_thread(cx, |thread, cx| thread.upsert_subagent(subagent, cx));
    }

    /// Append one streamed child event to the matching subagent's drill-in
    /// transcript, so an open inspector tails it incrementally. A child's tool
    /// executions are recorded as tool-call ownership so a grandchild's
    /// `parentToolCallId` resolves to this subagent, nesting the tree.
    fn handle_subagent_event(&self, frame: &Value, cx: &mut AsyncApp) {
        let Some(payload) = frame.get("payload") else {
            return;
        };
        let Some(id) = payload.get("id").and_then(Value::as_str) else {
            return;
        };
        let Some(event) = payload.get("event") else {
            return;
        };
        if event.get("type").and_then(Value::as_str) == Some("tool_execution_start")
            && let Some(tool_call_id) = subagent_event_tool_call_id(event)
        {
            self.subagent_tool_owners
                .borrow_mut()
                .insert(tool_call_id.to_owned(), id.to_owned().into());
        }
        let Some(line) = subagent_event_line(event) else {
            return;
        };
        let id: SharedString = id.to_owned().into();
        self.update_thread(cx, |thread, cx| {
            thread.append_subagent_event_line(id, line, cx)
        });
    }

    /// Routes the user's answer to a surfaced UI request back to OMP, keyed by
    /// request id. The pending-id check makes this answer-once: a second answer,
    /// or any answer after the session was cancelled/closed (which clears the
    /// pending set), is dropped without sending a frame.
    fn respond_ui_request(&self, request_id: &str, response: UiResponse) {
        if !self.pending_ui_requests.borrow_mut().remove(request_id) {
            return;
        }
        let payload = match response {
            UiResponse::Approve => json!({ "confirmed": true }),
            UiResponse::Deny => json!({ "confirmed": false }),
            UiResponse::Cancel => json!({ "cancelled": true }),
            UiResponse::Input(value) => json!({ "value": value }),
            UiResponse::Select(option_id) => json!({ "value": option_id.to_string() }),
        };
        self.send_frame(ui_response_frame(request_id, payload))
            .log_err();
    }

    /// Drops every outstanding UI request so a late answer can no longer reach
    /// OMP. Used on turn cancel, session close, and child exit.
    fn reject_pending_ui_requests(&self) {
        self.pending_ui_requests.borrow_mut().clear();
    }

    fn clear_thread_ui_requests(&self, cx: &mut App) {
        if let Some(thread) = self
            .thread
            .borrow()
            .as_ref()
            .and_then(|thread| thread.upgrade())
        {
            thread.update(cx, |thread, cx| thread.clear_ui_requests(cx));
        }
    }

    /// Rehydrate a single prior message replayed by a resumed OMP child. The
    /// frame routes to the current thread, so any other live session is
    /// untouched.
    fn handle_history_message(&self, frame: &Value, cx: &mut AsyncApp) {
        let Some(text) = frame
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        else {
            return;
        };
        let chunk = acp::ContentChunk::new(text.to_owned().into());
        let update = match frame.get("role").and_then(Value::as_str) {
            Some("user") => acp::SessionUpdate::UserMessageChunk(chunk),
            _ => acp::SessionUpdate::AgentMessageChunk(chunk),
        };
        self.send_session_update(update, cx);
    }

    fn apply_descriptor(&self, descriptor: &OmpDescriptor, cx: &mut AsyncApp) {
        let Some(title) = descriptor.title.clone() else {
            return;
        };
        if let Some(thread) = self
            .thread
            .borrow()
            .as_ref()
            .and_then(|thread| thread.upgrade())
        {
            let mut info = acp::SessionInfoUpdate::new().title(title);
            let meta = omp_session_meta(
                descriptor.session_id.as_deref(),
                descriptor.session_file.as_deref(),
            );
            if !meta.is_empty() {
                info = info.meta(meta);
            }
            let update = acp::SessionUpdate::SessionInfoUpdate(info);
            thread
                .update(cx, |thread, cx| thread.handle_session_update(update, cx))
                .log_err();
        }
    }

    fn cancel_active_turn(&self) {
        self.complete_prompt(acp::StopReason::Cancelled);
    }

    fn complete_prompt(&self, reason: acp::StopReason) {
        if let Some(prompt) = self.pending_prompt.borrow_mut().take() {
            prompt.tx.send(Ok(reason)).ok();
        }
    }

    fn mark_child_exited(&self, cx: &mut AsyncApp) {
        self.closed.set(true);
        self.outgoing.close();
        self.complete_prompt(acp::StopReason::Cancelled);
        self.reject_pending_requests("OMP child exited");
        self.reject_pending_ui_requests();
        if let Some(thread) = self
            .thread
            .borrow()
            .as_ref()
            .and_then(|thread| thread.upgrade())
        {
            thread.update(cx, |thread, cx| thread.clear_ui_requests(cx));
        }
        if let Some(mut child) = self.child.borrow_mut().take() {
            cx.background_spawn(async move {
                child.status().await.log_err();
                anyhow::Ok(())
            })
            .detach();
        }
    }

    fn reject_pending_requests(&self, message: &str) {
        for (_, tx) in self.pending_requests.borrow_mut().drain() {
            tx.send(Err(message.to_owned())).ok();
        }
    }

    fn kill(&self, cx: &mut App) {
        if let Some(mut child) = self.child.borrow_mut().take() {
            child.kill().log_err();
            cx.background_spawn(async move {
                child.status().await.log_err();
                anyhow::Ok(())
            })
            .detach();
        }
    }
}

impl Drop for OmpSessionState {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.get_mut().take() {
            child.kill().log_err();
        }
    }
}

fn prompt_text(prompt: &[acp::ContentBlock]) -> String {
    prompt
        .iter()
        .map(|block| match block {
            acp::ContentBlock::Text(text) => text.text.as_str(),
            _ => "",
        })
        .collect()
}

fn tool_title(name: &str, arguments: &Value, intent: Option<&str>) -> String {
    let intent = intent
        .filter(|intent| !intent.is_empty())
        .or_else(|| arguments.get("_i").and_then(Value::as_str))
        .filter(|intent| !intent.is_empty());
    match intent {
        Some(intent) => format!("{name}: {intent}"),
        None => name.to_owned(),
    }
}

fn tool_kind(name: &str) -> acp::ToolKind {
    match name {
        "read" => acp::ToolKind::Read,
        "edit" | "write" | "ast_edit" => acp::ToolKind::Edit,
        "find" | "search" | "ast_grep" | "web_search" => acp::ToolKind::Search,
        "bash" | "debug" | "eval" | "job" | "task" => acp::ToolKind::Execute,
        _ => acp::ToolKind::Other,
    }
}

fn tool_result_content(frame: &Value) -> Vec<acp::ToolCallContent> {
    let Some(blocks) = frame
        .get("content")
        .or_else(|| frame.get("result").and_then(|result| result.get("content")))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut text = String::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("text")
            && let Some(block_text) = block.get("text").and_then(Value::as_str)
            && !block_text.is_empty()
        {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(block_text);
        }
    }
    if text.is_empty() {
        Vec::new()
    } else {
        vec![acp::ToolCallContent::Content(acp::Content::new(text))]
    }
}

fn omp_session_meta(session_id: Option<&str>, session_file: Option<&str>) -> acp::Meta {
    let mut meta = acp::Meta::new();
    if let Some(session_id) = session_id {
        meta.insert("omp_session_id".to_owned(), session_id.into());
    }
    if let Some(session_file) = session_file {
        meta.insert("omp_session_file".to_owned(), session_file.into());
    }
    meta
}

fn parse_descriptor(state: &Value) -> OmpDescriptor {
    let session_name = state
        .get("sessionName")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty());
    let session_file = state
        .get("sessionFile")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty());
    let session_id = state
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let title = session_file
        .map(|file| match session_name {
            Some(name) => format!("{name} · {file}"),
            None => format!("OMP · {file}"),
        })
        .or_else(|| session_name.map(ToOwned::to_owned));
    OmpDescriptor {
        session_id: session_id.map(ToOwned::to_owned),
        session_file: session_file.map(ToOwned::to_owned),
        title,
    }
}

fn response_error(frame: &Value) -> String {
    frame
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("OMP request failed")
        .to_owned()
}

/// Maps an OMP `extension_ui_request` method name to its normalized kind.
/// Unknown methods (and the runtime's own `cancel`) return `None` so the bridge
/// fails them closed rather than surfacing a widget it can't answer.
fn ui_request_kind(method: &str) -> Option<UiRequestKind> {
    match method {
        "confirm" => Some(UiRequestKind::Approval),
        "input" => Some(UiRequestKind::Input),
        "select" => Some(UiRequestKind::Select),
        "editor" => Some(UiRequestKind::Editor),
        "open_url" | "openUrl" | "open-url" => Some(UiRequestKind::OpenUrl),
        _ => None,
    }
}

/// Normalizes an OMP `extension_ui_request` frame into the agent-neutral
/// [`UiRequest`] the panel renders. Field names are probed defensively so the
/// bridge tolerates protocol naming variants.
fn normalize_ui_request(id: String, kind: UiRequestKind, frame: &Value) -> UiRequest {
    let message = first_str(frame, &["message", "prompt", "title", "question"])
        .unwrap_or_default()
        .to_owned()
        .into();
    let scope = UiRequestScope {
        tool: first_shared(frame, &["tool", "toolName"]),
        action: first_shared(frame, &["action"]),
        path: first_shared(frame, &["path", "filePath", "file"]),
        workspace: first_shared(frame, &["workspace", "cwd", "workspaceRoot"]),
        session: first_shared(frame, &["sessionId", "session"]),
    };
    UiRequest {
        id: id.into(),
        kind,
        message,
        scope,
        options: parse_ui_options(frame),
        default_value: first_shared(frame, &["defaultValue", "value", "content", "default"]),
        url: first_shared(frame, &["url", "href", "link"]),
    }
}

fn parse_ui_options(frame: &Value) -> Vec<UiRequestOption> {
    let Some(options) = frame.get("options").and_then(Value::as_array) else {
        return Vec::new();
    };
    options
        .iter()
        .filter_map(|option| match option {
            Value::String(value) if !value.is_empty() => Some(UiRequestOption {
                id: value.clone().into(),
                label: value.clone().into(),
            }),
            Value::Object(_) => {
                let id = first_str(option, &["value", "id", "key"])?;
                let label = first_str(option, &["label", "name", "title"]).unwrap_or(id);
                Some(UiRequestOption {
                    id: id.to_owned().into(),
                    label: label.to_owned().into(),
                })
            }
            _ => None,
        })
        .collect()
}

fn first_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .filter(|found| !found.is_empty())
}

fn first_shared(value: &Value, keys: &[&str]) -> Option<SharedString> {
    first_str(value, keys).map(|found| found.to_owned().into())
}

/// Builds the deny/cancel response sent when a request can't be surfaced (no
/// panel) or isn't answerable (unknown method): approvals/open-url deny,
/// everything else cancels.
fn fail_closed_ui_response(id: &str, kind: Option<UiRequestKind>) -> Value {
    let payload = match kind {
        Some(UiRequestKind::Approval | UiRequestKind::OpenUrl) => json!({ "confirmed": false }),
        _ => json!({ "cancelled": true }),
    };
    ui_response_frame(id, payload)
}

/// Wraps a response payload in the `extension_ui_response` envelope keyed by id.
fn ui_response_frame(id: &str, payload: Value) -> Value {
    let mut frame = json!({
        "type": "extension_ui_response",
        "id": id,
    });
    if let Value::Object(fields) = payload {
        for (key, value) in fields {
            frame[key] = value;
        }
    }
    frame
}
/// Parse a `subagent_lifecycle`/`subagent_progress` payload into a normalized
/// [`Subagent`]. Lifecycle frames carry id/agent/status at the payload top
/// level; progress frames nest the live counters (and the id/status) under
/// `payload.progress`, so each field is read from whichever place it appears.
/// Fields a given frame omits are left `None`/empty so the merge in
/// [`AcpThread::upsert_subagent`] never clobbers an earlier value.
fn parse_subagent(payload: &Value) -> Option<Subagent> {
    let progress = payload.get("progress");
    let from_either = |key: &str| -> Option<&str> {
        payload
            .get(key)
            .and_then(Value::as_str)
            .or_else(|| progress.and_then(|p| p.get(key)).and_then(Value::as_str))
    };
    let id = from_either("id")?;
    let one_line =
        |text: &str| -> SharedString { text.lines().next().unwrap_or(text).to_owned().into() };
    Some(Subagent {
        id: id.to_owned().into(),
        agent: from_either("agent").unwrap_or_default().to_owned().into(),
        parent_id: payload
            .get("parentToolCallId")
            .and_then(Value::as_str)
            .map(|parent| parent.to_owned().into()),
        index: payload
            .get("index")
            .and_then(Value::as_u64)
            .or_else(|| {
                progress
                    .and_then(|p| p.get("index"))
                    .and_then(Value::as_u64)
            })
            .unwrap_or(0) as u32,
        status: from_either("status").unwrap_or_default().to_owned().into(),
        task: from_either("task")
            .or_else(|| from_either("assignment"))
            .map(one_line),
        tool_count: progress
            .and_then(|p| p.get("toolCount"))
            .and_then(Value::as_u64)
            .map(|count| count as u32),
        tokens: progress
            .and_then(|p| p.get("tokens"))
            .and_then(Value::as_u64),
        cost: progress.and_then(|p| p.get("cost")).and_then(Value::as_f64),
        model: progress
            .and_then(|p| p.get("resolvedModel"))
            .and_then(Value::as_str)
            .map(|model| model.to_owned().into()),
        recent_tools: progress
            .and_then(|p| p.get("recentTools"))
            .and_then(Value::as_array)
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|tool| tool.to_owned().into())
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// Render one streamed child `AgentSessionEvent` (the `payload.event` of a
/// `subagent_event` frame) as a single drill-in transcript line, or `None` to
/// skip noisy partial-delta events. Only tool starts, finished assistant
/// messages, and the terminal `agent_end` produce a line.
fn subagent_event_line(event: &Value) -> Option<SharedString> {
    match event.get("type").and_then(Value::as_str)? {
        "tool_execution_start" => {
            let name = event
                .get("name")
                .or_else(|| event.get("toolName"))
                .and_then(Value::as_str)?;
            Some(format!("→ {name}").into())
        }
        "message_end" => {
            let message = event.get("message")?;
            if message.get("role").and_then(Value::as_str) != Some("assistant") {
                return None;
            }
            let text = assistant_message_text(message)?;
            (!text.trim().is_empty()).then(|| text.into())
        }
        "agent_end" => Some("✓ finished".into()),
        _ => None,
    }
}

/// Concatenate the `text` blocks of a message's content array; `None` when the
/// message has no textual content.
fn assistant_message_text(message: &Value) -> Option<String> {
    let blocks = message.get("content")?.as_array()?;
    let text: String = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    (!text.is_empty()).then_some(text)
}

/// The tool-call id a child event refers to (`id` or `toolCallId`), used to
/// record which subagent owns a spawning tool call.
fn subagent_event_tool_call_id(event: &Value) -> Option<&str> {
    event
        .get("id")
        .or_else(|| event.get("toolCallId"))
        .and_then(Value::as_str)
}

/// Whether a prompt response signals a local-only completion (`agentInvoked:
/// false`) — no model turn ran, so no `agent_end`/`turn_end` will follow.
fn prompt_completed_locally(frame: &Value) -> bool {
    frame
        .get("data")
        .and_then(|data| data.get("agentInvoked"))
        .and_then(Value::as_bool)
        == Some(false)
}

/// Parse an OMP `available_commands_update` frame into the agent-neutral
/// command list the composer renders. Only top-level commands are surfaced;
/// subcommands and aliases are intentionally skipped for a minimal picker.
fn parse_available_commands(frame: &Value) -> Vec<acp::AvailableCommand> {
    let Some(commands) = frame.get("commands").and_then(Value::as_array) else {
        return Vec::new();
    };
    commands
        .iter()
        .filter_map(parse_available_command)
        .collect()
}

fn parse_available_command(command: &Value) -> Option<acp::AvailableCommand> {
    let name = command.get("name").and_then(Value::as_str)?;
    let description = command
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // OMP parses its own slash commands, so the full `/command args` line must
    // reach it verbatim as one prompt. Leaving the command uncategorized (no
    // Native meta) stops Zed from stripping the name and queueing the argument
    // as a separate prompt — `leading_native_command` only matches Native.
    let mut mapped = acp::AvailableCommand::new(name.to_owned(), description.to_owned());
    if let Some(hint) = command
        .get("input")
        .and_then(|input| input.get("hint"))
        .and_then(Value::as_str)
        .filter(|hint| !hint.is_empty())
    {
        mapped = mapped.input(acp::AvailableCommandInput::Unstructured(
            acp::UnstructuredCommandInput::new(hint.to_owned()),
        ));
    }
    Some(mapped)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use acp_thread::{AgentThreadEntry, ThreadStatus, ToolCallStatus};
    use gpui::TestAppContext;
    use indoc::formatdoc;
    use project::Project;
    use std::{fs, os::unix::fs::PermissionsExt, time::Duration};

    /// Deterministic OMP agent-panel smoke harness.
    ///
    /// Against a throwaway workspace it proves the OMP agent can: start a
    /// child, return to `Idle` after a harmless no-tool prompt, keep a
    /// *denied* tool call/result visible (it must not disappear), and surface
    /// the session (trace) file path OMP reports via `get_state`.
    ///
    /// Run on demand (deterministic; no network, no real/paid omp):
    /// `cargo test -p agent_servers omp_smoke -- --nocapture`.
    ///
    /// Opt in to a real omp (HITL only — runs a *paid* turn, never in CI):
    /// `OMP_BINARY=$(command -v omp) OMP_SMOKE_LIVE=1 cargo test -p agent_servers omp_smoke -- --nocapture`.
    #[gpui::test]
    async fn omp_smoke_agent_panel_harness(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;

        // Stable agent identity — never couple this smoke to display copy
        // (the user-facing label is owned by a separate issue).
        assert_eq!(crate::omp::OMP_AGENT_ID, "omp");
        assert_eq!(OmpAgentServer.agent_id(), AgentId::new("omp"));

        // Default: deterministic FakeOmp. A real/paid omp is used only when the
        // operator explicitly opts in with OMP_BINARY + OMP_SMOKE_LIVE=1.
        let live = std::env::var_os("OMP_BINARY").is_some()
            && std::env::var("OMP_SMOKE_LIVE").is_ok_and(|value| value == "1");
        let fixture = (!live).then(|| FakeOmp::new("smoke"));
        let command = match &fixture {
            Some(fixture) => fixture.command(),
            None => OmpCommand::default(),
        };

        let workspace = tempfile::tempdir().unwrap();
        let connection = Rc::new(OmpAgentConnection::new(command));
        let project = Project::example([workspace.path()], &mut cx.to_async()).await;
        let thread = cx
            .update(|cx| {
                connection
                    .clone()
                    .new_session(project, PathList::new(&[workspace.path()]), cx)
            })
            .await
            .unwrap();

        // Phase 0 — child started; record the session (trace) file path. OMP
        // reports `sessionFile` via get_state; `new_session` adopts the
        // reported session id and renders the title (`{sessionName} ·
        // {sessionFile}`) plus the `omp_session_file` session meta.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let title = loop {
            if let Some(title) = thread.read_with(cx, |thread, _| {
                thread.title().map(|title| title.to_string())
            }) {
                break title;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "OMP should report session state (with a session file) via get_state"
            );
            cx.background_executor
                .timer(Duration::from_millis(20))
                .await;
            cx.run_until_parked();
        };
        let session_file = title.rsplit(" · ").next().unwrap_or_default();
        assert!(
            !session_file.is_empty(),
            "OMP smoke must record a non-empty session (trace) file path"
        );
        eprintln!("omp_smoke session file: {session_file}");
        if fixture.is_some() {
            assert!(session_file.ends_with(".jsonl"));
        }

        // Phase 1 — harmless no-tool prompt streams text and returns to idle.
        thread
            .update(cx, |thread, cx| thread.send_raw("hello", cx))
            .await
            .unwrap();
        cx.run_until_parked();
        thread.read_with(cx, |thread, cx| {
            assert_eq!(thread.status(), ThreadStatus::Idle);
            assert!(
                thread
                    .entries()
                    .iter()
                    .any(|entry| matches!(entry, AgentThreadEntry::AssistantMessage(_))),
                "no-tool prompt should produce assistant text"
            );
            if fixture.is_some() {
                assert!(thread.to_markdown(cx).contains("Hello from fake OMP smoke"));
            }
        });

        // Phase 2 — a denied tool call must stay visible (the tool call and its
        // failed result do not disappear) and the turn still returns to idle.
        thread
            .update(cx, |thread, cx| thread.send_raw("fan out", cx))
            .await
            .unwrap();
        cx.run_until_parked();
        thread.read_with(cx, |thread, cx| {
            assert_eq!(thread.status(), ThreadStatus::Idle);
            let markdown = thread.to_markdown(cx);
            if fixture.is_some() {
                assert!(
                    markdown.contains("Tool call denied by user: task"),
                    "denied tool result text must remain visible"
                );
                let tool_call = thread
                    .entries()
                    .iter()
                    .find_map(|entry| match entry {
                        AgentThreadEntry::ToolCall(call) => Some(call),
                        _ => None,
                    })
                    .expect("denied OMP tool result should create a tool call entry");
                assert!(matches!(tool_call.status, ToolCallStatus::Failed));
            }
        });
    }

    #[gpui::test]
    async fn omp_no_tool_prompt_streams_text_and_ends_idle(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeOmp::new("normal");
        let connection = Rc::new(OmpAgentConnection::new(fixture.command()));
        let project = Project::example([fixture.dir.path()], &mut cx.to_async()).await;
        let thread = cx
            .update(|cx| {
                connection
                    .clone()
                    .new_session(project, PathList::new(&[fixture.dir.path()]), cx)
            })
            .await
            .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let title = loop {
            if let Some(title) = thread.read_with(cx, |thread, _| {
                thread.title().map(|title| title.to_string())
            }) {
                break title;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "OMP title should be set from get_state"
            );
            cx.background_executor
                .timer(Duration::from_millis(20))
                .await;
            cx.run_until_parked();
        };
        assert!(title.contains("Fake OMP"));
        assert!(title.contains(".jsonl"));

        thread
            .update(cx, |thread, cx| thread.send_raw("hello", cx))
            .await
            .unwrap();
        cx.run_until_parked();

        thread.read_with(cx, |thread, cx| {
            assert_eq!(thread.status(), ThreadStatus::Idle);
            assert!(thread.to_markdown(cx).contains("Hello from fake OMP"));
            assert!(matches!(
                thread.entries()[1],
                AgentThreadEntry::AssistantMessage(_)
            ));
        });
    }

    #[gpui::test]
    async fn omp_response_required_ui_request_fails_closed_without_hanging(
        cx: &mut TestAppContext,
    ) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeOmp::new("ui");
        let connection = Rc::new(OmpAgentConnection::new(fixture.command()));
        let project = Project::example([fixture.dir.path()], &mut cx.to_async()).await;
        let thread = cx
            .update(|cx| {
                connection
                    .clone()
                    .new_session(project, PathList::new(&[fixture.dir.path()]), cx)
            })
            .await
            .unwrap();

        // A response-required UI request now surfaces to the panel and waits for
        // the user instead of auto-denying. Acting as the panel, deny it: the
        // turn must fail closed (confirmed:false) and end Idle without hanging.
        let send = thread.update(cx, |thread, cx| thread.send_raw("needs approval", cx));
        let id = wait_for_ui_requests(&thread, 1, cx).await.remove(0);
        thread.update(cx, |thread, cx| {
            thread.respond_to_ui_request(id, UiResponse::Deny, cx)
        });
        send.await.unwrap();
        cx.run_until_parked();

        let seen_response = fs::read_to_string(&fixture.ui_response_path)
            .expect("fake OMP should record the deny response");
        assert!(seen_response.contains("\"type\":\"extension_ui_response\""));
        assert!(seen_response.contains("\"confirmed\":false"));
        thread.read_with(cx, |thread, _| {
            assert_eq!(thread.status(), ThreadStatus::Idle)
        });
    }

    #[gpui::test]
    async fn omp_tool_denial_waits_for_final_turn_and_surfaces_tool_result(
        cx: &mut TestAppContext,
    ) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeOmp::new("tool");
        let connection = Rc::new(OmpAgentConnection::new(fixture.command()));
        let project = Project::example([fixture.dir.path()], &mut cx.to_async()).await;
        let thread = cx
            .update(|cx| {
                connection
                    .clone()
                    .new_session(project, PathList::new(&[fixture.dir.path()]), cx)
            })
            .await
            .unwrap();

        thread
            .update(cx, |thread, cx| thread.send_raw("fan out", cx))
            .await
            .unwrap();

        thread.read_with(cx, |thread, cx| {
            assert_eq!(thread.status(), ThreadStatus::Idle);
            let markdown = thread.to_markdown(cx);
            assert!(markdown.contains("Denied again"));
            assert!(markdown.contains("Tool call denied by user: task"));
            let tool_call = thread
                .entries()
                .iter()
                .find_map(|entry| match entry {
                    AgentThreadEntry::ToolCall(call) => Some(call),
                    _ => None,
                })
                .expect("denied OMP tool result should create a tool call entry");
            assert!(matches!(tool_call.status, ToolCallStatus::Failed));
        });
    }

    #[gpui::test]
    async fn omp_failed_prompt_response_does_not_hang(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeOmp::new("reject");
        let connection = Rc::new(OmpAgentConnection::new(fixture.command()));
        let project = Project::example([fixture.dir.path()], &mut cx.to_async()).await;
        let thread = cx
            .update(|cx| {
                connection
                    .clone()
                    .new_session(project, PathList::new(&[fixture.dir.path()]), cx)
            })
            .await
            .unwrap();

        let err = thread
            .update(cx, |thread, cx| thread.send_raw("reject", cx))
            .await
            .expect_err("failed OMP prompt response should reject the turn");
        assert!(err.to_string().contains("prompt rejected"));
    }

    #[gpui::test]
    async fn omp_prompt_after_child_exit_errors_without_hanging(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeOmp::new("exit");
        let connection = Rc::new(OmpAgentConnection::new(fixture.command()));
        let project = Project::example([fixture.dir.path()], &mut cx.to_async()).await;
        let thread = cx
            .update(|cx| {
                connection
                    .clone()
                    .new_session(project, PathList::new(&[fixture.dir.path()]), cx)
            })
            .await
            .unwrap();

        let pid = fixture.pid();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while process_is_alive(pid) && std::time::Instant::now() < deadline {
            cx.background_executor
                .timer(Duration::from_millis(20))
                .await;
            cx.run_until_parked();
        }
        assert!(!process_is_alive(pid), "fake OMP child should have exited");
        cx.run_until_parked();

        let err = thread
            .update(cx, |thread, cx| thread.send_raw("after exit", cx))
            .await
            .expect_err("prompt after OMP child exit should fail immediately");
        assert!(err.to_string().contains("OMP child exited"));
    }

    #[gpui::test]
    async fn omp_cancel_and_close_do_not_leave_fake_child_alive(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeOmp::new("wait");
        let connection = Rc::new(OmpAgentConnection::new(fixture.command()));
        let project = Project::example([fixture.dir.path()], &mut cx.to_async()).await;
        let thread = cx
            .update(|cx| {
                connection
                    .clone()
                    .new_session(project, PathList::new(&[fixture.dir.path()]), cx)
            })
            .await
            .unwrap();
        let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());
        let pid = fixture.pid();
        let send = thread.update(cx, |thread, cx| thread.send_raw("wait", cx));
        cx.run_until_parked();

        cx.update(|cx| connection.cancel(&session_id, cx));
        send.await.unwrap();
        cx.update(|cx| connection.clone().close_session(&session_id, cx))
            .await
            .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while process_is_alive(pid) && std::time::Instant::now() < deadline {
            cx.background_executor
                .timer(Duration::from_millis(20))
                .await;
        }
        assert!(
            !process_is_alive(pid),
            "fake OMP child should exit after close"
        );
    }

    #[gpui::test]
    async fn omp_resume_rehydrates_saved_transcript_without_child_leak(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeResumableOmp::new();
        let connection = Rc::new(OmpAgentConnection::new(fixture.command()));
        let project = Project::example([fixture.dir.path()], &mut cx.to_async()).await;

        // Open a saved session the way the agent panel does after a restart:
        // it hands back the persisted session id + title from the history
        // store. OMP is spawned with `--resume <id>` and replays its trace.
        let session_id = acp::SessionId::new("saved-1");
        let thread = cx
            .update(|cx| {
                connection.clone().load_session(
                    session_id.clone(),
                    project,
                    PathList::new(&[fixture.dir.path()]),
                    Some("Saved chat".into()),
                    cx,
                )
            })
            .await
            .unwrap();
        cx.run_until_parked();

        thread.read_with(cx, |thread, cx| {
            // The resumed thread keeps its own saved id — never another's.
            assert_eq!(thread.session_id(), &session_id);
            let markdown = thread.to_markdown(cx);
            assert!(
                markdown.contains("earlier question for saved-1"),
                "resumed thread should rehydrate the prior user message"
            );
            assert!(
                markdown.contains("earlier answer for saved-1"),
                "resumed thread should rehydrate the prior assistant message"
            );
            // The title carries OMP's trace-file provenance.
            let title = thread
                .title()
                .map(|title| title.to_string())
                .unwrap_or_default();
            assert!(
                title.contains(".jsonl"),
                "resumed thread should surface the OMP trace-file provenance"
            );
        });

        // Closing the resumed session must not leak its child.
        let pid = fixture.pid_for("saved-1");
        assert!(process_is_alive(pid));
        cx.update(|cx| connection.clone().close_session(&session_id, cx))
            .await
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while process_is_alive(pid) && std::time::Instant::now() < deadline {
            cx.background_executor
                .timer(Duration::from_millis(20))
                .await;
        }
        assert!(
            !process_is_alive(pid),
            "resumed OMP child must not leak after close"
        );
    }

    #[gpui::test]
    async fn omp_new_session_passes_selected_workspace_cwd_to_omp(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeOmp::new("normal");
        // A throwaway "selected workspace" directory, distinct from the fixture
        // directory that holds the fake omp script.
        let workspace = tempfile::tempdir().unwrap();
        let connection = Rc::new(OmpAgentConnection::new(fixture.command()));
        let project = Project::example([workspace.path()], &mut cx.to_async()).await;
        let _thread = cx
            .update(|cx| {
                connection
                    .clone()
                    .new_session(project, PathList::new(&[workspace.path()]), cx)
            })
            .await
            .unwrap();

        // The launched child must be told the selected workspace via `--cwd`
        // and must actually run in that directory.
        let args = fixture.args();
        assert!(
            args.contains("--cwd"),
            "fake omp argv should carry a --cwd flag, got: {args}"
        );
        assert_eq!(
            fs::canonicalize(fixture.cwd()).unwrap(),
            fs::canonicalize(workspace.path()).unwrap(),
            "fake omp should be launched in the selected workspace cwd"
        );
    }

    #[gpui::test]
    async fn omp_two_saved_sessions_switch_and_close_independently(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeResumableOmp::new();
        let connection = Rc::new(OmpAgentConnection::new(fixture.command()));
        let project = Project::example([fixture.dir.path()], &mut cx.to_async()).await;

        let alpha = acp::SessionId::new("alpha");
        let beta = acp::SessionId::new("beta");
        let thread_alpha = cx
            .update(|cx| {
                connection.clone().load_session(
                    alpha.clone(),
                    project.clone(),
                    PathList::new(&[fixture.dir.path()]),
                    None,
                    cx,
                )
            })
            .await
            .unwrap();
        let thread_beta = cx
            .update(|cx| {
                connection.clone().load_session(
                    beta.clone(),
                    project.clone(),
                    PathList::new(&[fixture.dir.path()]),
                    None,
                    cx,
                )
            })
            .await
            .unwrap();
        cx.run_until_parked();

        // Each thread rehydrates only its own transcript — no cross-talk.
        thread_alpha.read_with(cx, |thread, cx| {
            let markdown = thread.to_markdown(cx);
            assert!(markdown.contains("earlier answer for alpha"));
            assert!(!markdown.contains("beta"));
        });
        thread_beta.read_with(cx, |thread, cx| {
            let markdown = thread.to_markdown(cx);
            assert!(markdown.contains("earlier answer for beta"));
            assert!(!markdown.contains("alpha"));
        });

        let pid_alpha = fixture.pid_for("alpha");
        let pid_beta = fixture.pid_for("beta");
        assert!(process_is_alive(pid_alpha) && process_is_alive(pid_beta));

        // Closing one session leaves the other's child untouched.
        cx.update(|cx| connection.clone().close_session(&alpha, cx))
            .await
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while process_is_alive(pid_alpha) && std::time::Instant::now() < deadline {
            cx.background_executor
                .timer(Duration::from_millis(20))
                .await;
        }
        assert!(
            !process_is_alive(pid_alpha),
            "the closed session's child should exit"
        );
        assert!(
            process_is_alive(pid_beta),
            "closing one OMP session must not kill another active session"
        );

        // Shutdown disposes the survivor too.
        cx.update(|cx| connection.dispose_all(cx));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while process_is_alive(pid_beta) && std::time::Instant::now() < deadline {
            cx.background_executor
                .timer(Duration::from_millis(20))
                .await;
        }
        assert!(!process_is_alive(pid_beta));
    }

    #[gpui::test]
    async fn omp_shutdown_disposes_all_children(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeResumableOmp::new();
        let connection = Rc::new(OmpAgentConnection::new(fixture.command()));
        let project = Project::example([fixture.dir.path()], &mut cx.to_async()).await;

        let mut pids = Vec::new();
        for id in ["s-1", "s-2", "s-3"] {
            cx.update(|cx| {
                connection.clone().load_session(
                    acp::SessionId::new(id),
                    project.clone(),
                    PathList::new(&[fixture.dir.path()]),
                    None,
                    cx,
                )
            })
            .await
            .unwrap();
            pids.push(fixture.pid_for(id));
        }
        cx.run_until_parked();
        assert!(pids.iter().all(|pid| process_is_alive(*pid)));

        // The app-quit hook runs dispose_all; no OMP child may outlive the app.
        cx.update(|cx| connection.dispose_all(cx));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while pids.iter().any(|pid| process_is_alive(*pid)) && std::time::Instant::now() < deadline
        {
            cx.background_executor
                .timer(Duration::from_millis(20))
                .await;
        }
        assert!(
            pids.iter().all(|pid| !process_is_alive(*pid)),
            "no OMP child may survive app shutdown"
        );
    }

    #[gpui::test]
    async fn omp_new_sessions_bind_each_selected_workspace_cwd(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeOmp::new("normal");
        let connection = Rc::new(OmpAgentConnection::new(fixture.command()));

        // Two distinct selected workspaces. Each new session is launched in its
        // own cwd; a live child's cwd is fixed at spawn, so opening a session in
        // a different workspace never moves an earlier one.
        let workspace_a = tempfile::tempdir().unwrap();
        let workspace_b = tempfile::tempdir().unwrap();

        let project_a = Project::example([workspace_a.path()], &mut cx.to_async()).await;
        let _thread_a = cx
            .update(|cx| {
                connection
                    .clone()
                    .new_session(project_a, PathList::new(&[workspace_a.path()]), cx)
            })
            .await
            .unwrap();

        let project_b = Project::example([workspace_b.path()], &mut cx.to_async()).await;
        let _thread_b = cx
            .update(|cx| {
                connection
                    .clone()
                    .new_session(project_b, PathList::new(&[workspace_b.path()]), cx)
            })
            .await
            .unwrap();

        // Each child recorded a launch marker inside the workspace it ran in.
        let marker_a = read_launch_marker(workspace_a.path());
        let marker_b = read_launch_marker(workspace_b.path());
        assert_eq!(
            fs::canonicalize(&marker_a).unwrap(),
            fs::canonicalize(workspace_a.path()).unwrap()
        );
        assert_eq!(
            fs::canonicalize(&marker_b).unwrap(),
            fs::canonicalize(workspace_b.path()).unwrap()
        );
        // The second session in workspace B did not disturb workspace A's cwd.
        assert_ne!(marker_a, marker_b);
    }

    #[gpui::test]
    async fn omp_missing_binary_yields_visible_launch_error(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let workspace = tempfile::tempdir().unwrap();
        let missing = workspace.path().join("definitely-not-omp");
        let command = OmpCommand {
            program: missing,
            prefix_args: Vec::new(),
        };
        let connection = Rc::new(OmpAgentConnection::new(command));
        let project = Project::example([workspace.path()], &mut cx.to_async()).await;

        // A missing binary must fail (not panic). The error carries a clear,
        // user-facing OMP status that the agent panel renders via its
        // "Failed to Launch" surface.
        let err = cx
            .update(|cx| {
                connection
                    .clone()
                    .new_session(project, PathList::new(&[workspace.path()]), cx)
            })
            .await
            .expect_err("a missing OMP binary must degrade with an error, not a panic");
        let full = format!("{err:#}");
        assert!(
            full.contains("omp.binary_path"),
            "missing-binary launch error must name the fix, got: {full}"
        );
    }

    #[test]
    fn omp_discovery_status_is_visible_when_binary_missing() {
        // Construct the missing-binary state directly so this does not depend on
        // whether a real omp happens to be installed on the host.
        let discovery = OmpDiscovery::default();
        assert!(!discovery.binary_available());
        assert!(!discovery.config_available());
        let label = discovery.status_label();
        assert!(!label.is_empty(), "status must be a visible message");
        assert!(
            label.contains("not found"),
            "a missing binary must surface a visible status, got: {label}"
        );
    }

    #[test]
    fn omp_discovery_probes_empty_workspace_without_crashing() {
        // Probing a workspace with no `.omp` config must degrade gracefully: no
        // panic, no config, zero commands, and a non-empty status. The binary
        // may or may not be present on the host, so it is not asserted here.
        let workspace = tempfile::tempdir().unwrap();
        let missing_binary = workspace.path().join("nonexistent-omp");
        let discovery = discover_omp(Some(&missing_binary), None, workspace.path());
        assert!(!discovery.config_available());
        assert_eq!(discovery.command_count, 0);
        assert!(!discovery.status_label().is_empty());
    }

    #[test]
    fn omp_discovery_finds_binary_and_workspace_config() {
        let workspace = tempfile::tempdir().unwrap();
        // A real (existing) fake binary file.
        let binary = workspace.path().join("omp");
        fs::write(&binary, "#!/bin/sh\n").unwrap();
        // Workspace config with two command files.
        let commands = workspace.path().join(".omp").join("commands");
        fs::create_dir_all(&commands).unwrap();
        fs::write(commands.join("build.md"), "build").unwrap();
        fs::write(commands.join("test.md"), "test").unwrap();

        let discovery = discover_omp(Some(&binary), None, workspace.path());
        assert!(discovery.binary_available());
        assert!(discovery.config_available());
        assert_eq!(discovery.command_count, 2);
        let label = discovery.status_label();
        assert!(label.contains("ready"), "got: {label}");
        assert!(label.contains("2 workspace commands"), "got: {label}");
    }

    #[test]
    fn omp_discovery_honors_relative_config_override() {
        let workspace = tempfile::tempdir().unwrap();
        let binary = workspace.path().join("omp");
        fs::write(&binary, "#!/bin/sh\n").unwrap();
        let custom = workspace.path().join("tools").join("omp-config");
        fs::create_dir_all(custom.join("commands")).unwrap();
        fs::write(custom.join("commands").join("ship.md"), "ship").unwrap();

        let discovery = discover_omp(
            Some(&binary),
            Some(Path::new("tools/omp-config")),
            workspace.path(),
        );
        assert!(discovery.config_available());
        assert_eq!(discovery.command_count, 1);
    }

    #[gpui::test]
    fn omp_settings_reload_preserves_workspace_defaults(cx: &mut gpui::App) {
        use gpui::UpdateGlobal;
        use settings::{Settings, SettingsStore};

        let store = SettingsStore::test(cx);
        cx.set_global(store);
        OmpSettings::register(cx);

        let omp_json =
            r#"{ "omp": { "binary_path": "/opt/omp/bin/omp", "config_dir": "workspace-omp" } }"#;
        SettingsStore::update_global(cx, |store, cx| {
            store.set_user_settings(omp_json, cx).unwrap();
        });

        // Workspace OMP defaults resolve both globally and through a worktree
        // location (the same lookup the agent panel uses for the selected
        // workspace).
        let location = settings::SettingsLocation {
            worktree_id: settings::WorktreeId::from_usize(1),
            path: util::rel_path::RelPath::empty(),
        };
        let before = OmpSettings::get_global(cx).clone();
        assert_eq!(before.binary_path, Some(PathBuf::from("/opt/omp/bin/omp")));
        assert_eq!(before.config_dir, Some(PathBuf::from("workspace-omp")));
        assert_eq!(
            OmpSettings::get(Some(location), cx).binary_path,
            before.binary_path,
            "workspace-scoped lookup must resolve the OMP defaults"
        );

        // A reload re-applies the settings content (as when the settings file is
        // re-read). The OMP defaults must survive intact.
        SettingsStore::update_global(cx, |store, cx| {
            store.set_user_settings(omp_json, cx).unwrap();
        });
        let after_reload = OmpSettings::get_global(cx).clone();
        assert_eq!(after_reload.binary_path, before.binary_path);
        assert_eq!(after_reload.config_dir, before.config_dir);

        // A reload that changes an UNRELATED setting must not reset the OMP
        // defaults that remain present.
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(
                    r#"{ "omp": { "binary_path": "/opt/omp/bin/omp", "config_dir": "workspace-omp" }, "auto_update": false }"#,
                    cx,
                )
                .unwrap();
        });
        let after_unrelated = OmpSettings::get(Some(location), cx);
        assert_eq!(
            after_unrelated.binary_path, before.binary_path,
            "workspace OMP binary default must be preserved across a settings reload"
        );
        assert_eq!(
            after_unrelated.config_dir, before.config_dir,
            "workspace OMP config default must be preserved across a settings reload"
        );
    }

    fn read_launch_marker(workspace: &Path) -> PathBuf {
        let marker = workspace.join(".omp-cwd");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(value) = fs::read_to_string(&marker)
                && !value.is_empty()
            {
                return PathBuf::from(value);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fake OMP did not write a launch marker in {}",
                workspace.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    struct FakeOmp {
        dir: tempfile::TempDir,
        script_path: PathBuf,
        pid_path: PathBuf,
        ui_response_path: PathBuf,
        cwd_path: PathBuf,
        args_path: PathBuf,
    }

    impl FakeOmp {
        fn new(mode: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script_path = dir.path().join("fake-omp.sh");
            let pid_path = dir.path().join("pid");
            let ui_response_path = dir.path().join("ui-response");
            let cwd_path = dir.path().join("cwd");
            let args_path = dir.path().join("args");
            fs::write(
                &script_path,
                formatdoc! {r#"
                    #!/bin/sh
                    printf '%s' "$$" > "{pid_path}"
                    cwd=$(pwd -P)
                    printf '%s' "$cwd" > "{cwd_path}"
                    printf '%s' "$*" > "{args_path}"
                    printf '%s' "$cwd" > "$cwd/.omp-cwd"
                    while IFS= read -r line; do
                      case "$line" in
                        *'"type":"get_state"'*)
                          id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
                          printf '{{"type":"response","id":"%s","success":true,"data":{{"sessionId":"fake-session","sessionName":"Fake OMP","sessionFile":"{pid_path}.jsonl"}}}}\n' "$id"
                          if [ "{mode}" = "exit" ]; then
                            exit 0
                          fi
                          ;;
                        *'"type":"prompt"'*)
                          id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
                          case "{mode}" in
                            reject)
                              printf '{{"type":"response","id":"%s","success":false,"error":"prompt rejected"}}\n' "$id"
                              ;;
                            *)
                              printf '{{"type":"response","id":"%s","success":true,"data":null}}\n' "$id"
                              case "{mode}" in
                                normal)
                                  printf '{{"type":"message_update","assistantMessageEvent":{{"type":"text_delta","delta":"Hello "}}}}\n'
                                  printf '{{"type":"message_update","assistantMessageEvent":{{"type":"text_delta","delta":"from fake OMP"}}}}\n'
                                  printf '{{"type":"agent_end"}}\n'
                                  ;;
                                ui)
                                  printf '{{"type":"extension_ui_request","id":"ui-1","method":"confirm","message":"Allow?"}}\n'
                                  ;;
                                tool)
                                  printf '{{"type":"message_update","assistantMessageEvent":{{"type":"toolcall_end","toolCall":{{"id":"tool-1","name":"task","arguments":{{"_i":"fan out"}}}}}}}}\n'
                                  printf '{{"type":"turn_end","message":{{"stopReason":"toolUse"}}}}\n'
                                  sleep 1
                                  printf '{{"type":"message_end","message":{{"role":"toolResult","id":"tool-1","toolName":"task","isError":true,"content":[{{"type":"text","text":"Tool call denied by user: task"}}]}}}}\n'
                                  printf '{{"type":"message_update","assistantMessageEvent":{{"type":"text_delta","delta":"Denied again"}}}}\n'
                                  printf '{{"type":"turn_end","message":{{"stopReason":"endTurn"}}}}\n'
                                  ;;
                                smoke)
                                  count_file="{pid_path}.count"
                                  count=$(cat "$count_file" 2>/dev/null || printf '0')
                                  count=$((count + 1))
                                  printf '%s' "$count" > "$count_file"
                                  if [ "$count" = "1" ]; then
                                    printf '{{"type":"message_update","assistantMessageEvent":{{"type":"text_delta","delta":"Hello "}}}}\n'
                                    printf '{{"type":"message_update","assistantMessageEvent":{{"type":"text_delta","delta":"from fake OMP smoke"}}}}\n'
                                    printf '{{"type":"agent_end"}}\n'
                                  else
                                    printf '{{"type":"message_update","assistantMessageEvent":{{"type":"toolcall_end","toolCall":{{"id":"tool-1","name":"task","arguments":{{"_i":"fan out"}}}}}}}}\n'
                                    printf '{{"type":"turn_end","message":{{"stopReason":"toolUse"}}}}\n'
                                    sleep 1
                                    printf '{{"type":"message_end","message":{{"role":"toolResult","id":"tool-1","toolName":"task","isError":true,"content":[{{"type":"text","text":"Tool call denied by user: task"}}]}}}}\n'
                                    printf '{{"type":"message_update","assistantMessageEvent":{{"type":"text_delta","delta":"Denied again"}}}}\n'
                                    printf '{{"type":"turn_end","message":{{"stopReason":"endTurn"}}}}\n'
                                  fi
                                  ;;
                                wait)
                                  ;;
                              esac
                              ;;
                          esac
                          ;;
                        *'"type":"extension_ui_response"'*)
                          printf '%s\n' "$line" > "{ui_response_path}"
                          printf '{{"type":"message_update","assistantMessageEvent":{{"type":"text_delta","delta":"denied"}}}}\n'
                          printf '{{"type":"turn_end"}}\n'
                          ;;
                        *'"type":"abort"'*)
                          printf '{{"type":"response","success":true,"data":null}}\n'
                          ;;
                      esac
                    done
                "#,
                    pid_path = pid_path.display(),
                    ui_response_path = ui_response_path.display(),
                    cwd_path = cwd_path.display(),
                    args_path = args_path.display(),
                    mode = mode,
                },
            )
            .unwrap();
            let mut permissions = fs::metadata(&script_path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&script_path, permissions).unwrap();
            Self {
                dir,
                script_path,
                pid_path,
                ui_response_path,
                cwd_path,
                args_path,
            }
        }

        /// Working directory the fake OMP child was launched in. Blocks briefly
        /// for the child to record it at startup.
        fn cwd(&self) -> PathBuf {
            PathBuf::from(self.read_startup(&self.cwd_path, "cwd"))
        }

        /// Space-joined argv the fake OMP child was launched with.
        fn args(&self) -> String {
            self.read_startup(&self.args_path, "args")
        }

        fn read_startup(&self, path: &Path, label: &str) -> String {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(value) = fs::read_to_string(path)
                    && !value.is_empty()
                {
                    return value;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "fake OMP did not record {label}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn command(&self) -> OmpCommand {
            OmpCommand {
                program: self.script_path.clone(),
                prefix_args: Vec::new(),
            }
        }

        fn pid(&self) -> i32 {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(pid) = fs::read_to_string(&self.pid_path)
                    && let Ok(pid) = pid.parse()
                {
                    return pid;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "fake OMP did not write pid"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    /// A FakeOmp variant that understands `--resume <id>`: it reports the
    /// resumed id as its OMP session id, replays a per-session transcript on
    /// `get_history`, writes a per-session pid file (so independent children
    /// can be tracked), and then stays alive blocked on stdin.
    struct FakeResumableOmp {
        dir: tempfile::TempDir,
        script_path: PathBuf,
    }

    impl FakeResumableOmp {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script_path = dir.path().join("fake-omp-resume.sh");
            fs::write(
                &script_path,
                formatdoc! {r#"
                    #!/bin/sh
                    resume=""
                    prev=""
                    for arg in "$@"; do
                      if [ "$prev" = "--resume" ]; then
                        resume="$arg"
                      fi
                      prev="$arg"
                    done
                    if [ -n "$resume" ]; then
                      sid="$resume"
                    else
                      sid="fresh-$$"
                    fi
                    printf '%s' "$$" > "{dir}/pid.$sid"
                    while IFS= read -r line; do
                      id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
                      case "$line" in
                        *'"type":"get_state"'*)
                          printf '{{"type":"response","id":"%s","success":true,"data":{{"sessionId":"%s","sessionName":"Resumed OMP","sessionFile":"{dir}/%s.jsonl"}}}}\n' "$id" "$sid" "$sid"
                          ;;
                        *'"type":"get_history"'*)
                          printf '{{"type":"history_message","role":"user","text":"earlier question for %s"}}\n' "$sid"
                          printf '{{"type":"history_message","role":"assistant","text":"earlier answer for %s"}}\n' "$sid"
                          printf '{{"type":"response","id":"%s","success":true,"data":null}}\n' "$id"
                          ;;
                        *'"type":"prompt"'*)
                          printf '{{"type":"response","id":"%s","success":true,"data":null}}\n' "$id"
                          ;;
                        *'"type":"abort"'*)
                          printf '{{"type":"response","id":"%s","success":true,"data":null}}\n' "$id"
                          ;;
                      esac
                    done
                "#,
                    dir = dir.path().display(),
                },
            )
            .unwrap();
            let mut permissions = fs::metadata(&script_path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&script_path, permissions).unwrap();
            Self { dir, script_path }
        }

        fn command(&self) -> OmpCommand {
            OmpCommand {
                program: self.script_path.clone(),
                prefix_args: Vec::new(),
            }
        }

        fn pid_for(&self, session_id: &str) -> i32 {
            let path = self.dir.path().join(format!("pid.{session_id}"));
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(pid) = fs::read_to_string(&path)
                    && let Ok(pid) = pid.parse()
                {
                    return pid;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "fake OMP did not write pid for {session_id}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    fn process_is_alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Deterministic fake OMP that drives the `extension_ui_request` roundtrip:
    /// it surfaces a request per `mode`, records every `extension_ui_response`
    /// it receives (appended, so double-answers are observable), and only
    /// "performs the action" (touches `mutation_path`) when an approval comes
    /// back `confirmed:true`. No network, no paid omp.
    struct FakeOmpUi {
        dir: tempfile::TempDir,
        script_path: PathBuf,
        responses_path: PathBuf,
        mutation_path: PathBuf,
    }

    impl FakeOmpUi {
        fn new(mode: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script_path = dir.path().join("fake-omp-ui.sh");
            let pid_path = dir.path().join("pid");
            let responses_path = dir.path().join("ui-responses");
            let mutation_path = dir.path().join("mutation");
            fs::write(
                &script_path,
                formatdoc! {r#"
                    #!/bin/sh
                    printf '%s' "$$" > "{pid_path}"
                    while IFS= read -r line; do
                      case "$line" in
                        *'"type":"get_state"'*)
                          id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
                          printf '{{"type":"response","id":"%s","success":true,"data":null}}\n' "$id"
                          ;;
                        *'"type":"prompt"'*)
                          id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
                          printf '{{"type":"response","id":"%s","success":true,"data":null}}\n' "$id"
                          case "{mode}" in
                            confirm)
                              printf '{{"type":"extension_ui_request","id":"ui-1","method":"confirm","message":"Write file?","tool":"write","path":"out.txt"}}\n'
                              ;;
                            input)
                              printf '{{"type":"extension_ui_request","id":"ui-1","method":"input","message":"Your name?","defaultValue":""}}\n'
                              ;;
                            select)
                              printf '{{"type":"extension_ui_request","id":"ui-1","method":"select","message":"Pick one","options":[{{"value":"a","label":"Option A"}},{{"value":"b","label":"Option B"}}]}}\n'
                              ;;
                            tool_gate)
                              printf '{{"type":"extension_ui_request","id":"ui-1","method":"select","title":"Allow tool: task","options":["Approve","Deny"]}}\n'
                              ;;
                            queue)
                              printf '{{"type":"extension_ui_request","id":"ui-1","method":"confirm","message":"First?","tool":"write","path":"a.txt"}}\n'
                              printf '{{"type":"extension_ui_request","id":"ui-2","method":"confirm","message":"Second?","tool":"write","path":"b.txt"}}\n'
                              ;;
                          esac
                          ;;
                        *'"type":"extension_ui_response"'*)
                          printf '%s\n' "$line" >> "{responses_path}"
                          delta="ack"
                          case "{mode}" in
                            input)
                              value=$(printf '%s\n' "$line" | sed -n 's/.*"value":"\([^"]*\)".*/\1/p')
                              delta="input:$value"
                              ;;
                            select)
                              sel=$(printf '%s\n' "$line" | sed -n 's/.*"value":"\([^"]*\)".*/\1/p')
                              delta="selected:$sel"
                              ;;
                            tool_gate)
                              val=$(printf '%s\n' "$line" | sed -n 's/.*"value":"\([^"]*\)".*/\1/p')
                              if [ "$val" = "Approve" ]; then
                                touch "{mutation_path}"
                                delta="gate:approved"
                              else
                                delta="gate:denied"
                              fi
                              ;;
                            *)
                              case "$line" in
                                *'"confirmed":true'*)
                                  touch "{mutation_path}"
                                  delta="result:approved"
                                  ;;
                                *)
                                  delta="result:denied"
                                  ;;
                              esac
                              ;;
                          esac
                          printf '{{"type":"message_update","assistantMessageEvent":{{"type":"text_delta","delta":"%s"}}}}\n' "$delta"
                          case "{mode}" in
                            queue) ;;
                            *) printf '{{"type":"turn_end","message":{{"stopReason":"endTurn"}}}}\n' ;;
                          esac
                          ;;
                        *'"type":"abort"'*)
                          printf '{{"type":"response","success":true,"data":null}}\n'
                          ;;
                      esac
                    done
                "#,
                    pid_path = pid_path.display(),
                    responses_path = responses_path.display(),
                    mutation_path = mutation_path.display(),
                    mode = mode,
                },
            )
            .unwrap();
            let mut permissions = fs::metadata(&script_path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&script_path, permissions).unwrap();
            Self {
                dir,
                script_path,
                responses_path,
                mutation_path,
            }
        }

        fn command(&self) -> OmpCommand {
            OmpCommand {
                program: self.script_path.clone(),
                prefix_args: Vec::new(),
            }
        }

        fn responses(&self) -> String {
            fs::read_to_string(&self.responses_path).unwrap_or_default()
        }
    }

    async fn start_ui_session(
        fixture: &FakeOmpUi,
        cx: &mut TestAppContext,
    ) -> (Rc<OmpAgentConnection>, gpui::Entity<AcpThread>) {
        let connection = Rc::new(OmpAgentConnection::new(fixture.command()));
        let project = Project::example([fixture.dir.path()], &mut cx.to_async()).await;
        let thread = cx
            .update(|cx| {
                connection
                    .clone()
                    .new_session(project, PathList::new(&[fixture.dir.path()]), cx)
            })
            .await
            .unwrap();
        (connection, thread)
    }

    async fn wait_for_ui_requests(
        thread: &gpui::Entity<AcpThread>,
        count: usize,
        cx: &mut TestAppContext,
    ) -> Vec<SharedString> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let ids = thread.read_with(cx, |thread, _| {
                thread
                    .ui_requests()
                    .iter()
                    .map(|request| request.id.clone())
                    .collect::<Vec<_>>()
            });
            if ids.len() >= count {
                return ids;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "expected {count} OMP UI request(s) to surface"
            );
            cx.background_executor
                .timer(Duration::from_millis(20))
                .await;
        }
    }

    #[gpui::test]
    async fn omp_ui_approve_sends_confirmed_true_and_runs_action(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeOmpUi::new("confirm");
        let (_connection, thread) = start_ui_session(&fixture, cx).await;

        let send = thread.update(cx, |thread, cx| thread.send_raw("write please", cx));
        let id = wait_for_ui_requests(&thread, 1, cx).await.remove(0);
        thread.update(cx, |thread, cx| {
            thread.respond_to_ui_request(id, UiResponse::Approve, cx)
        });
        send.await.unwrap();
        cx.run_until_parked();

        assert!(
            fixture.responses().contains("\"confirmed\":true"),
            "approve must send the exact positive response, got: {}",
            fixture.responses()
        );
        assert!(
            fixture.mutation_path.exists(),
            "approved action should have run"
        );
        thread.read_with(cx, |thread, cx| {
            assert!(thread.to_markdown(cx).contains("result:approved"));
            assert!(thread.ui_requests().is_empty());
        });
    }

    #[gpui::test]
    async fn omp_ui_deny_blocks_action_without_mutation(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeOmpUi::new("confirm");
        let (_connection, thread) = start_ui_session(&fixture, cx).await;

        let send = thread.update(cx, |thread, cx| thread.send_raw("write please", cx));
        let id = wait_for_ui_requests(&thread, 1, cx).await.remove(0);
        thread.update(cx, |thread, cx| {
            thread.respond_to_ui_request(id, UiResponse::Deny, cx)
        });
        send.await.unwrap();
        cx.run_until_parked();

        assert!(fixture.responses().contains("\"confirmed\":false"));
        assert!(
            !fixture.mutation_path.exists(),
            "denied action must leave the project unchanged"
        );
        thread.read_with(cx, |thread, cx| {
            assert!(thread.to_markdown(cx).contains("result:denied"));
        });
    }

    #[gpui::test]
    async fn omp_ui_input_roundtrips_into_transcript(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeOmpUi::new("input");
        let (_connection, thread) = start_ui_session(&fixture, cx).await;

        let send = thread.update(cx, |thread, cx| thread.send_raw("ask name", cx));
        let id = wait_for_ui_requests(&thread, 1, cx).await.remove(0);
        thread.update(cx, |thread, cx| {
            thread.respond_to_ui_request(id, UiResponse::Input("Ada Lovelace".to_owned()), cx)
        });
        send.await.unwrap();
        cx.run_until_parked();

        assert!(fixture.responses().contains("\"value\":\"Ada Lovelace\""));
        thread.read_with(cx, |thread, cx| {
            assert!(thread.to_markdown(cx).contains("Ada Lovelace"));
        });
    }

    #[gpui::test]
    async fn omp_ui_select_roundtrips_into_transcript(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeOmpUi::new("select");
        let (_connection, thread) = start_ui_session(&fixture, cx).await;

        let send = thread.update(cx, |thread, cx| thread.send_raw("pick", cx));
        let id = wait_for_ui_requests(&thread, 1, cx).await.remove(0);
        thread.read_with(cx, |thread, _| {
            let options = &thread.ui_requests()[0].options;
            assert_eq!(options.len(), 2);
            assert_eq!(options[0].id.as_ref(), "a");
            assert_eq!(options[1].id.as_ref(), "b");
        });
        thread.update(cx, |thread, cx| {
            thread.respond_to_ui_request(id, UiResponse::Select("b".into()), cx)
        });
        send.await.unwrap();
        cx.run_until_parked();

        assert!(fixture.responses().contains("\"value\":\"b\""));
        thread.read_with(cx, |thread, cx| {
            assert!(thread.to_markdown(cx).contains("selected:b"));
        });
    }

    /// Regression: a real OMP tool gate arrives as `method:"select"` with plain
    /// string options `["Approve","Deny"]`. Approving must round-trip the choice
    /// as the `value` field — OMP ignores `selected`, so the wrong field silently
    /// denied every gated tool (e.g. `task`) even when the user clicked Approve.
    #[gpui::test]
    async fn omp_tool_gate_select_approve_sends_value_and_runs(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeOmpUi::new("tool_gate");
        let (_connection, thread) = start_ui_session(&fixture, cx).await;

        let send = thread.update(cx, |thread, cx| thread.send_raw("spawn subagents", cx));
        let id = wait_for_ui_requests(&thread, 1, cx).await.remove(0);
        thread.read_with(cx, |thread, _| {
            let request = &thread.ui_requests()[0];
            assert_eq!(request.kind, UiRequestKind::Select);
            let options: Vec<&str> = request.options.iter().map(|o| o.id.as_ref()).collect();
            assert_eq!(options, vec!["Approve", "Deny"]);
        });
        thread.update(cx, |thread, cx| {
            thread.respond_to_ui_request(id, UiResponse::Select("Approve".into()), cx)
        });
        send.await.unwrap();
        cx.run_until_parked();

        assert!(
            fixture.responses().contains("\"value\":\"Approve\""),
            "approval must round-trip as the `value` field, not `selected`"
        );
        assert!(
            fixture.mutation_path.exists(),
            "approving the gate must let the tool run"
        );
        thread.read_with(cx, |thread, cx| {
            assert!(thread.to_markdown(cx).contains("gate:approved"));
        });
    }

    #[gpui::test]
    async fn omp_ui_request_cannot_be_answered_twice(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeOmpUi::new("confirm");
        let (_connection, thread) = start_ui_session(&fixture, cx).await;

        let send = thread.update(cx, |thread, cx| thread.send_raw("write please", cx));
        let id = wait_for_ui_requests(&thread, 1, cx).await.remove(0);
        thread.update(cx, |thread, cx| {
            thread.respond_to_ui_request(id.clone(), UiResponse::Approve, cx)
        });
        send.await.unwrap();
        cx.run_until_parked();
        // A second answer for the same id must be dropped.
        thread.update(cx, |thread, cx| {
            thread.respond_to_ui_request(id, UiResponse::Deny, cx)
        });
        cx.run_until_parked();

        let response_lines = fixture.responses().lines().count();
        assert_eq!(response_lines, 1, "a request must be answered exactly once");
    }

    #[gpui::test]
    async fn omp_ui_request_rejected_after_cancellation(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeOmpUi::new("confirm");
        let (connection, thread) = start_ui_session(&fixture, cx).await;

        let send = thread.update(cx, |thread, cx| thread.send_raw("write please", cx));
        let id = wait_for_ui_requests(&thread, 1, cx).await.remove(0);
        let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());
        cx.update(|cx| connection.cancel(&session_id, cx));
        send.await.unwrap();
        cx.run_until_parked();
        // The request was cleared on cancellation; a late answer is a no-op.
        thread.update(cx, |thread, cx| {
            thread.respond_to_ui_request(id, UiResponse::Approve, cx)
        });
        cx.run_until_parked();

        assert!(
            !fixture.responses().contains("\"confirmed\":true"),
            "no answer may reach OMP after cancellation"
        );
        assert!(!fixture.mutation_path.exists());
        thread.read_with(cx, |thread, _| {
            assert!(thread.ui_requests().is_empty());
        });
    }

    #[gpui::test]
    async fn omp_ui_requests_queue_in_arrival_order(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeOmpUi::new("queue");
        let (connection, thread) = start_ui_session(&fixture, cx).await;

        let _send = thread.update(cx, |thread, cx| thread.send_raw("two asks", cx));
        let ids = wait_for_ui_requests(&thread, 2, cx).await;
        assert_eq!(ids[0].as_ref(), "ui-1");
        assert_eq!(ids[1].as_ref(), "ui-2");

        thread.update(cx, |thread, cx| {
            thread.respond_to_ui_request(ids[0].clone(), UiResponse::Approve, cx)
        });
        thread.update(cx, |thread, cx| {
            thread.respond_to_ui_request(ids[1].clone(), UiResponse::Deny, cx)
        });
        cx.run_until_parked();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while fixture.responses().lines().count() < 2 && std::time::Instant::now() < deadline {
            cx.background_executor
                .timer(Duration::from_millis(20))
                .await;
            cx.run_until_parked();
        }

        let responses = fixture.responses();
        let lines: Vec<&str> = responses.lines().collect();
        assert_eq!(lines.len(), 2, "both queued requests should be answered");
        assert!(lines[0].contains("\"id\":\"ui-1\"") && lines[0].contains("\"confirmed\":true"));
        assert!(lines[1].contains("\"id\":\"ui-2\"") && lines[1].contains("\"confirmed\":false"));

        let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());
        cx.update(|cx| connection.clone().close_session(&session_id, cx))
            .await
            .unwrap();
    }

    /// A fake OMP that, on a prompt, streams a recorded subagent telemetry
    /// sequence: a main-agent `task` spawns `TaskA`/`TaskB` (roots), `TaskA`
    /// makes its own `task` call (tool `t1`) and spawns the grandchild
    /// `TaskA1`, progress frames update `TaskA` twice (dedup), and child events
    /// feed the drill-in transcript. Frames are catted from a plain-JSON file
    /// to avoid shell brace-escaping. No network, no paid omp.
    struct FakeOmpSubagents {
        dir: tempfile::TempDir,
        script_path: PathBuf,
    }

    impl FakeOmpSubagents {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script_path = dir.path().join("fake-omp-subagents.sh");
            let frames_path = dir.path().join("frames.jsonl");
            let frames = concat!(
                r#"{"type":"subagent_lifecycle","payload":{"id":"TaskA","agent":"quick_task","parentToolCallId":"t0","status":"started","index":0}}"#,
                "\n",
                r#"{"type":"subagent_lifecycle","payload":{"id":"TaskB","agent":"quick_task","parentToolCallId":"t0","status":"started","index":1}}"#,
                "\n",
                r#"{"type":"subagent_progress","payload":{"agent":"quick_task","parentToolCallId":"t0","task":"read files","progress":{"id":"TaskA","status":"running","toolCount":1,"tokens":100,"cost":0.01,"resolvedModel":"claude","recentTools":["read"]}}}"#,
                "\n",
                r#"{"type":"subagent_event","payload":{"id":"TaskA","event":{"type":"tool_execution_start","id":"t1","name":"task"}}}"#,
                "\n",
                r#"{"type":"subagent_lifecycle","payload":{"id":"TaskA1","agent":"quick_task","parentToolCallId":"t1","status":"started","index":0}}"#,
                "\n",
                r#"{"type":"subagent_progress","payload":{"parentToolCallId":"t0","progress":{"id":"TaskA","status":"running","toolCount":2,"tokens":200,"cost":0.02}}}"#,
                "\n",
                r#"{"type":"subagent_event","payload":{"id":"TaskA","event":{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"hello from A"}]}}}}"#,
                "\n",
                r#"{"type":"subagent_event","payload":{"id":"TaskB","event":{"type":"agent_end"}}}"#,
                "\n",
                r#"{"type":"subagent_progress","payload":{"parentToolCallId":"t0","progress":{"id":"TaskB","status":"completed","toolCount":0}}}"#,
                "\n",
                r#"{"type":"turn_end","message":{"stopReason":"endTurn"}}"#,
                "\n",
            );
            fs::write(&frames_path, frames).unwrap();
            fs::write(
                &script_path,
                formatdoc! {r#"
                    #!/bin/sh
                    while IFS= read -r line; do
                      case "$line" in
                        *'"type":"get_state"'*)
                          id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
                          printf '{{"type":"response","id":"%s","success":true,"data":null}}\n' "$id"
                          ;;
                        *'"type":"set_subagent_subscription"'*)
                          id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
                          printf '{{"type":"response","id":"%s","success":true,"data":{{"level":"events"}}}}\n' "$id"
                          ;;
                        *'"type":"prompt"'*)
                          id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
                          printf '{{"type":"response","id":"%s","success":true,"data":null}}\n' "$id"
                          cat "{frames_path}"
                          ;;
                        *'"type":"abort"'*)
                          printf '{{"type":"response","success":true,"data":null}}\n'
                          ;;
                      esac
                    done
                "#,
                    frames_path = frames_path.display(),
                },
            )
            .unwrap();
            let mut permissions = fs::metadata(&script_path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&script_path, permissions).unwrap();
            Self { dir, script_path }
        }

        fn command(&self) -> OmpCommand {
            OmpCommand {
                program: self.script_path.clone(),
                prefix_args: Vec::new(),
            }
        }
    }

    #[gpui::test]
    async fn omp_subagent_frames_build_nested_tree_and_tail_transcript(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeOmpSubagents::new();
        let connection = Rc::new(OmpAgentConnection::new(fixture.command()));
        let project = Project::example([fixture.dir.path()], &mut cx.to_async()).await;
        let thread = cx
            .update(|cx| {
                connection
                    .clone()
                    .new_session(project, PathList::new(&[fixture.dir.path()]), cx)
            })
            .await
            .unwrap();

        let send = thread.update(cx, |thread, cx| thread.send_raw("fan out", cx));
        send.await.unwrap();
        cx.run_until_parked();

        thread.read_with(cx, |thread, _| {
            let subagents = thread.subagents();
            assert_eq!(
                subagents
                    .iter()
                    .filter(|node| node.id.as_ref() == "TaskA")
                    .count(),
                1,
                "repeated TaskA frames must update in place, not duplicate"
            );
            assert_eq!(
                subagents.len(),
                3,
                "TaskA, TaskB, and the grandchild TaskA1"
            );

            let task_a = subagents.iter().find(|n| n.id.as_ref() == "TaskA").unwrap();
            let task_b = subagents.iter().find(|n| n.id.as_ref() == "TaskB").unwrap();
            let task_a1 = subagents
                .iter()
                .find(|n| n.id.as_ref() == "TaskA1")
                .unwrap();

            // Parent/child structure: main's children are roots; TaskA1 nests
            // under TaskA via the resolved tool-call ownership.
            assert_eq!(task_a.parent_id, None, "TaskA spawned by the main agent");
            assert_eq!(task_b.parent_id, None, "TaskB spawned by the main agent");
            assert_eq!(
                task_a1.parent_id.as_deref(),
                Some("TaskA"),
                "TaskA1 nests under TaskA"
            );

            // Progress merge: latest counter wins; an earlier-only field (the
            // recentTools/model from the first progress) is not clobbered by the
            // second progress frame that omits it.
            assert_eq!(task_a.tool_count, Some(2));
            assert_eq!(task_a.tokens, Some(200));
            assert_eq!(task_a.recent_tools, vec![SharedString::from("read")]);
            assert_eq!(task_a.model.as_deref(), Some("claude"));
            assert_eq!(task_b.status.as_ref(), "completed");

            // Drill-in transcript tails the streamed child events in order.
            let a_lines: Vec<String> = thread
                .subagent_transcript("TaskA")
                .iter()
                .map(SharedString::to_string)
                .collect();
            assert_eq!(
                a_lines,
                vec!["→ task".to_string(), "hello from A".to_string()]
            );
            let b_lines: Vec<String> = thread
                .subagent_transcript("TaskB")
                .iter()
                .map(SharedString::to_string)
                .collect();
            assert_eq!(b_lines, vec!["✓ finished".to_string()]);
        });
    }

    #[test]
    fn parse_subagent_reads_lifecycle_and_nested_progress_shapes() {
        let lifecycle: Value = serde_json::from_str(
            r#"{"id":"TaskA","agent":"quick_task","parentToolCallId":"t0","status":"started","index":1}"#,
        )
        .unwrap();
        let node = parse_subagent(&lifecycle).unwrap();
        assert_eq!(node.id.as_ref(), "TaskA");
        assert_eq!(node.agent.as_ref(), "quick_task");
        assert_eq!(node.parent_id.as_deref(), Some("t0"));
        assert_eq!(node.index, 1);
        assert_eq!(node.status.as_ref(), "started");
        assert_eq!(
            node.tool_count, None,
            "a lifecycle frame carries no counters"
        );

        let progress: Value = serde_json::from_str(
            r#"{"agent":"quick_task","parentToolCallId":"t0","task":"do a thing\nsecond line","progress":{"id":"TaskA","status":"running","toolCount":3,"tokens":42,"cost":0.5,"resolvedModel":"gpt","recentTools":["read","edit"]}}"#,
        )
        .unwrap();
        let node = parse_subagent(&progress).unwrap();
        assert_eq!(
            node.id.as_ref(),
            "TaskA",
            "id is read from the nested progress"
        );
        assert_eq!(node.status.as_ref(), "running");
        assert_eq!(
            node.task.as_deref(),
            Some("do a thing"),
            "task is the first line only"
        );
        assert_eq!(node.tool_count, Some(3));
        assert_eq!(node.tokens, Some(42));
        assert_eq!(node.cost, Some(0.5));
        assert_eq!(node.model.as_deref(), Some("gpt"));
        assert_eq!(
            node.recent_tools,
            vec![SharedString::from("read"), SharedString::from("edit")]
        );
    }

    #[test]
    fn subagent_event_line_renders_meaningful_events_only() {
        let tool: Value =
            serde_json::from_str(r#"{"type":"tool_execution_start","id":"t1","name":"read"}"#)
                .unwrap();
        assert_eq!(subagent_event_line(&tool).as_deref(), Some("→ read"));
        let message: Value = serde_json::from_str(
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}"#,
        )
        .unwrap();
        assert_eq!(subagent_event_line(&message).as_deref(), Some("hi"));
        let done: Value = serde_json::from_str(r#"{"type":"agent_end"}"#).unwrap();
        assert_eq!(subagent_event_line(&done).as_deref(), Some("✓ finished"));
        let delta: Value = serde_json::from_str(
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"x"}}"#,
        )
        .unwrap();
        assert_eq!(
            subagent_event_line(&delta),
            None,
            "partial deltas are skipped"
        );
        let tool_result: Value = serde_json::from_str(
            r#"{"type":"message_end","message":{"role":"toolResult","content":[{"type":"text","text":"out"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            subagent_event_line(&tool_result),
            None,
            "non-assistant messages are skipped"
        );
    }

    /// A fake OMP for the slash-command path: it emits an
    /// `available_commands_update` at startup (before the panel thread attaches,
    /// exercising the cache-and-flush), and treats any prompt as a local-only
    /// slash command — emitting `command_output` and acking with
    /// `data.agentInvoked:false` and no `agent_end`/`turn_end`. No paid omp.
    struct FakeOmpCommands {
        dir: tempfile::TempDir,
        script_path: PathBuf,
    }

    impl FakeOmpCommands {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script_path = dir.path().join("fake-omp-commands.sh");
            fs::write(
                &script_path,
                formatdoc! {r#"
                    #!/bin/sh
                    while IFS= read -r line; do
                      case "$line" in
                        *'"type":"get_state"'*)
                          id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
                          printf '{{"type":"available_commands_update","commands":[{{"name":"model","description":"Show current model"}},{{"name":"fast","description":"Toggle fast mode","input":{{"hint":"[on|off|status]"}},"source":"builtin"}}]}}\n'
                          printf '{{"type":"response","id":"%s","success":true,"data":null}}\n' "$id"
                          ;;
                        *'"type":"prompt"'*)
                          id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
                          printf '{{"type":"command_output","text":"Fast mode is off."}}\n'
                          printf '{{"type":"response","id":"%s","success":true,"data":{{"agentInvoked":false}}}}\n' "$id"
                          ;;
                        *'"type":"abort"'*)
                          printf '{{"type":"response","success":true,"data":null}}\n'
                          ;;
                      esac
                    done
                "#},
            )
            .unwrap();
            let mut permissions = fs::metadata(&script_path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&script_path, permissions).unwrap();
            Self { dir, script_path }
        }

        fn command(&self) -> OmpCommand {
            OmpCommand {
                program: self.script_path.clone(),
                prefix_args: Vec::new(),
            }
        }
    }

    async fn start_commands_session(
        fixture: &FakeOmpCommands,
        cx: &mut TestAppContext,
    ) -> gpui::Entity<AcpThread> {
        let connection = Rc::new(OmpAgentConnection::new(fixture.command()));
        let project = Project::example([fixture.dir.path()], &mut cx.to_async()).await;
        cx.update(|cx| {
            connection
                .clone()
                .new_session(project, PathList::new(&[fixture.dir.path()]), cx)
        })
        .await
        .unwrap()
    }

    #[gpui::test]
    async fn omp_available_commands_surface_to_thread(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeOmpCommands::new();
        let thread = start_commands_session(&fixture, cx).await;
        cx.run_until_parked();

        thread.read_with(cx, |thread, _| {
            let names: Vec<String> = thread
                .available_commands()
                .iter()
                .map(|command| command.name.to_string())
                .collect();
            assert_eq!(
                names,
                vec!["model".to_string(), "fast".to_string()],
                "startup commands surface to the composer after the thread attaches"
            );
            let fast = thread
                .available_commands()
                .iter()
                .find(|command| command.name == "fast")
                .unwrap();
            assert!(
                fast.input.is_some(),
                "a hinted command keeps its input hint"
            );
        });
    }

    #[gpui::test]
    async fn omp_local_slash_command_completes_without_hanging(cx: &mut TestAppContext) {
        crate::e2e_tests::init_test(cx).await;
        let fixture = FakeOmpCommands::new();
        let thread = start_commands_session(&fixture, cx).await;

        // A local-only command acks with agentInvoked:false and no agent_end;
        // the turn must still complete (awaiting `send` would hang otherwise).
        let send = thread.update(cx, |thread, cx| thread.send_raw("/fast status", cx));
        send.await.unwrap();
        cx.run_until_parked();

        thread.read_with(cx, |thread, cx| {
            assert!(
                thread.to_markdown(cx).contains("Fast mode is off."),
                "local command output lands in the transcript"
            );
            assert!(
                !thread.available_commands().is_empty(),
                "the command list survives the local turn"
            );
        });
    }

    #[test]
    fn parse_available_commands_maps_names_and_hints() {
        let frame: Value = serde_json::from_str(
            r#"{"commands":[{"name":"model","description":"Show model"},{"name":"fast","description":"Toggle","input":{"hint":"[on|off]"}}]}"#,
        )
        .unwrap();
        let commands = parse_available_commands(&frame);
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].name.to_string(), "model");
        assert!(commands[0].input.is_none(), "no hint -> no input");
        assert_eq!(commands[1].name.to_string(), "fast");
        assert!(
            commands[1].input.is_some(),
            "hint maps to unstructured input"
        );
        // OMP commands carry no category, so Zed sends the full `/command args`
        // line to the agent instead of stripping the name and queueing the
        // argument as a separate prompt (`leading_native_command` is Native-only).
        assert!(
            acp_thread::command_category_from_meta(&commands[1].meta).is_none(),
            "omp commands must not be Native, or their arguments get split off"
        );
    }

    #[test]
    fn parse_available_commands_empty_when_missing() {
        let frame: Value = serde_json::from_str(r#"{"type":"available_commands_update"}"#).unwrap();
        assert!(
            parse_available_commands(&frame).is_empty(),
            "a missing command list degrades to an empty picker"
        );
    }

    #[test]
    fn prompt_completed_locally_detects_agent_invoked_false() {
        let local: Value = serde_json::from_str(r#"{"data":{"agentInvoked":false}}"#).unwrap();
        assert!(prompt_completed_locally(&local));
        let agent: Value = serde_json::from_str(r#"{"data":{"agentInvoked":true}}"#).unwrap();
        assert!(!prompt_completed_locally(&agent));
        let null: Value = serde_json::from_str(r#"{"data":null}"#).unwrap();
        assert!(
            !prompt_completed_locally(&null),
            "no signal -> rely on agent_end"
        );
    }
}
