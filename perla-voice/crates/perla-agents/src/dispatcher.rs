//! The built-in tool dispatcher — port of `ToolDispatcher.swift`. Routes the
//! realtime model's function calls to the orchestrator (slow agent tools,
//! fast-ack pattern) and the filesystem helpers (fast tools).
//!
//! The engine handles two names itself before delegating here: `get_usage`
//! (cost lives in the session) and post-processing for `switch_workspace` /
//! `check_agent_session` (instruction refresh, held-update clearing).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use perla_tools::{fast, ToolCallContext, ToolDispatcher, ToolResult};

use crate::orchestrator::{AgentOrchestrator, AgentRunContext, SubmitOutcome};
use crate::transcripts;
use crate::types::AgentTool;

/// Live host state the dispatcher (and engine) read and mutate: the focused
/// workspace, the default runtime, and narration verbosity.
pub struct SharedAgentState {
    pub workspace: Mutex<String>,
    pub recent_workspaces: Mutex<Vec<String>>,
    pub runtime: Mutex<AgentTool>,
    pub detail_mode: AtomicBool,
    pub big_moments_only: AtomicBool,
}

impl SharedAgentState {
    pub fn new(workspace: String, recents: Vec<String>, runtime: AgentTool) -> Arc<Self> {
        Arc::new(Self {
            workspace: Mutex::new(workspace),
            recent_workspaces: Mutex::new(recents),
            runtime: Mutex::new(runtime),
            detail_mode: AtomicBool::new(true),
            big_moments_only: AtomicBool::new(false),
        })
    }

    pub fn workspace(&self) -> String {
        self.workspace.lock().unwrap().clone()
    }

    pub fn runtime(&self) -> AgentTool {
        *self.runtime.lock().unwrap()
    }
}

pub struct AgentDispatcher {
    pub orchestrator: Arc<AgentOrchestrator>,
    pub state: Arc<SharedAgentState>,
}

#[async_trait]
impl ToolDispatcher for AgentDispatcher {
    async fn dispatch(&self, name: &str, args: Value, ctx: ToolCallContext) -> ToolResult {
        match name {
            "run_claude_agent" => self.run_agent(AgentTool::Claude, &args, &ctx).await,
            "run_codex" => self.run_agent(AgentTool::Codex, &args, &ctx).await,
            "check_agent_session" => self.check_agent_session().await,
            "stop_agent" => self.stop_agent(),
            "steer_agent" => self.steer_agent(&args),
            "set_progress_updates" => self.set_progress_updates(&args),
            "switch_workspace" => self.switch_workspace(&args),
            "review_with_other_agent" => self.review_with_other_agent(&args, &ctx).await,
            "read_file" => match args.get("path").and_then(|p| p.as_str()) {
                Some(path) => fast::read_file(path).await,
                None => ToolResult::error("missing path"),
            },
            "list_dir" => match args.get("path").and_then(|p| p.as_str()) {
                Some(path) => fast::list_dir(path).await,
                None => ToolResult::error("missing path"),
            },
            "open_in_editor" => match args.get("path").and_then(|p| p.as_str()) {
                Some(path) => fast::open_in_editor(path).await,
                None => ToolResult::error("missing path"),
            },
            other => ToolResult::error(format!("unknown tool '{other}'")),
        }
    }
}

