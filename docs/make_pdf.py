"""Builds the hive feature guide: python3 docs/make_pdf.py docs/hive-features.pdf (needs reportlab, macOS fonts)."""
import sys
from reportlab.lib import colors
from reportlab.lib.enums import TA_LEFT
from reportlab.lib.pagesizes import A4
from reportlab.lib.styles import ParagraphStyle
from reportlab.lib.units import mm
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.ttfonts import TTFont
from reportlab.lib.fonts import addMapping
from reportlab.platypus import (
    BaseDocTemplate, Frame, PageTemplate, Paragraph, Spacer, Table, TableStyle,
    PageBreak, KeepTogether, Preformatted, NextPageTemplate,
)

OUT = sys.argv[1]
F = "/System/Library/Fonts/Supplemental/"
pdfmetrics.registerFont(TTFont("Body", F + "Arial Unicode.ttf"))
pdfmetrics.registerFont(TTFont("BodyB", F + "Arial Bold.ttf"))
pdfmetrics.registerFont(TTFont("Mono", F + "Courier New.ttf"))
pdfmetrics.registerFont(TTFont("MonoB", F + "Courier New Bold.ttf"))
addMapping("Body", 0, 0, "Body"); addMapping("Body", 1, 0, "BodyB")
addMapping("Body", 0, 1, "Body"); addMapping("Body", 1, 1, "BodyB")
addMapping("Mono", 0, 0, "Mono"); addMapping("Mono", 1, 0, "MonoB")

INK = colors.HexColor("#1f2328")
MUTED = colors.HexColor("#59636e")
ACCENT = colors.HexColor("#b7791f")      # honey
ACCENT_BG = colors.HexColor("#fdf6e3")
LINE = colors.HexColor("#d8dee4")
CODE_BG = colors.HexColor("#f6f8fa")

S = {
    "title": ParagraphStyle("title", fontName="BodyB", fontSize=34, leading=40, textColor=INK),
    "subtitle": ParagraphStyle("subtitle", fontName="Body", fontSize=14, leading=20, textColor=MUTED),
    "h1": ParagraphStyle("h1", fontName="BodyB", fontSize=18, leading=23, textColor=INK, spaceBefore=16, spaceAfter=8),
    "h2": ParagraphStyle("h2", fontName="BodyB", fontSize=12.5, leading=16, textColor=ACCENT, spaceBefore=10, spaceAfter=4),
    "body": ParagraphStyle("body", fontName="Body", fontSize=9.6, leading=14, textColor=INK, spaceAfter=5, alignment=TA_LEFT),
    "bullet": ParagraphStyle("bullet", fontName="Body", fontSize=9.6, leading=14, textColor=INK, leftIndent=12, bulletIndent=2, spaceAfter=2.5),
    "cell": ParagraphStyle("cell", fontName="Body", fontSize=8.6, leading=11.5, textColor=INK),
    "cellh": ParagraphStyle("cellh", fontName="BodyB", fontSize=8.6, leading=11.5, textColor=INK),
    "cellcode": ParagraphStyle("cellcode", fontName="Mono", fontSize=8.2, leading=11, textColor=INK),
    "code": ParagraphStyle("code", fontName="Mono", fontSize=8.2, leading=10.8, textColor=INK),
    "note": ParagraphStyle("note", fontName="Body", fontSize=9, leading=13, textColor=INK),
    "toc": ParagraphStyle("toc", fontName="Body", fontSize=11, leading=19, textColor=INK),
}


def esc(t):
    return t.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def c(t):
    """inline code"""
    return f'<font face="Mono" size="8.6" color="#8a4b08">{esc(t)}</font>'


def P(t, st="body"):
    return Paragraph(t, S[st])


def B(items):
    return [Paragraph(i, S["bullet"], bulletText="•") for i in items]


