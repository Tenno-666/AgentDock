//! Attached-session manager: owns up to N `kiro-cli acp` processes and broadcasts
//! their events to every connected client.
//!
//! Permission requests are forwarded to clients as `permission.request`; the first
//! `permission.reply` wins. Nothing is approved automatically: an unanswered request
//! keeps the agent waiting. Every request and resolution is appended to an audit log.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};

use crate::acp::client::{AcpClient, AcpError, Frame, Incoming};
use crate::discovery;
use crate::session_log::{MAX_TOOL_OUTPUT_CHARS, truncate};

pub const MAX_PROMPT_BYTES: usize = 100 * 1024;
const EVENT_BUFFER: usize = 1024;
/// Text of the kiro-cli session-lock error (S0 T3).
const BUSY_MARKER: &str = "active in another process";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HostError {
    pub code: &'static str,
    pub message: String,
}

impl HostError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct HostConfig {
    pub program: String,
    pub args: Vec<String>,
    pub max_sessions: usize,
    pub init_timeout: Duration,
    pub load_timeout: Duration,
    pub prompt_timeout: Duration,
    pub sessions_dir: PathBuf,
    /// `session.new` only accepts working directories under this root.
    pub allowed_root: PathBuf,
    /// Append-only JSONL audit log of permission requests; None disables it.
    pub audit_path: Option<PathBuf>,
}

/// Locate kiro-cli: $AGENTDOCK_KIRO_CLI, then $PATH, then ~/.local/bin (its install
/// location). Needed because services started by systemd/desktop do not get the
/// interactive shell PATH.
pub fn resolve_kiro_cli(
    env_override: Option<&str>,
    path_var: Option<&str>,
    home: &Path,
) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| {
        p.is_file() && std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    };
    if let Some(p) = env_override.filter(|s| !s.is_empty()) {
        let p = PathBuf::from(p);
        return executable(&p).then_some(p);
    }
    let from_path = path_var
        .into_iter()
        .flat_map(|v| v.split(':'))
        .filter(|d| !d.is_empty());
    from_path
        .map(|d| Path::new(d).join("kiro-cli"))
        .chain(std::iter::once(home.join(".local/bin/kiro-cli")))
        .find(|p| executable(p))
}

impl Default for HostConfig {
    fn default() -> Self {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let program = resolve_kiro_cli(
            std::env::var("AGENTDOCK_KIRO_CLI").ok().as_deref(),
            std::env::var("PATH").ok().as_deref(),
            &home,
        )
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "kiro-cli".into());
        Self {
            program,
            args: vec!["acp".into()],
            max_sessions: 4,
            init_timeout: Duration::from_secs(30),
            // S0: load took 5-9s, terminal resume once needed ~35s on a cold start.
            load_timeout: Duration::from_secs(60),
            prompt_timeout: Duration::from_secs(30 * 60),
            sessions_dir: discovery::kiro_sessions_dir(),
            audit_path: Some(home.join(".local/share/agentdock/audit.jsonl")),
            allowed_root: home,
        }
    }
}

struct Attached {
    session_id: OnceLock<String>,
    cwd: String,
    client: Arc<AcpClient>,
    prompt_running: AtomicBool,
    replaying: AtomicBool,
    closing: AtomicBool,
    /// toolCallId -> {title, kind, toolName, rawInput} from `tool_call` updates; the
    /// permission request itself only carries the title (S0 T1).
    tool_calls: Mutex<HashMap<String, Value>>,
    /// tmux pane the session was taken over from (for hand-back on detach).
    origin_pane: Mutex<Option<discovery::PaneRef>>,
}

struct PendingApproval {
    session_id: String,
    rpc_id: Value,
    client: Arc<AcpClient>,
    option_ids: Vec<String>,
    /// The `permission.request` event as sent to clients.
    event: Value,
}

