//! Line-delimited JSON over the daemon's Unix socket: one request, one response per connection.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    CreateTask {
        to: Option<String>,
        title: String,
        description: String,
    },
    Status,
    Tasks {
        all: bool,
    },
    Show {
        id: i64,
    },
    Reply {
        id: i64,
        message: String,
    },
    Cancel {
        id: i64,
    },
    Retry {
        id: i64,
    },
    /// Structured state for the dashboard (JSON in `text`)
    Snapshot,
    /// Usage of every model provider in its current period(s)
    Usage,
    /// Push a usage report (see usage.rs for the format)
    ReportUsage {
        report: serde_json::Value,
    },
    /// Slack users and their rights
    Users,
    UserSet {
        slack_id: String,
        role: String,
        name: Option<String>,
        agents: Vec<String>,
    },
    UserRemove {
        slack_id: String,
    },
    /// `tools …` command (see tools.rs)
    Tools {
        args: String,
    },
    Approvals,
    Decide {
        id: i64,
        decision: String,
    },
    /// claude's permission prompt, through `hive mcp` (blocks until decided)
    Approve {
        agent: String,
        task_id: i64,
        tool_name: String,
        input: serde_json::Value,
    },
    Proposals {
        all: bool,
    },
    ProposalDecide {
        id: i64,
        accept: bool,
        note: Option<String>,
    },
    AgentList,
    AgentShow {
        name: String,
    },
    AgentAdd {
        spec: crate::agents::AgentSpec,
    },
    AgentRemove {
        name: String,
    },
    AgentReload,
    /// A tool call made by an agent through `hive mcp`.
    Tool {
        agent: String,
        task_id: i64,
        name: String,
        args: serde_json::Value,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub text: String,
}

pub fn call(socket: &Path, req: &Request) -> Result<Response> {
    let mut s = UnixStream::connect(socket)
        .map_err(|e| anyhow!("cannot reach the daemon ({}: {e}). Start it with `hive daemon`.", socket.display()))?;
    s.write_all((serde_json::to_string(req)? + "\n").as_bytes())?;
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}
