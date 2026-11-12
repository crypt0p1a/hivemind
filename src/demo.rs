//! Demo mode: `hive demo` runs a whole company without any Claude subscription.
//!
//! The company's `claude_bin` is `hive-demo`: the runner then starts `hive demo-agent` instead of
//! `claude -p`. That simulated agent speaks the same protocol (prompt on stdin, stream-json on stdout)
//! and acts through the real `hive mcp` bridge, following a small scripted scenario: the CEO splits a
//! launch between marketing and the CTO, the CTO delegates the site to the developer, who asks the CTO
//! a question, and everyone writes real files in the workspace.

use crate::config::socket_path;
use crate::ipc::{self, Request};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const DEMO_BIN: &str = "hive-demo";

pub const DEMO_TASK: &str = "Launch a landing page for our coffee subscription\n\nWe sell freshly roasted coffee \
beans, delivered every month. We need the message, the page itself, and a launch summary.";

// ---------------------------------------------------------------- `hive demo`

/// Creates (or reuses) a demo company, starts its daemon, gives it the demo task and opens the dashboard.
pub fn run(dir: Option<PathBuf>, open_dash: bool) -> Result<()> {
    let dir = dir.unwrap_or_else(|| std::env::temp_dir().join("hive-demo"));
    let fresh = !dir.join("hive.toml").exists();
    if fresh {
        crate::init::run_quiet(&dir)?;
        let toml = std::fs::read_to_string(dir.join("hive.toml"))?
            .replace("name = \"My Company\"", "name = \"Demo Coffee Co\"")
            .replace(
                "mission = \"Describe what the company does, for whom, and the current goal.\"",
                "mission = \"Freshly roasted coffee beans, delivered every month. Goal: launch the subscription.\"",
            )
            .replace("claude_bin = \"claude\"", &format!("claude_bin = \"{DEMO_BIN}\""))
            .replace("macos = true", "macos = false");
        std::fs::write(dir.join("hive.toml"), toml)?;
    }
    let root = dir.canonicalize()?;
    let socket = socket_path(&root);
    std::fs::create_dir_all(root.join(".hive"))?;

    if !open_dash {
        // Foreground daemon (Ctrl-C stops it); the demo task is given as soon as it listens.
        println!("Demo company: {} — press Ctrl-C to stop.", root.display());
        let socket2 = socket.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while ipc::call(&socket2, &Request::Ping).is_err() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
            if fresh {
                let title = crate::util::title_of(DEMO_TASK);
                let _ = ipc::call(&socket2, &Request::CreateTask { to: None, title, description: DEMO_TASK.into() });
            }
        });
        let company = crate::config::Company::load(&root)?;
        return tokio::runtime::Runtime::new()?.block_on(crate::daemon::run(company));
    }

    let log = std::fs::OpenOptions::new().create(true).append(true).open(root.join(".hive/daemon.log"))?;
    let mut daemon = Command::new(std::env::current_exe()?)
        .arg("daemon")
        .env("HIVE_HOME", &root)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .context("starting the demo daemon")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while ipc::call(&socket, &Request::Ping).is_err() {
        if Instant::now() > deadline || daemon.try_wait()?.is_some() {
            let _ = daemon.kill();
            return Err(anyhow!("the demo daemon did not start (see {})", root.join(".hive/daemon.log").display()));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if fresh {
        let title = crate::util::title_of(DEMO_TASK);
        ipc::call(&socket, &Request::CreateTask { to: None, title, description: DEMO_TASK.into() })?;
    }

    let res = crate::dash::run(socket);
    let _ = Command::new("kill").arg("-INT").arg(daemon.id().to_string()).status();
    let _ = daemon.wait();
    println!(
        "Demo company: {}\n  deliverables: {}\n  run it again: hive demo --dir {}",
        root.display(),
        root.join("workspace").display(),
        root.display()
    );
    res
}

// ---------------------------------------------------------------- `hive demo-agent`

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn emit(v: Value) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

fn pause(secs: f64) {
    std::thread::sleep(Duration::from_secs_f64(secs));
}

/// A tiny MCP client: one `hive mcp` process per call.
struct Mcp {
    command: String,
    args: Vec<String>,
}

impl Mcp {
    fn from_config(path: &str) -> Result<Mcp> {
        let cfg: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        let server = &cfg["mcpServers"]["hive"];
        Ok(Mcp {
            command: server["command"].as_str().unwrap_or_default().to_string(),
            args: server["args"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a.as_str())
                .map(String::from)
                .collect(),
        })
    }

    fn call(&self, name: &str, arguments: Value) -> Result<String> {
        let mut p =
            Command::new(&self.command).args(&self.args).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
        let mut stdin = p.stdin.take().unwrap();
        let mut lines = BufReader::new(p.stdout.take().unwrap()).lines();
        let mut wait = |id: i64| -> Result<Value> {
            for line in lines.by_ref() {
                let v: Value = serde_json::from_str(&line?)?;
                if v["id"] == id {
                    return Ok(v);
                }
            }
            Err(anyhow!("MCP server closed"))
        };
        writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}))?;
        wait(1)?;
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": name, "arguments": arguments}})
        )?;
        let res = wait(2)?;
        drop(stdin);
        let _ = p.wait();
        Ok(res["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string())
    }
}