pub struct Host {
    config: HostConfig,
    sessions: Mutex<HashMap<String, Arc<Attached>>>,
    approvals: Mutex<HashMap<String, PendingApproval>>,
    next_approval: AtomicU64,
    /// Serializes attach/new so the session limit cannot be raced.
    op_lock: tokio::sync::Mutex<()>,
    audit_lock: Mutex<()>,
    events: broadcast::Sender<Value>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Host {
    pub fn new(config: HostConfig) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        Arc::new(Self {
            config,
            sessions: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            next_approval: AtomicU64::new(1),
            op_lock: tokio::sync::Mutex::new(()),
            audit_lock: Mutex::new(()),
            events,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    /// The agent executable that will be spawned.
    pub fn program(&self) -> &str {
        &self.config.program
    }

    /// Broadcast an event to every connected client.
    pub fn emit(&self, event: Value) {
        let _ = self.events.send(event); // no subscribers is fine
    }

    fn get(&self, session_id: &str) -> Option<Arc<Attached>> {
        self.sessions.lock().unwrap().get(session_id).cloned()
    }

    pub fn attached_list(&self) -> Vec<Value> {
        let mut list: Vec<Value> = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, a)| {
                json!({
                    "sessionId": id,
                    "cwd": a.cwd,
                    "pid": a.client.pid(),
                    "promptRunning": a.prompt_running.load(Ordering::SeqCst),
                })
            })
            .collect();
        list.sort_by(|a, b| a["sessionId"].as_str().cmp(&b["sessionId"].as_str()));
        list
    }

    fn check_limit(&self) -> Result<(), HostError> {
        let n = self.sessions.lock().unwrap().len();
        if n >= self.config.max_sessions {
            return Err(HostError::new(
                "LIMIT_REACHED",
                format!(
                    "已接管 {n} 个会话，达到上限 {}，请先断开一个",
                    self.config.max_sessions
                ),
            ));
        }
        Ok(())
    }

    fn insert(&self, session_id: &str, att: Arc<Attached>) {
        self.sessions
            .lock()
            .unwrap()
            .insert(session_id.to_string(), att.clone());
        self.emit(json!({
            "type": "session.attached",
            "sessionId": session_id,
            "cwd": att.cwd,
            "pid": att.client.pid(),
        }));
    }

    async fn spawn_attached(self: &Arc<Self>, cwd: &str) -> Result<Arc<Attached>, HostError> {
        let (client, rx) =
            AcpClient::spawn(&self.config.program, &self.config.args, Path::new(cwd)).map_err(
                |e| {
                    let hint = if e.to_string().contains("No such file") {
                        "（找不到 kiro-cli：请设置环境变量 AGENTDOCK_KIRO_CLI=<kiro-cli 绝对路径> 后重启 daemon）"
                    } else {
                        ""
                    };
                    HostError::new("SPAWN_FAILED", format!("{e}{hint}"))
                },
            )?;
        let att = Arc::new(Attached {
            session_id: OnceLock::new(),
            cwd: cwd.to_string(),
            client,
            prompt_running: AtomicBool::new(false),
            replaying: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            tool_calls: Mutex::new(HashMap::new()),
            origin_pane: Mutex::new(None),
        });
        tokio::spawn(pump(Arc::downgrade(self), att.clone(), rx));
        let init = att
            .client
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": {"readTextFile": false, "writeTextFile": false},
                        "terminal": false
                    }
                }),
                self.config.init_timeout,
            )
            .await;
        if let Err(e) = init {
            close(&att).await;
            return Err(map_acp_error(e, "initialize"));
        }
        Ok(att)
    }

    /// Take over an existing session (it must not be open in another process).
    pub async fn attach(self: &Arc<Self>, session_id: &str) -> Result<Value, HostError> {
        self.attach_opts(session_id, false).await
    }

    /// `takeover`: if the session is open in a tmux pane, quit it there first
    /// (Ctrl+U, `/quit`), then load it. Refused while the terminal is mid-turn.
    pub async fn attach_opts(
        self: &Arc<Self>,
        session_id: &str,
        takeover: bool,
    ) -> Result<Value, HostError> {
        if !discovery::is_valid_session_id(session_id) {
            return Err(HostError::new("BAD_REQUEST", "invalid sessionId"));
        }
        let _op = self.op_lock.lock().await;
        if let Some(a) = self.get(session_id) {
            return Ok(json!({"sessionId": session_id, "cwd": a.cwd, "alreadyAttached": true}));
        }
        self.check_limit()?;
        let cwd = read_meta_cwd(&self.config.sessions_dir, session_id).ok_or_else(|| {
            HostError::new(
                "NOT_FOUND",
                format!("session not found or has no cwd: {session_id}"),
            )
        })?;

        let origin =
            if takeover && discovery::is_session_locked(&self.config.sessions_dir, session_id) {
                Some(self.quit_in_terminal(session_id).await?)
            } else {
                None
            };

        let mut last_err = None;
        for attempt in 1..=2 {
            match self.try_load(session_id, &cwd).await {
                Ok(att) => {
                    *att.origin_pane.lock().unwrap() = origin.clone();
                    self.insert(session_id, att);
                    return Ok(json!({
                        "sessionId": session_id,
                        "cwd": cwd,
                        "alreadyAttached": false,
                        "attempts": attempt,
                        "tookOverFrom": origin.as_ref().map(|p| p.label.clone()),
                    }));
                }
                Err(e) if e.code == "LOAD_TIMEOUT" => {
                    tracing::warn!(session_id, attempt, "session/load timed out");
                    last_err = Some(e);
                }
                Err(e) => {
                    last_err = Some(e);
                    break;
                }
            }
        }
        let mut err = last_err.expect("loop ran");
        // We already quit it in the terminal: give it back rather than lose it.
        if let Some(pane) = &origin {
            match resume_in_pane(pane, session_id, &cwd).await {
                Ok(()) => err
                    .message
                    .push_str(&format!("；已在 {} 恢复终端会话", pane.label)),
                Err(e) => err.message.push_str(&format!(
                    "；恢复终端会话失败（{e}），请手动执行 kiro-cli --resume-id {session_id}"
                )),
            }
        }
        Err(err)
    }

    async fn quit_in_terminal(&self, session_id: &str) -> Result<discovery::PaneRef, HostError> {
        let dir = self.config.sessions_dir.clone();
        let sid = session_id.to_string();
        let found = tokio::task::spawn_blocking(move || {
            let pane = discovery::pane_of_session(&dir, &sid);
            let state = discovery::turn_state(&dir.join(format!("{sid}.jsonl")));
            let cmd = pane.as_ref().and_then(|(_, p)| p.current_command());
            (pane, state, cmd)
        })
        .await
        .map_err(|e| HostError::new("INTERNAL", e.to_string()))?;
        let (Some((_, pane)), state, cmd) = found else {
            return Err(HostError::new(
                "SESSION_BUSY",
                "会话正在其他进程中打开（不在 tmux 中，可能是 IDE），无法自动退出，请手动关闭后再接管",
            ));
        };
        if cmd.as_deref() != Some("kiro-cli") {
            return Err(HostError::new(
                "TAKEOVER_FAILED",
                format!(
                    "窗格 {} 的前台程序是 {:?}，不是 kiro-cli，未发送任何按键",
                    pane.label, cmd
                ),
            ));
        }
        if state == discovery::TurnState::Busy {
            return Err(HostError::new(
                "TERMINAL_BUSY",
                format!(
                    "会话在终端 {} 中正在回复或等待审批，为避免打断，未自动退出。请在终端完成后再接管",
                    pane.label
                ),
            ));
        }
        let p = pane.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            p.send_key("C-u")?; // clear any unsent draft
            std::thread::sleep(Duration::from_millis(200));
            p.send_text("/quit")?;
            p.send_key("Enter")
        })
        .await
        .map_err(|e| HostError::new("INTERNAL", e.to_string()))?
        .map_err(|e| HostError::new("TAKEOVER_FAILED", format!("发送按键失败：{e}")))?;
        tracing::info!(session_id, pane = %pane.label, "sent /quit for takeover");
        if !self
            .wait_unlocked(session_id, Duration::from_secs(15))
            .await
        {
            return Err(HostError::new(
                "TAKEOVER_FAILED",
                format!(
                    "已在 {} 发送 /quit，但 15 秒内会话没有退出，请查看终端",
                    pane.label
                ),
            ));
        }
        Ok(pane)
    }

    async fn wait_unlocked(&self, session_id: &str, max: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + max;
        loop {
            if !discovery::is_session_locked(&self.config.sessions_dir, session_id) {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    async fn try_load(
        self: &Arc<Self>,
        session_id: &str,
        cwd: &str,
    ) -> Result<Arc<Attached>, HostError> {
        let att = self.spawn_attached(cwd).await?;
        let _ = att.session_id.set(session_id.to_string());
        att.replaying.store(true, Ordering::SeqCst);
        let result = att
            .client
            .request(
                "session/load",
                json!({"sessionId": session_id, "cwd": cwd, "mcpServers": []}),
                self.config.load_timeout,
            )
            .await;
        // The pump handles frames in order, so every replayed update was emitted before
        // the load response resolved this request.
        att.replaying.store(false, Ordering::SeqCst);
        match result {
            Ok(_) => Ok(att),
            Err(e) => {
                close(&att).await;
                if is_busy(&e) {
                    Err(HostError::new("SESSION_BUSY", self.busy_hint(session_id)))
                } else if e == AcpError::Timeout {
                    Err(HostError::new(
                        "LOAD_TIMEOUT",
                        format!(
                            "加载会话超时（{}s），已重试仍失败",
                            self.config.load_timeout.as_secs()
                        ),
                    ))
                } else {
                    Err(map_acp_error(e, "session/load"))
                }
            }
        }
    }

    fn busy_hint(&self, session_id: &str) -> String {
        let panes = discovery::tmux_pane_map();
        let pane = discovery::list_active_in(&self.config.sessions_dir, Path::new("/proc"), &panes)
            .ok()
            .and_then(|list| list.into_iter().find(|s| s.session_id == session_id))
            .and_then(|s| s.tmux_pane);
        match pane {
            Some(p) => format!("会话正在终端中打开，请先在 tmux 窗口 {p} 里退出（/quit）后再接管"),
            None => "会话正在其他进程中打开（不在 tmux 中，可能是 IDE），请先关闭后再接管".into(),
        }
    }

    /// Start a brand-new session in `cwd`.
    pub async fn new_session(self: &Arc<Self>, cwd: &str) -> Result<Value, HostError> {
        let cwd = self.validate_cwd(cwd)?;
        let _op = self.op_lock.lock().await;
        self.check_limit()?;
        let att = self.spawn_attached(&cwd).await?;
        let result = att
            .client
            .request(
                "session/new",
                json!({"cwd": cwd, "mcpServers": []}),
                self.config.load_timeout,
            )
            .await;
        let session_id = match result {
            Ok(v) => v
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|s| discovery::is_valid_session_id(s))
                .map(str::to_string),
            Err(e) => {
                close(&att).await;
                return Err(map_acp_error(e, "session/new"));
            }
        };
        let Some(session_id) = session_id else {
            close(&att).await;
            return Err(HostError::new(
                "ACP_ERROR",
                "session/new returned no valid sessionId",
            ));
        };
        if self.get(&session_id).is_some() {
            close(&att).await;
            return Err(HostError::new(
                "ACP_ERROR",
                format!("session/new returned an id that is already attached: {session_id}"),
            ));
        }
        let _ = att.session_id.set(session_id.clone());
        self.insert(&session_id, att);
        Ok(json!({"sessionId": session_id, "cwd": cwd}))
    }

    fn validate_cwd(&self, cwd: &str) -> Result<String, HostError> {
        let bad = |m: &str| HostError::new("BAD_REQUEST", m.to_string());
        let p = Path::new(cwd);
        if !p.is_absolute() {
            return Err(bad("cwd must be an absolute path"));
        }
        let canon = p.canonicalize().map_err(|_| bad("cwd does not exist"))?;
        if !canon.is_dir() {
            return Err(bad("cwd is not a directory"));
        }
        let root = self
            .config
            .allowed_root
            .canonicalize()
            .map_err(|_| bad("allowed root unavailable"))?;
        if !canon.starts_with(&root) {
            return Err(bad(&format!("cwd must be under {}", root.display())));
        }
        canon
            .to_str()
            .map(str::to_string)
            .ok_or_else(|| bad("cwd is not valid UTF-8"))
    }

    /// Start one prompt turn. Returns once the request is written; output arrives as events.
    pub async fn prompt(
        self: &Arc<Self>,
        session_id: &str,
        text: &str,
    ) -> Result<Value, HostError> {
        if text.trim().is_empty() {
            return Err(HostError::new("BAD_REQUEST", "text is empty"));
        }
        if text.len() > MAX_PROMPT_BYTES {
            return Err(HostError::new(
                "BAD_REQUEST",
                format!("text exceeds {MAX_PROMPT_BYTES} bytes"),
            ));
        }
        let att = self.get(session_id).ok_or_else(not_attached)?;
        if att.prompt_running.swap(true, Ordering::SeqCst) {
            return Err(HostError::new(
                "PROMPT_RUNNING",
                "上一轮对话还没结束，可以先取消（prompt.cancel）",
            ));
        }
        self.emit(json!({"type": "prompt.started", "sessionId": session_id, "text": text}));
        let pending = match att
            .client
            .start_request(
                "session/prompt",
                json!({"sessionId": session_id, "prompt": [{"type": "text", "text": text}]}),
            )
            .await
        {
            Ok(p) => p,
            Err(e) => {
                att.prompt_running.store(false, Ordering::SeqCst);
                return Err(map_acp_error(e, "session/prompt"));
            }
        };
        let host = self.clone();
        let sid = session_id.to_string();
        tokio::spawn(async move {
            let result = att
                .client
                .finish_request(pending, host.config.prompt_timeout)
                .await;
            att.prompt_running.store(false, Ordering::SeqCst);
            host.emit(match result {
                Ok(v) => json!({"type": "prompt.done", "sessionId": sid, "stopReason": v.get("stopReason")}),
                Err(e) => json!({"type": "prompt.done", "sessionId": sid, "stopReason": null, "error": e.to_string()}),
            });
        });
        Ok(json!({"sessionId": session_id, "accepted": true}))
    }

    pub async fn cancel(&self, session_id: &str) -> Result<Value, HostError> {
        let att = self.get(session_id).ok_or_else(not_attached)?;
        att.client
            .notify("session/cancel", json!({"sessionId": session_id}))
            .await
            .map_err(|e| map_acp_error(e, "session/cancel"))?;
        // ACP: after session/cancel the client answers pending permission requests
        // with the `cancelled` outcome.
        for pa in self.take_approvals(session_id) {
            let _ = pa
                .client
                .respond(
                    pa.rpc_id.clone(),
                    json!({"outcome": {"outcome": "cancelled"}}),
                )
                .await;
            self.resolved(&pa, None, "cancelled", "prompt_cancelled");
        }
        Ok(json!({"sessionId": session_id, "cancelRequested": true}))
    }

    pub async fn detach(&self, session_id: &str) -> Result<Value, HostError> {
        self.detach_opts(session_id, false).await
    }

    /// `handback`: after closing, resume the session in the pane it was taken from.
    pub async fn detach_opts(&self, session_id: &str, handback: bool) -> Result<Value, HostError> {
        let att = self
            .sessions
            .lock()
            .unwrap()
            .remove(session_id)
            .ok_or_else(not_attached)?;
        close(&att).await;
        self.drop_approvals(session_id, "detached");
        self.emit(json!({"type": "session.closed", "sessionId": session_id, "reason": "detached"}));
        let origin = att.origin_pane.lock().unwrap().clone();
        let mut out = json!({"sessionId": session_id, "detached": true, "handedBack": false});
        if !handback {
            return Ok(out);
        }
        let Some(pane) = origin else {
            out["handbackError"] = json!("该会话不是从终端接管的，没有原窗格");
            return Ok(out);
        };
        if !self
            .wait_unlocked(session_id, Duration::from_secs(10))
            .await
        {
            out["handbackError"] = json!("会话锁未释放，未交还");
            return Ok(out);
        }
        match resume_in_pane(&pane, session_id, &att.cwd).await {
            Ok(()) => {
                out["handedBack"] = json!(true);
                out["pane"] = json!(pane.label);
            }
            Err(e) => out["handbackError"] = json!(e),
        }
        Ok(out)
    }

    /// Working directories of attached sessions (rename protection).
    pub fn attached_cwds(&self) -> Vec<String> {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .map(|a| a.cwd.clone())
            .collect()
    }

    pub async fn shutdown_all(&self) {
        let all: Vec<_> = self.sessions.lock().unwrap().drain().collect();
        for (id, att) in all {
            close(&att).await;
            self.drop_approvals(&id, "shutdown");
            tracing::info!(session_id = %id, "closed on shutdown");
        }
    }

    // ---------------------------------------------------------------- approvals

    /// Pending approvals, oldest first (for clients that connect later).
    pub fn pending_approvals(&self) -> Vec<Value> {
        let mut list: Vec<Value> = self
            .approvals
            .lock()
            .unwrap()
            .values()
            .map(|p| p.event.clone())
            .collect();
        list.sort_by_key(|e| e["createdAtMs"].as_u64());
        list
    }

    fn register_approval(&self, att: &Attached, rpc_id: Value, params: &Value) {
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .or_else(|| att.session_id.get().map(String::as_str))
            .unwrap_or_default()
            .to_string();
        let tool_call_id = params
            .pointer("/toolCall/toolCallId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let known = att
            .tool_calls
            .lock()
            .unwrap()
            .get(tool_call_id)
            .cloned()
            .unwrap_or(Value::Null);
        let raw_input = params
            .pointer("/toolCall/rawInput")
            .filter(|v| !v.is_null())
            .or_else(|| known.get("rawInput").filter(|v| !v.is_null()))
            .cloned();
        let options = params
            .get("options")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let option_ids = options
            .iter()
            .filter_map(|o| {
                o.get("optionId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        let approval_id = format!("ap-{}", self.next_approval.fetch_add(1, Ordering::Relaxed));
        let event = json!({
            "type": "permission.request",
            "approvalId": approval_id,
            "sessionId": session_id,
            "toolCallId": tool_call_id,
            "title": params.pointer("/toolCall/title").or_else(|| known.get("title")),
            "kind": known.get("kind"),
            "toolName": known.get("toolName"),
            "rawInput": raw_input,
            // The card must warn when parameters could not be matched.
            "inputUnknown": raw_input.is_none(),
            "options": options,
            "trustOptions": params.pointer("/_meta/trustOptions"),
            "createdAtMs": now_ms(),
        });
        self.audit(json!({
            "event": "requested",
            "approvalId": approval_id,
            "sessionId": session_id,
            "title": event["title"],
            "toolName": event["toolName"],
            "rawInput": event["rawInput"],
        }));
        self.approvals.lock().unwrap().insert(
            approval_id,
            PendingApproval {
                session_id,
                rpc_id,
                client: att.client.clone(),
                option_ids,
                event: event.clone(),
            },
        );
        self.emit(event);
    }

    /// Answer a pending approval. The first reply wins; later ones get APPROVAL_NOT_FOUND.
    pub async fn reply_permission(
        &self,
        approval_id: &str,
        option_id: &str,
    ) -> Result<Value, HostError> {
        let pa = {
            let mut map = self.approvals.lock().unwrap();
            let Some(pa) = map.get(approval_id) else {
                return Err(HostError::new(
                    "APPROVAL_NOT_FOUND",
                    "审批不存在或已被处理（可能已在其他设备上处理）",
                ));
            };
            if !pa.option_ids.iter().any(|o| o == option_id) {
                return Err(HostError::new(
                    "BAD_REQUEST",
                    format!("optionId must be one of {:?}", pa.option_ids),
                ));
            }
            map.remove(approval_id).expect("checked above")
        };
        let answer = json!({"outcome": {"outcome": "selected", "optionId": option_id}});
        if let Err(e) = pa.client.respond(pa.rpc_id.clone(), answer).await {
            self.resolved(&pa, Some(option_id), "failed", "agent_unreachable");
            return Err(map_acp_error(e, "permission reply"));
        }
        self.resolved(&pa, Some(option_id), "selected", "user");
        Ok(json!({"approvalId": approval_id, "optionId": option_id, "sessionId": pa.session_id}))
    }

    fn take_approvals(&self, session_id: &str) -> Vec<PendingApproval> {
        let mut map = self.approvals.lock().unwrap();
        let ids: Vec<String> = map
            .iter()
            .filter(|(_, p)| p.session_id == session_id)
            .map(|(k, _)| k.clone())
            .collect();
        ids.into_iter().filter_map(|k| map.remove(&k)).collect()
    }

    /// The agent is gone: nothing to answer, just tell clients the cards are void.
    fn drop_approvals(&self, session_id: &str, reason: &str) {
        for pa in self.take_approvals(session_id) {
            self.resolved(&pa, None, "cancelled", reason);
        }
    }

    fn resolved(&self, pa: &PendingApproval, option_id: Option<&str>, outcome: &str, reason: &str) {
        let approval_id = &pa.event["approvalId"];
        self.audit(json!({
            "event": "resolved",
            "approvalId": approval_id,
            "sessionId": pa.session_id,
            "title": pa.event["title"],
            "optionId": option_id,
            "outcome": outcome,
            "reason": reason,
        }));
        self.emit(json!({
            "type": "permission.resolved",
            "approvalId": approval_id,
            "sessionId": pa.session_id,
            "optionId": option_id,
            "outcome": outcome,
            "reason": reason,
        }));
    }

    fn audit(&self, mut record: Value) {
        let Some(path) = &self.config.audit_path else {
            return;
        };
        record["tsMs"] = json!(now_ms());
        let _guard = self.audit_lock.lock().unwrap();
        if let Err(e) = append_private(path, &record) {
            tracing::error!("audit log write failed ({}): {e}", path.display());
        }
    }
}

/// Append one JSON line; the file is created with mode 0600 (it contains commands).
fn append_private(path: &Path, record: &Value) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    writeln!(f, "{record}")
}

fn not_attached() -> HostError {
    HostError::new("NOT_ATTACHED", "session is not attached")
}

const SHELLS: [&str; 5] = ["bash", "zsh", "sh", "fish", "dash"];

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Type `cd <cwd> && kiro-cli --resume-id <id>` into the pane, only if it sits at a shell.
async fn resume_in_pane(
    pane: &discovery::PaneRef,
    session_id: &str,
    cwd: &str,
) -> Result<(), String> {
    let pane = pane.clone();
    let cmd = format!(
        "cd {} && kiro-cli --resume-id {session_id}",
        shell_quote(cwd)
    );
    tokio::task::spawn_blocking(move || {
        let fg = pane.current_command();
        match fg.as_deref() {
            Some(c) if SHELLS.contains(&c) => {}
            other => {
                return Err(format!(
                    "原窗格 {} 当前不在 shell（{other:?}），未交还",
                    pane.label
                ));
            }
        }
        pane.send_key("C-u")
            .and_then(|_| pane.send_text(&cmd))
            .and_then(|_| pane.send_key("Enter"))
            .map_err(|e| format!("发送按键失败：{e}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

async fn close(att: &Attached) {
    att.closing.store(true, Ordering::SeqCst);
    att.client.shutdown().await;
}

fn is_busy(e: &AcpError) -> bool {
    match e {
        AcpError::Rpc { message, data, .. } => {
            message.contains(BUSY_MARKER)
                || data
                    .as_ref()
                    .is_some_and(|d| d.to_string().contains(BUSY_MARKER))
        }
        _ => false,
    }
}

fn map_acp_error(e: AcpError, during: &str) -> HostError {
    let code = match e {
        AcpError::Timeout => "ACP_TIMEOUT",
        AcpError::Closed => "AGENT_EXITED",
        _ => "ACP_ERROR",
    };
    HostError::new(code, format!("{during}: {e}"))
}

fn read_meta_cwd(sessions_dir: &Path, session_id: &str) -> Option<String> {
    let raw = std::fs::read_to_string(sessions_dir.join(format!("{session_id}.json"))).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    v.get("cwd").and_then(Value::as_str).map(str::to_string)
}

fn content_text(content: &Value) -> String {
    // ContentBlock {type:"text", text} or ToolCallContent [{type:"content", content:{...}}]
    if let Some(t) = content.get("text").and_then(Value::as_str) {
        return t.to_string();
    }
    content
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.pointer("/content/text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// Remember tool-call parameters so permission cards can show them.
fn track_tool_call(att: &Attached, params: &Value) {
    let Some(u) = params.get("update") else {
        return;
    };
    let kind = u.get("sessionUpdate").and_then(Value::as_str);
    if !matches!(kind, Some("tool_call" | "tool_call_update")) {
        return;
    }
    let Some(id) = u.get("toolCallId").and_then(Value::as_str) else {
        return;
    };
    let mut map = att.tool_calls.lock().unwrap();
    if matches!(
        u.get("status").and_then(Value::as_str),
        Some("completed" | "failed")
    ) {
        map.remove(id);
        return;
    }
    let entry = map.entry(id.to_string()).or_insert_with(|| json!({}));
    for key in ["title", "kind", "rawInput"] {
        if let Some(v) = u.get(key).filter(|v| !v.is_null()) {
            entry[key] = v.clone();
        }
    }
    if let Some(name) = u.pointer("/_meta/kiro/toolName") {
        entry["toolName"] = name.clone();
    }
}

/// Translate an ACP `session/update` into a client event.
pub fn update_event(params: &Value, replay: bool) -> Value {
    let sid = params.get("sessionId").cloned().unwrap_or(Value::Null);
    let u = params.get("update").cloned().unwrap_or(Value::Null);
    let kind = u.get("sessionUpdate").and_then(Value::as_str).unwrap_or("");
    let role = match kind {
        "agent_message_chunk" => Some("assistant"),
        "agent_thought_chunk" => Some("thought"),
        "user_message_chunk" => Some("user"),
        _ => None,
    };
    if let Some(role) = role {
        let content = u.get("content").cloned().unwrap_or(Value::Null);
        return json!({
            "type": "message.chunk",
            "sessionId": sid,
            "replay": replay,
            "role": role,
            "contentType": content.get("type"),
            "text": content_text(&content),
        });
    }
    if kind == "tool_call" || kind == "tool_call_update" {
        let output = u.get("content").map(content_text).unwrap_or_default();
        let (output, truncated) = truncate(&output, MAX_TOOL_OUTPUT_CHARS);
        return json!({
            "type": "tool.call",
            "sessionId": sid,
            "replay": replay,
            "phase": if kind == "tool_call" { "start" } else { "update" },
            "toolCallId": u.get("toolCallId"),
            "title": u.get("title"),
            "kind": u.get("kind"),
            "status": u.get("status"),
            "rawInput": u.get("rawInput"),
            "output": output,
            "outputTruncated": truncated,
        });
    }
    json!({"type": "session.update", "sessionId": sid, "replay": replay, "update": u})
}

async fn pump(host: Weak<Host>, att: Arc<Attached>, mut rx: mpsc::UnboundedReceiver<Frame>) {
    while let Some(frame) = rx.recv().await {
        let Some(incoming) = att.client.resolve(frame) else {
            continue;
        };
        let Some(host) = host.upgrade() else {
            break;
        };
        match incoming {
            Incoming::Notification { method, params } if method == "session/update" => {
                let replay = att.replaying.load(Ordering::SeqCst);
                if !replay {
                    track_tool_call(&att, &params);
                }
                host.emit(update_event(&params, replay));
            }
            Incoming::Notification { method, .. } => {
                tracing::debug!(%method, "ignored agent notification");
            }
            Incoming::Request { id, method, params } if method == "session/request_permission" => {
                host.register_approval(&att, id, &params);
            }
            Incoming::Request { id, method, .. } => {
                let _ = att
                    .client
                    .respond_error(id, -32601, &format!("method not supported: {method}"))
                    .await;
            }
        }
    }
    att.client.fail_all_pending();
    let (Some(host), Some(sid)) = (host.upgrade(), att.session_id.get()) else {
        return;
    };
    let removed = {
        let mut map = host.sessions.lock().unwrap();
        if map.get(sid).is_some_and(|a| Arc::ptr_eq(a, &att)) {
            map.remove(sid);
            true
        } else {
            false
        }
    };
    if removed && !att.closing.load(Ordering::SeqCst) {
        host.drop_approvals(sid, "agent_exited");
        host.emit(json!({"type": "session.closed", "sessionId": sid, "reason": "agent_exited"}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_detection_uses_data_field() {
        let e = AcpError::Rpc {
            code: -32603,
            message: "Internal error".into(),
            data: Some(json!(
                "Failed to start session: Session is active in another process (PID 1)"
            )),
        };
        assert!(is_busy(&e));
        assert!(!is_busy(&AcpError::Timeout));
    }

    #[test]
    fn update_translation() {
        let chunk = update_event(
            &json!({"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}),
            false,
        );
        assert_eq!(
            (
                chunk["type"].as_str(),
                chunk["role"].as_str(),
                chunk["text"].as_str()
            ),
            (Some("message.chunk"), Some("assistant"), Some("hi"))
        );

        // tool_call_update as observed in S0 T1 (denied)
        let tool = update_event(
            &json!({"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"t","status":"failed",
                "content":[{"type":"content","content":{"type":"text","text":"User denied tool execution"}}],
                "rawInput":{"command":"echo hello"}}}),
            true,
        );
        assert_eq!(tool["type"], "tool.call");
        assert_eq!(tool["phase"], "update");
        assert_eq!(tool["status"], "failed");
        assert_eq!(tool["output"], "User denied tool execution");
        assert_eq!(tool["rawInput"]["command"], "echo hello");
        assert_eq!(tool["replay"], true);

        let other = update_event(
            &json!({"sessionId":"s","update":{"sessionUpdate":"plan","entries":[]}}),
            false,
        );
        assert_eq!(other["type"], "session.update");
    }

    #[test]
    fn kiro_cli_resolution_order() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("agentdock-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        let local = home.join(".local/bin");
        let other = base.join("bin");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let mk = |p: &Path, mode: u32| {
            std::fs::write(p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        mk(&local.join("kiro-cli"), 0o755);
        // systemd-like PATH without ~/.local/bin falls back to the install dir
        assert_eq!(
            resolve_kiro_cli(None, Some("/usr/bin:/bin"), &home),
            Some(local.join("kiro-cli"))
        );
        assert_eq!(
            resolve_kiro_cli(None, None, &home),
            Some(local.join("kiro-cli"))
        );
        // PATH wins over the fallback; non-executable entries are skipped
        mk(&other.join("kiro-cli"), 0o644);
        let path = other.to_str().unwrap().to_string();
        assert_eq!(
            resolve_kiro_cli(None, Some(&path), &home),
            Some(local.join("kiro-cli"))
        );
        mk(&other.join("kiro-cli"), 0o755);
        assert_eq!(
            resolve_kiro_cli(None, Some(&path), &home),
            Some(other.join("kiro-cli"))
        );
        // explicit override, and an invalid override is not silently replaced
        let o = other.join("kiro-cli").to_string_lossy().to_string();
        assert_eq!(
            resolve_kiro_cli(Some(&o), None, &home),
            Some(other.join("kiro-cli"))
        );
        assert_eq!(
            resolve_kiro_cli(Some("/nope/kiro-cli"), Some(&path), &home),
            None
        );
        std::fs::remove_file(local.join("kiro-cli")).unwrap();
        assert_eq!(resolve_kiro_cli(None, Some("/usr/bin"), &home), None);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(shell_quote("/home/mi/a b"), "'/home/mi/a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$(rm -rf ~)"), "'$(rm -rf ~)'");
    }

    #[test]
    fn audit_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let p =
            std::env::temp_dir().join(format!("agentdock-audit-{}/a.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&p);
        append_private(&p, &json!({"a": 1})).unwrap();
        append_private(&p, &json!({"a": 2})).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read_to_string(&p).unwrap().lines().count(), 2);
        std::fs::remove_dir_all(p.parent().unwrap()).unwrap();
    }
}
