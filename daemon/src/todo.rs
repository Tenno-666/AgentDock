//! Todo list: extraction of confirmed todo markers from session files + SQLite store.
//!
//! Marker (steering M43): a line in an AI reply that starts with `【待办已确认】`,
//! outside fenced code blocks, at most 3 leading spaces. Candidates (`【待办候选】`)
//! are not stored; only confirmed ones are.
//!
//! Dedup: one row per (session, message, title). Deleting an auto todo leaves a
//! tombstone so rescanning never brings it back.
//! Scanning is incremental: the byte offset of every session file is persisted and
//! only complete new lines are read.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;

use crate::discovery;
use crate::session_log::{Entry, parse_line};

pub const CONFIRMED_MARKER: &str = "【待办已确认】";
pub const MAX_TITLE_CHARS: usize = 500;
pub const PRIORITIES: [&str; 3] = ["high", "medium", "low"];

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Todo {
    pub id: i64,
    pub title: String,
    /// "open" | "done"
    pub status: String,
    /// "auto" (from a session) | "manual"
    pub source: String,
    pub agent: Option<String>,
    pub session_id: Option<String>,
    pub message_id: Option<String>,
    /// Reserved for the extended marker format: "high" | "medium" | "low".
    pub priority: Option<String>,
    /// Reserved: YYYY-MM-DD.
    pub due_date: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub done_at_ms: Option<i64>,
}

#[derive(Debug, Default, Clone)]
pub struct TodoPatch {
    pub title: Option<String>,
    pub status: Option<String>,
    /// Some(None) clears the field.
    pub priority: Option<Option<String>>,
    pub due_date: Option<Option<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoreError {
    pub code: &'static str,
    pub message: String,
}

impl StoreError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::new("INTERNAL", format!("database: {e}"))
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Titles of confirmed todo markers in one reply text.
pub fn extract_confirmed(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || line.len() - trimmed.len() > 3 {
            continue; // fenced or indented code block
        }
        if let Some(rest) = trimmed.strip_prefix(CONFIRMED_MARKER) {
            let title: String = rest.trim().chars().take(MAX_TITLE_CHARS).collect();
            if !title.is_empty() && !out.contains(&title) {
                out.push(title);
            }
        }
    }
    out
}

fn validate_title(title: &str) -> Result<String, StoreError> {
    let t = title.trim();
    if t.is_empty() {
        return Err(StoreError::new("BAD_REQUEST", "title is empty"));
    }
    if t.chars().count() > MAX_TITLE_CHARS {
        return Err(StoreError::new(
            "BAD_REQUEST",
            format!("title exceeds {MAX_TITLE_CHARS} characters"),
        ));
    }
    Ok(t.to_string())
}

fn validate_priority(p: &Option<String>) -> Result<(), StoreError> {
    match p {
        Some(v) if !PRIORITIES.contains(&v.as_str()) => Err(StoreError::new(
            "BAD_REQUEST",
            format!("priority must be one of {PRIORITIES:?}"),
        )),
        _ => Ok(()),
    }
}

