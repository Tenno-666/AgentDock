//! Host integration tests against tests/fake_agent.py (no model calls).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agentdock_daemon::acp_host::{Host, HostConfig};
use serde_json::{Value, json};
use tokio::sync::broadcast;

const OK_ID: &str = "aaaaaaaa-0000-0000-0000-00000000000a";
const BUSY_ID: &str = "aaaaaaaa-0000-0000-0000-00000000000b";
const HANG_ID: &str = "aaaaaaaa-0000-0000-0000-00000000000e";

struct Env {
    host: Arc<Host>,
    events: broadcast::Receiver<Value>,
    dir: PathBuf,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn setup(name: &str, max_sessions: usize) -> Env {
    let dir = std::env::temp_dir().join(format!("agentdock-host-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for id in [OK_ID, BUSY_ID, HANG_ID] {
        std::fs::write(
            dir.join(format!("{id}.json")),
            json!({"cwd": dir.to_str().unwrap()}).to_string(),
        )
        .unwrap();
    }
    let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_agent.py");
    let host = Host::new(HostConfig {
        program: "python3".into(),
        args: vec![fake.into()],
        max_sessions,
        init_timeout: Duration::from_secs(10),
        load_timeout: Duration::from_millis(800),
        prompt_timeout: Duration::from_secs(10),
        sessions_dir: dir.clone(),
        allowed_root: dir.clone(),
    });
    let events = host.subscribe();
    Env { host, events, dir }
}

/// Collect events until `stop` matches (inclusive). Fails after 10s.
async fn collect_until(
    rx: &mut broadcast::Receiver<Value>,
    stop: impl Fn(&Value) -> bool,
) -> Vec<Value> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let ev = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("timed out waiting for event")
            .expect("event channel closed");
        let done = stop(&ev);
        out.push(ev);
        if done {
            return out;
        }
    }
}

fn texts(events: &[Value], role: &str) -> Vec<(String, bool)> {
    events
        .iter()
        .filter(|e| e["type"] == "message.chunk" && e["role"] == role)
        .map(|e| (e["text"].as_str().unwrap().to_string(), e["replay"] == true))
        .collect()
}

#[tokio::test]
async fn attach_replays_history_then_prompt_streams() {
    let mut env = setup("attach", 4);
    let r = env.host.attach(OK_ID).await.unwrap();
    assert_eq!(r["alreadyAttached"], false);

    let evs = collect_until(&mut env.events, |e| e["type"] == "session.attached").await;
    assert_eq!(texts(&evs, "user"), vec![("old question".into(), true)]);
    assert_eq!(texts(&evs, "assistant"), vec![("old answer".into(), true)]);

    // attaching again is idempotent and does not spawn another agent
    let again = env.host.attach(OK_ID).await.unwrap();
    assert_eq!(again["alreadyAttached"], true);
    assert_eq!(env.host.attached_list().len(), 1);

    env.host.prompt(OK_ID, "hi").await.unwrap();
    let evs = collect_until(&mut env.events, |e| e["type"] == "prompt.done").await;
    assert_eq!(evs[0]["type"], "prompt.started");
    assert_eq!(texts(&evs, "assistant"), vec![("echo: hi".into(), false)]);
    assert_eq!(evs.last().unwrap()["stopReason"], "end_turn");

    env.host.detach(OK_ID).await.unwrap();
    let evs = collect_until(&mut env.events, |e| e["type"] == "session.closed").await;
    assert_eq!(evs.last().unwrap()["reason"], "detached");
    assert!(env.host.attached_list().is_empty());
    assert_eq!(
        env.host.prompt(OK_ID, "x").await.unwrap_err().code,
        "NOT_ATTACHED"
    );
}