def code(text):
    t = Table([[Preformatted(text.strip("\n"), S["code"])]], colWidths=[170 * mm])
    t.setStyle(TableStyle([
        ("BACKGROUND", (0, 0), (-1, -1), CODE_BG),
        ("BOX", (0, 0), (-1, -1), 0.5, LINE),
        ("LEFTPADDING", (0, 0), (-1, -1), 8), ("RIGHTPADDING", (0, 0), (-1, -1), 8),
        ("TOPPADDING", (0, 0), (-1, -1), 6), ("BOTTOMPADDING", (0, 0), (-1, -1), 6),
    ]))
    return [t, Spacer(1, 6)]


def table(rows, widths, code_col=0):
    data = []
    for i, r in enumerate(rows):
        row = []
        for j, cell in enumerate(r):
            if i == 0:
                row.append(Paragraph(cell, S["cellh"]))
            elif j == code_col and code_col is not None:
                row.append(Paragraph(esc(cell), S["cellcode"]))
            else:
                row.append(Paragraph(cell, S["cell"]))
        data.append(row)
    t = Table(data, colWidths=[w * mm for w in widths], repeatRows=1)
    t.setStyle(TableStyle([
        ("BACKGROUND", (0, 0), (-1, 0), ACCENT_BG),
        ("LINEBELOW", (0, 0), (-1, 0), 0.8, ACCENT),
        ("LINEBELOW", (0, 1), (-1, -1), 0.4, LINE),
        ("VALIGN", (0, 0), (-1, -1), "TOP"),
        ("LEFTPADDING", (0, 0), (-1, -1), 5), ("RIGHTPADDING", (0, 0), (-1, -1), 5),
        ("TOPPADDING", (0, 0), (-1, -1), 4), ("BOTTOMPADDING", (0, 0), (-1, -1), 4),
    ]))
    return [t, Spacer(1, 8)]


def note(text):
    t = Table([[Paragraph(text, S["note"])]], colWidths=[170 * mm])
    t.setStyle(TableStyle([
        ("BACKGROUND", (0, 0), (-1, -1), ACCENT_BG),
        ("LINEBEFORE", (0, 0), (0, -1), 3, ACCENT),
        ("LEFTPADDING", (0, 0), (-1, -1), 9), ("RIGHTPADDING", (0, 0), (-1, -1), 9),
        ("TOPPADDING", (0, 0), (-1, -1), 6), ("BOTTOMPADDING", (0, 0), (-1, -1), 6),
    ]))
    return [t, Spacer(1, 8)]


SECTIONS = []


def h1(n, title):
    SECTIONS.append((n, title))
    return P(f'<font color="#b7791f">{n}</font>  {title}', "h1")


def on_page(canv, doc):
    canv.saveState()
    canv.setStrokeColor(LINE)
    canv.setLineWidth(0.5)
    canv.line(20 * mm, 15 * mm, 190 * mm, 15 * mm)
    canv.setFont("Body", 8)
    canv.setFillColor(MUTED)
    canv.drawString(20 * mm, 10.5 * mm, "hive · a team of Claude Code agents")
    canv.drawRightString(190 * mm, 10.5 * mm, str(doc.page))
    canv.restoreState()


def on_cover(canv, doc):
    canv.saveState()
    canv.setFillColor(ACCENT)
    canv.rect(0, A4[1] - 12 * mm, A4[0], 12 * mm, stroke=0, fill=1)
    # honeycomb
    import math
    canv.setStrokeColor(colors.HexColor("#ecd9a8"))
    canv.setLineWidth(1.2)
    r = 13 * mm
    for row in range(4):
        for col in range(4):
            cx = 120 * mm + col * r * 1.5
            cy = 22 * mm + row * r * math.sqrt(3) + (col % 2) * r * math.sqrt(3) / 2
            pts = [(cx + r * math.cos(math.radians(60 * k)), cy + r * math.sin(math.radians(60 * k))) for k in range(6)]
            p = canv.beginPath()
            p.moveTo(*pts[0])
            for q in pts[1:]:
                p.lineTo(*q)
            p.close()
            canv.drawPath(p, stroke=1, fill=0)
    canv.restoreState()


story = []

