//! The voice-orchestrator engine — port of `RealtimeSession.swift` onto the
//! WebSocket transport. One actor task owns all session state; the outside
//! world drives it with `EngineCommand`s and renders from `EngineEvent`s.
//!
//! Ported 1:1 where the Swift encodes hard-won lessons:
//! - barge-in double-kill (`response.cancel` + local playback clear),
//! - the awaiting-user-response gap guard (side channel never jumps in
//!   between the user finishing a sentence and the model answering it),
//! - side-channel coalescing + hold mode,
//! - transparent reconnect with a conversation recap injected into the fresh
//!   session's instructions,
//! - proactive pre-cap rotation during a silent gap,
//! - fast-ack agent tools with out-of-band completion narration.

use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use perla_agents::digest;
use perla_agents::dispatcher::{AgentDispatcher, SharedAgentState};
use perla_agents::narration::Narration;
use perla_agents::orchestrator::{AgentEvent, AgentOrchestrator, LaunchOptions};
use perla_agents::transcripts::{normalize_cwd, TurnOutcome};
use perla_agents::types::AgentTool;
use perla_audio::AudioSystem;
use perla_hands::{HandsDispatcher, HandsEvent, HandsPool};
use perla_herdr::{BoardWatcher, HerdrClient, HerdrDispatcher, HerdrEvent};
use perla_provider::events as pe;
use perla_provider::{Connection, Dialect, ProviderSettings};
use perla_tools::{
    builder_tools, hands_tools, herdr_tools, omarchy_tools,
    prompt::build_board_clause, prompt::build_hands_instructions,
    assist_tools, prompt::build_desktop_instructions, prompt::build_system_instructions,
    prompt::PromptContext, AssistDispatcher,
    AssistLayer, LayeredDispatcher, OmarchyDispatcher,
    ToolCallContext, ToolDispatcher, ToolResult,
};

use crate::config::{Config, ProviderKind};
use crate::cost::TokenPrices;
use crate::events::{
    ConnectingPhase, EngineCommand, EngineEvent, Role, Speaker, Status, TranscriptLine,
};
use crate::language::LanguageLock;
use crate::recap;
use crate::sidechannel::{SideChannel, SideChannelItem, SideChannelKind};

/// Handle to a running engine. Cheap to clone; dropping every handle does NOT
/// stop the session — send `EngineCommand::Stop` for that.
#[derive(Clone)]
pub struct Engine {
    commands: mpsc::UnboundedSender<EngineCommand>,
}

impl Engine {
    /// Full default wiring. Hands mode (the default): one grok-build session
    /// is Perla's hands for everything. Agents mode: the Claude Code / Codex
    /// CLI orchestrator, like the macOS app.
    /// Must be called inside a tokio runtime.
    pub fn start(config: Config) -> (Engine, mpsc::UnboundedReceiver<EngineEvent>) {
        let state = SharedAgentState::new(
            config.workspace.to_string_lossy().into_owned(),
            config
                .recent_workspaces
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            AgentTool::from_id(&config.runtime).unwrap_or(AgentTool::Claude),
        );
        state
            .detail_mode
            .store(config.detail_mode, std::sync::atomic::Ordering::Relaxed);
        state.big_moments_only.store(
            config.big_moments_only,
            std::sync::atomic::Ordering::Relaxed,
        );

        // Desktop-only: no coding agent behind her, so none of their tools are
        // offered. Without this, a box with an unauthenticated grok still shows
        // run_task/check_task, and the model reaches for them, gets
        // "Authentication required", and tells the user a *website* wants a
        // login. Removing the tool removes the whole class of confabulation.
        if config.mode == "desktop" {
            let dispatcher: Arc<dyn ToolDispatcher> =
                wrap_omarchy(Arc::new(NoAgentDispatcher), &config);
            return Self::spawn_full(config, state, dispatcher, None, None, None, None, None);
        }

        if config.mode == "agents" {
            let (orchestrator, agent_rx) = AgentOrchestrator::new();
            orchestrator.set_detail_mode(config.detail_mode);
            orchestrator.set_launch_options(
                state.runtime(),
                LaunchOptions {
                    model: config.agent_model.clone(),
                    effort: config.agent_effort.clone(),
                },
            );
            let dispatcher: Arc<dyn ToolDispatcher> = wrap_omarchy(
                Arc::new(AgentDispatcher {
                    orchestrator: orchestrator.clone(),
                    state: state.clone(),
                }),
                &config,
            );
            Self::spawn_full(
                config,
                state,
                dispatcher,
                Some(orchestrator),
                Some(agent_rx),
                None,
                None,
                None,
            )
        } else {
            let (pool, hands_rx) =
                HandsPool::new(config.hands_binary.clone(), config.hands_model.clone());
            let hands_dispatcher = Arc::new(HandsDispatcher {
                pool: pool.clone(),
                state: state.clone(),
            });

            // Herdr board: auto-enabled when the multiplexer is actually
            // there (binary + running server), unless the config says no.
            let use_herdr = config
                .herdr
                .unwrap_or_else(perla_herdr::herdr_available)
                && perla_herdr::herdr_available();
            let (dispatcher, herdr_rx): (
                Arc<dyn ToolDispatcher>,
                Option<mpsc::UnboundedReceiver<HerdrEvent>>,
            ) = if use_herdr {
                match HerdrClient::new() {
                    Some(client) => {
                        let tracked: perla_herdr::TrackedCommands = Default::default();
                        let rx = BoardWatcher::start(client.clone(), tracked.clone());
                        let herdr = Arc::new(HerdrDispatcher {
                            client,
                            state: state.clone(),
                            tracked,
                        });
                        (
                            Arc::new(CombinedDispatcher {
                                herdr,
                                hands: hands_dispatcher,
                            }),
                            Some(rx),
                        )
                    }
                    None => (hands_dispatcher, None),
                }
            } else {
                (hands_dispatcher, None)
            };
            let dispatcher = wrap_omarchy(dispatcher, &config);
            Self::spawn_full(
                config,
                state,
                dispatcher,
                None,
                None,
                Some(pool),
                Some(hands_rx),
                herdr_rx,
            )
        }
    }

    /// Embedding seam: the host supplies its own tool dispatcher (wrapping or
    /// replacing the built-in agent backend). The voice loop, side channel,
    /// and session hardening are identical.
    pub fn start_with_dispatcher(
        config: Config,
        dispatcher: Arc<dyn ToolDispatcher>,
    ) -> (Engine, mpsc::UnboundedReceiver<EngineEvent>) {
        let state = SharedAgentState::new(
            config.workspace.to_string_lossy().into_owned(),
            config
                .recent_workspaces
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            AgentTool::from_id(&config.runtime).unwrap_or(AgentTool::Claude),
        );
        Self::spawn_full(config, state, dispatcher, None, None, None, None, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_full(
        config: Config,
        state: Arc<SharedAgentState>,
        dispatcher: Arc<dyn ToolDispatcher>,
        orchestrator: Option<Arc<AgentOrchestrator>>,
        agent_rx: Option<mpsc::UnboundedReceiver<AgentEvent>>,
        hands: Option<Arc<HandsPool>>,
        hands_rx: Option<mpsc::UnboundedReceiver<HandsEvent>>,
        herdr_rx: Option<mpsc::UnboundedReceiver<HerdrEvent>>,
    ) -> (Engine, mpsc::UnboundedReceiver<EngineEvent>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();

        // Agent events feed the same internal mailbox as everything else.
        if let Some(mut rx) = agent_rx {
            let tx = internal_tx.clone();
            tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    if tx.send(Internal::Agent(ev)).is_err() {
                        break;
                    }
                }
            });
        }
        if let Some(mut rx) = hands_rx {
            let tx = internal_tx.clone();
            tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    if tx.send(Internal::Hands(ev)).is_err() {
                        break;
                    }
                }
            });
        }
        let herdr_active = herdr_rx.is_some();
        if let Some(mut rx) = herdr_rx {
            let tx = internal_tx.clone();
            tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    if tx.send(Internal::Herdr(ev)).is_err() {
                        break;
                    }
                }
            });
        }

        let mut language = LanguageLock::new();
        language.pin_user(config.voice_language.as_deref());
        let prices = TokenPrices::for_model(config.provider, &config.active_provider().model);
        let hold = config.hold_announcements;
        let start_muted = config.start_muted;

        let mut tools = if config.mode == "desktop" {
            Vec::new()
        } else if hands.is_some() {
            hands_tools()
        } else {
            builder_tools()
        };
        if herdr_active {
            tools.extend(herdr_tools());
        }
        if config.omarchy.fast_desktop_tools {
            tools.extend(omarchy_tools());
            // Typing, key presses and clicks go straight to omarchy-harness,
            // so they work on a box without the grok CLI. omarchy_help rides
            // along: the same layer, and the answer to "how does this work".
            tools.extend(assist_tools());
        }
        let actor = Actor {
            config,
            state,
            dispatcher,
            orchestrator,
            hands,
            herdr_active,
            tools: tools.iter().map(|t| t.openai_shape()).collect(),
            events: event_tx,
            internal_tx,
            conn: None,
            conn_generation: 0,
            audio: None,
            playback: None,
            last_user_at: None,
            audio_sender: Arc::new(StdMutex::new(None)),
            status: Status::Disconnected,
            speaker: Speaker::Idle,
            muted: start_muted,
            keep_alive: false,
            reconnect_pending: false,
            reconnect_attempt: 0,
            in_flight_response: false,
            pending_tools: 0,
            awaiting_user_response: false,
            awaiting_deadline: None,
            session_connected_at: None,
            rotation_due: false,
            base_instructions: String::new(),
            language,
            side: SideChannel::default(),
            hold_announcements: hold,
            narration: Narration::new(),
            narration_cwd: None,
            transcript: Vec::new(),
            call_transcript_start: 0,
            session_usd: 0.0,
            prices,
            transient_item_ids: Vec::new(),
        };
        tokio::spawn(actor.run(cmd_rx, internal_rx));
        (Engine { commands: cmd_tx }, event_rx)
    }

    pub fn send(&self, command: EngineCommand) {
        let _ = self.commands.send(command);
    }
}

