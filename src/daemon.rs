//! The orchestrator: owns the DB, schedules agent runs, serves the CLI / MCP bridge over a Unix socket.

use crate::config::{log_path, socket_path, Company};
use crate::db::{Db, Task};
use crate::ipc::{Request, Response};
use crate::util::trunc;
use crate::{access, agents, approval, collab, notify, runner, tools, usage};
use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, Notify};

pub struct RunHandle {
    pub agent: String,
    pub task_id: i64,
    pub kill: Option<oneshot::Sender<()>>,
}

pub struct App {
    pub company: Company,
    pub db: Db,
    /// Poked whenever something may be schedulable.
    pub wake: Notify,
    pub running: Mutex<HashMap<i64, RunHandle>>,
    pub shutting_down: AtomicBool,
    pub exe: PathBuf,
    pub socket: PathBuf,
    pub http: reqwest::Client,
    /// Recent activity, shown by the dashboard.
    pub events: Mutex<VecDeque<(String, String)>>,
    pub approvals: Mutex<HashMap<i64, approval::Pending>>,
    pub next_approval: AtomicI64,
}

impl App {
    /// Prints an activity line and keeps it for the dashboard.
    pub fn log(&self, text: &str) {
        println!("{text}");
        let mut ev = self.events.lock().unwrap();
        ev.push_back((chrono::Local::now().format("%H:%M:%S").to_string(), text.to_string()));
        while ev.len() > 200 {
            ev.pop_front();
        }
    }
}

