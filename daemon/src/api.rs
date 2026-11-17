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
//! Replies: {"id": <same>, "type": "<request type>.result", ...}
//!          {"id": <same>, "type": "error", "code": "...", "message": "..."}
//! Events (no id) are pushed to every connection: message.chunk, tool.call,
//! permission.autoRejected, prompt.started, prompt.done, session.attached,
//! session.closed, session.update, events.lagged.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};

use crate::acp_host::{Host, HostError};
use crate::{discovery, session_log};

pub const DEFAULT_VIEW_LIMIT: usize = 200;
pub const MAX_VIEW_LIMIT: usize = 5000;

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
        }
    }
}

pub fn router(host: Arc<Host>) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/ws", get(ws_upgrade))
        .with_state(host)
}

async fn ws_upgrade(State(host): State<Arc<Host>>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, host))
}

async fn handle_socket(socket: WebSocket, host: Arc<Host>) {
    let (mut sink, mut stream) = socket.split();
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<Value>();
    let mut events = host.subscribe();

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
        let host = host.clone();
        let reply_tx = reply_tx.clone();
        tokio::spawn(async move {
            let _ = reply_tx.send(handle_text(&host, &text).await);
        });
    }
    writer.abort();
}

pub async fn handle_text(host: &Arc<Host>, text: &str) -> Value {
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
    match handle_request(host, req).await {
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

async fn handle_request(host: &Arc<Host>, req: Request) -> Result<Value, HostError> {
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
    }
}

fn error(id: Value, code: &str, message: impl Into<String>) -> Value {
    json!({"id": id, "type": "error", "code": code, "message": message.into()})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp_host::HostConfig;

    #[tokio::test]
    async fn rejects_bad_input_with_id_echo() {
        let host = Host::new(HostConfig::default());
        let r = handle_text(&host, "{bad").await;
        assert_eq!(
            (r["type"].as_str(), r["code"].as_str()),
            (Some("error"), Some("BAD_REQUEST"))
        );

        let r = handle_text(&host, r#"{"id":7,"type":"nope"}"#).await;
        assert_eq!(
            (r["type"].as_str(), r["id"].as_i64()),
            (Some("error"), Some(7))
        );

        let r = handle_text(
            &host,
            r#"{"id":"a","type":"session.view","sessionId":"../../etc/passwd"}"#,
        )
        .await;
        assert_eq!(r["message"], "invalid sessionId");
        assert_eq!(r["id"], "a");

        let r = handle_text(
            &host,
            r#"{"id":1,"type":"prompt.send","sessionId":"00000000-0000-0000-0000-000000000000","text":"hi"}"#,
        )
        .await;
        assert_eq!(r["code"], "NOT_ATTACHED");

        let r = handle_text(&host, r#"{"id":2,"type":"attached.list"}"#).await;
        assert_eq!(r["type"], "attached.list.result");
        assert_eq!(r["sessions"], json!([]));
    }
}
