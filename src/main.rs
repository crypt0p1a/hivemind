mod access;
mod agents;
mod approval;
mod collab;
mod config;
mod daemon;
mod dash;
mod db;
mod init;
mod ipc;
mod logs;
mod mcp;
mod notify;
mod runner;
mod tools;
mod usage;
mod util;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::{socket_path, Company};
use ipc::Request;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "hive", version, about = "A team of Claude Code agents that delegate tasks to each other")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a company (hive.toml + agents/) in a directory
    Init {
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Run the orchestrator (in the foreground)
    Daemon,
    /// Give a task to an agent (default: default_agent)
    Task {
        #[arg(long, short)]
        to: Option<String>,
        #[arg(required = true)]
        text: Vec<String>,
    },
    /// Live dashboard in the terminal
    Dash,
    /// Agents state (available / busy / queued)
    Status,
    /// Tree of the open tasks
    Tasks {
        /// Include closed tasks
        #[arg(long, short)]
        all: bool,
    },
    /// Task detail: result, sub-tasks, runs
    Show { id: i64 },
    /// Answer an agent / send a message about a task (reopens a closed main task)
    Reply {
        id: i64,
        #[arg(required = true)]
        text: Vec<String>,
    },
    /// Cancel a task and its sub-tasks
    Cancel { id: i64 },
    /// Retry a failed or cancelled task
    Retry { id: i64 },
    /// Show the log of a run (id) or of an agent's latest run (name)
    Logs {
        target: String,
        #[arg(long, short)]
        follow: bool,
        /// Raw JSON
        #[arg(long)]
        raw: bool,
    },
    /// Model providers usage in the current period
    Usage {
        /// Read a JSON usage report on stdin and send it to the daemon
        #[arg(long)]
        push: bool,
    },
    /// Slack users and their rights (no argument: list)
    User {
        #[command(subcommand)]
        action: Option<UserCmd>,
    },
    /// Agent tools: hive tools [dev [allow|deny|remove <rule> | profile add|rm <p>]] · hive tools profile <p> …
    Tools {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Pending tool approvals
    Approvals,
    /// Answer an approval: hive approve 3 [once|always|deny]
    Approve {
        id: i64,
        #[arg(default_value = "once")]
        decision: String,
    },
    /// Hire proposals from the agents
    Proposals {
        /// Include already handled proposals
        #[arg(long, short)]
        all: bool,
    },
    /// Accept or reject a proposal: hive proposal accept 2 · hive proposal reject 2 "not now"
    Proposal {
        /// accept | reject
        decision: String,
        id: i64,
        note: Vec<String>,
    },
    /// List the configured agents (agents/*.toml files)
    Agents,
    /// Create, remove, reload agents live
    Agent {
        #[command(subcommand)]
        action: AgentCmd,
    },
    /// MCP server spawned by each agent (internal)
    #[command(hide = true)]
    Mcp {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long)]
        agent: String,
        #[arg(long)]
        task: i64,
    },
}

#[derive(Subcommand)]
enum AgentCmd {
    /// Agent sheet (role, capabilities, effective tools, prompt)
    Show { name: String },
    /// Create an agent: hive agent add designer --title "UI Designer" --reports-to cto --profiles edit,web
    Add {
        name: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        reports_to: Option<String>,
        #[arg(long, value_delimiter = ',')]
        capabilities: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        profiles: Vec<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        workdir: Option<String>,
        /// Role prompt
        #[arg(long, default_value = "")]
        prompt: String,
        /// Role prompt read from a file
        #[arg(long)]
        prompt_file: Option<PathBuf>,
    },
    /// Remove an agent (its file is renamed .toml.removed)
    Rm { name: String },
    /// Re-read agents/*.toml (after a hand edit)
    Reload,
}

