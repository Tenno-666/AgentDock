//! dockctl: command line client for agentdock-daemon (acceptance testing).
//!
//!   dockctl sessions                 list active sessions on this machine
//!   dockctl view <id> [limit]        show session history from the session file
//!   dockctl attached                 list sessions attached by the daemon
//!   dockctl attach <id>              take over a session (prints replayed history)
//!   dockctl new <cwd>                start a new session in <cwd>
//!   dockctl send <id> <text>         send one message, stream the reply until done
//!   dockctl chat <id>                interactive: each stdin line is one message
//!   dockctl cancel <id>              cancel the running turn
//!   dockctl detach <id>              release a session (resume it in a terminal later)
//!   dockctl raw '<json>'             send a raw request, print the raw reply
//!
//! Options (env): AGENTDOCK_URL (default ws://127.0.0.1:7820/ws), DOCKCTL_THOUGHTS=1

use std::io::Write;

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

struct Conn {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    show_thoughts: bool,
}

impl Conn {
    async fn connect() -> anyhow::Result<Self> {
        let url =
            std::env::var("AGENTDOCK_URL").unwrap_or_else(|_| "ws://127.0.0.1:7820/ws".into());
        let (ws, _) = connect_async(&url)
            .await
            .with_context(|| format!("connect {url}"))?;
        Ok(Self {
            ws,
            next_id: 1,
            show_thoughts: std::env::var("DOCKCTL_THOUGHTS").is_ok_and(|v| v == "1"),
        })
    }

    async fn send(&mut self, mut req: Value) -> anyhow::Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        req["id"] = json!(id);
        self.ws.send(Message::Text(req.to_string().into())).await?;
        Ok(id)
    }

    async fn recv(&mut self) -> anyhow::Result<Value> {
        while let Some(msg) = self.ws.next().await {
            if let Message::Text(t) = msg? {
                return Ok(serde_json::from_str(t.as_str())?);
            }
        }
        bail!("connection closed")
    }

    /// Send a request; print events for `session` while waiting; return the reply.
    async fn call(&mut self, req: Value, session: Option<&str>) -> anyhow::Result<Value> {
        let id = self.send(req).await?;
        loop {
            let m = self.recv().await?;
            if m["id"] == json!(id) {
                if m["type"] == "error" {
                    bail!("[{}] {}", m["code"].as_str().unwrap_or("?"), m["message"]);
                }
                return Ok(m);
            }
            if session.is_some_and(|s| m["sessionId"] == s) {
                self.print_event(&m);
            }
        }
    }

    /// Send one prompt and stream events until prompt.done for this session.
    async fn send_prompt(&mut self, session: &str, text: &str) -> anyhow::Result<()> {
        self.call(
            json!({"type": "prompt.send", "sessionId": session, "text": text}),
            Some(session),
        )
        .await?;
        loop {
            let m = self.recv().await?;
            if m["sessionId"] != session {
                continue;
            }
            self.print_event(&m);
            if m["type"] == "prompt.done" || m["type"] == "session.closed" {
                return Ok(());
            }
        }
    }

    fn print_event(&self, m: &Value) {
        let replay = if m["replay"] == true { "[回放]" } else { "" };
        match m["type"].as_str().unwrap_or("") {
            "message.chunk" => match m["role"].as_str() {
                Some("assistant") => {
                    print!("{}", m["text"].as_str().unwrap_or(""));
                    let _ = std::io::stdout().flush();
                }
                Some("thought") if self.show_thoughts => {
                    print!("\x1b[2m{}\x1b[0m", m["text"].as_str().unwrap_or(""));
                }
                Some("user") => println!("\n{replay}[用户] {}", m["text"].as_str().unwrap_or("")),
                _ => {}
            },
            "tool.call" => println!(
                "\n  {replay}[工具 {}] {} {}{}",
                m["status"]
                    .as_str()
                    .unwrap_or(m["phase"].as_str().unwrap_or("")),
                m["title"].as_str().unwrap_or(""),
                if m["rawInput"].is_null() {
                    String::new()
                } else {
                    m["rawInput"].to_string()
                },
                if m["output"].as_str().is_some_and(|o| !o.is_empty()) {
                    format!(" → {}", m["output"].as_str().unwrap_or("").trim_end())
                } else {
                    String::new()
                }
            ),
            "permission.autoRejected" => println!(
                "\n  [审批] 已自动拒绝（S2 不支持远程审批）：{}",
                m["title"].as_str().unwrap_or("")
            ),
            "prompt.done" => match m["error"].as_str() {
                Some(e) => println!("\n[结束] 出错：{e}"),
                None => println!("\n[结束] stopReason={}", m["stopReason"]),
            },
            "session.closed" => println!("\n[会话关闭] reason={}", m["reason"]),
            _ => {}
        }
    }
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

