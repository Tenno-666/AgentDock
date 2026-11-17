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
//!   dockctl approvals                list pending approvals
//!   dockctl approve <approvalId> <optionId>   answer an approval (e.g. from another terminal)
//!   dockctl todo [open|done|all]     list todos (default open)
//!   dockctl todo add <title> [high|medium|low] [YYYY-MM-DD]
//!   dockctl todo done|reopen|rm <n>  update / delete todo #n
//!   dockctl todo watch               print todo.changed events
//!   dockctl raw '<json>'             send a raw request, print the raw reply
//!
//! During send/chat a permission card is shown and the answer is read from stdin:
//!   y = allow_once, a = allow_always, n = reject_once, empty line = keep waiting
//!   (answer elsewhere with `dockctl approve`).
//! Options (env): AGENTDOCK_URL (default ws://127.0.0.1:7820/ws), DOCKCTL_THOUGHTS=1

use std::io::Write;

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader, Lines, Stdin};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type Input = Lines<BufReader<Stdin>>;

struct Conn {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    show_thoughts: bool,
}

fn option_for_key(key: &str) -> Option<&'static str> {
    match key.trim() {
        "y" | "Y" => Some("allow_once"),
        "a" | "A" => Some("allow_always"),
        "n" | "N" => Some("reject_once"),
        _ => None,
    }
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

    /// Ask on stdin; returns once answered here, or when someone else answered.
    async fn handle_approval(&mut self, req: &Value, input: &mut Input) -> anyhow::Result<()> {
        let approval_id = req["approvalId"].as_str().unwrap_or_default().to_string();
        loop {
            print!("  选择 [y]允许一次 [a]总是允许 [n]拒绝，回车=暂不处理 > ");
            let _ = std::io::stdout().flush();
            let line = input.next_line().await?.unwrap_or_default();
            let Some(option) = option_for_key(&line) else {
                println!(
                    "  （保持等待，可在其他终端执行：dockctl approve {approval_id} <optionId>）"
                );
                return Ok(());
            };
            let r = self
                .send(json!({"type": "permission.reply", "approvalId": approval_id, "optionId": option}))
                .await?;
            loop {
                let m = self.recv().await?;
                if m["id"] == json!(r) {
                    if m["type"] == "error" {
                        println!(
                            "  回复失败：[{}] {}",
                            m["code"].as_str().unwrap_or("?"),
                            m["message"]
                        );
                        if m["code"] == "APPROVAL_NOT_FOUND" {
                            return Ok(());
                        }
                        break; // ask again
                    }
                    return Ok(());
                }
                self.print_event(&m);
            }
        }
    }

    /// Send one prompt and stream events until prompt.done for this session.
    async fn send_prompt(
        &mut self,
        session: &str,
        text: &str,
        input: &mut Input,
    ) -> anyhow::Result<()> {
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
            match m["type"].as_str() {
                Some("permission.request") => self.handle_approval(&m, input).await?,
                Some("prompt.done" | "session.closed") => return Ok(()),
                _ => {}
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
            "permission.request" => print_card(m),
            "permission.resolved" => println!(
                "  [审批结果] {} → {}（{}）",
                m["approvalId"].as_str().unwrap_or("-"),
                m["optionId"]
                    .as_str()
                    .unwrap_or(m["outcome"].as_str().unwrap_or("-")),
                m["reason"].as_str().unwrap_or("-")
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

fn print_card(m: &Value) {
    println!(
        "\n  ┌─ 审批请求 {} ─ 会话 {}",
        m["approvalId"].as_str().unwrap_or("-"),
        m["sessionId"].as_str().unwrap_or("-")
    );
    println!(
        "  │ 工具：{} ({})",
        m["toolName"].as_str().unwrap_or("?"),
        m["kind"].as_str().unwrap_or("?")
    );
    println!("  │ 标题：{}", m["title"].as_str().unwrap_or("-"));
    if m["inputUnknown"] == true {
        println!("  │ ⚠ 未能获取完整参数，请谨慎批准");
    } else {
        let pretty = serde_json::to_string_pretty(&m["rawInput"]).unwrap_or_default();
        for line in pretty.lines() {
            println!("  │ {line}");
        }
    }
    let opts: Vec<&str> = m["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| o["optionId"].as_str())
        .collect();
    println!("  └─ 可选：{}", opts.join(" / "));
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
| send <id> <text> | chat <id> | cancel <id> | detach <id> | approvals | approve <approvalId> <optionId> \
| todo [open|done|all|add <title> [priority] [YYYY-MM-DD]|done <n>|reopen <n>|rm <n>|watch] | raw '<json>'";

fn print_todo(t: &Value) {
    let mark = if t["status"] == "done" { "x" } else { " " };
    let mut extra = Vec::new();
    if let Some(p) = t["priority"].as_str() {
        extra.push(format!("优先级:{p}"));
    }
    if let Some(d) = t["dueDate"].as_str() {
        extra.push(format!("截止:{d}"));
    }
    match t["source"].as_str() {
        Some("auto") => extra.push(format!(
            "来源:{} {}",
            t["agent"].as_str().unwrap_or("?"),
            t["sessionId"].as_str().unwrap_or("-")
        )),
        _ => extra.push("手动".into()),
    }
    println!(
        "[{mark}] #{:<4} {}  ({})",
        t["id"],
        t["title"].as_str().unwrap_or(""),
        extra.join("，")
    );
}

async fn todo_cmd(c: &mut Conn, args: &[String]) -> anyhow::Result<()> {
    let id = |s: Option<&String>| -> anyhow::Result<i64> {
        s.context("missing todo id")?
            .trim_start_matches('#')
            .parse()
            .context("todo id must be a number")
    };
    let sub = args.first().map(String::as_str).unwrap_or("open");
    match sub {
        "open" | "done" | "all" if args.len() <= 1 => {
            let r = c
                .call(json!({"type": "todo.list", "status": sub}), None)
                .await?;
            let list = r["todos"].as_array().cloned().unwrap_or_default();
            for t in &list {
                print_todo(t);
            }
            println!("共 {} 条（{sub}）", list.len());
        }
        "add" if (2..=4).contains(&args.len()) => {
            let mut req = json!({"type": "todo.add", "title": args[1]});
            for a in &args[2..] {
                if a.len() == 10 && a.as_bytes()[4] == b'-' {
                    req["dueDate"] = json!(a);
                } else {
                    req["priority"] = json!(a);
                }
            }
            let r = c.call(req, None).await?;
            print_todo(&r["todo"]);
        }
        "done" | "reopen" if args.len() == 2 => {
            let status = if sub == "done" { "done" } else { "open" };
            let r = c
                .call(
                    json!({"type": "todo.update", "todoId": id(args.get(1))?, "status": status}),
                    None,
                )
                .await?;
            print_todo(&r["todo"]);
        }
        "rm" if args.len() == 2 => {
            let todo_id = id(args.get(1))?;
            c.call(json!({"type": "todo.delete", "todoId": todo_id}), None)
                .await?;
            println!("已删除 #{todo_id}");
        }
        "watch" if args.len() == 1 => {
            println!("监听待办变化，Ctrl-C 退出");
            loop {
                let m = c.recv().await?;
                if m["type"] == "todo.changed" {
                    print!("[{}] ", m["action"].as_str().unwrap_or("?"));
                    if m["todo"].is_object() {
                        print_todo(&m["todo"]);
                    } else {
                        println!("#{}", m["todoId"]);
                    }
                }
            }
        }
        _ => bail!(USAGE),
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str);
    let mut input: Input = BufReader::new(tokio::io::stdin()).lines();
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
        (Some("send"), 3) => c.send_prompt(&args[1], &args[2], &mut input).await?,
        (Some("chat"), 2) => {
            println!(
                "已连接 {}，每行一条消息，Ctrl-D 退出（会话保持接管状态）",
                args[1]
            );
            while let Some(line) = input.next_line().await? {
                if !line.trim().is_empty() {
                    c.send_prompt(&args[1], &line, &mut input).await?;
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
        (Some("approvals"), 1) => {
            let r = c.call(json!({"type": "permission.list"}), None).await?;
            let list = r["approvals"].as_array().cloned().unwrap_or_default();
            for a in &list {
                print_card(a);
            }
            println!("共 {} 个待审批", list.len());
        }
        (Some("approve"), 3) => {
            let r = c
                .call(
                    json!({"type": "permission.reply", "approvalId": args[1], "optionId": args[2]}),
                    None,
                )
                .await?;
            println!("已回复 {} → {}", r["approvalId"], r["optionId"]);
        }
        (Some("todo"), _) => todo_cmd(&mut c, &args[1..]).await?,
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