/// Everything that lands in the actor's internal mailbox.
enum Internal {
    /// A provider event from transport leg `generation` (stale legs ignored).
    Inbound {
        generation: u64,
        event: Value,
    },
    TransportClosed {
        generation: u64,
    },
    AttemptReconnect,
    PlaybackDrained(bool),
    ToolFinished {
        call_id: String,
        name: String,
        result: ToolResult,
    },
    Agent(AgentEvent),
    Hands(HandsEvent),
    Herdr(HerdrEvent),
    /// An async builder (completion announcement) staged a side-channel item.
    Speak(SideChannelItem),
}

struct Actor {
    config: Config,
    state: Arc<SharedAgentState>,
    dispatcher: Arc<dyn ToolDispatcher>,
    orchestrator: Option<Arc<AgentOrchestrator>>,
    /// Hands mode: the per-workspace grok session pool (None in agents mode).
    hands: Option<Arc<HandsPool>>,
    /// Herdr board integration is live (board tools + watcher events).
    herdr_active: bool,
    /// OpenAI-shaped tool schemas sent in every session.update.
    tools: Vec<Value>,
    events: mpsc::UnboundedSender<EngineEvent>,
    internal_tx: mpsc::UnboundedSender<Internal>,

    // ── transport ──────────────────────────────────────────────────────
    conn: Option<Connection>,
    /// Bumped per transport leg; forwarder tasks tag events with it so a dead
    /// leg's stragglers can't corrupt the fresh one.
    conn_generation: u64,
    audio: Option<AudioSystem>,
    playback: Option<perla_audio::PlaybackHandle>,
    /// When the user last said or typed anything. Drives the idle stop.
    last_user_at: Option<Instant>,
    /// The capture pipe's live target. Swapped per leg, None while offline —
    /// mic frames go straight to the socket without a trip through the actor.
    audio_sender: Arc<StdMutex<Option<mpsc::UnboundedSender<Value>>>>,

    // ── session state ──────────────────────────────────────────────────
    status: Status,
    speaker: Speaker,
    muted: bool,
    /// True from a successful Start until Stop/fatal — transient drops
    /// reconnect instead of ending the session while this holds.
    keep_alive: bool,
    reconnect_pending: bool,
    reconnect_attempt: u32,
    in_flight_response: bool,
    pending_tools: usize,
    /// True from speech_stopped until the model's answer begins — the gap
    /// guard that keeps announcements from racing the user's answer.
    awaiting_user_response: bool,
    awaiting_deadline: Option<Instant>,
    session_connected_at: Option<Instant>,
    /// The soft rotation timer elapsed; waiting for a silent gap (or the
    /// hard deadline) to swap transports.
    rotation_due: bool,

    /// Instructions WITHOUT the language clause (recap included), so a lock
    /// change can rebuild + re-send them mid-call.
    base_instructions: String,
    language: LanguageLock,
    side: SideChannel,
    hold_announcements: bool,
    narration: Narration,
    /// The project whose milestones Narration is baselining — per-project
    /// diff sets, reset on focus change.
    narration_cwd: Option<String>,
    transcript: Vec<TranscriptLine>,
    /// Where THIS call starts in the transcript, so recaps never leak an
    /// older call's lines.
    call_transcript_start: usize,
    session_usd: f64,
    prices: TokenPrices,
    /// Screenshot messages are needed for exactly one answer. Leaving them in
    /// the provider conversation makes every later turn pay for the pixels.
    transient_item_ids: Vec<String>,
}

const TRANSCRIPT_CAP: usize = 2000;

