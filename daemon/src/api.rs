//! WebSocket API.
//!
//! Requests carry an `id` that is echoed in the reply:
//!   {"id": 1, "type": "sessions.list"}
//!   {"id": 2, "type": "session.view", "sessionId": "...", "limit": 200}
//!   {"id": 3, "type": "attached.list"}
//!   {"id": 4, "type": "session.attach", "sessionId": "..."}
//!   {"id": 5, "type": "session.new", "cwd": "/home/..."}
//!   {"id": 6, "type": "prompt.send", "sessionId": "...", "text": "..."}
//!   {"id": 7, "type": "prompt.cancel", "sessionId": "..."}
//!   {"id": 8, "type": "session.detach", "sessionId": "..."}
//!   {"id": 9, "type": "permission.list"}
//!   {"id": 10, "type": "permission.reply", "approvalId": "ap-1", "optionId": "allow_once"}
//!   {"id": 11, "type": "todo.list", "status": "open|done|all"}
//!   {"id": 12, "type": "todo.add", "title": "...", "priority": "high", "dueDate": "2026-12-01"}
//!   {"id": 13, "type": "todo.update", "todoId": 1, "status": "done", "priority": null}
//!   {"id": 14, "type": "todo.delete", "todoId": 1}
//! Replies: {"id": <same>, "type": "<request type>.result", ...}
//!          {"id": <same>, "type": "error", "code": "...", "message": "..."}
//! Events (no id) are pushed to every connection: message.chunk, tool.call,
//! permission.request, permission.resolved, prompt.started, prompt.done,
//! session.attached, session.closed, session.update, todo.changed, events.lagged.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};

use crate::acp_host::{Host, HostError};
use crate::todo::{StoreError, TodoPatch, TodoStore};
use crate::{discovery, session_log};

pub const DEFAULT_VIEW_LIMIT: usize = 200;
pub const MAX_VIEW_LIMIT: usize = 5000;

#[derive(Clone)]
pub struct AppState {
    pub host: Arc<Host>,
    pub todos: Arc<TodoStore>,
}

/// Absent -> None, null -> Some(None), value -> Some(Some(v)).
fn nullable<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
enum Request {
    #[serde(rename = "sessions.list")]
    SessionsList,
    #[serde(rename = "session.view", rename_all = "camelCase")]
    SessionView {
        session_id: String,
        limit: Option<usize>,
    },
    #[serde(rename = "attached.list")]
    AttachedList,
    #[serde(rename = "session.attach", rename_all = "camelCase")]
    SessionAttach { session_id: String },
    #[serde(rename = "session.new")]
    SessionNew { cwd: String },
    #[serde(rename = "prompt.send", rename_all = "camelCase")]
    PromptSend { session_id: String, text: String },
    #[serde(rename = "prompt.cancel", rename_all = "camelCase")]
    PromptCancel { session_id: String },
    #[serde(rename = "session.detach", rename_all = "camelCase")]
    SessionDetach { session_id: String },
    #[serde(rename = "permission.list")]
    PermissionList,
    #[serde(rename = "permission.reply", rename_all = "camelCase")]
    PermissionReply {
        approval_id: String,
        option_id: String,
    },
    #[serde(rename = "todo.list")]
    TodoList { status: Option<String> },
    #[serde(rename = "todo.add", rename_all = "camelCase")]
    TodoAdd {
        title: String,
        priority: Option<String>,
        due_date: Option<String>,
    },
    #[serde(rename = "todo.update", rename_all = "camelCase")]
    TodoUpdate {
        todo_id: i64,
        title: Option<String>,
        status: Option<String>,
        #[serde(default, deserialize_with = "nullable")]
        priority: Option<Option<String>>,
        #[serde(default, deserialize_with = "nullable")]
        due_date: Option<Option<String>>,
    },
    #[serde(rename = "todo.delete", rename_all = "camelCase")]
    TodoDelete { todo_id: i64 },
}

impl Request {
    fn result_type(&self) -> &'static str {
        match self {
            Request::SessionsList => "sessions.list.result",
            Request::SessionView { .. } => "session.view.result",
            Request::AttachedList => "attached.list.result",
            Request::SessionAttach { .. } => "session.attach.result",
            Request::SessionNew { .. } => "session.new.result",
            Request::PromptSend { .. } => "prompt.send.result",
            Request::PromptCancel { .. } => "prompt.cancel.result",
            Request::SessionDetach { .. } => "session.detach.result",
            Request::PermissionList => "permission.list.result",
            Request::PermissionReply { .. } => "permission.reply.result",
            Request::TodoList { .. } => "todo.list.result",
            Request::TodoAdd { .. } => "todo.add.result",
            Request::TodoUpdate { .. } => "todo.update.result",
            Request::TodoDelete { .. } => "todo.delete.result",
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/ws", get(ws_upgrade))
        .with_state(state)
}

async fn ws_upgrade(State(state): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sink, mut stream) = socket.split();
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<Value>();
    let mut events = state.host.subscribe();