# ---------------------------------------------------------------- cover
story += [
    Spacer(1, 45 * mm),
    P("hive", "title"),
    Spacer(1, 4 * mm),
    P("A team of Claude Code agents that split the work<br/>according to their capabilities, and report back to you.", "subtitle"),
    Spacer(1, 12 * mm),
    P("Feature guide", "h2"),
    P("CEO, CTO, developers, marketing… every agent is a Claude Code session (Claude subscription) with "
      "its role, capabilities and tools. You give a goal; the agents delegate, ask each other questions, "
      "deliver, and let you know when it's done."),
    Spacer(1, 6 * mm),
    P('<font color="#59636e">Version 0.1 · November 2026 · Rust binary, CLI, terminal dashboard, Slack, Docker</font>'),
    NextPageTemplate("content"),
    PageBreak(),
]

body = []
# ---------------------------------------------------------------- 1
body.append(h1("1", "Overview"))
body.append(P("hive is an orchestrator: a daemon that keeps the tasks in a SQLite database, starts one "
              "<b>claude -p</b> process per agent and task, and gives the agents MCP tools to work together. "
              "You drive it from the command line, a terminal dashboard or Slack."))
body += code("""
you -- CLI / dashboard / Slack -->  hive daemon  -- spawns -->  claude -p  (ceo, task #1)
                                    | SQLite                        |
                                    | scheduler, quotas             v
                                    +---- Unix socket <---------  hive mcp
                                                         (delegate_task, ask_agent,
                                                          complete_task, ask_owner...)
""")
body.append(P("Highlights", "h2"))
body += B([
    "<b>Configurable team</b>: roles, capabilities, model (Haiku, Sonnet, Opus), tools, hierarchy.",
    "<b>Capability-based delegation</b>: every agent sees the team and hands work to the right teammate.",
    "<b>Real availability</b>: an agent is busy while a process runs for it; it can also declare itself busy.",
    "<b>Completion notifications</b>: terminal, macOS notification, Slack thread, mentioning the requester.",
    "<b>Working memory</b>: one Claude session per agent and task, resumed at every wake-up (<b>--resume</b>).",
    "<b>Everything changes live</b>: agents, tools, rights, without restarting the daemon.",
    "<b>Guardrails</b>: delegation depth, anti-loop rules, human arbitration, tool approvals, quotas.",
])

# ---------------------------------------------------------------- 2
body.append(h1("2", "Installation and start"))
body.append(P("Locally", "h2"))
body += code("""
cargo install --path .                      # from the hive repository
hive init ~/my-company && cd ~/my-company   # hive.toml + agents/*.toml + workspace/
hive daemon                                 # terminal 1: the orchestrator
hive dash                                   # terminal 2: the dashboard
""")
body.append(P("With Docker (recommended: agents are isolated)", "h2"))
body += code("""
cp .env.example .env        # CLAUDE_CODE_OAUTH_TOKEN, Slack tokens
claude setup-token          # on your machine: token tied to the Claude subscription
mkdir company && docker compose run --rm hive init /company
docker compose up -d && docker compose exec hive hive dash
""")
body += B([
    f"{c('./company')} is mounted in the container: edit the configuration from the host and find the deliverables in {c('company/workspace/')}.",
    f"The {c('claude-home')} volume keeps the Claude sessions, needed for agents to resume their conversations.",
    "The container runs as a non-root user; Claude Code is preinstalled.",
    f"After a code change: {c('docker compose up -d --build')}.",
])

# ---------------------------------------------------------------- 3
body.append(h1("3", "The agent team"))
body.append(P(f"One agent = one {c('agents/<name>.toml')} file. {c('hive init')} creates a starting team: "
              "CEO, CTO, developer, marketing."))
