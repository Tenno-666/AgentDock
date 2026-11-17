//! Read-only parser for Kiro session history (`<id>.jsonl`).
//!
//! The format has no public spec. Observed on kiro-cli 2.13.0 across 26782 lines:
//! line kinds Prompt / AssistantMessage / ToolResults / Compaction (all `version: v1`);
//! content kinds text / thinking / toolUse / toolResult / image.
//! Unknown shapes are kept as `Other` instead of failing the whole file.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

/// Long tool outputs are truncated in views; the full text stays in the session file.
pub const MAX_TOOL_OUTPUT_CHARS: usize = 2000;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Entry {
    #[serde(rename_all = "camelCase")]
    User {
        message_id: Option<String>,
        text: String,
        image_count: usize,
        timestamp: Option<i64>,
    },
    #[serde(rename_all = "camelCase")]
    Assistant {
        message_id: Option<String>,
        text: String,
        tool_uses: Vec<ToolUse>,
    },
    #[serde(rename_all = "camelCase")]
    ToolResults {
        message_id: Option<String>,
        results: Vec<ToolResult>,
    },
    #[serde(rename_all = "camelCase")]
    Compaction { summary_chars: usize },
    #[serde(rename_all = "camelCase")]
    Other { kind: String },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolUse {
    pub tool_use_id: Option<String>,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub tool_use_id: Option<String>,
    pub status: Option<String>,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub total_entries: usize,
    pub skipped_lines: usize,
    pub entries: Vec<Entry>,
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn content_items(data: &Value) -> &[Value] {
    data.get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Concatenate `{kind:"text", data:"..."}` items.
fn join_text(items: &[Value]) -> String {
    items
        .iter()
        .filter(|c| c.get("kind").and_then(Value::as_str) == Some("text"))
        .filter_map(|c| c.get("data").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

fn truncate(s: &str, max_chars: usize) -> (String, bool) {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => (s[..idx].to_string(), true),
        None => (s.to_string(), false),
    }
}

fn parse_tool_result(item: &Value) -> Option<ToolResult> {
    if item.get("kind").and_then(Value::as_str) != Some("toolResult") {
        return None;
    }
    let d = item.get("data")?;
    let inner = d
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut text = join_text(inner);
    if text.is_empty() && !inner.is_empty() {
        // non-text results (e.g. structured json): show compact json
        text = Value::Array(inner.to_vec()).to_string();
    }
    let (text, truncated) = truncate(&text, MAX_TOOL_OUTPUT_CHARS);
    Some(ToolResult {
        tool_use_id: str_field(d, "toolUseId"),
        status: str_field(d, "status"),
        text,
        truncated,
    })
}

pub fn parse_line(line: &str) -> Option<Entry> {
    let v: Value = serde_json::from_str(line).ok()?;
    let kind = v.get("kind").and_then(Value::as_str)?.to_string();
    let data = v.get("data").cloned().unwrap_or(Value::Null);
    let message_id = str_field(&data, "message_id");
    let items = content_items(&data);
    let entry = match kind.as_str() {
        "Prompt" => Entry::User {
            message_id,
            text: join_text(items),
            image_count: items
                .iter()
                .filter(|c| c.get("kind").and_then(Value::as_str) == Some("image"))
                .count(),
            timestamp: data.pointer("/meta/timestamp").and_then(Value::as_i64),
        },
        "AssistantMessage" => Entry::Assistant {
            message_id,
            text: join_text(items),
            tool_uses: items
                .iter()
                .filter(|c| c.get("kind").and_then(Value::as_str) == Some("toolUse"))
                .filter_map(|c| c.get("data"))
                .map(|d| ToolUse {
                    tool_use_id: str_field(d, "toolUseId"),
                    name: str_field(d, "name").unwrap_or_default(),
                    input: d.get("input").cloned().unwrap_or(Value::Null),
                })
                .collect(),
        },
        "ToolResults" => Entry::ToolResults {
            message_id,
            results: items.iter().filter_map(parse_tool_result).collect(),
        },
        "Compaction" => Entry::Compaction {
            summary_chars: data
                .get("summary")
                .and_then(Value::as_str)
                .map(|s| s.chars().count())
                .unwrap_or(0),
        },
        _ => Entry::Other { kind },
    };
    Some(entry)
}

/// Read a session file. `limit` keeps only the last N entries (None = all).
pub fn read_session(path: &Path, limit: Option<usize>) -> std::io::Result<SessionView> {
    let reader = BufReader::new(File::open(path)?);
    let mut entries = Vec::new();
    let mut skipped = 0;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match parse_line(&line) {
            Some(e) => entries.push(e),
            None => skipped += 1,
        }
    }
    let total = entries.len();
    if let Some(n) = limit
        && total > n
    {
        entries.drain(..total - n);
    }
    Ok(SessionView {
        total_entries: total,
        skipped_lines: skipped,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Lines copied (shortened) from spike session 164a187e.
    const PROMPT: &str = r#"{"version":"v1","kind":"Prompt","data":{"message_id":"m1","content":[{"kind":"text","data":"run echo"}],"meta":{"timestamp":1794818774}}}"#;
    const TOOL_USE: &str = r#"{"version":"v1","kind":"AssistantMessage","data":{"message_id":"m2","content":[{"kind":"text","data":""},{"kind":"toolUse","data":{"toolUseId":"t1","name":"shell","input":{"command":"echo hello"}}}]}}"#;
    const TOOL_RESULT: &str = r#"{"version":"v1","kind":"ToolResults","data":{"message_id":"m3","content":[{"kind":"toolResult","data":{"toolUseId":"t1","content":[{"kind":"text","data":"User denied tool execution"}],"status":"error"}}],"results":{}}}"#;
    const THINKING: &str = r#"{"version":"v1","kind":"AssistantMessage","data":{"message_id":"m4","content":[{"kind":"thinking","data":{"text":"secret","signature":"x"}},{"kind":"text","data":"hello"}]}}"#;

    #[test]
    fn parses_prompt() {
        let e = parse_line(PROMPT).unwrap();
        assert_eq!(
            e,
            Entry::User {
                message_id: Some("m1".into()),
                text: "run echo".into(),
                image_count: 0,
                timestamp: Some(1794818774)
            }
        );
    }

    #[test]
    fn parses_tool_use_and_result() {
        match parse_line(TOOL_USE).unwrap() {
            Entry::Assistant {
                tool_uses, text, ..
            } => {
                assert_eq!(text, "");
                assert_eq!(tool_uses[0].name, "shell");
                assert_eq!(tool_uses[0].input["command"], "echo hello");
            }
            other => panic!("{other:?}"),
        }
        match parse_line(TOOL_RESULT).unwrap() {
            Entry::ToolResults { results, .. } => {
                assert_eq!(results[0].status.as_deref(), Some("error"));
                assert_eq!(results[0].text, "User denied tool execution");
                assert!(!results[0].truncated);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn thinking_is_not_shown_as_reply_text() {
        match parse_line(THINKING).unwrap() {
            Entry::Assistant { text, .. } => assert_eq!(text, "hello"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_and_broken_lines_are_tolerated() {
        assert_eq!(
            parse_line(r#"{"version":"v2","kind":"NewThing","data":{}}"#),
            Some(Entry::Other {
                kind: "NewThing".into()
            })
        );
        assert_eq!(parse_line("{not json"), None);
        assert_eq!(parse_line(r#"{"no_kind":1}"#), None);
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let (s, t) = truncate("中文测试", 2);
        assert_eq!((s.as_str(), t), ("中文", true));
        let (s, t) = truncate("ab", 5);
        assert_eq!((s.as_str(), t), ("ab", false));
    }

    #[test]
    fn read_session_with_limit_and_skips() {
        let p = std::env::temp_dir().join(format!("agentdock-log-{}.jsonl", std::process::id()));
        std::fs::write(
            &p,
            [PROMPT, "{broken", TOOL_USE, "", TOOL_RESULT].join("\n"),
        )
        .unwrap();
        let v = read_session(&p, Some(2)).unwrap();
        assert_eq!(v.total_entries, 3);
        assert_eq!(v.skipped_lines, 1);
        assert_eq!(v.entries.len(), 2);
        assert!(matches!(v.entries[0], Entry::Assistant { .. }));
        std::fs::remove_file(&p).unwrap();
    }
}
