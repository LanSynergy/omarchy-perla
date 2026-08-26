//! Herdr-facing tools: spawn visible agents and commands in tabs, inspect
//! the whole board, steer or stop any agent on it — including ones the user
//! started by hand.

use std::sync::Arc;

use async_trait::async_trait;
use rand::Rng;
use serde_json::{json, Value};

use perla_agents::dispatcher::SharedAgentState;
use perla_agents::types::AgentTool;
use perla_tools::{ToolCallContext, ToolDispatcher, ToolResult};

use crate::board::{TrackedCommand, TrackedCommands, EXIT_MARKER};
use crate::client::HerdrClient;

pub struct HerdrDispatcher {
    pub client: HerdrClient,
    pub state: Arc<SharedAgentState>,
    /// `run_command` panes the board watcher polls for process exit.
    pub tracked: TrackedCommands,
}

/// Tool names this dispatcher owns (the engine's combiner routes on this).
pub const HERDR_TOOLS: [&str; 6] = [
    "start_agent",
    "run_command",
    "check_board",
    "steer_agent",
    "stop_agent",
    "read_pane",
];

#[async_trait]
impl ToolDispatcher for HerdrDispatcher {
    async fn dispatch(&self, name: &str, args: Value, _ctx: ToolCallContext) -> ToolResult {
        match name {
            "start_agent" => self.start_agent(&args).await,
            "run_command" => self.run_command(&args).await,
            "check_board" => self.check_board().await,
            "steer_agent" => self.steer_agent(&args).await,
            "stop_agent" => self.stop_agent(&args).await,
            "read_pane" => self.read_pane(&args).await,
            other => ToolResult::error(format!("unknown tool '{other}'")),
        }
    }
}

impl HerdrDispatcher {
    /// Open a new tab in Perla's herdr workspace, start the agent in it, and
    /// (optionally) hand it a first task. Visible to the user immediately.
    async fn start_agent(&self, args: &Value) -> ToolResult {
        let kind = args
            .get("kind")
            .and_then(|k| k.as_str())
            .unwrap_or("claude")
            .to_lowercase();
        let task = args.get("task").and_then(|t| t.as_str()).unwrap_or("");
        let requested = args.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let name = match self.unique_name(requested, &kind).await {
            Ok(n) => n,
            Err(e) => return ToolResult::error(format!("{e:#}")),
        };

        let Some(workspace) = self.spawn_workspace().await else {
            return ToolResult::error("couldn't determine a herdr workspace to spawn in");
        };
        let cwd = self.state.workspace();

        let (tab_id, pane_id) = match self.client.tab_create(&workspace, &cwd, &name).await {
            Ok(v) => v,
            Err(e) => return ToolResult::error(format!("couldn't open a tab: {e:#}")),
        };
        if let Err(e) = self
            .client
            .wait_for_prompt(&pane_id, std::time::Duration::from_secs(10))
            .await
        {
            let _ = self.client.tab_close(&tab_id).await;
            return ToolResult::error(format!("{e:#}"));
        }
        if let Err(e) = self.client.agent_start(&name, &kind, &pane_id).await {
            let _ = self.client.tab_close(&tab_id).await;
            return ToolResult::error(format!("couldn't start {kind}: {e:#}"));
        }
        if !task.is_empty() {
            if let Err(e) = self.client.agent_prompt(&name, task).await {
                return ToolResult::failure(json!({
                    "error": format!("{kind} started as '{name}' but the task didn't send: {e:#}"),
                    "agent": name,
                }));
            }
        }
        ToolResult::success(json!({
            "status": "started",
            "agent": name,
            "kind": kind,
            "note": format!(
                "{kind} is running in a visible tab as '{name}'{}. This returned immediately — you'll get a system note when its state changes (finished or needs input). Use steer_agent/stop_agent/read_pane with the name '{name}'.",
                if task.is_empty() { " (no task yet — steer_agent to give it one)" } else { "" }
            ),
        }))
    }

