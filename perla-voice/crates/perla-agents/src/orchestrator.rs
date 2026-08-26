//! The agent-session layer — port of `EmbeddedTerminalSession.swift`, with
//! one execution mode (the cross-platform one): hidden PTY sessions owned by
//! the engine. macOS-only paths (Terminal.app scripting, engage-external)
//! are out of scope for v1; the seams they'd plug into are the same.
//!
//! Semantics ported 1:1:
//! - fast-ack `submit` — returns the instant the prompt is typed, NOT when
//!   the turn ends; completion arrives out-of-band via `AgentEvent`.
//! - dedup — a repeat of the in-flight prompt bounces as `AlreadyRunning`
//!   (the model re-sending on a long turn must never type twice).
//! - queue — one held follow-up per (tool, cwd), newest wins, auto-submitted
//!   on a CLEAN turn end only.
//! - `finish_turn` — THE single completion point, idempotent per run: JSONL
//!   tail, Claude Stop hook, interrupt, exit watch, and End all race into it;
//!   whichever lands first wins.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::digest::{self, AgentDigest};
use crate::hooks::{HookEvent, HookKind, HookServer};
use crate::paths;
use crate::pty::HiddenAgentSession;
use crate::transcripts::{self, TurnOutcome};
use crate::types::AgentTool;

/// Per-run bookkeeping handed back when the turn finally ends. `token` is the
/// run's identity for the finish_turn ownership guard.
#[derive(Debug, Clone)]
pub struct AgentRunContext {
    pub history_id: Option<String>,
    pub token: u64,
    pub started_at: Instant,
}

impl AgentRunContext {
    pub fn new(history_id: Option<String>) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            history_id,
            token: NEXT.fetch_add(1, Ordering::Relaxed),
            started_at: Instant::now(),
        }
    }
}

/// Result of `submit`, returned the instant the prompt is handed over.
#[derive(Debug, Clone)]
pub enum SubmitOutcome {
    Submitted,
    /// The SAME prompt is already in flight; we deliberately did NOT re-send.
    AlreadyRunning {
        running_task: String,
    },
    /// A different turn is in flight; this prompt auto-runs when it ends.
    Queued {
        behind_task: String,
    },
    /// The tool binary isn't installed / the spawn failed.
    Unavailable(String),
}

/// Out-of-band notifications to the voice engine.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A turn began (running=true) or ended (running=false) on (tool, cwd).
    Running {
        tool: AgentTool,
        cwd: String,
        running: bool,
        ok: bool,
    },
    /// A submitted turn ended — success, interrupt, timeout, or cancellation.
    TurnFinished {
        tool: AgentTool,
        cwd: String,
        outcome: TurnOutcome,
        context: AgentRunContext,
    },
    /// A queued prompt was auto-submitted after the previous turn ended.
    QueuedStarted {
        tool: AgentTool,
        cwd: String,
        prompt: String,
    },
    /// The agent posted a needs-attention notice (question, permission ask).
    NeedsAttention {
        tool: AgentTool,
        cwd: String,
        message: String,
    },
    /// Live transcript digest while a turn runs (detail-mode narration food).
    Progress {
        tool: AgentTool,
        cwd: String,
        digest: AgentDigest,
        elapsed_secs: f64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    tool: AgentTool,
    cwd: String,
}

#[derive(Clone)]
struct InFlightRun {
    prompt: String,
    context: AgentRunContext,
}

/// Per-tool launch options the host can set (model / reasoning effort).
#[derive(Debug, Clone, Default)]
pub struct LaunchOptions {
    pub model: Option<String>,
    pub effort: Option<String>,
}

struct Inner {
    in_flight: HashMap<Key, InFlightRun>,
    queued: HashMap<Key, InFlightRun>,
    hidden: HashMap<Key, HiddenAgentSession>,
    wait_tasks: HashMap<Key, JoinHandle<()>>,
    progress_tasks: HashMap<Key, JoinHandle<()>>,
    launch_options: HashMap<AgentTool, LaunchOptions>,
    /// Progress watchers only sample the transcript while this is on.
    detail_mode: bool,
}

pub struct AgentOrchestrator {
    inner: Mutex<Inner>,
    events: mpsc::UnboundedSender<AgentEvent>,
    hooks: tokio::sync::Mutex<Option<Arc<HookServer>>>,
}