impl Actor {
    async fn run(
        mut self,
        mut cmd_rx: mpsc::UnboundedReceiver<EngineCommand>,
        mut internal_rx: mpsc::UnboundedReceiver<Internal>,
    ) {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    Some(cmd) => self.handle_command(cmd).await,
                    None => break, // every Engine handle dropped
                },
                msg = internal_rx.recv() => match msg {
                    Some(msg) => self.handle_internal(msg).await,
                    None => break,
                },
                _ = tick.tick() => self.housekeeping(),
            }
        }
        self.shutdown(None);
    }

    // ── commands ────────────────────────────────────────────────────────

    async fn handle_command(&mut self, command: EngineCommand) {
        match command {
            EngineCommand::Start => self.begin_session().await,
            EngineCommand::Stop => self.shutdown(None),
            EngineCommand::ToggleMute => self.set_muted(!self.muted),
            EngineCommand::SetMuted(on) => self.set_muted(on),
            EngineCommand::PushToTalk(down) => self.set_muted(!down),
            EngineCommand::SendText(text) => self.send_text(&text),
            EngineCommand::DeliverHeldUpdates => {
                if !self.side.is_empty() {
                    self.side.releasing_held = true;
                    self.flush_side_channel();
                }
            }
            EngineCommand::SetWorkspace(path) => self.set_workspace(path),
            EngineCommand::SetRuntime(runtime) => {
                if let Some(tool) = AgentTool::from_id(&runtime) {
                    *self.state.runtime.lock().unwrap() = tool;
                    if let Some(orch) = &self.orchestrator {
                        orch.set_launch_options(
                            tool,
                            LaunchOptions {
                                model: self.config.agent_model.clone(),
                                effort: self.config.agent_effort.clone(),
                            },
                        );
                    }
                    self.rebuild_and_repin();
                }
            }
            EngineCommand::SetDetailMode {
                on,
                big_moments_only,
            } => {
                self.state
                    .detail_mode
                    .store(on, std::sync::atomic::Ordering::Relaxed);
                self.state
                    .big_moments_only
                    .store(big_moments_only, std::sync::atomic::Ordering::Relaxed);
                if let Some(orch) = &self.orchestrator {
                    orch.set_detail_mode(on);
                }
            }
        }
    }

    async fn begin_session(&mut self) {
        if !matches!(self.status, Status::Disconnected | Status::Error) {
            return; // idempotent while connecting/connected
        }
        self.call_transcript_start = self.transcript.len();
        self.session_usd = 0.0;
        self.keep_alive = true;
        self.muted = self.config.start_muted;

        if let Err(e) = self.ensure_audio() {
            self.fail(format!("Audio devices unavailable: {e}"));
            return;
        }
        self.emit(EngineEvent::Muted(self.muted));

        match self.connect_leg(false).await {
            Ok(()) => {}
            Err(e) => self.fail(format!("Failed to start session: {e:#}")),
        }
    }

    /// One transport handshake — used by both the initial connect and every
    /// reconnect/rotation. `reconnect` picks the recap source: the stored
    /// last-call recap (fresh call) vs the live conversation (same call,
    /// fresh transport).
    async fn connect_leg(&mut self, reconnect: bool) -> anyhow::Result<()> {
        self.set_status(
            Status::Connecting,
            None,
            reconnect,
            Some(ConnectingPhase::Handshake),
        );

        // The language pin must ride the FIRST instructions of the leg, or
        // the model is free to guess from accent until a later update lands.
        self.language
            .pin_user(self.config.voice_language.as_deref());
        let mut instructions = self.build_instructions();
        if reconnect {
            // Each leg is a FRESH provider session (no prior conversation in
            // context) — feed it the running conversation so the swap is
            // seamless instead of greeting the user an hour into a call.
            if let Some(recap) = self.conversation_recap(12) {
                instructions.push_str(&format!(
                    "\n\nThis is the SAME ongoing call — the transport was refreshed mid-session. \
                     Do NOT greet the user again or reintroduce yourself; just continue naturally. \
                     Conversation so far:\n{recap}"
                ));
            }
        } else if let Some(last) = recap::stored(&self.state.workspace()) {
            instructions.push_str(&format!(
                "\n\n{last}\nUse this only for continuity — don't recite it or bring it up unprompted."
            ));
        }
        self.base_instructions = instructions;

        let resolved = self.config.active_provider();
        let provider_kind = resolved.kind;
        let settings = ProviderSettings {
            dialect: match resolved.kind {
                ProviderKind::OpenAi => Dialect::OpenAi,
                ProviderKind::Grok => Dialect::Grok,
                ProviderKind::Gemini => Dialect::Gemini,
            },
            url: resolved.url,
            api_key: resolved.api_key,
            model: resolved.model,
        };
        let mut conn = perla_provider::connect(&settings).await?;

        self.conn_generation += 1;
        let generation = self.conn_generation;
        let mut inbound = conn.take_events().expect("fresh connection");
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = inbound.recv().await {
                if tx.send(Internal::Inbound { generation, event }).is_err() {
                    return;
                }
            }
            let _ = tx.send(Internal::TransportClosed { generation });
        });

        // Point the capture pipe at the new socket.
        *self.audio_sender.lock().unwrap() = Some(conn.outbound_sender());

        self.set_status(
            Status::Connecting,
            None,
            reconnect,
            Some(ConnectingPhase::Ready),
        );

        // First update of the transport carries the audio block (voice is
        // only accepted before the model's first audio); later re-pins don't.
        let vad = pe::VadParams {
            silence_duration_ms: self.config.vad.silence_duration_ms,
            prefix_padding_ms: self.config.vad.prefix_padding_ms,
            threshold: self.config.vad.threshold,
        };
        let limits = (provider_kind == ProviderKind::OpenAi).then_some(pe::SessionLimits {
            max_output_tokens: self.config.max_output_tokens,
            context_token_limit: self.config.context_token_limit,
            retention_ratio: self.config.retention_ratio,
        });
        conn.send(pe::session_update(
            &self.pinned(),
            &self.tools,
            Some(&self.config.voice),
            &vad,
            limits.as_ref(),
        ));
        self.conn = Some(conn);

        if let Some(audio) = &self.audio {
            audio.set_muted(self.muted);
        }
        self.in_flight_response = false;
        self.pending_tools = 0;
        self.transient_item_ids.clear();
        self.speaker = Speaker::Idle;
        self.emit(EngineEvent::Speaker(self.speaker));
        self.narration.reset();
        if !reconnect {
            // A NEW call may be in a different language; mid-call swaps keep
            // the lock.
            self.language.reset();
        }
        self.side.reset();
        self.emit(EngineEvent::HeldUpdates(0));
        self.awaiting_user_response = false;
        self.awaiting_deadline = None;
        self.session_connected_at = Some(Instant::now());
        self.last_user_at.get_or_insert_with(Instant::now);
        self.rotation_due = false;
        self.reconnect_attempt = 0;
        self.set_status(Status::Connected, None, false, None);
        info!(reconnect, "voice session connected");
        Ok(())
    }

    /// Start the shared audio system once per Start; it survives transport
    /// swaps (the mic doesn't blink at the 50-minute rotation).
    fn ensure_audio(&mut self) -> anyhow::Result<()> {
        if self.audio.is_some() {
            return Ok(());
        }
        let mut audio = AudioSystem::start(perla_audio::AudioOptions {
            start_muted: self.muted,
            echo_guard: self.config.audio.echo_guard,
            barge_rms: self.config.audio.barge_rms,
            aec: self.config.audio.aec,
        })?;
        self.playback = Some(audio.playback());

        // Capture pipe: mic frames → base64 → straight onto whatever socket
        // is live. High-frequency traffic never touches the actor loop.
        let mut capture = audio.take_capture().expect("fresh audio system");
        let sender = self.audio_sender.clone();
        tokio::spawn(async move {
            while let Some(frame) = capture.recv().await {
                let target = sender.lock().unwrap().clone();
                if let Some(target) = target {
                    let mut bytes = Vec::with_capacity(frame.len() * 2);
                    for s in &frame {
                        bytes.extend_from_slice(&s.to_le_bytes());
                    }
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    let _ = target.send(pe::append_audio(&b64));
                }
            }
        });

        // Playback drained → actor (the "user genuinely stopped hearing
        // Perla" signal that gates the side channel and the speaker orb).
        let mut drained = audio.playback_drained.clone();
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            while drained.changed().await.is_ok() {
                let v = *drained.borrow();
                if tx.send(Internal::PlaybackDrained(v)).is_err() {
                    break;
                }
            }
        });

        // Mic level → UI events (already ~10Hz from the audio thread).
        let mut level = audio.mic_level.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            while level.changed().await.is_ok() {
                let v = *level.borrow();
                if events.send(EngineEvent::MicLevel(v)).is_err() {
                    break;
                }
            }
        });

        self.audio = Some(audio);
        Ok(())
    }

    /// End the session for real: persist the recap, stop reconnect/rotation,
    /// tear down transport + audio, terminate agents.
    fn shutdown(&mut self, error: Option<String>) {
        if let Some(recap) = self.conversation_recap(8) {
            recap::persist(&self.state.workspace(), &recap);
        }
        self.keep_alive = false;
        self.reconnect_pending = false;
        self.session_connected_at = None;
        self.rotation_due = false;
        *self.audio_sender.lock().unwrap() = None;
        if let Some(conn) = self.conn.take() {
            conn.close();
        }
        if let Some(mut audio) = self.audio.take() {
            audio.stop();
        }
        self.playback = None;
        if let Some(orch) = &self.orchestrator {
            orch.terminate_all();
        }
        if let Some(hands) = &self.hands {
            hands.terminate_all();
        }
        self.side.reset();
        self.emit(EngineEvent::HeldUpdates(0));
        self.in_flight_response = false;
        self.pending_tools = 0;
        self.speaker = Speaker::Idle;
        self.emit(EngineEvent::Speaker(self.speaker));
        match error {
            Some(message) => self.set_status(Status::Error, Some(message), false, None),
            None => self.set_status(Status::Disconnected, None, false, None),
        }
    }

    fn fail(&mut self, message: String) {
        warn!("voice session failed: {message}");
        self.shutdown(Some(message));
    }

    fn set_muted(&mut self, on: bool) {
        self.muted = on;
        if let Some(audio) = &self.audio {
            audio.set_muted(on);
        }
        self.emit(EngineEvent::Muted(on));
        // Unmuting: drop any stale buffered input so the next utterance
        // starts clean (port of the Swift clearAudio-on-unmute).
        if !on {
            self.send_provider(pe::clear_input_audio());
        }
    }

    fn send_text(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        // Typed input never passes through audio transcription — append it
        // to the transcript here (recaps need it too). Deliberately does NOT
        // train the language lock: typed tasks are agent commands,
        // overwhelmingly English regardless of what the user speaks.
        self.append_transcript(Role::User, trimmed.to_string());
        self.send_provider(pe::create_user_message(trimmed));
        if !self.in_flight_response {
            self.send_provider(pe::create_response(None, false));
        }
    }

    fn set_workspace(&mut self, path: PathBuf) {
        let ws = crate::config::expand_tilde(&path)
            .to_string_lossy()
            .into_owned();
        *self.state.workspace.lock().unwrap() = ws.clone();
        {
            let mut recents = self.state.recent_workspaces.lock().unwrap();
            recents.retain(|p| p != &ws);
            recents.insert(0, ws);
        }
        self.rebuild_and_repin();
    }

    // ── internal mailbox ────────────────────────────────────────────────

    async fn handle_internal(&mut self, msg: Internal) {
        match msg {
            Internal::Inbound { generation, event } => {
                if generation == self.conn_generation {
                    self.handle_provider_event(event);
                }
            }
            Internal::TransportClosed { generation } => {
                if generation != self.conn_generation {
                    return; // an old leg we already replaced
                }
                self.conn = None;
                *self.audio_sender.lock().unwrap() = None;
                if self.keep_alive {
                    debug!("transport dropped — scheduling reconnect");
                    self.schedule_reconnect(self.backoff());
                }
            }
            Internal::AttemptReconnect => {
                self.reconnect_pending = false;
                if !self.keep_alive {
                    return;
                }
                match self.connect_leg(true).await {
                    Ok(()) => {}
                    Err(e) => {
                        let text = format!("{e:#}");
                        if is_fatal_connect_error(&text) {
                            // No amount of retrying heals a bad key — surface
                            // WHY instead of an endless "Reconnecting…".
                            self.fail(text);
                        } else {
                            self.reconnect_attempt += 1;
                            let backoff = self.backoff();
                            debug!("reconnect attempt failed ({text}); retrying in {backoff:?}");
                            self.schedule_reconnect(backoff);
                        }
                    }
                }
            }
            Internal::PlaybackDrained(drained) => {
                if drained && self.speaker == Speaker::Model {
                    // Perla genuinely finished speaking (or was cut off) —
                    // THE authoritative idle flip, not response.done.
                    self.speaker = Speaker::Idle;
                    self.emit(EngineEvent::Speaker(self.speaker));
                    self.flush_side_channel();
                }
            }
            Internal::ToolFinished {
                call_id,
                name,
                result,
            } => {
                self.finish_tool(&call_id, &name, result);
            }
            Internal::Agent(event) => self.handle_agent_event(event),
            Internal::Hands(event) => self.handle_hands_event(event),
            Internal::Herdr(event) => self.handle_herdr_event(event),
            Internal::Speak(item) => {
                self.speak_side_channel(item);
            }
        }
    }

    // ── provider events ─────────────────────────────────────────────────

    fn handle_provider_event(&mut self, event: Value) {
        let kind = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match kind {
            "session.created" | "session.updated" => {}

            "input_audio_buffer.speech_started" => {
                // Barge-in, client side. The server's `interrupt_response`
                // stops generation, but audio already delivered keeps playing
                // — killing the response AND the local queue is what makes
                // "stop" actually stop. Guarded on Model specifically, NOT on
                // in_flight_response: cancelling a pure function-call
                // response would silently eat the tool call.
                if self.speaker == Speaker::Model {
                    self.send_provider(pe::cancel_response());
                    if let Some(playback) = &self.playback {
                        playback.clear();
                    }
                }
                self.speaker = Speaker::User;
                self.emit(EngineEvent::Speaker(self.speaker));
                // The user barged in — whatever we were about to volunteer waits.
                self.awaiting_user_response = false;
                self.awaiting_deadline = None;
            }

            "input_audio_buffer.speech_stopped" => {
                if self.speaker == Speaker::User {
                    self.speaker = Speaker::Idle;
                    self.emit(EngineEvent::Speaker(self.speaker));
                }
                // Hold the side channel shut until the model answers them.
                // Self-releases after 3s (a VAD false positive on a cough
                // produces no answer) so the queue can't stall forever.
                self.awaiting_user_response = true;
                self.awaiting_deadline = Some(Instant::now() + Duration::from_secs(3));
            }

            "conversation.item.input_audio_transcription.completed" => {
                if let Some(text) = event.get("transcript").and_then(|t| t.as_str()) {
                    self.append_transcript(Role::User, text.to_string());
                    if self.language.observe(text) {
                        self.repin_language();
                    }
                }
            }

            "response.created" => {
                self.in_flight_response = true;
                // The model is answering the user — the gap is closed.
                self.awaiting_user_response = false;
                self.awaiting_deadline = None;
            }

            "response.output_audio.delta" | "response.audio.delta" => {
                if let Some(b64) = event.get("delta").and_then(|d| d.as_str()) {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                        let samples: Vec<i16> = bytes
                            .chunks_exact(2)
                            .map(|c| i16::from_le_bytes([c[0], c[1]]))
                            .collect();
                        if let Some(playback) = &self.playback {
                            playback.push_pcm16(&samples);
                        }
                    }
                }
                if self.speaker != Speaker::Model {
                    self.speaker = Speaker::Model;
                    self.emit(EngineEvent::Speaker(self.speaker));
                }
            }

            "response.output_audio_transcript.done" | "response.audio_transcript.done" => {
                if let Some(text) = event.get("transcript").and_then(|t| t.as_str()) {
                    self.append_transcript(Role::Assistant, text.to_string());
                }
            }

            "response.done" => {
                let was_side_channel = self.side.busy;
                self.in_flight_response = false;
                self.side.busy = false;
                // Playback usually lags generation — the speaker flip lives
                // on PlaybackDrained, not here.
                let calls: Vec<Value> = event
                    .pointer("/response/output")
                    .and_then(|o| o.as_array())
                    .map(|items| {
                        items
                            .iter()
                            .filter(|i| {
                                i.get("type").and_then(|t| t.as_str()) == Some("function_call")
                            })
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                for call in calls {
                    self.handle_function_call(&call);
                }
                if let Some(usage) = event.pointer("/response/usage") {
                    let usd = self.prices.usd_for_usage(usage);
                    if usd > 0.0 {
                        self.session_usd += usd;
                        self.emit(EngineEvent::Cost {
                            session_usd: self.session_usd,
                        });
                    }
                }
                // Keep screenshots and proactive narration out of the rolling
                // provider context. They have already served their one turn;
                // deleting them now preserves prompt-cache locality and avoids
                // repeatedly billing image/audio history.
                for item_id in self.transient_item_ids.drain(..).collect::<Vec<_>>() {
                    self.send_provider(pe::delete_item(&item_id));
                }
                if was_side_channel {
                    let output_ids: Vec<String> = event
                        .pointer("/response/output")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
                        .filter_map(|item| item.get("id").and_then(Value::as_str))
                        .map(str::to_owned)
                        .collect();
                    for item_id in output_ids {
                        self.send_provider(pe::delete_item(&item_id));
                    }
                }
                // Now that this reply is finished, deliver any queued
                // proactive update the user is owed.
                self.flush_side_channel();
            }

            "response.cancelled" => {
                self.in_flight_response = false;
                self.side.busy = false;
                for item_id in self.transient_item_ids.drain(..).collect::<Vec<_>>() {
                    self.send_provider(pe::delete_item(&item_id));
                }
                if self.speaker == Speaker::Model {
                    self.speaker = Speaker::Idle;
                    self.emit(EngineEvent::Speaker(self.speaker));
                }
                self.flush_side_channel();
            }

            "error" => {
                let message = event
                    .pointer("/error/message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("realtime error")
                    .to_string();
                let code = event.pointer("/error/code").and_then(|c| c.as_str());
                if is_benign_realtime_error(code, &message) {
                    return; // barge-in races — not "the session is sick"
                }
                if !self.keep_alive {
                    return; // teardown noise from our own close
                }
                warn!("realtime error, reconnecting: {message}");
                if let Some(conn) = self.conn.take() {
                    conn.close();
                }
                *self.audio_sender.lock().unwrap() = None;
                self.schedule_reconnect(self.backoff());
            }

            _ => {}
        }
    }

    // ── tools ───────────────────────────────────────────────────────────

    fn handle_function_call(&mut self, item: &Value) {
        let Some(name) = item.get("name").and_then(|n| n.as_str()).map(String::from) else {
            return;
        };
        let Some(call_id) = item
            .get("call_id")
            .and_then(|c| c.as_str())
            .map(String::from)
        else {
            return;
        };
        let args: Value = item
            .get("arguments")
            .and_then(|a| a.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| json!({}));

        self.pending_tools += 1;
        self.set_status(Status::ToolRunning, None, false, None);
        self.emit(EngineEvent::AgentActivity(Some(human_activity_line(
            &name, &args,
        ))));
        self.append_transcript(Role::Tool, format!("→ {}", debug_tool_call(&name, &args)));

        // Cost lives in the session, not the dispatcher — answer inline.
        if name == "get_usage" {
            let result = ToolResult::success(json!({
                "session_cost_usd": (self.session_usd * 100.0).round() / 100.0,
                "note": "Estimated cost of this voice session so far, from provider token usage.",
            }));
            self.finish_tool(&call_id, &name, result);
            return;
        }

        // Slow agent tools get a pre-allocated history id so the out-of-band
        // completion (minutes later) can be matched back to this call.
        let history_id = if name == "run_claude_agent" || name == "run_codex" || name == "run_task"
        {
            Some(format!(
                "task-{}-{}",
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
                &uuid::Uuid::new_v4().to_string()[..4]
            ))
        } else {
            None
        };
        let ctx = ToolCallContext {
            call_id: call_id.clone(),
            history_id,
            started_at: SystemTime::now(),
        };
        let dispatcher = self.dispatcher.clone();
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let result = dispatcher.dispatch(&name, args, ctx).await;
            let _ = tx.send(Internal::ToolFinished {
                call_id,
                name,
                result,
            });
        });
    }

    fn finish_tool(&mut self, call_id: &str, name: &str, result: ToolResult) {
        self.pending_tools = self.pending_tools.saturating_sub(1);
        self.append_transcript(Role::Tool, debug_tool_result(result.ok, &result.payload));

        if name == "switch_workspace" && result.ok {
            // The dispatcher already moved SharedAgentState — refresh the
            // system prompt so the model's context names the new workspace.
            self.rebuild_and_repin();
        }
        if (name == "check_agent_session" || name == "check_task")
            && result.ok
            && self.hold_announcements
        {
            // The model just relayed the agent's status — held updates would
            // replay stale news.
            self.side.clear_queue();
            self.emit(EngineEvent::HeldUpdates(0));
        }

        self.send_provider(pe::create_function_output(call_id, &result.output_json()));
        // Pixels cannot ride inside a function result, so a `see` call lands in
        // two parts: the facts above, the picture here. It must go in before the
        // response is asked for, or the model answers without having looked.
        if let Some(data_url) = result.payload.get("__image").and_then(Value::as_str) {
            let caption = result
                .payload
                .get("__image_caption")
                .and_then(Value::as_str)
                .unwrap_or("Screenshot");
            let item_id = format!("item_{}", uuid::Uuid::new_v4().simple());
            self.send_provider(pe::create_user_image(&item_id, data_url, caption));
            self.transient_item_ids.push(item_id);
        }
        if !self.in_flight_response {
            self.send_provider(pe::create_response(None, false));
        }
        if self.pending_tools == 0 && self.status == Status::ToolRunning {
            // Only restore if the transition is still ours — a reconnect
            // mid-tool must not be stomped back to Connected.
            self.set_status(Status::Connected, None, false, None);
        }
        self.emit(EngineEvent::AgentActivity(None));
    }

    // ── agent events (out-of-band) ──────────────────────────────────────

    fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Running {
                tool, cwd, running, ..
            } => {
                self.emit(EngineEvent::AgentRunning {
                    tool: tool.label().to_string(),
                    cwd,
                    running,
                });
            }
            AgentEvent::TurnFinished {
                tool,
                cwd,
                outcome,
                context,
            } => {
                self.handle_turn_finished(tool, cwd, outcome, context.started_at);
            }
            AgentEvent::QueuedStarted { tool, prompt, .. } => {
                let prompt: String = prompt.chars().take(120).collect();
                self.speak_side_channel(SideChannelItem {
                    kind: SideChannelKind::Milestone,
                    text: format!(
                        "[live agent status] the queued task just started on {}: {prompt}",
                        tool.label()
                    ),
                    instructions: Some(
                        "In one short casual spoken sentence tell the user the queued task is now starting."
                            .into(),
                    ),
                    facts: Vec::new(),
                });
            }
            AgentEvent::NeedsAttention { tool, cwd, message } => {
                let folder = leaf(&cwd);
                let message: String = message.chars().take(300).collect();
                self.speak_side_channel(SideChannelItem {
                    kind: SideChannelKind::Completion,
                    text: format!("[agent needs attention] {} in {folder} says: {message}", tool.label()),
                    instructions: Some(
                        "The coding agent is waiting on the user. In one short spoken sentence relay \
                         what it needs — name the project if the user has several running. If the user \
                         answers, pass it along with steer_agent."
                            .into(),
                    ),
                    facts: Vec::new(),
                });
            }
            AgentEvent::Progress {
                cwd,
                digest,
                elapsed_secs,
                ..
            } => self.narrate_progress(cwd, digest, elapsed_secs),
        }
    }

    /// Live-digest narration, shared by agents and hands mode.
    fn narrate_progress(
        &mut self,
        cwd: String,
        digest: perla_agents::AgentDigest,
        elapsed_secs: f64,
    ) {
        // FOCUSED project only: interleaving several sessions' step
        // diffs through one Narration baseline would be noise.
        // Background projects still announce completions by name.
        if normalize_cwd(&cwd) != normalize_cwd(&self.state.workspace()) {
            return;
        }
        // Hold mode does NOT bail here — ingest still advances the
        // milestone baseline (no flood of stale "just finished" lines
        // if hold is switched off mid-turn). Truthfulness is safe:
        // facts are logged as spoken at flush time, and hold drops
        // milestones before they reach the queue.
        if self.narration_cwd.as_deref() != Some(cwd.as_str()) {
            self.narration.reset();
            self.side.spoken_facts.clear();
            self.narration_cwd = Some(cwd.clone());
        }
        let enabled = self
            .state
            .detail_mode
            .load(std::sync::atomic::Ordering::Relaxed);
        let big = self
            .state
            .big_moments_only
            .load(std::sync::atomic::Ordering::Relaxed);
        if self.narration.ingest(&digest, elapsed_secs, enabled, big) {
            if let Some(u) = self.narration.drain() {
                self.speak_side_channel(SideChannelItem {
                    kind: SideChannelKind::Milestone,
                    text: u.text,
                    instructions: Some(u.instructions),
                    facts: u.facts,
                });
            }
        }
    }

    // ── hands events (out-of-band) ──────────────────────────────────────

    fn handle_hands_event(&mut self, event: HandsEvent) {
        match event {
            HandsEvent::Running { cwd, running, .. } => {
                self.emit(EngineEvent::AgentRunning {
                    tool: "hands".to_string(),
                    cwd,
                    running,
                });
            }
            HandsEvent::QueuedStarted { prompt, .. } => {
                let prompt: String = prompt.chars().take(120).collect();
                self.speak_side_channel(SideChannelItem {
                    kind: SideChannelKind::Milestone,
                    text: format!("[live agent status] the queued task just started: {prompt}"),
                    instructions: Some(
                        "In one short casual spoken sentence tell the user the queued task is now starting."
                            .into(),
                    ),
                    facts: Vec::new(),
                });
            }
            HandsEvent::Progress {
                cwd,
                digest,
                elapsed_secs,
            } => self.narrate_progress(cwd, digest, elapsed_secs),
            HandsEvent::TurnFinished {
                cwd,
                outcome,
                changed_files,
                ..
            } => self.handle_hands_turn_finished(cwd, outcome, changed_files),
        }
    }

    /// Hands completion — same choreography as the agents version, but the
    /// changed files arrive first-class in the event (the protocol reported
    /// each edit), so there's no transcript re-digest or mtime filtering.
    fn handle_hands_turn_finished(
        &mut self,
        cwd: String,
        outcome: TurnOutcome,
        changed_files: Vec<String>,
    ) {
        let mark = if outcome.ok { "✓" } else { "✗" };
        let short: String = outcome.summary.chars().take(160).collect();
        self.append_transcript(Role::Tool, format!("{mark} {short}"));

        let mut already_said: Vec<String> = Vec::new();
        if self
            .narration_cwd
            .as_deref()
            .map(|n| normalize_cwd(n) == normalize_cwd(&cwd))
            .unwrap_or(false)
        {
            already_said = std::mem::take(&mut self.side.spoken_facts);
            self.narration.reset();
        }

        let folder = leaf(&cwd);
        if outcome.interrupted {
            self.send_provider(pe::create_system_message(&format!(
                "[live agent status] The user stopped the running task in {folder} — nothing is \
                 running there now. Do not announce this; just know it if the user asks or gives \
                 a new task."
            )));
            return;
        }

        let summary: String = outcome.summary.chars().take(600).collect();
        let names: Vec<String> = changed_files
            .iter()
            .rev()
            .take(6)
            .rev()
            .map(|p| leaf(p))
            .collect();
        let files_line = if names.is_empty() {
            String::new()
        } else {
            let more = if changed_files.len() > 6 {
                format!(" and {} more", changed_files.len() - 6)
            } else {
                String::new()
            };
            format!(" Files changed: {}{more}.", names.join(", "))
        };
        let said_line = if already_said.is_empty() {
            String::new()
        } else {
            format!(
                " You ALREADY told the user, as it happened: {}. Do not say any of that again.",
                already_said.join("; ")
            )
        };
        let verdict = if outcome.ok { "finished" } else { "stopped" };
        self.speak_side_channel(SideChannelItem {
            kind: SideChannelKind::Completion,
            text: format!("The task in {folder} just {verdict}. Result: {summary}.{files_line}"),
            instructions: Some(format!(
                "The work just ended. Say ONE short spoken sentence: the headline outcome — did \
                 it work, did the build or tests pass, is anything broken or waiting on them — \
                 and then OFFER the details, e.g. \"want me to walk you through it?\". Do NOT \
                 list the steps, do not enumerate changed files, and never read the raw summary \
                 aloud. Name the project only if several are running. If the user then asks for \
                 the details, THAT is when you explain what was done, drawing on the result note \
                 in this conversation.{said_line}"
            )),
            facts: Vec::new(),
        });
    }

    /// Out-of-band completion for a voice-submitted agent turn — possibly
    /// minutes after the fast-ack. On genuine completion, nudges the model to
    /// tell the user what got done. A user stop comes through ok=false and
    /// stays silent — the user already knows.
    fn handle_turn_finished(
        &mut self,
        tool: AgentTool,
        cwd: String,
        outcome: TurnOutcome,
        started_at: Instant,
    ) {
        let mark = if outcome.ok { "✓" } else { "✗" };
        let short: String = outcome.summary.chars().take(160).collect();
        self.append_transcript(Role::Tool, format!("{mark} {short}"));

        // Narration state is per-TURN. Capture what was actually SPOKEN this
        // turn (the completion is told not to repeat it), then clear the
        // baseline — or a later turn's identically worded to-do is silently
        // swallowed as already-announced. Only for the baselined project.
        let mut already_said: Vec<String> = Vec::new();
        if self
            .narration_cwd
            .as_deref()
            .map(|n| normalize_cwd(n) == normalize_cwd(&cwd))
            .unwrap_or(false)
        {
            already_said = std::mem::take(&mut self.side.spoken_facts);
            self.narration.reset();
        }

        // The user killed the turn themselves — no speech, but the model
        // still has a dangling "task running" in its context.
        if outcome.interrupted {
            let folder = leaf(&cwd);
            self.send_provider(pe::create_system_message(&format!(
                "[live agent status] The user manually interrupted the {} task in {folder} — it is \
                 stopped now, nothing is running there. Do not announce this; just know it if the \
                 user asks or gives a new task.",
                tool.label()
            )));
            return;
        }
        if !outcome.ok {
            return;
        }

        // Fold in what the turn actually touched. The digest read is file
        // I/O — off the actor. The digest tail spans earlier turns too, so
        // keep only files whose mtime falls inside THIS turn (small slack).
        let summary: String = outcome.summary.chars().take(600).collect();
        let turn_started = SystemTime::now() - started_at.elapsed() - Duration::from_secs(5);
        let tx = self.internal_tx.clone();
        let cwd_for_digest = cwd.clone();
        tokio::spawn(async move {
            let files: Vec<String> = tokio::task::spawn_blocking(move || {
                digest::digest(tool, &cwd_for_digest)
                    .map(|d| d.changed_files)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|path| {
                        std::fs::metadata(path)
                            .and_then(|m| m.modified())
                            .map(|mtime| mtime >= turn_started)
                            .unwrap_or(false)
                    })
                    .collect()
            })
            .await
            .unwrap_or_default();

            let names: Vec<String> = files.iter().rev().take(6).rev().map(|p| leaf(p)).collect();
            let files_line = if names.is_empty() {
                String::new()
            } else {
                let more = if files.len() > 6 {
                    format!(" and {} more", files.len() - 6)
                } else {
                    String::new()
                };
                format!(" Files changed: {}{more}.", names.join(", "))
            };
            let folder = leaf(&cwd);
            // The milestones already narrated WHAT got done as it happened.
            // This line's job is the VERDICT plus an OFFER.
            let said_line = if already_said.is_empty() {
                String::new()
            } else {
                format!(
                    " You ALREADY told the user, as it happened: {}. Do not say any of that again.",
                    already_said.join("; ")
                )
            };
            let _ = tx.send(Internal::Speak(SideChannelItem {
                kind: SideChannelKind::Completion,
                text: format!(
                    "The {} task in {folder} just finished. Result: {summary}.{files_line}",
                    tool.label()
                ),
                instructions: Some(format!(
                    "The agent's turn just ended. Say ONE short spoken sentence: the headline \
                     outcome — did it work, did the build or tests pass, is anything broken or \
                     waiting on them — and then OFFER the details, e.g. \"want me to walk you \
                     through it?\". Do NOT list the steps, do not enumerate changed files, and \
                     never read the raw summary aloud. Name the project only if several are \
                     running. If the user then asks for the details, THAT is when you explain \
                     what was done, drawing on the result note in this conversation.{said_line}"
                )),
                facts: Vec::new(),
            }));
        });
    }

    // ── herdr board events ──────────────────────────────────────────────

    /// State changes anywhere on the board. Blocked and finished are news
    /// the user is owed (spoken); appear/vanish/start are silent context so
    /// the model's picture of the board stays current.
    fn handle_herdr_event(&mut self, event: HerdrEvent) {
        match event {
            HerdrEvent::AgentStatus {
                target,
                kind,
                workspace,
                from,
                to,
                title,
                ..
            } => {
                let task = title
                    .filter(|t| !t.is_empty())
                    .map(|t| {
                        let t: String = t.chars().take(120).collect();
                        format!(" (task: {t})")
                    })
                    .unwrap_or_default();
                self.emit(EngineEvent::AgentRunning {
                    tool: kind.clone(),
                    cwd: workspace.clone(),
                    running: to == "working",
                });
                if to == "blocked" {
                    self.speak_side_channel(SideChannelItem {
                        kind: SideChannelKind::Completion,
                        text: format!(
                            "[board] {kind} '{target}' in {workspace} is BLOCKED — waiting on input{task}."
                        ),
                        instructions: Some(
                            "An agent on the board is waiting on the user. In one short spoken \
                             sentence say which agent and what it seems to need (use read_pane \
                             first if you need the exact question). Offer to pass their answer \
                             along with steer_agent."
                                .into(),
                        ),
                        facts: Vec::new(),
                    });
                } else if from == "working" && (to == "idle" || to == "done") {
                    self.speak_side_channel(SideChannelItem {
                        kind: SideChannelKind::Completion,
                        text: format!(
                            "[board] {kind} '{target}' in {workspace} just finished working{task}."
                        ),
                        instructions: Some(
                            "An agent on the board finished its work. ONE short spoken sentence: \
                             which agent, the headline, and offer details (read_pane has its \
                             output if the user asks). Name the workspace only if several are \
                             active."
                                .into(),
                        ),
                        facts: Vec::new(),
                    });
                } else {
                    // Started working / unknown flips: context, not speech.
                    self.send_provider(pe::create_system_message(&format!(
                        "[board] {kind} '{target}' in {workspace} is now {to}{task}. Do not \
                         announce this; just know it."
                    )));
                }
                self.append_transcript(
                    Role::Tool,
                    format!("· board: {kind} {target} {from}→{to} in {workspace}"),
                );
            }
            HerdrEvent::AgentAppeared {
                target,
                kind,
                workspace,
            } => {
                self.send_provider(pe::create_system_message(&format!(
                    "[board] a {kind} agent ('{target}') appeared in {workspace}. If you didn't \
                     start it, the user did — you can steer/stop/read it by that name. Don't \
                     announce this."
                )));
                self.append_transcript(Role::Tool, format!("· board: + {kind} {target} in {workspace}"));
            }
            HerdrEvent::AgentGone {
                target,
                kind,
                workspace,
            } => {
                self.send_provider(pe::create_system_message(&format!(
                    "[board] the {kind} agent '{target}' in {workspace} is gone (pane closed). \
                     Don't announce this."
                )));
                self.append_transcript(Role::Tool, format!("· board: - {kind} {target} in {workspace}"));
            }
            HerdrEvent::CommandFinished {
                pane_id,
                label,
                command,
                exit_code,
                tail,
            } => {
                let mark = if exit_code == 0 { "✓" } else { "✗" };
                self.append_transcript(
                    Role::Tool,
                    format!("{mark} command tab '{label}' exited ({exit_code})"),
                );
                if exit_code == 0 {
                    self.speak_side_channel(SideChannelItem {
                        kind: SideChannelKind::Milestone,
                        text: format!(
                            "[board] the '{label}' command tab finished successfully (`{command}`, exit 0). Last output:\n{tail}"
                        ),
                        instructions: Some(
                            "A command the user could see just finished cleanly. ONE short spoken \
                             sentence — which tab and that it succeeded. Mention a headline from \
                             the output only if it matters (e.g. test counts)."
                                .into(),
                        ),
                        facts: Vec::new(),
                    });
                } else {
                    self.speak_side_channel(SideChannelItem {
                        kind: SideChannelKind::Completion,
                        text: format!(
                            "[board] the '{label}' command tab FAILED (`{command}`, exit {exit_code}). Pane {pane_id}. Last output:\n{tail}"
                        ),
                        instructions: Some(
                            "A command tab just died or failed. ONE short spoken sentence: which \
                             tab, that it failed, and the likely cause from the output (e.g. \
                             'port already in use', 'two tests failing'). Offer to dig in or fix \
                             it — your hands can."
                                .into(),
                        ),
                        facts: Vec::new(),
                    });
                }
            }
        }
    }

    // ── side channel ────────────────────────────────────────────────────

    fn speak_side_channel(&mut self, item: SideChannelItem) {
        if self.side.stage(item, self.hold_announcements) {
            self.flush_side_channel();
        }
    }

    fn flush_side_channel(&mut self) {
        if self.side.busy || self.in_flight_response {
            return;
        }
        // Never talk over the user, and never jump the gap between their
        // utterance ending and the model answering it. Both re-trigger a
        // flush later: response.done, or the awaiting-guard's own timeout.
        if self.speaker == Speaker::User || self.awaiting_user_response {
            return;
        }
        if self.side.is_empty() {
            self.side.releasing_held = false; // drained — hold gate re-engages
            self.emit(EngineEvent::HeldUpdates(0));
            return;
        }
        // Hold mode: don't volunteer anything — the queue waits behind the
        // "updates ready" signal until DeliverHeldUpdates releases it.
        if self.hold_announcements && !self.side.releasing_held {
            self.emit(EngineEvent::HeldUpdates(self.side.len()));
            return;
        }
        let Some(item) = self.side.pop() else { return };
        self.emit(EngineEvent::HeldUpdates(if self.side.releasing_held {
            self.side.len()
        } else {
            0
        }));
        self.side.busy = true;
        // This is the moment the milestone is genuinely spoken — only now do
        // its facts count as "already told the user".
        if item.kind == SideChannelKind::Milestone {
            self.side.spoken_facts.extend(item.facts.iter().cloned());
        }
        self.append_transcript(Role::Tool, format!("· {}", item.text));
        // `response.instructions` REPLACES the session instructions for this
        // response — the language rule has to ride along, or Perla answers in
        // the user's language and then announces in English.
        let instructions = match &item.instructions {
            Some(base) => format!(
                "{base}\n\nLive update to relay:\n{}\n\n{}",
                item.text,
                self.language.clause()
            ),
            None => format!(
                "In one short spoken sentence relay this live update:\n{}\n\n{}",
                item.text,
                self.language.clause()
            ),
        };
        self.send_provider(pe::create_response(Some(&instructions), true));
    }

    // ── reconnect + rotation ────────────────────────────────────────────

    fn backoff(&self) -> Duration {
        // 1, 2, 4, 8, 8, … seconds. Transport trouble retries forever — only
        // the user (or a fatal) ends it.
        Duration::from_secs_f64(f64::min(
            8.0,
            2f64.powi(self.reconnect_attempt.min(3) as i32),
        ))
    }

    fn schedule_reconnect(&mut self, delay: Duration) {
        if !self.keep_alive || self.reconnect_pending {
            return;
        }
        self.reconnect_pending = true;
        if let Some(conn) = self.conn.take() {
            conn.close();
        }
        *self.audio_sender.lock().unwrap() = None;
        if let Some(playback) = &self.playback {
            playback.clear();
        }
        self.speaker = Speaker::Idle;
        self.emit(EngineEvent::Speaker(self.speaker));
        self.in_flight_response = false;
        self.side.busy = false;
        self.set_status(
            Status::Connecting,
            None,
            true,
            Some(ConnectingPhase::Handshake),
        );
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(Internal::AttemptReconnect);
        });
    }

    fn housekeeping(&mut self) {
        // The awaiting-user gap guard self-releases if no response arrives
        // (a VAD false positive produces no answer) so the queue can't stall.
        if self.awaiting_user_response {
            if let Some(deadline) = self.awaiting_deadline {
                if Instant::now() >= deadline {
                    self.awaiting_user_response = false;
                    self.awaiting_deadline = None;
                    self.flush_side_channel();
                }
            }
        }

        // Idle stop. Muting would still hold the session open; ending it is
        // what actually stops the meter. Only while nothing is happening, so a
        // long tool run or a reply in progress is never cut off.
        if self.config.idle_stop_secs > 0
            && self.status == Status::Connected
            && self.speaker == Speaker::Idle
            && !self.in_flight_response
            && self.pending_tools == 0
            && !self.side.busy
        {
            if let Some(at) = self.last_user_at {
                if at.elapsed().as_secs() >= self.config.idle_stop_secs {
                    let mins = self.config.idle_stop_secs / 60;
                    info!(idle_secs = self.config.idle_stop_secs, "stopping idle session");
                    self.append_transcript(
                        Role::Tool,
                        format!("· session ended after {mins} min idle (say the word to start again)"),
                    );
                    self.last_user_at = None;
                    self.shutdown(None);
                    return;
                }
            }
        }

        // Proactive rotation: the provider caps a session at ~60 min. Swap to
        // a fresh one before that, during a silent gap so the ~1s handshake
        // is inaudible — but never past the hard deadline (+8s).
        if self.status == Status::Connected && self.keep_alive && self.conn.is_some() {
            if let Some(at) = self.session_connected_at {
                let elapsed = at.elapsed().as_secs();
                if elapsed >= self.config.rotate_after_secs {
                    self.rotation_due = true;
                }
                if self.rotation_due {
                    let silent = self.speaker == Speaker::Idle
                        && !self.in_flight_response
                        && !self.side.busy;
                    let hard = elapsed >= self.config.rotate_after_secs + 8;
                    if silent || hard {
                        info!(elapsed, "rotating to a fresh realtime session");
                        self.rotation_due = false;
                        self.session_connected_at = None;
                        self.schedule_reconnect(Duration::ZERO);
                    }
                }
            }
        }
    }

    // ── instructions / language ─────────────────────────────────────────

    fn build_instructions(&self) -> String {
        let workspace = self.state.workspace();
        let runtime = self.state.runtime();
        let recents = self.state.recent_workspaces.lock().unwrap().clone();
        if self.config.mode == "desktop" {
            return build_desktop_instructions(&PromptContext {
                workspace: &workspace,
                runtime: "desktop",
                model: None,
                recent_workspaces: &recents,
            });
        }
        if self.hands.is_some() {
            let mut base = build_hands_instructions(&PromptContext {
                workspace: &workspace,
                runtime: "hands",
                model: self.config.hands_model.as_deref(),
                recent_workspaces: &recents,
            });
            if self.herdr_active {
                base.push_str(build_board_clause());
            }
            base
        } else {
            build_system_instructions(&PromptContext {
                workspace: &workspace,
                runtime: runtime.id(),
                model: self.config.agent_model.as_deref(),
                recent_workspaces: &recents,
            })
        }
    }

    fn pinned(&self) -> String {
        format!("{}\n\n{}", self.base_instructions, self.language.clause())
    }

    /// The lock moved (or the workspace/runtime changed) — re-send the
    /// session instructions so ordinary turns follow too, not just the
    /// side-channel responses. Mid-call updates never carry `voice`.
    fn repin_language(&mut self) {
        let vad = pe::VadParams {
            silence_duration_ms: self.config.vad.silence_duration_ms,
            prefix_padding_ms: self.config.vad.prefix_padding_ms,
            threshold: self.config.vad.threshold,
        };
        let limits = (self.config.provider == ProviderKind::OpenAi).then_some(pe::SessionLimits {
            max_output_tokens: self.config.max_output_tokens,
            context_token_limit: self.config.context_token_limit,
            retention_ratio: self.config.retention_ratio,
        });
        let update = pe::session_update(
            &self.pinned(),
            &self.tools,
            None,
            &vad,
            limits.as_ref(),
        );
        self.send_provider(update);
    }

    fn rebuild_and_repin(&mut self) {
        self.base_instructions = self.build_instructions();
        if matches!(self.status, Status::Connected | Status::ToolRunning) {
            self.repin_language();
        }
    }

    // ── recap ───────────────────────────────────────────────────────────

    /// The last few user/assistant exchanges of THIS call, formatted for
    /// instruction injection. None when nothing has been said yet.
    fn conversation_recap(&self, max_lines: usize) -> Option<String> {
        let start = self.call_transcript_start.min(self.transcript.len());
        let lines: Vec<String> = self.transcript[start..]
            .iter()
            .filter(|l| matches!(l.role, Role::User | Role::Assistant))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .take(max_lines)
            .rev()
            .map(|l| {
                let who = if l.role == Role::User { "User" } else { "You" };
                let text: String = if l.text.chars().count() > 200 {
                    l.text.chars().take(200).collect::<String>() + "…"
                } else {
                    l.text.clone()
                };
                format!("{who}: {text}")
            })
            .collect();
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }

    // ── plumbing ────────────────────────────────────────────────────────

    fn send_provider(&mut self, event: Value) {
        if let Some(conn) = &self.conn {
            if !conn.send(event) {
                // The writer is gone — treat as a transport drop; the reader
                // forwarder will send TransportClosed shortly too.
                self.conn = None;
            }
        }
    }

    fn append_transcript(&mut self, role: Role, text: String) {
        // One place to notice the user, whether they typed or spoke.
        if role == Role::User {
            self.last_user_at = Some(Instant::now());
        }
        let line = TranscriptLine {
            role,
            text,
            at: SystemTime::now(),
        };
        self.transcript.push(line.clone());
        // Debug/recap plumbing, not chat history — cap it, shifting the
        // call-start index so recaps are unaffected by trimming.
        let overflow = self.transcript.len().saturating_sub(TRANSCRIPT_CAP);
        if overflow > 0 {
            self.transcript.drain(..overflow);
            self.call_transcript_start = self.call_transcript_start.saturating_sub(overflow);
        }
        self.emit(EngineEvent::Transcript(line));
    }

    fn set_status(
        &mut self,
        status: Status,
        error: Option<String>,
        reconnecting: bool,
        phase: Option<ConnectingPhase>,
    ) {
        self.status = status;
        self.emit(EngineEvent::Status {
            status,
            error,
            reconnecting,
            phase,
        });
    }

    fn emit(&self, event: EngineEvent) {
        let _ = self.events.send(event);
    }
}