body += code('''
name = "frontend-dev"
title = "Frontend developer"
reports_to = "cto"
capabilities = ["react", "css", "accessibility"]
model = "sonnet"                 # haiku | sonnet | opus | full id
provider = "claude"              # provider whose quota is tracked
max_concurrent = 1               # parallel tasks for this agent
workdir = "workspace"            # working directory (relative or absolute, e.g. a git repo)
permission_mode = "default"
profiles = ["dev", "web"]        # initial tools (see section 9)
prompt = """Your role, your rules, what your result must contain..."""
''')
body.append(P("Create, remove, reload live", "h2"))
body += table([
    ["Command", "Effect"],
    ["hive agent add designer --title \"UI Designer\" --reports-to cto --capabilities ui,mockups --profiles edit,web --model sonnet --prompt \"…\"",
     f"Writes {c('agents/designer.toml')} and adds the agent to the team immediately. {c('--prompt-file role.md')} for a long prompt."],
    ["hive agent show designer", "Sheet: role, model, capabilities, profiles, effective tools, prompt."],
    ["hive agent rm designer", f"Renames the file to {c('.toml.removed')}. Refused with open tasks, subordinates, or for the default agent."],
    ["hive agent reload", "Re-reads agents/*.toml after a hand edit."],
], [95, 75])
body.append(P("The other agents discover a newcomer at their next run: it appears in their description of "
              "the team and in list_agents."))

# ---------------------------------------------------------------- 4
body.append(h1("4", "Tasks: lifecycle"))
body += table([
    ["Status", "Meaning"],
    ["todo", "Assigned, not started yet."],
    ["in_progress", "An agent works on it (or is about to be resumed)."],
    ["waiting", "The agent waits for its sub-tasks or questions to finish."],
    ["blocked", "Waiting for a human: question, arbitration, hire proposal."],
    ["done · failed · cancelled", "Closed. Can be reopened (see below)."],
], [45, 125])
body.append(P("How an agent works", "h2"))
body += B([
    "It receives its task in a dedicated Claude session, with its role, the company mission and the team.",
    f"It delegates with {c('delegate_task')}, then ends its turn. It is <b>woken up once</b>, when all its sub-tasks are closed, with their results: no polling, no wasted quota.",
    f"It hands its work in with {c('complete_task')} (a self-contained result: deliverables, file paths, decisions). If it ends without calling it, its last message becomes the result.",
    f"It can fail cleanly ({c('fail_task')}), ask you a question ({c('ask_owner')}), look at the team ({c('list_agents')}, {c('task_info')}) or declare its availability ({c('set_status')}).",
])
body.append(P("Reopening a finished task", "h2"))
body.append(P("Threads stay alive. A message on a finished, failed or cancelled main task <b>reopens</b> it: its "
              "agent resumes <b>the same conversation</b>, with your new request and its previous result, and may "
              "delegate again. In Slack, just reply in the thread; from the CLI: " + c('hive reply <id> "…"') + ". "
              "Acknowledgements (\"thanks\", \"ok\", \"great\"…) don't reopen anything."))
body.append(P(f"Other actions: {c('hive cancel <id>')} cancels the task and all its sub-tasks (processes stopped, parent told); "
              f"{c('hive retry <id>')} retries a failed or cancelled task."))

# ---------------------------------------------------------------- 5
body.append(h1("5", "Collaboration between agents"))
body += table([
    ["Tool", "Use", "Control"],
    ["delegate_task", "Hand a sub-task to any teammate, according to their capabilities.",
     "Max depth (max_delegation_depth). Not to oneself, nor to an agent already upstream in the chain."],
    ["ask_agent", "Ask a teammate a question, including the one who gave the task.",
     "The answering agent can neither delegate nor ask another question: no chain possible."],
    ["propose_agent", "Propose hiring a new agent when a capability is missing.",
     "Recruiters only (ceo, cto by default). An admin decides."],
], [30, 70, 70])
body.append(P("Questions with positive feedback", "h2"))
body += B([
    "The answering agent answers <b>with the context of its own task</b>: its session is forked, the original is left untouched. A CTO therefore knows why it asked its developer for something.",
    f"The answer flows back into the asker's task and stays visible ({c('hive show')}, marked \"?\" in the tree).",
    "With every new question between two agents, their previous exchanges are recalled to the answerer: it builds on them instead of repeating itself.",
])
body.append(P("Anti-loop human arbitration", "h2"))
body.append(P("The question is not forwarded and a human decides when:"))
body += B([
    f"the same two agents already exchanged {c('max_questions_per_pair')} times on a task (3 by default);",
    f"the task and its sub-tasks received {c('max_questions_per_tree')} questions (12 by default);",
    "the question is almost identical to one already asked.",
])
body.append(P("The task becomes \"blocked\", the arbitration lands in the Slack thread with the history of the "
              "exchanges, and your answer goes to the agent, which resumes."))
