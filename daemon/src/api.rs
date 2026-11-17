//! WebSocket API (S1: sessions.list, session.view).
//!
//! Request:  {"id": <any>, "type": "sessions.list"}
//!           {"id": <any>, "type": "session.view", "sessionId": "...", "limit": 200}
//! Response: {"id": <same>, "type": "<request type>.result", ...}
//!           {"id": <same>, "type": "error", "message": "..."}

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Value, json};

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
}

pub fn router() -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/ws", get(ws_upgrade))
}

async fn ws_upgrade(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
    while let Some(Ok(msg)) = socket.recv().await {
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Close(_) => break,
            _ => continue,
        };
        let reply = handle_text(&text).await;
        if socket
            .send(Message::Text(reply.to_string().into()))
            .await
            .is_err()
        {
            break;
        }
    }
}

pub async fn handle_text(text: &str) -> Value {
    let raw: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return error(Value::Null, format!("invalid json: {e}")),
    };
    let id = raw.get("id").cloned().unwrap_or(Value::Null);
    let req: Request = match serde_json::from_value(raw) {
        Ok(r) => r,
        Err(e) => return error(id, format!("invalid request: {e}")),
    };
    match handle_request(req).await {
        Ok(mut v) => {
            v["id"] = id;
            v
        }
        Err(e) => error(id, e),
    }
}

async fn handle_request(req: Request) -> Result<Value, String> {
    match req {
        Request::SessionsList => {
            let sessions = tokio::task::spawn_blocking(discovery::list_active)
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("discovery failed: {e}"))?;
            Ok(json!({"type": "sessions.list.result", "sessions": sessions}))
        }
        Request::SessionView { session_id, limit } => {
            if !discovery::is_valid_session_id(&session_id) {
                return Err("invalid sessionId".into());
            }
            let limit = limit.unwrap_or(DEFAULT_VIEW_LIMIT).min(MAX_VIEW_LIMIT);
            let path = discovery::kiro_sessions_dir().join(format!("{session_id}.jsonl"));
            if !path.exists() {
                return Err(format!("session not found: {session_id}"));
            }
            let view =
                tokio::task::spawn_blocking(move || session_log::read_session(&path, Some(limit)))
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| format!("read failed: {e}"))?;
            Ok(json!({"type": "session.view.result", "sessionId": session_id, "view": view}))
        }
    }
}

fn error(id: Value, message: impl Into<String>) -> Value {
    json!({"id": id, "type": "error", "message": message.into()})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_bad_input_with_id_echo() {
        let r = handle_text("{bad").await;
        assert_eq!(r["type"], "error");

        let r = handle_text(r#"{"id":7,"type":"nope"}"#).await;
        assert_eq!(
            (r["type"].as_str(), r["id"].as_i64()),
            (Some("error"), Some(7))
        );

        let r =
            handle_text(r#"{"id":"a","type":"session.view","sessionId":"../../etc/passwd"}"#).await;
        assert_eq!(r["message"], "invalid sessionId");
        assert_eq!(r["id"], "a");
    }
}
