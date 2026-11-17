#!/usr/bin/env python3
"""S0 ACP probe: a minimal Agent Client Protocol client for kiro-cli.

Only uses the Python standard library. Every JSON-RPC message sent/received
is appended to spike/logs/<test>_<timestamp>.jsonl for later analysis.

Usage:
  acp_probe.py t1                 # new session, permission reject then allow
  acp_probe.py t2 <session_id>    # load existing session, record replay
  acp_probe.py t3 <session_id>    # load + prompt (while terminal also has it open)
"""
import json
import os
import queue
import subprocess
import sys
import threading
import time

ROOT = os.path.dirname(os.path.abspath(__file__))
LOG_DIR = os.path.join(ROOT, "logs")
WORKDIR = os.path.join(ROOT, "workdir")
PROMPT_ECHO = "请调用 shell 工具执行命令 `echo hello`，然后只回复命令输出。不要调用其他工具。"


class AcpClient:
    def __init__(self, test_name, permission_decisions):
        os.makedirs(LOG_DIR, exist_ok=True)
        ts = time.strftime("%Y%m%d_%H%M%S")
        self.log_path = os.path.join(LOG_DIR, f"{test_name}_{ts}.jsonl")
        self.log = open(self.log_path, "a", encoding="utf-8")
        # decisions consumed in order for each permission request: "allow" / "reject"
        self.decisions = list(permission_decisions)
        self.next_id = 1
        self.pending = {}
        self.updates = []
        self.permission_requests = []
        self.lock = threading.Lock()
        self.proc = subprocess.Popen(
            ["kiro-cli", "acp"],
            cwd=WORKDIR,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=open(os.path.join(LOG_DIR, f"{test_name}_{ts}.stderr.log"), "w"),
            text=True,
            bufsize=1,
        )
        threading.Thread(target=self._reader, daemon=True).start()

    def _write_log(self, direction, msg):
        rec = {"t": round(time.time(), 3), "dir": direction, "msg": msg}
        with self.lock:
            self.log.write(json.dumps(rec, ensure_ascii=False) + "\n")
            self.log.flush()

    def _send(self, msg):
        self._write_log("out", msg)
        self.proc.stdin.write(json.dumps(msg, ensure_ascii=False) + "\n")
        self.proc.stdin.flush()

    def _reader(self):
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                self._write_log("in_raw", line)
                continue
            self._write_log("in", msg)
            self._dispatch(msg)

    def _dispatch(self, msg):
        if "method" in msg and "id" in msg:  # request from agent
            self._handle_agent_request(msg)
        elif "method" in msg:  # notification
            if msg["method"] == "session/update":
                self.updates.append(msg["params"])
        elif "id" in msg:  # response to our request
            q = self.pending.pop(msg["id"], None)
            if q:
                q.put(msg)

    def _handle_agent_request(self, msg):
        method = msg["method"]
        if method == "session/request_permission":
            params = msg["params"]
            self.permission_requests.append(params)
            options = params.get("options", [])
            decision = self.decisions.pop(0) if self.decisions else "reject"
            want = "allow_once" if decision == "allow" else "reject_once"
            chosen = next((o for o in options if o.get("kind") == want), None)
            if chosen is None:
                chosen = next((o for o in options if o.get("kind", "").startswith(decision[:5])), None)
            if chosen is None:
                result = {"outcome": {"outcome": "cancelled"}}
            else:
                result = {"outcome": {"outcome": "selected", "optionId": chosen["optionId"]}}
            print(f"[permission] decision={decision} -> {result}")
            self._send({"jsonrpc": "2.0", "id": msg["id"], "result": result})
        else:
            # We declare no fs/terminal capability; refuse anything else.
            self._send({"jsonrpc": "2.0", "id": msg["id"],
                        "error": {"code": -32601, "message": f"method not supported: {method}"}})

    def request(self, method, params, timeout=180):
        rid = self.next_id
        self.next_id += 1
        q = queue.Queue()
        self.pending[rid] = q
        self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        try:
            return q.get(timeout=timeout)
        except queue.Empty:
            return {"error": {"message": f"timeout after {timeout}s"}}

    def initialize(self):
        return self.request("initialize", {
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {"readTextFile": False, "writeTextFile": False}, "terminal": False},
        }, timeout=30)

    def close(self):
        try:
            self.proc.stdin.close()
            self.proc.wait(timeout=10)
        except Exception:
            self.proc.kill()
        self.log.close()


def summarize_updates(updates):
    counts = {}
    for u in updates:
        kind = u.get("update", {}).get("sessionUpdate", "?")
        counts[kind] = counts.get(kind, 0) + 1
    return counts


def agent_text(updates):
    return "".join(
        u["update"].get("content", {}).get("text", "")
        for u in updates
        if u.get("update", {}).get("sessionUpdate") == "agent_message_chunk"
    )


def run_t1():
    c = AcpClient("t1", ["reject", "allow"])
    print("init:", json.dumps(c.initialize().get("result", {}), ensure_ascii=False)[:300])
    r = c.request("session/new", {"cwd": WORKDIR, "mcpServers": []}, timeout=60)
    sid = r.get("result", {}).get("sessionId")
    print("session/new:", r)
    if not sid:
        c.close(); return
    with open(os.path.join(LOG_DIR, "last_session.txt"), "w") as f:
        f.write(sid)
    for round_no in (1, 2):
        before = len(c.updates)
        r = c.request("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": PROMPT_ECHO}]})
        new = c.updates[before:]
        print(f"\n== round {round_no} stopReason={r.get('result', r)}")
        print("   updates:", summarize_updates(new))
        print("   agent text:", agent_text(new)[:300])
    print("\npermission requests received:", len(c.permission_requests))
    c.close()
    print("log:", c.log_path, "\nsession:", sid)


def assert_spike_session(sid):
    """Safety guard: refuse to touch any session not created under spike/workdir."""
    meta_path = os.path.expanduser(f"~/.kiro/sessions/cli/{sid}.json")
    try:
        with open(meta_path, encoding="utf-8") as f:
            cwd = json.load(f).get("cwd")
    except (OSError, json.JSONDecodeError) as e:
        sys.exit(f"[guard] cannot read session meta {meta_path}: {e}")
    if cwd != WORKDIR:
        sys.exit(f"[guard] refuse: session {sid} cwd={cwd!r} is not {WORKDIR!r}")


def run_load(test_name, sid, send_prompt):
    assert_spike_session(sid)
    c = AcpClient(test_name, ["allow"])
    c.initialize()
    t0 = time.time()
    r = c.request("session/load", {"sessionId": sid, "cwd": WORKDIR, "mcpServers": []}, timeout=60)
    time.sleep(2)  # allow trailing replay notifications
    print(f"session/load ({time.time() - t0:.1f}s):", json.dumps(r, ensure_ascii=False)[:300])
    print("replayed updates:", summarize_updates(c.updates))
    print("replayed agent text:", agent_text(c.updates)[:500])
    if send_prompt:
        before = len(c.updates)
        r = c.request("session/prompt", {"sessionId": sid, "prompt": [{"type": "text",
                      "text": "这是来自 ACP 的消息，请只回复：ACP-OK"}]})
        print("prompt result:", r.get("result", r))
        print("agent text:", agent_text(c.updates[before:])[:300])
    c.close()
    print("log:", c.log_path)


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__); sys.exit(1)
    cmd = sys.argv[1]
    if cmd == "t1":
        run_t1()
    elif cmd in ("t2", "t3") and len(sys.argv) == 3:
        run_load(cmd, sys.argv[2], send_prompt=(cmd == "t3"))
    else:
        print(__doc__); sys.exit(1)
