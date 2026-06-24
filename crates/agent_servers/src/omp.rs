use crate::{AgentServer, AgentServerDelegate};
use acp_thread::{AcpThread, AgentConnection, UserMessageId, meta_with_tool_name};
use action_log::ActionLog;
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use futures::{
    AsyncBufReadExt as _, AsyncWriteExt as _, FutureExt as _, StreamExt as _, channel::oneshot,
    io::BufReader,
};
use gpui::{App, AppContext as _, AsyncApp, Entity, SharedString, Subscription, Task};
use project::{AgentId, Project};
use serde_json::{Value, json};
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
        let command = OmpCommand::default();
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
            session.state.reject_pending_requests("OMP agent shutting down");
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
            let state = cx
                .update(|cx| OmpSessionState::spawn(command, &work_dir, resume_id.as_deref(), cx))?;

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
            if let Some(descriptor) = descriptor.as_ref() {
                state.apply_descriptor(descriptor, cx);
            }
            connection
                .sessions
                .borrow_mut()
                .insert(session_id, OmpSession { state: state.clone() });
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
        while pids.iter().any(|pid| process_is_alive(*pid))
            && std::time::Instant::now() < deadline
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

    struct FakeOmp {
        dir: tempfile::TempDir,
        script_path: PathBuf,
        pid_path: PathBuf,
        ui_response_path: PathBuf,
    }

    impl FakeOmp {
        fn new(mode: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script_path = dir.path().join("fake-omp.sh");
            let pid_path = dir.path().join("pid");
            let ui_response_path = dir.path().join("ui-response");
            fs::write(
                &script_path,
                formatdoc! {r#"
                    #!/bin/sh
                    printf '%s' "$$" > "{pid_path}"
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
}
