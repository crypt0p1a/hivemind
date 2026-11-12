//! Tool approvals: when an agent wants a tool its rules don't allow, `claude` calls our
//! `mcp__hive__approve` (via `--permission-prompt-tool`). We ask the admins (Slack buttons, CLI,
//! dashboard) and answer allow / deny. "Always" adds an allow rule to the agent.

use crate::daemon::App;
use crate::notify;
use crate::util::trunc;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Once,
    Always,
    Deny,
}

impl Decision {
    pub fn parse(s: &str) -> Result<Decision> {
        Ok(match s.to_lowercase().as_str() {
            "" | "once" | "ok" | "yes" => Decision::Once,
            "always" => Decision::Always,
            "deny" | "no" => Decision::Deny,
            _ => bail!("decision expected: once | always | deny"),
        })
    }
}

pub struct Pending {
    pub id: i64,
    pub agent: String,
    pub task_id: i64,
    pub tool: String,
    pub summary: String,
    /// Rule added to the agent on "always"
    pub rule: String,
    pub slack_ts: Option<String>,
    pub tx: Option<oneshot::Sender<(Decision, String)>>,
}

/// Rule that "always" would add, as narrow as reasonable.
fn suggest_rule(tool: &str, input: &Value) -> String {
    match tool {
        "Bash" => match input["command"].as_str() {
            Some(c) if !c.contains('\n') && c.len() <= 200 => format!("Bash({})", c.trim()),
            _ => "Bash".into(),
        },
        "WebFetch" => input["url"]
            .as_str()
            .and_then(|u| u.split("://").nth(1))
            .and_then(|r| r.split(['/', '?', '#']).next())
            .map(|host| format!("WebFetch(domain:{host})"))
            .unwrap_or_else(|| "WebFetch".into()),
        _ => tool.to_string(),
    }
}

fn describe(tool: &str, input: &Value) -> String {
    let detail = match tool {
        "Bash" => input["command"].as_str().map(|c| format!("`{}`", trunc(c, 300))),
        "Edit" | "Write" | "Read" | "MultiEdit" | "NotebookEdit" => {
            input["file_path"].as_str().or(input["notebook_path"].as_str()).map(String::from)
        }
        "WebFetch" => input["url"].as_str().map(String::from),
        "WebSearch" => input["query"].as_str().map(|q| format!("\"{q}\"")),
        _ => None,
    };
    match detail {
        Some(d) => format!("{tool}: {d}"),
        None => format!("{tool} {}", trunc(&input.to_string(), 300)),
    }
}

/// Called (through the MCP bridge) by claude's permission prompt. Blocks until a decision.
/// Returns the JSON claude expects: `{"behavior":"allow","updatedInput":…}` / `{"behavior":"deny","message":…}`.
pub async fn request(app: &Arc<App>, agent: &str, task_id: i64, tool: &str, input: Value) -> Result<String> {
    let deny = |m: &str| json!({"behavior": "deny", "message": m}).to_string();
    let cfg = &app.company.cfg.tools;
    if !cfg.approvals {
        return Ok(deny("tool not allowed for this agent (approvals disabled)"));
    }
    let id = app.next_approval.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel();
    let pending = Pending {
        id,
        agent: agent.to_string(),
        task_id,
        tool: tool.to_string(),
        summary: describe(tool, &input),
        rule: suggest_rule(tool, &input),
        slack_ts: None,
        tx: Some(tx),
    };
    app.log(&format!("🔐 approval #{id}: {agent} (#{task_id}) wants {}", trunc(&pending.summary, 120)));
    let ts = notify::approval_request(app, &pending).await;
    {
        let mut p = pending;
        p.slack_ts = ts;
        app.approvals.lock().unwrap().insert(id, p);
    }
    let res = tokio::time::timeout(Duration::from_secs(cfg.approval_timeout_secs), rx).await;
    let p = app.approvals.lock().unwrap().remove(&id);
    let (decision, by) = match res {
        Ok(Ok(d)) => d,
        Ok(Err(_)) => (Decision::Deny, "run ended".to_string()),
        Err(_) => (Decision::Deny, format!("no answer after {} min", cfg.approval_timeout_secs / 60)),
    };
    let rule = p.as_ref().map(|p| p.rule.clone()).unwrap_or_else(|| suggest_rule(tool, &input));
    if decision == Decision::Always {
        app.db.add_rule("agent", agent, "allow", &rule)?;
    }
    let verdict = match decision {
        Decision::Once => format!("✅ allowed once by {by}"),
        Decision::Always => format!("✅ always allowed by {by} (rule {rule})"),
        Decision::Deny => format!("⛔ denied ({by})"),
    };
    app.log(&format!("🔐 approval #{id}: {verdict}"));
    if let Some(p) = &p {
        notify::approval_done(app, p, &verdict).await;
    }
    Ok(match decision {
        Decision::Deny => deny(&format!(
            "The owner did not allow this tool ({by}). Find another way, or explain in your result what you would need."
        )),
        _ => json!({"behavior": "allow", "updatedInput": input}).to_string(),
    })
}

/// Answer a pending approval. `by`: who decided (shown in Slack / logs).
pub fn decide(app: &App, id: i64, decision: Decision, by: &str) -> Result<String> {
    let mut map = app.approvals.lock().unwrap();
    let p = map.get_mut(&id).ok_or_else(|| anyhow!("approval #{id} not found (already handled?)"))?;
    let tx = p.tx.take().ok_or_else(|| anyhow!("approval #{id} already handled"))?;
    let _ = tx.send((decision, by.to_string()));
    Ok(format!("approval #{id} ({} · {}): {decision:?}", p.agent, trunc(&p.summary, 80)))
}

/// Pending approvals of a run that ended: they resolve as denied.
pub fn drop_for(app: &App, agent: &str, task_id: i64) {
    app.approvals.lock().unwrap().retain(|_, p| !(p.agent == agent && p.task_id == task_id));
}

pub fn list_text(app: &App) -> String {
    let map = app.approvals.lock().unwrap();
    if map.is_empty() {
        return "No pending approval.".into();
    }
    let mut v: Vec<&Pending> = map.values().collect();
    v.sort_by_key(|p| p.id);
    v.iter()
        .map(|p| {
            format!(
                "#{} · {} (task #{}) wants {}\n    always would add: {}",
                p.id, p.agent, p.task_id, p.summary, p.rule
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn snapshot(app: &App) -> Value {
    let map = app.approvals.lock().unwrap();
    let mut v: Vec<&Pending> = map.values().collect();
    v.sort_by_key(|p| p.id);
    json!(v
        .iter()
        .map(|p| json!({"id": p.id, "agent": p.agent, "task_id": p.task_id, "tool": p.tool, "summary": p.summary, "rule": p.rule}))
        .collect::<Vec<_>>())
}

/// Agents currently waiting for an approval.
pub fn waiting_agents(app: &App) -> Vec<String> {
    app.approvals.lock().unwrap().values().map(|p| p.agent.clone()).collect()
}