const USAGE: &str = "usage: dockctl sessions | view <id> [limit] | attached | attach <id> | new <cwd> \
| send <id> <text> | chat <id> | cancel <id> | detach <id> | raw '<json>'";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str);
    let mut c = Conn::connect().await?;
    match (arg(0), args.len()) {
        (Some("sessions"), 1) => {
            print_sessions(&c.call(json!({"type": "sessions.list"}), None).await?)
        }
        (Some("view"), 2 | 3) => {
            let limit = arg(2)
                .map(str::parse::<usize>)
                .transpose()
                .context("limit must be a number")?;
            print_view(
                &c.call(
                    json!({"type": "session.view", "sessionId": args[1], "limit": limit}),
                    None,
                )
                .await?,
            )
        }
        (Some("attached"), 1) => {
            let r = c.call(json!({"type": "attached.list"}), None).await?;
            for s in r["sessions"].as_array().into_iter().flatten() {
                println!(
                    "{}  pid={}  running={}  cwd={}",
                    s["sessionId"].as_str().unwrap_or("-"),
                    s["pid"],
                    s["promptRunning"],
                    s["cwd"].as_str().unwrap_or("-")
                );
            }
            println!(
                "共 {} 个已接管",
                r["sessions"].as_array().map_or(0, Vec::len)
            );
        }
        (Some("attach"), 2) => {
            let r = c
                .call(
                    json!({"type": "session.attach", "sessionId": args[1]}),
                    Some(&args[1]),
                )
                .await?;
            println!(
                "\n[接管成功] {} alreadyAttached={} attempts={}",
                args[1], r["alreadyAttached"], r["attempts"]
            );
        }
        (Some("new"), 2) => {
            let r = c
                .call(json!({"type": "session.new", "cwd": args[1]}), None)
                .await?;
            println!("[新会话] {}", r["sessionId"].as_str().unwrap_or("-"));
        }
        (Some("send"), 3) => c.send_prompt(&args[1], &args[2]).await?,
        (Some("chat"), 2) => {
            println!(
                "已连接 {}，每行一条消息，Ctrl-D 退出（会话保持接管状态）",
                args[1]
            );
            let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
            while let Some(line) = lines.next_line().await? {
                if !line.trim().is_empty() {
                    c.send_prompt(&args[1], &line).await?;
                }
            }
        }
        (Some("cancel"), 2) => {
            c.call(json!({"type": "prompt.cancel", "sessionId": args[1]}), None)
                .await?;
            println!("已请求取消");
        }
        (Some("detach"), 2) => {
            c.call(
                json!({"type": "session.detach", "sessionId": args[1]}),
                None,
            )
            .await?;
            println!(
                "已断开 {}，可在终端执行：kiro-cli --resume-id {}",
                args[1], args[1]
            );
        }
        (Some("raw"), 2) => {
            let req: Value = serde_json::from_str(&args[1]).context("raw json")?;
            let id = c.send(req).await?;
            loop {
                let m = c.recv().await?;
                if m["id"] == json!(id) {
                    println!("{}", serde_json::to_string_pretty(&m)?);
                    break;
                }
            }
        }
        _ => bail!(USAGE),
    }
    Ok(())
}
