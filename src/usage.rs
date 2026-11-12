//! Usage API: where each model provider stands in its current period(s).
//!
//! Normalized report (pushed with `hive usage --push`, the socket op `report_usage`,
//! or printed on stdout by a configured `[[usage.providers]] command`):
//!
//! ```json
//! {"provider": "openai",
//!  "windows": [{"name": "monthly", "used": 12.5, "limit": 50, "unit": "usd",
//!               "utilization": 0.25, "resets_at": 1795000000, "status": "allowed"}]}
//! ```
//! Every window field but `name` is optional; `utilization` (0..1) is derived from used/limit when absent.
//! The `claude` provider is fed automatically by the `rate_limit_event`s of every run.

use crate::daemon::App;
use crate::util::trunc;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Window {
    pub name: String,
    #[serde(default)]
    pub used: Option<f64>,
    #[serde(default)]
    pub limit: Option<f64>,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub utilization: Option<f64>,
    /// Unix seconds
    #[serde(default)]
    pub resets_at: Option<i64>,
    /// allowed | allowed_warning | rejected | …
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default, skip_deserializing)]
    pub provider: String,
    #[serde(default, skip_deserializing)]
    pub updated_at: String,
    #[serde(default, skip_deserializing)]
    pub source: String,
}

impl Window {
    pub fn ratio(&self) -> Option<f64> {
        self.utilization.or_else(|| match (self.used, self.limit) {
            (Some(u), Some(l)) if l > 0.0 => Some(u / l),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub provider: String,
    pub windows: Vec<Window>,
}

pub fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn record(app: &App, report: &Report, source: &str) -> Result<()> {
    if report.provider.trim().is_empty() || report.windows.is_empty() {
        bail!("empty usage report (provider and at least one window required)");
    }
    for w in &report.windows {
        app.db.set_usage(&report.provider, w, source)?;
    }
    Ok(())
}

/// `{"type":"rate_limit_event","rate_limit_info":{"unifiedWindows":{"five_hour":{"utilization":…,"resetsAt":…,"status":…}}}}`
pub fn from_claude_event(v: &Value) -> Option<Report> {
    let info = &v["rate_limit_info"];
    let mut windows = Vec::new();
    if let Some(map) = info["unifiedWindows"].as_object() {
        for (name, w) in map {
            windows.push(Window {
                name: name.clone(),
                used: None,
                limit: None,
                unit: Some("%".into()),
                utilization: w["utilization"].as_f64(),
                resets_at: w["resetsAt"].as_i64(),
                status: w["status"].as_str().map(String::from),
                provider: String::new(),
                updated_at: String::new(),
                source: String::new(),
            });
        }
    } else if let Some(name) = info["rateLimitType"].as_str() {
        windows.push(Window {
            name: name.into(),
            used: None,
            limit: None,
            unit: None,
            utilization: info["utilization"].as_f64(),
            resets_at: info["resetsAt"].as_i64(),
            status: info["status"].as_str().map(String::from),
            provider: String::new(),
            updated_at: String::new(),
            source: String::new(),
        });
    }
    (!windows.is_empty()).then(|| Report { provider: "claude".into(), windows })
}

/// If new runs on `provider` must wait, until when (unix seconds; `None` reset date = 15 min).
pub fn paused_until(app: &App, provider: &str) -> Result<Option<i64>> {
    let threshold = app.company.cfg.usage.pause_above;
    let now = now_unix();
    let mut until: Option<i64> = None;
    for w in app.db.usage(Some(provider))? {
        let exhausted = w.status.as_deref().is_some_and(|s| s == "rejected" || s == "limited")
            || w.ratio().is_some_and(|r| r >= threshold);
        if !exhausted {
            continue;
        }
        let reset = w.resets_at.unwrap_or(now + 900);
        if reset > now {
            until = Some(until.map_or(reset, |u| u.max(reset)));
        }
    }
    Ok(until)
}

pub fn fmt_time(ts: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|d| d.format("%a %d/%m %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

pub fn bar(ratio: f64, width: usize) -> String {
    let filled = ((ratio.clamp(0.0, 1.0)) * width as f64).round() as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

pub fn text(app: &App) -> Result<String> {
    let rows = app.db.usage(None)?;
    if rows.is_empty() {
        return Ok("No usage data yet (claude fills in with the first run).".into());
    }
    let mut s = String::new();
    let mut last = String::new();
    for w in &rows {
        let provider = w.provider.as_str();
        if provider != last {
            let paused = paused_until(app, provider)?
                .map(|u| format!("  ⏸ paused until {}", fmt_time(u)))
                .unwrap_or_default();
            s += &format!("{provider}{paused}\n");
            last = provider.to_string();
        }
        let ratio = w.ratio();
        let amount = match (w.used, w.limit) {
            (Some(u), Some(l)) => format!(" {u}/{l}{}", w.unit.as_deref().map(|x| format!(" {x}")).unwrap_or_default()),
            (Some(u), None) => format!(" {u}{}", w.unit.as_deref().map(|x| format!(" {x}")).unwrap_or_default()),
            _ => String::new(),
        };
        s += &format!(
            "  {:<12} {} {:>4}{}{}{} · maj {}\n",
            trunc(&w.name, 12),
            ratio.map(|r| bar(r, 20)).unwrap_or_else(|| " ".repeat(20)),
            ratio.map(|r| format!("{:.0}%", r * 100.0)).unwrap_or_else(|| "?".into()),
            amount,
            w.resets_at.map(|t| format!(" · reset {}", fmt_time(t))).unwrap_or_default(),
            w.status.as_deref().filter(|s| *s != "allowed").map(|s| format!(" · {s}")).unwrap_or_default(),
            w.updated_at
        );
    }
    Ok(s.trim_end().to_string())
}

/// Polls the `[[usage.providers]]` that have a `command`.
pub async fn poll_loop(app: Arc<App>) {
    let providers = app.company.cfg.usage.providers.clone();
    if providers.iter().all(|p| p.command.is_none()) {
        return;
    }
    let mut last: Vec<i64> = vec![0; providers.len()];
    loop {
        let now = now_unix();
        for (i, p) in providers.iter().enumerate() {
            let Some(cmd) = &p.command else { continue };
            if now - last[i] < p.every_secs as i64 {
                continue;
            }
            last[i] = now;
            let out = tokio::process::Command::new("sh")
                .args(["-c", cmd])
                .current_dir(&app.company.root)
                .output()
                .await;
            let res: Result<()> = match out {
                Ok(o) if o.status.success() => serde_json::from_slice::<Value>(&o.stdout)
                    .map_err(anyhow::Error::from)
                    .and_then(|mut v| {
                        if v.get("provider").is_none() {
                            v["provider"] = Value::String(p.name.clone());
                        }
                        let r: Report = serde_json::from_value(v)?;
                        record(&app, &r, "command")
                    }),
                Ok(o) => Err(anyhow::anyhow!("code {}: {}", o.status, String::from_utf8_lossy(&o.stderr).trim())),
                Err(e) => Err(e.into()),
            };
            if let Err(e) = res {
                eprintln!("usage {}: {e:#}", p.name);
            }
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}
