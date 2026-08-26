//! The engine's public event/command surface. Any UI (TUI, tray, web view,
//! host app) renders from `EngineEvent` and drives via `EngineCommand` —
//! this is the embedding contract.

use std::path::PathBuf;
use std::time::SystemTime;

use serde::Serialize;

/// Port of `RealtimeStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Disconnected,
    Connecting,
    Connected,
    ToolRunning,
    Error,
}

/// Port of `RealtimeSpeaker` — who is talking right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    Idle,
    User,
    Model,
}

/// Sub-phases of `Connecting`, so a UI can show progress instead of a lone
/// spinner. Port of `ConnectingPhase`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectingPhase {
    Handshake,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TranscriptLine {
    pub role: Role,
    pub text: String,
    #[serde(skip)]
    pub at: SystemTime,
}

/// Everything the engine tells the outside world.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineEvent {
    Status {
        status: Status,
        /// Set when status == Error.
        error: Option<String>,
        /// True while transparently bringing the voice link back.
        reconnecting: bool,
        phase: Option<ConnectingPhase>,
    },
    Speaker(Speaker),
    Muted(bool),
    Transcript(TranscriptLine),
    /// Human-readable "Working on foo.rs" line while a tool call runs.
    AgentActivity(Option<String>),
    /// An agent run began or ended in some workspace.
    AgentRunning {
        tool: String,
        cwd: String,
        running: bool,
    },
    /// Accumulated session cost estimate in USD (0 when pricing unknown).
    Cost {
        session_usd: f64,
    },
    /// Updates queued behind hold mode, waiting for the user.
    HeldUpdates(usize),
    /// Mic input level 0..=1 for meters/orbs. Throttled.
    MicLevel(f32),
}

/// Everything the outside world can ask the engine to do.
#[derive(Debug, Clone)]
pub enum EngineCommand {
    /// Connect the voice session (idempotent while connecting/connected).
    Start,
    /// End the session for real — stops reconnect/rotation and agents.
    Stop,
    ToggleMute,
    SetMuted(bool),
    /// Push-to-talk: mic hot while true.
    PushToTalk(bool),
    /// Typed task — injected as a user message (never trains language lock).
    SendText(String),
    /// The user asked to hear updates held behind hold mode.
    DeliverHeldUpdates,
    SetWorkspace(PathBuf),
    /// "claude" | "codex"
    SetRuntime(String),
    SetDetailMode {
        on: bool,
        big_moments_only: bool,
    },
}
