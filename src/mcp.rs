//! `hive mcp`: a minimal stdio MCP server spawned by each agent's `claude` process.
//! Every tool call is forwarded to the daemon, tagged with the agent and its current task.

use crate::ipc::{self, Request};
use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::Path;

fn tools() -> Value {
    json!([
        {
            "name": "list_agents",
            "description": "Lists the team: name, role, capabilities and state (available / busy / queued).",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "delegate_task",
            "description": "Creates a sub-task of your current task and hands it to a teammate. They don't see your conversation: the description must be self-contained (context, expectations, definition of done). You will be resumed once all your sub-tasks are closed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": {"type": "string", "description": "Agent name (see list_agents)"},
                    "title": {"type": "string", "description": "Titre court"},
                    "description": {"type": "string", "description": "Complete, self-contained description"}
                },
                "required": ["to", "title", "description"]
            }
        },
        {
            "name": "ask_agent",
            "description": "Asks a teammate a question (including the one who gave you the task) when you lack a detail or an opinion they are best placed to give. They answer without being able to delegate; you will be resumed with their answer (end your turn). Previous exchanges are recalled to them: don't ask an already answered question again. Too many back-and-forths trigger a human arbitration.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": {"type": "string", "description": "Agent name"},
                    "question": {"type": "string", "description": "Precise, self-contained question, with the useful context"}
                },
                "required": ["to", "question"]
            }
        },
        {
            "name": "propose_agent",
            "description": "Proposes hiring a new agent when the team lacks a needed capability (recruiters only, ceo and cto by default). A human admin accepts or rejects; the task waits for the decision (end your turn).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Short identifier: letters, digits, - and _"},
                    "title": {"type": "string", "description": "Job title"},
                    "reports_to": {"type": "string", "description": "Agent they will report to"},
                    "capabilities": {"type": "array", "items": {"type": "string"}},
                    "profiles": {"type": "array", "items": {"type": "string"}, "description": "Tool profiles: read, web, edit, dev…"},
                    "model": {"type": "string", "description": "haiku, sonnet or opus"},
                    "prompt": {"type": "string", "description": "Description of the role, its responsibilities and rules"},
                    "reason": {"type": "string", "description": "Why the current team is not enough"}
                },
                "required": ["name", "title", "capabilities", "prompt", "reason"]
            }
        },
        {
            "name": "complete_task",
            "description": "Marks your current task as done and sends the result to whoever requested it.",
            "inputSchema": {
                "type": "object",
                "properties": {"result": {"type": "string", "description": "Complete, self-contained result: deliverables, file paths, decisions, open points"}},
                "required": ["result"]
            }
        },
        {
            "name": "fail_task",
            "description": "Marks your current task as impossible, with the reason.",
            "inputSchema": {
                "type": "object",
                "properties": {"reason": {"type": "string"}},
                "required": ["reason"]
            }
        },
        {
            "name": "ask_owner",
            "description": "Asks the human owner a question (decision, access, missing information). The task becomes \"blocked\" until they answer; end your turn right after.",
            "inputSchema": {
                "type": "object",
                "properties": {"question": {"type": "string"}},
                "required": ["question"]
            }
        },
        {
            "name": "set_status",
            "description": "Declares your availability and a note visible to the team and the owner.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "availability": {"type": "string", "enum": ["available", "busy"]},
                    "note": {"type": "string"}
                },
                "required": ["availability"]
            }
        },
        {
            "name": "approve",
            "description": "Internal to the orchestrator (permission approvals): never call it yourself.",
            "inputSchema": {
                "type": "object",
                "properties": {"tool_name": {"type": "string"}, "input": {"type": "object"}, "tool_use_id": {"type": "string"}},
                "required": ["tool_name", "input"]
            }
        },
        {
            "name": "task_info",
            "description": "Details of a task (your current task by default): description, result, sub-tasks and their state.",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "integer"}}
            }
        }
    ])
}

pub fn serve(socket: &Path, agent: &str, task: i64) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue }; // notification: no reply
        let method = msg["method"].as_str().unwrap_or("");
        let result: Result<Value, (i64, String)> = match method {
            "initialize" => Ok(json!({
                "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "hive", "version": env!("CARGO_PKG_VERSION")}
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => {
                let name = msg["params"]["name"].as_str().unwrap_or("").to_string();
                let args = msg["params"].get("arguments").cloned().unwrap_or(json!({}));
                let req = if name == "approve" {
                    Request::Approve {
                        agent: agent.to_string(),
                        task_id: task,
                        tool_name: args["tool_name"].as_str().unwrap_or("").to_string(),
                        input: args.get("input").cloned().unwrap_or(json!({})),
                    }
                } else {
                    Request::Tool { agent: agent.to_string(), task_id: task, name, args }
                };
                let (text, is_error) = match ipc::call(socket, &req) {
                    Ok(r) => (r.text, !r.ok),
                    Err(e) => (format!("{e:#}"), true),
                };
                Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
            }
            _ => Err((-32601, format!("unknown method: {method}"))),
        };
        let resp = match result {
            Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
            Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        };
        writeln!(stdout, "{resp}")?;
        stdout.flush()?;
    }
    Ok(())
}
