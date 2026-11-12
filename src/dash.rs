//! `hive dash`: live terminal dashboard (ratatui) — agents, provider usage, task tree, details, activity.

use crate::ipc::{self, Request};
use crate::usage::{bar, fmt_time};
use crate::util::{title_of, trunc};
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, List, ListItem, ListState, Paragraph, Row, Table, Wrap};
use ratatui::{DefaultTerminal, Frame};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Deserialize, Default)]
struct Snap {
    company: String,
    running: usize,
    max_parallel: usize,
    agents: Vec<AgentRow>,
    tasks: Vec<TaskRow>,
    usage: Vec<UsageRow>,
    paused: HashMap<String, Option<i64>>,
    events: Vec<(String, String)>,
    #[serde(default)]
    approvals: Vec<ApprovalRow>,
    #[serde(default)]
    proposals: Vec<ProposalRow>,
}

#[derive(Deserialize)]
struct ProposalRow {
    id: i64,
    proposed_by: String,
    name: Option<String>,
}

#[derive(Deserialize)]
struct ApprovalRow {
    id: i64,
    agent: String,
    task_id: i64,
    summary: String,
    rule: String,
}

#[derive(Deserialize)]
struct AgentRow {
    name: String,
    title: String,
    state: String,
    note: String,
}

#[derive(Deserialize, Clone)]
struct TaskRow {
    id: i64,
    parent_id: Option<i64>,
    title: String,
    assignee: String,
    status: String,
    #[serde(default)]
    kind: String,
}

#[derive(Deserialize)]
struct UsageRow {
    provider: String,
    name: String,
    used: Option<f64>,
    limit: Option<f64>,
    unit: Option<String>,
    utilization: Option<f64>,
    resets_at: Option<i64>,
}

enum Mode {
    Normal,
    NewTask(String),
    Reply(i64, String),
    ConfirmCancel(i64),
}

struct Dash {
    socket: PathBuf,
    snap: Snap,
    error: Option<String>,
    rows: Vec<(usize, TaskRow)>,
    list: ListState,
    detail: String,
    scroll: u16,
    mode: Mode,
    flash: Option<(String, bool, Instant)>,
    show_closed: bool,
    dirty: bool,
}

pub fn run(socket: PathBuf) -> Result<()> {
    let mut terminal = ratatui::init();
    let mut dash = Dash {
        socket,
        snap: Snap::default(),
        error: None,
        rows: Vec::new(),
        list: ListState::default(),
        detail: String::new(),
        scroll: 0,
        mode: Mode::Normal,
        flash: None,
        show_closed: true,
        dirty: true,
    };
    let res = dash.event_loop(&mut terminal);
    ratatui::restore();
    res
}

fn status_color(s: &str) -> Color {
    match s {
        "todo" => Color::Gray,
        "in_progress" => Color::Yellow,
        "waiting" => Color::Cyan,
        "blocked" => Color::Magenta,
        "done" => Color::Green,
        "failed" => Color::Red,
        _ => Color::DarkGray,
    }
}

fn state_color(s: &str) -> Color {
    if s.starts_with("busy") {
        Color::Yellow
    } else if s.starts_with("paused") || s.starts_with("waiting") {
        Color::Red
    } else if s.starts_with("queued") {
        Color::Cyan
    } else {
        Color::Green
    }
}

fn ratio_color(r: f64) -> Color {
    if r >= 0.9 {
        Color::Red
    } else if r >= 0.7 {
        Color::Yellow
    } else {
        Color::Green
    }
}

impl Dash {
    fn call(&self, req: &Request) -> Result<String, String> {
        match ipc::call(&self.socket, req) {
            Ok(r) if r.ok => Ok(r.text),
            Ok(r) => Err(r.text),
            Err(e) => Err(format!("{e:#}")),
        }
    }

    fn selected_id(&self) -> Option<i64> {
        self.list.selected().and_then(|i| self.rows.get(i)).map(|(_, t)| t.id)
    }

