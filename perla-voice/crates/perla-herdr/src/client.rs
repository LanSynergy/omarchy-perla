//! Thin typed wrapper over the `herdr` CLI. Every command returns
//! `{"id": "...", "result": {...}}` on stdout; server errors are JSON on
//! stderr with exit status 1.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;

/// How long a single CLI call may take. `agent start` legitimately waits up
/// to ~30s for the agent to become ready; everything else is instant.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct HerdrClient {
    binary: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HerdrWorkspace {
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub agent_status: String,
    #[serde(default)]
    pub pane_count: u32,
    #[serde(default)]
    pub tab_count: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HerdrAgent {
    /// Agent kind as detected by herdr: "claude" | "codex" | "grok" | …
    pub agent: String,
    #[serde(default)]
    pub agent_status: String,
    #[serde(default)]
    pub cwd: String,
    pub pane_id: String,
    #[serde(default)]
    pub tab_id: String,
    #[serde(default)]
    pub workspace_id: String,
    /// User/Perla-assigned unique name, when one was set.
    #[serde(default)]
    pub name: Option<String>,
    /// Herdr's human title for the pane (e.g. the agent's task headline).
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    #[serde(default)]
    pub focused: bool,
    /// For claude: carries the agent's own session id — joinable to its
    /// JSONL transcript for rich digests.
    #[serde(default)]
    pub agent_session: Option<Value>,
}

impl HerdrAgent {
    /// The handle to target this agent with (name if set, else pane id).
    pub fn target(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.pane_id)
    }
}

impl HerdrClient {
    pub fn new() -> Option<Self> {
        crate::which_herdr().map(|binary| Self { binary })
    }

