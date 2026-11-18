//! Active Kiro session discovery.
//!
//! Evidence (docs/S0-实测记录.md): every open Kiro session holds
//! `~/.kiro/sessions/cli/<id>.lock` = `{"pid":N,"started_at":...}` where N is a
//! `kiro-cli-chat acp` process. Stale locks exist (process gone), so liveness and
//! process name are both checked. The owning tmux pane is found by walking the
//! parent chain until a process whose stdin is a pane tty.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

const KIRO_PROCESS_MARKER: &str = "kiro-cli-chat";
const MAX_PARENT_DEPTH: usize = 32;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub session_id: String,
    pub agent: &'static str,
    pub pid: u32,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// tmux target such as `Kiro:3.1`, None if not running inside tmux (e.g. IDE).
    pub tmux_pane: Option<String>,
    pub tty: Option<String>,
}

#[derive(Deserialize)]
struct LockFile {
    pid: u32,
}

#[derive(Deserialize, Default)]
struct SessionMeta {
    cwd: Option<String>,
    title: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

pub fn kiro_sessions_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join(".kiro/sessions/cli")
}

/// Session ids are UUIDs; reject anything else so ids can be safely joined to paths.
pub fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

pub fn parse_lock_pid(content: &str) -> Option<u32> {
    serde_json::from_str::<LockFile>(content)
        .ok()
        .map(|l| l.pid)
}

/// `/proc/<pid>/stat`: "pid (comm) state ppid ...". comm may contain spaces and ')'.
pub fn parse_ppid(stat: &str) -> Option<u32> {
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(1)?.parse().ok()
}

/// Parse `tmux list-panes -a -F '#{pane_tty} <target>'` output into tty -> target.
/// A space separator is used because tmux replaces tabs with `_` when the locale is
/// not UTF-8 (e.g. daemons started by systemd); tty paths never contain spaces.
pub fn parse_tmux_panes(output: &str) -> HashMap<String, String> {
    output
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(tty, target)| (tty.trim().to_string(), target.trim().to_string()))
        .collect()
}

/// tmux with a UTF-8 locale, so output and `send-keys -l` text are not mangled
/// when the daemon runs without one.
fn tmux_cmd() -> Command {
    let mut c = Command::new("tmux");
    c.env("LC_ALL", "C.UTF-8");
    c
}

/// A tmux pane addressable through its server socket.
#[derive(Debug, Clone, PartialEq)]
pub struct PaneRef {
    pub socket: PathBuf,
    /// `session:window.pane`
    pub target: String,
    /// Human hint, e.g. `Kiro:3.1` or `busy:1.1 [-L agentdock]`.
    pub label: String,
}

/// Panes of every tmux server of the current user (`/tmp/tmux-<uid>/*`), keyed by tty.
pub fn tmux_panes_detailed() -> HashMap<String, PaneRef> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let mut map = HashMap::new();
    let Ok(uid) = fs::metadata("/proc/self").map(|m| m.uid()) else {
        return map;
    };
    let base = std::env::var_os("TMUX_TMPDIR").unwrap_or_else(|| "/tmp".into());
    let dir = PathBuf::from(base).join(format!("tmux-{uid}"));
    let Ok(entries) = fs::read_dir(&dir) else {
        return map; // tmux never started
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_socket()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let out = tmux_cmd()
            .arg("-S")
            .arg(entry.path())
            .args([
                "list-panes",
                "-a",
                "-F",
                "#{pane_tty} #{session_name}:#{window_index}.#{pane_index}",
            ])
            .output();
        let Ok(o) = out else { continue };
        if !o.status.success() {
            continue; // stale socket, server gone
        }
        for (tty, target) in parse_tmux_panes(&String::from_utf8_lossy(&o.stdout)) {
            let label = if name == "default" {
                target.clone()
            } else {
                format!("{target} [-L {name}]")
            };
            map.insert(
                tty,
                PaneRef {
                    socket: entry.path(),
                    target,
                    label,
                },
            );
        }
    }
    map
}

/// tty -> pane label, for display.
pub fn tmux_pane_map() -> HashMap<String, String> {
    tmux_panes_detailed()
        .into_iter()
        .map(|(tty, p)| (tty, p.label))
        .collect()
}

impl PaneRef {
    fn tmux(&self) -> Command {
        let mut c = tmux_cmd();
        c.arg("-S").arg(&self.socket);
        c
    }

    /// Foreground command of the pane (e.g. `kiro-cli`, `bash`), None if the pane is gone.
    pub fn current_command(&self) -> Option<String> {
        let o = self
            .tmux()
            .args([
                "display-message",
                "-p",
                "-t",
                &self.target,
                "#{pane_current_command}",
            ])
            .output()
            .ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
    }

