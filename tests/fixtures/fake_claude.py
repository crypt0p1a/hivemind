#!/usr/bin/env python3
"""A scriptable stand-in for `claude -p`, used by the end-to-end tests.

hive starts it exactly like the real CLI: prompt on stdin, flags on the command line, stream-json on
stdout. Its behaviour comes from a plan (JSON file in $HIVE_FAKE_PLAN):

    {"ceo": [[action, ...], [action, ...]], "dev": [[action, ...]]}

one list of actions per run of the agent, in order (run 1, run 2, …). After the last planned run,
an agent answers "ok from <agent>". Actions:

    {"tool": "delegate_task", "args": {...}}   call a hive MCP tool ("{task}" is replaced by the task id)
    {"approve": {"tool_name": "Bash", "input": {...}}}   what claude does for a non-allowed tool
    {"rate_limit": {"five_hour": {"utilization": 1.0, "resets_in": 3600, "status": "rejected"}}}
    {"sleep": 30}
    {"fail": "message"}                         error result, exit code 1
    {"result": "text"}                          final answer (default: "ok from <agent>")

Every invocation is appended to $HIVE_FAKE_STATE/calls.jsonl (agent, task, args, prompt) and every
tool answer to $HIVE_FAKE_STATE/results.jsonl, for the tests to check.
"""
import json
import os
import subprocess
import sys
import time
import uuid

STATE = os.environ["HIVE_FAKE_STATE"]
AGENT = os.environ.get("HIVE_AGENT", "?")
TASK = os.environ.get("HIVE_TASK", "0")


def flag_values(argv, name):
    """Values of a flag; variadic flags take everything up to the next --flag."""
    out = []
    for i, a in enumerate(argv):
        if a == name:
            j = i + 1
            while j < len(argv) and not argv[j].startswith("--"):
                out.append(argv[j])
                j += 1
    return out


def append(name, value):
    with open(os.path.join(STATE, name), "a") as f:
        f.write(json.dumps(value) + "\n")


def runs_so_far():
    try:
        with open(os.path.join(STATE, "calls.jsonl")) as f:
            return sum(1 for line in f if json.loads(line)["agent"] == AGENT)
    except FileNotFoundError:
        return 0


def emit(obj):
    print(json.dumps(obj), flush=True)


def substitute(v):
    if isinstance(v, str):
        return v.replace("{task}", TASK)
    if isinstance(v, list):
        return [substitute(x) for x in v]
    if isinstance(v, dict):
        return {k: substitute(x) for k, x in v.items()}
    return v


def mcp_call(cfg, name, arguments):
    server = cfg["mcpServers"]["hive"]
    p = subprocess.Popen([server["command"], *server["args"]], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)

    def send(msg):
        p.stdin.write(json.dumps(msg) + "\n")
        p.stdin.flush()

    def wait(id_):
        for line in p.stdout:
            msg = json.loads(line)
            if msg.get("id") == id_:
                return msg
        raise RuntimeError("MCP server closed")

    send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}})
    wait(1)
    send({"jsonrpc": "2.0", "method": "notifications/initialized"})
    send({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": name, "arguments": arguments}})
    res = wait(2)["result"]
    p.stdin.close()
    p.wait()
    return {"is_error": res["isError"], "text": res["content"][0]["text"]}


def main():
    argv = sys.argv[1:]
    prompt = sys.stdin.read()
    index = runs_so_far()
    append("calls.jsonl", {"agent": AGENT, "task": int(TASK), "run": index + 1, "args": argv, "prompt": prompt})

    resume = flag_values(argv, "--resume")
    session = resume[0] if resume and "--fork-session" not in argv else str(uuid.uuid4())
    emit({"type": "system", "subtype": "init", "session_id": session, "model": "fake"})

    with open(os.environ["HIVE_FAKE_PLAN"]) as f:
        plan = json.load(f).get(AGENT, [])
    actions = plan[index] if index < len(plan) else []
    with open(flag_values(argv, "--mcp-config")[0]) as f:
        cfg = json.load(f)

    result = f"ok from {AGENT}"
    for action in actions:
        if "tool" in action:
            r = mcp_call(cfg, action["tool"], substitute(action.get("args", {})))
            append("results.jsonl", {"agent": AGENT, "task": int(TASK), "tool": action["tool"], **r})
        elif "approve" in action:
            r = mcp_call(cfg, "approve", substitute(action["approve"]))
            append("results.jsonl", {"agent": AGENT, "task": int(TASK), "tool": "approve", **r})
        elif "rate_limit" in action:
            windows = {}
            for name, w in action["rate_limit"].items():
                w = dict(w)
                w["resetsAt"] = int(time.time()) + w.pop("resets_in", 3600)
                windows[name] = w
            emit({"type": "rate_limit_event", "rate_limit_info": {"unifiedWindows": windows}, "session_id": session})
        elif "sleep" in action:
            time.sleep(action["sleep"])
        elif "fail" in action:
            emit({"type": "result", "subtype": "error", "is_error": True, "result": action["fail"], "session_id": session})
            sys.exit(1)
        elif "result" in action:
            result = action["result"]

    emit({"type": "assistant", "message": {"content": [{"type": "text", "text": result}]}, "session_id": session})
    emit({"type": "result", "subtype": "success", "is_error": False, "result": result,
          "total_cost_usd": 0.001, "num_turns": 1, "session_id": session})


if __name__ == "__main__":
    main()
