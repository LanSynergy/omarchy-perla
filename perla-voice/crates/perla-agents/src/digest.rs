//! `AgentDigest` — a read-only snapshot of what the agent is doing, parsed
//! from the tail of its JSONL transcript. Port of `AgentDigest.swift`.
//! Powers `check_agent_session` and detail-mode narration.

use std::path::Path;

use serde_json::Value;

use crate::transcripts::{self, claude_interrupt_content};
use crate::types::AgentTool;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Todo {
    pub text: String,
    /// pending | in_progress | completed
    pub status: String,
}

#[derive(Debug, Clone, Default)]
pub struct AgentDigest {
    pub session_id: Option<String>,
    pub turn_complete: bool,
    pub last_message: Option<String>,
    pub todos: Vec<Todo>,
    /// e.g. "Edit NotchPerlaView.swift", "Bash xcodebuild …"
    pub recent_actions: Vec<String>,
    /// Paths the agent wrote/edited, oldest first, deduped (most recent wins).
    pub changed_files: Vec<String>,
}

impl AgentDigest {
    fn note_changed_file(&mut self, path: &str) {
        if path.is_empty() {
            return;
        }
        self.changed_files.retain(|p| p != path);
        self.changed_files.push(path.to_string());
    }
}

/// Digest the newest transcript of (tool, cwd). None if the workspace has no
/// agent transcript yet.
pub fn digest(tool: AgentTool, cwd: &str) -> Option<AgentDigest> {
    let newest = transcripts::newest_transcript(tool, cwd)?;
    digest_file(tool, &newest.path)
}

/// Digest a SPECIFIC transcript — for pinned sessions, where newest-in-folder
/// could be a same-folder sibling's file.
pub fn digest_file(tool: AgentTool, path: &Path) -> Option<AgentDigest> {
    let mut d = AgentDigest {
        session_id: transcripts::extract_session_id(tool, path),
        ..Default::default()
    };
    let tail = transcripts::read_tail(path, 256 * 1024);
    let lines: Vec<&str> = tail.lines().collect();
    match tool {
        AgentTool::Claude => parse_claude(&lines, &mut d),
        AgentTool::Codex => parse_codex(&lines, &mut d),
    }
    let keep_from = d.recent_actions.len().saturating_sub(10);
    d.recent_actions.drain(..keep_from);
    let keep_from = d.changed_files.len().saturating_sub(20);
    d.changed_files.drain(..keep_from);
    Some(d)
}

// Public (not private) so fixture tests can feed lines without the filesystem.
pub fn parse_claude(lines: &[&str], d: &mut AgentDigest) {
    for line in lines {
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        // An Esc interrupt writes a synthetic USER line, never an assistant
        // stop_reason — without this the digest reports an interrupted turn
        // as still running.
        if obj.get("type").and_then(|t| t.as_str()) == Some("user") {
            if let Some(umsg) = obj.get("message") {
                if claude_interrupt_content(umsg.get("content")) {
                    d.turn_complete = true;
                    continue;
                }
            }
        }

        if obj.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let Some(msg) = obj.get("message") else {
            continue;
        };

        if let Some(stop) = msg.get("stop_reason").and_then(|s| s.as_str()) {
            d.turn_complete = stop == "end_turn";
        }

        let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for c in content {
            match c.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = c.get("text").and_then(|t| t.as_str()) {
                        if !t.trim().is_empty() {
                            d.last_message = Some(t.to_string());
                        }
                    }
                }
                Some("tool_use") => {
                    let name = c.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                    let empty = Value::Object(Default::default());
                    let input = c.get("input").unwrap_or(&empty);
                    if name == "TodoWrite" {
                        if let Some(todos) = input.get("todos").and_then(|t| t.as_array()) {
                            d.todos = todos
                                .iter()
                                .filter_map(|td| {
                                    let text = td
                                        .get("content")
                                        .or_else(|| td.get("activeForm"))
                                        .and_then(|t| t.as_str())?;
                                    Some(Todo {
                                        text: text.to_string(),
                                        status: td
                                            .get("status")
                                            .and_then(|s| s.as_str())
                                            .unwrap_or("pending")
                                            .to_string(),
                                    })
                                })
                                .collect();
                        }
                    } else {
                        if MUTATING_CLAUDE_TOOLS.contains(&name) {
                            if let Some(path) = input.get("file_path").and_then(|p| p.as_str()) {
                                d.note_changed_file(path);
                            }
                        }
                        d.recent_actions.push(describe(name, input));
                    }
                }
                _ => {}
            }
        }
    }
}

