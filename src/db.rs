use crate::access::{Role, User};
use crate::usage::Window;
use crate::util::now;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS tasks (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    parent_id   INTEGER,
    title       TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    assignee    TEXT NOT NULL,
    created_by  TEXT NOT NULL,
    status      TEXT NOT NULL,            -- todo|in_progress|waiting|blocked|done|failed|cancelled
    result      TEXT,
    depth       INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
-- Messages waiting to be delivered to an agent, in the context of one task.
-- run_id: NULL = pending, -1 = dropped, otherwise the run that consumed it.
CREATE TABLE IF NOT EXISTS inbox (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    agent      TEXT NOT NULL,
    task_id    INTEGER NOT NULL,
    kind       TEXT NOT NULL,             -- assigned|child_done|child_failed|owner|retry|resume
    body       TEXT NOT NULL,
    created_at TEXT NOT NULL,
    run_id     INTEGER
);
CREATE INDEX IF NOT EXISTS inbox_pending ON inbox(run_id, agent, task_id);
-- One claude session per (agent, task): resumed on every wake-up.
CREATE TABLE IF NOT EXISTS sessions (
    agent      TEXT NOT NULL,
    task_id    INTEGER NOT NULL,
    session_id TEXT NOT NULL,
    PRIMARY KEY (agent, task_id)
);
CREATE TABLE IF NOT EXISTS runs (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    agent      TEXT NOT NULL,
    task_id    INTEGER NOT NULL,
    status     TEXT NOT NULL,             -- running|ok|error|killed|aborted
    started_at TEXT NOT NULL,
    ended_at   TEXT,
    cost_usd   REAL
);
-- Slack thread of each root task (all its sub-tasks talk in it).
CREATE TABLE IF NOT EXISTS slack_threads (
    task_id INTEGER PRIMARY KEY,
    ts      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS slack_threads_ts ON slack_threads(ts);
CREATE TABLE IF NOT EXISTS usage (
    provider    TEXT NOT NULL,
    win         TEXT NOT NULL,
    used        REAL,
    lim         REAL,
    unit        TEXT,
    utilization REAL,
    resets_at   INTEGER,
    status      TEXT,
    source      TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (provider, win)
);
CREATE TABLE IF NOT EXISTS users (
    slack_id   TEXT PRIMARY KEY,
    name       TEXT,
    role       TEXT NOT NULL,          -- viewer|member|admin
    agents     TEXT NOT NULL DEFAULT '', -- comma separated; empty = all
    created_at TEXT NOT NULL
);
-- Tool access. scope: agent|profile; kind: allow|deny|profile (an agent's attached profile)
CREATE TABLE IF NOT EXISTS tool_rules (
    scope TEXT NOT NULL,
    name  TEXT NOT NULL,
    kind  TEXT NOT NULL,
    rule  TEXT NOT NULL,
    PRIMARY KEY (scope, name, kind, rule)
);
-- Hires proposed by agents, decided by an admin
CREATE TABLE IF NOT EXISTS proposals (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    proposed_by TEXT NOT NULL,
    task_id     INTEGER NOT NULL,
    spec        TEXT NOT NULL,   -- JSON AgentSpec
    reason      TEXT NOT NULL,
    status      TEXT NOT NULL,   -- pending|accepted|rejected
    decided_by  TEXT,
    note        TEXT,
    slack_ts    TEXT,
    created_at  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS agent_notes (
    agent        TEXT PRIMARY KEY,
    availability TEXT NOT NULL,
    note         TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);
";

#[derive(Debug, Clone, serde::Serialize)]
pub struct Task {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub title: String,
    pub description: String,
    pub assignee: String,
    pub created_by: String,
    pub status: String,
    pub result: Option<String>,
    pub depth: i64,
    /// Who asked for the root task: a Slack user id, or None (CLI / dashboard)
    pub requested_by: Option<String>,
    /// task | question (ask_agent)
    pub kind: String,
    pub created_at: String,
    pub updated_at: String,
}

impl Task {
    pub fn is_closed(&self) -> bool {
        matches!(self.status.as_str(), "done" | "failed" | "cancelled")
    }
}

#[derive(Debug, Clone)]
pub struct InboxItem {
    pub kind: String,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct Run {
    pub id: i64,
    pub agent: String,
    pub status: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub cost_usd: Option<f64>,
}

const TASK_COLS: &str =
    "id, parent_id, title, description, assignee, created_by, status, result, depth, created_at, updated_at, requested_by, kind";

fn row_task(r: &Row) -> rusqlite::Result<Task> {
    Ok(Task {
        id: r.get(0)?,
        parent_id: r.get(1)?,
        title: r.get(2)?,
        description: r.get(3)?,
        assignee: r.get(4)?,
        created_by: r.get(5)?,
        status: r.get(6)?,
        result: r.get(7)?,
        depth: r.get(8)?,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
        requested_by: r.get(11)?,
        kind: r.get(12)?,
    })
}

fn pending_in(c: &Connection, agent: &str, task_id: i64) -> rusqlite::Result<Vec<InboxItem>> {
    let mut st = c.prepare(
        "SELECT kind, body FROM inbox WHERE agent = ?1 AND task_id = ?2 AND run_id IS NULL ORDER BY id",
    )?;
    let rows = st
        .query_map(params![agent, task_id], |r| Ok(InboxItem { kind: r.get(0)?, body: r.get(1)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub struct Db(Mutex<Connection>);

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let c = Connection::open(path)?;
        c.busy_timeout(std::time::Duration::from_secs(5))?;
        c.execute_batch(SCHEMA)?;
        // Migrations (column additions); "duplicate column" means already applied.
        let _ = c.execute("ALTER TABLE tasks ADD COLUMN requested_by TEXT", []);
        let _ = c.execute("ALTER TABLE tasks ADD COLUMN kind TEXT NOT NULL DEFAULT 'task'", []);
        Ok(Db(Mutex::new(c)))
    }

    fn c(&self) -> MutexGuard<'_, Connection> {
        self.0.lock().unwrap()
    }

    // ---- tasks ----

    #[allow(clippy::too_many_arguments)]
    pub fn create_task(
        &self,
        parent_id: Option<i64>,
        title: &str,
        description: &str,
        assignee: &str,
        created_by: &str,
        depth: i64,
        requested_by: Option<&str>,
    ) -> Result<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO tasks (parent_id, title, description, assignee, created_by, status, depth, created_at, updated_at, requested_by)
             VALUES (?1, ?2, ?3, ?4, ?5, 'todo', ?6, ?7, ?7, ?8)",
            params![parent_id, title, description, assignee, created_by, depth, now(), requested_by],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn task(&self, id: i64) -> Result<Option<Task>> {
        let c = self.c();
        let t = c
            .query_row(&format!("SELECT {TASK_COLS} FROM tasks WHERE id = ?1"), [id], row_task)
            .optional()?;
        Ok(t)
    }

    fn tasks_where(&self, clause: &str, p: impl rusqlite::Params) -> Result<Vec<Task>> {
        let c = self.c();
        let mut st = c.prepare(&format!("SELECT {TASK_COLS} FROM tasks {clause} ORDER BY id"))?;
        let rows = st.query_map(p, row_task)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Open tasks + the `recent` most recently closed ones.
    pub fn tasks_for_dashboard(&self, recent: i64) -> Result<Vec<Task>> {
        self.tasks_where(
            "WHERE status NOT IN ('done', 'failed', 'cancelled')
                OR id IN (SELECT id FROM tasks WHERE status IN ('done', 'failed', 'cancelled') ORDER BY updated_at DESC LIMIT ?1)",
            [recent],
        )
    }

    pub fn tasks(&self, all: bool) -> Result<Vec<Task>> {
        if all {
            self.tasks_where("", [])
        } else {
            self.tasks_where("WHERE status NOT IN ('done', 'failed', 'cancelled')", [])
        }
    }

    pub fn children(&self, id: i64) -> Result<Vec<Task>> {
        self.tasks_where("WHERE parent_id = ?1", [id])
    }

    pub fn open_children(&self, id: i64) -> Result<i64> {
        let c = self.c();
        let n = c.query_row(
            "SELECT COUNT(*) FROM tasks WHERE parent_id = ?1 AND status NOT IN ('done', 'failed', 'cancelled')",
            [id],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    pub fn set_status(&self, id: i64, status: &str, result: Option<&str>) -> Result<()> {
        let c = self.c();
        match result {
            Some(r) => c.execute(
                "UPDATE tasks SET status = ?1, result = ?2, updated_at = ?3 WHERE id = ?4",
                params![status, r, now(), id],
            )?,
            None => c.execute(
                "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE id = ?3",
                params![status, now(), id],
            )?,
        };
        Ok(())
    }

    // ---- inbox ----

    pub fn add_inbox(&self, agent: &str, task_id: i64, kind: &str, body: &str) -> Result<()> {
        self.c().execute(
            "INSERT INTO inbox (agent, task_id, kind, body, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![agent, task_id, kind, body, now()],
        )?;
        Ok(())
    }

    /// (agent, task) pairs with pending messages, oldest first.
    pub fn pending_groups(&self) -> Result<Vec<(String, i64)>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT agent, task_id FROM inbox WHERE run_id IS NULL GROUP BY agent, task_id ORDER BY MIN(id)",
        )?;
        let rows = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn pending(&self, agent: &str, task_id: i64) -> Result<Vec<InboxItem>> {
        let c = self.c();
        Ok(pending_in(&c, agent, task_id)?)
    }

    /// Pending messages, marked as consumed by `run_id`.
    pub fn take_inbox(&self, agent: &str, task_id: i64, run_id: i64) -> Result<Vec<InboxItem>> {
        let c = self.c();
        let items = pending_in(&c, agent, task_id)?;
        c.execute(
            "UPDATE inbox SET run_id = ?1 WHERE agent = ?2 AND task_id = ?3 AND run_id IS NULL",
            params![run_id, agent, task_id],
        )?;
        Ok(items)
    }

    pub fn drop_inbox(&self, task_id: i64) -> Result<()> {
        self.c()
            .execute("UPDATE inbox SET run_id = -1 WHERE task_id = ?1 AND run_id IS NULL", [task_id])?;
        Ok(())
    }

    /// agent -> number of tasks with pending messages
    pub fn pending_counts(&self) -> Result<HashMap<String, i64>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT agent, COUNT(DISTINCT task_id) FROM inbox WHERE run_id IS NULL GROUP BY agent",
        )?;
        let rows = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        Ok(rows)
    }

    // ---- sessions ----

    pub fn session(&self, agent: &str, task_id: i64) -> Result<Option<String>> {
        let c = self.c();
        let s = c
            .query_row(
                "SELECT session_id FROM sessions WHERE agent = ?1 AND task_id = ?2",
                params![agent, task_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(s)
    }

    pub fn set_session(&self, agent: &str, task_id: i64, session_id: &str) -> Result<()> {
        self.c().execute(
            "INSERT OR REPLACE INTO sessions (agent, task_id, session_id) VALUES (?1, ?2, ?3)",
            params![agent, task_id, session_id],
        )?;
        Ok(())
    }

    // ---- runs ----

    pub fn create_run(&self, agent: &str, task_id: i64) -> Result<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO runs (agent, task_id, status, started_at) VALUES (?1, ?2, 'running', ?3)",
            params![agent, task_id, now()],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn finish_run(&self, id: i64, status: &str, cost: Option<f64>) -> Result<()> {
        self.c().execute(
            "UPDATE runs SET status = ?1, ended_at = ?2, cost_usd = ?3 WHERE id = ?4",
            params![status, now(), cost, id],
        )?;
        Ok(())
    }

    pub fn runs_for_task(&self, task_id: i64) -> Result<Vec<Run>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT id, agent, status, started_at, ended_at, cost_usd FROM runs WHERE task_id = ?1 ORDER BY id",
        )?;
        let rows = st
            .query_map([task_id], |r| {
                Ok(Run {
                    id: r.get(0)?,
                    agent: r.get(1)?,
                    status: r.get(2)?,
                    started_at: r.get(3)?,
                    ended_at: r.get(4)?,
                    cost_usd: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Runs left `running` by a previous daemon: mark them aborted and return them.
    pub fn recover_stale_runs(&self) -> Result<Vec<(String, i64)>> {
        let c = self.c();
        let mut st = c.prepare("SELECT agent, task_id FROM runs WHERE status = 'running'")?;
        let rows = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        c.execute(
            "UPDATE runs SET status = 'aborted', ended_at = ?1 WHERE status = 'running'",
            [now()],
        )?;
        Ok(rows)
    }

    // ---- self-declared agent status ----

    pub fn set_note(&self, agent: &str, availability: &str, note: &str) -> Result<()> {
        self.c().execute(
            "INSERT OR REPLACE INTO agent_notes (agent, availability, note, updated_at) VALUES (?1, ?2, ?3, ?4)",
            params![agent, availability, note, now()],
        )?;
        Ok(())
    }

    /// agent -> (availability, note)
    pub fn notes(&self) -> Result<HashMap<String, (String, String)>> {
        let c = self.c();
        let mut st = c.prepare("SELECT agent, availability, note FROM agent_notes")?;
        let rows = st
            .query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        Ok(rows)
    }

    // ---- slack threads ----

    pub fn thread_of(&self, task_id: i64) -> Result<Option<String>> {
        let c = self.c();
        let ts = c
            .query_row("SELECT ts FROM slack_threads WHERE task_id = ?1", [task_id], |r| r.get(0))
            .optional()?;
        Ok(ts)
    }

    pub fn set_thread(&self, task_id: i64, ts: &str) -> Result<()> {
        self.c().execute(
            "INSERT OR REPLACE INTO slack_threads (task_id, ts) VALUES (?1, ?2)",
            params![task_id, ts],
        )?;
        Ok(())
    }

    pub fn task_by_thread(&self, ts: &str) -> Result<Option<i64>> {
        let c = self.c();
        let id = c
            .query_row("SELECT task_id FROM slack_threads WHERE ts = ?1", [ts], |r| r.get(0))
            .optional()?;
        Ok(id)
    }

    /// Root of a task's tree.
    pub fn root_of(&self, mut id: i64) -> Result<i64> {
        while let Some(p) = self.task(id)?.and_then(|t| t.parent_id) {
            id = p;
        }
        Ok(id)
    }

    /// Most recently updated blocked task (waiting for the owner) in the tree under `root`.
    pub fn blocked_in_tree(&self, root: i64) -> Result<Option<i64>> {
        let c = self.c();
        let id = c
            .query_row(
                "WITH RECURSIVE tree(id) AS (SELECT ?1 UNION ALL SELECT t.id FROM tasks t JOIN tree ON t.parent_id = tree.id)
                 SELECT t.id FROM tasks t JOIN tree ON t.id = tree.id WHERE t.status = 'blocked'
                 ORDER BY t.updated_at DESC, t.id DESC LIMIT 1",
                [root],
                |r| r.get(0),
            )
            .optional()?;
        Ok(id)
    }

    // ---- usage ----

    pub fn set_usage(&self, provider: &str, w: &Window, source: &str) -> Result<()> {
        self.c().execute(
            "INSERT OR REPLACE INTO usage (provider, win, used, lim, unit, utilization, resets_at, status, source, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![provider, w.name, w.used, w.limit, w.unit, w.utilization, w.resets_at, w.status, source, now()],
        )?;
        Ok(())
    }

    pub fn usage(&self, provider: Option<&str>) -> Result<Vec<Window>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT provider, win, used, lim, unit, utilization, resets_at, status, source, updated_at FROM usage
             WHERE ?1 IS NULL OR provider = ?1 ORDER BY provider, win",
        )?;
        let rows = st
            .query_map([provider], |r| {
                Ok(Window {
                    provider: r.get(0)?,
                    name: r.get(1)?,
                    used: r.get(2)?,
                    limit: r.get(3)?,
                    unit: r.get(4)?,
                    utilization: r.get(5)?,
                    resets_at: r.get(6)?,
                    status: r.get(7)?,
                    source: r.get(8)?,
                    updated_at: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- users ----

    pub fn user_count(&self) -> Result<i64> {
        Ok(self.c().query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?)
    }

    fn row_user(r: &Row) -> rusqlite::Result<(String, Option<String>, String, String)> {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    }

    fn to_user((slack_id, name, role, agents): (String, Option<String>, String, String)) -> User {
        User {
            slack_id,
            name,
            role: Role::parse(&role).unwrap_or(Role::Viewer),
            agents: agents.split(',').map(str::trim).filter(|a| !a.is_empty()).map(String::from).collect(),
        }
    }

    pub fn user(&self, slack_id: &str) -> Result<Option<User>> {
        let c = self.c();
        let u = c
            .query_row(
                "SELECT slack_id, name, role, agents FROM users WHERE slack_id = ?1",
                [slack_id],
                Self::row_user,
            )
            .optional()?;
        Ok(u.map(Self::to_user))
    }

    pub fn users(&self) -> Result<Vec<User>> {
        let c = self.c();
        let mut st = c.prepare("SELECT slack_id, name, role, agents FROM users ORDER BY role DESC, slack_id")?;
        let rows = st.query_map([], Self::row_user)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().map(Self::to_user).collect())
    }

    pub fn set_user(&self, u: &User) -> Result<()> {
        self.c().execute(
            "INSERT INTO users (slack_id, name, role, agents, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(slack_id) DO UPDATE SET name = COALESCE(excluded.name, users.name),
                                                 role = excluded.role, agents = excluded.agents",
            params![u.slack_id, u.name, u.role.as_str(), u.agents.join(","), now()],
        )?;
        Ok(())
    }

    pub fn remove_user(&self, slack_id: &str) -> Result<()> {
        self.c().execute("DELETE FROM users WHERE slack_id = ?1", [slack_id])?;
        Ok(())
    }

    // ---- tool rules ----

    pub fn add_rule(&self, scope: &str, name: &str, kind: &str, rule: &str) -> Result<()> {
        self.c().execute(
            "INSERT OR IGNORE INTO tool_rules (scope, name, kind, rule) VALUES (?1, ?2, ?3, ?4)",
            params![scope, name, kind, rule],
        )?;
        Ok(())
    }

    pub fn remove_rule(&self, scope: &str, name: &str, kind: &str, rule: &str) -> Result<usize> {
        Ok(self.c().execute(
            "DELETE FROM tool_rules WHERE scope = ?1 AND name = ?2 AND kind = ?3 AND rule = ?4",
            params![scope, name, kind, rule],
        )?)
    }

    pub fn rules(&self, scope: &str, name: &str, kind: &str) -> Result<Vec<String>> {
        let c = self.c();
        let mut st = c.prepare("SELECT rule FROM tool_rules WHERE scope = ?1 AND name = ?2 AND kind = ?3 ORDER BY rule")?;
        let rows = st.query_map(params![scope, name, kind], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(rows)
    }

    pub fn profile_names(&self) -> Result<Vec<String>> {
        let c = self.c();
        let mut st = c.prepare("SELECT DISTINCT name FROM tool_rules WHERE scope = 'profile' ORDER BY name")?;
        let rows = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(rows)
    }

    pub fn delete_profile(&self, name: &str) -> Result<()> {
        let c = self.c();
        c.execute("DELETE FROM tool_rules WHERE scope = 'profile' AND name = ?1", [name])?;
        c.execute("DELETE FROM tool_rules WHERE scope = 'agent' AND kind = 'profile' AND rule = ?1", [name])?;
        Ok(())
    }

    pub fn meta_flag(&self, key: &str) -> Result<bool> {
        let c = self.c();
        let v: Option<String> = c.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()?;
        Ok(v.is_some())
    }

    pub fn set_meta_flag(&self, key: &str) -> Result<()> {
        self.c().execute("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, '1')", [key])?;
        Ok(())
    }

    pub fn set_kind(&self, id: i64, kind: &str) -> Result<()> {
        self.c().execute("UPDATE tasks SET kind = ?1 WHERE id = ?2", params![kind, id])?;
        Ok(())
    }

    /// Every `ask_agent` question in the tree under `root`, oldest first.
    pub fn questions_in_tree(&self, root: i64) -> Result<Vec<Task>> {
        let c = self.c();
        let mut st = c.prepare(&format!(
            "WITH RECURSIVE tree(id) AS (SELECT ?1 UNION ALL SELECT t.id FROM tasks t JOIN tree ON t.parent_id = tree.id)
             SELECT {TASK_COLS} FROM tasks WHERE id IN (SELECT id FROM tree) AND kind = 'question' ORDER BY id"
        ))?;
        let rows = st.query_map([root], row_task)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- hire proposals ----

    pub fn create_proposal(&self, proposed_by: &str, task_id: i64, spec: &str, reason: &str) -> Result<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO proposals (proposed_by, task_id, spec, reason, status, created_at) VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
            params![proposed_by, task_id, spec, reason, now()],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn proposal(&self, id: i64) -> Result<Option<Proposal>> {
        let c = self.c();
        let p = c
            .query_row(&format!("SELECT {PROPOSAL_COLS} FROM proposals WHERE id = ?1"), [id], row_proposal)
            .optional()?;
        Ok(p)
    }

    pub fn proposals(&self, pending_only: bool) -> Result<Vec<Proposal>> {
        let c = self.c();
        let clause = if pending_only { "WHERE status = 'pending'" } else { "" };
        let mut st = c.prepare(&format!("SELECT {PROPOSAL_COLS} FROM proposals {clause} ORDER BY id"))?;
        let rows = st.query_map([], row_proposal)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn decide_proposal(&self, id: i64, status: &str, by: &str, note: Option<&str>) -> Result<()> {
        self.c().execute(
            "UPDATE proposals SET status = ?1, decided_by = ?2, note = ?3 WHERE id = ?4",
            params![status, by, note, id],
        )?;
        Ok(())
    }

    pub fn set_proposal_ts(&self, id: i64, ts: &str) -> Result<()> {
        self.c().execute("UPDATE proposals SET slack_ts = ?1 WHERE id = ?2", params![ts, id])?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Proposal {
    pub id: i64,
    pub proposed_by: String,
    pub task_id: i64,
    pub spec: String,
    pub reason: String,
    pub status: String,
    pub decided_by: Option<String>,
    pub slack_ts: Option<String>,
}

const PROPOSAL_COLS: &str = "id, proposed_by, task_id, spec, reason, status, decided_by, slack_ts";

fn row_proposal(r: &Row) -> rusqlite::Result<Proposal> {
    Ok(Proposal {
        id: r.get(0)?,
        proposed_by: r.get(1)?,
        task_id: r.get(2)?,
        spec: r.get(3)?,
        reason: r.get(4)?,
        status: r.get(5)?,
        decided_by: r.get(6)?,
        slack_ts: r.get(7)?,
    })
}
