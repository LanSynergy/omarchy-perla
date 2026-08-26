//! Perla's hands — a persistent headless `grok` (grok-build) session per
//! workspace, driven over ACP. Where `perla-agents` tails JSONL transcripts
//! to reverse-engineer what a CLI is doing, this crate gets the same signals
//! FIRST-CLASS from the protocol: plan updates become todos, tool calls
//! become recent actions and changed files, and the `session/prompt` response
//! IS the turn-end signal.
//!
//! The orchestration contract is kept identical to `perla-agents` so the
//! engine and the voice prompt behave the same way:
//! - fast-ack submit (returns when handed off, completion arrives later),
//! - dedup (re-sent identical prompt bounces as AlreadyRunning),
//! - queueing (new prompts during a turn run right after it),
//! - one live session per workspace.

pub mod acp;
pub mod dispatcher;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::{info, warn};

use perla_agents::digest::{AgentDigest, Todo};
use perla_agents::orchestrator::AgentRunContext;
use perla_agents::transcripts::{normalize_cwd, TurnOutcome};

pub use dispatcher::HandsDispatcher;

/// Out-of-band notifications to the voice engine — the hands-flavored
/// mirror of `perla_agents::AgentEvent`.
#[derive(Debug)]
pub enum HandsEvent {
    /// A turn began (running=true) or the queue drained (running=false).
    Running {
        cwd: String,
        running: bool,
        ok: bool,
    },
    /// A submitted turn ended. `changed_files` is what THIS turn touched —
    /// there is no transcript to re-digest, the protocol told us directly.
    TurnFinished {
        cwd: String,
        outcome: TurnOutcome,
        context: AgentRunContext,
        changed_files: Vec<String>,
    },
    /// A queued prompt was picked up after the previous turn ended.
    QueuedStarted { cwd: String, prompt: String },
    /// Live digest while a turn runs (detail-mode narration food).
    Progress {
        cwd: String,
        digest: AgentDigest,
        elapsed_secs: f64,
    },
}

/// Result of a submit — same shape and meaning as the agents crate's.
#[derive(Debug, Clone)]
pub enum HandsSubmit {
    Submitted,
    AlreadyRunning { running_task: String },
    Queued { behind_task: String },
    Unavailable(String),
}

struct Turn {
    prompt: String,
    context: AgentRunContext,
    /// Steering messages ride the queue too, but their completion is not
    /// announced — the user asked for a course correction, not a new result.
    quiet: bool,
    /// Files the agent edited during THIS turn (protocol-reported).
    changed_files: Vec<String>,
}

struct SessionState {
    /// Front = the running turn; the rest are queued server-side (ACP queues
    /// `session/prompt` requests and answers each when ITS turn ends).
    turns: VecDeque<Turn>,
    digest: AgentDigest,
    /// Streaming buffer for the current turn's agent message.
    message_buf: String,
    closed: bool,
}

/// One persistent grok process bound to one workspace.
pub struct HandsSession {
    cwd: String,
    client: acp::AcpClient,
    session_id: String,
    events: mpsc::UnboundedSender<HandsEvent>,
    state: Mutex<SessionState>,
}

/// How long a single turn may run before we give up on its response.
const TURN_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