impl AgentDispatcher {
    /// Fast-ack: `submit` returns the instant the prompt is handed to the
    /// agent, NOT when the turn ends — the fix for "long turn → model thinks
    /// the prompt was lost → re-sends it". The turn's actual result is
    /// narrated later via `AgentEvent::TurnFinished`.
    async fn run_agent(&self, tool: AgentTool, args: &Value, ctx: &ToolCallContext) -> ToolResult {
        let task = args.get("task").and_then(|t| t.as_str()).unwrap_or("");
        // Hard-pin to the user-picked workspace; deliberately ignore
        // args["cwd"] even when the model passes one — honoring it let the
        // model spawn fresh sessions whenever it wanted to "work in a
        // subfolder", breaking the one-session-per-workspace contract. If it
        // wants a subfolder it can `cd` inside the running TUI.
        let cwd = self.state.workspace();
        let context = AgentRunContext::new(ctx.history_id.clone());

        match self.orchestrator.submit(tool, task, &cwd, context).await {
            SubmitOutcome::Submitted => ToolResult::success(json!({
                "status": "submitted",
                "note": format!(
                    "Started {}. It's running now — I'll tell you the moment it finishes. Don't call this again for the same task; ask me how it's going, steer it, or say stop.",
                    tool.label()
                ),
            })),
            SubmitOutcome::AlreadyRunning { running_task } => ToolResult::success(json!({
                "status": "already_running",
                "note": format!(
                    "{} is still working on the previous task — I did NOT start a duplicate. To add to it use steer_agent; to halt it use stop_agent.",
                    tool.label()
                ),
                "running_task": truncate(&running_task, 200),
            })),
            SubmitOutcome::Queued { behind_task } => ToolResult::success(json!({
                "status": "queued",
                "note": format!(
                    "{} is still finishing the previous task — this one is queued and will start automatically the moment it's done. Do NOT resend it.",
                    tool.label()
                ),
                "current_task": truncate(&behind_task, 200),
            })),
            SubmitOutcome::Unavailable(msg) => ToolResult::error(msg),
        }
    }

    /// Read-only snapshot for "what's it doing / are we done / how many left".
    async fn check_agent_session(&self) -> ToolResult {
        let tool = self.state.runtime();
        let cwd = self.state.workspace();
        let running = self.orchestrator.is_running(tool, &cwd);

        // Cross-project view — every OTHER live session, so the model can
        // answer "what's happening across my projects".
        let focused = transcripts::normalize_cwd(&cwd);
        let others: Vec<Value> = self
            .orchestrator
            .live_sessions()
            .into_iter()
            .filter(|(_, c, _)| c != &focused)
            .map(|(t, c, working)| {
                json!({
                    "project": c.rsplit('/').next().unwrap_or(&c),
                    "tool": t.label(),
                    "status": if working { "working" } else { "idle" },
                })
            })
            .collect();

        let digest = {
            let cwd = cwd.clone();
            tokio::task::spawn_blocking(move || crate::digest::digest(tool, &cwd))
                .await
                .ok()
                .flatten()
        };
        let Some(d) = digest else {
            let mut payload = json!({
                "tool": tool.label(),
                "is_running": running,
                "note": "No agent transcript for this workspace yet — nothing has run here, or it just started.",
            });
            if !others.is_empty() {
                payload["other_sessions"] = Value::Array(others);
            }
            return ToolResult::success(payload);
        };

        let is_running = running || !d.turn_complete;
        let mut payload = json!({
            "tool": tool.label(),
            "is_running": is_running,
            "turn_complete": d.turn_complete,
            "session_id": d.session_id,
        });
        if is_running {
            if let Some(last) = d.recent_actions.last() {
                payload["current_activity"] = json!(last);
            }
        }
        if let Some(last) = &d.last_message {
            payload["last_message"] = json!(last);
        }
        if !d.todos.is_empty() {
            payload["todos"] = Value::Array(
                d.todos
                    .iter()
                    .map(|t| json!({ "text": t.text, "status": t.status }))
                    .collect(),
            );
            let done = d.todos.iter().filter(|t| t.status == "completed").count();
            let in_progress = d.todos.iter().filter(|t| t.status == "in_progress").count();
            let left = d.todos.len() - done - in_progress;
            payload["todo_summary"] = json!(format!(
                "{done} done, {in_progress} in progress, {left} left (of {})",
                d.todos.len()
            ));
        }
        if !d.recent_actions.is_empty() {
            payload["recent_actions"] = json!(d.recent_actions);
        }
        if !d.changed_files.is_empty() {
            payload["changed_files"] = json!(d
                .changed_files
                .iter()
                .map(|p| p.rsplit('/').next().unwrap_or(p))
                .collect::<Vec<_>>());
            payload["changed_files_count"] = json!(d.changed_files.len());
        }
        if !others.is_empty() {
            payload["other_sessions"] = Value::Array(others);
        }
        ToolResult::success(payload)
    }

    /// Halt the agent's current turn (Esc into the live PTY) but keep the
    /// session open.
    fn stop_agent(&self) -> ToolResult {
        let tool = self.state.runtime();
        let cwd = self.state.workspace();
        let stopped = self.orchestrator.interrupt(tool, &cwd);
        let note = if stopped {
            "Interrupted the agent; the session is still open for a new direction."
        } else {
            "No agent is currently running in this workspace."
        };
        ToolResult::success(json!({ "stopped": stopped, "note": note }))
    }