// ── helpers ─────────────────────────────────────────────────────────────

fn wrap_omarchy(
    inner: Arc<dyn ToolDispatcher>,
    config: &crate::config::Config,
) -> Arc<dyn ToolDispatcher> {
    if !config.omarchy.fast_desktop_tools {
        return inner;
    }
    let omarchy = Arc::new(LayeredDispatcher {
        omarchy: Arc::new(OmarchyDispatcher::system(
            config.omarchy.confirm_destructive,
        )),
        inner,
    });
    Arc::new(AssistLayer {
        assist: Arc::new(AssistDispatcher::system()),
        inner: omarchy,
    })
}

/// Desktop mode has no agent behind it: the omarchy and assist layers wrap this
/// and answer everything they own, so anything reaching here is a tool that was
/// never offered.
struct NoAgentDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for NoAgentDispatcher {
    async fn dispatch(&self, name: &str, _args: Value, _ctx: ToolCallContext) -> ToolResult {
        ToolResult::error(format!(
            "'{name}' is not available: Perla is running in desktop mode with no coding agent. \
             Say so plainly — do not blame a website, a login, or a missing permission."
        ))
    }
}

/// Routes board tools to the herdr dispatcher, everything else to the hands.
struct CombinedDispatcher {
    herdr: Arc<HerdrDispatcher>,
    hands: Arc<HandsDispatcher>,
}