impl HandsSession {
    /// Spawn the agent, run the ACP handshake, create the session, and start
    /// the notification + progress pumps.
    pub async fn connect(
        binary: &Path,
        cwd: &str,
        model: Option<&str>,
        events: mpsc::UnboundedSender<HandsEvent>,
    ) -> anyhow::Result<Arc<Self>> {
        let mut args: Vec<String> = vec!["agent".into(), "--always-approve".into()];
        if let Some(model) = model {
            args.push("--model".into());
            args.push(model.to_string());
        }
        args.push("stdio".into());

        let cwd_path = PathBuf::from(cwd);
        let (client, mut notifications) = acp::AcpClient::spawn(binary, &args, &cwd_path)?;

        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": { "readTextFile": false, "writeTextFile": false },
                        "terminal": false
                    }
                }),
                HANDSHAKE_TIMEOUT,
            )
            .await?;

        let new_session = client
            .request(
                "session/new",
                json!({ "cwd": cwd, "mcpServers": [] }),
                HANDSHAKE_TIMEOUT,
            )
            .await?;
        let session_id = new_session
            .get("sessionId")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("session/new returned no sessionId"))?
            .to_string();

        let session = Arc::new(HandsSession {
            cwd: normalize_cwd(cwd),
            client,
            session_id,
            events,
            state: Mutex::new(SessionState {
                turns: VecDeque::new(),
                digest: AgentDigest::default(),
                message_buf: String::new(),
                closed: false,
            }),
        });

        // Notification pump: protocol updates → digest.
        let pump = session.clone();
        tokio::spawn(async move {
            while let Some(n) = notifications.recv().await {
                pump.on_notification(&n.method, &n.params);
            }
            // Channel closed = process exited. Fail any turns still tracked
            // (their request waiters were already failed by the reader).
            pump.state.lock().unwrap().closed = true;
        });

        // Progress pump: while a turn runs, snapshot the digest for
        // narration — same 1.5s cadence as the transcript watcher.
        let ticker = session.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                let (digest, elapsed) = {
                    let state = ticker.state.lock().unwrap();
                    if state.closed {
                        return;
                    }
                    let Some(front) = state.turns.front() else {
                        continue;
                    };
                    (
                        state.digest.clone(),
                        front.context.started_at.elapsed().as_secs_f64(),
                    )
                };
                let _ = ticker.events.send(HandsEvent::Progress {
                    cwd: ticker.cwd.clone(),
                    digest,
                    elapsed_secs: elapsed,
                });
            }
        });

        info!(cwd = %session.cwd, "hands session ready");
        Ok(session)
    }

    /// Fast-ack submit. The `session/prompt` response arrives when the turn
    /// ENDS, so it's awaited on a background task; ACP queues overlapping
    /// prompt requests server-side.
    pub fn submit(
        self: &Arc<Self>,
        prompt: &str,
        context: AgentRunContext,
        quiet: bool,
    ) -> HandsSubmit {
        let prompt = prompt.trim().to_string();
        let token = context.token;
        let outcome = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return HandsSubmit::Unavailable("the hands process exited — try again".into());
            }
            if let Some(same) = state.turns.iter().find(|t| t.prompt == prompt) {
                return HandsSubmit::AlreadyRunning {
                    running_task: same.prompt.clone(),
                };
            }
            let behind = state.turns.front().map(|t| t.prompt.clone());
            let is_first = behind.is_none();
            if is_first {
                // Fresh turn: clear the message stream; the plan and action
                // history persist until the agent replaces them.
                state.message_buf.clear();
                state.digest.last_message = None;
            }
            state.turns.push_back(Turn {
                prompt: prompt.clone(),
                context,
                quiet,
                changed_files: Vec::new(),
            });
            match behind {
                None => HandsSubmit::Submitted,
                Some(behind_task) => HandsSubmit::Queued { behind_task },
            }
        };

        if matches!(outcome, HandsSubmit::Submitted) {
            let _ = self.events.send(HandsEvent::Running {
                cwd: self.cwd.clone(),
                running: true,
                ok: true,
            });
        }

        let session = self.clone();
        tokio::spawn(async move {
            let result = session
                .client
                .request(
                    "session/prompt",
                    json!({
                        "sessionId": session.session_id,
                        "prompt": [{ "type": "text", "text": prompt }]
                    }),
                    TURN_TIMEOUT,
                )
                .await;
            session.turn_finished(token, result);
        });
        outcome
    }

    /// The `session/prompt` response landed — resolve the turn it belongs to.
    fn turn_finished(&self, token: u64, result: anyhow::Result<Value>) {
        let (turn, digest_message, next_prompt) = {
            let mut state = self.state.lock().unwrap();
            let Some(pos) = state.turns.iter().position(|t| t.context.token == token) else {
                return;
            };
            // Responses should resolve in FIFO order; tolerate out-of-order
            // by removing the matching turn wherever it sits.
            let turn = state.turns.remove(pos).unwrap();
            // Capture the finished turn's final message BEFORE the promoted
            // turn resets the stream.
            let message = state.digest.last_message.clone();
            // A newly promoted front turn starts its clock now — it spent
            // the time so far waiting, not working.
            if let Some(front) = state.turns.front_mut() {
                front.context.started_at = std::time::Instant::now();
                state.message_buf.clear();
                state.digest.last_message = None;
            }
            (turn, message, state.turns.front().map(|t| t.prompt.clone()))
        };

        let outcome = match result {
            Ok(response) => {
                let stop = response
                    .get("stopReason")
                    .and_then(|s| s.as_str())
                    .unwrap_or("end_turn");
                match stop {
                    "end_turn" | "max_turn_requests" => TurnOutcome {
                        ok: true,
                        summary: digest_message.unwrap_or_else(|| "Done.".into()),
                        session_id: Some(self.session_id.clone()),
                        interrupted: false,
                    },
                    // `session/cancel` is only ever sent on the user's
                    // behalf — semantically identical to Esc in a TUI.
                    "cancelled" => TurnOutcome {
                        ok: false,
                        summary: "Stopped by the user.".into(),
                        session_id: Some(self.session_id.clone()),
                        interrupted: true,
                    },
                    "refusal" => TurnOutcome {
                        ok: false,
                        summary: "The agent declined the request.".into(),
                        session_id: Some(self.session_id.clone()),
                        interrupted: false,
                    },
                    other => TurnOutcome {
                        ok: false,
                        summary: format!("The turn ended early ({other})."),
                        session_id: Some(self.session_id.clone()),
                        interrupted: false,
                    },
                }
            }
            Err(e) => {
                warn!(cwd = %self.cwd, "hands turn failed: {e:#}");
                TurnOutcome {
                    ok: false,
                    summary: format!("The task failed: {e:#}"),
                    session_id: Some(self.session_id.clone()),
                    interrupted: false,
                }
            }
        };

        if !turn.quiet {
            let _ = self.events.send(HandsEvent::TurnFinished {
                cwd: self.cwd.clone(),
                outcome,
                context: turn.context,
                changed_files: turn.changed_files,
            });
        }
        match next_prompt {
            Some(prompt) => {
                let _ = self.events.send(HandsEvent::QueuedStarted {
                    cwd: self.cwd.clone(),
                    prompt,
                });
            }
            None => {
                let _ = self.events.send(HandsEvent::Running {
                    cwd: self.cwd.clone(),
                    running: false,
                    ok: true,
                });
            }
        }
    }

    /// Cancel the running turn. The prompt's response then arrives with
    /// `stopReason: cancelled` and resolves through the normal path.
    pub fn cancel(&self) -> bool {
        let running = !self.state.lock().unwrap().turns.is_empty();
        if running {
            self.client
                .notify("session/cancel", json!({ "sessionId": self.session_id }));
        }
        running
    }

    pub fn is_running(&self) -> bool {
        !self.state.lock().unwrap().turns.is_empty()
    }

    /// (running, digest snapshot, queued-behind count)
    pub fn snapshot(&self) -> (bool, AgentDigest, usize) {
        let state = self.state.lock().unwrap();
        let running = !state.turns.is_empty();
        let queued = state.turns.len().saturating_sub(1);
        (running, state.digest.clone(), queued)
    }

    pub fn terminate(&self) {
        self.state.lock().unwrap().closed = true;
        self.client.kill();
    }

    // ── protocol → digest ───────────────────────────────────────────────

    fn on_notification(&self, method: &str, params: &Value) {
        if method != "session/update" && method != "x.ai/session/update" {
            return;
        }
        if params.get("sessionId").and_then(|s| s.as_str()) != Some(self.session_id.as_str()) {
            return;
        }
        let Some(update) = params.get("update") else {
            return;
        };
        let kind = update
            .get("sessionUpdate")
            .and_then(|k| k.as_str())
            .unwrap_or("");
        let mut state = self.state.lock().unwrap();
        match kind {
            "agent_message_chunk" => {
                if let Some(text) = update.pointer("/content/text").and_then(|t| t.as_str()) {
                    state.message_buf.push_str(text);
                    let trimmed = state.message_buf.trim();
                    if !trimmed.is_empty() {
                        state.digest.last_message = Some(trimmed.to_string());
                    }
                }
            }
            "plan" => {
                if let Some(entries) = update.get("entries").and_then(|e| e.as_array()) {
                    state.digest.todos = entries
                        .iter()
                        .filter_map(|e| {
                            let text = e.get("content").and_then(|c| c.as_str())?;
                            Some(Todo {
                                text: text.to_string(),
                                status: e
                                    .get("status")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("pending")
                                    .to_string(),
                            })
                        })
                        .collect();
                }
            }
            "tool_call" | "tool_call_update" => {
                if kind == "tool_call" {
                    let title = update
                        .get("title")
                        .and_then(|t| t.as_str())
                        .unwrap_or("tool");
                    let line: String = title.chars().take(64).collect();
                    state.digest.recent_actions.push(line);
                    let overflow = state.digest.recent_actions.len().saturating_sub(10);
                    state.digest.recent_actions.drain(..overflow);
                    // ACP message chunks have no boundary marker — a tool
                    // call means the preceding text was pre-work commentary,
                    // so restart the buffer. `last_message` then converges on
                    // the FINAL post-work message, which is what the
                    // completion summary should carry.
                    state.message_buf.clear();
                }
                // Edits carry their target paths in `locations` — that's the
                // changed-files signal, no mtime heuristics needed.
                let is_edit = update.get("kind").and_then(|k| k.as_str()) == Some("edit");
                if is_edit {
                    let paths: Vec<String> = update
                        .get("locations")
                        .and_then(|l| l.as_array())
                        .map(|locs| {
                            locs.iter()
                                .filter_map(|l| l.get("path").and_then(|p| p.as_str()))
                                .map(String::from)
                                .collect()
                        })
                        .unwrap_or_default();
                    for path in paths {
                        state.digest.changed_files.retain(|p| p != &path);
                        state.digest.changed_files.push(path.clone());
                        if let Some(front) = state.turns.front_mut() {
                            front.changed_files.retain(|p| p != &path);
                            front.changed_files.push(path);
                        }
                    }
                    let overflow = state.digest.changed_files.len().saturating_sub(20);
                    state.digest.changed_files.drain(..overflow);
                }
            }
            _ => {}
        }
    }
}