    fn refresh(&mut self) {
        let keep = self.selected_id();
        match self.call(&Request::Snapshot).and_then(|t| serde_json::from_str::<Snap>(&t).map_err(|e| e.to_string())) {
            Ok(s) => {
                self.snap = s;
                self.error = None;
            }
            Err(e) => {
                self.error = Some(e);
                return;
            }
        }
        // Task tree, flattened with depth.
        let tasks: Vec<&TaskRow> = self
            .snap
            .tasks
            .iter()
            .filter(|t| self.show_closed || !matches!(t.status.as_str(), "done" | "failed" | "cancelled"))
            .collect();
        let ids: HashSet<i64> = tasks.iter().map(|t| t.id).collect();
        let mut kids: HashMap<i64, Vec<&TaskRow>> = HashMap::new();
        let mut roots = Vec::new();
        for t in &tasks {
            match t.parent_id {
                Some(p) if ids.contains(&p) => kids.entry(p).or_default().push(t),
                _ => roots.push(*t),
            }
        }
        roots.reverse(); // most recent root first
        fn walk(t: &TaskRow, d: usize, kids: &HashMap<i64, Vec<&TaskRow>>, out: &mut Vec<(usize, TaskRow)>) {
            out.push((d, t.clone()));
            for k in kids.get(&t.id).into_iter().flatten() {
                walk(k, d + 1, kids, out);
            }
        }
        let mut rows = Vec::new();
        for r in roots {
            walk(r, 0, &kids, &mut rows);
        }
        self.rows = rows;
        let idx = keep
            .and_then(|id| self.rows.iter().position(|(_, t)| t.id == id))
            .or(if self.rows.is_empty() { None } else { Some(0) });
        self.list.select(idx);
        self.detail = match self.selected_id() {
            Some(id) => self.call(&Request::Show { id }).unwrap_or_else(|e| e),
            None => "No tasks. Press n to create one.".into(),
        };
    }