#[async_trait::async_trait]
impl ToolDispatcher for CombinedDispatcher {
    async fn dispatch(&self, name: &str, args: Value, ctx: ToolCallContext) -> ToolResult {
        if perla_herdr::dispatcher::HERDR_TOOLS.contains(&name) {
            self.herdr.dispatch(name, args, ctx).await
        } else {
            self.hands.dispatch(name, args, ctx).await
        }
    }
}

fn leaf(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_string()
}

/// Matched on code first, message second — the codes have been renamed
/// before; the wording is the more stable signal in practice.
fn is_benign_realtime_error(code: Option<&str>, message: &str) -> bool {
    if let Some(code) = code {
        if [
            "response_cancel_not_active",
            "output_audio_buffer_clear_not_active",
            "input_audio_buffer_commit_empty",
        ]
        .contains(&code)
        {
            return true;
        }
    }
    let m = message.to_lowercase();
    m.contains("no active response")
        || m.contains("cancellation failed")
        || m.contains("buffer is empty")
        || m.contains("already empty")
}

/// Auth/key trouble no retry can heal; everything else is transport noise
/// worth retrying forever.
fn is_fatal_connect_error(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("no api key")
        || t.contains("401")
        || t.contains("403")
        || t.contains("unauthorized")
        || t.contains("forbidden")
}