pub async fn run(company: Company) -> Result<()> {
    let state = company.state_dir();
    std::fs::create_dir_all(state.join("logs"))?;
    std::fs::create_dir_all(state.join("runs"))?;
    for a in &company.agents() {
        std::fs::create_dir_all(company.workdir(a))?;
    }

    let socket = socket_path(&company.root);
    if socket.exists() {
        if UnixStream::connect(&socket).await.is_ok() {
            bail!("a daemon is already running ({})", socket.display());
        }
        std::fs::remove_file(&socket)?;
    }

    let db = Db::open(&state.join("hive.db"))?;
    let app = Arc::new(App {
        company,
        db,
        wake: Notify::new(),
        running: Mutex::new(HashMap::new()),
        shutting_down: AtomicBool::new(false),
        exe: std::env::current_exe()?,
        socket: socket.clone(),
        http: reqwest::Client::new(),
        events: Mutex::new(VecDeque::new()),
        approvals: Mutex::new(HashMap::new()),
        next_approval: AtomicI64::new(1),
    });
    tools::seed(&app)?;

    // `[slack] allowed_users` bootstraps the admins; afterwards users are managed live.
    if let Some(slack) = &app.company.cfg.slack {
        for id in &slack.allowed_users {
            if app.db.user(id)?.is_none() {
                app.db.set_user(&access::User { slack_id: id.clone(), name: None, role: access::Role::Admin, agents: vec![] })?;
            }
        }
    }

    for (agent, task_id) in app.db.recover_stale_runs()? {
        if let Some(t) = app.db.task(task_id)? {
            if !t.is_closed() && t.status != "blocked" {
                app.db.add_inbox(&agent, task_id, "resume", RESUME_MSG)?;
            }
        }
    }

    let listener = UnixListener::bind(&socket)?;
    println!(
        "🐝 hive \"{}\" — {} agents, max {} parallel runs — socket {}",
        app.company.cfg.name,
        app.company.agents().len(),
        app.company.cfg.max_parallel_runs,
        socket.display()
    );

    {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((s, _)) => {
                        let app = app.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_conn(app, s).await {
                                eprintln!("connection: {e:#}");
                            }
                        });
                    }
                    Err(e) => eprintln!("accept: {e}"),
                }
            }
        });
    }

    if app.company.cfg.slack.is_some() {
        tokio::spawn(notify::slack_loop(app.clone()));
    }
    tokio::spawn(usage::poll_loop(app.clone()));

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        if let Err(e) = schedule(&app) {
            eprintln!("scheduler: {e:#}");
        }
        tokio::select! {
            _ = app.wake.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            _ = tokio::signal::ctrl_c() => break,
            _ = sigterm.recv() => break,
        }
    }

    // Graceful stop: kill running claude processes; their runners requeue a "resume" message.
    println!("stopping…");
    app.shutting_down.store(true, Ordering::SeqCst);
    for h in app.running.lock().unwrap().values_mut() {
        if let Some(k) = h.kill.take() {
            let _ = k.send(());
        }
    }
    for _ in 0..50 {
        if app.running.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

pub const RESUME_MSG: &str =
    "Your previous session on this task was interrupted (orchestrator restart). Check where you were and carry on.";

/// Start a run for every (agent, task) that has pending messages, within the concurrency limits.
fn schedule(app: &Arc<App>) -> Result<()> {
    let max = app.company.cfg.max_parallel_runs.max(1);
    for (agent, task_id) in app.db.pending_groups()? {
        let (total, per_agent, same) = {
            let r = app.running.lock().unwrap();
            (
                r.len(),
                r.values().filter(|h| h.agent == agent).count(),
                r.values().any(|h| h.agent == agent && h.task_id == task_id),
            )
        };
        if total >= max {
            break;
        }
        let Some(acfg) = app.company.agent(&agent) else {
            eprintln!("unknown agent '{agent}' (task #{task_id}): messages dropped");
            app.db.drop_inbox(task_id)?;
            continue;
        };
        if same || per_agent >= acfg.max_concurrent.max(1) {
            continue;
        }
        if usage::paused_until(app, &acfg.provider)?.is_some() {
            continue; // quota of this provider exhausted until its reset
        }
        let Some(task) = app.db.task(task_id)? else {
            app.db.drop_inbox(task_id)?;
            continue;
        };
        if task.is_closed() {
            app.db.drop_inbox(task_id)?;
            continue;
        }
        if task.status == "blocked" {
            continue; // waiting for the owner's answer
        }
        // Don't wake an agent for each finished sub-task: wait until they are all closed.
        let pending = app.db.pending(&agent, task_id)?;
        if app.db.open_children(task_id)? > 0 && pending.iter().all(|i| i.kind.starts_with("child_")) {
            continue;
        }

        let run_id = app.db.create_run(&agent, task_id)?;
        let items = app.db.take_inbox(&agent, task_id, run_id)?;
        let (tx, rx) = oneshot::channel();
        app.running
            .lock()
            .unwrap()
            .insert(run_id, RunHandle { agent: agent.clone(), task_id, kill: Some(tx) });
        app.log(&format!("▶ run {run_id}: {agent} on #{task_id} \"{}\"", trunc(&task.title, 60)));
        tokio::spawn(runner::run(app.clone(), run_id, acfg, task, items, rx));
    }
    Ok(())
}

async fn handle_conn(app: Arc<App>, s: UnixStream) -> Result<()> {
    let (r, mut w) = s.into_split();
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).await?;
    let resp = match serde_json::from_str::<Request>(&line) {
        Ok(req) => match handle(&app, req).await {
            Ok(text) => Response { ok: true, text },
            Err(e) => Response { ok: false, text: format!("{e:#}") },
        },
        Err(e) => Response { ok: false, text: format!("invalid request: {e}") },
    };
    w.write_all((serde_json::to_string(&resp)? + "\n").as_bytes()).await?;
    Ok(())
}

