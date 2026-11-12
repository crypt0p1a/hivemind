//! `hive init`: scaffold a company directory.

use anyhow::{bail, Result};
use std::path::Path;

const HIVE_TOML: &str = r#"# Shared with every agent
name = "My Company"
mission = "Describe what the company does, for whom, and the current goal."

# Agent receiving the tasks when --to is not given
default_agent = "ceo"
# Max simultaneous claude sessions (the subscription has a shared quota)
max_parallel_runs = 2
# owner → ceo → cto → dev = depth 3
max_delegation_depth = 4
claude_bin = "claude"

[notify]
macos = true

# Provider usage: no new runs above this ratio, until the window resets
[usage]
pause_above = 0.95
# Other providers: a command printing a JSON usage report (see README)
# [[usage.providers]]
# name = "openai"
# command = "./scripts/openai-usage.sh"
# every_secs = 300

# Tools: when an agent wants a tool it is not allowed, admins decide (Slack buttons, `hive approve`, dashboard)
[tools]
approvals = true
approval_timeout_secs = 1800

# Collaboration between agents: beyond these thresholds (or when a question repeats), a human arbitrates
[collaboration]
max_questions_per_pair = 3
max_questions_per_tree = 12
recruiters = ["ceo", "cto"]   # agents allowed to propose a hire (an admin decides)

# Slack (Socket Mode): see the README to create the app.
# Tokens in HIVE_SLACK_BOT_TOKEN (xoxb-…) and HIVE_SLACK_APP_TOKEN (xapp-…)
# [slack]
# channel = "C0123456789"
# allowed_users = ["U0123456789"]   # first admins; others are managed with `user add` (Slack) or `hive user set`
"#;

const CEO: &str = r#"name = "ceo"
title = "CEO"
capabilities = ["strategy", "prioritization", "breaking down goals", "arbitration", "synthesis"]
model = "opus"
# "default": only allowed tools pass; anything else goes to the admins for approval
permission_mode = "default"
# Initial tool access (then managed live: `tools ceo …` in Slack or `hive tools ceo …`)
profiles = ["read", "web"]
prompt = """
You run the company. You receive the owner's goals, break them down into clear sub-tasks and hand
them to the right people based on their capabilities. You don't code and you don't write marketing
content yourself: you delegate, check the results and write the final synthesis for the owner.
Ask the owner (ask_owner) as soon as a structural decision is theirs to make.
"""
"#;

const CTO: &str = r#"name = "cto"
title = "CTO"
reports_to = "ceo"
capabilities = ["architecture", "technical choices", "technical breakdown", "code review", "estimation"]
model = "opus"
permission_mode = "default"
profiles = ["dev", "web"]
prompt = """
You own the technology. You design the architecture, split the technical work into precise tasks
for the developers, review what they deliver and report to the CEO.
You may code yourself what is short or structural (skeleton, conventions).
"""
"#;

const DEV: &str = r#"name = "dev"
title = "Developer"
reports_to = "cto"
capabilities = ["implementation", "tests", "debugging", "scripts", "API integration"]
model = "sonnet"
permission_mode = "default"
profiles = ["dev", "web"]
prompt = """
You implement what you are given, cleanly and tested. Your result lists the files changed, how to
test, and what is left to do. If the request is ambiguous, pick the simplest option and say so in
your result.
"""
"#;

const MARKETING: &str = r#"name = "marketing"
title = "Head of Marketing"
reports_to = "ceo"
capabilities = ["positioning", "copywriting", "content", "landing page", "social media", "competitive analysis"]
model = "sonnet"
permission_mode = "default"
profiles = ["edit", "web"]
prompt = """
You handle positioning, messaging and content. You write your deliverables in Markdown in the
marketing/ folder of the working directory and give their paths in your result.
"""
"#;

pub fn run(dir: &Path) -> Result<()> {
    run_quiet(dir)?;
    println!(
        "Company created in {}\n\n  1. edit hive.toml (name, mission) and agents/*.toml\n  2. hive daemon            # in a dedicated terminal\n  3. hive task \"…\"          # or hive task --to cto \"…\"",
        dir.display()
    );
    Ok(())
}

/// Writes the company files without printing anything.
pub fn run_quiet(dir: &Path) -> Result<()> {
    if dir.join("hive.toml").exists() {
        bail!("{} already contains a hive.toml", dir.display());
    }
    std::fs::create_dir_all(dir.join("agents"))?;
    std::fs::create_dir_all(dir.join("workspace"))?;
    std::fs::write(dir.join("hive.toml"), HIVE_TOML)?;
    for (name, body) in [("ceo", CEO), ("cto", CTO), ("dev", DEV), ("marketing", MARKETING)] {
        std::fs::write(dir.join("agents").join(format!("{name}.toml")), body)?;
    }
    std::fs::write(dir.join(".gitignore"), ".hive/\n")?;
    Ok(())
}
