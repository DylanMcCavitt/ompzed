use crate::{AgentServer, AgentServerDelegate};
use acp_thread::{
    AcpThread, AgentConnection, UiRequest, UiRequestKind, UiRequestOption, UiRequestScope,
    UiResponse, UserMessageId, meta_with_tool_name,
};
use action_log::ActionLog;
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result, anyhow};
use collections::{HashMap, HashSet};
use futures::{
    AsyncBufReadExt as _, AsyncWriteExt as _, StreamExt as _, channel::oneshot, io::BufReader,
};
use gpui::{App, AppContext as _, AsyncApp, Entity, SharedString, Task};
use project::{AgentId, Project};
use serde_json::{Value, json};
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
        let command = OmpCommand::default();
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
                pending_ui_requests: RefCell::default(),
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
            Some("extension_ui_request") => self.handle_ui_request(&frame, cx),
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
            UiResponse::Select(option_id) => json!({ "selected": option_id.to_string() }),
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
                              sel=$(printf '%s\n' "$line" | sed -n 's/.*"selected":"\([^"]*\)".*/\1/p')
                              delta="selected:$sel"
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

        assert!(fixture.responses().contains("\"selected\":\"b\""));
        thread.read_with(cx, |thread, cx| {
            assert!(thread.to_markdown(cx).contains("selected:b"));
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
}