fn validate_due(d: &Option<String>) -> Result<(), StoreError> {
    let Some(v) = d else { return Ok(()) };
    let b = v.as_bytes();
    let ok = b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
        && (1..=12).contains(&v[5..7].parse::<u32>().unwrap_or(0))
        && (1..=31).contains(&v[8..10].parse::<u32>().unwrap_or(0));
    if ok {
        Ok(())
    } else {
        Err(StoreError::new("BAD_REQUEST", "dueDate must be YYYY-MM-DD"))
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS todos (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    title         TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'open',
    source        TEXT NOT NULL,
    agent         TEXT,
    session_id    TEXT,
    message_id    TEXT,
    priority      TEXT,
    due_date      TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    done_at_ms    INTEGER,
    deleted       INTEGER NOT NULL DEFAULT 0,
    dedup_key     TEXT UNIQUE
);
CREATE TABLE IF NOT EXISTS scan_offsets (
    path   TEXT PRIMARY KEY,
    offset INTEGER NOT NULL
);
PRAGMA user_version = 1;
";

const COLUMNS: &str = "id, title, status, source, agent, session_id, message_id, priority, \
due_date, created_at_ms, updated_at_ms, done_at_ms";

fn row_to_todo(r: &Row) -> rusqlite::Result<Todo> {
    Ok(Todo {
        id: r.get(0)?,
        title: r.get(1)?,
        status: r.get(2)?,
        source: r.get(3)?,
        agent: r.get(4)?,
        session_id: r.get(5)?,
        message_id: r.get(6)?,
        priority: r.get(7)?,
        due_date: r.get(8)?,
        created_at_ms: r.get(9)?,
        updated_at_ms: r.get(10)?,
        done_at_ms: r.get(11)?,
    })
}

pub struct TodoStore {
    conn: Mutex<Connection>,
}

impl TodoStore {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StoreError::new("INTERNAL", format!("create dir: {e}")))?;
        }
        let conn = Connection::open(path)?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn get_locked(conn: &Connection, id: i64) -> Result<Option<Todo>, StoreError> {
        Ok(conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM todos WHERE id = ?1 AND deleted = 0"),
                [id],
                row_to_todo,
            )
            .optional()?)
    }

    /// `status`: "open" | "done" | "all"
    pub fn list(&self, status: &str) -> Result<Vec<Todo>, StoreError> {
        let filter = match status {
            "open" => "AND status = 'open'",
            "done" => "AND status = 'done'",
            "all" => "",
            _ => {
                return Err(StoreError::new(
                    "BAD_REQUEST",
                    "status must be open, done or all",
                ));
            }
        };
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM todos WHERE deleted = 0 {filter} ORDER BY created_at_ms DESC, id DESC"
        ))?;
        let rows = stmt.query_map([], row_to_todo)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn add_manual(
        &self,
        title: &str,
        priority: Option<String>,
        due_date: Option<String>,
    ) -> Result<Todo, StoreError> {
        let title = validate_title(title)?;
        validate_priority(&priority)?;
        validate_due(&due_date)?;
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO todos (title, source, priority, due_date, created_at_ms, updated_at_ms) \
             VALUES (?1, 'manual', ?2, ?3, ?4, ?4)",
            params![title, priority, due_date, now],
        )?;
        let id = conn.last_insert_rowid();
        Self::get_locked(&conn, id)?.ok_or_else(|| StoreError::new("INTERNAL", "insert lost"))
    }

    pub fn update(&self, id: i64, patch: TodoPatch) -> Result<Todo, StoreError> {
        let title = patch.title.as_deref().map(validate_title).transpose()?;
        if let Some(s) = &patch.status
            && s != "open"
            && s != "done"
        {
            return Err(StoreError::new(
                "BAD_REQUEST",
                "status must be open or done",
            ));
        }
        if let Some(p) = &patch.priority {
            validate_priority(p)?;
        }
        if let Some(d) = &patch.due_date {
            validate_due(d)?;
        }
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        let mut cur = Self::get_locked(&conn, id)?
            .ok_or_else(|| StoreError::new("NOT_FOUND", format!("todo {id} not found")))?;
        if let Some(t) = title {
            cur.title = t;
        }
        if let Some(s) = patch.status
            && s != cur.status
        {
            cur.done_at_ms = (s == "done").then_some(now);
            cur.status = s;
        }
        if let Some(p) = patch.priority {
            cur.priority = p;
        }
        if let Some(d) = patch.due_date {
            cur.due_date = d;
        }
        conn.execute(
            "UPDATE todos SET title = ?2, status = ?3, priority = ?4, due_date = ?5, \
             done_at_ms = ?6, updated_at_ms = ?7 WHERE id = ?1",
            params![
                id,
                cur.title,
                cur.status,
                cur.priority,
                cur.due_date,
                cur.done_at_ms,
                now
            ],
        )?;
        Self::get_locked(&conn, id)?.ok_or_else(|| StoreError::new("INTERNAL", "update lost"))
    }

    /// Soft delete: the dedup key stays so the same marker is never re-imported.
    pub fn delete(&self, id: i64) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE todos SET deleted = 1, updated_at_ms = ?2 WHERE id = ?1 AND deleted = 0",
            params![id, now_ms()],
        )?;
        if n == 0 {
            return Err(StoreError::new("NOT_FOUND", format!("todo {id} not found")));
        }
        Ok(())
    }

    /// Insert one extracted todo; returns it if it is new.
    pub fn insert_auto(
        &self,
        agent: &str,
        session_id: &str,
        message_id: Option<&str>,
        title: &str,
    ) -> Result<Option<Todo>, StoreError> {
        let key = format!("{agent}|{session_id}|{}|{title}", message_id.unwrap_or(""));
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "INSERT OR IGNORE INTO todos (title, source, agent, session_id, message_id, \
             created_at_ms, updated_at_ms, dedup_key) VALUES (?1, 'auto', ?2, ?3, ?4, ?5, ?5, ?6)",
            params![title, agent, session_id, message_id, now, key],
        )?;
        if n == 0 {
            return Ok(None);
        }
        let id = conn.last_insert_rowid();
        Self::get_locked(&conn, id)
    }

    fn offset(&self, path: &str) -> Result<u64, StoreError> {
        let conn = self.conn.lock().unwrap();
        let v: Option<i64> = conn
            .query_row(
                "SELECT offset FROM scan_offsets WHERE path = ?1",
                [path],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.unwrap_or(0).max(0) as u64)
    }

    fn set_offset(&self, path: &str, offset: u64) -> Result<(), StoreError> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO scan_offsets (path, offset) VALUES (?1, ?2) \
             ON CONFLICT(path) DO UPDATE SET offset = excluded.offset",
            params![path, offset as i64],
        )?;
        Ok(())
    }

    /// Scan new complete lines of one Kiro session file. Returns newly added todos.
    pub fn scan_file(&self, path: &Path) -> Result<Vec<Todo>, StoreError> {
        let io =
            |e: std::io::Error| StoreError::new("INTERNAL", format!("{}: {e}", path.display()));
        let Some(session_id) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".jsonl"))
            .filter(|id| discovery::is_valid_session_id(id))
        else {
            return Ok(Vec::new());
        };
        let key = path.to_string_lossy().to_string();
        let len = std::fs::metadata(path).map_err(io)?.len();
        let mut offset = self.offset(&key)?;
        if len < offset {
            offset = 0; // file was rewritten; dedup keys prevent duplicates
        }
        if len == offset {
            return Ok(Vec::new());
        }
        let mut file = File::open(path).map_err(io)?;
        file.seek(SeekFrom::Start(offset)).map_err(io)?;
        let mut reader = BufReader::new(file);
        let mut added = Vec::new();
        let mut buf = Vec::new();
        let marker = CONFIRMED_MARKER.as_bytes();
        loop {
            buf.clear();
            let n = reader.read_until(b'\n', &mut buf).map_err(io)?;
            if n == 0 || buf.last() != Some(&b'\n') {
                break; // EOF or a line still being written
            }
            offset += n as u64;
            // Cheap byte filter before JSON parsing (session files total hundreds of MB).
            if !buf.windows(marker.len()).any(|w| w == marker) {
                continue;
            }
            let Ok(line) = std::str::from_utf8(&buf) else {
                continue;
            };
            if let Some(Entry::Assistant {
                message_id, text, ..
            }) = parse_line(line)
            {
                for title in extract_confirmed(&text) {
                    if let Some(t) =
                        self.insert_auto("kiro", session_id, message_id.as_deref(), &title)?
                    {
                        added.push(t);
                    }
                }
            }
        }
        self.set_offset(&key, offset)?;
        Ok(added)
    }

    pub fn scan_dir(&self, dir: &Path) -> Result<Vec<Todo>, StoreError> {
        let mut added = Vec::new();
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => return Err(StoreError::new("INTERNAL", format!("read_dir: {e}"))),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            match self.scan_file(&path) {
                Ok(mut v) => added.append(&mut v),
                Err(e) => tracing::warn!("todo scan {}: {}", path.display(), e.message),
            }
        }
        Ok(added)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn extracts_only_line_start_markers_outside_code() {
        let text = "分析完毕。\n\
            【待办候选】不应入库\n\
            【待办已确认】修复 A 的 B\n\
            正文里提到 `【待办已确认】内联` 不算\n\
            > 【待办已确认】引用块不算\n\
            ```\n【待办已确认】代码块里不算\n```\n\
            \x20\x20\x20\x20【待办已确认】缩进代码不算\n\
            \x20\x20【待办已确认】  补充 C 的测试  \n\
            【待办已确认】\n\
            【待办已确认】修复 A 的 B\n";
        assert_eq!(
            extract_confirmed(text),
            vec!["修复 A 的 B", "补充 C 的测试"]
        );
    }

    #[test]
    fn manual_crud_and_validation() {
        let s = TodoStore::open_in_memory().unwrap();
        let t = s
            .add_manual(
                "  写 README  ",
                Some("high".into()),
                Some("2026-12-01".into()),
            )
            .unwrap();
        assert_eq!(
            (t.title.as_str(), t.source.as_str(), t.status.as_str()),
            ("写 README", "manual", "open")
        );
        assert_eq!(
            s.add_manual(" ", None, None).unwrap_err().code,
            "BAD_REQUEST"
        );
        assert_eq!(
            s.add_manual("x", Some("urgent".into()), None)
                .unwrap_err()
                .code,
            "BAD_REQUEST"
        );
        assert_eq!(
            s.add_manual("x", None, Some("2026-13-01".into()))
                .unwrap_err()
                .code,
            "BAD_REQUEST"
        );
        assert_eq!(
            s.add_manual("x", None, Some("26-1-1".into()))
                .unwrap_err()
                .code,
            "BAD_REQUEST"
        );

        let done = s
            .update(
                t.id,
                TodoPatch {
                    status: Some("done".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(done.done_at_ms.is_some());
        assert_eq!(s.list("open").unwrap().len(), 0);
        assert_eq!(s.list("done").unwrap().len(), 1);
        let reopened = s
            .update(
                t.id,
                TodoPatch {
                    status: Some("open".into()),
                    priority: Some(None),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!((reopened.done_at_ms, reopened.priority), (None, None));
        assert_eq!(s.list("bogus").unwrap_err().code, "BAD_REQUEST");

        s.delete(t.id).unwrap();
        assert!(s.list("all").unwrap().is_empty());
        assert_eq!(s.delete(t.id).unwrap_err().code, "NOT_FOUND");
        assert_eq!(
            s.update(t.id, TodoPatch::default()).unwrap_err().code,
            "NOT_FOUND"
        );
    }

    fn line(kind: &str, mid: &str, text: &str) -> String {
        serde_json::json!({"version":"v1","kind":kind,"data":{"message_id":mid,
            "content":[{"kind":"text","data":text}]}})
        .to_string()
            + "\n"
    }

    #[test]
    fn incremental_scan_dedup_partial_lines_and_tombstones() {
        let dir = std::env::temp_dir().join(format!("agentdock-todo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sid = "aaaaaaaa-0000-0000-0000-000000000001";
        let path = dir.join(format!("{sid}.jsonl"));
        let s = TodoStore::open_in_memory().unwrap();

        let mut f = std::fs::File::create(&path).unwrap();
        // user prompts quoting the marker are ignored; only assistant text counts
        f.write_all(line("Prompt", "u1", "【待办已确认】用户写的不算").as_bytes())
            .unwrap();
        f.write_all(line("AssistantMessage", "m1", "好的\n【待办已确认】任务一").as_bytes())
            .unwrap();
        // half-written line must not be consumed
        let partial = line("AssistantMessage", "m2", "【待办已确认】任务二");
        let (head, tail) = partial.split_at(20);
        f.write_all(head.as_bytes()).unwrap();
        f.flush().unwrap();

        let added = s.scan_dir(&dir).unwrap();
        assert_eq!(
            added.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
            vec!["任务一"]
        );
        assert_eq!(added[0].session_id.as_deref(), Some(sid));
        assert_eq!(added[0].message_id.as_deref(), Some("m1"));
        assert!(
            s.scan_dir(&dir).unwrap().is_empty(),
            "no new complete lines"
        );

        f.write_all(tail.as_bytes()).unwrap();
        f.flush().unwrap();
        let added = s.scan_dir(&dir).unwrap();
        assert_eq!(
            added.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
            vec!["任务二"]
        );

        // file rewritten (shorter): rescan from 0, dedup keeps one copy each
        drop(f);
        std::fs::write(
            &path,
            line("AssistantMessage", "m1", "【待办已确认】任务一"),
        )
        .unwrap();
        assert!(s.scan_dir(&dir).unwrap().is_empty());
        assert_eq!(s.list("all").unwrap().len(), 2);

        // deleted auto todo does not come back
        let one = s
            .list("all")
            .unwrap()
            .into_iter()
            .find(|t| t.title == "任务一")
            .unwrap();
        s.delete(one.id).unwrap();
        std::fs::write(
            &path,
            line("AssistantMessage", "m1", "【待办已确认】任务一"),
        )
        .unwrap();
        s.set_offset(&path.to_string_lossy(), 0).unwrap();
        assert!(s.scan_dir(&dir).unwrap().is_empty());
        assert_eq!(s.list("all").unwrap().len(), 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn store_file_is_private_and_persistent() {
        use std::os::unix::fs::PermissionsExt;
        let p = std::env::temp_dir().join(format!("agentdock-db-{}/t.sqlite3", std::process::id()));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        {
            let s = TodoStore::open(&p).unwrap();
            s.add_manual("持久化", None, None).unwrap();
        }
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let s = TodoStore::open(&p).unwrap();
        assert_eq!(s.list("all").unwrap()[0].title, "持久化");
        std::fs::remove_dir_all(p.parent().unwrap()).unwrap();
    }
}
