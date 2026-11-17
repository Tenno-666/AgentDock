//! Minimal ACP (Agent Client Protocol) client over a child process's stdio.
//!
//! Framing: one JSON-RPC 2.0 message per line (verified against kiro-cli 2.13.0 in S0).
//! The reader task forwards every frame, responses included, into one ordered channel.
//! The owner drains it and calls [`AcpClient::resolve`], so notifications that arrive
//! before a response (e.g. history replay before the `session/load` result) are always
//! handled before the awaiting request returns.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq)]
pub enum AcpError {
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    Timeout,
    Closed,
    Io(String),
}

impl std::fmt::Display for AcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcpError::Rpc {
                code,
                message,
                data,
            } => match data {
                Some(d) => write!(f, "rpc error {code}: {message} ({d})"),
                None => write!(f, "rpc error {code}: {message}"),
            },
            AcpError::Timeout => write!(f, "request timed out"),
            AcpError::Closed => write!(f, "agent process closed"),
            AcpError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for AcpError {}

/// Messages initiated by the agent.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

#[derive(Debug, PartialEq)]
pub enum Frame {
    Response {
        id: u64,
        result: Result<Value, AcpError>,
    },
    Incoming(Incoming),
    Invalid,
}

pub fn classify(v: Value) -> Frame {
    let method = v.get("method").and_then(Value::as_str).map(str::to_string);
    let id = v.get("id").cloned().filter(|i| !i.is_null());
    let params = || v.get("params").cloned().unwrap_or(Value::Null);
    match (method, id) {
        (Some(method), Some(id)) => Frame::Incoming(Incoming::Request {
            id,
            method,
            params: params(),
        }),
        (Some(method), None) => Frame::Incoming(Incoming::Notification {
            method,
            params: params(),
        }),
        (None, Some(id)) => {
            // We only ever send numeric ids.
            let Some(id) = id.as_u64() else {
                return Frame::Invalid;
            };
            let result = match v.get("error") {
                Some(err) => Err(AcpError::Rpc {
                    code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    data: err.get("data").cloned(),
                }),
                None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
            };
            Frame::Response { id, result }
        }
        (None, None) => Frame::Invalid,
    }
}

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<Value, AcpError>>>>;

/// A request that has been written to the agent and awaits its response.
pub struct PendingRequest {
    id: u64,
    rx: oneshot::Receiver<Result<Value, AcpError>>,
}

pub struct AcpClient {
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    child: tokio::sync::Mutex<Option<Child>>,
    pending: Pending,
    next_id: AtomicU64,
    pid: Option<u32>,
}

impl AcpClient {
    /// Spawn the agent. The returned receiver must be drained, passing each frame to
    /// [`resolve`](Self::resolve); it closes when the agent's stdout reaches EOF.
    pub fn spawn(
        program: &str,
        args: &[String],
        cwd: &Path,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<Frame>), AcpError> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| AcpError::Io(format!("spawn {program}: {e}")))?;
        let stdin = child.stdin.take().ok_or(AcpError::Closed)?;
        let stdout = child.stdout.take().ok_or(AcpError::Closed)?;
        let pid = child.id();

        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let frame = match serde_json::from_str::<Value>(&line) {
                    Ok(v) => classify(v),
                    Err(e) => {
                        tracing::warn!("non-json line from agent: {e}");
                        continue;
                    }
                };
                if tx.send(frame).is_err() {
                    break;
                }
            }
        });

        let client = Arc::new(Self {
            stdin: tokio::sync::Mutex::new(Some(stdin)),
            child: tokio::sync::Mutex::new(Some(child)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            pid,
        });
        Ok((client, rx))
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Complete a pending request for responses; hand back agent-initiated messages.
    pub fn resolve(&self, frame: Frame) -> Option<Incoming> {
        match frame {
            Frame::Response { id, result } => {
                if let Some(waiter) = self.pending.lock().unwrap().remove(&id) {
                    let _ = waiter.send(result);
                }
                None
            }
            Frame::Incoming(m) => Some(m),
            Frame::Invalid => {
                tracing::warn!("invalid acp frame");
                None
            }
        }
    }

    /// Call when the frame channel has closed.
    pub fn fail_all_pending(&self) {
        for (_, waiter) in self.pending.lock().unwrap().drain() {
            let _ = waiter.send(Err(AcpError::Closed));
        }
    }

    async fn write(&self, msg: &Value) -> Result<(), AcpError> {
        let mut guard = self.stdin.lock().await;
        let stdin = guard.as_mut().ok_or(AcpError::Closed)?;
        let mut line = msg.to_string().into_bytes();
        line.push(b'\n');
        stdin
            .write_all(&line)
            .await
            .map_err(|e| AcpError::Io(e.to_string()))?;
        stdin.flush().await.map_err(|e| AcpError::Io(e.to_string()))
    }

    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, AcpError> {
        let pending = self.start_request(method, params).await?;
        self.finish_request(pending, timeout).await
    }

    /// Write a request and return without waiting, so that later writes (e.g. a
    /// `session/cancel` notification) are guaranteed to reach the agent after it.
    pub async fn start_request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<PendingRequest, AcpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = self.write(&msg).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        Ok(PendingRequest { id, rx })
    }

    pub async fn finish_request(
        &self,
        pending: PendingRequest,
        timeout: Duration,
    ) -> Result<Value, AcpError> {
        match tokio::time::timeout(timeout, pending.rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(AcpError::Closed),
            Err(_) => {
                self.pending.lock().unwrap().remove(&pending.id);
                Err(AcpError::Timeout)
            }
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), AcpError> {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    pub async fn respond(&self, id: Value, result: Value) -> Result<(), AcpError> {
        self.write(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await
    }

    pub async fn respond_error(&self, id: Value, code: i64, message: &str) -> Result<(), AcpError> {
        self.write(
            &json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        )
        .await
    }

    /// Close stdin (the agent exits on EOF), then kill if it does not exit in time.
    pub async fn shutdown(&self) {
        self.stdin.lock().await.take();
        if let Some(mut child) = self.child.lock().await.take()
            && tokio::time::timeout(SHUTDOWN_GRACE, child.wait())
                .await
                .is_err()
        {
            let _ = child.kill().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_response_ok_and_error() {
        assert_eq!(
            classify(json!({"jsonrpc":"2.0","id":3,"result":{"x":1}})),
            Frame::Response {
                id: 3,
                result: Ok(json!({"x":1}))
            }
        );
        // Real kiro-cli error observed in S0 (session lock).
        let busy = classify(
            json!({"jsonrpc":"2.0","id":2,"error":{"code":-32603,"message":"Internal error",
            "data":"Failed to start session: Session is active in another process (PID 1419646)"}}),
        );
        match busy {
            Frame::Response {
                id: 2,
                result: Err(AcpError::Rpc { code, data, .. }),
            } => {
                assert_eq!(code, -32603);
                assert!(
                    data.unwrap()
                        .as_str()
                        .unwrap()
                        .contains("active in another process")
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn classify_agent_messages() {
        assert!(matches!(
            classify(json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s"}})),
            Frame::Incoming(Incoming::Notification { .. })
        ));
        assert!(matches!(
            classify(
                json!({"jsonrpc":"2.0","id":0,"method":"session/request_permission","params":{}})
            ),
            Frame::Incoming(Incoming::Request { .. })
        ));
        assert_eq!(
            classify(json!({"id":"not-ours","result":1})),
            Frame::Invalid
        );
        assert_eq!(classify(json!({"foo":1})), Frame::Invalid);
    }
}