// Codex writes two kinds of line we care about:
//   • `event_msg`     — turn lifecycle + the final agent message.
//   • `response_item` — the plan (`update_plan`) and every tool call
//     (`exec_command` / `shell_command` / `apply_patch` / …).
pub fn parse_codex(lines: &[&str], d: &mut AgentDigest) {
    for line in lines {
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match obj.get("type").and_then(|t| t.as_str()) {
            Some("event_msg") => {
                let Some(p) = obj.get("payload") else {
                    continue;
                };
                match p.get("type").and_then(|t| t.as_str()) {
                    Some("task_complete") => {
                        d.turn_complete = true;
                        if let Some(m) = p.get("last_agent_message").and_then(|m| m.as_str()) {
                            if !m.is_empty() {
                                d.last_message = Some(m.to_string());
                            }
                        }
                    }
                    // Esc / user interrupt — the turn is over without a
                    // task_complete ever arriving.
                    Some("turn_aborted") => d.turn_complete = true,
                    Some("agent_message") => {
                        if let Some(m) = p.get("message").and_then(|m| m.as_str()) {
                            if !m.is_empty() {
                                d.last_message = Some(m.to_string());
                                d.turn_complete = false;
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some("response_item") => {
                let Some(p) = obj.get("payload") else {
                    continue;
                };
                if p.get("type").and_then(|t| t.as_str()) != Some("function_call") {
                    continue;
                }
                let Some(name) = p.get("name").and_then(|n| n.as_str()) else {
                    continue;
                };
                // `arguments` is a JSON-ENCODED STRING, not a nested object.
                let args = decode_args(p.get("arguments"));
                match name {
                    "update_plan" => {
                        // Status values (pending|in_progress|completed) already
                        // match Todo, so "3 of 7 done" lights up for Codex too.
                        if let Some(plan) = args.get("plan").and_then(|p| p.as_array()) {
                            d.todos = plan
                                .iter()
                                .filter_map(|step| {
                                    let text = step.get("step").and_then(|s| s.as_str())?;
                                    Some(Todo {
                                        text: text.to_string(),
                                        status: step
                                            .get("status")
                                            .and_then(|s| s.as_str())
                                            .unwrap_or("pending")
                                            .to_string(),
                                    })
                                })
                                .collect();
                            d.turn_complete = false; // a plan update = still working
                        }
                    }
                    "exec_command" | "shell_command" => {
                        let cmd = args
                            .get("cmd")
                            .or_else(|| args.get("command"))
                            .and_then(|c| c.as_str())
                            .unwrap_or("");
                        if !cmd.is_empty() {
                            let prefix: String = cmd.chars().take(48).collect();
                            d.recent_actions.push(format!("Run {prefix}"));
                        }
                        // apply_patch often rides inside a shell heredoc — the
                        // patch body carries "*** Update File: path" markers.
                        for path in patched_file_paths(cmd) {
                            d.note_changed_file(&path);
                        }
                    }
                    "apply_patch" => {
                        let patch = args
                            .get("input")
                            .or_else(|| args.get("patch"))
                            .and_then(|c| c.as_str())
                            .unwrap_or("");
                        for path in patched_file_paths(patch) {
                            d.note_changed_file(&path);
                        }
                        d.recent_actions.push("apply_patch".into());
                    }
                    _ => {
                        let target = args
                            .get("path")
                            .or_else(|| args.get("file_path"))
                            .and_then(|p| p.as_str())
                            .map(last_path_component)
                            .unwrap_or_default();
                        d.recent_actions.push(if target.is_empty() {
                            name.to_string()
                        } else {
                            format!("{name} {target}")
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

/// Codex serializes a function call's `arguments` as a JSON string; decode it.
/// Tolerates an already-decoded object defensively.
fn decode_args(raw: Option<&Value>) -> Value {
    match raw {
        Some(Value::Object(_)) => raw.unwrap().clone(),
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

/// Claude Code tools whose `file_path` input means the file was written.
const MUTATING_CLAUDE_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// Extract target paths from an apply_patch body: "*** Update File: path" /
/// "*** Add File: path" — delete markers are skipped (a deletion isn't a file
/// the user can go look at).
fn patched_file_paths(text: &str) -> Vec<String> {
    if !text.contains("*** ") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for line in text.lines() {
        for marker in ["*** Update File: ", "*** Add File: "] {
            if let Some(rest) = line.strip_prefix(marker) {
                let path = rest.trim();
                if !path.is_empty() {
                    out.push(path.to_string());
                }
            }
        }
    }
    out
}

fn last_path_component(p: &str) -> String {
    p.rsplit('/').next().unwrap_or(p).to_string()
}

/// "Edit NotchPerlaView.swift" / "Bash xcodebuild …" — tool name + a target.
fn describe(name: &str, input: &Value) -> String {
    let target = if let Some(p) = input.get("file_path").and_then(|p| p.as_str()) {
        last_path_component(p)
    } else if let Some(p) = input.get("path").and_then(|p| p.as_str()) {
        last_path_component(p)
    } else if let Some(cmd) = input.get("command").and_then(|c| c.as_str()) {
        cmd.chars().take(48).collect()
    } else if let Some(pat) = input.get("pattern").and_then(|p| p.as_str()) {
        pat.to_string()
    } else {
        String::new()
    };
    if target.is_empty() {
        name.to_string()
    } else {
        format!("{name} {target}")
    }
}