    fn event_loop(&mut self, term: &mut DefaultTerminal) -> Result<()> {
        let mut last = Instant::now();
        loop {
            if self.dirty || last.elapsed() >= Duration::from_secs(1) {
                self.refresh();
                self.dirty = false;
                last = Instant::now();
            }
            term.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(200))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press && self.on_key(k.code, k.modifiers) {
                        return Ok(());
                    }
                }
            }
        }
    }

    fn flash(&mut self, r: Result<String, String>) {
        let (msg, ok) = match r {
            Ok(m) => (m, true),
            Err(e) => (e, false),
        };
        self.flash = Some((msg.lines().next().unwrap_or("").to_string(), ok, Instant::now()));
        self.dirty = true;
    }

    fn move_sel(&mut self, delta: i64) {
        if self.rows.is_empty() {
            return;
        }
        let i = self.list.selected().unwrap_or(0) as i64 + delta;
        self.list.select(Some(i.clamp(0, self.rows.len() as i64 - 1) as usize));
        self.scroll = 0;
        self.dirty = true;
    }

    /// Returns true to quit.
    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
            return true;
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => match code {
                KeyCode::Char('q') | KeyCode::Esc => return true,
                KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
                KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
                KeyCode::PageDown | KeyCode::Char('J') => self.scroll = self.scroll.saturating_add(5),
                KeyCode::PageUp | KeyCode::Char('K') => self.scroll = self.scroll.saturating_sub(5),
                KeyCode::Tab | KeyCode::Char('a') => {
                    self.show_closed = !self.show_closed;
                    self.dirty = true;
                }
                KeyCode::Char('n') => self.mode = Mode::NewTask(String::new()),
                // Oldest pending tool approval: y once, Y always, x deny
                KeyCode::Char(c @ ('y' | 'Y' | 'x')) => {
                    if let Some(a) = self.snap.approvals.first() {
                        let decision = match c {
                            'y' => "once",
                            'Y' => "always",
                            _ => "deny",
                        };
                        let r = self.call(&Request::Decide { id: a.id, decision: decision.into() });
                        self.flash(r);
                    }
                }
                KeyCode::Char('r') => {
                    if let Some(id) = self.selected_id() {
                        self.mode = Mode::Reply(id, String::new());
                    }
                }
                KeyCode::Char('c') => {
                    if let Some(id) = self.selected_id() {
                        self.mode = Mode::ConfirmCancel(id);
                    }
                }
                KeyCode::Char('R') => {
                    if let Some(id) = self.selected_id() {
                        let r = self.call(&Request::Retry { id });
                        self.flash(r);
                    }
                }
                _ => {}
            },
            Mode::ConfirmCancel(id) => {
                if matches!(code, KeyCode::Char('y')) {
                    let r = self.call(&Request::Cancel { id });
                    self.flash(r);
                }
            }
            Mode::NewTask(mut buf) => match code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let text = buf.trim();
                    if !text.is_empty() {
                        let (to, body) = match text.strip_prefix('@') {
                            Some(r) => {
                                let (a, b) = r.split_once(char::is_whitespace).unwrap_or((r, ""));
                                (Some(a.to_string()), b.trim().to_string())
                            }
                            None => (None, text.to_string()),
                        };
                        let r = self.call(&Request::CreateTask { to, title: title_of(&body), description: body });
                        self.flash(r);
                    }
                }
                KeyCode::Backspace => {
                    buf.pop();
                    self.mode = Mode::NewTask(buf);
                }
                KeyCode::Char(c) => {
                    buf.push(c);
                    self.mode = Mode::NewTask(buf);
                }
                _ => self.mode = Mode::NewTask(buf),
            },
            Mode::Reply(id, mut buf) => match code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    if !buf.trim().is_empty() {
                        let r = self.call(&Request::Reply { id, message: buf.trim().to_string() });
                        self.flash(r);
                    }
                }
                KeyCode::Backspace => {
                    buf.pop();
                    self.mode = Mode::Reply(id, buf);
                }
                KeyCode::Char(c) => {
                    buf.push(c);
                    self.mode = Mode::Reply(id, buf);
                }
                _ => self.mode = Mode::Reply(id, buf),
            },
        }
        false
    }

    fn draw(&mut self, f: &mut Frame) {
        let s = &self.snap;
        let usage_h = if s.usage.is_empty() { 3 } else { s.usage.len() as u16 + s.paused.len() as u16 + 2 };
        let approvals_h = (s.approvals.len().min(3) + s.proposals.len().min(2)) as u16;
        let [header, approvals_a, body, events, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(approvals_h),
            Constraint::Min(10),
            Constraint::Length(8),
            Constraint::Length(1),
        ])
        .areas(f.area());
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(48), Constraint::Percentage(52)]).areas(body);
        let [agents_a, usage_a, tasks_a] = Layout::vertical([
            Constraint::Length(s.agents.len() as u16 + 3),
            Constraint::Length(usage_h),
            Constraint::Min(5),
        ])
        .areas(left);

        // Header
        let mut h = vec![
            Span::styled(" 🐝 hive ", Style::new().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw(format!(" {}  ·  runs {}/{}", s.company, s.running, s.max_parallel)),
        ];
        if let Some(e) = &self.error {
            h.push(Span::styled(format!("  ⚠ {}", trunc(e, 80)), Style::new().fg(Color::Red)));
        }
        f.render_widget(Line::from(h), header);

        // Pending tool approvals (oldest first; y / Y / x answer the first one)
        let lines: Vec<Line> = s
            .approvals
            .iter()
            .take(3)
            .enumerate()
            .map(|(i, a)| {
                let keys = if i == 0 { format!("  [y] once · [Y] always ({}) · [x] deny", a.rule) } else { String::new() };
                Line::from(vec![
                    Span::styled(" 🔐 ", Style::new().bg(Color::Magenta)),
                    Span::styled(
                        format!(" #{} {} (task #{}) wants {}", a.id, a.agent, a.task_id, trunc(&a.summary, 60)),
                        Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(keys, Style::new().fg(Color::DarkGray)),
                ])
            })
            .chain(s.proposals.iter().take(2).map(|p| {
                Line::from(vec![
                    Span::styled(" 🧑‍💼 ", Style::new().bg(Color::Blue)),
                    Span::styled(
                        format!(" hire #{}: {} proposes {}", p.id, p.proposed_by, p.name.as_deref().unwrap_or("?")),
                        Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  → Slack or hive proposal accept|reject {}", p.id), Style::new().fg(Color::DarkGray)),
                ])
            }))
            .collect();
        f.render_widget(Paragraph::new(lines), approvals_a);

        // Agents
        let rows: Vec<Row> = s
            .agents
            .iter()
            .map(|a| {
                Row::new(vec![
                    Cell::from(Span::styled(a.name.clone(), Style::new().add_modifier(Modifier::BOLD))),
                    Cell::from(a.title.clone()),
                    Cell::from(Span::styled(a.state.clone(), Style::new().fg(state_color(&a.state)))),
                    Cell::from(Span::styled(a.note.clone(), Style::new().fg(Color::DarkGray))),
                ])
            })
            .collect();
        let table = Table::new(
            rows,
            [Constraint::Length(11), Constraint::Length(16), Constraint::Length(24), Constraint::Min(5)],
        )
        .header(Row::new(vec!["agent", "role", "state", "note"]).style(Style::new().fg(Color::DarkGray)))
        .block(Block::bordered().title(" Agents "));
        f.render_widget(table, agents_a);

        // Usage
        let mut lines = Vec::new();
        let mut last = "";
        for w in &s.usage {
            if w.provider != last {
                let paused = s
                    .paused
                    .get(&w.provider)
                    .copied()
                    .flatten()
                    .map(|u| format!("  ⏸ paused until {}", fmt_time(u)))
                    .unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::styled(w.provider.clone(), Style::new().add_modifier(Modifier::BOLD)),
                    Span::styled(paused, Style::new().fg(Color::Red)),
                ]));
                last = &w.provider;
            }
            let ratio = w.utilization.or_else(|| match (w.used, w.limit) {
                (Some(u), Some(l)) if l > 0.0 => Some(u / l),
                _ => None,
            });
            let amount = match (w.used, w.limit) {
                (Some(u), Some(l)) => format!(" {u}/{l} {}", w.unit.clone().unwrap_or_default()),
                _ => String::new(),
            };
            lines.push(Line::from(vec![
                Span::raw(format!("  {:<10} ", trunc(&w.name, 10))),
                Span::styled(
                    ratio.map(|r| bar(r, 16)).unwrap_or_else(|| "?".to_string()),
                    Style::new().fg(ratio.map(ratio_color).unwrap_or(Color::DarkGray)),
                ),
                Span::raw(format!(" {:>4}", ratio.map(|r| format!("{:.0}%", r * 100.0)).unwrap_or_default())),
                Span::raw(amount),
                Span::styled(
                    w.resets_at.map(|t| format!(" · reset {}", fmt_time(t))).unwrap_or_default(),
                    Style::new().fg(Color::DarkGray),
                ),
            ]));
        }
        if lines.is_empty() {
            lines.push(Line::styled("no data yet", Style::new().fg(Color::DarkGray)));
        }
        f.render_widget(Paragraph::new(lines).block(Block::bordered().title(" Usage ")), usage_a);

        // Tasks
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|(d, t)| {
                ListItem::new(Line::from(vec![
                    Span::raw("  ".repeat(*d)),
                    Span::styled(
                        format!("{}#{} ", if t.kind == "question" { "❓" } else { "" }, t.id),
                        Style::new().fg(Color::DarkGray),
                    ),
                    Span::styled(format!("{:<12}", t.status), Style::new().fg(status_color(&t.status))),
                    Span::styled(format!("{} ", t.assignee), Style::new().add_modifier(Modifier::BOLD)),
                    Span::raw(t.title.clone()),
                ]))
            })
            .collect();
        let title = if self.show_closed { " Tasks (all) " } else { " Tasks (open) " };
        let list = List::new(items)
            .block(Block::bordered().title(title))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, tasks_a, &mut self.list);

        // Detail
        f.render_widget(
            Paragraph::new(self.detail.as_str())
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0))
                .block(Block::bordered().title(" Detail ")),
            right,
        );

        // Activity
        let n = events.height.saturating_sub(2) as usize;
        let ev: Vec<Line> = s
            .events
            .iter()
            .rev()
            .take(n)
            .rev()
            .map(|(t, m)| Line::from(vec![Span::styled(format!("{t} "), Style::new().fg(Color::DarkGray)), Span::raw(m.clone())]))
            .collect();
        f.render_widget(Paragraph::new(ev).block(Block::bordered().title(" Activity ")), events);

        // Footer: input, confirmation, flash or key hints
        let key = |k: &str, l: &str| {
            vec![
                Span::styled(format!(" {k} "), Style::new().fg(Color::Black).bg(Color::DarkGray)),
                Span::raw(format!(" {l}  ")),
            ]
        };
        let line = match &self.mode {
            Mode::NewTask(b) => Line::from(format!(" New task ([@agent] text, Enter / Esc) > {b}█")),
            Mode::Reply(id, b) => Line::from(format!(" Message for #{id} (Enter / Esc) > {b}█")),
            Mode::ConfirmCancel(id) => {
                Line::styled(format!(" Cancel #{id} and its sub-tasks? (y / n)"), Style::new().fg(Color::Red))
            }
            Mode::Normal => match &self.flash {
                Some((m, ok, at)) if at.elapsed() < Duration::from_secs(4) => Line::styled(
                    format!(" {m}"),
                    Style::new().fg(if *ok { Color::Green } else { Color::Red }),
                ),
                _ => Line::from(
                    [
                        key("↑↓", "task"),
                        key("n", "new"),
                        key("r", "reply"),
                        key("c", "cancel"),
                        key("R", "retry"),
                        key("tab", "closed"),
                        key("J/K", "scroll"),
                        key("q", "quit"),
                    ]
                    .concat(),
                ),
            },
        };
        f.render_widget(line, footer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn renders() {
        let snap: Snap = serde_json::from_value(serde_json::json!({
            "company": "My Company", "running": 1, "max_parallel": 2,
            "agents": [
                {"name": "ceo", "title": "CEO", "provider": "claude", "state": "busy (#1)", "note": ""},
                {"name": "dev", "title": "Developer", "provider": "claude", "state": "available", "note": "on the frontend"}
            ],
            "tasks": [
                {"id": 1, "parent_id": null, "title": "Launch v1", "assignee": "ceo", "status": "waiting"},
                {"id": 2, "parent_id": 1, "title": "Landing page", "assignee": "dev", "status": "in_progress"}
            ],
            "usage": [{"provider": "claude", "name": "five_hour", "utilization": 0.42, "resets_at": 1794493200}],
            "paused": {"claude": null},
            "events": [["11:02:03", "▶ run 3: dev on #2 \"Landing page\""]]
        })).unwrap();
        let mut d = Dash {
            socket: PathBuf::new(), snap, error: None, rows: vec![], list: ListState::default(),
            detail: "#1 Launch v1\nstatus: waiting".into(), scroll: 0, mode: Mode::Normal,
            flash: None, show_closed: true, dirty: false,
        };
        d.rows = d.snap.tasks.iter().map(|t| (t.parent_id.is_some() as usize, t.clone())).collect();
        d.list.select(Some(1));
        let mut term = Terminal::new(TestBackend::new(110, 28)).unwrap();
        term.draw(|f| d.draw(f)).unwrap();
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        println!("{out}");
        assert!(out.contains("Landing page"));
    }
}
