//! End-to-end tests: the real `hive` binary (daemon, CLI, MCP bridge) driving a scripted fake
//! `claude` (tests/fixtures/fake_claude.py) that calls the hive tools like a real agent would.

use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const HIVE: &str = env!("CARGO_BIN_EXE_hive");
static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Hive {
    dir: PathBuf,
    socket: PathBuf,
    state: PathBuf,
    plan: PathBuf,
    daemon: Option<Child>,
}

impl Hive {
    /// A fresh company driven by `plan` (see fake_claude.py); `edits` are (from, to) replacements in hive.toml.
    fn new(plan: Value, edits: &[(&str, &str)]) -> Hive {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let base = std::env::temp_dir().join(format!("hive-e2e-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("company");
        let state = base.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let out = Command::new(HIVE).arg("init").arg(&dir).output().unwrap();
        assert!(out.status.success(), "init: {}", String::from_utf8_lossy(&out.stderr));

        let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_claude.py");
        let mut toml = std::fs::read_to_string(dir.join("hive.toml"))
            .unwrap()
            .replace("claude_bin = \"claude\"", &format!("claude_bin = {:?}", fake.display().to_string()))
            .replace("macos = true", "macos = false");
        for (from, to) in edits {
            assert!(toml.contains(from), "hive.toml has no {from:?}");
            toml = toml.replace(from, to);
        }
        std::fs::write(dir.join("hive.toml"), toml).unwrap();
        let plan_path = base.join("plan.json");
        std::fs::write(&plan_path, plan.to_string()).unwrap();

        let mut h = Hive { socket: base.join("hive.sock"), dir, state, plan: plan_path, daemon: None };
        h.start();
        h
    }

    fn env(&self, cmd: &mut Command) {
        cmd.env("HIVE_HOME", &self.dir)
            .env("HIVE_SOCKET", &self.socket)
            .env("HIVE_FAKE_PLAN", &self.plan)
            .env("HIVE_FAKE_STATE", &self.state)
            .env_remove("HIVE_SLACK_BOT_TOKEN")
            .env_remove("HIVE_SLACK_APP_TOKEN");
    }

    fn start(&mut self) {
        let log = std::fs::OpenOptions::new().create(true).append(true).open(self.state.join("daemon.log")).unwrap();
        let mut cmd = Command::new(HIVE);
        cmd.arg("daemon").stdin(Stdio::null()).stdout(log.try_clone().unwrap()).stderr(log);
        self.env(&mut cmd);
        self.daemon = Some(cmd.spawn().unwrap());
        self.wait_for("the daemon socket", || self.run(&["status"]).0);
    }

    /// Graceful stop (SIGINT, like Ctrl-C).
    fn stop(&mut self) {
        if let Some(mut d) = self.daemon.take() {
            let _ = Command::new("kill").arg("-INT").arg(d.id().to_string()).status();
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if d.try_wait().unwrap().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = d.kill();
            let _ = d.wait();
        }
    }

    fn run(&self, args: &[&str]) -> (bool, String) {
        let mut cmd = Command::new(HIVE);
        cmd.args(args);
        self.env(&mut cmd);
        let out = cmd.output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        (out.status.success(), text)
    }

    #[track_caller]
    fn ok(&self, args: &[&str]) -> String {
        let (ok, out) = self.run(args);
        assert!(ok, "hive {args:?} failed:\n{out}\n--- daemon log ---\n{}", self.daemon_log());
        out
    }

    #[track_caller]
    fn err(&self, args: &[&str]) -> String {
        let (ok, out) = self.run(args);
        assert!(!ok, "hive {args:?} should have failed:\n{out}");
        out
    }

    #[track_caller]
    fn wait_for(&self, what: &str, cond: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("timeout waiting for {what}\n--- daemon log ---\n{}", self.daemon_log());
    }

    fn status_of(&self, id: i64) -> String {
        let out = self.run(&["show", &id.to_string()]).1;
        out.lines()
            .find_map(|l| l.strip_prefix("status: "))
            .and_then(|l| l.split(' ').next())
            .unwrap_or("?")
            .to_string()
    }

    #[track_caller]
    fn wait_status(&self, id: i64, status: &str) {
        self.wait_for(&format!("task #{id} to be {status}"), || self.status_of(id) == status);
    }

    fn jsonl(&self, name: &str) -> Vec<Value> {
        std::fs::read_to_string(self.state.join(name))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Invocations of the fake claude by `agent`, in order.
    fn calls(&self, agent: &str) -> Vec<Value> {
        self.jsonl("calls.jsonl").into_iter().filter(|c| c["agent"] == agent).collect()
    }

    /// Tool answers received by the fake agents.
    fn results(&self, tool: &str) -> Vec<Value> {
        self.jsonl("results.jsonl").into_iter().filter(|r| r["tool"] == tool).collect()
    }

    fn daemon_log(&self) -> String {
        std::fs::read_to_string(self.state.join("daemon.log")).unwrap_or_default()
    }
}

impl Drop for Hive {
    fn drop(&mut self) {
        self.stop();
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(self.dir.parent().unwrap());
        }
    }
}

fn args(call: &Value) -> Vec<String> {
    call["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect()
}

/// Value following `flag` in the argv of a call.
fn flag(call: &Value, name: &str) -> Option<String> {
    let a = args(call);
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1).cloned())
}

fn session_of(h: &Hive, agent: &str, run: usize) -> String {
    // The session id is printed by the fake in its first stream line; hive stores it and passes it
    // back with --resume: read it from the run log.
    let log = std::fs::read_dir(h.dir.join(".hive/logs"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(&format!("-{agent}.jsonl")))
        .collect::<Vec<_>>();
    let mut logs = log;
    logs.sort_by_key(|p| p.file_name().unwrap().to_string_lossy().split('-').nth(1).unwrap().parse::<i64>().unwrap());
    let text = std::fs::read_to_string(&logs[run - 1]).unwrap();
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "system")
        .and_then(|v| v["session_id"].as_str().map(String::from))
        .unwrap()
}

// ---------------------------------------------------------------- tests

#[test]
fn delegation_round_trip_resumes_the_delegator() {
    let h = Hive::new(
        json!({
            "ceo": [
                [{"tool": "delegate_task", "args": {"to": "dev", "title": "Write the page", "description": "index.html"}}],
                [{"tool": "complete_task", "args": {"result": "launch ready"}}]
            ],
            "dev": [[{"tool": "complete_task", "args": {"result": "page written"}}]]
        }),
        &[],
    );
    assert!(h.ok(&["task", "Launch the product"]).contains("Task #1 assigned to ceo"));
    h.wait_status(1, "done");

    let show = h.ok(&["show", "1"]);
    assert!(show.contains("launch ready"), "{show}");
    assert!(show.contains("#2 [done] dev"), "{show}");
    assert_eq!(h.status_of(2), "done");

    // The ceo was woken up once, in the same session, with the sub-task result.
    let ceo = h.calls("ceo");
    assert_eq!(ceo.len(), 2);
    assert!(ceo[1]["prompt"].as_str().unwrap().contains("page written"));
    assert_eq!(flag(&ceo[1], "--resume"), Some(session_of(&h, "ceo", 1)));
    // The delegated agent got a self-contained assignment.
    let dev = h.calls("dev");
    assert!(dev[0]["prompt"].as_str().unwrap().contains("Requested by: ceo (sub-task of #1"));

    // A closed sub-task cannot be reopened directly.
    assert!(h.err(&["reply", "2", "again"]).contains("is a sub-task"));
    let tree = h.ok(&["tasks", "-a"]);
    assert!(tree.contains("#1 [done] ceo") && tree.contains("  #2 [done] dev"), "{tree}");
}

#[test]
fn last_message_becomes_the_result() {
    let h = Hive::new(json!({"dev": [[{"result": "just text"}]]}), &[]);
    h.ok(&["task", "--to", "dev", "Say something"]);
    h.wait_status(1, "done");
    assert!(h.ok(&["show", "1"]).contains("just text"));
    // Readable log of the agent's latest run, and the raw stream.
    let log = h.ok(&["logs", "dev"]);
    assert!(
        log.contains("▶ run 1 — dev on task #1") && log.contains("■ end (success") && log.contains("just text"),
        "{log}"
    );
    assert!(h.ok(&["logs", "1", "--raw"]).contains("\"type\": \"result\""));
    assert!(h.err(&["logs", "nobody"]).contains("no log for 'nobody'"));
}

#[test]
fn delegation_guardrails() {
    let h = Hive::new(
        json!({
            "cto": [[{"tool": "delegate_task", "args": {"to": "dev", "title": "t", "description": "d"}}]],
            "dev": [[
                {"tool": "delegate_task", "args": {"to": "dev", "title": "t", "description": "d"}},
                {"tool": "delegate_task", "args": {"to": "cto", "title": "t", "description": "d"}},
                {"tool": "delegate_task", "args": {"to": "nobody", "title": "t", "description": "d"}},
                {"tool": "complete_task", "args": {"result": "done"}}
            ]]
        }),
        &[],
    );
    h.ok(&["task", "--to", "cto", "Build it"]);
    h.wait_status(1, "done");
    let r: Vec<String> = h.results("delegate_task").iter().map(|r| r["text"].as_str().unwrap().to_string()).collect();
    assert!(r[0].starts_with("Sub-task #2 assigned to dev"), "{r:?}");
    assert!(r[1].contains("cannot delegate a task to yourself"), "{r:?}");
    assert!(r[2].contains("already upstream"), "{r:?}");
    assert!(r[3].contains("unknown agent"), "{r:?}");
}

#[test]
fn ask_agent_answers_from_a_fork_and_loops_go_to_a_human() {
    let h = Hive::new(
        json!({
            "cto": [
                [{"tool": "delegate_task", "args": {"to": "dev", "title": "Write port.txt", "description": "ask me the port"}}],
                [{"tool": "complete_task", "args": {"result": "8080"}}],
                [{"tool": "complete_task", "args": {"result": "all good"}}]
            ],
            "dev": [
                [{"tool": "ask_agent", "args": {"to": "cto", "question": "Which port should the server use?"}}],
                [{"tool": "ask_agent", "args": {"to": "cto", "question": "And which host?"}}],
                [{"tool": "complete_task", "args": {"result": "port 8080 on localhost"}}]
            ]
        }),
        &[("max_questions_per_pair = 3", "max_questions_per_pair = 1")],
    );
    h.ok(&["task", "--to", "cto", "Configure the server"]);
    // The second question exceeds the pair limit: the dev's task waits for a human.
    h.wait_status(2, "blocked");
    let asks = h.results("ask_agent");
    assert!(asks[0]["text"].as_str().unwrap().starts_with("Question #3 sent to cto"), "{asks:?}");
    assert!(asks[1]["text"].as_str().unwrap().starts_with("Too many back-and-forths"), "{asks:?}");
    assert!(h.daemon_log().contains("⚖️ arbitration on #2"));

    // The cto answered the question from a fork of its own session on #1.
    let cto = h.calls("cto");
    assert_eq!(cto[1]["task"], 3);
    assert!(args(&cto[1]).contains(&"--fork-session".to_string()));
    assert_eq!(flag(&cto[1], "--resume"), Some(session_of(&h, "cto", 1)));
    // The answer came back to the dev.
    assert!(h.calls("dev")[1]["prompt"].as_str().unwrap().contains("Answer from cto to your question #3:\n8080"));
    assert!(h.ok(&["tasks", "-a"]).contains("❓#3"));

    // The human decides; the dev resumes and the whole chain completes.
    h.ok(&["reply", "2", "Use localhost"]);
    h.wait_status(1, "done");
    assert!(h.calls("dev")[2]["prompt"].as_str().unwrap().contains("Use localhost"));
    assert!(h.ok(&["show", "1"]).contains("all good"));
}

#[test]
fn answering_agent_cannot_delegate_or_ask() {
    let h = Hive::new(
        json!({
            "dev": [[{"tool": "ask_agent", "args": {"to": "marketing", "question": "Slogan?"}}]],
            "marketing": [[
                {"tool": "delegate_task", "args": {"to": "dev", "title": "t", "description": "d"}},
                {"tool": "ask_agent", "args": {"to": "ceo", "question": "Budget?"}},
                {"tool": "complete_task", "args": {"result": "Ship it"}}
            ]]
        }),
        &[],
    );
    h.ok(&["task", "--to", "dev", "Landing page"]);
    h.wait_status(1, "done");
    let d = &h.results("delegate_task")[0];
    assert!(d["is_error"].as_bool().unwrap() && d["text"].as_str().unwrap().contains("answering a question"));
    let a = &h.results("ask_agent")[1];
    assert!(a["is_error"].as_bool().unwrap() && a["text"].as_str().unwrap().contains("answering a question"));
}

#[test]
fn hire_proposal_is_decided_by_an_admin() {
    let h = Hive::new(
        json!({
            "ceo": [
                [{"tool": "propose_agent", "args": {
                    "name": "designer", "title": "UI Designer", "reports_to": "cto",
                    "capabilities": ["ui", "mockups"], "profiles": ["edit"], "model": "haiku",
                    "prompt": "You design the interfaces.", "reason": "Nobody can do mockups."}}],
                [{"tool": "delegate_task", "args": {"to": "designer", "title": "Mockup", "description": "home page"}}],
                [{"tool": "complete_task", "args": {"result": "designed"}}]
            ],
            "dev": [[{"tool": "propose_agent", "args": {
                "name": "x", "title": "X", "capabilities": [], "prompt": "p", "reason": "r"}}]]
        }),
        &[],
    );
    h.ok(&["task", "--to", "dev", "Try to hire"]);
    h.wait_status(1, "done");
    assert!(h.results("propose_agent")[0]["text"].as_str().unwrap().contains("only ceo, cto can propose"));

    h.ok(&["task", "We need mockups"]);
    h.wait_status(2, "blocked");
    assert!(h.ok(&["proposals"]).contains("#1 [pending] ceo proposes designer — UI Designer (→ cto)"));
    assert!(h.ok(&["proposal", "accept", "1"]).contains("designer joined the team"));
    assert!(h.err(&["proposal", "reject", "1"]).contains("already handled"));
    assert!(h.dir.join("agents/designer.toml").exists());

    h.wait_status(2, "done");
    assert!(h.calls("ceo")[1]["prompt"].as_str().unwrap().contains("Hire accepted"));
    assert_eq!(h.calls("designer").len(), 1);
    assert!(h.ok(&["tools", "designer"]).contains("profiles     : edit"));
}

#[test]
fn reply_reopens_a_finished_task_in_the_same_session() {
    let h = Hive::new(json!({"marketing": [[{"result": "Fresh bread daily"}], [{"result": "Du pain frais"}]]}), &[]);
    h.ok(&["task", "--to", "marketing", "A slogan"]);
    h.wait_status(1, "done");
    assert!(h.ok(&["reply", "1", "Translate it to French"]).contains("Task #1 reopened"));
    h.wait_for("the second run", || h.calls("marketing").len() == 2);
    h.wait_status(1, "done");
    let m = h.calls("marketing");
    let prompt = m[1]["prompt"].as_str().unwrap();
    assert!(
        prompt.contains("is reopened")
            && prompt.contains("Translate it to French")
            && prompt.contains("Fresh bread daily")
    );
    assert_eq!(flag(&m[1], "--resume"), Some(session_of(&h, "marketing", 1)));
    assert!(h.ok(&["show", "1"]).contains("Du pain frais"));
}

#[test]
fn cancel_stops_the_run_and_retry_resumes() {
    let h = Hive::new(json!({"dev": [[{"sleep": 60}], [{"result": "second try"}]]}), &[]);
    h.ok(&["task", "--to", "dev", "Long job"]);
    h.wait_for("dev's first run to start", || h.calls("dev").len() == 1);
    assert!(h.ok(&["cancel", "1"]).contains("1 task(s) cancelled"));
    h.wait_status(1, "cancelled");
    h.wait_for("dev to be available", || h.ok(&["status"]).contains("available"));
    assert!(h.ok(&["show", "1"]).contains("· killed ·"));

    h.ok(&["retry", "1"]);
    h.wait_status(1, "done");
    assert!(h.ok(&["show", "1"]).contains("second try"));
    assert!(h.err(&["retry", "1"]).contains("only failed/cancelled tasks"));
}

#[test]
fn tool_rules_are_passed_and_approvals_add_rules() {
    let h = Hive::new(
        json!({"dev": [[
            {"approve": {"tool_name": "Bash", "input": {"command": "make deploy"}}},
            {"approve": {"tool_name": "WebFetch", "input": {"url": "https://example.com/x"}}},
            {"result": "deployed"}
        ]]}),
        &[],
    );
    h.ok(&["tools", "dev", "profile", "rm", "dev"]);
    h.ok(&["tools", "dev", "allow", "Bash(npm test)"]);
    h.ok(&["tools", "dev", "deny", "Bash(rm *)"]);
    assert!(h.err(&["tools", "dev", "allow", "Bash(oops"]).contains("invalid rule"));

    h.ok(&["task", "--to", "dev", "Deploy"]);
    h.wait_for("the first approval", || h.ok(&["approvals"]).contains("#1 · dev (task #1) wants Bash: `make deploy`"));
    assert!(h.ok(&["status"]).contains("waiting for approval"));
    h.ok(&["approve", "1", "always"]);
    h.wait_for("the second approval", || h.ok(&["approvals"]).contains("#2 · dev"));
    h.ok(&["approve", "2", "deny"]);
    h.wait_status(1, "done");

    let r = h.results("approve");
    let first: Value = serde_json::from_str(r[0]["text"].as_str().unwrap()).unwrap();
    let second: Value = serde_json::from_str(r[1]["text"].as_str().unwrap()).unwrap();
    assert_eq!(first["behavior"], "allow");
    assert_eq!(first["updatedInput"]["command"], "make deploy");
    assert_eq!(second["behavior"], "deny");
    assert!(h.ok(&["tools", "dev"]).contains("Bash(make deploy)"));

    let a = args(&h.calls("dev")[0]);
    let allowed_at = a.iter().position(|x| x == "--allowedTools").unwrap();
    assert!(a[allowed_at..].contains(&"Bash(npm test)".to_string()));
    assert!(a[allowed_at..].contains(&"mcp__hive".to_string()));
    assert!(!a[allowed_at..].contains(&"Bash".to_string()), "the dev profile was removed: {a:?}");
    assert_eq!(flag(&h.calls("dev")[0], "--disallowedTools"), Some("Bash(rm *)".into()));
    assert_eq!(flag(&h.calls("dev")[0], "--permission-prompt-tool"), Some("mcp__hive__approve".into()));
}

#[test]
fn exhausted_quota_requeues_instead_of_failing() {
    let h = Hive::new(
        json!({"dev": [[
            {"rate_limit": {"five_hour": {"utilization": 1.0, "resets_in": 3600, "status": "rejected"},
                            "seven_day": {"utilization": 0.4, "resets_in": 300000, "status": "allowed"}}},
            {"fail": "Claude usage limit reached"}
        ]]}),
        &[],
    );
    h.ok(&["task", "--to", "dev", "Heavy work"]);
    h.wait_for("the quota pause", || h.daemon_log().contains("⏸ #1 (dev): claude quota reached"));
    assert_ne!(h.status_of(1), "failed");
    let status = h.ok(&["status"]);
    assert!(status.contains("paused (quota"), "{status}");
    let usage = h.ok(&["usage"]);
    assert!(usage.contains("five_hour") && usage.contains("100%") && usage.contains("rejected"), "{usage}");

    // Other providers push their own windows.
    let mut push = Command::new(HIVE);
    push.args(["usage", "--push"]).stdin(Stdio::piped()).stdout(Stdio::piped());
    h.env(&mut push);
    let mut child = push.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"provider":"openai","windows":[{"name":"monthly","used":12.5,"limit":50,"unit":"usd"}]}"#)
        .unwrap();
    assert!(child.wait().unwrap().success());
    assert!(h.ok(&["usage"]).contains("25% 12.5/50 usd"));
}