    let writer = tokio::spawn(async move {
        loop {
            // biased: an event emitted before a reply is always delivered first
            // (e.g. history replay arrives before the session.attach result).
            let out = tokio::select! {
                biased;
                ev = events.recv() => match ev {
                    Ok(v) => v,
                    Err(broadcast::error::RecvError::Lagged(n)) => json!({"type": "events.lagged", "skipped": n}),
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                reply = reply_rx.recv() => match reply {
                    Some(v) => v,
                    None => break,
                },
            };
            if sink
                .send(Message::Text(out.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = stream.next().await {
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Close(_) => break,
            _ => continue,
        };
        // Handle concurrently: attach can take seconds and must not block other requests.
        let state = state.clone();
        let reply_tx = reply_tx.clone();
        tokio::spawn(async move {
            let _ = reply_tx.send(handle_text(&state, &text).await);
        });
    }
    writer.abort();
}

pub async fn handle_text(state: &AppState, text: &str) -> Value {
    let raw: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return error(Value::Null, "BAD_REQUEST", format!("invalid json: {e}")),
    };
    let id = raw.get("id").cloned().unwrap_or(Value::Null);
    let req: Request = match serde_json::from_value(raw) {
        Ok(r) => r,
        Err(e) => return error(id, "BAD_REQUEST", format!("invalid request: {e}")),
    };
    let result_type = req.result_type();
    match handle_request(state, req).await {
        Ok(mut v) => {
            v["id"] = id;
            v["type"] = json!(result_type);
            v
        }
        Err(e) => error(id, e.code, e.message),
    }
}

fn internal(e: impl std::fmt::Display) -> HostError {
    HostError::new("INTERNAL", e.to_string())
}

fn store_err(e: StoreError) -> HostError {
    HostError::new(e.code, e.message)
}

/// Run a blocking store operation off the async runtime.
async fn with_store<T: Send + 'static>(
    todos: &Arc<TodoStore>,
    f: impl FnOnce(&TodoStore) -> Result<T, StoreError> + Send + 'static,
) -> Result<T, HostError> {
    let todos = todos.clone();
    tokio::task::spawn_blocking(move || f(&todos))
        .await
        .map_err(internal)?
        .map_err(store_err)
}

async fn handle_request(state: &AppState, req: Request) -> Result<Value, HostError> {
    let host = &state.host;
    match req {
        Request::SessionsList => {
            let sessions = tokio::task::spawn_blocking(discovery::list_active)
                .await
                .map_err(internal)?
                .map_err(|e| HostError::new("INTERNAL", format!("discovery failed: {e}")))?;
            Ok(json!({"sessions": sessions}))
        }
        Request::SessionView { session_id, limit } => {
            if !discovery::is_valid_session_id(&session_id) {
                return Err(HostError::new("BAD_REQUEST", "invalid sessionId"));
            }
            let limit = limit.unwrap_or(DEFAULT_VIEW_LIMIT).min(MAX_VIEW_LIMIT);
            let path = discovery::kiro_sessions_dir().join(format!("{session_id}.jsonl"));
            if !path.exists() {
                return Err(HostError::new(
                    "NOT_FOUND",
                    format!("session not found: {session_id}"),
                ));
            }
            let view =
                tokio::task::spawn_blocking(move || session_log::read_session(&path, Some(limit)))
                    .await
                    .map_err(internal)?
                    .map_err(|e| HostError::new("INTERNAL", format!("read failed: {e}")))?;
            Ok(json!({"sessionId": session_id, "view": view}))
        }
        Request::AttachedList => Ok(json!({"sessions": host.attached_list()})),
        Request::SessionAttach { session_id } => host.attach(&session_id).await,
        Request::SessionNew { cwd } => host.new_session(&cwd).await,
        Request::PromptSend { session_id, text } => host.prompt(&session_id, &text).await,
        Request::PromptCancel { session_id } => host.cancel(&session_id).await,
        Request::SessionDetach { session_id } => host.detach(&session_id).await,
        Request::PermissionList => Ok(json!({"approvals": host.pending_approvals()})),
        Request::PermissionReply {
            approval_id,
            option_id,
        } => host.reply_permission(&approval_id, &option_id).await,
        Request::TodoList { status } => {
            let status = status.unwrap_or_else(|| "open".into());
            let todos = with_store(&state.todos, move |s| s.list(&status)).await?;
            Ok(json!({"todos": todos}))
        }
        Request::TodoAdd {
            title,
            priority,
            due_date,
        } => {
            let todo = with_store(&state.todos, move |s| {
                s.add_manual(&title, priority, due_date)
            })
            .await?;
            host.emit(json!({"type": "todo.changed", "action": "added", "todo": todo}));
            Ok(json!({"todo": todo}))
        }
        Request::TodoUpdate {
            todo_id,
            title,
            status,
            priority,
            due_date,
        } => {
            let patch = TodoPatch {
                title,
                status,
                priority,
                due_date,
            };
            let todo = with_store(&state.todos, move |s| s.update(todo_id, patch)).await?;
            host.emit(json!({"type": "todo.changed", "action": "updated", "todo": todo}));
            Ok(json!({"todo": todo}))
        }
        Request::TodoDelete { todo_id } => {
            with_store(&state.todos, move |s| s.delete(todo_id)).await?;
            host.emit(json!({"type": "todo.changed", "action": "deleted", "todoId": todo_id}));
            Ok(json!({"todoId": todo_id, "deleted": true}))
        }
    }
}

fn error(id: Value, code: &str, message: impl Into<String>) -> Value {
    json!({"id": id, "type": "error", "code": code, "message": message.into()})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp_host::HostConfig;

    fn state() -> AppState {
        AppState {
            host: Host::new(HostConfig {
                audit_path: None,
                ..HostConfig::default()
            }),
            todos: Arc::new(TodoStore::open_in_memory().unwrap()),
        }
    }

    #[tokio::test]
    async fn rejects_bad_input_with_id_echo() {
        let st = state();
        let r = handle_text(&st, "{bad").await;
        assert_eq!(
            (r["type"].as_str(), r["code"].as_str()),
            (Some("error"), Some("BAD_REQUEST"))
        );

        let r = handle_text(&st, r#"{"id":7,"type":"nope"}"#).await;
        assert_eq!(
            (r["type"].as_str(), r["id"].as_i64()),
            (Some("error"), Some(7))
        );

        let r = handle_text(
            &st,
            r#"{"id":"a","type":"session.view","sessionId":"../../etc/passwd"}"#,
        )
        .await;
        assert_eq!(r["message"], "invalid sessionId");
        assert_eq!(r["id"], "a");

        let r = handle_text(
            &st,
            r#"{"id":1,"type":"prompt.send","sessionId":"00000000-0000-0000-0000-000000000000","text":"hi"}"#,
        )
        .await;
        assert_eq!(r["code"], "NOT_ATTACHED");

        let r = handle_text(&st, r#"{"id":2,"type":"attached.list"}"#).await;
        assert_eq!(r["type"], "attached.list.result");
        assert_eq!(r["sessions"], json!([]));

        let r = handle_text(&st, r#"{"id":3,"type":"permission.list"}"#).await;
        assert_eq!(r["approvals"], json!([]));
        let r = handle_text(
            &st,
            r#"{"id":4,"type":"permission.reply","approvalId":"ap-1","optionId":"allow_once"}"#,
        )
        .await;
        assert_eq!(r["code"], "APPROVAL_NOT_FOUND");
    }

    #[tokio::test]
    async fn todo_requests_and_events() {
        let st = state();
        let mut events = st.host.subscribe();
        let r = handle_text(
            &st,
            r#"{"id":1,"type":"todo.add","title":"写 S5","priority":"high","dueDate":"2026-12-01"}"#,
        )
        .await;
        assert_eq!(r["type"], "todo.add.result");
        let id = r["todo"]["id"].as_i64().unwrap();
        assert_eq!(r["todo"]["dueDate"], "2026-12-01");
        let ev = events.recv().await.unwrap();
        assert_eq!(
            (ev["type"].as_str(), ev["action"].as_str()),
            (Some("todo.changed"), Some("added"))
        );

        // explicit null clears priority; absent dueDate is kept
        let r = handle_text(
            &st,
            &format!(
                r#"{{"id":2,"type":"todo.update","todoId":{id},"status":"done","priority":null}}"#
            ),
        )
        .await;
        assert_eq!(r["todo"]["status"], "done");
        assert_eq!(r["todo"]["priority"], Value::Null);
        assert_eq!(r["todo"]["dueDate"], "2026-12-01");

        let r = handle_text(&st, r#"{"id":3,"type":"todo.list"}"#).await;
        assert_eq!(r["todos"], json!([]), "default filter is open");
        let r = handle_text(&st, r#"{"id":4,"type":"todo.list","status":"all"}"#).await;
        assert_eq!(r["todos"].as_array().unwrap().len(), 1);

        let r = handle_text(
            &st,
            &format!(r#"{{"id":5,"type":"todo.delete","todoId":{id}}}"#),
        )
        .await;
        assert_eq!(r["deleted"], true);
        let r = handle_text(
            &st,
            &format!(r#"{{"id":6,"type":"todo.delete","todoId":{id}}}"#),
        )
        .await;
        assert_eq!(r["code"], "NOT_FOUND");
        let r = handle_text(&st, r#"{"id":7,"type":"todo.add","title":""}"#).await;
        assert_eq!(r["code"], "BAD_REQUEST");
    }
}