pub async fn handle(app: &Arc<App>, req: Request) -> Result<String> {
    match req {
        Request::Ping => Ok("pong".into()),
        Request::CreateTask { to, title, description } => {
            let (id, to) = create_owner_task(app, to, &title, &description, None)?;
            notify::announce(app, id).await;
            Ok(format!("Task #{id} assigned to {to}"))
        }
        Request::Status => status_text(app),
        Request::Tasks { all } => tasks_text(app, all),
        Request::Show { id } => show_text(app, id),
        Request::Reply { id, message } => {
            let t = task_or_err(app, id)?;
            if t.is_closed() {
                return reopen(app, &t, &message).await;
            }
            app.db.add_inbox(
                &t.assignee,
                id,
                "owner",
                &format!("Message from the owner about task #{id}:\n{message}"),
            )?;
            if t.status == "blocked" {
                app.db.set_status(id, "in_progress", None)?;
            }
            app.wake.notify_one();
            Ok(format!("Message delivered to {} (task #{id})", t.assignee))
        }
        Request::Cancel { id } => {
            let n = cancel_tree(app, id)?;
            Ok(format!("{n} task(s) cancelled"))
        }
        Request::Retry { id } => {
            let t = task_or_err(app, id)?;
            if !matches!(t.status.as_str(), "failed" | "cancelled") {
                bail!("task #{id} is '{}': only failed/cancelled tasks can be retried", t.status);
            }
            app.db.set_status(id, "todo", None)?;
            app.db.add_inbox(
                &t.assignee,
                id,
                "retry",
                &format!(
                    "The owner asks you to take this task up again. Previous outcome ({}):\n{}",
                    t.status,
                    t.result.as_deref().unwrap_or("(none)")
                ),
            )?;
            app.wake.notify_one();
            Ok(format!("Task #{id} retried ({})", t.assignee))
        }
        Request::Usage => usage::text(app),
        Request::Snapshot => snapshot(app),
        Request::ReportUsage { report } => {
            let r: usage::Report = serde_json::from_value(report)?;
            usage::record(app, &r, "push")?;
            app.wake.notify_one();
            Ok(format!("usage of {} recorded ({} window(s))", r.provider, r.windows.len()))
        }
        Request::Users => access::users_text(app),
        Request::UserSet { slack_id, role, name, agents } => {
            let u = access::User {
                slack_id: access::parse_slack_id(&slack_id)?,
                name,
                role: access::Role::parse(&role)?,
                agents,
            };
            access::set_user(app, &u)
        }
        Request::UserRemove { slack_id } => access::remove_user(app, &access::parse_slack_id(&slack_id)?),
        Request::Proposals { all } => collab::proposals_text(app, all),
        Request::ProposalDecide { id, accept, note } => {
            collab::decide_proposal(app, id, accept, "owner (CLI)", note.as_deref()).await
        }
        Request::AgentList => Ok(agents::list_text(app)),
        Request::AgentShow { name } => agents::show(app, &name),
        Request::AgentAdd { spec } => agents::add(app, spec),
        Request::AgentRemove { name } => agents::remove(app, &name),
        Request::AgentReload => agents::reload(app),
        Request::Tools { args } => tools::command(app, &args),
        Request::Approvals => Ok(approval::list_text(app)),
        Request::Decide { id, decision } => approval::decide(app, id, approval::Decision::parse(&decision)?, "owner (CLI)"),
        Request::Approve { agent, task_id, tool_name, input } => {
            approval::request(app, &agent, task_id, &tool_name, input).await
        }
        Request::Tool { agent, task_id, name, args } => tool(app, &agent, task_id, &name, &args).await,
    }
}

/// A message on a closed root task reopens it: its agent resumes the same session with the new
/// request and its previous result; the new result is reported like the first one.
async fn reopen(app: &Arc<App>, t: &Task, message: &str) -> Result<String> {
    let id = t.id;
    if t.kind == "question" {
        bail!("#{id} is a question between agents: it cannot be reopened");
    }
    if let Some(p) = t.parent_id {
        let root = app.db.root_of(p)?;
        bail!("#{id} is a sub-task: reopen the main task #{root} instead (its agent will restart what is needed)");
    }
    app.db.set_status(id, "todo", None)?;
    app.db.add_inbox(
        &t.assignee,
        id,
        "reopen",
        &format!(
            "🔁 Task #{id} is reopened (it was '{}'). New request from the owner:\n{message}\n\nYour previous result:\n{}",
            t.status,
            t.result.as_deref().unwrap_or("(none)")
        ),
    )?;
    app.wake.notify_one();
    app.log(&format!("🔁 #{id} reopened ({})", t.assignee));
    notify::thread_event(app, id, &format!("🔁 #{id} reopened, {} picks it up with your request", t.assignee)).await;
    Ok(format!("🔁 Task #{id} reopened: {} picks it up with your request (`cancel` to cancel).", t.assignee))
}

fn task_or_err(app: &App, id: i64) -> Result<Task> {
    app.db.task(id)?.ok_or_else(|| anyhow!("task #{id} not found"))
}

/// Agent a new owner task goes to (`to`, or the default agent), checked to exist.
pub fn owner_target(app: &App, to: Option<String>) -> Result<String> {
    let to = to.unwrap_or_else(|| app.company.cfg.default_agent.clone());
    if app.company.agent(&to).is_none() {
        bail!("unknown agent '{to}'");
    }
    Ok(to)
}

