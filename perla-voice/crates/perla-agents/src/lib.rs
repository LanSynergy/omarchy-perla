//! Coding-agent backend for perla-voice.
//!
//! Everything Perla knows about driving local coding agents (Claude Code,
//! Codex): spawning them in hidden PTYs, tailing their JSONL transcripts for
//! turn-end / interrupt / progress, the fast-ack submit + dedup + queue
//! orchestration, the Claude hook server, and the narration milestone engine.
//!
//! Hosts that bring their OWN agent (a coding harness, a CMS backend) skip
//! this crate's CLI backend and implement [`backend::AgentBackend`] instead.

pub mod backend;
pub mod digest;
pub mod dispatcher;
pub mod hooks;
pub mod narration;
pub mod orchestrator;
pub mod paths;
pub mod pty;
pub mod transcripts;
pub mod types;

pub use backend::AgentBackend;
pub use digest::AgentDigest;
pub use dispatcher::{AgentDispatcher, SharedAgentState};
pub use narration::Narration;
pub use orchestrator::{AgentEvent, AgentOrchestrator, AgentRunContext, SubmitOutcome};
pub use transcripts::TurnOutcome;
pub use types::AgentTool;