#[test]
fn interrupted_runs_resume_after_a_restart() {
    let mut h = Hive::new(json!({"dev": [[{"sleep": 60}], [{"result": "finished after restart"}]]}), &[]);
    h.ok(&["task", "--to", "dev", "Survive a restart"]);
    h.wait_for("dev's first run to start", || h.calls("dev").len() == 1);
    h.stop();
    h.start();
    h.wait_status(1, "done");
    let dev = h.calls("dev");
    assert!(dev[1]["prompt"].as_str().unwrap().contains("previous session on this task was interrupted"));
    assert!(h.ok(&["show", "1"]).contains("· aborted ·"));
}

#[test]
fn agents_and_users_are_managed_live() {
    let h = Hive::new(json!({}), &[]);
    let added = h.ok(&[
        "agent",
        "add",
        "designer",
        "--title",
        "UI Designer",
        "--reports-to",
        "cto",
        "--capabilities",
        "ui,mockups",
        "--profiles",
        "edit,web",
        "--prompt",
        "You design.",
    ]);
    assert!(added.contains("designer — UI Designer (reports to cto)"), "{added}");
    assert!(h.err(&["agent", "add", "designer", "--title", "X"]).contains("already exists"));
    assert!(h.err(&["agent", "add", "x", "--title", "X", "--profiles", "nope"]).contains("unknown profile"));
    assert!(h.err(&["agent", "rm", "ceo"]).contains("default agent"));
    assert!(h.err(&["agent", "rm", "cto"]).contains("report to cto"));
    assert!(h.ok(&["agent", "rm", "designer"]).contains("designer removed"));
    assert!(h.dir.join("agents/designer.toml.removed").exists());
    assert!(!h.ok(&["agent", "reload"]).contains("designer"));

    assert!(h.ok(&["user"]).contains("No users"));
    h.ok(&["user", "set", "U0ADMIN", "admin", "--name", "Kevin"]);
    h.ok(&["user", "set", "U0MEMBER", "member", "--agents", "marketing"]);
    assert!(h.err(&["user", "set", "U0X", "member", "--agents", "nope"]).contains("unknown agent"));
    assert!(h.err(&["user", "rm", "U0ADMIN"]).contains("last admin"));
    let users = h.ok(&["user"]);
    assert!(users.contains("U0ADMIN") && users.contains("Kevin") && users.contains("agents: marketing"), "{users}");
    h.ok(&["user", "rm", "U0MEMBER"]);
}

#[test]
fn demo_mode_runs_a_whole_company_without_claude() {
    let base = std::env::temp_dir().join(format!("hive-e2e-demo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let (dir, socket) = (base.join("company"), base.join("hive.sock"));
    let mut demo = Command::new(HIVE)
        .args(["demo", "--no-dash", "--dir"])
        .arg(&dir)
        .env("HIVE_SOCKET", &socket)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let show =
        || Command::new(HIVE).args(["show", "1"]).env("HIVE_HOME", &dir).env("HIVE_SOCKET", &socket).output().unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut last = String::new();
    while Instant::now() < deadline {
        last = String::from_utf8_lossy(&show().stdout).to_string();
        if last.contains("status: done") {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = Command::new("kill").arg("-INT").arg(demo.id().to_string()).status();
    let _ = demo.wait();
    assert!(last.contains("status: done"), "the demo did not finish:\n{last}");
    assert!(last.contains("#2 [done] marketing") && last.contains("#3 [done] cto"), "{last}");
    for f in ["marketing/copy.md", "site/index.html", "launch-summary.md"] {
        assert!(dir.join("workspace").join(f).exists(), "{f} missing");
    }
    let _ = std::fs::remove_dir_all(&base);
}