    /// Send one key name (e.g. `C-u`, `Enter`).
    pub fn send_key(&self, key: &str) -> std::io::Result<()> {
        self.run(&["send-keys", "-t", &self.target, key])
    }

    /// Type text literally (no key-name interpretation).
    pub fn send_text(&self, text: &str) -> std::io::Result<()> {
        self.run(&["send-keys", "-t", &self.target, "-l", text])
    }

    fn run(&self, args: &[&str]) -> std::io::Result<()> {
        let st = self.tmux().args(args).status()?;
        if st.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "tmux {} failed: {st}",
                args[0]
            )))
        }
    }
}

/// Pane owning a live session (via its lock pid), None if not in tmux.
pub fn pane_of_session(sessions_dir: &Path, session_id: &str) -> Option<(u32, PaneRef)> {
    let panes = tmux_panes_detailed();
    let labels = panes
        .iter()
        .map(|(k, v)| (k.clone(), v.label.clone()))
        .collect();
    let s = list_active_in(sessions_dir, Path::new("/proc"), &labels)
        .ok()?
        .into_iter()
        .find(|s| s.session_id == session_id)?;
    let pane = panes.get(s.tty.as_deref()?)?.clone();
    Some((s.pid, pane))
}

/// True while a live kiro process holds the session lock.
pub fn is_session_locked(sessions_dir: &Path, session_id: &str) -> bool {
    fs::read_to_string(sessions_dir.join(format!("{session_id}.lock")))
        .ok()
        .as_deref()
        .and_then(parse_lock_pid)
        .is_some_and(|pid| is_kiro_process(Path::new("/proc"), pid))
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TurnState {
    /// Last record is a final AI reply (or nothing yet): safe to quit.
    Idle,
    /// A turn is in progress: waiting for the model, a tool, or a y/n approval.
    Busy,
}

/// Infer whether a session is mid-turn from the last record of its JSONL file.
/// Unparseable tails count as Busy (fail safe).
pub fn turn_state(jsonl: &Path) -> TurnState {
    use crate::session_log::{Entry, parse_line};
    let Some(last) = last_line(jsonl) else {
        return TurnState::Idle; // no file / empty: nothing started
    };
    match parse_line(&last) {
        Some(Entry::Assistant { tool_uses, .. }) if tool_uses.is_empty() => TurnState::Idle,
        Some(Entry::Compaction { .. }) => TurnState::Idle,
        _ => TurnState::Busy,
    }
}

/// Last non-empty complete line, read backwards in chunks (lines can be MBs long).
fn last_line(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut end = len;
    let mut buf: Vec<u8> = Vec::new();
    const CHUNK: u64 = 64 * 1024;
    loop {
        let start = end.saturating_sub(CHUNK);
        let mut chunk = vec![0u8; (end - start) as usize];
        f.seek(SeekFrom::Start(start)).ok()?;
        f.read_exact(&mut chunk).ok()?;
        chunk.extend_from_slice(&buf);
        buf = chunk;
        let trimmed_end = buf.iter().rposition(|b| !b.is_ascii_whitespace())? + 1;
        if let Some(nl) = buf[..trimmed_end].iter().rposition(|&b| b == b'\n') {
            return String::from_utf8(buf[nl + 1..trimmed_end].to_vec()).ok();
        }
        if start == 0 {
            return String::from_utf8(buf[..trimmed_end].to_vec()).ok();
        }
        end = start;
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RecentSession {
    pub session_id: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub updated_at: Option<String>,
}

/// All sessions with metadata, newest first.
pub fn list_all_meta(sessions_dir: &Path) -> Vec<RecentSession> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(sessions_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(id) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        if !is_valid_session_id(id) {
            continue;
        }
        let meta = read_meta(sessions_dir, id);
        out.push(RecentSession {
            session_id: id.to_string(),
            cwd: meta.cwd,
            title: meta.title,
            updated_at: meta.updated_at,
        });
    }
    out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    out
}

fn is_kiro_process(proc_root: &Path, pid: u32) -> bool {
    match fs::read(proc_root.join(pid.to_string()).join("cmdline")) {
        Ok(raw) => String::from_utf8_lossy(&raw).contains(KIRO_PROCESS_MARKER),
        Err(_) => false,
    }
}

/// Walk up the parent chain, return (tty, pane) of the first process whose stdin is a known pane.
fn find_pane(
    proc_root: &Path,
    pid: u32,
    panes: &HashMap<String, String>,
) -> (Option<String>, Option<String>) {
    let mut current = pid;
    let mut first_tty = None;
    for _ in 0..MAX_PARENT_DEPTH {
        if current <= 1 {
            break;
        }
        let dir = proc_root.join(current.to_string());
        if let Ok(target) = fs::read_link(dir.join("fd/0")) {
            let tty = target.to_string_lossy().to_string();
            if tty.starts_with("/dev/pts/") {
                if let Some(pane) = panes.get(&tty) {
                    return (Some(tty), Some(pane.clone()));
                }
                first_tty.get_or_insert(tty);
            }
        }
        match fs::read_to_string(dir.join("stat"))
            .ok()
            .as_deref()
            .and_then(parse_ppid)
        {
            Some(ppid) => current = ppid,
            None => break,
        }
    }
    (first_tty, None)
}

fn read_meta(sessions_dir: &Path, id: &str) -> SessionMeta {
    fs::read_to_string(sessions_dir.join(format!("{id}.json")))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// List live sessions. `proc_root` is `/proc` in production, a fake tree in tests.
pub fn list_active_in(
    sessions_dir: &Path,
    proc_root: &Path,
    panes: &HashMap<String, String>,
) -> std::io::Result<Vec<SessionInfo>> {
    let mut result = Vec::new();
    for entry in fs::read_dir(sessions_dir)? {
        let path = entry?.path();
        let Some(id) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".lock"))
        else {
            continue;
        };
        if !is_valid_session_id(id) {
            continue;
        }
        let Some(pid) = fs::read_to_string(&path)
            .ok()
            .as_deref()
            .and_then(parse_lock_pid)
        else {
            continue;
        };
        if !is_kiro_process(proc_root, pid) {
            continue; // stale lock or pid reused by another program
        }
        let meta = read_meta(sessions_dir, id);
        let (tty, tmux_pane) = find_pane(proc_root, pid, panes);
        result.push(SessionInfo {
            session_id: id.to_string(),
            agent: "kiro",
            pid,
            cwd: meta.cwd,
            title: meta.title,
            created_at: meta.created_at,
            updated_at: meta.updated_at,
            tmux_pane,
            tty,
        });
    }
    result.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(result)
}

pub fn list_active() -> std::io::Result<Vec<SessionInfo>> {
    list_active_in(&kiro_sessions_dir(), Path::new("/proc"), &tmux_pane_map())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn ppid_handles_spaces_and_parens_in_comm() {
        assert_eq!(parse_ppid("1234 (bun) S 7845 1234 ..."), Some(7845));
        assert_eq!(parse_ppid("1 (a b) c)) S 99 1"), Some(99));
        assert_eq!(parse_ppid("garbage"), None);
    }

    #[test]
    fn lock_pid() {
        assert_eq!(
            parse_lock_pid(r#"{"pid":7900,"started_at":"2026-11-09T02:43:49Z"}"#),
            Some(7900)
        );
        assert_eq!(parse_lock_pid("not json"), None);
    }

    #[test]
    fn session_id_validation_blocks_path_traversal() {
        assert!(is_valid_session_id("164a187e-b6a9-4e55-9850-d11aeb3edeb3"));
        assert!(!is_valid_session_id("../../etc/passwd"));
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id("abc/def"));
    }

    #[test]
    fn tmux_output_parsing() {
        let m = parse_tmux_panes("/dev/pts/3 Kiro:1.1\n/dev/pts/5 Kiro:3.1\nbadline\n");
        assert_eq!(m.get("/dev/pts/5").map(String::as_str), Some("Kiro:3.1"));
        assert_eq!(m.len(), 2);
    }

    struct FakeProc {
        root: PathBuf,
    }

    impl FakeProc {
        fn add(&self, pid: u32, ppid: u32, cmdline: &str, stdin: Option<&str>) {
            let d = self.root.join(pid.to_string());
            fs::create_dir_all(d.join("fd")).unwrap();
            fs::write(d.join("cmdline"), cmdline.replace(' ', "\0")).unwrap();
            fs::write(d.join("stat"), format!("{pid} (x) S {ppid} 0 0")).unwrap();
            if let Some(t) = stdin {
                symlink(t, d.join("fd/0")).unwrap();
            }
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agentdock-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn discovery_filters_stale_and_maps_panes() {
        let base = temp_dir("disc");
        let sessions = base.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let proc = FakeProc {
            root: base.join("proc"),
        };

        // live session in tmux: acp(100, no tty) -> tui(90, /dev/pts/5)
        proc.add(90, 1, "bun tui.js chat", Some("/dev/pts/5"));
        proc.add(100, 90, "/home/mi/.local/bin/kiro-cli-chat acp", None);
        let live = "aaaaaaaa-0000-0000-0000-000000000001";
        fs::write(sessions.join(format!("{live}.lock")), r#"{"pid":100}"#).unwrap();
        fs::write(
            sessions.join(format!("{live}.json")),
            r#"{"cwd":"/w","title":"t","updated_at":"2026-11-16T09:00:00Z"}"#,
        )
        .unwrap();

        // stale: pid not present
        fs::write(
            sessions.join("aaaaaaaa-0000-0000-0000-000000000002.lock"),
            r#"{"pid":555}"#,
        )
        .unwrap();
        // pid reused by an unrelated program
        proc.add(200, 1, "/usr/bin/vim", None);
        fs::write(
            sessions.join("aaaaaaaa-0000-0000-0000-000000000003.lock"),
            r#"{"pid":200}"#,
        )
        .unwrap();
        // live but not in tmux (e.g. IDE)
        proc.add(300, 1, "kiro-cli-chat acp", None);
        fs::write(
            sessions.join("aaaaaaaa-0000-0000-0000-000000000004.lock"),
            r#"{"pid":300}"#,
        )
        .unwrap();

        let panes = parse_tmux_panes("/dev/pts/5 Kiro:3.1\n");
        let got = list_active_in(&sessions, &proc.root, &panes).unwrap();

        assert_eq!(got.len(), 2, "{got:?}");
        let s = got.iter().find(|s| s.session_id == live).unwrap();
        assert_eq!(s.tmux_pane.as_deref(), Some("Kiro:3.1"));
        assert_eq!(s.tty.as_deref(), Some("/dev/pts/5"));
        assert_eq!(s.cwd.as_deref(), Some("/w"));
        let ide = got.iter().find(|s| s.pid == 300).unwrap();
        assert_eq!(ide.tmux_pane, None);
        assert_eq!(ide.cwd, None);

        fs::remove_dir_all(&base).unwrap();
    }

    fn line(kind: &str, content: &str) -> String {
        format!(
            r#"{{"version":"v1","kind":"{kind}","data":{{"message_id":"m","content":[{content}]}}}}"#
        )
    }

    #[test]
    fn turn_state_from_last_record() {
        let base = temp_dir("turn");
        let p = base.join("s.jsonl");
        assert_eq!(
            turn_state(&p),
            TurnState::Idle,
            "missing file = nothing started"
        );
        let prompt = line("Prompt", r#"{"kind":"text","data":"hi"}"#);
        let reply = line("AssistantMessage", r#"{"kind":"text","data":"ok"}"#);
        let tool = line(
            "AssistantMessage",
            r#"{"kind":"toolUse","data":{"toolUseId":"t","name":"shell","input":{}}}"#,
        );
        let result = line(
            "ToolResults",
            r#"{"kind":"toolResult","data":{"toolUseId":"t","content":[]}}"#,
        );
        for (lines, want) in [
            (vec![&prompt], TurnState::Busy),
            (vec![&prompt, &reply], TurnState::Idle),
            (vec![&prompt, &tool], TurnState::Busy), // waiting for tool / y-n approval
            (vec![&prompt, &tool, &result], TurnState::Busy),
        ] {
            let text = lines
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n")
                + "\n\n";
            fs::write(&p, text).unwrap();
            assert_eq!(turn_state(&p), want, "{lines:?}");
        }
        fs::write(&p, "{not json\n").unwrap();
        assert_eq!(turn_state(&p), TurnState::Busy, "fail safe");
        // last line longer than one read chunk
        let big = line(
            "AssistantMessage",
            &format!(r#"{{"kind":"text","data":"{}"}}"#, "x".repeat(200_000)),
        );
        fs::write(&p, format!("{prompt}\n{big}\n")).unwrap();
        assert_eq!(turn_state(&p), TurnState::Idle);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn all_meta_sorted_newest_first() {
        let base = temp_dir("recent");
        fs::write(
            base.join("aaaaaaaa-0000-0000-0000-000000000001.json"),
            r#"{"cwd":"/a","updated_at":"2026-01-01"}"#,
        )
        .unwrap();
        fs::write(
            base.join("aaaaaaaa-0000-0000-0000-000000000002.json"),
            r#"{"cwd":"/b","updated_at":"2026-02-01"}"#,
        )
        .unwrap();
        fs::write(base.join("not-a-session.txt"), "x").unwrap();
        let all = list_all_meta(&base);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].cwd.as_deref(), Some("/b"));
        fs::remove_dir_all(&base).unwrap();
    }
}
