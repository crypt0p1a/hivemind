//! One run = one `claude -p` process for one (agent, task), resuming that pair's session.

use crate::config::{log_path, AgentConfig, Company};
use crate::daemon::{complete, fail, App, RESUME_MSG};
use crate::db::{InboxItem, Task};
use crate::util::trunc;
use crate::{approval, collab, notify, tools, usage};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;

#[derive(Default)]
struct Outcome {
    killed: bool,
    exit_ok: bool,
    is_error: bool,
    result: Option<String>,
    session_id: Option<String>,
    cost: Option<f64>,
    stderr: String,
}

pub async fn run(
    app: Arc<App>,
    run_id: i64,
    agent: AgentConfig,
    task: Task,
    items: Vec<InboxItem>,
    kill_rx: oneshot::Receiver<()>,
) {
    let outcome = execute(&app, run_id, &agent, &task, &items, kill_rx).await;
    app.running.lock().unwrap().remove(&run_id);
    approval::drop_for(&app, &agent.name, task.id);
    if let Err(e) = finish(&app, run_id, &agent, task.id, outcome).await {
        eprintln!("run {run_id}: {e:#}");
    }
    app.wake.notify_one();
}

async fn execute(
    app: &App,
    run_id: i64,
    agent: &AgentConfig,
    task: &Task,
    items: &[InboxItem],
    mut kill_rx: oneshot::Receiver<()>,
) -> Result<Outcome> {
    if matches!(task.status.as_str(), "todo" | "waiting") {
        app.db.set_status(task.id, "in_progress", None)?;
    }
    let session = app.db.session(&agent.name, task.id)?;
    let prompt = build_prompt(task, items, session.is_some());
    let system = system_prompt(&app.company, agent);

    let mcp_path = app.company.state_dir().join("runs").join(format!("run-{run_id}.mcp.json"));
    let mcp = json!({ "mcpServers": { "hive": {
        "type": "stdio",
        "command": app.exe,
        "args": ["mcp", "--socket", app.socket, "--agent", agent.name, "--task", task.id.to_string()],
    }}});
    std::fs::write(&mcp_path, serde_json::to_vec_pretty(&mcp)?)?;

    // Rules are read from the DB at every run: changes apply without restart.
    let eff = tools::effective(app, &agent.name)?;
    let mut allow = eff.allow.clone();
    allow.push("mcp__hive".into());

    let mut cmd = Command::new(&app.company.cfg.claude_bin);
    cmd.arg("-p")
        .args(["--output-format", "stream-json", "--verbose"])
        .arg("--append-system-prompt")
        .arg(&system)
        .arg("--mcp-config")
        .arg(&mcp_path)
        .arg("--strict-mcp-config")
        .arg("--permission-mode")
        .arg(&agent.permission_mode);
    if let Some(m) = &agent.model {
        cmd.arg("--model").arg(m);
    }
    if let Some(s) = &session {
        cmd.arg("--resume").arg(s);
    } else if task.kind == "question" {
        // Answer with the context of the agent's own (ancestor) task, without touching that session.
        if let Some(s) = collab::ancestor_session(app, &agent.name, task)? {
            cmd.arg("--resume").arg(s).arg("--fork-session");
        }
    }
    // One argv per rule: rules may contain spaces ("Bash(git push *)").
    cmd.arg("--allowedTools").args(&allow);
    if !eff.deny.is_empty() {
        cmd.arg("--disallowedTools").args(&eff.deny);
    }
    let approvals = &app.company.cfg.tools;
    if approvals.approvals {
        cmd.arg("--permission-prompt-tool").arg("mcp__hive__approve");
        // The approval may take a while: don't let claude time the MCP call out first.
        cmd.env("MCP_TOOL_TIMEOUT", ((approvals.approval_timeout_secs + 120) * 1000).to_string());
    }
    cmd.current_dir(app.company.workdir(agent))
        .env_remove("CLAUDECODE")
        .env("HIVE_AGENT", &agent.name)
        .env("HIVE_TASK", task.id.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().with_context(|| format!("starting '{}'", app.company.cfg.claude_bin))?;

    // Prompt on stdin: no argv length limit, no leading-dash ambiguity.
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(prompt.as_bytes()).await?;
    }
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let stderr_task = tokio::spawn(async move {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s).await;
        s
    });

    let mut log = tokio::fs::File::create(log_path(&app.company.root, run_id, &agent.name)).await?;
    let header = json!({"type": "hive", "run": run_id, "agent": agent.name, "task": task.id, "prompt": prompt});
    log.write_all(format!("{header}\n").as_bytes()).await?;

    let mut out = Outcome::default();
    let mut lines = BufReader::new(stdout).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => match line? {
                Some(l) => {
                    log.write_all(l.as_bytes()).await?;
                    log.write_all(b"\n").await?;
                    if let Ok(v) = serde_json::from_str::<Value>(&l) {
                        if let Some(s) = v.get("session_id").and_then(Value::as_str) {
                            out.session_id = Some(s.to_string());
                        }
                        if v["type"] == "rate_limit_event" {
                            if let Some(r) = usage::from_claude_event(&v) {
                                if let Err(e) = usage::record(app, &r, "claude-run") {
                                    eprintln!("usage: {e:#}");
                                }
                            }
                        }
                        if v["type"] == "result" {
                            out.result = v["result"].as_str().map(String::from);
                            out.cost = v["total_cost_usd"].as_f64();
                            out.is_error = v["is_error"].as_bool().unwrap_or(false);
                        }
                    }
                }
                None => break,
            },
            _ = &mut kill_rx => {
                out.killed = true;
                let _ = child.kill().await;
                break;
            }
        }
    }
    let status = child.wait().await?;
    out.exit_ok = status.success();
    out.stderr = stderr_task.await.unwrap_or_default();
    if !out.stderr.trim().is_empty() {
        let l = json!({"type": "hive_stderr", "text": out.stderr});
        log.write_all(format!("{l}\n").as_bytes()).await?;
    }
    Ok(out)
}