/// Number of demo runs so far (drives the usage gauges).
fn bump_counter(state: &Path) -> u64 {
    let n = std::fs::read_to_string(state).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0) + 1;
    let _ = std::fs::write(state, n.to_string());
    n
}

pub fn agent(args: Vec<String>) -> Result<()> {
    let mut prompt = String::new();
    std::io::stdin().read_to_string(&mut prompt)?;
    let agent = std::env::var("HIVE_AGENT").unwrap_or_default();
    let mcp_path = flag(&args, "--mcp-config").ok_or_else(|| anyhow!("--mcp-config missing"))?.to_string();
    let mcp = Mcp::from_config(&mcp_path)?;
    let session = match (flag(&args, "--resume"), args.iter().any(|a| a == "--fork-session")) {
        (Some(s), false) => s.to_string(),
        _ => format!("demo-{agent}-{}", std::process::id()),
    };
    emit(json!({"type": "system", "subtype": "init", "session_id": session, "model": "demo"}));

    // .hive/runs/run-N.mcp.json → .hive/demo-runs
    let state = Path::new(&mcp_path).parent().and_then(Path::parent).map(|p| p.join("demo-runs"));
    let n = state.as_deref().map(bump_counter).unwrap_or(1);
    let reset = chrono::Utc::now().timestamp() + 3 * 3600 + 1200;
    emit(json!({"type": "rate_limit_event", "session_id": session, "rate_limit_info": {"unifiedWindows": {
        "five_hour": {"utilization": (0.06 * n as f64).min(0.9), "resetsAt": reset, "status": "allowed"},
        "seven_day": {"utilization": (0.21 + 0.015 * n as f64).min(0.9), "resetsAt": reset + 4 * 86400, "status": "allowed"},
    }}}));

    let resumed = prompt.starts_with("Updates on your task");
    let question = prompt.contains("# Question #");
    let say = |text: &str| emit(json!({"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}}));
    let note = |availability: &str, text: &str| {
        let _ = mcp.call("set_status", json!({"availability": availability, "note": text}));
    };

    let result = match (agent.as_str(), resumed, question) {
        ("cto", _, true) => {
            say("Answering the developer's question.");
            pause(2.0);
            "Plain HTML with Tailwind from its CDN: no build step, one index.html, easy to host anywhere.".to_string()
        }
        ("ceo", false, _) => {
            say("Splitting the launch between marketing and tech.");
            mcp.call("list_agents", json!({}))?;
            pause(1.5);
            mcp.call("delegate_task", json!({"to": "marketing", "title": "Positioning and landing page copy",
                "description": "Write the positioning, the headline, three benefits and the call to action for a monthly \
                coffee bean subscription. Deliver marketing/copy.md."}))?;
            pause(1.0);
            mcp.call(
                "delegate_task",
                json!({"to": "cto", "title": "Build and ship the landing page",
                "description": "A one-page site for the subscription: hero, benefits, pricing, sign-up button. Use \
                marketing/copy.md when it exists. Deliver site/index.html."}),
            )?;
            "Delegated the copy to marketing and the site to the CTO.".to_string()
        }
        ("ceo", true, _) => {
            say("Both sub-tasks are done, writing the launch summary.");
            pause(2.0);
            std::fs::write(
                "launch-summary.md",
                "# Launch summary\n\n- Copy: marketing/copy.md\n- Site: site/index.html\n- Next: share the page, \
                 collect the first sign-ups.\n",
            )?;
            "The landing page is ready: copy in marketing/copy.md, page in site/index.html, summary in \
             launch-summary.md."
                .to_string()
        }
        ("marketing", _, _) => {
            note("busy", "writing the landing copy");
            say("Drafting the positioning and the copy.");
            pause(5.0);
            std::fs::create_dir_all("marketing")?;
            std::fs::write(
                "marketing/copy.md",
                "# Fresh beans, every month\n\nRoasted this week, at your door next week.\n\n- Roasted to order\n\
                 - Pause or cancel anytime\n- A new origin every month\n\n**Start my subscription — €14/month**\n",
            )?;
            note("available", "");
            "Copy delivered in marketing/copy.md: headline \"Fresh beans, every month\", three benefits, CTA at €14/month."
                .to_string()
        }
        ("cto", false, _) => {
            say("Planning the page and handing the build to the developer.");
            pause(2.0);
            mcp.call("delegate_task", json!({"to": "dev", "title": "Implement site/index.html",
                "description": "Build the landing page (hero, benefits, pricing, sign-up button) from marketing/copy.md. \
                Ask me if a technical choice is unclear."}))?;
            "Build delegated to the developer.".to_string()
        }
        ("cto", true, _) => {
            say("Reviewing the developer's page.");
            pause(2.5);
            "Reviewed site/index.html: responsive, no build step, ready to host.".to_string()
        }
        ("dev", false, _) => {
            say("Checking the stack with the CTO before starting.");
            pause(1.5);
            mcp.call(
                "ask_agent",
                json!({"to": "cto", "question": "Which stack should the landing page use: a framework \
                with a build step, or plain HTML?"}),
            )?;
            "Waiting for the CTO's answer.".to_string()
        }
        ("dev", true, _) => {
            note("busy", "building the landing page");
            say("Building the page with plain HTML and Tailwind.");
            pause(6.0);
            std::fs::create_dir_all("site")?;
            std::fs::write(
                "site/index.html",
                "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><title>Fresh beans, every month</title>\n\
                 <script src=\"https://cdn.tailwindcss.com\"></script></head>\n<body class=\"bg-amber-50 text-stone-800\">\n\
                 <main class=\"max-w-2xl mx-auto py-24 px-6 text-center\">\n<h1 class=\"text-5xl font-bold\">Fresh beans, \
                 every month</h1>\n<p class=\"mt-4 text-xl\">Roasted this week, at your door next week.</p>\n<a href=\"#\" \
                 class=\"inline-block mt-10 bg-stone-800 text-white px-8 py-4 rounded-full\">Start my subscription — \
                 €14/month</a>\n</main></body></html>\n",
            )?;
            note("available", "");
            "site/index.html written (hero, benefits, pricing, CTA), plain HTML + Tailwind CDN.".to_string()
        }
        _ => {
            say("Working on it.");
            pause(3.0);
            "Done (demo agent).".to_string()
        }
    };

    say(&result);
    emit(json!({"type": "result", "subtype": "success", "is_error": false, "result": result,
        "total_cost_usd": 0.0, "num_turns": 1, "session_id": session}));
    Ok(())
}
