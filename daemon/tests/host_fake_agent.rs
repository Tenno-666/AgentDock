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
        audit_path: Some(dir.join("audit.jsonl")),
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
async fn permission_allow_flow_with_audit() {
    let mut env = setup("perm-allow", 4);
    env.host.attach(OK_ID).await.unwrap();
    env.host.prompt(OK_ID, "use a tool").await.unwrap();
    let evs = collect_until(&mut env.events, |e| e["type"] == "permission.request").await;
    let req = evs.last().unwrap().clone();
    // parameters come from the preceding tool_call update, matched by toolCallId
    assert_eq!(req["rawInput"]["command"], "echo hello");
    assert_eq!(req["inputUnknown"], false);
    assert_eq!(req["options"].as_array().unwrap().len(), 3);
    assert_eq!(env.host.pending_approvals().len(), 1);

    let id = req["approvalId"].as_str().unwrap();
    assert_eq!(
        env.host
            .reply_permission(id, "bogus")
            .await
            .unwrap_err()
            .code,
        "BAD_REQUEST"
    );
    env.host.reply_permission(id, "allow_once").await.unwrap();
    // first reply wins
    assert_eq!(
        env.host
            .reply_permission(id, "reject_once")
            .await
            .unwrap_err()
            .code,
        "APPROVAL_NOT_FOUND"
    );
    let evs = collect_until(&mut env.events, |e| e["type"] == "prompt.done").await;
    let resolved = evs
        .iter()
        .find(|e| e["type"] == "permission.resolved")
        .unwrap();
    assert_eq!(
        (resolved["optionId"].as_str(), resolved["reason"].as_str()),
        (Some("allow_once"), Some("user"))
    );
    let end = evs
        .iter()
        .find(|e| e["type"] == "tool.call" && e["phase"] == "update")
        .unwrap();
    assert_eq!(end["status"], "completed");
    assert!(
        texts(&evs, "assistant")
            .iter()
            .any(|(t, _)| t == "perm: allow_once")
    );
    assert!(env.host.pending_approvals().is_empty());

    let audit = std::fs::read_to_string(env.dir.join("audit.jsonl")).unwrap();
    let lines: Vec<Value> = audit
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "{audit}");
    assert_eq!(lines[0]["event"], "requested");
    assert_eq!(lines[0]["rawInput"]["command"], "echo hello");
    assert_eq!(
        (lines[1]["event"].as_str(), lines[1]["optionId"].as_str()),
        (Some("resolved"), Some("allow_once"))
    );
    env.host.shutdown_all().await;
}

#[tokio::test]
async fn permission_reject_flow() {
    let mut env = setup("perm-reject", 4);
    env.host.attach(OK_ID).await.unwrap();
    env.host.prompt(OK_ID, "use a tool").await.unwrap();
    let evs = collect_until(&mut env.events, |e| e["type"] == "permission.request").await;
    let id = evs.last().unwrap()["approvalId"]
        .as_str()
        .unwrap()
        .to_string();
    env.host.reply_permission(&id, "reject_once").await.unwrap();
    let evs = collect_until(&mut env.events, |e| e["type"] == "prompt.done").await;
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
async fn unanswered_permission_waits_and_cancel_or_detach_voids_it() {
    let mut env = setup("perm-wait", 4);
    env.host.attach(OK_ID).await.unwrap();
    env.host.prompt(OK_ID, "use a tool").await.unwrap();
    collect_until(&mut env.events, |e| e["type"] == "permission.request").await;

    // nothing is approved automatically: still pending after a while
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(env.host.pending_approvals().len(), 1);
    assert_eq!(env.host.attached_list()[0]["promptRunning"], true);

    env.host.cancel(OK_ID).await.unwrap();
    let evs = collect_until(&mut env.events, |e| e["type"] == "prompt.done").await;
    let resolved = evs
        .iter()
        .find(|e| e["type"] == "permission.resolved")
        .unwrap();
    assert_eq!(
        (resolved["outcome"].as_str(), resolved["reason"].as_str()),
        (Some("cancelled"), Some("prompt_cancelled"))
    );
    assert_eq!(evs.last().unwrap()["stopReason"], "cancelled");
    assert!(env.host.pending_approvals().is_empty());

    // a pending card is voided when the session is detached
    env.host.prompt(OK_ID, "use a tool again").await.unwrap();
    collect_until(&mut env.events, |e| e["type"] == "permission.request").await;
    env.host.detach(OK_ID).await.unwrap();
    let evs = collect_until(&mut env.events, |e| e["type"] == "session.closed").await;
    let resolved = evs
        .iter()
        .find(|e| e["type"] == "permission.resolved")
        .unwrap();
    assert_eq!(resolved["reason"], "detached");
    assert!(env.host.pending_approvals().is_empty());
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
