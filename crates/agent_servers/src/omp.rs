use crate::{AgentServer, AgentServerDelegate};
use acp_thread::{AcpThread, AgentConnection, UserMessageId, meta_with_tool_name};
use action_log::ActionLog;
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use futures::{
    AsyncBufReadExt as _, AsyncWriteExt as _, StreamExt as _, channel::oneshot, io::BufReader,
};
use gpui::{App, AppContext as _, AsyncApp, Entity, SharedString, Task};
use project::{AgentId, Project};
use serde_json::{Value, json};
use settings::Settings as _;
use std::{
    any::Any,
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    process::Stdio,
    rc::{Rc, Weak},
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
        cx.spawn(async move |_| {
            Ok(Rc::new(OmpAgentConnection::new(command)) as Rc<dyn AgentConnection>)
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
}

impl OmpAgentConnection {
    fn new(command: OmpCommand) -> Self {
        Self {
            command,
            sessions: RefCell::default(),
            next_session_id: Cell::new(0),
        }
    }

    fn next_session_id(&self) -> acp::SessionId {
        let id = self.next_session_id.get();
        self.next_session_id.set(id + 1);
        acp::SessionId::new(format!("omp-{id}"))
    }

    fn create_session(
        self: &Rc<Self>,
        session_id: acp::SessionId,
        project: Entity<Project>,
        work_dirs: PathList,
        cx: &mut App,
    ) -> Result<Entity<AcpThread>> {
        let work_dir = work_dirs
            .ordered_paths()
            .next()
            .cloned()
            .context("OMP agent requires a workspace directory")?;
        let state = OmpSessionState::spawn(self.command.clone(), &work_dir, cx)?;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let thread: Entity<AcpThread> = cx.new(|cx| {
            AcpThread::new(
                None,
                None,
                Some(work_dirs),
                self.clone(),
                project,
                action_log,
                session_id.clone(),
                watch::Receiver::constant(acp::PromptCapabilities::new()),
                cx,
            )
        });
        state.set_thread(thread.downgrade());
        state.request_state_update(cx);
        self.sessions
            .borrow_mut()
            .insert(session_id, OmpSession { state });
        Ok(thread)
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
        let session_id = self.next_session_id();
        Task::ready(self.create_session(session_id, project, work_dirs, cx))
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

    fn cancel(&self, session_id: &acp::SessionId, _cx: &mut App) {
        if let Some(session) = self.sessions.borrow().get(session_id) {
            session.state.send_abort();
            session.state.cancel_active_turn();
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

struct OmpSessionState {
    outgoing: async_channel::Sender<Value>,
    child: RefCell<Option<Child>>,
    thread: RefCell<Option<gpui::WeakEntity<AcpThread>>>,
    pending_prompt: RefCell<Option<PendingPrompt>>,
    pending_requests: RefCell<HashMap<String, oneshot::Sender<Result<Value, String>>>>,
    closed: Cell<bool>,
    next_request_id: Cell<u64>,
    _read_task: Task<()>,
    _write_task: Task<Result<()>>,
    _stderr_task: Task<Result<()>>,
}

impl OmpSessionState {
    fn spawn(command: OmpCommand, work_dir: &Path, cx: &mut App) -> Result<Rc<Self>> {
        let mut cmd = std::process::Command::new(&command.program);
        cmd.args(&command.prefix_args)
            .arg("--mode")
            .arg("rpc-ui")
            .arg("--cwd")
            .arg(work_dir)
            .arg("--approval-mode")
            .arg("always-ask")
            .current_dir(work_dir);
        if let Some(path) = augmented_path() {
            cmd.env("PATH", path);
        }

        let mut child = Child::spawn(cmd, Stdio::piped(), Stdio::piped(), Stdio::piped())?;
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

    fn request_state_update(self: &Rc<Self>, cx: &mut App) {
        let Ok(rx) = self.send_request("get_state", json!({ "type": "get_state" })) else {
            return;
        };
        let weak = Rc::downgrade(self);
        cx.spawn(async move |cx| {
            let Ok(Ok(state)) = rx.await else {
                return;
            };
            if let Some(state_ref) = weak.upgrade() {
                state_ref.apply_state_update(state, cx);
            }
        })
        .detach();
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
            Some("extension_ui_request") => self.handle_ui_request(&frame),
            _ => {}
        }
    }

    fn resolve_response(&self, frame: Value) {
        let Some(id) = frame.get("id").and_then(Value::as_str) else {
            return;
        };
        let is_error = frame.get("success").and_then(Value::as_bool) == Some(false);
        if is_error
            && self
                .pending_prompt
                .borrow()
                .as_ref()
                .is_some_and(|prompt| prompt.id == id)
        {
            if let Some(prompt) = self.pending_prompt.borrow_mut().take() {
                prompt.tx.send(Err(anyhow!(response_error(&frame)))).ok();
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

    fn handle_ui_request(&self, frame: &Value) {
        let Some(id) = frame.get("id").and_then(Value::as_str) else {
            return;
        };
        let method = frame.get("method").and_then(Value::as_str);
        let response = match method {
            Some("confirm") => json!({
                "type": "extension_ui_response",
                "id": id,
                "confirmed": false,
            }),
            Some("select" | "input" | "editor" | "cancel") => json!({
                "type": "extension_ui_response",
                "id": id,
                "cancelled": true,
            }),
            _ => return,
        };
        log::warn!(
            "default-denying OMP UI request `{}` ({})",
            method.unwrap_or("unknown"),
            id
        );
        self.send_frame(response).log_err();
    }

    fn apply_state_update(&self, state: Value, cx: &mut AsyncApp) {
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
        let Some(title) = session_file
            .map(|file| match session_name {
                Some(name) => format!("{name} · {file}"),
                None => format!("OMP · {file}"),
            })
            .or_else(|| session_name.map(ToOwned::to_owned))
        else {
            return;
        };
        if let Some(thread) = self
            .thread
            .borrow()
            .as_ref()
            .and_then(|thread| thread.upgrade())
        {
            let mut info = acp::SessionInfoUpdate::new().title(title);
            let meta = omp_session_meta(session_id, session_file);
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

fn response_error(frame: &Value) -> String {
    frame
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("OMP request failed")
        .to_owned()
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
        // reports `sessionFile` via get_state; `apply_state_update` renders it
        // into the thread title (`{sessionName} · {sessionFile}`) and the
        // `omp_session_file` session meta.
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

        thread
            .update(cx, |thread, cx| thread.send_raw("needs approval", cx))
            .await
            .unwrap();
        cx.run_until_parked();

        let seen_response = fs::read_to_string(&fixture.ui_response_path)
            .expect("fake OMP should record fail-closed response");
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

    fn process_is_alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }
}