/// Root task from the owner (CLI, Slack). `requested_by`: Slack user id. Returns (id, assignee).
pub fn create_owner_task(
    app: &Arc<App>,
    to: Option<String>,
    title: &str,
    description: &str,
    requested_by: Option<&str>,
) -> Result<(i64, String)> {
    let to = owner_target(app, to)?;
    let by = match requested_by {
        Some(id) => format!("owner ({})", app.db.user(id)?.map(|u| u.label()).unwrap_or_else(|| id.to_string())),
        None => "owner".into(),
    };
    let id = create_task(app, None, title, description, &to, &by, 0, requested_by)?;
    Ok((id, to))
}

#[allow(clippy::too_many_arguments)]
fn create_task(
    app: &Arc<App>,
    parent: Option<&Task>,
    title: &str,
    description: &str,
    to: &str,
    by: &str,
    depth: i64,
    requested_by: Option<&str>,
) -> Result<i64> {
    let id = app.db.create_task(parent.map(|p| p.id), title, description, to, by, depth, requested_by)?;
    app.log(&format!("📋 #{id} {by} → {to}: {}", trunc(title, 60)));
    let mut body = format!("# Task #{id}: {title}\nRequested by: {by}");
    if let Some(p) = parent {
        body += &format!(" (sub-task of #{} \"{}\")", p.id, p.title);
    }
    body += &format!("\n\n{description}");
    app.db.add_inbox(to, id, "assigned", &body)?;
    app.wake.notify_one();
    if parent.is_some() {
        let (app, text) = (app.clone(), format!("↳ {by} delegates #{id} to {to}: {title}"));
        tokio::spawn(async move { notify::thread_event(&app, id, &text).await });
    }
    Ok(id)
}

pub async fn complete(app: &Arc<App>, id: i64, result: &str) -> Result<()> {
    let t = task_or_err(app, id)?;
    if t.is_closed() {
        return Ok(());
    }
    app.db.set_status(id, "done", Some(result))?;
    app.log(&format!("✅ #{id} done by {}", t.assignee));
    match t.parent_id.map(|p| app.db.task(p)).transpose()?.flatten() {
        Some(p) if !p.is_closed() => {
            app.db.add_inbox(
                &p.assignee,
                p.id,
                "child_done",
                &if t.kind == "question" {
                    format!("💬 Answer from {} to your question #{id}:\n{result}", t.assignee)
                } else {
                    format!("✅ Sub-task #{id} \"{}\" done by {}.\nResult:\n{result}", t.title, t.assignee)
                },
            )?;
            app.wake.notify_one();
            let ev = if t.kind == "question" {
                format!("💬 {} answered (#{id}): {}", t.assignee, trunc(result, 300))
            } else {
                format!("✓ #{id} done by {}", t.assignee)
            };
            notify::thread_event(app, id, &ev).await;
        }
        Some(_) => {}
        None => {
            notify::owner(app, id, &format!("✅ #{id} done by {} — {}\n\n{result}", t.assignee, t.title)).await;
        }
    }
    Ok(())
}

pub async fn fail(app: &Arc<App>, id: i64, reason: &str) -> Result<()> {
    let t = task_or_err(app, id)?;
    if t.is_closed() {
        return Ok(());
    }
    app.db.set_status(id, "failed", Some(reason))?;
    app.log(&format!("❌ #{id} failed ({})", t.assignee));
    match t.parent_id.map(|p| app.db.task(p)).transpose()?.flatten() {
        Some(p) if !p.is_closed() => {
            app.db.add_inbox(
                &p.assignee,
                p.id,
                "child_failed",
                &if t.kind == "question" {
                    format!("❔ {} could not answer your question #{id}: {reason}", t.assignee)
                } else {
                    format!("❌ Sub-task #{id} \"{}\" failed ({}).\nReason:\n{reason}", t.title, t.assignee)
                },
            )?;
            app.wake.notify_one();
            notify::thread_event(app, id, &format!("✗ #{id} failed ({}): {}", t.assignee, trunc(reason, 300))).await;
        }
        Some(_) => {}
        None => {
            notify::owner(
                app,
                id,
                &format!("❌ #{id} failed ({}) — {}\n\n{reason}\n\n→ hive retry {id}", t.assignee, t.title),
            )
            .await;
        }
    }
    Ok(())
}