/// Human-readable activity line for UIs. Tool names like `run_claude_agent`
/// are an implementation detail the user shouldn't see.
fn human_activity_line(name: &str, args: &Value) -> String {
    let target: Option<String> = args
        .get("path")
        .and_then(|p| p.as_str())
        .filter(|p| !p.is_empty())
        .map(leaf)
        .or_else(|| {
            args.get("task")
                .and_then(|t| t.as_str())
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(|t| {
                    if t.chars().count() > 36 {
                        t.chars().take(36).collect::<String>() + "…"
                    } else {
                        t.to_string()
                    }
                })
        });
    match name {
        "run_task" => target
            .map(|t| format!("Working on {t}"))
            .unwrap_or_else(|| "Working".into()),
        "run_claude_agent" => target
            .map(|t| format!("Working on {t}"))
            .unwrap_or_else(|| "Working with Claude".into()),
        "run_codex" => target
            .map(|t| format!("Working on {t}"))
            .unwrap_or_else(|| "Working with Codex".into()),
        "read_file" => target
            .map(|t| format!("Reading {t}"))
            .unwrap_or_else(|| "Reading file".into()),
        "list_dir" => target
            .map(|t| format!("Listing {t}"))
            .unwrap_or_else(|| "Listing directory".into()),
        "open_in_editor" => target
            .map(|t| format!("Opening {t}"))
            .unwrap_or_else(|| "Opening in editor".into()),
        other => {
            let words = other.replace('_', " ");
            let mut c = words.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => words,
            }
        }
    }
}

