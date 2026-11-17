#!/usr/bin/env python3
"""Fake ACP agent for integration tests. Mimics kiro-cli 2.13.0 behaviour seen in S0.

Session id suffix controls session/load:
  ...b  -> lock error "Session is active in another process"
  ...e  -> never answers (timeout)
  other -> replays one user + one assistant chunk, then succeeds
Prompt text controls session/prompt:
  contains "tool" -> tool_call + session/request_permission, result depends on the answer
  contains "slow" -> waits for session/cancel
  otherwise       -> echoes the text
"""
import json
import os
import sys

counter = 0
pending = None  # (prompt request id, session id)


def send(m):
    m["jsonrpc"] = "2.0"
    sys.stdout.write(json.dumps(m) + "\n")
    sys.stdout.flush()


def update(sid, u):
    send({"method": "session/update", "params": {"sessionId": sid, "update": u}})


def chunk(sid, kind, text):
    update(sid, {"sessionUpdate": kind, "content": {"type": "text", "text": text}})


for line in sys.stdin:
    m = json.loads(line)
    method, mid, p = m.get("method"), m.get("id"), m.get("params") or {}
    if method == "initialize":
        send({"id": mid, "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": True}}})
    elif method == "session/new":
        counter += 1
        # unique across agent processes, like real UUIDs
        sid = "%08x-0000-0000-0000-%012d" % (os.getpid() & 0xFFFFFFFF, counter)
        send({"id": mid, "result": {"sessionId": sid}})
    elif method == "session/load":
        sid = p["sessionId"]
        if sid.endswith("b"):
            send({"id": mid, "error": {"code": -32603, "message": "Internal error",
                  "data": "Failed to start session: Session is active in another process (PID 42)"}})
        elif sid.endswith("e"):
            pass
        else:
            chunk(sid, "user_message_chunk", "old question")
            chunk(sid, "agent_message_chunk", "old answer")
            send({"id": mid, "result": None})
    elif method == "session/prompt":
        sid = p["sessionId"]
        text = "".join(x.get("text", "") for x in p["prompt"])
        chunk(sid, "agent_message_chunk", "echo: " + text)
        if "tool" in text:
            update(sid, {"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Running: echo hello",
                         "kind": "execute", "rawInput": {"command": "echo hello"}})
            send({"id": "perm-1", "method": "session/request_permission", "params": {
                "sessionId": sid, "toolCall": {"toolCallId": "t1", "title": "Running: echo hello"},
                "options": [{"optionId": "allow_once", "name": "Yes", "kind": "allow_once"},
                            {"optionId": "allow_always", "name": "Always", "kind": "allow_always"},
                            {"optionId": "reject_once", "name": "No", "kind": "reject_once"}]}})
            pending = (mid, sid)
        elif "slow" in text:
            pending = (mid, sid)
        else:
            send({"id": mid, "result": {"stopReason": "end_turn"}})
    elif method is None and mid == "perm-1":
        if pending is None:  # turn already cancelled; late answer is ignored
            continue
        prompt_id, sid = pending
        outcome = m["result"]["outcome"]
        opt = outcome.get("optionId") if outcome.get("outcome") == "selected" else "cancelled"
        update(sid, {"sessionUpdate": "tool_call_update", "toolCallId": "t1",
                     "status": "completed" if opt == "allow_once" else "failed"})
        chunk(sid, "agent_message_chunk", "perm: %s" % opt)
        send({"id": prompt_id, "result": {"stopReason": "end_turn"}})
        pending = None
    elif method == "session/cancel":
        if pending:
            send({"id": pending[0], "result": {"stopReason": "cancelled"}})
            pending = None
    elif method and mid is not None:
        send({"id": mid, "error": {"code": -32601, "message": "unsupported"}})