fn cancel_tree(app: &App, id: i64) -> Result<usize> {
    let root = task_or_err(app, id)?;
    let mut ids = HashSet::new();
    let mut stack = vec![id];
    while let Some(i) = stack.pop() {
        if ids.insert(i) {
            stack.extend(app.db.children(i)?.into_iter().map(|c| c.id));
        }
    }
    let mut n = 0;
    for &i in &ids {
        if let Some(t) = app.db.task(i)? {
            if !t.is_closed() {
                app.db.set_status(i, "cancelled", None)?;
                n += 1;
            }
        }
        app.db.drop_inbox(i)?;
    }
    for h in app.running.lock().unwrap().values_mut() {
        if ids.contains(&h.task_id) {
            if let Some(k) = h.kill.take() {
                let _ = k.send(());
            }
        }
    }
    // Let the parent know, so it doesn't wait forever.
    if let Some(p) = root.parent_id.map(|p| app.db.task(p)).transpose()?.flatten() {
        if !p.is_closed() {
            app.db.add_inbox(
                &p.assignee,
                p.id,
                "child_failed",
                &format!("🚫 Sub-task #{id} \"{}\" cancelled by the owner.", root.title),
            )?;
        }
    }
    app.wake.notify_one();
    Ok(n)
}

/// Assignees of the task and all its ancestors.
fn chain_assignees(app: &App, mut t: Task) -> Result<Vec<String>> {
    let mut out = vec![t.assignee.clone()];
    while let Some(p) = t.parent_id.map(|p| app.db.task(p)).transpose()?.flatten() {
        out.push(p.assignee.clone());
        t = p;
    }
    Ok(out)
}

fn agent_states(app: &App) -> Result<HashMap<String, String>> {
    let mut running: HashMap<String, Vec<i64>> = HashMap::new();
    for h in app.running.lock().unwrap().values() {
        running.entry(h.agent.clone()).or_default().push(h.task_id);
    }
    let pending = app.db.pending_counts()?;
    let notes = app.db.notes()?;
    let waiting = approval::waiting_agents(app);
    let mut out = HashMap::new();
    for a in &app.company.agents() {
        let s = if waiting.contains(&a.name) {
            "waiting for approval".to_string()
        } else if let Some(ts) = running.get(&a.name) {
            let ts: Vec<String> = ts.iter().map(|t| format!("#{t}")).collect();
            format!("busy ({})", ts.join(", "))
        } else if pending.get(&a.name).copied().unwrap_or(0) > 0 {
            match usage::paused_until(app, &a.provider)? {
                Some(u) => format!("paused (quota → {})", usage::fmt_time(u)),
                None => "queued".into(),
            }
        } else if notes.get(&a.name).is_some_and(|(av, _)| av == "busy") {
            "busy (declared)".into()
        } else {
            "available".into()
        };
        out.insert(a.name.clone(), s);
    }
    Ok(out)
}

fn status_text(app: &App) -> Result<String> {
    let states = agent_states(app)?;
    let notes = app.db.notes()?;
    let n_running = app.running.lock().unwrap().len();
    let mut s = format!(
        "🐝 {} — runs: {}/{}\n\n{:<14} {:<26} {}\n",
        app.company.cfg.name, n_running, app.company.cfg.max_parallel_runs, "AGENT", "STATE", "NOTE"
    );
    for a in &app.company.agents() {
        let note = notes.get(&a.name).map(|(_, n)| trunc(n, 50)).unwrap_or_default();
        s += &format!("{:<14} {:<26} {}\n", a.name, states[&a.name], note);
    }
    let open = app.db.tasks(false)?;
    s += &format!("\n{} open task(s) — `hive tasks`\n\n{}", open.len(), usage::text(app)?);
    Ok(s)
}