    /// Run a plain shell command in a fresh visible tab (dev servers, tests,
    /// npm, anything). Not tracked by agent states — read_pane to inspect.
    async fn run_command(&self, args: &Value) -> ToolResult {
        let command = args
            .get("command")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .trim();
        if command.is_empty() {
            return ToolResult::error("empty command");
        }
        let label = args
            .get("label")
            .and_then(|l| l.as_str())
            .filter(|l| !l.trim().is_empty())
            .map(sanitize_name)
            .unwrap_or_else(|| {
                sanitize_name(command.split_whitespace().next().unwrap_or("cmd"))
            });

        let Some(workspace) = self.spawn_workspace().await else {
            return ToolResult::error("couldn't determine a herdr workspace to spawn in");
        };
        let cwd = self.state.workspace();
        let (tab_id, pane_id) = match self.client.tab_create(&workspace, &cwd, &label).await {
            Ok(v) => v,
            Err(e) => return ToolResult::error(format!("couldn't open a tab: {e:#}")),
        };
        if let Err(e) = self
            .client
            .wait_for_prompt(&pane_id, std::time::Duration::from_secs(10))
            .await
        {
            let _ = self.client.tab_close(&tab_id).await;
            return ToolResult::error(format!("{e:#}"));
        }
        // Append an exit marker so the board watcher can detect when (and
        // how) the process ends — crashes get narrated without being asked.
        let wrapped = format!("{command}; printf '{EXIT_MARKER}%s\\n' $?");
        if let Err(e) = self.client.pane_run(&pane_id, &wrapped).await {
            return ToolResult::error(format!("couldn't run the command: {e:#}"));
        }
        self.tracked.lock().unwrap().push(TrackedCommand {
            pane_id: pane_id.clone(),
            label: label.clone(),
            command: command.to_string(),
        });
        ToolResult::success(json!({
            "status": "running",
            "pane": pane_id,
            "note": format!(
                "Running in a visible tab labeled '{label}'. You'll get a system note when it exits (with its exit code) — including if it crashes. For live output before then, use read_pane with pane '{pane_id}'."
            ),
        }))
    }

    /// The whole board: every workspace and every agent with its live state,
    /// plus rich to-do digests for claude/codex working in Perla's focused
    /// project (joined from their transcripts).
    async fn check_board(&self) -> ToolResult {
        let (workspaces, agents) = match (
            self.client.workspaces().await,
            self.client.agents().await,
        ) {
            (Ok(w), Ok(a)) => (w, a),
            (Err(e), _) | (_, Err(e)) => return ToolResult::error(format!("{e:#}")),
        };
        let ws_label = |id: &str| -> String {
            workspaces
                .iter()
                .find(|w| w.workspace_id == id)
                .map(|w| w.label.clone())
                .unwrap_or_else(|| id.to_string())
        };
        let focused_cwd = perla_agents::transcripts::normalize_cwd(&self.state.workspace());

        let mut entries = Vec::new();
        for agent in &agents {
            let mut entry = json!({
                "agent": agent.target(),
                "kind": agent.agent,
                "status": agent.agent_status,
                "workspace": ws_label(&agent.workspace_id),
                "project": agent.cwd.rsplit('/').next().unwrap_or(&agent.cwd),
            });
            if let Some(title) = &agent.terminal_title_stripped {
                entry["task"] = json!(title);
            }
            // Rich digest join — only for claude/codex in the focused
            // project, to keep this call fast.
            if perla_agents::transcripts::normalize_cwd(&agent.cwd) == focused_cwd {
                let tool = match agent.agent.as_str() {
                    "claude" => Some(AgentTool::Claude),
                    "codex" => Some(AgentTool::Codex),
                    _ => None,
                };
                if let Some(tool) = tool {
                    let cwd = agent.cwd.clone();
                    let digest = tokio::task::spawn_blocking(move || {
                        perla_agents::digest::digest(tool, &cwd)
                    })
                    .await
                    .ok()
                    .flatten();
                    if let Some(d) = digest {
                        if !d.todos.is_empty() {
                            let done =
                                d.todos.iter().filter(|t| t.status == "completed").count();
                            entry["todo_summary"] =
                                json!(format!("{done} of {} done", d.todos.len()));
                        }
                        if let Some(m) = &d.last_message {
                            entry["last_message"] =
                                json!(m.chars().take(200).collect::<String>());
                        }
                    }
                }
            }
            entries.push(entry);
        }

        let ws: Vec<Value> = workspaces
            .iter()
            .map(|w| {
                json!({
                    "label": w.label,
                    "tabs": w.tab_count,
                    "panes": w.pane_count,
                    "focused": w.focused,
                })
            })
            .collect();
        ToolResult::success(json!({
            "agents": entries,
            "workspaces": ws,
            "note": if entries.is_empty() {
                "No agents are on the board right now."
            } else {
                "Live board state. 'blocked' means the agent is waiting on input — offer to relay."
            },
        }))
    }

