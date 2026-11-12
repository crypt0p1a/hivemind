//! Agents talking to each other, with loop control, and agents proposing hires.
//!
//! `ask_agent`: any agent may question any other (even upstream). The question is a child task of
//! kind `question`; the answerer may only answer (no delegation, no further question), so no chain
//! can form. Answers flow back into the asker's task (positive feedback) and earlier exchanges are
//! recalled with every new question. A human arbitrates when the same pair keeps going back and
//! forth, when a tree has too many questions, or when a question repeats an answered one.
//!
//! `propose_agent`: a recruiter (ceo, cto by default) proposes a new agent; an admin accepts
//! (the agent is created on the spot) or rejects; the proposer is resumed with the decision.

use crate::agents::{self, AgentSpec};
use crate::daemon::App;
use crate::db::Task;
use crate::notify;
use crate::util::trunc;
use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

fn words(s: &str) -> HashSet<String> {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() > 2).map(String::from).collect()
}

/// Jaccard similarity of the significant words.
pub fn similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (words(a), words(b));
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    a.intersection(&b).count() as f64 / a.union(&b).count() as f64
}

fn exchange_text(q: &Task) -> String {
    let answer = match q.status.as_str() {
        "done" => q.result.clone().unwrap_or_default(),
        "failed" => format!("(no answer: {})", q.result.clone().unwrap_or_default()),
        s => format!("(in progress: {s})"),
    };
    format!("- {} → {}: {}\n  ↳ {}", q.created_by, q.assignee, trunc(&q.description, 400), trunc(&answer, 600))
}

/// `ask_agent` tool. Returns the message for the asking agent.
pub async fn ask_agent(app: &Arc<App>, agent: &str, t: &Task, to: &str, question: &str) -> Result<String> {
    if t.kind == "question" {
        bail!("you are answering a question: answer with what you know (complete_task), without asking another one");
    }
    if t.is_closed() {
        bail!("task #{} is already closed", t.id);
    }
    if to == agent {
        bail!("you cannot ask yourself a question");
    }
    app.company.agent(to).ok_or_else(|| anyhow!("unknown agent '{to}' — use list_agents"))?;

    let root = app.db.root_of(t.id)?;
    let history = app.db.questions_in_tree(root)?;
    let pair: Vec<&Task> = history
        .iter()
        .filter(|q| (q.created_by == agent && q.assignee == to) || (q.created_by == to && q.assignee == agent))
        .collect();
    let cfg = &app.company.cfg.collaboration;
    let repeat = history
        .iter()
        .find(|q| q.created_by == agent && q.assignee == to && similarity(&q.description, question) >= 0.7);

    let reason = if let Some(q) = repeat {
        Some(format!("question very close to #{}, already asked", q.id))
    } else if pair.len() >= cfg.max_questions_per_pair {
        Some(format!("{} exchanges already between {agent} and {to} on this task", pair.len()))
    } else if history.len() >= cfg.max_questions_per_tree {
        Some(format!("{} questions already asked on this task", history.len()))
    } else {
        None
    };

    if let Some(reason) = reason {
        // Loop: a human decides instead of the agents going around in circles.
        app.db.set_status(t.id, "blocked", None)?;
        let recent: Vec<String> = pair.iter().rev().take(4).rev().map(|q| exchange_text(q)).collect();
        app.log(&format!("⚖️ arbitration on #{}: {agent} ↔ {to} ({reason})", t.id));
        notify::owner(
            app,
            t.id,
            &format!(
                "⚖️ Arbitration needed on #{} ({reason}).\n{agent} wanted to ask {to}:\n{}\n\nPrevious exchanges:\n{}\n\n→ Decide by replying in this thread (or hive reply {} \"…\"): your answer goes to {agent}.",
                t.id,
                trunc(question, 800),
                if recent.is_empty() { "(none)".into() } else { recent.join("\n") },
                t.id
            ),
        )
        .await;
        return Ok(format!(
            "Too many back-and-forths ({reason}): your question goes to a human for arbitration. End your turn, you will be resumed with their decision."
        ));
    }

    // Positive feedback: the answerer sees what was already said between them.
    let previous = if pair.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nPrevious exchanges between you on this task (don't repeat yourself, build on them):\n{}",
            pair.iter().map(|q| exchange_text(q)).collect::<Vec<_>>().join("\n")
        )
    };
    let title = format!("Question from {agent}: {}", trunc(question.lines().next().unwrap_or(""), 60));
    let id = app.db.create_task(Some(t.id), &title, question, to, agent, t.depth + 1, t.requested_by.as_deref())?;
    app.db.set_kind(id, "question")?;
    let body = format!(
        "# Question #{id} from {agent}\nAsked as part of their task #{} \"{}\".\n\n{question}{previous}\n\n\
         Answer with complete_task(result = your answer, precise and self-contained). You can neither delegate \
         nor ask another question here; if you don't know, say so in your answer (or fail_task).",
        t.id, t.title
    );
    app.db.add_inbox(to, id, "assigned", &body)?;
    app.wake.notify_one();
    app.log(&format!("💬 #{id} {agent} → {to}: {}", trunc(question, 60)));
    notify::thread_event(app, id, &format!("💬 {agent} asks {to} (#{id}): {}", trunc(question, 300))).await;
    Ok(format!(
        "Question #{id} sent to {to}. End your turn when you have nothing else to do: you will be resumed with their answer."
    ))
}

