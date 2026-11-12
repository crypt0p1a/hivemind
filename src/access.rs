//! Who may do what from Slack. The CLI / dashboard (local socket) is always admin.
//!
//! - viewer : status, tasks, show, usage
//! - member : + create tasks (optionally only for some agents), act on the tasks they requested
//! - admin  : everything, including user management
//!
//! Users live in the DB (managed live with `user add/rm` or `hive user …`).
//! `[slack] allowed_users` only bootstraps the first admins. With no user at all, Slack is open (everyone admin).

use crate::daemon::App;
use anyhow::{bail, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Viewer,
    Member,
    Admin,
}

impl Role {
    pub fn parse(s: &str) -> Result<Role> {
        Ok(match s.to_lowercase().as_str() {
            "viewer" => Role::Viewer,
            "member" => Role::Member,
            "admin" => Role::Admin,
            _ => bail!("unknown role '{s}' (viewer, member, admin)"),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Member => "member",
            Role::Admin => "admin",
        }
    }
}

#[derive(Clone, Debug)]
pub struct User {
    pub slack_id: String,
    pub name: Option<String>,
    pub role: Role,
    /// Agents a member may give tasks to; empty = all.
    pub agents: Vec<String>,
}

impl User {
    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.slack_id.clone())
    }

    pub fn can_assign(&self, agent: &str) -> bool {
        self.role == Role::Admin || self.agents.is_empty() || self.agents.iter().any(|a| a == agent)
    }
}

/// Reading (status, tasks, show, usage) is allowed to every known user.
pub enum Action<'a> {
    CreateTask {
        to: &'a str,
    },
    /// reply / cancel / retry on a task whose root was requested by `requester`
    ActOnTask {
        requester: Option<&'a str>,
    },
    ManageUsers,
}

pub fn check(user: &User, action: Action) -> Result<()> {
    match action {
        Action::CreateTask { to } => {
            if user.role < Role::Member {
                bail!("your role ({}) cannot create tasks", user.role.as_str());
            }
            if !user.can_assign(to) {
                bail!("you can only give tasks to: {}", user.agents.join(", "));
            }
            Ok(())
        }
        Action::ActOnTask { requester } => {
            if user.role == Role::Admin || (user.role == Role::Member && requester == Some(user.slack_id.as_str())) {
                Ok(())
            } else {
                bail!("only an admin or the person who requested the task can act on it")
            }
        }
        Action::ManageUsers => {
            if user.role != Role::Admin {
                bail!("admins only");
            }
            Ok(())
        }
    }
}

/// The Slack user, if allowed to talk to hive at all.
pub fn resolve(app: &App, slack_id: &str) -> Result<Option<User>> {
    if app.db.user_count()? == 0 {
        return Ok(Some(User { slack_id: slack_id.into(), name: None, role: Role::Admin, agents: vec![] }));
    }
    app.db.user(slack_id)
}

/// `<@U123>`, `<@U123|kevin>` or `U123` → `U123`
pub fn parse_slack_id(s: &str) -> Result<String> {
    let id = s.trim().trim_start_matches("<@").trim_end_matches('>');
    let id = id.split('|').next().unwrap_or(id).trim_start_matches('@');
    if id.len() < 3 || !id.chars().all(|c| c.is_ascii_alphanumeric()) || !id.starts_with(['U', 'W']) {
        bail!("Slack id expected (an @mention or U…), got '{s}'");
    }
    Ok(id.to_string())
}

pub fn users_text(app: &App) -> Result<String> {
    let users = app.db.users()?;
    if users.is_empty() {
        return Ok("No users: Slack is open to the whole channel (everyone is admin).".into());
    }
    let mut s = String::new();
    for u in users {
        s += &format!(
            "{:<12} {:<7} {}{}\n",
            u.slack_id,
            u.role.as_str(),
            u.name.as_deref().unwrap_or("-"),
            if u.agents.is_empty() { String::new() } else { format!(" · agents: {}", u.agents.join(", ")) }
        );
    }
    Ok(s.trim_end().to_string())
}

pub fn set_user(app: &App, u: &User) -> Result<String> {
    for a in &u.agents {
        if app.company.agent(a).is_none() {
            bail!("unknown agent '{a}'");
        }
    }
    if u.role != Role::Admin {
        guard_last_admin(app, &u.slack_id)?;
    }
    app.db.set_user(u)?;
    Ok(format!("{} is now {}", u.label(), u.role.as_str()))
}

pub fn remove_user(app: &App, slack_id: &str) -> Result<String> {
    if app.db.user(slack_id)?.is_none() {
        bail!("{slack_id} is not registered");
    }
    guard_last_admin(app, slack_id)?;
    app.db.remove_user(slack_id)?;
    Ok(format!("{slack_id} removed"))
}

fn guard_last_admin(app: &App, slack_id: &str) -> Result<()> {
    let admins: Vec<User> = app.db.users()?.into_iter().filter(|u| u.role == Role::Admin).collect();
    if admins.len() == 1 && admins[0].slack_id == slack_id {
        bail!("this is the last admin: appoint another admin first");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(role: Role, agents: &[&str]) -> User {
        User { slack_id: "U1".into(), name: None, role, agents: agents.iter().map(|a| a.to_string()).collect() }
    }

    #[test]
    fn rules() {
        let viewer = u(Role::Viewer, &[]);
        let member = u(Role::Member, &["marketing"]);
        let admin = u(Role::Admin, &[]);
        assert!(check(&viewer, Action::CreateTask { to: "ceo" }).is_err());
        assert!(check(&member, Action::CreateTask { to: "marketing" }).is_ok());
        assert!(check(&member, Action::CreateTask { to: "dev" }).is_err());
        assert!(check(&admin, Action::CreateTask { to: "dev" }).is_ok());
        assert!(check(&member, Action::ActOnTask { requester: Some("U1") }).is_ok());
        assert!(check(&member, Action::ActOnTask { requester: Some("U2") }).is_err());
        assert!(check(&member, Action::ActOnTask { requester: None }).is_err());
        assert!(check(&viewer, Action::ActOnTask { requester: Some("U1") }).is_err());
        assert!(check(&admin, Action::ActOnTask { requester: None }).is_ok());
        assert!(check(&member, Action::ManageUsers).is_err());
        assert!(check(&admin, Action::ManageUsers).is_ok());
    }

    #[test]
    fn slack_ids() {
        assert_eq!(parse_slack_id("<@U3WTY95CG>").unwrap(), "U3WTY95CG");
        assert_eq!(parse_slack_id("<@U3WTY95CG|kevin>").unwrap(), "U3WTY95CG");
        assert_eq!(parse_slack_id("U3WTY95CG").unwrap(), "U3WTY95CG");
        assert!(parse_slack_id("@kevin").is_err());
    }
}