body.append(P("Hires proposed by the agents", "h2"))
body.append(P("The CEO or the CTO proposes a position: name, title, manager, capabilities, tool profiles, model, "
              "prompt and justification. An admin accepts (the agent is created on the spot) or rejects, with "
              f"Slack buttons, {c('proposal accept|reject <id> [reason]')} in the channel, or "
              f"{c('hive proposal accept <id>')}. The proposer waits for the decision, then resumes knowing it."))

# ---------------------------------------------------------------- 6
body.append(h1("6", "Command line"))
body += table([
    ["Command", "Purpose"],
    ["hive init [dir]", "Create a company (hive.toml, agents/, workspace/)."],
    ["hive daemon", "Run the orchestrator."],
    ["hive dash", "Live dashboard (section 7)."],
    ["hive task [--to agent] \"…\"", "New task (for default_agent by default)."],
    ["hive status", "Agents: available, busy (#tasks), queued, paused (quota), waiting for approval. Plus usage."],
    ["hive tasks [-a]", "Task tree (with -a: closed tasks included)."],
    ["hive show <id>", "Description, result, sub-tasks, questions, runs and cost."],
    ["hive reply <id> \"…\"", "Answer an agent; reopens a closed main task."],
    ["hive cancel <id> · hive retry <id>", "Cancel a sub-tree · retry a task."],
    ["hive logs <agent|run> [-f] [--raw]", "What the agent does: text, tool calls, result."],
    ["hive usage [--push]", "Provider quotas · push a report (stdin)."],
    ["hive tools [agent …]", "Agent tools: view, allow, deny, profiles."],
    ["hive approvals · hive approve <id> [once|always|deny]", "Pending tool approvals."],
    ["hive proposals [-a] · hive proposal accept|reject <id> [reason]", "Proposed hires."],
    ["hive agents · hive agent add|show|rm|reload", "The team, live creation and removal."],
    ["hive user · hive user set <U…> <role> [--agents a,b] [--name N] · hive user rm <U…>", "Slack users and their rights."],
], [80, 90])
body.append(P(f"The company directory is found by walking up to the closest {c('hive.toml')}, or through {c('HIVE_HOME')}. "
              f"With Docker: {c('docker compose exec hive hive <command>')}."))

# ---------------------------------------------------------------- 7
body.append(h1("7", "Terminal dashboard"))
body.append(P(f"{c('hive dash')} shows live, refreshed every second:"))
body += B([
    "<b>Agents</b>: role, colored state, declared note.",
    "<b>Usage</b>: a gauge per quota window and provider, reset date, pause if any.",
    "<b>Tasks</b>: navigable tree (questions marked \"?\"), closed tasks shown or hidden.",
    "<b>Detail</b> of the selected task: description, result, sub-tasks, runs, cost.",
    "<b>Activity</b>: the daemon's event feed.",
    "<b>Alert bar</b>: pending tool approvals and hires.",
])
body += table([
    ["Key", "Action"],
    ["↑ ↓  /  j k", "Select a task"],
    ["n", "New task (\"@cto text\" to target an agent)"],
    ["r", "Write to the selected task's agent (reopens it if closed)"],
    ["c  ·  R", "Cancel (with confirmation)  ·  Retry"],
    ["y  ·  Y  ·  x", "Allow once · always · deny the oldest tool request"],
    ["tab", "Show / hide closed tasks"],
    ["J K  /  PgUp PgDn", "Scroll the detail"],
    ["q", "Quit (agents keep working)"],
], [45, 125])

# ---------------------------------------------------------------- 8
body.append(h1("8", "Slack and rights"))
body.append(P("The bot uses Socket Mode: no public URL, the daemon opens an outbound websocket. Every main "
              "task has its <b>thread</b>: delegations, questions between agents, finished sub-tasks, approvals, "
              "hires. Results, failures and questions are also broadcast to the channel, mentioning the person "
              "who requested the task."))
