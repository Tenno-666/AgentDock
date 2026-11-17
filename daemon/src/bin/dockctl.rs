//! dockctl: command line client for agentdock-daemon (acceptance testing).
//!
//!   dockctl sessions             list active sessions
//!   dockctl view <id> [limit]    show session history
//!   dockctl raw '<json>'         send a raw request, print raw reply
//!
//! Server address: AGENTDOCK_URL (default ws://127.0.0.1:7820/ws)

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

async fn call(req: Value) -> anyhow::Result<Value> {
    let url = std::env::var("AGENTDOCK_URL").unwrap_or_else(|_| "ws://127.0.0.1:7820/ws".into());
    let (mut ws, _) = connect_async(&url)
        .await
        .with_context(|| format!("connect {url}"))?;
    ws.send(Message::Text(req.to_string().into())).await?;
    while let Some(msg) = ws.next().await {
        if let Message::Text(t) = msg? {
            let _ = ws.close(None).await;
            return Ok(serde_json::from_str(t.as_str())?);
        }
    }
    bail!("connection closed without reply")
}

fn short(s: Option<&str>, n: usize) -> String {
    let s = s.unwrap_or("-").replace('\n', " ");
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n).collect::<String>())
    } else {
        s
    }
}

fn print_sessions(r: &Value) {
    let list = r["sessions"].as_array().cloned().unwrap_or_default();
    println!(
        "{:<10} {:<9} {:<8} {:<22} {:<36} TITLE",
        "PANE", "TTY", "PID", "UPDATED", "SESSION"
    );
    for s in &list {
        println!(
            "{:<10} {:<9} {:<8} {:<22} {:<36} {}",
            s["tmuxPane"].as_str().unwrap_or("(无tmux)"),
            s["tty"].as_str().unwrap_or("-").trim_start_matches("/dev/"),
            s["pid"].to_string(),
            short(s["updatedAt"].as_str(), 19),
            s["sessionId"].as_str().unwrap_or("-"),
            short(s["title"].as_str(), 30),
        );
        println!("{:<10} cwd: {}", "", s["cwd"].as_str().unwrap_or("-"));
    }
    println!("共 {} 个活跃会话", list.len());
}

fn print_view(r: &Value) {
    let v = &r["view"];
    println!(
        "会话 {}：共 {} 条，显示 {} 条，跳过无法解析的行 {} 行\n",
        r["sessionId"].as_str().unwrap_or("-"),
        v["totalEntries"],
        v["entries"].as_array().map_or(0, Vec::len),
        v["skippedLines"]
    );
    for e in v["entries"].as_array().into_iter().flatten() {
        match e["role"].as_str().unwrap_or("") {
            "user" => println!("[用户] {}", e["text"].as_str().unwrap_or("")),
            "assistant" => {
                let t = e["text"].as_str().unwrap_or("");
                if !t.is_empty() {
                    println!("[AI] {t}");
                }
                for tu in e["toolUses"].as_array().into_iter().flatten() {
                    println!(
                        "  [工具调用] {} {}",
                        tu["name"].as_str().unwrap_or("?"),
                        tu["input"]
                    );
                }
            }
            "toolResults" => {
                for tr in e["results"].as_array().into_iter().flatten() {
                    println!(
                        "  [工具结果:{}] {}",
                        tr["status"].as_str().unwrap_or("?"),
                        short(tr["text"].as_str(), 200)
                    );
                }
            }
            "compaction" => println!("[上下文压缩] 摘要 {} 字", e["summaryChars"]),
            other => println!("[未知类型 {other}] {}", e["kind"]),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let req = match args.first().map(String::as_str) {
        Some("sessions") => json!({"id": 1, "type": "sessions.list"}),
        Some("view") if args.len() >= 2 => {
            let limit = args
                .get(2)
                .map(|s| s.parse::<usize>())
                .transpose()
                .context("limit must be a number")?;
            json!({"id": 1, "type": "session.view", "sessionId": args[1], "limit": limit})
        }
        Some("raw") if args.len() == 2 => serde_json::from_str(&args[1]).context("raw json")?,
        _ => bail!("usage: dockctl sessions | view <sessionId> [limit] | raw '<json>'"),
    };
    let r = call(req).await?;
    match (args[0].as_str(), r["type"].as_str()) {
        (_, Some("error")) => bail!("server error: {}", r["message"]),
        ("sessions", _) => print_sessions(&r),
        ("view", _) => print_view(&r),
        _ => println!("{}", serde_json::to_string_pretty(&r)?),
    }
    Ok(())
}
