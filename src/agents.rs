//! Agents created / removed / reloaded on the fly. `agents/<name>.toml` stays the source of truth:
//! creating an agent writes its file, removing one renames it `<name>.toml.removed`, and
//! `reload` picks up hand edits — all without restarting the daemon.

use crate::config::{valid_agent_name, AgentConfig};
use crate::daemon::App;
use crate::tools;
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentSpec {
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub reports_to: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(default)]
    pub prompt: String,
}

fn q(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn list(v: &[String]) -> String {
    format!("[{}]", v.iter().map(|s| q(s)).collect::<Vec<_>>().join(", "))
}

fn to_toml(s: &AgentSpec) -> String {
    let mut t = String::new();
    let _ = writeln!(t, "name = {}", q(&s.name));
    let _ = writeln!(t, "title = {}", q(&s.title));
    if let Some(r) = &s.reports_to {
        let _ = writeln!(t, "reports_to = {}", q(r));
    }
    let _ = writeln!(t, "capabilities = {}", list(&s.capabilities));
    if let Some(m) = &s.model {
        let _ = writeln!(t, "model = {}", q(m));
    }
    if let Some(w) = &s.workdir {
        let _ = writeln!(t, "workdir = {}", q(w));
    }
    t += "permission_mode = \"default\"\n";
    t += "# Initial tool access; then managed live (`tools <agent> …`)\n";
    let _ = writeln!(t, "profiles = {}", list(&s.profiles));
    let prompt = s.prompt.trim();
    if prompt.contains("'''") {
        let _ = writeln!(t, "prompt = {}", q(prompt));
    } else {
        let _ = writeln!(t, "prompt = '''\n{prompt}\n'''");
    }
    t
}

/// Checks a new agent's spec against the current team (used by `add` and hire proposals).
pub fn validate(app: &App, spec: &AgentSpec) -> Result<()> {
    if !valid_agent_name(&spec.name) {
        bail!("invalid agent name '{}' (letters, digits, - and _, 32 max)", spec.name);
    }
    if spec.title.trim().is_empty() {
        bail!("a title is required (title=\"…\")");
    }
    if app.company.agent(&spec.name).is_some() {
        bail!("agent '{}' already exists", spec.name);
    }
    if let Some(r) = &spec.reports_to {
        if app.company.agent(r).is_none() {
            bail!("unknown manager '{r}'");
        }
    }
    let known = app.db.profile_names()?;
    for p in &spec.profiles {
        if !known.contains(p) {
            bail!("unknown profile '{p}' (profiles: {})", known.join(", "));
        }
    }
    Ok(())
}

pub fn add(app: &App, spec: AgentSpec) -> Result<String> {
    validate(app, &spec)?;
    let path = app.company.agents_dir().join(format!("{}.toml", spec.name));
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    let body = to_toml(&spec);
    // Make sure what we write is what we will read back.
    toml::from_str::<AgentConfig>(&body).map_err(|e| anyhow!("generated config is invalid: {e}"))?;
    std::fs::write(&path, body)?;
    if let Err(e) = reload(app) {
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    app.log(&format!("🆕 agent {} ({}) created", spec.name, spec.title));
    show(app, &spec.name)
}

pub fn remove(app: &App, name: &str) -> Result<String> {
    let a = app.company.agent(name).ok_or_else(|| anyhow!("unknown agent '{name}'"))?;
    if app.company.cfg.default_agent == name {
        bail!("{name} is the default agent (default_agent in hive.toml)");
    }
    let subordinates: Vec<String> =
        app.company.agents().into_iter().filter(|x| x.reports_to.as_deref() == Some(name)).map(|x| x.name).collect();
    if !subordinates.is_empty() {
        bail!("{} report to {name}: change their reports_to first", subordinates.join(", "));
    }
    let open: Vec<String> =
        app.db.tasks(false)?.into_iter().filter(|t| t.assignee == name).map(|t| format!("#{}", t.id)).collect();
    if !open.is_empty() {
        bail!("{name} has open tasks ({}): finish or cancel them first", open.join(", "));
    }
    let path = app.company.agents_dir().join(format!("{name}.toml"));
    if !path.exists() {
        bail!("{} not found (is the agent defined in another file?)", path.display());
    }
    std::fs::rename(&path, path.with_extension("toml.removed"))?;
    reload(app)?;
    app.log(&format!("🗑 agent {name} ({}) removed", a.title));
    Ok(format!("{name} removed (file renamed {name}.toml.removed, its past tasks remain viewable)"))
}

/// Re-reads `agents/*.toml`, seeds the tools of new agents, creates their workdirs.
pub fn reload(app: &App) -> Result<String> {
    let before: Vec<String> = app.company.agents().into_iter().map(|a| a.name).collect();
    let agents = app.company.reload_agents()?;
    tools::seed(app)?;
    for a in &agents {
        std::fs::create_dir_all(app.company.workdir(a))?;
    }
    let names: Vec<String> = agents.iter().map(|a| a.name.clone()).collect();
    let added: Vec<&String> = names.iter().filter(|n| !before.contains(n)).collect();
    let removed: Vec<&String> = before.iter().filter(|n| !names.contains(n)).collect();
    app.wake.notify_one();
    Ok(format!(
        "{} agent(s): {}{}{}",
        names.len(),
        names.join(", "),
        if added.is_empty() {
            String::new()
        } else {
            format!(" · added: {}", added.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))
        },
        if removed.is_empty() {
            String::new()
        } else {
            format!(" · removed: {}", removed.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))
        }
    ))
}

