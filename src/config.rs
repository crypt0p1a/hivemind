use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

fn d_agent() -> String {
    "ceo".into()
}
fn d_parallel() -> usize {
    2
}
fn d_depth() -> i64 {
    4
}
fn d_claude() -> String {
    "claude".into()
}
fn d_one() -> usize {
    1
}
fn d_workdir() -> String {
    "workspace".into()
}
fn d_provider() -> String {
    "claude".into()
}
fn d_perm() -> String {
    "acceptEdits".into()
}

/// `hive.toml`
#[derive(Debug, Clone, Deserialize)]
pub struct HiveConfig {
    pub name: String,
    #[serde(default)]
    pub mission: String,
    #[serde(default = "d_agent")]
    pub default_agent: String,
    #[serde(default = "d_parallel")]
    pub max_parallel_runs: usize,
    #[serde(default = "d_depth")]
    pub max_delegation_depth: i64,
    #[serde(default = "d_claude")]
    pub claude_bin: String,
    #[serde(default)]
    pub notify: NotifyConfig,
    #[serde(default)]
    pub slack: Option<SlackConfig>,
    #[serde(default)]
    pub usage: UsageConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub collaboration: CollabConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NotifyConfig {
    #[serde(default)]
    pub macos: bool,
}

fn d_pause() -> f64 {
    0.95
}
fn d_every() -> u64 {
    300
}

#[derive(Debug, Clone, Deserialize)]
pub struct UsageConfig {
    /// No new run on a provider once one of its windows is above this ratio (until its reset).
    #[serde(default = "d_pause")]
    pub pause_above: f64,
    #[serde(default)]
    pub providers: Vec<UsageProvider>,
}

impl Default for UsageConfig {
    fn default() -> Self {
        UsageConfig { pause_above: d_pause(), providers: Vec::new() }
    }
}

/// A provider whose usage is pulled by running `command` (prints a usage report as JSON).
#[derive(Debug, Clone, Deserialize)]
pub struct UsageProvider {
    pub name: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default = "d_every")]
    pub every_secs: u64,
}

fn d_true() -> bool {
    true
}
fn d_approval_timeout() -> u64 {
    1800
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolsConfig {
    /// Ask the admins (Slack buttons, CLI, dashboard) when an agent wants a tool it isn't allowed
    #[serde(default = "d_true")]
    pub approvals: bool,
    /// Denied when nobody answers within this delay
    #[serde(default = "d_approval_timeout")]
    pub approval_timeout_secs: u64,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        ToolsConfig { approvals: true, approval_timeout_secs: d_approval_timeout() }
    }
}

fn d_pair() -> usize {
    3
}
fn d_tree() -> usize {
    12
}
fn d_recruiters() -> Vec<String> {
    vec!["ceo".into(), "cto".into()]
}

/// Agents talking to each other (`ask_agent`) and proposing hires (`propose_agent`).
#[derive(Debug, Clone, Deserialize)]
pub struct CollabConfig {
    /// Questions between the same two agents within one root task before a human arbitrates
    #[serde(default = "d_pair")]
    pub max_questions_per_pair: usize,
    /// Questions within one root task (all agents) before a human arbitrates
    #[serde(default = "d_tree")]
    pub max_questions_per_tree: usize,
    /// Agents allowed to propose new hires
    #[serde(default = "d_recruiters")]
    pub recruiters: Vec<String>,
}

