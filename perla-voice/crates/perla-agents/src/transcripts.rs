//! Transcript discovery + turn-end / interrupt parsing — port of
//! `TerminalSession.swift`'s static helpers. These parsers encode the exact
//! JSONL shapes Claude Code and Codex write; semantics are byte-for-byte with
//! the macOS app (and its fixture tests).

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use crate::types::AgentTool;

#[derive(Debug, Clone)]
pub struct NewestFile {
    pub path: PathBuf,
    pub size: u64,
    pub mtime: SystemTime,
}

#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub ok: bool,
    pub summary: String,
    pub session_id: Option<String>,
    /// The USER killed the turn (Esc in the TUI) — not a failure and not a
    /// completion. Consumers skip "needs attention" and just make the model
    /// aware the task stopped.
    pub interrupted: bool,
}

/// Normalize a cwd so `~/foo`, `/Users/x/foo` and `/Users/x/foo/` all hash to
/// the same key — a mismatch silently spawns a second session per workspace.
pub fn normalize_cwd(cwd: &str) -> String {
    let expanded = if let Some(rest) = cwd.strip_prefix("~/") {
        dirs::home_dir()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(cwd))
    } else if cwd == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(cwd))
    } else {
        PathBuf::from(cwd)
    };
    let mut s = expanded.to_string_lossy().to_string();
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    s
}

/// Transcript directory for (tool, cwd).
pub fn transcript_dir(tool: AgentTool, cwd: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    match tool {
        AgentTool::Claude => {
            // ~/.claude/projects/<cwd with / and . → ->
            let encoded: String = cwd
                .chars()
                .map(|c| if c == '/' || c == '.' { '-' } else { c })
                .collect();
            Some(home.join(".claude/projects").join(encoded))
        }
        AgentTool::Codex => {
            // ~/.codex/sessions/<year>/<month>/<day>/ — newest day dir.
            // Cached 30s: it costs three directory listings and every watcher
            // hits it multiple times per second. The short TTL also covers
            // midnight rollover.
            static CACHE: Mutex<Option<(PathBuf, Instant)>> = Mutex::new(None);
            {
                let cache = CACHE.lock().unwrap();
                if let Some((dir, at)) = cache.as_ref() {
                    if at.elapsed() < Duration::from_secs(30) {
                        return Some(dir.clone());
                    }
                }
            }
            let root = home.join(".codex/sessions");
            let day = max_child_dir(&root)
                .and_then(|y| max_child_dir(&y))
                .and_then(|m| max_child_dir(&m))?;
            *CACHE.lock().unwrap() = Some((day.clone(), Instant::now()));
            Some(day)
        }
    }
}

fn max_child_dir(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<PathBuf> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if best.as_ref().is_none_or(|b| {
            path.file_name().unwrap_or_default() > b.file_name().unwrap_or_default()
        }) {
            best = Some(path);
        }
    }
    best
}

/// The newest transcript for (tool, cwd). For Claude the directory itself is
/// cwd-scoped, so newest-by-mtime is enough. Codex's day directory is GLOBAL
/// — every workspace's sessions land in one folder — so we pick the newest
/// file whose `session_meta.cwd` matches, falling back to plain newest (a
/// brand-new session's meta line may not have flushed yet).
pub fn newest_transcript(tool: AgentTool, cwd: &str) -> Option<NewestFile> {
    let dir = transcript_dir(tool, cwd)?;
    match tool {
        AgentTool::Claude => newest_jsonl(&dir),
        AgentTool::Codex => newest_codex_jsonl(&dir, cwd),
    }
}

pub fn has_recent_transcript(tool: AgentTool, cwd: &str) -> bool {
    newest_transcript(tool, cwd)
        .and_then(|n| n.mtime.elapsed().ok())
        .map(|age| age < Duration::from_secs(7 * 24 * 3600))
        .unwrap_or(false)
}

fn newest_jsonl(dir: &Path) -> Option<NewestFile> {
    let mut best: Option<NewestFile> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if best.as_ref().is_none_or(|b| mtime > b.mtime) {
            best = Some(NewestFile {
                path,
                size: meta.len(),
                mtime,
            });
        }
    }
    best
}

