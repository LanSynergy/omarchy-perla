//! The board watcher — polls herdr's agent list and turns state transitions
//! into events the voice engine narrates. This is how Perla knows about
//! EVERYTHING on the board, including agents the user started by hand.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::debug;

use crate::client::{HerdrAgent, HerdrClient};

/// A command pane spawned by `run_command`, watched until its process exits.
#[derive(Debug, Clone)]
pub struct TrackedCommand {
    pub pane_id: String,
    pub label: String,
    pub command: String,
}

/// Shared between the dispatcher (which registers panes) and the watcher
/// (which polls them for the exit marker).
pub type TrackedCommands = Arc<Mutex<Vec<TrackedCommand>>>;

/// Printed by the wrapped command when it exits; carries the exit code.
pub const EXIT_MARKER: &str = "__perla_exit=";

/// A state transition on the board worth telling the engine about.
#[derive(Debug, Clone)]
pub enum HerdrEvent {
    /// An agent's herdr status changed (working / idle / blocked / done /
    /// unknown). `title` is herdr's task headline for the pane.
    AgentStatus {
        /// Target handle: unique name if set, else pane id.
        target: String,
        /// Agent kind: "claude" | "codex" | "grok" | …
        kind: String,
        /// Herdr workspace label the pane lives in ("Clase", "offsec"…).
        workspace: String,
        cwd: String,
        from: String,
        to: String,
        title: Option<String>,
    },
    /// An agent appeared on the board (spawned by Perla OR by the user).
    AgentAppeared {
        target: String,
        kind: String,
        workspace: String,
    },
    /// An agent's pane is gone.
    AgentGone {
        target: String,
        kind: String,
        workspace: String,
    },
    /// A `run_command` pane's process exited (0 = success; anything else is
    /// a failure or crash worth telling the user about).
    CommandFinished {
        pane_id: String,
        label: String,
        command: String,
        exit_code: i32,
        /// Last output lines, marker stripped — context for narration.
        tail: String,
    },
}

pub struct BoardWatcher;

impl BoardWatcher {
    /// Start polling. The first poll is a silent baseline — existing agents
    /// don't produce a flood of "appeared" events at startup. `tracked` is
    /// the registry of `run_command` panes to watch for process exit.
    pub fn start(
        client: HerdrClient,
        tracked: TrackedCommands,
    ) -> mpsc::UnboundedReceiver<HerdrEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(Self::run(Arc::new(client), tracked, tx));
        rx
    }

    async fn run(
        client: Arc<HerdrClient>,
        tracked: TrackedCommands,
        tx: mpsc::UnboundedSender<HerdrEvent>,
    ) {
        let mut known: Option<HashMap<String, HerdrAgent>> = None;
        loop {
            tokio::time::sleep(Duration::from_millis(2000)).await;
            if tx.is_closed() {
                return;
            }
            Self::poll_commands(&client, &tracked, &tx).await;
            let (agents, workspaces) = match (client.agents().await, client.workspaces().await) {
                (Ok(a), Ok(w)) => (a, w),
                (Err(e), _) | (_, Err(e)) => {
                    debug!("board poll failed: {e:#}");
                    continue;
                }
            };
            let ws_label = |id: &str| -> String {
                workspaces
                    .iter()
                    .find(|w| w.workspace_id == id)
                    .map(|w| w.label.clone())
                    .unwrap_or_else(|| id.to_string())
            };
            let current: HashMap<String, HerdrAgent> = agents
                .into_iter()
                .map(|a| (a.pane_id.clone(), a))
                .collect();

            let Some(previous) = known.replace(current) else {
                continue; // baseline established silently
            };
            let now = known.as_ref().unwrap();

            for (pane_id, agent) in now {
                match previous.get(pane_id) {
                    None => {
                        let _ = tx.send(HerdrEvent::AgentAppeared {
                            target: agent.target().to_string(),
                            kind: agent.agent.clone(),
                            workspace: ws_label(&agent.workspace_id),
                        });
                    }
                    Some(prev) if prev.agent_status != agent.agent_status => {
                        let _ = tx.send(HerdrEvent::AgentStatus {
                            target: agent.target().to_string(),
                            kind: agent.agent.clone(),
                            workspace: ws_label(&agent.workspace_id),
                            cwd: agent.cwd.clone(),
                            from: prev.agent_status.clone(),
                            to: agent.agent_status.clone(),
                            title: agent.terminal_title_stripped.clone(),
                        });
                    }
                    _ => {}
                }
            }
            for (pane_id, prev) in &previous {
                if !now.contains_key(pane_id) {
                    let _ = tx.send(HerdrEvent::AgentGone {
                        target: prev.target().to_string(),
                        kind: prev.agent.clone(),
                        workspace: ws_label(&prev.workspace_id),
                    });
                }
            }
        }
    }

    /// Check tracked `run_command` panes for the exit marker. A pane that
    /// can't be read anymore was closed by the user — dropped silently.
    async fn poll_commands(
        client: &HerdrClient,
        tracked: &TrackedCommands,
        tx: &mpsc::UnboundedSender<HerdrEvent>,
    ) {
        let snapshot: Vec<TrackedCommand> = tracked.lock().unwrap().clone();
        for cmd in snapshot {
            let text = match client.read_target(&cmd.pane_id, 25).await {
                Ok(t) => t,
                Err(_) => {
                    tracked.lock().unwrap().retain(|c| c.pane_id != cmd.pane_id);
                    continue;
                }
            };
            if text.trim().is_empty() {
                // Pane gone (herdr returns empty for closed panes on some
                // paths) — check it still exists before assuming "no output".
                if client.call(&["pane", "get", &cmd.pane_id]).await.is_err() {
                    tracked.lock().unwrap().retain(|c| c.pane_id != cmd.pane_id);
                }
                continue;
            }
            // The command line itself also contains the marker text (it was
            // typed into the shell) — only lines that START with the marker
            // count, which the echoed output does and the prompt line doesn't.
            let exit_code = text.lines().rev().find_map(|line| {
                line.trim()
                    .strip_prefix(EXIT_MARKER)
                    .and_then(|v| v.trim().parse::<i32>().ok())
            });
            let Some(exit_code) = exit_code else { continue };
            tracked.lock().unwrap().retain(|c| c.pane_id != cmd.pane_id);
            let tail: String = text
                .lines()
                .filter(|l| !l.contains(EXIT_MARKER))
                .rev()
                .take(12)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            let _ = tx.send(HerdrEvent::CommandFinished {
                pane_id: cmd.pane_id,
                label: cmd.label,
                command: cmd.command,
                exit_code,
                tail: tail.chars().take(1500).collect(),
            });
        }
    }
}