    fn steer_agent(&self, args: &Value) -> ToolResult {
        let message = args
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if message.is_empty() {
            return ToolResult::error("empty message");
        }
        let tool = self.state.runtime();
        let cwd = self.state.workspace();
        let sent = self.orchestrator.steer(tool, &cwd, &message);
        let note = if sent {
            "Sent to the running agent."
        } else {
            "Nothing is running here — start a task with run_claude_agent first."
        };
        ToolResult {
            ok: sent,
            payload: crate::dispatcher::to_map(json!({ "sent": sent, "note": note })),
        }
    }

    /// Map the voice intent onto the two narration flags.
    fn set_progress_updates(&self, args: &Value) -> ToolResult {
        let mode = args
            .get("mode")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_lowercase();
        let (on, big_only, note) = match mode.as_str() {
            "off" => (false, false, "Okay — I'll stay quiet about progress and only update you when you ask or when it's done."),
            "big" => (true, true, "Got it — I'll only flag the big moments as it works."),
            "steps" => (true, false, "Will do — I'll keep you posted on each step as it goes."),
            other => return ToolResult::error(format!("unknown mode '{other}' — use off, steps, or big")),
        };
        self.state.detail_mode.store(on, Ordering::Relaxed);
        self.state
            .big_moments_only
            .store(big_only, Ordering::Relaxed);
        self.orchestrator.set_detail_mode(on);
        ToolResult::success(json!({ "mode": mode, "note": note }))
    }

    /// Switch the active workspace by spoken name or path. Lenient: a real
    /// directory path wins, then an exact folder-name match against recents,
    /// then a substring match.
    fn switch_workspace(&self, args: &Value) -> ToolResult {
        let query = args
            .get("workspace")
            .and_then(|w| w.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if query.is_empty() {
            return ToolResult::error("empty workspace");
        }
        let expanded = transcripts::normalize_cwd(&query);
        let recents = self.state.recent_workspaces.lock().unwrap().clone();
        let resolved = if std::path::Path::new(&expanded).is_dir() {
            Some(expanded)
        } else {
            let q = query.to_lowercase();
            let leaf = |p: &String| p.rsplit('/').next().unwrap_or(p).to_lowercase();
            recents
                .iter()
                .find(|p| leaf(p) == q)
                .or_else(|| recents.iter().find(|p| leaf(p).contains(&q)))
                .or_else(|| recents.iter().find(|p| p.to_lowercase().contains(&q)))
                .cloned()
        };
        let Some(resolved) = resolved else {
            return ToolResult::failure(json!({
                "error": format!("No workspace matches '{query}'."),
                "recent_workspaces": recents,
            }));
        };
        *self.state.workspace.lock().unwrap() = resolved.clone();
        {
            let mut r = self.state.recent_workspaces.lock().unwrap();
            r.retain(|p| p != &resolved);
            r.insert(0, resolved.clone());
        }
        let leaf = resolved.rsplit('/').next().unwrap_or(&resolved).to_string();
        ToolResult::success(json!({
            "workspace": resolved,
            "note": format!("Switched — agent tasks now run in {leaf}."),
        }))
    }

    /// Second opinion: flip claude↔codex and submit a canned review of the
    /// working tree. Rides the normal run_agent path, so it gets the same
    /// fast-ack / queue / completion-narration behavior.
    async fn review_with_other_agent(&self, args: &Value, ctx: &ToolCallContext) -> ToolResult {
        let focus = args
            .get("focus")
            .and_then(|f| f.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let reviewer = self.state.runtime().other();
        let mut task = "Review the current uncommitted changes in this repository (git status + git diff, including untracked files). Report real problems — bugs, broken edge cases, regressions — not style nits. End with a short verdict on whether the changes are safe to keep.".to_string();
        if !focus.is_empty() {
            task += &format!(" Focus especially on: {focus}.");
        }
        self.run_agent(reviewer, &json!({ "task": task }), ctx)
            .await
    }
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

pub(crate) fn to_map(v: Value) -> serde_json::Map<String, Value> {
    match v {
        Value::Object(m) => m,
        other => {
            let mut m = serde_json::Map::new();
            m.insert("value".into(), other);
            m
        }
    }
}
