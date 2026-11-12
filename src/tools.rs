//! Live tool access of the agents: rules in the DB, applied at each agent's next run (no restart).
//!
//! - an agent has profiles + its own allow / deny rules;
//! - a profile is a named set of allow / deny rules (`read`, `web`, `edit`, `dev`…);
//! - effective allow = profiles' allows ∪ agent's allows; deny = profiles' denies ∪ agent's denies (deny wins);
//! - rules use Claude Code's syntax: `Read`, `Bash(git *)`, `Edit(src/**)`, `WebFetch(domain:github.com)`, `mcp__server`.
//!
//! `agents/*.toml` `allowed_tools` / `profiles` only seed an agent the first time the daemon sees it.

use crate::daemon::App;
use anyhow::{anyhow, bail, Result};
use std::collections::BTreeSet;

pub const BUILTIN_PROFILES: &[(&str, &[&str], &[&str])] = &[
    ("read", &["Read", "Glob", "Grep"], &[]),
    ("web", &["WebSearch", "WebFetch"], &[]),
    ("edit", &["Read", "Glob", "Grep", "Edit", "Write"], &[]),
    ("dev", &["Read", "Glob", "Grep", "Edit", "Write", "Bash"], &["Bash(git push *)", "Bash(rm -rf *)"]),
];

