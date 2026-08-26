//! `HandsDispatcher` — routes the realtime model's function calls into the
//! hands pool. Same contract as `perla_agents::AgentDispatcher` (fast-ack
//! statuses, workspace pinning, fast tools) with the hands-mode tool names.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use perla_agents::dispatcher::SharedAgentState;
use perla_agents::orchestrator::AgentRunContext;
use perla_agents::transcripts::normalize_cwd;
use perla_tools::{fast, ToolCallContext, ToolDispatcher, ToolResult};

use crate::{HandsPool, HandsSubmit};

pub struct HandsDispatcher {
    pub pool: Arc<HandsPool>,
    pub state: Arc<SharedAgentState>,
}

#[async_trait]
impl ToolDispatcher for HandsDispatcher {
    async fn dispatch(&self, name: &str, args: Value, ctx: ToolCallContext) -> ToolResult {
        match name {
            "run_task" => self.run_task(&args, &ctx).await,
            "check_task" => self.check_task(),
            "stop_task" => self.stop_task(),
            "steer_task" => self.steer_task(&args).await,
            "set_progress_updates" => self.set_progress_updates(&args),
            "switch_workspace" => self.switch_workspace(&args),
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

impl HandsDispatcher {
    /// Fast-ack: returns the instant the task is handed to the hands, not
    /// when it finishes — completion is narrated out-of-band.
    async fn run_task(&self, args: &Value, ctx: &ToolCallContext) -> ToolResult {
        let task = args.get("task").and_then(|t| t.as_str()).unwrap_or("");
        if task.trim().is_empty() {
            return ToolResult::error("empty task");
        }
        // Hard-pin to the user-picked workspace (see AgentDispatcher for the
        // history of why args["cwd"] is deliberately ignored).
        let cwd = self.state.workspace();
        let context = AgentRunContext::new(ctx.history_id.clone());

        match self.pool.submit(&cwd, task, context, false).await {
            HandsSubmit::Submitted => ToolResult::success(json!({
                "status": "submitted",
                "note": "On it — your hands are working now. This returned immediately; you'll get a system note the moment the work finishes, so relay results then. NEVER re-send the same task; ask check_task, steer with steer_task, or halt with stop_task.",
            })),
            HandsSubmit::AlreadyRunning { running_task } => ToolResult::success(json!({
                "status": "already_running",
                "note": "That exact task is still in progress — I did NOT start a duplicate. Use steer_task to add to it or stop_task to halt it.",
                "running_task": truncate(&running_task, 200),
            })),
            HandsSubmit::Queued { behind_task } => ToolResult::success(json!({
                "status": "queued",
                "note": "The hands are mid-task — this one is queued and starts automatically the moment the current one ends. Do NOT resend it.",
                "current_task": truncate(&behind_task, 200),
            })),
            HandsSubmit::Unavailable(msg) => ToolResult::error(msg),
        }
    }

    /// Read-only status snapshot, protocol-fed (no transcript parsing).
    fn check_task(&self) -> ToolResult {
        let cwd = self.state.workspace();

        let others: Vec<Value> = self
            .pool
            .live_sessions()
            .into_iter()
            .filter(|(c, _)| c != &normalize_cwd(&cwd))
            .map(|(c, working)| {
                json!({
                    "project": c.rsplit('/').next().unwrap_or(&c),
                    "status": if working { "working" } else { "idle" },
                })
            })
            .collect();

        let Some((running, digest, queued)) = self.pool.snapshot(&cwd) else {
            let mut payload = json!({
                "is_running": false,
                "note": "Nothing has run in this workspace yet this session.",
            });
            if !others.is_empty() {
                payload["other_sessions"] = Value::Array(others);
            }
            return ToolResult::success(payload);
        };

        let mut payload = json!({ "is_running": running });
        if queued > 0 {
            payload["queued_tasks"] = json!(queued);
        }
        if running {
            if let Some(last) = digest.recent_actions.last() {
                payload["current_activity"] = json!(last);
            }
        }
        if let Some(last) = &digest.last_message {
            payload["last_message"] = json!(last);
        }
        if !digest.todos.is_empty() {
            payload["todos"] = Value::Array(
                digest
                    .todos
                    .iter()
                    .map(|t| json!({ "text": t.text, "status": t.status }))
                    .collect(),
            );
            let done = digest
                .todos
                .iter()
                .filter(|t| t.status == "completed")
                .count();
            let in_progress = digest
                .todos
                .iter()
                .filter(|t| t.status == "in_progress")
                .count();
            let left = digest.todos.len() - done - in_progress;
            payload["todo_summary"] = json!(format!(
                "{done} done, {in_progress} in progress, {left} left (of {})",
                digest.todos.len()
            ));
        }
        if !digest.recent_actions.is_empty() {
            payload["recent_actions"] = json!(digest.recent_actions);
        }
        if !digest.changed_files.is_empty() {
            payload["changed_files"] = json!(digest
                .changed_files
                .iter()
                .map(|p| p.rsplit('/').next().unwrap_or(p))
                .collect::<Vec<_>>());
            payload["changed_files_count"] = json!(digest.changed_files.len());
        }
        if !others.is_empty() {
            payload["other_sessions"] = Value::Array(others);
        }
        ToolResult::success(payload)
    }

    fn stop_task(&self) -> ToolResult {
        let cwd = self.state.workspace();
        let stopped = self.pool.cancel(&cwd);
        let note = if stopped {
            "Stopped the current work; the session stays open for a new direction."
        } else {
            "Nothing is currently running in this workspace."
        };
        ToolResult::success(json!({ "stopped": stopped, "note": note }))
    }

    /// Steering rides the prompt queue as a QUIET turn: the agent folds the
    /// correction in right after its current step, and its completion is
    /// not separately announced (it's a course change, not a new result).
    async fn steer_task(&self, args: &Value) -> ToolResult {
        let message = args
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if message.is_empty() {
            return ToolResult::error("empty message");
        }
        let cwd = self.state.workspace();
        if !self.pool.is_running(&cwd) {
            return ToolResult::failure(json!({
                "sent": false,
                "note": "Nothing is running here — start the work with run_task instead.",
            }));
        }
        let prompt = format!(
            "[Steering from the user, mid-task] {message}\nFold this into the work you just did or are doing — do not restart from scratch."
        );
        let context = AgentRunContext::new(None);
        match self.pool.submit(&cwd, &prompt, context, true).await {
            HandsSubmit::Queued { .. } | HandsSubmit::Submitted => ToolResult::success(json!({
                "sent": true,
                "note": "Passed along — the hands will fold it in right after the current step.",
            })),
            HandsSubmit::AlreadyRunning { .. } => ToolResult::success(json!({
                "sent": true,
                "note": "That same instruction is already pending.",
            })),
            HandsSubmit::Unavailable(msg) => ToolResult::error(msg),
        }
    }

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
        ToolResult::success(json!({ "mode": mode, "note": note }))
    }

    /// Same lenient resolution as the agents dispatcher: real path, exact
    /// folder-name match against recents, then substring.
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
        let expanded = normalize_cwd(&query);
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
            "note": format!("Switched — tasks now run in {leaf}."),
        }))
    }
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}