impl Default for CollabConfig {
    fn default() -> Self {
        CollabConfig {
            max_questions_per_pair: d_pair(),
            max_questions_per_tree: d_tree(),
            recruiters: d_recruiters(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SlackConfig {
    /// Channel id (C…) where the bot posts and listens
    pub channel: String,
    /// First admins (Slack ids U…), created at startup; other users are managed live (`user add`)
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// xoxb-… (or HIVE_SLACK_BOT_TOKEN)
    #[serde(default)]
    pub bot_token: Option<String>,
    /// xapp-… with connections:write (or HIVE_SLACK_APP_TOKEN)
    #[serde(default)]
    pub app_token: Option<String>,
}

fn secret(v: &Option<String>, env: &str) -> Option<String> {
    v.clone()
        .filter(|t| !t.trim().is_empty())
        .or_else(|| std::env::var(env).ok().filter(|t| !t.trim().is_empty()))
}

impl SlackConfig {
    pub fn bot_token(&self) -> Option<String> {
        secret(&self.bot_token, "HIVE_SLACK_BOT_TOKEN")
    }
    pub fn app_token(&self) -> Option<String> {
        secret(&self.app_token, "HIVE_SLACK_APP_TOKEN")
    }
}

/// `agents/<name>.toml`
#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub reports_to: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Usage provider this agent consumes (paused together when its quota is exhausted)
    #[serde(default = "d_provider")]
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "d_one")]
    pub max_concurrent: usize,
    #[serde(default = "d_workdir")]
    pub workdir: String,
    #[serde(default = "d_perm")]
    pub permission_mode: String,
    /// Initial tool rules (seeded once into the DB; then managed live with `tools …`)
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Initial tool profiles (read, web, edit, dev, or your own)
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub prompt: String,
}

#[derive(Debug, Clone)]
pub struct Company {
    pub root: PathBuf,
    pub cfg: HiveConfig,
    /// Reloadable at runtime (agents created / removed on the fly).
    agents: Arc<RwLock<Vec<AgentConfig>>>,
}

impl Company {
    /// `$HIVE_HOME`, or the closest parent directory holding a `hive.toml`.
    pub fn find() -> Result<PathBuf> {
        if let Ok(h) = std::env::var("HIVE_HOME") {
            return Ok(PathBuf::from(h).canonicalize()?);
        }
        let mut dir = std::env::current_dir()?;
        loop {
            if dir.join("hive.toml").is_file() {
                return Ok(dir.canonicalize()?);
            }
            if !dir.pop() {
                bail!("no hive.toml here or in any parent directory (run `hive init` or set HIVE_HOME)");
            }
        }
    }

    pub fn load(root: &Path) -> Result<Company> {
        let root = root.canonicalize()?;
        let raw = std::fs::read_to_string(root.join("hive.toml")).context("reading hive.toml")?;
        let cfg: HiveConfig = toml::from_str(&raw).context("invalid hive.toml")?;

        let agents = load_agents(&root, &cfg)?;
        Ok(Company { root, cfg, agents: Arc::new(RwLock::new(agents)) })
    }

    /// Snapshot of the current agents.
    pub fn agents(&self) -> Vec<AgentConfig> {
        self.agents.read().unwrap().clone()
    }

    pub fn agent(&self, name: &str) -> Option<AgentConfig> {
        self.agents.read().unwrap().iter().find(|a| a.name == name).cloned()
    }

    /// Re-reads `agents/*.toml`; on error the current agents are kept.
    pub fn reload_agents(&self) -> Result<Vec<AgentConfig>> {
        let agents = load_agents(&self.root, &self.cfg)?;
        *self.agents.write().unwrap() = agents.clone();
        Ok(agents)
    }

    pub fn agents_dir(&self) -> PathBuf {
        self.root.join("agents")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join(".hive")
    }

    pub fn workdir(&self, a: &AgentConfig) -> PathBuf {
        let p = PathBuf::from(&a.workdir);
        if p.is_absolute() {
            p
        } else {
            self.root.join(p)
        }
    }
}

pub fn log_path(root: &Path, run_id: i64, agent: &str) -> PathBuf {
    root.join(".hive").join("logs").join(format!("run-{run_id}-{agent}.jsonl"))
}

/// Unix sockets paths are limited to ~104 bytes on macOS: fall back to the temp dir.
pub fn socket_path(root: &Path) -> PathBuf {
    if let Ok(s) = std::env::var("HIVE_SOCKET") {
        return PathBuf::from(s);
    }
    let p = root.join(".hive").join("hive.sock");
    if p.as_os_str().len() < 100 {
        return p;
    }
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut h);
    std::env::temp_dir().join(format!("hive-{:x}.sock", h.finish()))
}

fn load_agents(root: &Path, cfg: &HiveConfig) -> Result<Vec<AgentConfig>> {
    let dir = root.join("agents");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    files.sort();
    let mut agents = Vec::new();
    for p in files {
        let raw = std::fs::read_to_string(&p)?;
        let a: AgentConfig = toml::from_str(&raw).with_context(|| format!("invalid {}", p.display()))?;
        agents.push(a);
    }
    validate_agents(cfg, &agents)?;
    Ok(agents)
}

pub fn valid_agent_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 32 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn validate_agents(cfg: &HiveConfig, agents: &[AgentConfig]) -> Result<()> {
    let mut names = HashSet::new();
    for a in agents {
        if !valid_agent_name(&a.name) {
            bail!("invalid agent name '{}' (letters, digits, - and _)", a.name);
        }
        if !names.insert(a.name.clone()) {
            bail!("agent '{}' defined twice", a.name);
        }
    }
    for a in agents {
        if let Some(r) = &a.reports_to {
            if !names.contains(r) {
                bail!("{}: reports_to '{r}' does not exist", a.name);
            }
        }
    }
    if !names.contains(&cfg.default_agent) {
        bail!("default_agent '{}' does not exist in agents/", cfg.default_agent);
    }
    Ok(())
}