pub fn newest_codex_jsonl(dir: &Path, cwd: &str) -> Option<NewestFile> {
    let target = normalize_cwd(cwd);
    let mut files: Vec<NewestFile> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            Some(NewestFile {
                path: e.path(),
                size: meta.len(),
                mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            })
        })
        .collect();
    files.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    // Header reads are ~8KB each — cap how many we inspect.
    for f in files.iter().take(12) {
        if let Some(c) = codex_session_cwd(&f.path) {
            if normalize_cwd(&c) == target {
                return Some(f.clone());
            }
        }
    }
    files.into_iter().next()
}

/// `cwd` from a Codex transcript's first (session_meta) line. Memoized per
/// path — the header is immutable once written. nil results are NOT cached
/// (a brand-new file's header may not have flushed yet).
pub fn codex_session_cwd(path: &Path) -> Option<String> {
    static CACHE: Mutex<Option<std::collections::HashMap<PathBuf, String>>> = Mutex::new(None);
    {
        let cache = CACHE.lock().unwrap();
        if let Some(map) = cache.as_ref() {
            if let Some(v) = map.get(path) {
                return Some(v.clone());
            }
        }
    }
    let first = read_head(path, 8192)?;
    let line = first.lines().next()?;
    let v: Value = serde_json::from_str(line).ok()?;
    let cwd = v.get("payload")?.get("cwd")?.as_str()?.to_string();
    let mut cache = CACHE.lock().unwrap();
    let map = cache.get_or_insert_with(Default::default);
    if map.len() >= 512 {
        map.clear();
    }
    map.insert(path.to_path_buf(), cwd.clone());
    Some(cwd)
}

fn read_head(path: &Path, max: usize) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; max];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(String::from_utf8_lossy(&buf).to_string())
}

/// Last `max_bytes` of a file, dropping a partial first line so JSON parses.
pub fn read_tail(path: &Path, max_bytes: u64) -> String {
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let size = f.seek(SeekFrom::End(0)).unwrap_or(0);
    let start = size.saturating_sub(max_bytes);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut data = Vec::new();
    if f.read_to_end(&mut data).is_err() {
        return String::new();
    }
    let mut text = String::from_utf8_lossy(&data).to_string();
    if start > 0 {
        if let Some(nl) = text.find('\n') {
            text = text[nl + 1..].to_string();
        }
    }
    text
}

// ── Per-tool turn-end parsers (match the macOS app byte-for-byte) ──────────

