//! Owner notifications (terminal, macOS, Slack) and the Slack bot (Socket Mode: no public URL needed).
//!
//! Every root task gets a Slack thread; its sub-tasks, results and the agents' questions are posted
//! in it, and the owner answers by replying in the thread.

use crate::access::{self, Action, Role, User};
use crate::approval::{self, Decision, Pending};
use crate::daemon::{create_owner_task, handle, owner_target, App};
use crate::ipc::Request;
use crate::util::{title_of, trunc};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// Important event about a task (done, failed, question): terminal + macOS + Slack (broadcast to the channel).
pub async fn owner(app: &App, task_id: i64, text: &str) {
    app.log(&format!("🔔 {}", trunc(text.lines().next().unwrap_or(""), 200)));
    if app.company.cfg.notify.macos {
        let _ = tokio::process::Command::new("osascript")
            .args([
                "-e",
                "on run argv",
                "-e",
                "display notification (item 2 of argv) with title (item 1 of argv)",
                "-e",
                "end run",
                &format!("Hive — {}", app.company.cfg.name),
                &trunc(text, 240),
            ])
            .status()
            .await;
    }
    if let Err(e) = post_in_thread(app, task_id, text, true).await {
        eprintln!("slack: {e:#}");
    }
}

/// Minor event (delegation, sub-task done): Slack thread only.
pub async fn thread_event(app: &App, task_id: i64, text: &str) {
    if let Err(e) = post_in_thread(app, task_id, text, false).await {
        eprintln!("slack: {e:#}");
    }
}