body += table([
    ["Where", "Message", "Effect"],
    ["Channel", "plain text", "New task for the default agent; your message becomes the thread."],
    ["Channel", "@cto text", "New task for a given agent."],
    ["Channel", "status · tasks [all] · show <id> · usage · me · help", "Read."],
    ["Channel", "cancel <id> · retry <id> · reply <id> <text>", "Act on a task."],
    ["Channel", "agents · agent show|add|rm · agents reload", "Team (create / remove: admins). For agent add, the prompt goes on the following lines."],
    ["Channel", "tools … · approvals · approve <id> …", "Tools and approvals (changes: admins)."],
    ["Channel", "proposals · proposal accept|reject <id>", "Hires (decision: admins)."],
    ["Channel", "users · user add @x member [agents=a,b] [name=X] · user rm @x", "Rights management (admins)."],
    ["Thread", "text", "Sent to the agent (first to the one waiting for an answer); reopens the task if closed."],
    ["Thread", "cancel · retry · show", "Act on the thread's task."],
], [17, 68, 85], code_col=1)
body.append(P("Roles", "h2"))
body += table([
    ["Role", "Can"],
    ["viewer", "Read: status, tasks, show, usage, tools, approvals, agents."],
    ["member", "+ create tasks (optionally only for some agents) and act on their own tasks."],
    ["admin", "Everything: users, agents, tools, approvals, hires, all tasks."],
], [25, 145])
body += B([
    f"{c('[slack] allowed_users')} creates the first admins; then everything is managed live.",
    "The last admin cannot be removed nor demoted.",
    "Messages from people without access are ignored (and noted in the activity feed).",
    "The CLI and the dashboard, which use the local socket, have all rights.",
])

# ---------------------------------------------------------------- 9
body.append(h1("9", "Agent tools"))
body.append(P("Rules are stored in the database and read at every run: a change applies at the agent's next "
              "run, without restart. They use Claude Code's syntax."))
body += table([
    ["Rule", "Meaning"],
    ["Read", "The whole tool."],
    ["Bash(npm test)", "One precise command."],
    ["Bash(git *)", "A command pattern."],
    ["Edit(src/**)", "Editing restricted to a path."],
    ["WebFetch(domain:github.com)", "Web access restricted to a domain."],
    ["mcp__server", "The tools of an MCP server."],
], [60, 110])
body.append(P("Profiles", "h2"))
body.append(P("Named rule sets attached to agents. An agent's own rules come on top; a deny always wins."))
body += table([
    ["Profile", "Allows", "Denies"],
    ["read", "Read, Glob, Grep", "—"],
    ["web", "WebSearch, WebFetch", "—"],
    ["edit", "read + Edit, Write", "—"],
    ["dev", "edit + Bash", "Bash(git push *), Bash(rm -rf *)"],
], [25, 80, 65], code_col=None)
body += code('''
hive tools                                   # overview
hive tools dev allow "Bash(npm test)"        # / deny / remove <rule>
hive tools dev profile add web               # / profile rm web
hive tools profile deploy allow "Bash(./deploy.sh)"
hive tools profile deploy delete
''')
body.append(P("On-demand approvals", "h2"))
body.append(P("When an agent wants a tool that is neither allowed nor denied, it doesn't get a flat refusal: a "
              "request goes to the admins (\"dev wants to use Bash: npm install\"). Slack buttons <b>Allow once</b> / "
              f"<b>Always</b> / <b>Deny</b>, {c('hive approve <id> once|always|deny')} or the y / Y / x keys of the "
              "dashboard. \"Always\" adds the rule to the agent. Without an answer within 30 minutes "
              f"({c('approval_timeout_secs')}), it is a deny. While waiting, the agent shows \"waiting for approval\"."))

# ---------------------------------------------------------------- 10
body.append(h1("10", "Quotas and provider usage"))
body.append(P("hive tracks each model provider's usage over its windows (5 hours, 7 days, month…). Claude is fed "
              f"automatically by the {c('rate_limit_event')} events of every run. Other providers send a "
              "normalized report:"))