/// A parseable turn-end line → the agent's final message ("Done." when empty).
pub fn parse_turn_end(tool: AgentTool, line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    match tool {
        AgentTool::Claude => {
            if v.get("type")?.as_str()? != "assistant" {
                return None;
            }
            let msg = v.get("message")?;
            if msg.get("stop_reason")?.as_str()? != "end_turn" {
                return None;
            }
            let content = msg.get("content")?.as_array()?;
            let text: Vec<&str> = content
                .iter()
                .filter(|c| c.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
                .collect();
            let joined = text.join("\n");
            Some(if joined.is_empty() {
                "Done.".into()
            } else {
                joined
            })
        }
        AgentTool::Codex => {
            if v.get("type")?.as_str()? != "event_msg" {
                return None;
            }
            let p = v.get("payload")?;
            if p.get("type")?.as_str()? != "task_complete" {
                return None;
            }
            let text = p
                .get("last_agent_message")
                .and_then(|m| m.as_str())
                .unwrap_or("");
            Some(if text.is_empty() {
                "Done.".into()
            } else {
                text.into()
            })
        }
    }
}

/// A line that means the USER killed the turn mid-flight (Esc in the TUI).
/// Claude writes a synthetic user message "[Request interrupted by user…]";
/// Codex emits an `event_msg` `turn_aborted`. Neither matches
/// `parse_turn_end`, so without this the tail keeps waiting on a dead turn.
pub fn parse_turn_interrupt(tool: AgentTool, line: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    match tool {
        AgentTool::Claude => {
            v.get("type").and_then(|t| t.as_str()) == Some("user")
                && v.get("message")
                    .map(|m| claude_interrupt_content(m.get("content")))
                    .unwrap_or(false)
        }
        AgentTool::Codex => {
            v.get("type").and_then(|t| t.as_str()) == Some("event_msg")
                && v.get("payload")
                    .and_then(|p| p.get("type"))
                    .and_then(|t| t.as_str())
                    == Some("turn_aborted")
        }
    }
}

/// True when a Claude user-message `content` value (plain string or item
/// array, depending on CLI version) is the synthetic interrupt marker.
/// Shared with the digest parser so both drift together.
pub fn claude_interrupt_content(content: Option<&Value>) -> bool {
    match content {
        Some(Value::String(s)) => s.starts_with("[Request interrupted by user"),
        Some(Value::Array(items)) => items.iter().any(|i| {
            i.get("type").and_then(|t| t.as_str()) == Some("text")
                && i.get("text")
                    .and_then(|t| t.as_str())
                    .map(|t| t.starts_with("[Request interrupted by user"))
                    .unwrap_or(false)
        }),
        _ => false,
    }
}

pub fn extract_session_id(tool: AgentTool, path: &Path) -> Option<String> {
    match tool {
        // Filename is `<session-uuid>.jsonl`.
        AgentTool::Claude => path.file_stem().map(|s| s.to_string_lossy().to_string()),
        // First line is `session_meta` with payload.id.
        AgentTool::Codex => {
            let head = read_head(path, 8192)?;
            let line = head.lines().next()?;
            let v: Value = serde_json::from_str(line).ok()?;
            v.get("payload")?.get("id")?.as_str().map(str::to_string)
        }
    }
}

// ── Turn-end wait (rolling silence budget) ─────────────────────────────────

/// Tail the newest transcript until a turn-end / interrupt line appears.
///
/// The deadline is ROLLING, not flat: a long agent turn keeps the transcript
/// growing, so we only give up after it has been silent for the whole budget
/// (a flat cap reported every long build as "timed out" mid-work). The budget
/// is generous because one long quiet tool call writes nothing between start
/// and result. There is no absolute cap — the caller aborts the task on End.
pub async fn wait_for_turn_end(tool: AgentTool, cwd: &str) -> TurnOutcome {
    let silence_budget = Duration::from_secs(10 * 60);
    let mut last_activity = Instant::now();
    let mut saw_growth = false;
    let mut watched: Option<PathBuf> = None;
    let mut offset: u64 = 0;
    let mut starting_mtime: Option<SystemTime> = None;
    let mut ticks: u64 = 0;

    while last_activity.elapsed() < silence_budget {
        tokio::time::sleep(Duration::from_millis(300)).await;
        ticks += 1;

        // Pin to a specific transcript on first sight, then only tail
        // additions. Roll-over discovery (Codex resume / Claude compaction
        // can move the turn-end line to a NEW file mid-turn) runs every ~3s.
        if watched.is_none() {
            let Some(newest) = newest_transcript(tool, cwd) else {
                continue;
            };
            offset = newest.size;
            starting_mtime = Some(newest.mtime);
            watched = Some(newest.path);
            last_activity = Instant::now(); // transcript appeared — activity
        } else if ticks.is_multiple_of(10) {
            if let Some(newest) = newest_transcript(tool, cwd) {
                if Some(&newest.path) != watched.as_ref()
                    && starting_mtime.is_some_and(|m| newest.mtime > m)
                {
                    watched = Some(newest.path);
                    offset = 0;
                    starting_mtime = Some(newest.mtime);
                    last_activity = Instant::now();
                }
            }
        }

        let Some(path) = watched.as_ref() else {
            continue;
        };
        let Some(chunk) = read_from(path, offset) else {
            continue;
        };
        offset += chunk.len() as u64;
        if !chunk.is_empty() {
            saw_growth = true;
            last_activity = Instant::now();
        }

        let text = String::from_utf8_lossy(&chunk);
        for line in text.lines() {
            if let Some(summary) = parse_turn_end(tool, line) {
                return TurnOutcome {
                    ok: true,
                    summary,
                    session_id: extract_session_id(tool, path),
                    interrupted: false,
                };
            }
            if parse_turn_interrupt(tool, line) {
                return TurnOutcome {
                    ok: false,
                    summary: "The user interrupted the agent in the terminal — the task stopped mid-turn.".into(),
                    session_id: extract_session_id(tool, path),
                    interrupted: true,
                };
            }
        }
    }

    // Two silences: a transcript that grew but never yielded a parseable
    // turn-end is the format-drift canary; one that never grew means the
    // agent never started.
    let summary = if saw_growth {
        "I lost track of the agent — its transcript went quiet without a clear finish. It may be waiting for input, or its transcript format changed."
    } else {
        "Timed out waiting for the agent to start."
    };
    TurnOutcome {
        ok: false,
        summary: summary.into(),
        session_id: None,
        interrupted: false,
    }
}

fn read_from(path: &Path, offset: u64) -> Option<Vec<u8>> {
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut data = Vec::new();
    f.read_to_end(&mut data).ok()?;
    Some(data)
}