    /// Send an instruction to ANY agent on the board by name or pane id.
    async fn steer_agent(&self, args: &Value) -> ToolResult {
        let target = args.get("agent").and_then(|a| a.as_str()).unwrap_or("");
        let message = args
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .trim();
        if target.is_empty() || message.is_empty() {
            return ToolResult::error("need both agent and message");
        }
        match self.client.agent_prompt(target, message).await {
            Ok(()) => ToolResult::success(json!({
                "sent": true,
                "note": format!("Delivered to {target}."),
            })),
            Err(e) => ToolResult::error(format!("{e:#}")),
        }
    }

    /// Escape into the agent's UI — halts its current work, keeps it open.
    async fn stop_agent(&self, args: &Value) -> ToolResult {
        let target = args.get("agent").and_then(|a| a.as_str()).unwrap_or("");
        if target.is_empty() {
            return ToolResult::error("need the agent name");
        }
        match self.client.agent_send_keys(target, "esc").await {
            Ok(()) => ToolResult::success(json!({
                "stopped": true,
                "note": format!("Interrupted {target}; its pane stays open for a new direction."),
            })),
            Err(e) => ToolResult::error(format!("{e:#}")),
        }
    }

    /// Recent output of any pane/agent — for "what is it saying?".
    async fn read_pane(&self, args: &Value) -> ToolResult {
        let target = args
            .get("target")
            .or_else(|| args.get("agent"))
            .and_then(|a| a.as_str())
            .unwrap_or("");
        if target.is_empty() {
            return ToolResult::error("need the agent name or pane id");
        }
        let lines = args.get("lines").and_then(|l| l.as_u64()).unwrap_or(60) as u32;
        match self.client.read_target(target, lines.min(200)).await {
            Ok(text) => {
                let tail: String = text
                    .lines()
                    .rev()
                    .take(lines as usize)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                // Keep the END of the output (that's where the news is).
                let total = tail.chars().count();
                let clipped: String = tail.chars().skip(total.saturating_sub(4000)).collect();
                ToolResult::success(json!({ "target": target, "output": clipped }))
            }
            Err(e) => ToolResult::error(format!("{e:#}")),
        }
    }

    // ── helpers ─────────────────────────────────────────────────────────

    /// The herdr workspace new tabs go to: the one Perla's pane lives in,
    /// else the focused one.
    async fn spawn_workspace(&self) -> Option<String> {
        if let Some(ws) = crate::own_workspace_id() {
            return Some(ws);
        }
        self.client
            .workspaces()
            .await
            .ok()?
            .into_iter()
            .find(|w| w.focused)
            .map(|w| w.workspace_id)
    }

    /// Herdr agent names must be `[a-z][a-z0-9_-]{0,31}` and unique among
    /// live agents.
    async fn unique_name(&self, requested: &str, kind: &str) -> anyhow::Result<String> {
        let base = if requested.trim().is_empty() {
            kind.to_string()
        } else {
            sanitize_name(requested)
        };
        let taken: Vec<String> = self
            .client
            .agents()
            .await?
            .into_iter()
            .filter_map(|a| a.name)
            .collect();
        if !taken.contains(&base) {
            return Ok(base);
        }
        for _ in 0..8 {
            let suffix: u32 = rand::rng().random_range(2..100);
            let candidate = format!("{base}-{suffix}");
            if !taken.contains(&candidate) {
                return Ok(candidate);
            }
        }
        Ok(format!("{base}-{}", rand::rng().random_range(100..10000)))
    }
}

/// Force a string into herdr's name grammar: lowercase, `[a-z0-9_-]`,
/// starts with a letter, max 32 chars.
fn sanitize_name(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.to_lowercase().chars() {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
        if ok {
            out.push(c);
        } else if c.is_whitespace() && !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 32 {
            break;
        }
    }
    while out.starts_with(|c: char| !c.is_ascii_lowercase()) && !out.is_empty() {
        out.remove(0);
    }
    if out.is_empty() {
        out = "agent".into();
    }
    out.trim_end_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::sanitize_name;

    #[test]
    fn sanitize_produces_valid_names() {
        assert_eq!(sanitize_name("My Reviewer!"), "my-reviewer");
        assert_eq!(sanitize_name("123abc"), "abc");
        assert_eq!(sanitize_name("???"), "agent");
        assert!(sanitize_name(&"x".repeat(64)).len() <= 32);
    }
}