fn tasks_text(app: &App, all: bool) -> Result<String> {
    let tasks = app.db.tasks(all)?;
    if tasks.is_empty() {
        return Ok("No tasks.".into());
    }
    let ids: HashSet<i64> = tasks.iter().map(|t| t.id).collect();
    let mut kids: HashMap<i64, Vec<&Task>> = HashMap::new();
    let mut roots = Vec::new();
    for t in &tasks {
        match t.parent_id {
            Some(p) if ids.contains(&p) => kids.entry(p).or_default().push(t),
            _ => roots.push(t),
        }
    }
    fn walk(t: &Task, lvl: usize, kids: &HashMap<i64, Vec<&Task>>, out: &mut String) {
        out.push_str(&format!(
            "{}{}#{} [{}] {} — {}\n",
            "  ".repeat(lvl),
            if t.kind == "question" { "❓" } else { "" },
            t.id,
            t.status,
            t.assignee,
            trunc(&t.title, 70)
        ));
        if let Some(ks) = kids.get(&t.id) {
            for k in ks {
                walk(k, lvl + 1, kids, out);
            }
        }
    }
    let mut out = String::new();
    for r in roots {
        walk(r, 0, &kids, &mut out);
    }
    Ok(out.trim_end().to_string())
}

fn show_text(app: &App, id: i64) -> Result<String> {
    let t = task_or_err(app, id)?;
    let mut s = format!(
        "#{} {}\nstatus: {} · assigned to: {} · requested by: {}{}\ncreated: {} · updated: {}\n\n## Description\n{}",
        t.id,
        t.title,
        t.status,
        t.assignee,
        t.created_by,
        t.parent_id.map(|p| format!(" · parent: #{p}")).unwrap_or_default(),
        t.created_at,
        t.updated_at,
        t.description
    );
    if let Some(r) = &t.result {
        s += &format!("\n\n## Result\n{r}");
    }
    let kids = app.db.children(id)?;
    if !kids.is_empty() {
        s += "\n\n## Sub-tasks";
        for k in kids {
            if k.kind == "question" {
                s += &format!(
                    "\n❓#{} [{}] {} → {}: {}{}",
                    k.id,
                    k.status,
                    k.created_by,
                    k.assignee,
                    trunc(&k.description, 120),
                    k.result.as_deref().map(|r| format!("\n   ↳ {}", trunc(r, 300))).unwrap_or_default()
                );
            } else {
                s += &format!("\n#{} [{}] {} — {}", k.id, k.status, k.assignee, trunc(&k.title, 70));
            }
        }
    }
    let runs = app.db.runs_for_task(id)?;
    if !runs.is_empty() {
        s += "\n\n## Runs";
        for r in runs {
            s += &format!(
                "\nrun {} · {} · {} · {} → {}{} · {}",
                r.id,
                r.agent,
                r.status,
                r.started_at,
                r.ended_at.as_deref().unwrap_or("…"),
                r.cost_usd.map(|c| format!(" · ${c:.3}")).unwrap_or_default(),
                log_path(&app.company.root, r.id, &r.agent).display()
            );
        }
    }
    Ok(s)
}

fn arg<'a>(args: &'a Value, k: &str) -> Result<&'a str> {
    args.get(k)
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("missing parameter '{k}'"))
}