    /// Run `herdr <args>` and return the parsed `result` object.
    pub async fn call(&self, args: &[&str]) -> Result<Value> {
        let output = tokio::time::timeout(
            CALL_TIMEOUT,
            Command::new(&self.binary).args(args).output(),
        )
        .await
        .map_err(|_| anyhow!("herdr {} timed out", args.first().unwrap_or(&"")))?
        .with_context(|| format!("running herdr {args:?}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = serde_json::from_str::<Value>(stderr.trim())
                .ok()
                .and_then(|v| {
                    v.pointer("/error/message")
                        .and_then(|m| m.as_str())
                        .map(String::from)
                })
                .unwrap_or_else(|| stderr.trim().to_string());
            return Err(anyhow!("herdr {}: {message}", args.join(" ")));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Some commands (e.g. `pane run`) succeed with empty stdout.
        if stdout.trim().is_empty() {
            return Ok(Value::Null);
        }
        let parsed: Value = serde_json::from_str(stdout.trim())
            .with_context(|| format!("parsing herdr {args:?} output"))?;
        Ok(parsed.get("result").cloned().unwrap_or(parsed))
    }

    /// Like `call`, but for commands that print plain text on stdout
    /// (`pane read`, `agent read`) instead of the JSON envelope.
    pub async fn call_text(&self, args: &[&str]) -> Result<String> {
        let output = tokio::time::timeout(
            CALL_TIMEOUT,
            Command::new(&self.binary).args(args).output(),
        )
        .await
        .map_err(|_| anyhow!("herdr {} timed out", args.first().unwrap_or(&"")))?
        .with_context(|| format!("running herdr {args:?}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("herdr {}: {}", args.join(" "), stderr.trim()));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    // ── reads ───────────────────────────────────────────────────────────

    pub async fn workspaces(&self) -> Result<Vec<HerdrWorkspace>> {
        let result = self.call(&["workspace", "list"]).await?;
        let list = result.get("workspaces").cloned().unwrap_or(Value::Null);
        Ok(serde_json::from_value(list).unwrap_or_default())
    }

    pub async fn agents(&self) -> Result<Vec<HerdrAgent>> {
        let result = self.call(&["agent", "list"]).await?;
        let list = result.get("agents").cloned().unwrap_or(Value::Null);
        Ok(serde_json::from_value(list).unwrap_or_default())
    }

    /// Recent pane text (plain). Agent-hosting panes prefer the agent
    /// surface; falls back to the raw pane, and to the visible screen when
    /// the scrollback is still empty.
    pub async fn read_target(&self, target: &str, lines: u32) -> Result<String> {
        let lines_s = lines.to_string();
        for (noun, source) in [
            ("agent", "recent-unwrapped"),
            ("pane", "recent-unwrapped"),
            ("pane", "visible"),
        ] {
            match self
                .call_text(&[noun, "read", target, "--source", source, "--lines", &lines_s])
                .await
            {
                Ok(text) if !text.trim().is_empty() => return Ok(text),
                _ => continue,
            }
        }
        Ok(String::new())
    }

    /// Pane ids of a workspace, in listing order.
    pub async fn pane_ids(&self, workspace: &str) -> Result<Vec<String>> {
        let result = self
            .call(&["pane", "list", "--workspace", workspace])
            .await?;
        Ok(result
            .get("panes")
            .and_then(|p| p.as_array())
            .map(|panes| {
                panes
                    .iter()
                    .filter_map(|p| p.get("pane_id").and_then(|i| i.as_str()))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// True when the pane is an idle shell at its prompt (safe to `pane run`).
    pub async fn pane_at_prompt(&self, pane_id: &str) -> Result<bool> {
        let result = self
            .call(&["pane", "process-info", "--pane", pane_id])
            .await?;
        let procs = result
            .pointer("/process_info/foreground_processes")
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default();
        // At prompt = exactly the shell itself in the foreground.
        Ok(procs.len() == 1
            && procs[0]
                .get("name")
                .and_then(|n| n.as_str())
                .map(|n| n.ends_with("sh"))
                .unwrap_or(false))
    }

    /// A freshly created pane needs a moment before its shell is at the
    /// prompt; typing earlier gets dropped (and herdr refuses `agent start`
    /// until its own detection catches up). Ready = the shell is the only
    /// foreground process AND the prompt has visibly rendered.
    pub async fn wait_for_prompt(&self, pane_id: &str, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let at_prompt = self.pane_at_prompt(pane_id).await.unwrap_or(false);
            if at_prompt {
                let rendered = self
                    .call_text(&["pane", "read", pane_id, "--source", "visible", "--lines", "5"])
                    .await
                    .map(|t| !t.trim().is_empty())
                    .unwrap_or(false);
                if rendered {
                    return Ok(());
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow!("pane {pane_id} never reached a shell prompt"));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    // ── writes ──────────────────────────────────────────────────────────

    /// New tab in `workspace`; returns (tab_id, root_pane_id).
    pub async fn tab_create(
        &self,
        workspace: &str,
        cwd: &str,
        label: &str,
    ) -> Result<(String, String)> {
        let result = self
            .call(&[
                "tab",
                "create",
                "--workspace",
                workspace,
                "--cwd",
                cwd,
                "--label",
                label,
                "--no-focus",
            ])
            .await?;
        let tab_id = result
            .pointer("/tab/tab_id")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow!("tab create returned no tab_id"))?
            .to_string();
        let pane_id = result
            .pointer("/root_pane/pane_id")
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow!("tab create returned no root pane"))?
            .to_string();
        Ok((tab_id, pane_id))
    }

    /// Start a recognized agent in an existing shell pane; waits until herdr
    /// considers it ready for input. Herdr's shell detection can lag a fresh
    /// pane by a few seconds, so "not an available shell" is retried.
    pub async fn agent_start(&self, name: &str, kind: &str, pane_id: &str) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
        loop {
            match self
                .call(&["agent", "start", name, "--kind", kind, "--pane", pane_id])
                .await
            {
                Ok(_) => return Ok(()),
                Err(e)
                    if e.to_string().contains("not an available shell")
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Submit text to an agent (fast-ack: no --wait; the board watcher owns
    /// state tracking).
    pub async fn agent_prompt(&self, target: &str, text: &str) -> Result<()> {
        self.call(&["agent", "prompt", target, text]).await?;
        Ok(())
    }

    pub async fn agent_send_keys(&self, target: &str, key: &str) -> Result<()> {
        self.call(&["agent", "send-keys", target, key]).await?;
        Ok(())
    }

    /// Run a shell command in a pane (atomically types it + Enter).
    pub async fn pane_run(&self, pane_id: &str, command: &str) -> Result<()> {
        self.call(&["pane", "run", pane_id, command]).await?;
        Ok(())
    }

    pub async fn tab_close(&self, tab_id: &str) -> Result<()> {
        self.call(&["tab", "close", tab_id]).await?;
        Ok(())
    }

    pub async fn workspace_focus(&self, workspace: &str) -> Result<()> {
        self.call(&["workspace", "focus", workspace]).await?;
        Ok(())
    }

    /// Path to the herdr binary (for exec-ing the attach UI).
    pub fn binary(&self) -> &std::path::Path {
        &self.binary
    }

    pub async fn workspace_create(&self, label: &str, cwd: &str) -> Result<(String, String)> {
        let result = self
            .call(&[
                "workspace",
                "create",
                "--label",
                label,
                "--cwd",
                cwd,
                "--no-focus",
            ])
            .await?;
        let ws = result
            .pointer("/workspace/workspace_id")
            .and_then(|w| w.as_str())
            .ok_or_else(|| anyhow!("workspace create returned no id"))?
            .to_string();
        let pane = result
            .pointer("/root_pane/pane_id")
            .and_then(|p| p.as_str())
            .unwrap_or_default()
            .to_string();
        Ok((ws, pane))
    }
}