/// Opens the Slack thread of a new root task created outside Slack.
pub async fn announce(app: &App, task_id: i64) {
    if app.company.cfg.slack.is_none() {
        return;
    }
    if let Err(e) = ensure_thread(app, task_id).await {
        eprintln!("slack: {e:#}");
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

async fn slack_api(app: &App, method: &str, body: Value) -> Result<Value> {
    let slack = app.company.cfg.slack.as_ref().ok_or_else(|| anyhow!("slack not configured"))?;
    let token = slack.bot_token().ok_or_else(|| anyhow!("no bot token (HIVE_SLACK_BOT_TOKEN)"))?;
    let v: Value = app
        .http
        .post(format!("https://slack.com/api/{method}"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await?
        .json()
        .await?;
    if v["ok"] != true {
        bail!("{method}: {}", v["error"].as_str().unwrap_or("unknown error"));
    }
    Ok(v)
}

/// `mention`: Slack user id pinged in front of the (escaped) text.
async fn post(app: &App, thread_ts: Option<&str>, text: &str, broadcast: bool, mention: Option<&str>) -> Result<String> {
    let slack = app.company.cfg.slack.as_ref().ok_or_else(|| anyhow!("slack not configured"))?;
    let mut text = trunc(text, 3500);
    if text.ends_with('…') {
        text += "\n(more: `hive show`)";
    }
    let mut text = escape(&text);
    if let Some(m) = mention {
        text = format!("<@{m}> {text}");
    }
    let mut body = json!({"channel": slack.channel, "text": text, "unfurl_links": false});
    if let Some(ts) = thread_ts {
        body["thread_ts"] = json!(ts);
        body["reply_broadcast"] = json!(broadcast);
    }
    let v = slack_api(app, "chat.postMessage", body).await?;
    Ok(v["ts"].as_str().unwrap_or_default().to_string())
}

/// Thread of the task's root, created on first use.
async fn ensure_thread(app: &App, task_id: i64) -> Result<String> {
    let root = app.db.root_of(task_id)?;
    if let Some(ts) = app.db.thread_of(root)? {
        return Ok(ts);
    }
    let t = app.db.task(root)?.ok_or_else(|| anyhow!("task #{root} not found"))?;
    let ts = post(
        app,
        None,
        &format!("📋 #{} → {}: {}\n_Reply in this thread to talk to the team about this task._", t.id, t.assignee, t.title),
        false,
        None,
    )
    .await?;
    app.db.set_thread(root, &ts)?;
    Ok(ts)
}

async fn post_in_thread(app: &App, task_id: i64, text: &str, broadcast: bool) -> Result<()> {
    if app.company.cfg.slack.is_none() {
        return Ok(());
    }
    let ts = ensure_thread(app, task_id).await?;
    // Important events ping whoever asked for the task.
    let requester = match broadcast {
        true => app.db.task(app.db.root_of(task_id)?)?.and_then(|t| t.requested_by),
        false => None,
    };
    post(app, Some(&ts), text, broadcast, requester.as_deref()).await?;
    Ok(())
}

// ---------------------------------------------------------------- approvals

/// Asks for an approval: macOS notification + Slack message with buttons. Returns the Slack ts.
pub async fn approval_request(app: &App, p: &Pending) -> Option<String> {
    let text = format!("🔐 {} wants to use {} (task #{}, approval #{})", p.agent, p.summary, p.task_id, p.id);
    if app.company.cfg.notify.macos {
        let _ = tokio::process::Command::new("osascript")
            .args(["-e", "on run argv", "-e", "display notification (item 1 of argv) with title \"Hive — approval\"", "-e", "end run", &trunc(&text, 240)])
            .status()
            .await;
    }
    app.company.cfg.slack.as_ref()?;
    let res: Result<String> = async {
        let thread = ensure_thread(app, p.task_id).await?;
        let button = |label: &str, action: &str, style: Option<&str>| {
            let mut b = json!({"type": "button", "text": {"type": "plain_text", "text": trunc(label, 70)},
                               "action_id": action, "value": p.id.to_string()});
            if let Some(st) = style {
                b["style"] = json!(st);
            }
            b
        };
        let blocks = json!([
            {"type": "section", "text": {"type": "mrkdwn", "text": format!(
                "🔐 *{}* wants to use {}\n_Task #{} · approval #{} · admins only_",
                escape(&p.agent), escape(&p.summary), p.task_id, p.id)}},
            {"type": "actions", "elements": [
                button("Allow once", "approve_once", Some("primary")),
                button(&format!("Always: {}", p.rule), "approve_always", None),
                button("Deny", "approve_deny", Some("danger")),
            ]}
        ]);
        let slack = app.company.cfg.slack.as_ref().unwrap();
        let v = slack_api(
            app,
            "chat.postMessage",
            json!({"channel": slack.channel, "thread_ts": thread, "reply_broadcast": true,
                   "text": escape(&text), "blocks": blocks}),
        )
        .await?;
        Ok(v["ts"].as_str().unwrap_or_default().to_string())
    }
    .await;
    match res {
        Ok(ts) => Some(ts),
        Err(e) => {
            eprintln!("slack approval: {e:#}");
            None
        }
    }
}

/// Replaces the buttons by the verdict.
pub async fn approval_done(app: &App, p: &Pending, verdict: &str) {
    let (Some(slack), Some(ts)) = (&app.company.cfg.slack, &p.slack_ts) else { return };
    let text = format!("🔐 {} wanted to use {} (approval #{}) — {}", p.agent, p.summary, p.id, verdict);
    let res = slack_api(
        app,
        "chat.update",
        json!({"channel": slack.channel, "ts": ts, "text": escape(&text),
               "blocks": [{"type": "section", "text": {"type": "mrkdwn", "text": escape(&text)}}]}),
    )
    .await;
    if let Err(e) = res {
        eprintln!("slack approval: {e:#}");
    }
}

/// Hire proposal: notification + Slack message with Accept / Reject buttons. Returns the Slack ts.
pub async fn proposal_request(app: &App, p: &crate::db::Proposal, spec: &crate::agents::AgentSpec) -> Option<String> {
    let text = format!(
        "🧑‍💼 {} proposes to hire {} — {}{}\ncapabilities: {} · profiles: {} · model: {}\nwhy: {}\nrole: {}",
        p.proposed_by,
        spec.name,
        spec.title,
        spec.reports_to.as_ref().map(|r| format!(" (→ {r})")).unwrap_or_default(),
        spec.capabilities.join(", "),
        if spec.profiles.is_empty() { "none".into() } else { spec.profiles.join(", ") },
        spec.model.as_deref().unwrap_or("default"),
        trunc(&p.reason, 600),
        trunc(&spec.prompt, 800)
    );
    app.log(&format!("🔔 {}", text.lines().next().unwrap_or("")));
    if app.company.cfg.notify.macos {
        let _ = tokio::process::Command::new("osascript")
            .args(["-e", "on run argv", "-e", "display notification (item 1 of argv) with title \"Hive — hire\"", "-e", "end run", &trunc(&text, 240)])
            .status()
            .await;
    }
    let slack = app.company.cfg.slack.as_ref()?;
    let res: Result<String> = async {
        let thread = ensure_thread(app, p.task_id).await?;
        let blocks = json!([
            {"type": "section", "text": {"type": "mrkdwn", "text": format!("{}\n_Proposal #{} · decision by admins only_", escape(&text), p.id)}},
            {"type": "actions", "elements": [
                {"type": "button", "text": {"type": "plain_text", "text": "Accept"}, "style": "primary", "action_id": "proposal_accept", "value": p.id.to_string()},
                {"type": "button", "text": {"type": "plain_text", "text": "Reject"}, "style": "danger", "action_id": "proposal_reject", "value": p.id.to_string()},
            ]}
        ]);
        let v = slack_api(
            app,
            "chat.postMessage",
            json!({"channel": slack.channel, "thread_ts": thread, "reply_broadcast": true, "text": escape(&text), "blocks": blocks}),
        )
        .await?;
        Ok(v["ts"].as_str().unwrap_or_default().to_string())
    }
    .await;
    match res {
        Ok(ts) => Some(ts),
        Err(e) => {
            eprintln!("slack hire: {e:#}");
            None
        }
    }
}

pub async fn proposal_done(app: &App, p: &crate::db::Proposal, spec: &crate::agents::AgentSpec, verdict: &str) {
    let (Some(slack), Some(ts)) = (&app.company.cfg.slack, &p.slack_ts) else { return };
    let text = format!("🧑‍💼 {} proposed to hire {} ({}) — proposal #{} {verdict}", p.proposed_by, spec.name, spec.title, p.id);
    let res = slack_api(
        app,
        "chat.update",
        json!({"channel": slack.channel, "ts": ts, "text": escape(&text),
               "blocks": [{"type": "section", "text": {"type": "mrkdwn", "text": escape(&text)}}]}),
    )
    .await;
    if let Err(e) = res {
        eprintln!("slack hire: {e:#}");
    }
}

/// Button click (Socket Mode "interactive" envelope).
async fn on_interactive(app: &Arc<App>, payload: &Value) -> Result<()> {
    if payload["type"] != "block_actions" {
        return Ok(());
    }
    let user_id = payload["user"]["id"].as_str().unwrap_or("");
    for a in payload["actions"].as_array().into_iter().flatten() {
        if let Some(accept) = match a["action_id"].as_str() {
            Some("proposal_accept") => Some(true),
            Some("proposal_reject") => Some(false),
            _ => None,
        } {
            let id: i64 = a["value"].as_str().and_then(|v| v.parse().ok()).unwrap_or(0);
            let msg = match access::resolve(app, user_id)?.filter(|u| u.role == Role::Admin) {
                None => "⛔ only admins can decide on a hire".to_string(),
                Some(u) => match crate::collab::decide_proposal(app, id, accept, &u.label(), None).await {
                    Ok(_) => continue,
                    Err(e) => format!("❌ {e:#}"),
                },
            };
            let slack = app.company.cfg.slack.as_ref().unwrap();
            slack_api(app, "chat.postEphemeral", json!({"channel": slack.channel, "user": user_id, "text": msg})).await?;
            continue;
        }
        let decision = match a["action_id"].as_str() {
            Some("approve_once") => Decision::Once,
            Some("approve_always") => Decision::Always,
            Some("approve_deny") => Decision::Deny,
            _ => continue,
        };
        let id: i64 = a["value"].as_str().and_then(|v| v.parse().ok()).unwrap_or(0);
        let allowed = access::resolve(app, user_id)?.filter(|u| u.role == Role::Admin);
        let msg = match allowed {
            None => "⛔ only admins can approve a tool".to_string(),
            Some(u) => match approval::decide(app, id, decision, &u.label()) {
                Ok(_) => continue, // the message is updated by approval::request
                Err(e) => format!("❌ {e:#}"),
            },
        };
        let slack = app.company.cfg.slack.as_ref().unwrap();
        slack_api(app, "chat.postEphemeral", json!({"channel": slack.channel, "user": user_id, "text": msg})).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------- Socket Mode

pub async fn slack_loop(app: Arc<App>) {
    let Some(slack) = app.company.cfg.slack.clone() else { return };
    if slack.bot_token().is_none() || slack.app_token().is_none() {
        eprintln!("slack: HIVE_SLACK_BOT_TOKEN (xoxb-…) and HIVE_SLACK_APP_TOKEN (xapp-…) are required");
        return;
    }
    loop {
        if let Err(e) = socket_session(&app).await {
            eprintln!("slack: {e:#}");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// One websocket session; returns when Slack asks to reconnect or the socket drops.
async fn socket_session(app: &Arc<App>) -> Result<()> {
    let slack = app.company.cfg.slack.as_ref().unwrap();
    let open: Value = app
        .http
        .post("https://slack.com/api/apps.connections.open")
        .bearer_auth(slack.app_token().unwrap_or_default())
        .send()
        .await?
        .json()
        .await?;
    if open["ok"] != true {
        bail!("apps.connections.open: {}", open["error"].as_str().unwrap_or("?"));
    }
    let url = open["url"].as_str().context("missing websocket url")?;
    let (ws, _) = tokio_tungstenite::connect_async(url).await.context("websocket connection")?;
    let (mut tx, mut rx) = ws.split();
    while let Some(msg) = rx.next().await {
        match msg? {
            Message::Text(t) => {
                let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                // Ack first: Slack redelivers after 3 s.
                if let Some(eid) = v["envelope_id"].as_str() {
                    tx.send(Message::Text(json!({"envelope_id": eid}).to_string().into())).await?;
                }
                match v["type"].as_str() {
                    Some("hello") => println!("💬 Slack connected (channel {})", slack.channel),
                    Some("disconnect") => return Ok(()),
                    Some("interactive") => {
                        let payload = v["payload"].clone();
                        let app = app.clone();
                        tokio::spawn(async move {
                            if let Err(e) = on_interactive(&app, &payload).await {
                                eprintln!("slack interactive: {e:#}");
                            }
                        });
                    }
                    Some("events_api") => {
                        let ev = v["payload"]["event"].clone();
                        let app = app.clone();
                        tokio::spawn(async move {
                            if let Err(e) = on_event(&app, &ev).await {
                                eprintln!("slack event: {e:#}");
                            }
                        });
                    }
                    _ => {}
                }
            }
            Message::Ping(_) => tx.flush().await?, // pong is queued by tungstenite
            Message::Close(_) => return Ok(()),
            _ => {}
        }
    }
    Ok(())
}

const HELP: &str = "*In the channel*
• a plain message → new task for the default agent
• `@cto <text>` → new task for a given agent
• `status` · `tasks` · `tasks all` · `show <id>` · `usage` · `cancel <id>` · `retry <id>` · `reply <id> <text>`
*In a task thread*
• a message → sent to the agent (first to the one waiting for an answer)
• finished task: a message *reopens* it (the agent resumes with all its context); \"thanks\", \"ok\"… don't reopen anything
• `cancel` · `retry` · `show`
*Rights* (`me` to see yours)
• viewer: read · member: create tasks and act on their own · admin: everything
• admin: `users` · `user add @someone member [agents=marketing,dev] [name=Alice]` · `user rm @someone`
*Agents* (create / remove: admins, no restart)
• `agents` · `agent show dev` · `agent rm designer` · `agents reload` (re-reads agents/*.toml)
• `agent add designer title=\"UI Designer\" reports_to=cto capabilities=ui,mockups profiles=edit,web model=sonnet`
   followed, on the next lines, by the role prompt
*Agent tools* (changes: admins, applied at the next run)
• `tools` · `tools dev` · `tools dev allow Bash(npm test)` · `tools dev deny Bash(git push *)` · `tools dev remove <rule>`
• `tools dev profile add web` · `tools profile web allow WebFetch` · `tools profile web delete`
• `approvals` · `approve <id> once|always|deny` (or the buttons under the request)
*Hires proposed by agents* (decision: admins)
• `proposals` · `proposal accept <id>` · `proposal reject <id> [reason]` (or the buttons)
*Arbitrations*: when agents go around in circles, the question lands in the thread; your answer is passed on.";

async fn on_event(app: &Arc<App>, ev: &Value) -> Result<()> {
    let slack = app.company.cfg.slack.as_ref().unwrap();
    if ev["type"] != "message" || ev.get("subtype").is_some() || ev.get("bot_id").is_some() {
        return Ok(());
    }
    if ev["channel"].as_str() != Some(slack.channel.as_str()) {
        return Ok(());
    }
    let user_id = ev["user"].as_str().unwrap_or("");
    let Some(user) = access::resolve(app, user_id)? else {
        app.log(&format!("🔒 Slack message ignored: {user_id} has no access"));
        return Ok(());
    };
    let ts = ev["ts"].as_str().unwrap_or("").to_string();
    // Drop a leading mention of the bot ("<@U123> …").
    let raw = ev["text"].as_str().unwrap_or("").trim();
    let raw = match raw.strip_prefix("<@").and_then(|r| r.split_once('>')) {
        Some((_, rest)) => rest.trim(),
        None => raw,
    };
    let text = unescape(raw);
    if text.is_empty() {
        return Ok(());
    }

    let (thread, reply) = match ev["thread_ts"].as_str().filter(|t| *t != ts) {
        Some(thread) => (thread.to_string(), in_thread(app, &user, thread, &text).await),
        None => (ts.clone(), in_channel(app, &user, &ts, &text).await),
    };
    let out = reply.unwrap_or_else(|e| format!("❌ {e:#}"));
    if !out.is_empty() {
        post(app, Some(&thread), &out, false, None).await?;
    }
    Ok(())
}

/// Slack id of whoever requested the root of task `id`.
fn requester_of(app: &App, id: i64) -> Result<Option<String>> {
    Ok(app.db.task(app.db.root_of(id)?)?.and_then(|t| t.requested_by))
}

/// Short acknowledgements that should not reopen a finished task.
fn is_ack(text: &str) -> bool {
    let t: String = text.to_lowercase().chars().filter(|c| c.is_alphanumeric() || *c == ' ').collect();
    let t = t.trim();
    const ACKS: &[&str] = &[
        // English
        "", "ok", "okay", "thanks", "thank you", "thanks a lot", "thx", "ty", "great", "nice", "cool", "perfect",
        "awesome", "excellent", "good job", "well done", "ok thanks", "great thanks", "perfect thanks", "noted",
        // French
        "merci", "merci beaucoup", "merci bien", "top", "parfait", "super", "génial", "genial", "nickel", "bravo",
        "impeccable", "parfait merci", "top merci", "super merci", "ok merci",
    ];
    ACKS.contains(&t)
}

async fn in_thread(app: &Arc<App>, user: &User, thread: &str, text: &str) -> Result<String> {
    let root = app
        .db
        .task_by_thread(thread)?
        .ok_or_else(|| anyhow!("this thread is not linked to any task"))?;
    let requester = requester_of(app, root)?;
    let act = || access::check(user, Action::ActOnTask { requester: requester.as_deref() });
    let req = match text.to_lowercase().as_str() {
        "show" => Request::Show { id: root },
        "cancel" => {
            act()?;
            Request::Cancel { id: root }
        }
        "retry" => {
            act()?;
            Request::Retry { id: root }
        }
        _ => {
            act()?;
            // "thanks", "ok"… on a finished task: nothing to reopen.
            let closed = app.db.task(root)?.is_some_and(|t| t.is_closed());
            if closed && is_ack(text) {
                return Ok(String::new());
            }
            let id = app.db.blocked_in_tree(root)?.unwrap_or(root);
            Request::Reply { id, message: format!("({}) {text}", user.label()) }
        }
    };
    handle(app, req).await
}

async fn in_channel(app: &Arc<App>, user: &User, ts: &str, text: &str) -> Result<String> {
    let (word, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let rest = rest.trim();
    let id_arg = |r: &str| -> Result<(i64, String)> {
        let (id, rest) = r.split_once(char::is_whitespace).unwrap_or((r, ""));
        let id = id.trim_start_matches('#').parse().map_err(|_| anyhow!("task id expected"))?;
        Ok((id, rest.trim().to_string()))
    };
    let act_on = |id: i64| -> Result<()> {
        let requester = requester_of(app, id)?;
        access::check(user, Action::ActOnTask { requester: requester.as_deref() })
    };
    let req = match word.to_lowercase().as_str() {
        "help" if rest.is_empty() => return Ok(HELP.into()),
        "me" if rest.is_empty() => {
            return Ok(format!(
                "{} · role {}{}",
                user.label(),
                user.role.as_str(),
                if user.agents.is_empty() || user.role == Role::Admin {
                    String::new()
                } else {
                    format!(" · agents: {}", user.agents.join(", "))
                }
            ))
        }
        "status" if rest.is_empty() => Request::Status,
        "tasks" if rest.is_empty() || rest == "all" => Request::Tasks { all: rest == "all" },
        "usage" if rest.is_empty() => Request::Usage,
        "show" => Request::Show { id: id_arg(rest)?.0 },
        "cancel" => {
            let id = id_arg(rest)?.0;
            act_on(id)?;
            Request::Cancel { id }
        }
        "retry" => {
            let id = id_arg(rest)?.0;
            act_on(id)?;
            Request::Retry { id }
        }
        "reply" => {
            let (id, message) = id_arg(rest)?;
            if message.is_empty() {
                bail!("usage: reply <id> <text>");
            }
            act_on(id)?;
            Request::Reply { id, message: format!("({}) {message}", user.label()) }
        }
        "agents" if rest.is_empty() => Request::AgentList,
        "agents" if rest == "reload" => {
            access::check(user, Action::ManageUsers)?;
            Request::AgentReload
        }
        "agent" => {
            let (sub, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            match sub {
                "show" => Request::AgentShow { name: args.trim().to_string() },
                "add" => {
                    access::check(user, Action::ManageUsers)?;
                    let (line, prompt) = args.split_once('\n').unwrap_or((args, ""));
                    Request::AgentAdd { spec: crate::agents::parse_add(line, prompt)? }
                }
                "rm" | "remove" => {
                    access::check(user, Action::ManageUsers)?;
                    Request::AgentRemove { name: args.trim().to_string() }
                }
                _ => bail!("usage: agent show <name> · agent rm <name> · {}", crate::agents::ADD_USAGE),
            }
        }
        "tools" => {
            if !rest.is_empty() {
                access::check(user, Action::ManageUsers)?; // changing tools: admins only
            }
            Request::Tools { args: rest.to_string() }
        }
        "approvals" if rest.is_empty() => Request::Approvals,
        "proposals" => Request::Proposals { all: rest == "all" },
        "proposal" => {
            access::check(user, Action::ManageUsers)?;
            let (verb, r) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            let accept = match verb {
                "accept" => true,
                "reject" => false,
                _ => bail!("usage: proposal accept <id> · proposal reject <id> [reason]"),
            };
            let (id, note) = id_arg(r.trim())?;
            let note = Some(note).filter(|n| !n.is_empty());
            return crate::collab::decide_proposal(app, id, accept, &user.label(), note.as_deref()).await;
        }
        "approve" => {
            access::check(user, Action::ManageUsers)?;
            let (id, decision) = id_arg(rest)?;
            return approval::decide(app, id, Decision::parse(&decision)?, &user.label());
        }
        "users" if rest.is_empty() => {
            access::check(user, Action::ManageUsers)?;
            Request::Users
        }
        "user" => {
            access::check(user, Action::ManageUsers)?;
            user_command(rest)?
        }
        _ => {
            // New task; the user's message becomes the task's thread.
            let (to, body) = match text.strip_prefix('@') {
                Some(r) => {
                    let (a, b) = r.split_once(char::is_whitespace).unwrap_or((r, ""));
                    (Some(a.to_string()), b.trim())
                }
                None => (None, text),
            };
            if body.is_empty() {
                bail!("empty task");
            }
            let to = owner_target(app, to)?;
            access::check(user, Action::CreateTask { to: &to })?;
            let (id, to) = create_owner_task(app, Some(to), &title_of(body), body, Some(&user.slack_id))?;
            app.db.set_thread(id, ts)?;
            return Ok(format!("📋 Task #{id} assigned to {to}. I'll keep you posted in this thread."));
        }
    };
    let out = handle(app, req).await?;
    Ok(format!("```\n{out}\n```"))
}

/// `add <@U…> <role> [agents=a,b] [name=Alice]` | `rm <@U…>`
fn user_command(rest: &str) -> Result<Request> {
    let mut words = rest.split_whitespace();
    let usage = "usage: user add @someone viewer|member|admin [agents=a,b] [name=Alice] · user rm @someone";
    match words.next() {
        Some("add") | Some("set") => {
            let slack_id = words.next().ok_or_else(|| anyhow!(usage))?.to_string();
            let role = words.next().ok_or_else(|| anyhow!(usage))?.to_string();
            let (mut name, mut agents) = (None, Vec::new());
            for w in words {
                if let Some(v) = w.strip_prefix("agents=") {
                    agents = v.split(',').filter(|a| !a.is_empty()).map(String::from).collect();
                } else if let Some(v) = w.strip_prefix("name=") {
                    name = Some(v.to_string());
                } else {
                    bail!("{usage}");
                }
            }
            Ok(Request::UserSet { slack_id, role, name, agents })
        }
        Some("rm") | Some("remove") => {
            let slack_id = words.next().ok_or_else(|| anyhow!(usage))?.to_string();
            Ok(Request::UserRemove { slack_id })
        }
        _ => bail!("{usage}"),
    }
}

#[cfg(test)]
mod tests {
    use super::is_ack;

    #[test]
    fn acks() {
        assert!(is_ack("Thanks!"));
        assert!(is_ack("ok 👍"));
        assert!(is_ack("Great, thanks"));
        assert!(is_ack("Merci !"));
        assert!(!is_ack("thanks, can you add a French version?"));
        assert!(!is_ack("redo the logo in blue"));
    }
}