impl AgentOrchestrator {
    /// Create the orchestrator; `events` receives everything out-of-band.
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<AgentEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let orch = Arc::new(Self {
            inner: Mutex::new(Inner {
                in_flight: HashMap::new(),
                queued: HashMap::new(),
                hidden: HashMap::new(),
                wait_tasks: HashMap::new(),
                progress_tasks: HashMap::new(),
                launch_options: HashMap::new(),
                detail_mode: true,
            }),
            events: tx,
            hooks: tokio::sync::Mutex::new(None),
        });
        (orch, rx)
    }

    /// Boot the Claude hook listener (idempotent). Failure is silent — the
    /// JSONL poller carries the whole load, exactly like the macOS app.
    pub async fn start_hooks(self: &Arc<Self>) {
        let mut guard = self.hooks.lock().await;
        if guard.is_some() {
            return;
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<HookEvent>();
        if let Some(server) = HookServer::start(tx).await {
            *guard = Some(server);
            let orch = self.clone();
            tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    orch.handle_hook(event);
                }
            });
        } else {
            warn!("hook server could not bind; falling back to transcript polling only");
        }
    }

    pub fn set_detail_mode(&self, on: bool) {
        self.inner.lock().unwrap().detail_mode = on;
    }

    pub fn set_launch_options(&self, tool: AgentTool, options: LaunchOptions) {
        self.inner
            .lock()
            .unwrap()
            .launch_options
            .insert(tool, options);
    }

    pub fn is_running(&self, tool: AgentTool, cwd: &str) -> bool {
        let key = Key {
            tool,
            cwd: transcripts::normalize_cwd(cwd),
        };
        self.inner.lock().unwrap().in_flight.contains_key(&key)
    }

    /// (tool, cwd, is_working) for every live hidden session.
    pub fn live_sessions(&self) -> Vec<(AgentTool, String, bool)> {
        let inner = self.inner.lock().unwrap();
        let mut out: Vec<(AgentTool, String, bool)> = inner
            .hidden
            .keys()
            .map(|k| (k.tool, k.cwd.clone(), inner.in_flight.contains_key(k)))
            .collect();
        out.sort_by(|a, b| a.1.cmp(&b.1));
        out
    }

    pub fn digest(&self, tool: AgentTool, cwd: &str) -> Option<AgentDigest> {
        digest::digest(tool, &transcripts::normalize_cwd(cwd))
    }

    // ── submit (fast-ack) ───────────────────────────────────────────────

    /// Hand a voice-triggered prompt to the agent and return IMMEDIATELY —
    /// the moment the prompt is typed into the TUI, NOT when the turn ends.
    pub async fn submit(
        self: &Arc<Self>,
        tool: AgentTool,
        prompt: &str,
        cwd: &str,
        context: AgentRunContext,
    ) -> SubmitOutcome {
        let Some(binary) = paths::binary_for(tool) else {
            return SubmitOutcome::Unavailable(format!(
                "{} isn't installed. {}",
                tool.label(),
                tool.install_hint()
            ));
        };
        let key = Key {
            tool,
            cwd: transcripts::normalize_cwd(cwd),
        };

        // Dedup reservation — synchronous (one lock scope, no await between
        // the check and the reserve) so two near-simultaneous function calls
        // for the same workspace can't both slip past it. Released by
        // finish_turn, whichever signal fires it first.
        let has_live_session = {
            let mut inner = self.inner.lock().unwrap();
            if let Some(running) = inner.in_flight.get(&key).cloned() {
                let queued_same = inner
                    .queued
                    .get(&key)
                    .map(|q| q.prompt == prompt)
                    .unwrap_or(false);
                if running.prompt == prompt || queued_same {
                    return SubmitOutcome::AlreadyRunning {
                        running_task: running.prompt,
                    };
                }
                // A genuinely new follow-up — hold it for auto-submit on turn
                // end. Newest wins.
                if let Some(displaced) = inner.queued.remove(&key) {
                    let _ = self.events.send(AgentEvent::TurnFinished {
                        tool: key.tool,
                        cwd: key.cwd.clone(),
                        outcome: dropped_outcome("Replaced by a newer queued task."),
                        context: displaced.context,
                    });
                }
                inner.queued.insert(
                    key.clone(),
                    InFlightRun {
                        prompt: prompt.to_string(),
                        context,
                    },
                );
                return SubmitOutcome::Queued {
                    behind_task: running.prompt,
                };
            }
            inner.in_flight.insert(
                key.clone(),
                InFlightRun {
                    prompt: prompt.to_string(),
                    context: context.clone(),
                },
            );
            inner.hidden.contains_key(&key)
        };

        // A hidden PTY already hosts this (tool, cwd) — type straight into it.
        if has_live_session {
            if prompt.is_empty() {
                self.inner.lock().unwrap().in_flight.remove(&key);
                return SubmitOutcome::Submitted;
            }
            {
                let inner = self.inner.lock().unwrap();
                if let Some(session) = inner.hidden.get(&key) {
                    session.send_prompt(prompt);
                }
            }
            self.begin_turn(key, context);
            return SubmitOutcome::Submitted;
        }

        // Cold start: spawn a fresh hidden PTY session.
        match self.cold_start(tool, &key, &binary, &[]).await {
            Ok(()) => {}
            Err(e) => {
                self.inner.lock().unwrap().in_flight.remove(&key);
                return SubmitOutcome::Unavailable(format!(
                    "Couldn't start {} in the background: {e}",
                    tool.label()
                ));
            }
        }
        if prompt.is_empty() {
            self.inner.lock().unwrap().in_flight.remove(&key);
            return SubmitOutcome::Submitted;
        }
        // The cold start suspended ~1s — an End/interrupt in that window
        // already resolved this run. Don't type a prompt the user cancelled.
        {
            let inner = self.inner.lock().unwrap();
            match inner.in_flight.get(&key) {
                Some(run) if run.context.token == context.token => {}
                _ => {
                    return SubmitOutcome::Unavailable(
                        "The session was ended before the task could be typed.".into(),
                    )
                }
            }
            if let Some(session) = inner.hidden.get(&key) {
                session.send_prompt(prompt);
            }
        }
        self.begin_turn(key, context);
        SubmitOutcome::Submitted
    }

    async fn cold_start(
        self: &Arc<Self>,
        tool: AgentTool,
        key: &Key,
        binary: &std::path::Path,
        base_flags: &[String],
    ) -> anyhow::Result<()> {
        let flags = self.launch_flags(tool, base_flags).await;
        let session = HiddenAgentSession::spawn(tool, &key.cwd, &binary.to_string_lossy(), &flags)?;
        let mut exited = session.exited.clone();
        self.inner
            .lock()
            .unwrap()
            .hidden
            .insert(key.clone(), session);

        // Exit watch: crash, /exit, external kill — NOT terminate(). Free the
        // slot and resolve the in-flight run NOW instead of burning the
        // 10-minute silence budget as a stuck "working" indicator.
        {
            let orch = self.clone();
            let key = key.clone();
            tokio::spawn(async move {
                let _ = exited.changed().await;
                orch.handle_session_exit(&key);
            });
        }

        // Let the TUI boot and draw its composer before anyone types into it.
        tokio::time::sleep(Duration::from_millis(900)).await;
        let alive = self
            .inner
            .lock()
            .unwrap()
            .hidden
            .get(key)
            .map(|s| s.is_alive())
            .unwrap_or(false);
        if !alive {
            anyhow::bail!("the agent process died during startup");
        }
        Ok(())
    }

    /// Full launch flags: base (resume args) + permission mode + model/effort +
    /// for Claude the hook `--settings` (empty when the hook server can't
    /// start, degrading to polling-only).
    async fn launch_flags(self: &Arc<Self>, tool: AgentTool, base: &[String]) -> Vec<String> {
        let mut flags: Vec<String> = base.to_vec();
        flags.extend(tool.launch_flags());
        let options = self
            .inner
            .lock()
            .unwrap()
            .launch_options
            .get(&tool)
            .cloned()
            .unwrap_or_default();
        if let Some(model) = options.model.filter(|m| !m.is_empty()) {
            flags.push("--model".into());
            flags.push(model);
        }
        if let Some(effort) = options.effort.filter(|e| !e.is_empty()) {
            match tool {
                AgentTool::Claude => {
                    flags.push("--effort".into());
                    flags.push(effort);
                }
                AgentTool::Codex => {
                    flags.push("-c".into());
                    flags.push(format!("model_reasoning_effort=\"{effort}\""));
                }
            }
        }
        if tool == AgentTool::Claude {
            self.start_hooks().await;
            if let Some(server) = self.hooks.lock().await.as_ref() {
                flags.extend(server.claude_launch_flags());
            }
        }
        flags
    }

    /// Working indicator + JSONL turn-end wait + progress sampling for the
    /// turn. The wait races the Claude Stop hook into finish_turn, whose
    /// ownership guard keeps them idempotent. (Codex has no hooks, so for it
    /// the wait IS the completion signal.)
    fn begin_turn(self: &Arc<Self>, key: Key, context: AgentRunContext) {
        let _ = self.events.send(AgentEvent::Running {
            tool: key.tool,
            cwd: key.cwd.clone(),
            running: true,
            ok: true,
        });

        let mut inner = self.inner.lock().unwrap();
        if let Some(old) = inner.wait_tasks.remove(&key) {
            old.abort();
        }
        {
            let orch = self.clone();
            let key2 = key.clone();
            let ctx = context.clone();
            inner.wait_tasks.insert(
                key.clone(),
                tokio::spawn(async move {
                    let outcome = transcripts::wait_for_turn_end(key2.tool, &key2.cwd).await;
                    orch.finish_turn(&key2, outcome, &ctx);
                }),
            );
        }
        if let Some(old) = inner.progress_tasks.remove(&key) {
            old.abort();
        }
        {
            let orch = self.clone();
            let key2 = key.clone();
            let ctx = context;
            inner.progress_tasks.insert(
                key.clone(),
                tokio::spawn(async move {
                    orch.watch_progress(key2, ctx).await;
                }),
            );
        }
    }

    /// Sample the transcript ~every 1.5s while this run owns the slot and
    /// emit Progress events. The digest read (file I/O + JSON parse) runs on
    /// the blocking pool. Narration decides what's worth saying.
    async fn watch_progress(self: Arc<Self>, key: Key, context: AgentRunContext) {
        loop {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            {
                let inner = self.inner.lock().unwrap();
                match inner.in_flight.get(&key) {
                    Some(run) if run.context.token == context.token => {}
                    _ => return,
                }
                if !inner.detail_mode {
                    continue;
                }
            }
            let tool = key.tool;
            let cwd = key.cwd.clone();
            let snapshot = tokio::task::spawn_blocking(move || digest::digest(tool, &cwd)).await;
            let Ok(Some(snapshot)) = snapshot else {
                continue;
            };
            {
                let inner = self.inner.lock().unwrap();
                match inner.in_flight.get(&key) {
                    Some(run) if run.context.token == context.token => {}
                    _ => return,
                }
            }
            let _ = self.events.send(AgentEvent::Progress {
                tool: key.tool,
                cwd: key.cwd.clone(),
                digest: snapshot,
                elapsed_secs: context.started_at.elapsed().as_secs_f64(),
            });
        }
    }

    // ── control ─────────────────────────────────────────────────────────

    /// Interrupt the current turn: Esc into the live PTY (stops the agent
    /// generating but keeps the session at its prompt) and resolve the
    /// in-flight run as stopped. False if no live session matches.
    pub fn interrupt(self: &Arc<Self>, tool: AgentTool, cwd: &str) -> bool {
        let key = Key {
            tool,
            cwd: transcripts::normalize_cwd(cwd),
        };
        let run = {
            let inner = self.inner.lock().unwrap();
            let Some(session) = inner.hidden.get(&key) else {
                return false;
            };
            session.send_interrupt();
            inner.in_flight.get(&key).cloned()
        };
        if let Some(run) = run {
            self.finish_turn(
                &key,
                TurnOutcome {
                    ok: false,
                    summary: "Stopped by the user.".into(),
                    session_id: None,
                    interrupted: false,
                },
                &run.context,
            );
        } else {
            let mut inner = self.inner.lock().unwrap();
            if let Some(t) = inner.wait_tasks.remove(&key) {
                t.abort();
            }
            if let Some(t) = inner.progress_tasks.remove(&key) {
                t.abort();
            }
        }
        self.drop_queued(&key, "Stopped by the user.");
        true
    }

    /// Inject a mid-run instruction into the live agent without starting a
    /// new task. False if no live session matches.
    pub fn steer(&self, tool: AgentTool, cwd: &str, message: &str) -> bool {
        let key = Key {
            tool,
            cwd: transcripts::normalize_cwd(cwd),
        };
        let inner = self.inner.lock().unwrap();
        match inner.hidden.get(&key) {
            Some(session) => {
                session.send_prompt(message);
                true
            }
            None => false,
        }
    }

    /// End everything: resolve in-flight runs, terminate every hidden agent,
    /// cancel watchers, drop queued follow-ups. Called when the voice session
    /// ends — an in-flight task must not keep "working" after End.
    pub fn terminate_all(self: &Arc<Self>) {
        let (runs, queued_keys) = {
            let inner = self.inner.lock().unwrap();
            (
                inner
                    .in_flight
                    .iter()
                    .map(|(k, r)| (k.clone(), r.clone()))
                    .collect::<Vec<_>>(),
                inner.queued.keys().cloned().collect::<Vec<_>>(),
            )
        };
        for (key, run) in runs {
            self.finish_turn(
                &key,
                TurnOutcome {
                    ok: false,
                    summary: "The session was ended.".into(),
                    session_id: None,
                    interrupted: false,
                },
                &run.context,
            );
        }
        for key in queued_keys {
            self.drop_queued(&key, "The session was ended.");
        }
        let mut inner = self.inner.lock().unwrap();
        for (_, mut session) in inner.hidden.drain() {
            session.terminate();
        }
        for (_, t) in inner.wait_tasks.drain() {
            t.abort();
        }
        for (_, t) in inner.progress_tasks.drain() {
            t.abort();
        }
    }

    // ── completion ──────────────────────────────────────────────────────

    /// THE single turn-completion point — called by the JSONL-tail wait task,
    /// the Claude Stop hook, interrupt, the exit watch, and End. The
    /// ownership guard makes it idempotent per run: whichever signal arrives
    /// first performs the side effects; every later one is a no-op.
    fn finish_turn(self: &Arc<Self>, key: &Key, outcome: TurnOutcome, context: &AgentRunContext) {
        let queued_next = {
            let mut inner = self.inner.lock().unwrap();
            match inner.in_flight.get(key) {
                Some(run) if run.context.token == context.token => {}
                _ => return,
            }
            inner.in_flight.remove(key);
            if let Some(t) = inner.wait_tasks.remove(key) {
                t.abort();
            }
            if let Some(t) = inner.progress_tasks.remove(key) {
                t.abort();
            }
            inner.queued.remove(key)
        };

        let _ = self.events.send(AgentEvent::Running {
            tool: key.tool,
            cwd: key.cwd.clone(),
            running: false,
            ok: outcome.ok,
        });
        let ok = outcome.ok;
        let _ = self.events.send(AgentEvent::TurnFinished {
            tool: key.tool,
            cwd: key.cwd.clone(),
            outcome,
            context: context.clone(),
        });

        // Drain the queue — but only after a CLEAN turn end. On a timeout /
        // lost-track / stopped outcome the agent may still be mid-turn, and
        // typing a held prompt into it would be the double-prompt failure.
        if let Some(next) = queued_next {
            if ok {
                let _ = self.events.send(AgentEvent::QueuedStarted {
                    tool: key.tool,
                    cwd: key.cwd.clone(),
                    prompt: next.prompt.clone(),
                });
                let orch = self.clone();
                let key = key.clone();
                tokio::spawn(async move {
                    orch.submit(key.tool, &next.prompt, &key.cwd, next.context)
                        .await;
                });
            } else {
                let _ = self.events.send(AgentEvent::TurnFinished {
                    tool: key.tool,
                    cwd: key.cwd.clone(),
                    outcome: dropped_outcome("The previous turn didn't end cleanly."),
                    context: next.context,
                });
            }
        }
    }

    /// Claude's Stop hook fired — the turn is over RIGHT NOW. Resolve it from
    /// the transcript digest instead of waiting on the poller; if the poller
    /// got there first, finish_turn's ownership guard makes this a no-op.
    fn handle_hook(self: &Arc<Self>, event: HookEvent) {
        let key = Key {
            tool: AgentTool::Claude,
            cwd: transcripts::normalize_cwd(&event.cwd),
        };
        // When the event names its process it must be OUR session — a Stop or
        // Notification from an un-engaged sibling in the same folder must not
        // close the run (nor be voiced as if it came from our session).
        let (matches, run) = {
            let inner = self.inner.lock().unwrap();
            let matches = match (event.pid, inner.hidden.get(&key)) {
                (Some(pid), Some(session)) => session.pid == Some(pid),
                (None, Some(_)) => true,
                _ => false,
            };
            (matches, inner.in_flight.get(&key).cloned())
        };
        if !matches {
            return;
        }
        match event.kind {
            HookKind::Stop => {
                let Some(run) = run else { return };
                let orch = self.clone();
                let transcript = event.transcript_path.clone();
                let cwd = key.cwd.clone();
                tokio::spawn(async move {
                    // Digest the event's own transcript when it names one —
                    // newest-in-folder could be a same-folder sibling's file.
                    let digest = tokio::task::spawn_blocking(move || match transcript {
                        Some(path) => {
                            digest::digest_file(AgentTool::Claude, std::path::Path::new(&path))
                        }
                        None => digest::digest(AgentTool::Claude, &cwd),
                    })
                    .await
                    .ok()
                    .flatten();
                    orch.finish_turn(
                        &key,
                        TurnOutcome {
                            ok: true,
                            summary: digest
                                .as_ref()
                                .and_then(|d| d.last_message.clone())
                                .unwrap_or_else(|| "Done.".into()),
                            session_id: digest.and_then(|d| d.session_id),
                            interrupted: false,
                        },
                        &run.context,
                    );
                });
            }
            HookKind::Notification { message } => {
                let _ = self.events.send(AgentEvent::NeedsAttention {
                    tool: AgentTool::Claude,
                    cwd: key.cwd,
                    message,
                });
            }
        }
    }

    /// The hidden agent exited on its own (crash, /exit, external kill).
    fn handle_session_exit(self: &Arc<Self>, key: &Key) {
        let run = {
            let mut inner = self.inner.lock().unwrap();
            let intentional = inner
                .hidden
                .get(key)
                .map(|s| s.was_terminated())
                .unwrap_or(true);
            if intentional {
                return; // terminate()/terminate_all already handled it
            }
            debug!(tool = key.tool.id(), cwd = %key.cwd, "hidden agent exited unexpectedly");
            inner.hidden.remove(key);
            inner.in_flight.get(key).cloned()
        };
        if let Some(run) = run {
            self.finish_turn(
                key,
                TurnOutcome {
                    ok: false,
                    summary: "The background agent exited unexpectedly.".into(),
                    session_id: None,
                    interrupted: false,
                },
                &run.context,
            );
        } else {
            let mut inner = self.inner.lock().unwrap();
            if let Some(t) = inner.wait_tasks.remove(key) {
                t.abort();
            }
            if let Some(t) = inner.progress_tasks.remove(key) {
                t.abort();
            }
            let _ = self.events.send(AgentEvent::Running {
                tool: key.tool,
                cwd: key.cwd.clone(),
                running: false,
                ok: false,
            });
        }
        self.drop_queued(key, "The agent process exited.");
    }

    /// A held prompt is being dropped without ever running. Report it through
    /// the normal turn-finished channel (ok=false) so bookkeeping stays
    /// consistent.
    fn drop_queued(&self, key: &Key, reason: &str) {
        let run = self.inner.lock().unwrap().queued.remove(key);
        if let Some(run) = run {
            let _ = self.events.send(AgentEvent::TurnFinished {
                tool: key.tool,
                cwd: key.cwd.clone(),
                outcome: dropped_outcome(reason),
                context: run.context,
            });
        }
    }
}

fn dropped_outcome(reason: &str) -> TurnOutcome {
    TurnOutcome {
        ok: false,
        summary: format!("Queued task was not started: {reason}"),
        session_id: None,
        interrupted: false,
    }
}