pub fn show(app: &App, name: &str) -> Result<String> {
    let a = app.company.agent(name).ok_or_else(|| anyhow!("unknown agent '{name}'"))?;
    let e = tools::effective(app, name)?;
    Ok(format!(
        "{} — {}{}\n  model: {} · provider: {} · workdir: {}\n  capabilities: {}\n  profiles: {} · tools ✓ {} · ✗ {}\n\n{}",
        a.name,
        a.title,
        a.reports_to.as_ref().map(|r| format!(" (reports to {r})")).unwrap_or_default(),
        a.model.as_deref().unwrap_or("default"),
        a.provider,
        a.workdir,
        if a.capabilities.is_empty() { "—".into() } else { a.capabilities.join(", ") },
        if e.profiles.is_empty() { "—".into() } else { e.profiles.join(", ") },
        if e.allow.is_empty() { "—".into() } else { e.allow.join(", ") },
        if e.deny.is_empty() { "—".into() } else { e.deny.join(", ") },
        a.prompt.trim()
    ))
}

pub fn list_text(app: &App) -> String {
    app.company
        .agents()
        .iter()
        .map(|a| {
            format!(
                "{:<12} {}{} · {} · {}",
                a.name,
                a.title,
                a.reports_to.as_ref().map(|r| format!(" (→ {r})")).unwrap_or_default(),
                a.model.as_deref().unwrap_or("default"),
                a.capabilities.join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Splits on whitespace, keeping "quoted strings" (straight or curly quotes) together.
fn tokens(s: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q || (q == '“' && c == '”') => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '“' => {
                quote = Some(c);
                has = true;
            }
            None if c.is_whitespace() => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            None => {
                cur.push(c);
                has = true;
            }
        }
    }
    if quote.is_some() {
        bail!("unclosed quote");
    }
    if has {
        out.push(cur);
    }
    Ok(out)
}

fn csv(v: &str) -> Vec<String> {
    v.split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect()
}

pub const ADD_USAGE: &str =
    "agent add <name> title=\"…\" [reports_to=cto] [capabilities=a,b] [profiles=dev,web] [model=sonnet] [workdir=…]
<role prompt on the following lines>";

/// Slack: first line `agent add <name> key=value…`, the following lines are the role prompt.
pub fn parse_add(first_line_args: &str, prompt: &str) -> Result<AgentSpec> {
    let toks = tokens(first_line_args)?;
    let mut it = toks.into_iter();
    let name = it.next().ok_or_else(|| anyhow!("usage: {ADD_USAGE}"))?;
    let mut s = AgentSpec { name, prompt: prompt.trim().to_string(), ..Default::default() };
    for t in it {
        let (k, v) = t.split_once('=').ok_or_else(|| anyhow!("'{t}': key=value expected\nusage: {ADD_USAGE}"))?;
        match k.to_lowercase().as_str() {
            "title" => s.title = v.to_string(),
            "reports_to" | "manager" => s.reports_to = Some(v.to_string()),
            "capabilities" | "skills" => s.capabilities = csv(v),
            "profiles" => s.profiles = csv(v),
            "model" => s.model = Some(v.to_string()),
            "workdir" => s.workdir = Some(v.to_string()),
            _ => bail!("unknown key '{k}'\nusage: {ADD_USAGE}"),
        }
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        let s = parse_add(
            "designer title=\"UI Designer\" reports_to=cto capabilities=ui,mockups profiles=edit,web model=sonnet",
            "You design the interfaces.",
        )
        .unwrap();
        assert_eq!(s.name, "designer");
        assert_eq!(s.title, "UI Designer");
        assert_eq!(s.reports_to.as_deref(), Some("cto"));
        assert_eq!(s.capabilities, vec!["ui", "mockups"]);
        assert_eq!(s.profiles, vec!["edit", "web"]);
        let body = to_toml(&s);
        let back: AgentConfig = toml::from_str(&body).unwrap();
        assert_eq!(back.title, "UI Designer");
        assert_eq!(back.prompt.trim(), "You design the interfaces.");
        assert!(parse_add("x title=\"oops", "").is_err());
        assert!(parse_add("x foo=bar", "").is_err());
    }
}