#[tokio::test]
async fn permission_requests_are_auto_rejected() {
    let mut env = setup("perm", 4);
    env.host.attach(OK_ID).await.unwrap();
    env.host.prompt(OK_ID, "use a tool").await.unwrap();
    let evs = collect_until(&mut env.events, |e| e["type"] == "prompt.done").await;

    let rejected: Vec<_> = evs
        .iter()
        .filter(|e| e["type"] == "permission.autoRejected")
        .collect();
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0]["outcome"]["optionId"], "reject_once");
    let start = evs
        .iter()
        .find(|e| e["type"] == "tool.call" && e["phase"] == "start")
        .unwrap();
    assert_eq!(start["rawInput"]["command"], "echo hello");
    let end = evs
        .iter()
        .find(|e| e["type"] == "tool.call" && e["phase"] == "update")
        .unwrap();
    assert_eq!(end["status"], "failed");
    assert!(
        texts(&evs, "assistant")
            .iter()
            .any(|(t, _)| t == "perm: reject_once")
    );
    env.host.shutdown_all().await;
}

#[tokio::test]
async fn load_errors_are_mapped() {
    let env = setup("errors", 4);
    let busy = env.host.attach(BUSY_ID).await.unwrap_err();
    assert_eq!(busy.code, "SESSION_BUSY");
    assert!(busy.message.contains("再接管"), "{}", busy.message);

    let hang = env.host.attach(HANG_ID).await.unwrap_err();
    assert_eq!(hang.code, "LOAD_TIMEOUT");

    let missing = env
        .host
        .attach("aaaaaaaa-0000-0000-0000-000000000099")
        .await
        .unwrap_err();
    assert_eq!(missing.code, "NOT_FOUND");
    assert_eq!(
        env.host.attach("../x").await.unwrap_err().code,
        "BAD_REQUEST"
    );
    assert!(
        env.host.attached_list().is_empty(),
        "failed attaches must not leak slots"
    );
}

#[tokio::test]
async fn session_limit_and_cwd_validation() {
    let env = setup("limit", 2);
    let cwd = env.dir.to_str().unwrap().to_string();
    env.host.new_session(&cwd).await.unwrap();
    env.host.new_session(&cwd).await.unwrap();
    assert_eq!(
        env.host.new_session(&cwd).await.unwrap_err().code,
        "LIMIT_REACHED"
    );
    assert_eq!(
        env.host.attach(OK_ID).await.unwrap_err().code,
        "LIMIT_REACHED"
    );

    assert_eq!(
        env.host.new_session("relative/dir").await.unwrap_err().code,
        "BAD_REQUEST"
    );
    assert_eq!(
        env.host.new_session("/etc").await.unwrap_err().code,
        "BAD_REQUEST"
    );
    env.host.shutdown_all().await;
}

#[tokio::test]
async fn concurrent_prompt_rejected_and_cancel_works() {
    let mut env = setup("cancel", 4);
    env.host.attach(OK_ID).await.unwrap();
    env.host.prompt(OK_ID, "slow please").await.unwrap();
    assert_eq!(
        env.host.prompt(OK_ID, "second").await.unwrap_err().code,
        "PROMPT_RUNNING"
    );
    env.host.cancel(OK_ID).await.unwrap();
    let evs = collect_until(&mut env.events, |e| e["type"] == "prompt.done").await;
    assert_eq!(evs.last().unwrap()["stopReason"], "cancelled");
    // a new turn is accepted after the previous one finished
    env.host.prompt(OK_ID, "third").await.unwrap();
    collect_until(&mut env.events, |e| e["type"] == "prompt.done").await;
    env.host.shutdown_all().await;
}

#[tokio::test]
async fn agent_crash_releases_slot() {
    let mut env = setup("crash", 4);
    env.host.attach(OK_ID).await.unwrap();
    let pid = env.host.attached_list()[0]["pid"].as_u64().unwrap();
    std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .unwrap();
    let evs = collect_until(&mut env.events, |e| e["type"] == "session.closed").await;
    assert_eq!(evs.last().unwrap()["reason"], "agent_exited");
    assert!(env.host.attached_list().is_empty());
}