/// One `HandsSession` per workspace, created lazily on first submit —
/// the hands-flavored mirror of `AgentOrchestrator`.
pub struct HandsPool {
    binary: Mutex<Option<PathBuf>>,
    model: Option<String>,
    sessions: tokio::sync::Mutex<HashMap<String, Arc<HandsSession>>>,
    events: mpsc::UnboundedSender<HandsEvent>,
}

impl HandsPool {
    pub fn new(
        binary_override: Option<PathBuf>,
        model: Option<String>,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<HandsEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                binary: Mutex::new(binary_override),
                model,
                sessions: tokio::sync::Mutex::new(HashMap::new()),
                events: tx,
            }),
            rx,
        )
    }

    async fn session_for(&self, cwd: &str) -> anyhow::Result<Arc<HandsSession>> {
        let key = normalize_cwd(cwd);
        let mut sessions = self.sessions.lock().await;
        // A session whose process died gets replaced transparently.
        if let Some(existing) = sessions.get(&key) {
            if !existing.state.lock().unwrap().closed {
                return Ok(existing.clone());
            }
            sessions.remove(&key);
        }
        let Some(binary) = self.resolve_binary() else {
            anyhow::bail!(
                "The grok CLI isn't installed. Install grok-build (or set hands_binary in the config) to give Perla her hands."
            );
        };
        let session =
            HandsSession::connect(&binary, &key, self.model.as_deref(), self.events.clone())
                .await?;
        sessions.insert(key, session.clone());
        Ok(session)
    }

    /// Fast-ack submit into the workspace's session (spawning it if needed).
    pub async fn submit(
        &self,
        cwd: &str,
        prompt: &str,
        context: AgentRunContext,
        quiet: bool,
    ) -> HandsSubmit {
        match self.session_for(cwd).await {
            Ok(session) => session.submit(prompt, context, quiet),
            Err(e) => HandsSubmit::Unavailable(format!("{e:#}")),
        }
    }

    pub fn cancel(&self, cwd: &str) -> bool {
        self.existing(cwd).map(|s| s.cancel()).unwrap_or(false)
    }

    pub fn is_running(&self, cwd: &str) -> bool {
        self.existing(cwd).map(|s| s.is_running()).unwrap_or(false)
    }

    pub fn snapshot(&self, cwd: &str) -> Option<(bool, AgentDigest, usize)> {
        self.existing(cwd).map(|s| s.snapshot())
    }

    /// (cwd, working) for every live session — the cross-project view.
    pub fn live_sessions(&self) -> Vec<(String, bool)> {
        match self.sessions.try_lock() {
            Ok(sessions) => sessions
                .iter()
                .filter(|(_, s)| !s.state.lock().unwrap().closed)
                .map(|(cwd, s)| (cwd.clone(), s.is_running()))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn terminate_all(&self) {
        if let Ok(mut sessions) = self.sessions.try_lock() {
            for (_, session) in sessions.drain() {
                session.terminate();
            }
        }
    }

    fn existing(&self, cwd: &str) -> Option<Arc<HandsSession>> {
        let key = normalize_cwd(cwd);
        self.sessions.try_lock().ok()?.get(&key).cloned()
    }

    /// Explicit override, then `~/.grok/bin/grok`, then $PATH.
    fn resolve_binary(&self) -> Option<PathBuf> {
        let mut cached = self.binary.lock().unwrap();
        if let Some(path) = cached.as_ref() {
            return Some(path.clone());
        }
        let found = find_grok_binary()?;
        *cached = Some(found.clone());
        Some(found)
    }
}

/// Locate the grok binary the way a shell would, plus the installer's
/// default location.
pub fn find_grok_binary() -> Option<PathBuf> {
    if let Some(home) = dirs::home_dir() {
        let installed = home.join(".grok/bin/grok");
        if installed.is_file() {
            return Some(installed);
        }
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            let candidate = Path::new(dir).join("grok");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