async fn finish(
    app: &Arc<App>,
    run_id: i64,
    agent: &AgentConfig,
    task_id: i64,
    outcome: Result<Outcome>,
) -> Result<()> {
    let o = match outcome {
        Ok(o) => o,
        Err(e) => {
            app.db.finish_run(run_id, "error", None)?;
            return fail(app, task_id, &format!("orchestrator error: {e:#}")).await;
        }
    };
    if let Some(s) = &o.session_id {
        app.db.set_session(&agent.name, task_id, s)?;
    }
    let shutting_down = app.shutting_down.load(Ordering::SeqCst);
    let failed = !o.killed && (!o.exit_ok || o.is_error);
    let status = match () {
        _ if shutting_down => "aborted",
        _ if o.killed => "killed",
        _ if failed => "error",
        _ => "ok",
    };
    app.db.finish_run(run_id, status, o.cost)?;
    app.log(&format!("■ run {run_id} ({}): {status}", agent.name));

    let Some(t) = app.db.task(task_id)? else { return Ok(()) };
    if t.is_closed() || t.status == "blocked" {
        return Ok(()); // the agent closed it / asked the owner through the tools
    }
    if shutting_down {
        app.db.add_inbox(&agent.name, task_id, "resume", RESUME_MSG)?;
        return Ok(());
    }
    if o.killed {
        return Ok(());
    }
    if failed {
        // Quota exhausted: not the agent's fault — requeue, the scheduler resumes after the reset.
        if let Some(until) = usage::paused_until(app, &agent.provider)? {
            app.db.add_inbox(&agent.name, task_id, "resume", RESUME_MSG)?;
            let msg = format!(
                "⏸ #{task_id} ({}): {} quota reached, automatic resume around {}",
                agent.name,
                agent.provider,
                usage::fmt_time(until)
            );
            app.log(&msg);
            notify::thread_event(app, task_id, &msg).await;
            return Ok(());
        }
        let detail = o.result.filter(|r| !r.trim().is_empty()).unwrap_or_else(|| trunc(o.stderr.trim(), 1500));
        return fail(app, task_id, &format!("run {run_id} of {} failed: {detail}", agent.name)).await;
    }
    if app.db.open_children(task_id)? > 0 {
        app.db.set_status(task_id, "waiting", None)?;
        return Ok(());
    }
    if !app.db.pending(&agent.name, task_id)?.is_empty() {
        return Ok(()); // new messages arrived during the run: it will be resumed
    }
    // The agent ended its turn without closing the task: its last answer is the result.
    let result = o.result.unwrap_or_else(|| "(no result text)".into());
    complete(app, task_id, &result).await
}