body += code('''
{"provider": "openai",
 "windows": [{"name": "monthly", "used": 12.5, "limit": 50, "unit": "usd",
              "utilization": 0.25, "resets_at": 1795000000, "status": "allowed"}]}
''')
body += B([
    "<b>Push</b>: " + c("echo '{…}' | hive usage --push") + " (or the report_usage operation on the socket).",
    f"<b>Pull</b>: a command declared in {c('[[usage.providers]]')}, run periodically, printing this JSON.",
    f"<b>Automatic pause</b>: above {c('pause_above')} (95% by default) on a window, or when the provider answers \"rejected\", no new run is started for its agents until the reset.",
    "<b>No failure because of quota</b>: a run stopped by a limit puts the task back in the queue; it resumes by itself after the reset.",
    f"Concurrency limits: {c('max_parallel_runs')} (global) and {c('max_concurrent')} (per agent).",
])

# ---------------------------------------------------------------- 11
body.append(h1("11", "Robustness and security"))
body += B([
    "<b>Resume after restart</b>: interrupted runs (stop, crash, update) are resumed automatically, in the same session.",
    "<b>Clean stop</b>: Ctrl-C or SIGTERM (docker stop) stops the claude processes and queues their tasks for resume.",
    "<b>Isolation</b>: with Docker, the agents (and their Bash) only reach the container and the company directory.",
    "<b>Least privilege</b>: per-agent tools, denies first, human approval for the rest.",
    "<b>Human control</b>: questions, arbitrations, approvals and hires go to the authorized humans.",
    "<b>Traceability</b>: every run is logged (hive logs), costs and durations per task (hive show), activity feed.",
    "<b>Secrets</b>: tokens in .env or environment variables, never in the database.",
])

# ---------------------------------------------------------------- 12
body.append(h1("12", "Configuration reference"))
body += code('''
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
# [[usage.providers]]
# name = "openai"
# command = "./scripts/openai-usage.sh"
# every_secs = 300

[tools]
approvals = true
approval_timeout_secs = 1800

[collaboration]
max_questions_per_pair = 3
max_questions_per_tree = 12
recruiters = ["ceo", "cto"]

[slack]
channel = "C0123456789"
allowed_users = ["U0123456789"]     # first admins
''')
body += table([
    ["Environment variable", "Purpose"],
    ["CLAUDE_CODE_OAUTH_TOKEN", "Claude subscription (claude setup-token), mostly for Docker."],
    ["HIVE_SLACK_BOT_TOKEN", "Slack bot token (xoxb-…)."],
    ["HIVE_SLACK_APP_TOKEN", "Socket Mode token (xapp-…, scope connections:write)."],
    ["HIVE_HOME", "Company directory (otherwise: closest hive.toml)."],
    ["HIVE_SOCKET", "Path of the daemon socket."],
    ["HIVE_UID", "Uid of the container user (Linux)."],
], [60, 110])
body.append(P("Slack app", "h2"))
body.append(P("Created from a manifest (provided in the README): scopes chat:write, channels:history, groups:history; "
              "events message.channels and message.groups; Socket Mode and Interactivity enabled (for the buttons)."))

# ---------------------------------------------------------------- 13
body.append(h1("13", "Current limits and next steps"))
body += B([
    "A single execution provider (Claude Code); other providers are only tracked for quotas.",
    "No spontaneous information messages between agents working in parallel (send_message), no multi-agent meetings.",
    "No isolated git working directory per agent (worktree): agents sharing a workdir share the files.",
    "No quota budget per agent or per task.",
    "Slack: one channel; no slash commands; buttons require the Interactivity option.",
])

story += body

doc = BaseDocTemplate(
    OUT, pagesize=A4, leftMargin=20 * mm, rightMargin=20 * mm, topMargin=18 * mm, bottomMargin=22 * mm,
    title="hive — feature guide", author="hive", subject="A team of Claude Code agents",
)
frame = Frame(doc.leftMargin, doc.bottomMargin, doc.width, doc.height, id="f")
doc.addPageTemplates([
    PageTemplate(id="cover", frames=[frame], onPage=on_cover),
    PageTemplate(id="content", frames=[frame], onPage=on_page),
])
doc.build(story)
print(OUT)
