# hive

[![CI](https://github.com/nonopn/hivemind/actions/workflows/ci.yml/badge.svg)](https://github.com/nonopn/hivemind/actions/workflows/ci.yml)

A team of Claude Code agents (CEO, CTO, developers, marketing…) that split the work according to their
capabilities, ask each other questions and report back to you. Each agent is a `claude -p` process: it runs
on your Claude subscription.

```
you ── CLI / dashboard / Slack ──▶ hive daemon ──spawn──▶ claude -p (ceo, task #1)
                                     ▲  SQLite                 │
                                     └──── Unix socket ◀── hive mcp (delegate_task, ask_agent,
                                                            complete_task, ask_owner, list_agents…)
```

- [Quick start](#quick-start) · [Docker](#docker) · [Commands](#commands) · [How it works](#how-it-works)
- [Agents](#agents) · [Collaboration](#collaboration-between-agents) · [Agent tools](#agent-tools)
- [Usage & quotas](#provider-usage-api) · [Slack](#slack) · [Rights](#rights) · [Development](#development)

## Quick start

```sh
cargo install --path .
hive init ~/my-company && cd ~/my-company   # hive.toml + agents/*.toml + workspace/
hive daemon                                 # terminal 1: the orchestrator
hive dash                                   # terminal 2: the dashboard
```

In the dashboard, press `n` to create a task (`@cto …` to target an agent). Deliverables land in `workspace/`.

## Docker

The recommended way to run it: agents (and their shell access) are isolated in the container.

```sh
cp .env.example .env            # CLAUDE_CODE_OAUTH_TOKEN, Slack tokens
claude setup-token              # on your machine: long-lived token tied to your Claude subscription
mkdir company && docker compose run --rm hive init /company
docker compose up -d
docker compose exec hive hive dash
```

| Command | Effect |
|---|---|
| `docker compose up -d --build` | rebuild the image after a code change, then restart |
| `docker compose exec hive hive <command>` | any hive command (`status`, `task "…"`, `dash`…) |
| `docker compose logs -f` | daemon activity |
| `docker compose restart` | after editing `company/hive.toml` |
| `docker compose down` | stop |

- `./company` is mounted in the container: edit the configuration from the host, find deliverables in
  `company/workspace/`.
- The `claude-home` volume keeps the Claude sessions (needed for agents to resume their conversations).
- The container runs as a non-root user (`HIVE_UID`, 1000 by default; use `HIVE_UID=$(id -u)` on Linux).
- Add the tools your agents need (node, cargo…) to the `Dockerfile`.

## Commands

| Command | Purpose |
|---|---|
| `hive init [dir]` | create a company |
| `hive daemon` | run the orchestrator |
| `hive dash` | live dashboard: agents, usage, task tree, detail, activity |
| `hive task [--to agent] "…"` | new task (for `default_agent` by default) |
| `hive status` | agents: available / busy (#tasks) / queued / paused (quota) / waiting for approval |
| `hive tasks [-a]` | task tree (`-a`: include closed tasks) |
| `hive show <id>` | description, result, sub-tasks, questions, runs and cost |
| `hive reply <id> "…"` | answer an agent; reopens a closed main task |
| `hive cancel <id>` / `hive retry <id>` | cancel a sub-tree / retry a failed task |
| `hive logs <agent\|run> [-f] [--raw]` | what the agent does: text, tool calls, result |
| `hive usage [--push]` | provider quotas / push a usage report (stdin) |
| `hive tools [agent …]` | agent tools: view, allow, deny, profiles (live) |
| `hive approvals` / `hive approve <id> [once\|always\|deny]` | pending tool approvals |
| `hive proposals [-a]` / `hive proposal accept\|reject <id> [reason]` | hires proposed by the agents |
| `hive agents` / `hive agent add\|show\|rm\|reload` | the team; create and remove agents live |
| `hive user [set\|rm …]` | Slack users and their rights |

The company directory is found by walking up to the closest `hive.toml`, or through `HIVE_HOME`.

Dashboard keys: `↑↓` task · `n` new · `r` reply · `c` cancel · `R` retry · `tab` show/hide closed tasks ·
`J/K` scroll the detail · `y`/`Y`/`x` allow once / always / deny the oldest tool request · `q` quit
(agents keep working).

## How it works

- **Tasks**: `todo → in_progress → waiting (sub-tasks) / blocked (human needed) → done | failed | cancelled`.
- **Delegation**: an agent calls `delegate_task(to, title, description)`, then ends its turn. It is resumed
  (same Claude session, `--resume`) **once**, when all its sub-tasks are closed, with their results: no polling,
  no wasted quota.
- **Completion**: `complete_task(result)` sends the result up to the parent. A finished owner task triggers a
  notification (terminal, macOS, Slack). If an agent ends its turn without calling it, its last message
  becomes the result.
- **Reopening**: a message on a finished, failed or cancelled main task reopens it; its agent resumes the
  same conversation with the new request and its previous result. Acknowledgements ("thanks", "ok"…) don't.
- **Availability**: an agent is busy when a process runs for it; it can also declare itself busy with a note
  (`set_status`). `max_concurrent` (per agent) and `max_parallel_runs` (global) protect the quota.
- **Guardrails**: max delegation depth, no delegation to oneself or to an agent already upstream in the chain,
  loop detection with human arbitration, per-agent tools with approvals.
- **Restarts**: interrupted runs are resumed automatically at the next start.

## Agents

One file per agent in `agents/`:

```toml
name = "frontend-dev"
title = "Frontend developer"
reports_to = "cto"
capabilities = ["react", "css", "accessibility"]
provider = "claude"              # provider whose quota is tracked
model = "sonnet"                 # alias or full id
max_concurrent = 1
workdir = "workspace"            # relative to the company directory, or absolute (e.g. a repo)
permission_mode = "default"
profiles = ["dev", "web"]        # initial tool access (see Agent tools)
prompt = """Your role, your rules, what your result must contain…"""
```

`profiles` / `allowed_tools` are only used **to seed** the agent: on first start they are copied to the
database, then tool access is managed live.

### Creating / removing agents on the fly

No restart needed. `agents/<name>.toml` stays the source of truth: creating an agent writes its file,
removing one renames it `.toml.removed`, and `reload` picks up hand edits.

```sh
hive agent add designer --title "UI Designer" --reports-to cto \
  --capabilities ui,mockups --profiles edit,web --model sonnet \
  --prompt "You design simple, accessible interfaces."     # or --prompt-file role.md
hive agent show designer
hive agent rm designer        # refused with open tasks, subordinates, or for the default agent
hive agent reload             # after editing agents/*.toml
```

In Slack (admins), the role prompt goes on the following lines:

```
agent add designer title="UI Designer" reports_to=cto capabilities=ui,mockups profiles=edit,web model=sonnet
You design simple, accessible interfaces. You deliver HTML/CSS in design/.
```

The other agents see the newcomer at their next run (team description, `list_agents`).

## Collaboration between agents

- **`delegate_task`**: hand a sub-task to any teammate (never to an agent already upstream in the chain).
- **`ask_agent`**: ask any teammate a question, **including the one who gave the task**. The answering agent
  answers from the context of its own task (its session is forked with `--fork-session`, the original is left
  untouched). While answering, it can neither delegate nor ask another question: no chain can form. The answer
  flows back into the asker's task (visible with `hive show`, marked `?` in the tree) and previous exchanges
  are recalled with every new question.
- **Human arbitration**: when two agents already exchanged `max_questions_per_pair` times on a task, when the
  task received `max_questions_per_tree` questions, or when a question repeats an answered one, it is not
  forwarded. The task becomes `blocked` and the arbitration lands in the Slack thread (and as a notification):
  your answer in the thread (or `hive reply <id> "…"`) goes to the agent.
- **`propose_agent`**: recruiters (`ceo`, `cto` by default) propose a new agent when a capability is missing.
  An admin accepts (the agent is created on the spot) or rejects: Slack buttons,
  `proposal accept|reject <id> [reason]` in Slack, or `hive proposals` / `hive proposal accept 2`. The proposer
  waits for the decision, then resumes.

```toml
[collaboration]
max_questions_per_pair = 3
max_questions_per_tree = 12
recruiters = ["ceo", "cto"]
```

## Agent tools

Rules live in the database and are read at every run: a change applies at the agent's next run, without
restart. They use Claude Code's syntax: `Read`, `Bash`, `Bash(npm test)`, `Bash(git *)`, `Edit(src/**)`,
`WebFetch(domain:github.com)`, `mcp__server`.

- **Profiles**: named rule sets attached to agents. Builtin: `read` (Read, Glob, Grep), `web` (WebSearch,
  WebFetch), `edit` (+ Edit, Write), `dev` (+ Bash, except `git push` and `rm -rf`).
- **Own rules** of an agent, on top of its profiles. A deny always wins.

```sh
hive tools                                   # overview
hive tools dev                               # detail (profiles, rules, effective)
hive tools dev allow "Bash(npm test)"
hive tools dev deny "Bash(git push *)"
hive tools dev remove "Bash(npm test)"
hive tools dev profile add web               # / profile rm web
hive tools profile deploy allow "Bash(./deploy.sh)"   # creates or updates a profile
hive tools profile deploy delete
```

Same syntax in Slack (`tools dev allow Bash(npm test)`), admins only.

**On-demand approvals**: when an agent wants a tool that is neither allowed nor denied, it doesn't get a flat
refusal. A request goes to the admins: buttons in the task's Slack thread (**Allow once** / **Always** /
**Deny**), `hive approve <id> once|always|deny`, or `y`/`Y`/`x` in the dashboard. "Always" adds the rule to the
agent. Without an answer within `approval_timeout_secs` (30 min by default) it is a deny. Disable with
`[tools] approvals = false`.

## Provider usage (API)

Each provider exposes one or more **windows** (5 hours, 7 days, month…):

```json
{"provider": "openai",
 "windows": [{"name": "monthly", "used": 12.5, "limit": 50, "unit": "usd",
              "utilization": 0.25, "resets_at": 1795000000, "status": "allowed"}]}
```

Only `name` is required per window; `utilization` (0..1) is derived from `used/limit` when missing, `resets_at`
is in Unix seconds. Three ways to feed it:

- **claude**: automatic, from the `rate_limit_event`s emitted by every run (`five_hour`, `seven_day` windows).
- **push**: `echo '{…}' | hive usage --push` (or the `report_usage` op on the socket).
- **pull**: a command printing the JSON, run periodically:

  ```toml
  [[usage.providers]]
  name = "openai"
  command = "./scripts/openai-usage.sh"
  every_secs = 300
  ```

Above `[usage] pause_above` (0.95 by default) on a window, or when a provider answers `rejected`, no new run is
started for the agents of that provider until the reset. A run that fails because of the quota doesn't fail the
task: it resumes automatically after the reset.

## Slack

The bot uses **Socket Mode**: no public URL, the daemon only needs outbound Internet access.

1. https://api.slack.com/apps → *Create New App* → *From a manifest*:

   ```yaml
   display_information:
     name: Hive
   features:
     bot_user:
       display_name: hive
       always_online: true
   oauth_config:
     scopes:
       bot: [chat:write, channels:history, groups:history]
   settings:
     event_subscriptions:
       bot_events: [message.channels, message.groups]
     interactivity:
       is_enabled: true          # approval / hire buttons
     socket_mode_enabled: true
   ```
2. *Basic Information → App-Level Tokens*: a token with `connections:write` → `HIVE_SLACK_APP_TOKEN` (xapp-…).
3. *Install App* → *Bot User OAuth Token* → `HIVE_SLACK_BOT_TOKEN` (xoxb-…).
4. In Slack: create a channel, `/invite @hive`, and copy its id (channel details, `C…`).
5. In `hive.toml`:

   ```toml
   [slack]
   channel = "C0123456789"
   allowed_users = ["U0123456789"]   # first admins (your Slack member id)
   ```

Every main task has its **thread**: delegations, questions between agents, finished sub-tasks, approvals,
hires. Final results, failures and questions are also broadcast to the channel, mentioning the requester.

- In the channel: a plain message → new task; `@cto …` → for a given agent; `status`, `tasks [all]`, `show 3`,
  `usage`, `cancel 3`, `retry 3`, `reply 3 …`, `agents`, `tools`, `approvals`, `proposals`, `me`, `help`.
- In a task thread: a message goes to the agent (first to the one waiting for an answer) and reopens the task
  if it is finished; `cancel`, `retry`, `show`.

## Rights

| Role | Can |
|---|---|
| `viewer` | read: `status`, `tasks`, `show`, `usage`, `tools`, `approvals`, `agents` |
| `member` | + create tasks (optionally only for some agents), act on **their own** tasks |
| `admin` | everything: users, agents, tools, approvals, hires, all tasks |

`allowed_users` creates the first admins; then: `users`, `user add @alice member agents=marketing,dev name=Alice`,
`user rm @alice` (Slack, admins) or `hive user set U0123 member --agents marketing --name Alice` (CLI). `me`
shows your rights. The last admin cannot be removed or demoted. Messages from people without access are
ignored. The CLI and the dashboard (local socket) have all rights.

## Configuration reference

```toml
name = "My Company"
mission = "What the company does, for whom, and the current goal."
default_agent = "ceo"
max_parallel_runs = 2
max_delegation_depth = 4
claude_bin = "claude"

[notify]
macos = true

[usage]
pause_above = 0.95

[tools]
approvals = true
approval_timeout_secs = 1800

[collaboration]
max_questions_per_pair = 3
max_questions_per_tree = 12
recruiters = ["ceo", "cto"]

[slack]
channel = "C0123456789"
allowed_users = ["U0123456789"]
```

| Environment variable | Purpose |
|---|---|
| `CLAUDE_CODE_OAUTH_TOKEN` | Claude subscription (`claude setup-token`), mostly for Docker |
| `HIVE_SLACK_BOT_TOKEN` / `HIVE_SLACK_APP_TOKEN` | Slack bot token (xoxb-…) / Socket Mode token (xapp-…) |
| `HIVE_HOME` | company directory (otherwise: closest `hive.toml`) |
| `HIVE_SOCKET` | daemon socket path |
| `HIVE_UID` | uid of the container user (Linux) |

## Development

```sh
cargo build                    # target/debug/hive
cargo nextest run              # or cargo test
cargo clippy --all-targets
cargo fmt --all
git config core.hooksPath .githooks   # pre-commit secret scan (gitleaks)
```

**Tests.** Unit tests live next to the code. `tests/e2e.rs` runs the real binary (daemon, CLI, MCP
bridge) against `tests/fixtures/fake_claude.py`, a scripted stand-in for `claude -p`: each agent follows a
plan of tool calls (`delegate_task`, `ask_agent`, `propose_agent`, `approve`…), and the tests check the
resulting tasks, prompts, sessions (`--resume`, `--fork-session`) and tool flags. They need `python3` and
take about two seconds. No Claude subscription is used.

**CI** (`.github/workflows/ci.yml`), on every push and pull request:

| Job | Checks |
|---|---|
| Secrets | gitleaks (history and working tree) and trufflehog; no `.env`, `company/` or database committed |
| Format and clippy | `cargo fmt --check`, `cargo clippy --all-targets` with warnings as errors |
| Tests | unit and end-to-end tests on Linux x86-64, Linux arm64 and macOS, JUnit report |
| Coverage | llvm-cov over the unit and end-to-end tests, minimum 55% of the lines, HTML report |
| Docker | image built for amd64 and arm64, then `init`, daemon and a task run inside the container |
| Audit | `cargo audit` on the dependencies (also weekly) |

`docs/hive-features.pdf` is the feature guide; regenerate it with `python3 docs/make_pdf.py docs/hive-features.pdf`
(needs `reportlab`; Python 3.10+, or `reportlab<4.3` on Python 3.9).

Source layout:

| Module | Role |
|---|---|
| `main.rs` | CLI |
| `config.rs` | `hive.toml` and `agents/*.toml` |
| `db.rs` | SQLite storage: tasks, inbox, sessions, runs, users, tool rules, proposals |
| `daemon.rs` | scheduler, socket server, task lifecycle, agent tools |
| `runner.rs` | one `claude -p` run per (agent, task) |
| `mcp.rs` | stdio MCP server bridging the agents' tool calls to the daemon |
| `ipc.rs` | JSON protocol over the Unix socket |
| `notify.rs` | notifications and the Slack bot |
| `access.rs` | Slack users and rights |
| `tools.rs` / `approval.rs` | agent tool rules, profiles and approvals |
| `collab.rs` | `ask_agent` with loop control, hire proposals |
| `agents.rs` | agents created / removed / reloaded live |
| `usage.rs` | provider usage API and quota pauses |
| `dash.rs` | ratatui dashboard |
| `logs.rs` | readable run logs |
