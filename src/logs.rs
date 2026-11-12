//! `hive logs`: readable view of a run's stream-json log.

use crate::util::trunc;
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// `target` is a run id, or an agent name (its latest run).
fn find_log(root: &Path, target: &str) -> Result<PathBuf> {
    let dir = root.join(".hive").join("logs");
    let mut best: Option<(i64, PathBuf)> = None;
    for e in std::fs::read_dir(&dir)?.filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix("run-").and_then(|r| r.strip_suffix(".jsonl")) else { continue };
        let Some((id, agent)) = rest.split_once('-') else { continue };
        let Ok(id) = id.parse::<i64>() else { continue };
        let matches = match target.parse::<i64>() {
            Ok(n) => n == id,
            Err(_) => agent == target,
        };
        if matches && best.as_ref().is_none_or(|(b, _)| id > *b) {
            best = Some((id, e.path()));
        }
    }
    best.map(|(_, p)| p).ok_or_else(|| anyhow!("no log for '{target}'"))
}

pub fn show(root: &Path, target: &str, follow: bool, raw: bool) -> Result<()> {
    let path = find_log(root, target)?;
    eprintln!("{}", path.display());
    let mut reader = BufReader::new(std::fs::File::open(&path)?);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            if follow {
                std::thread::sleep(std::time::Duration::from_millis(500));
                continue;
            }
            break;
        }
        if raw {
            print!("{line}");
        } else if let Ok(v) = serde_json::from_str::<Value>(&line) {
            pretty(&v);
        }
    }
    Ok(())
}

fn short(v: &Value, n: usize) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    trunc(&s.replace('\n', " ⏎ "), n)
}

fn pretty(v: &Value) {
    match v["type"].as_str().unwrap_or("") {
        "hive" => println!(
            "▶ run {} — {} on task #{}\n{}\n",
            v["run"], v["agent"].as_str().unwrap_or("?"), v["task"], v["prompt"].as_str().unwrap_or("")
        ),
        "system" if v["subtype"] == "init" => println!(
            "· session {} · model {}",
            v["session_id"].as_str().unwrap_or("?"),
            v["model"].as_str().unwrap_or("?")
        ),
        "assistant" => {
            for c in v["message"]["content"].as_array().into_iter().flatten() {
                match c["type"].as_str() {
                    Some("text") => println!("{}", c["text"].as_str().unwrap_or("")),
                    Some("tool_use") => println!("→ {} {}", c["name"].as_str().unwrap_or("?"), short(&c["input"], 200)),
                    _ => {}
                }
            }
        }
        "user" => {
            for c in v["message"]["content"].as_array().into_iter().flatten() {
                if c["type"] == "tool_result" {
                    let content = match &c["content"] {
                        Value::Array(parts) => parts
                            .iter()
                            .filter_map(|p| p["text"].as_str())
                            .collect::<Vec<_>>()
                            .join(" "),
                        other => short(other, 1000),
                    };
                    println!("  ← {}", trunc(&content.replace('\n', " ⏎ "), 200));
                }
            }
        }
        "result" => println!(
            "\n■ end ({}{}, {} turns)\n{}",
            v["subtype"].as_str().unwrap_or("?"),
            v["total_cost_usd"].as_f64().map(|c| format!(", ${c:.3}")).unwrap_or_default(),
            v["num_turns"],
            v["result"].as_str().unwrap_or("")
        ),
        "hive_stderr" => println!("! stderr: {}", v["text"].as_str().unwrap_or("")),
        _ => {}
    }
}
