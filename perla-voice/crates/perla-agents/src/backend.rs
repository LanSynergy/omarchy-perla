//! The embeddability seam: "the agent" behind the voice orchestrator can be
//! the built-in CLI backend (Claude Code / Codex in hidden PTYs) or anything
//! the host provides — their own coding harness, a CMS job runner, a remote
//! worker. Implement this trait and hand it to your dispatcher.

use async_trait::async_trait;
use std::sync::Arc;

use crate::digest::AgentDigest;
use crate::orchestrator::{AgentOrchestrator, AgentRunContext, SubmitOutcome};
use crate::types::AgentTool;

#[async_trait]
pub trait AgentBackend: Send + Sync {
    /// Human name for narration ("Claude Code", "the build agent").
    fn label(&self) -> String;

    /// Fast-ack submit: hand the prompt over and return immediately.
    /// Completion must be reported out-of-band (however the implementation
    /// signals it — the built-in one emits `AgentEvent::TurnFinished`).
    async fn submit(&self, prompt: &str, cwd: &str, context: AgentRunContext) -> SubmitOutcome;

    /// Halt the current turn but keep the session alive. False = nothing ran.
    async fn interrupt(&self, cwd: &str) -> bool;

    /// Inject a mid-run instruction without starting a new task.
    async fn steer(&self, cwd: &str, message: &str) -> bool;

    /// Read-only snapshot of the current session for status questions.
    async fn digest(&self, cwd: &str) -> Option<AgentDigest>;

    fn is_running(&self, cwd: &str) -> bool;
}

/// The built-in backend: one CLI tool driven through the orchestrator.
pub struct CliAgentBackend {
    pub tool: AgentTool,
    pub orchestrator: Arc<AgentOrchestrator>,
}

#[async_trait]
impl AgentBackend for CliAgentBackend {
    fn label(&self) -> String {
        self.tool.label().to_string()
    }

    async fn submit(&self, prompt: &str, cwd: &str, context: AgentRunContext) -> SubmitOutcome {
        self.orchestrator
            .submit(self.tool, prompt, cwd, context)
            .await
    }

    async fn interrupt(&self, cwd: &str) -> bool {
        self.orchestrator.interrupt(self.tool, cwd)
    }

    async fn steer(&self, cwd: &str, message: &str) -> bool {
        self.orchestrator.steer(self.tool, cwd, message)
    }

    async fn digest(&self, cwd: &str) -> Option<AgentDigest> {
        let tool = self.tool;
        let cwd = cwd.to_string();
        tokio::task::spawn_blocking(move || crate::digest::digest(tool, &cwd))
            .await
            .ok()
            .flatten()
    }

    fn is_running(&self, cwd: &str) -> bool {
        self.orchestrator.is_running(self.tool, cwd)
    }
}