/// Previous session of `agent` on an ancestor of `task` (to answer a question with its context).
pub fn ancestor_session(app: &App, agent: &str, task: &Task) -> Result<Option<String>> {
    let mut cur = task.parent_id;
    while let Some(id) = cur {
        let Some(t) = app.db.task(id)? else { break };
        if t.assignee == agent {
            if let Some(s) = app.db.session(agent, id)? {
                return Ok(Some(s));
            }
        }
        cur = t.parent_id;
    }
    Ok(None)
}

// ---------------------------------------------------------------- hires

fn str_list(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a.iter().filter_map(|x| x.as_str()).map(String::from).collect(),
        Value::String(s) => s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect(),
        _ => vec![],
    }
}

pub async fn propose_agent(app: &Arc<App>, agent: &str, t: &Task, args: &Value) -> Result<String> {
    if !app.company.cfg.collaboration.recruiters.iter().any(|r| r == agent) {
        bail!("only {} can propose a hire: ask them (ask_agent)", app.company.cfg.collaboration.recruiters.join(", "));
    }
    if t.kind == "question" {
        bail!("no hiring from an answer to a question");
    }
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let spec = AgentSpec {
        name: s("name").ok_or_else(|| anyhow!("missing parameter 'name'"))?,
        title: s("title").ok_or_else(|| anyhow!("missing parameter 'title'"))?,
        reports_to: s("reports_to"),
        capabilities: str_list(&args["capabilities"]),
        profiles: str_list(&args["profiles"]),
        model: s("model"),
        workdir: None,
        prompt: s("prompt").ok_or_else(|| anyhow!("missing parameter 'prompt'"))?,
    };
    let reason = s("reason").ok_or_else(|| anyhow!("missing parameter 'reason'"))?;
    agents::validate(app, &spec)?;
    if app.db.proposals(true)?.iter().any(|p| p.spec.contains(&format!("\"name\":\"{}\"", spec.name))) {
        bail!("a hire of '{}' is already waiting for a decision", spec.name);
    }
    let id = app.db.create_proposal(agent, t.id, &serde_json::to_string(&spec)?, &reason)?;
    app.db.set_status(t.id, "blocked", None)?;
    app.log(&format!("🧑‍💼 proposal #{id}: {agent} wants to hire {} ({})", spec.name, spec.title));
    if let Some(p) = app.db.proposal(id)? {
        if let Some(ts) = notify::proposal_request(app, &p, &spec).await {
            app.db.set_proposal_ts(id, &ts)?;
        }
    }
    Ok(format!("Hire proposal #{id} sent to the admins. End your turn: you will be resumed with their decision."))
}

/// Accept (creates the agent) or reject a proposal; resumes the proposer.
pub async fn decide_proposal(app: &Arc<App>, id: i64, accept: bool, by: &str, note: Option<&str>) -> Result<String> {
    let p = app.db.proposal(id)?.ok_or_else(|| anyhow!("proposal #{id} not found"))?;
    if p.status != "pending" {
        bail!(
            "proposal #{id} already handled ({}{})",
            p.status,
            p.decided_by.map(|b| format!(" by {b}")).unwrap_or_default()
        );
    }
    let spec: AgentSpec = serde_json::from_str(&p.spec)?;
    let (status, verdict, inbox) = if accept {
        agents::add(app, spec.clone())?;
        (
            "accepted",
            format!("✅ accepted by {by}: {} joined the team", spec.name),
            format!(
                "✅ Hire accepted by {by}: {} ({}) is now part of the team. You can delegate work to them.{}",
                spec.name,
                spec.title,
                note.map(|n| format!("\nNote: {n}")).unwrap_or_default()
            ),
        )
    } else {
        (
            "rejected",
            format!("⛔ rejected by {by}{}", note.map(|n| format!(": {n}")).unwrap_or_default()),
            format!(
                "⛔ Hire of {} rejected by {by}{}. Carry on with the current team, or propose something else.",
                spec.name,
                note.map(|n| format!(" ({n})")).unwrap_or_default()
            ),
        )
    };
    app.db.decide_proposal(id, status, by, note)?;
    if let Some(t) = app.db.task(p.task_id)? {
        if !t.is_closed() {
            app.db.add_inbox(&t.assignee, t.id, "owner", &inbox)?;
            if t.status == "blocked" {
                app.db.set_status(t.id, "in_progress", None)?;
            }
            app.wake.notify_one();
        }
    }
    app.log(&format!("🧑‍💼 proposal #{id} {verdict}"));
    notify::proposal_done(app, &p, &spec, &verdict).await;
    Ok(format!("proposal #{id} {verdict}"))
}

pub fn proposals_text(app: &App, all: bool) -> Result<String> {
    let list = app.db.proposals(!all)?;
    if list.is_empty() {
        return Ok("No pending hire proposal.".into());
    }
    let mut s = String::new();
    for p in list {
        let spec: AgentSpec = serde_json::from_str(&p.spec)?;
        s += &format!(
            "#{} [{}] {} proposes {} — {}{} · capabilities: {} · profiles: {}\n    why: {}\n",
            p.id,
            p.status,
            p.proposed_by,
            spec.name,
            spec.title,
            spec.reports_to.as_ref().map(|r| format!(" (→ {r})")).unwrap_or_default(),
            spec.capabilities.join(", "),
            spec.profiles.join(", "),
            trunc(&p.reason, 300)
        );
    }
    Ok(s.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::similarity;

    #[test]
    fn similar() {
        assert!(
            similarity(
                "Which database should we use for the project?",
                "Which database should we use for this project"
            ) > 0.7
        );
        assert!(similarity("Which database should we use?", "What is the marketing budget for the launch?") < 0.3);
    }
}
