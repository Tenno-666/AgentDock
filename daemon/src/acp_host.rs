//! Attached-session manager: owns up to N `kiro-cli acp` processes and broadcasts
//! their events to every connected client.
//!
//! S2 policy: every permission request is auto-rejected (remote approval is S3).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

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
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            program: "kiro-cli".into(),
            args: vec!["acp".into()],
            max_sessions: 4,
            init_timeout: Duration::from_secs(30),
            // S0: load took 5-9s, terminal resume once needed ~35s on a cold start.
            load_timeout: Duration::from_secs(60),
            prompt_timeout: Duration::from_secs(30 * 60),
            sessions_dir: discovery::kiro_sessions_dir(),
            allowed_root: PathBuf::from(std::env::var_os("HOME").unwrap_or_default()),
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
}

pub struct Host {
    config: HostConfig,
    sessions: Mutex<HashMap<String, Arc<Attached>>>,
    /// Serializes attach/new so the session limit cannot be raced.
    op_lock: tokio::sync::Mutex<()>,
    events: broadcast::Sender<Value>,
}

impl Host {
    pub fn new(config: HostConfig) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        Arc::new(Self {
            config,
            sessions: Mutex::new(HashMap::new()),
            op_lock: tokio::sync::Mutex::new(()),
            events,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    fn emit(&self, event: Value) {
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
            AcpClient::spawn(&self.config.program, &self.config.args, Path::new(cwd))
                .map_err(|e| HostError::new("SPAWN_FAILED", e.to_string()))?;
        let att = Arc::new(Attached {
            session_id: OnceLock::new(),
            cwd: cwd.to_string(),
            client,
            prompt_running: AtomicBool::new(false),
            replaying: AtomicBool::new(false),
            closing: AtomicBool::new(false),
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

        let mut last_err = None;
        for attempt in 1..=2 {
            match self.try_load(session_id, &cwd).await {
                Ok(att) => {
                    self.insert(session_id, att);
                    return Ok(json!({
                        "sessionId": session_id,
                        "cwd": cwd,
                        "alreadyAttached": false,
                        "attempts": attempt,
                    }));
                }
                Err(e) if e.code == "LOAD_TIMEOUT" => {
                    tracing::warn!(session_id, attempt, "session/load timed out");
                    last_err = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err.expect("loop ran"))
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
        Ok(json!({"sessionId": session_id, "cancelRequested": true}))
    }

    pub async fn detach(&self, session_id: &str) -> Result<Value, HostError> {
        let att = self
            .sessions
            .lock()
            .unwrap()
            .remove(session_id)
            .ok_or_else(not_attached)?;
        close(&att).await;
        self.emit(json!({"type": "session.closed", "sessionId": session_id, "reason": "detached"}));
        Ok(json!({"sessionId": session_id, "detached": true}))
    }

    pub async fn shutdown_all(&self) {
        let all: Vec<_> = self.sessions.lock().unwrap().drain().collect();
        for (id, att) in all {
            close(&att).await;
            tracing::info!(session_id = %id, "closed on shutdown");
        }
    }
}

fn not_attached() -> HostError {
    HostError::new("NOT_ATTACHED", "session is not attached")
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

/// Pick the reject option for a permission request (S2 policy).
pub fn auto_reject_outcome(params: &Value) -> Value {
    let option = params
        .get("options")
        .and_then(Value::as_array)
        .and_then(|opts| {
            opts.iter()
                .find(|o| o["kind"] == "reject_once")
                .or_else(|| {
                    opts.iter()
                        .find(|o| o["kind"].as_str().is_some_and(|k| k.starts_with("reject")))
                })
        })
        .and_then(|o| o.get("optionId").and_then(Value::as_str));
    match option {
        Some(id) => json!({"outcome": {"outcome": "selected", "optionId": id}}),
        None => json!({"outcome": {"outcome": "cancelled"}}),
    }
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
                host.emit(update_event(&params, att.replaying.load(Ordering::SeqCst)));
            }
            Incoming::Notification { method, .. } => {
                tracing::debug!(%method, "ignored agent notification");
            }
            Incoming::Request { id, method, params } if method == "session/request_permission" => {
                let outcome = auto_reject_outcome(&params);
                if let Err(e) = att.client.respond(id, outcome.clone()).await {
                    tracing::warn!("failed to answer permission request: {e}");
                }
                host.emit(json!({
                    "type": "permission.autoRejected",
                    "sessionId": params.get("sessionId"),
                    "toolCallId": params.pointer("/toolCall/toolCallId"),
                    "title": params.pointer("/toolCall/title"),
                    "outcome": outcome["outcome"],
                }));
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
        host.emit(json!({"type": "session.closed", "sessionId": sid, "reason": "agent_exited"}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_reject_picks_reject_once() {
        // options exactly as kiro-cli 2.13.0 sent them in S0 T1
        let p = json!({"options":[
            {"optionId":"allow_once","kind":"allow_once"},
            {"optionId":"allow_always","kind":"allow_always"},
            {"optionId":"reject_once","kind":"reject_once"}]});
        assert_eq!(
            auto_reject_outcome(&p)["outcome"]["optionId"],
            "reject_once"
        );
        let only_always = json!({"options":[{"optionId":"r","kind":"reject_always"}]});
        assert_eq!(
            auto_reject_outcome(&only_always)["outcome"]["optionId"],
            "r"
        );
        let none = json!({"options":[{"optionId":"a","kind":"allow_once"}]});
        assert_eq!(
            auto_reject_outcome(&none)["outcome"]["outcome"],
            "cancelled"
        );
        assert_eq!(
            auto_reject_outcome(&json!({}))["outcome"]["outcome"],
            "cancelled"
        );
    }

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
}