fn debug_tool_call(name: &str, args: &Value) -> String {
    if let Some(t) = args
        .get("task")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
    {
        let t: String = t.chars().take(100).collect();
        return format!("{name} · {t}");
    }
    if let Some(p) = args
        .get("path")
        .and_then(|p| p.as_str())
        .filter(|p| !p.is_empty())
    {
        return format!("{name} · {p}");
    }
    match args.as_object() {
        Some(map) if !map.is_empty() => {
            let mut kv: Vec<String> = map.iter().map(|(k, v)| format!("{k}={v}")).collect();
            kv.sort();
            let joined: String = kv.join(", ").chars().take(100).collect();
            format!("{name} · {joined}")
        }
        _ => name.to_string(),
    }
}

fn debug_tool_result(ok: bool, payload: &serde_json::Map<String, Value>) -> String {
    let mark = if ok { "✓" } else { "✗" };
    let take = |key: &str| {
        payload
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(160).collect::<String>())
    };
    if let Some(e) = take("error") {
        return format!("{mark} {e}");
    }
    if let Some(s) = take("summary") {
        return format!("{mark} {s}");
    }
    if let Some(s) = take("todo_summary") {
        return format!("{mark} {s}");
    }
    if let Some(n) = take("note") {
        return format!("{mark} {n}");
    }
    let json = serde_json::to_string(payload).unwrap_or_else(|_| "done".into());
    let json: String = json.chars().take(160).collect();
    format!("{mark} {json}")
}