/// Tools exposed to agents (see `mcp.rs` for their schemas).
async fn tool(app: &Arc<App>, agent: &str, task_id: i64, name: &str, args: &Value) -> Result<String> {
    let t = task_or_err(app, task_id)?;
    match name {
        "list_agents" => {
            let states = agent_states(app)?;
            let notes = app.db.notes()?;
            let mut s = String::new();
            for a in &app.company.agents() {
                let me = if a.name == agent { " (you)" } else { "" };
                s += &format!(
                    "- {}{me} — {}{} — {}\n  capabilities: {}\n",
                    a.name,
                    a.title,
                    a.reports_to.as_ref().map(|r| format!(", reports to {r}")).unwrap_or_default(),
                    states[&a.name],
                    a.capabilities.join(", ")
                );
                if let Some((_, n)) = notes.get(&a.name).filter(|(_, n)| !n.is_empty()) {
                    s += &format!("  note: {n}\n");
                }
            }
            Ok(s)
        }
        "ask_agent" => collab::ask_agent(app, agent, &t, arg(args, "to")?, arg(args, "question")?).await,
        "propose_agent" => collab::propose_agent(app, agent, &t, args).await,
        "delegate_task" => {
            if t.kind == "question" {
                bail!("you are answering a question: no delegation here, answer with what you know (complete_task)");
            }
            if t.is_closed() {
                bail!("task #{task_id} is already closed ({})", t.status);
            }
            let to = arg(args, "to")?;
            let title = arg(args, "title")?;
            let description = arg(args, "description")?;
            let target = app
                .company
                .agent(to)
                .ok_or_else(|| anyhow!("unknown agent '{to}' — use list_agents"))?;
            if to == agent {
                bail!("you cannot delegate a task to yourself: do it directly");
            }
            let max = app.company.cfg.max_delegation_depth;
            if t.depth + 1 > max {
                bail!("max delegation depth reached ({max}): do it yourself or report back");
            }
            if chain_assignees(app, t.clone())?.iter().any(|a| a == to) {
                bail!("{to} is already upstream in this task chain: report back with complete_task / fail_task (or ask_agent) instead of delegating to them");
            }
            let id = create_task(app, Some(&t), title, description, to, agent, t.depth + 1, t.requested_by.as_deref())?;
            Ok(format!(
                "Sub-task #{id} assigned to {to} ({}). When you have nothing else to do, end your turn: you will be resumed automatically once your sub-tasks are done.",
                target.title
            ))
        }
        "complete_task" => {
            let result = arg(args, "result")?;
            if app.db.open_children(task_id)? > 0 {
                bail!("sub-tasks of #{task_id} are still open: wait for their results (end your turn) or cancel them before closing");
            }
            complete(app, task_id, result).await?;
            Ok(format!("Task #{task_id} done, result sent to {}. You can end your turn.", t.created_by))
        }
        "fail_task" => {
            let reason = arg(args, "reason")?;
            fail(app, task_id, reason).await?;
            Ok(format!("Task #{task_id} marked as failed. You can end your turn."))
        }
        "ask_owner" => {
            let q = arg(args, "question")?;
            app.db.set_status(task_id, "blocked", None)?;
            notify::owner(
                app,
                task_id,
                &format!("❓ #{task_id} — {agent} asks:\n{q}\n\n→ hive reply {task_id} \"…\""),
            )
            .await;
            Ok("Question sent to the owner. End your turn now: you will be resumed with their answer.".into())
        }
        "set_status" => {
            let availability = arg(args, "availability")?;
            if !matches!(availability, "available" | "busy") {
                bail!("availability must be 'available' or 'busy'");
            }
            let note = args.get("note").and_then(|v| v.as_str()).unwrap_or("");
            app.db.set_note(agent, availability, note)?;
            Ok("Status updated.".into())
        }
        "task_info" => {
            let id = args.get("id").and_then(|v| v.as_i64()).unwrap_or(task_id);
            show_text(app, id)
        }
        _ => bail!("unknown tool '{name}'"),
    }
}

fn snapshot(app: &App) -> Result<String> {
    let states = agent_states(app)?;
    let notes = app.db.notes()?;
    let agents: Vec<Value> = app
        .company
        .agents()
        .iter()
        .map(|a| {
            serde_json::json!({
                "name": a.name, "title": a.title, "provider": a.provider,
                "state": states[&a.name],
                "note": notes.get(&a.name).map(|(_, n)| n.clone()).unwrap_or_default(),
            })
        })
        .collect();
    let mut paused = serde_json::Map::new();
    let windows = app.db.usage(None)?;
    for w in &windows {
        if !paused.contains_key(&w.provider) {
            paused.insert(w.provider.clone(), serde_json::json!(usage::paused_until(app, &w.provider)?));
        }
    }
    let events: Vec<(String, String)> = app.events.lock().unwrap().iter().cloned().collect();
    let v = serde_json::json!({
        "company": app.company.cfg.name,
        "running": app.running.lock().unwrap().len(),
        "max_parallel": app.company.cfg.max_parallel_runs,
        "agents": agents,
        "tasks": app.db.tasks_for_dashboard(30)?,
        "usage": windows,
        "paused": paused,
        "events": events,
        "approvals": approval::snapshot(app),
        "proposals": app.db.proposals(true)?.iter().map(|p| serde_json::json!({
            "id": p.id, "proposed_by": p.proposed_by,
            "name": serde_json::from_str::<Value>(&p.spec).ok().and_then(|v| v["name"].as_str().map(String::from)),
        })).collect::<Vec<_>>(),
    });
    Ok(v.to_string())
}