#[derive(Subcommand)]
enum UserCmd {
    /// Add or update a user: hive user set U0123 member --agents marketing,dev
    Set {
        /// Slack id (U…)
        slack_id: String,
        /// viewer | member | admin
        role: String,
        #[arg(long)]
        name: Option<String>,
        /// Agents a member may give tasks to (empty = all)
        #[arg(long, value_delimiter = ',')]
        agents: Vec<String>,
    },
    /// Remove a user
    Rm { slack_id: String },
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let req = match cli.cmd {
        Cmd::Init { dir } => return init::run(&dir),
        Cmd::Mcp { socket, agent, task } => return mcp::serve(&socket, &agent, task),
        Cmd::Daemon => {
            let company = Company::load(&Company::find()?)?;
            return tokio::runtime::Runtime::new()?.block_on(daemon::run(company));
        }
        Cmd::Dash => return dash::run(socket_path(&Company::find()?)),
        Cmd::Logs { target, follow, raw } => return logs::show(&Company::find()?, &target, follow, raw),
        Cmd::Agents => {
            let c = Company::load(&Company::find()?)?;
            for a in &c.agents() {
                println!(
                    "{:<12} {}{} · {} {} · {}\n             capabilities: {}",
                    a.name,
                    a.title,
                    a.reports_to.as_ref().map(|r| format!(" (→ {r})")).unwrap_or_default(),
                    a.provider,
                    a.model.as_deref().unwrap_or("default"),
                    a.permission_mode,
                    a.capabilities.join(", ")
                );
            }
            return Ok(());
        }
        Cmd::Task { to, text } => {
            let text = text.join(" ");
            Request::CreateTask { to, title: util::title_of(&text), description: text }
        }
        Cmd::Status => Request::Status,
        Cmd::Tasks { all } => Request::Tasks { all },
        Cmd::Show { id } => Request::Show { id },
        Cmd::Reply { id, text } => Request::Reply { id, message: text.join(" ") },
        Cmd::Cancel { id } => Request::Cancel { id },
        Cmd::Retry { id } => Request::Retry { id },
        Cmd::User { action: None } => Request::Users,
        Cmd::User { action: Some(UserCmd::Set { slack_id, role, name, agents }) } => {
            Request::UserSet { slack_id, role, name, agents }
        }
        Cmd::User { action: Some(UserCmd::Rm { slack_id }) } => Request::UserRemove { slack_id },
        Cmd::Agent { action } => match action {
            AgentCmd::Show { name } => Request::AgentShow { name },
            AgentCmd::Rm { name } => Request::AgentRemove { name },
            AgentCmd::Reload => Request::AgentReload,
            AgentCmd::Add { name, title, reports_to, capabilities, profiles, model, workdir, prompt, prompt_file } => {
                let prompt = match prompt_file {
                    Some(f) => std::fs::read_to_string(f)?,
                    None => prompt,
                };
                Request::AgentAdd {
                    spec: agents::AgentSpec { name, title, reports_to, capabilities, profiles, model, workdir, prompt },
                }
            }
        },
        Cmd::Proposals { all } => Request::Proposals { all },
        Cmd::Proposal { decision, id, note } => {
            let accept = match decision.as_str() {
                "accept" => true,
                "reject" => false,
                _ => anyhow::bail!("decision: accept | reject"),
            };
            let note = Some(note.join(" ")).filter(|n| !n.is_empty());
            Request::ProposalDecide { id, accept, note }
        }
        Cmd::Tools { args } => Request::Tools { args: args.join(" ") },
        Cmd::Approvals => Request::Approvals,
        Cmd::Approve { id, decision } => Request::Decide { id, decision },
        Cmd::Usage { push: false } => Request::Usage,
        Cmd::Usage { push: true } => {
            let mut raw = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw)?;
            Request::ReportUsage { report: serde_json::from_str(&raw)? }
        }
    };
    let resp = ipc::call(&socket_path(&Company::find()?), &req)?;
    if resp.ok {
        println!("{}", resp.text);
        Ok(())
    } else {
        anyhow::bail!(resp.text)
    }
}