#[derive(Debug, Default, Clone)]
pub struct Effective {
    pub profiles: Vec<String>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

/// Seeds builtin profiles and agents that have never been configured.
pub fn seed(app: &App) -> Result<()> {
    if !app.db.meta_flag("builtin_profiles_seeded")? {
        for (name, allow, deny) in BUILTIN_PROFILES {
            for r in *allow {
                app.db.add_rule("profile", name, "allow", r)?;
            }
            for r in *deny {
                app.db.add_rule("profile", name, "deny", r)?;
            }
        }
        app.db.set_meta_flag("builtin_profiles_seeded")?;
    }
    for a in &app.company.agents() {
        let key = format!("tools_seeded:{}", a.name);
        if !app.db.meta_flag(&key)? {
            for r in &a.allowed_tools {
                app.db.add_rule("agent", &a.name, "allow", r)?;
            }
            for p in &a.profiles {
                app.db.add_rule("agent", &a.name, "profile", p)?;
            }
            app.db.set_meta_flag(&key)?;
        }
    }
    Ok(())
}

pub fn effective(app: &App, agent: &str) -> Result<Effective> {
    let profiles = app.db.rules("agent", agent, "profile")?;
    let mut allow = BTreeSet::new();
    let mut deny = BTreeSet::new();
    for p in &profiles {
        allow.extend(app.db.rules("profile", p, "allow")?);
        deny.extend(app.db.rules("profile", p, "deny")?);
    }
    allow.extend(app.db.rules("agent", agent, "allow")?);
    deny.extend(app.db.rules("agent", agent, "deny")?);
    Ok(Effective { profiles, allow: allow.into_iter().collect(), deny: deny.into_iter().collect() })
}

/// Light check of Claude Code's rule syntax: `Name` or `Name(spec)`.
pub fn normalize_rule(raw: &str) -> Result<String> {
    let r = raw.trim().trim_matches(|c| c == '`' || c == '"' || c == '\'' || c == '“' || c == '”').trim();
    let name = r.split('(').next().unwrap_or("");
    let ok_name = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    let ok_args = match r.find('(') {
        Some(_) => r.ends_with(')'),
        None => true,
    };
    if !ok_name || !ok_args {
        bail!("invalid rule '{raw}' (e.g. Read, Bash(npm test), Bash(git *), Edit(src/**), WebFetch(domain:github.com))");
    }
    Ok(r.to_string())
}

fn agent_exists(app: &App, agent: &str) -> Result<()> {
    app.company.agent(agent).map(|_| ()).ok_or_else(|| anyhow!("unknown agent '{agent}'"))
}

fn profile_exists(app: &App, name: &str) -> Result<()> {
    if app.db.profile_names()?.iter().any(|p| p == name) {
        Ok(())
    } else {
        bail!("unknown profile '{name}' (profiles: {})", app.db.profile_names()?.join(", "))
    }
}

fn list(v: &[String]) -> String {
    if v.is_empty() {
        "—".to_string()
    } else {
        v.join(", ")
    }
}

pub fn agent_text(app: &App, agent: &str) -> Result<String> {
    agent_exists(app, agent)?;
    let e = effective(app, agent)?;
    let own_allow = app.db.rules("agent", agent, "allow")?;
    let own_deny = app.db.rules("agent", agent, "deny")?;
    Ok(format!(
        "{agent}\n  profiles     : {}\n  allows       : {}\n  denies       : {}\n  effective ✓  : {}\n  effective ✗  : {}",
        list(&e.profiles),
        list(&own_allow),
        list(&own_deny),
        list(&e.allow),
        list(&e.deny)
    ))
}

pub fn overview_text(app: &App) -> Result<String> {
    let mut s = String::from("AGENTS\n");
    for a in &app.company.agents() {
        let e = effective(app, &a.name)?;
        s += &format!(
            "  {:<11} profiles [{}] · ✓ {} · ✗ {}\n",
            a.name,
            e.profiles.join(", "),
            list(&e.allow),
            list(&e.deny)
        );
    }
    s += "\nPROFILES\n";
    for p in app.db.profile_names()? {
        let allow = app.db.rules("profile", &p, "allow")?;
        let deny = app.db.rules("profile", &p, "deny")?;
        s += &format!(
            "  {:<11} ✓ {}{}\n",
            p,
            list(&allow),
            if deny.is_empty() { String::new() } else { format!(" · ✗ {}", deny.join(", ")) }
        );
    }
    Ok(s.trim_end().to_string())
}

/// `tools …` command, shared by Slack and the CLI (the caller checked the rights).
///
/// - `tools`                                → overview
/// - `tools <agent>`                        → detail
/// - `tools <agent> allow|deny <rule>`      → add a rule
/// - `tools <agent> remove <rule>`          → remove a rule (allow and deny)
/// - `tools <agent> profile add|rm <name>`  → attach / detach a profile
/// - `tools profile <name> allow|deny|remove <rule>` · `tools profile <name> delete`
pub fn command(app: &App, args: &str) -> Result<String> {
    let args = args.trim();
    if args.is_empty() {
        return overview_text(app);
    }
    let (first, rest) = split(args);
    if first == "profile" {
        return profile_command(app, rest);
    }
    let agent = first;
    agent_exists(app, agent)?;
    if rest.is_empty() {
        return agent_text(app, agent);
    }
    let (verb, rest) = split(rest);
    match verb {
        "allow" => {
            let r = normalize_rule(rest)?;
            app.db.remove_rule("agent", agent, "deny", &r)?;
            app.db.add_rule("agent", agent, "allow", &r)?;
        }
        "deny" => {
            let r = normalize_rule(rest)?;
            app.db.remove_rule("agent", agent, "allow", &r)?;
            app.db.add_rule("agent", agent, "deny", &r)?;
        }
        "remove" | "rm" => {
            let r = normalize_rule(rest)?;
            let n = app.db.remove_rule("agent", agent, "allow", &r)? + app.db.remove_rule("agent", agent, "deny", &r)?;
            if n == 0 {
                bail!("{agent} has no own rule '{r}' (it may come from a profile)");
            }
        }
        "profile" => {
            let (op, name) = split(rest);
            match op {
                "add" => {
                    profile_exists(app, name)?;
                    app.db.add_rule("agent", agent, "profile", name)?;
                }
                "rm" | "remove" => {
                    app.db.remove_rule("agent", agent, "profile", name)?;
                }
                _ => bail!("usage: tools {agent} profile add|rm <profile>"),
            }
        }
        _ => bail!("usage: tools {agent} allow|deny|remove <rule> · tools {agent} profile add|rm <profile>"),
    }
    Ok(format!("{}\n(applied at {agent}'s next run)", agent_text(app, agent)?))
}

fn profile_command(app: &App, args: &str) -> Result<String> {
    let (name, rest) = split(args);
    if name.is_empty() {
        return overview_text(app);
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("invalid profile name '{name}'");
    }
    let (verb, rest) = split(rest);
    match verb {
        "allow" => {
            let r = normalize_rule(rest)?;
            app.db.remove_rule("profile", name, "deny", &r)?;
            app.db.add_rule("profile", name, "allow", &r)?;
        }
        "deny" => {
            let r = normalize_rule(rest)?;
            app.db.remove_rule("profile", name, "allow", &r)?;
            app.db.add_rule("profile", name, "deny", &r)?;
        }
        "remove" | "rm" => {
            let r = normalize_rule(rest)?;
            app.db.remove_rule("profile", name, "allow", &r)?;
            app.db.remove_rule("profile", name, "deny", &r)?;
        }
        "delete" => {
            profile_exists(app, name)?;
            app.db.delete_profile(name)?;
            return Ok(format!("profile {name} deleted (and detached from the agents)"));
        }
        _ => bail!("usage: tools profile <name> allow|deny|remove <rule> · tools profile <name> delete"),
    }
    let allow = app.db.rules("profile", name, "allow")?;
    let deny = app.db.rules("profile", name, "deny")?;
    Ok(format!(
        "profile {name}: ✓ {} · ✗ {}\n(applied at the next run of the agents using it)",
        list(&allow),
        list(&deny)
    ))
}

fn split(s: &str) -> (&str, &str) {
    let s = s.trim();
    match s.split_once(char::is_whitespace) {
        Some((a, b)) => (a, b.trim()),
        None => (s, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_rule;

    #[test]
    fn rules() {
        assert_eq!(normalize_rule("`Bash(git push *)`").unwrap(), "Bash(git push *)");
        assert_eq!(normalize_rule("Read").unwrap(), "Read");
        assert_eq!(normalize_rule("mcp__github").unwrap(), "mcp__github");
        assert!(normalize_rule("Bash(git").is_err());
        assert!(normalize_rule("rm -rf /").is_err());
    }
}