fn build_prompt(task: &Task, items: &[InboxItem], resumed: bool) -> String {
    let mut s = String::new();
    if resumed {
        s += &format!("Updates on your task #{} \"{}\":\n\n", task.id, task.title);
    } else if !items.iter().any(|i| i.kind == "assigned") {
        // Fresh session without the assignment message (e.g. retry after a lost session).
        s += &format!(
            "# Task #{}: {}\nRequested by: {}\n\n{}\n\n---\n\n",
            task.id, task.title, task.created_by, task.description
        );
    }
    s += &items.iter().map(|i| i.body.as_str()).collect::<Vec<_>>().join("\n\n---\n\n");
    s += "\n\n(Reminder: when the task is done, call complete_task with a complete result; otherwise fail_task, ask_owner, ask_agent or delegate_task.)";
    s
}

fn system_prompt(company: &Company, agent: &AgentConfig) -> String {
    let cfg = &company.cfg;
    let mut s = format!("You are **{}**, {} at the company \"{}\".\n", agent.name, agent.title, cfg.name);
    if !cfg.mission.trim().is_empty() {
        s += &format!("Company mission: {}\n", cfg.mission.trim());
    }
    if let Some(r) = &agent.reports_to {
        s += &format!("You report to {r}.\n");
    }
    if !agent.prompt.trim().is_empty() {
        s += &format!("\n## Your role\n{}\n", agent.prompt.trim());
    }
    s += "\n## The team\n";
    for a in &company.agents() {
        s += &format!(
            "- **{}** — {}{} · capabilities: {}\n",
            a.name,
            a.title,
            a.reports_to.as_ref().map(|r| format!(" (reports to {r})")).unwrap_or_default(),
            a.capabilities.join(", ")
        );
    }
    s += &format!(
        "
## How you work (Hive orchestrator)
You are an autonomous agent. Each session is about ONE task (#N, given in the message). You have the `hive` MCP tools:
- `list_agents`: the team, everyone's capabilities and who is busy.
- `delegate_task`: hand a sub-task to the teammate whose capabilities fit best. The description must stand on its own: they don't see your conversation.
- `complete_task`: when your task is done, with a complete, self-contained result (deliverables, file paths, decisions). This is what your requester receives.
- `fail_task`: if the task is impossible.
- `ask_agent`: ask a teammate a question (even the one who gave you the task) when they are best placed to answer.
- `ask_owner`: for a decision or information only the owner (the human) can give.
- `task_info`, `set_status`.{recruit}
After delegating or asking a question (to an agent or the owner), end your turn: you will be resumed automatically with the results. Don't wait and don't poll.
Don't do work that clearly belongs to a better-suited teammate, and don't delegate trivial things.
Your working directory is {} (it may be shared with other agents).
",
        company.workdir(agent).display(),
        recruit = if company.cfg.collaboration.recruiters.contains(&agent.name) {
            "\n- `propose_agent`: propose hiring an agent when a capability is really missing (an admin decides)."
        } else {
            ""
        }
    );
    s
}
