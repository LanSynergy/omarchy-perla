//! `AgentTool` — port of the Swift enum of the same name.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentTool {
    Claude,
    Codex,
}

impl AgentTool {
    pub fn label(&self) -> &'static str {
        match self {
            AgentTool::Claude => "Claude Code",
            AgentTool::Codex => "Codex",
        }
    }

    /// The CLI binary name the user needs on their PATH.
    pub fn binary_name(&self) -> &'static str {
        match self {
            AgentTool::Claude => "claude",
            AgentTool::Codex => "codex",
        }
    }

    pub fn id(&self) -> &'static str {
        match self {
            AgentTool::Claude => "claude",
            AgentTool::Codex => "codex",
        }
    }

    /// Bypass approval prompts — the headless PTY means the user can't answer
    /// them anyway. Same flags the macOS app uses.
    pub fn launch_flags(&self) -> Vec<String> {
        match self {
            AgentTool::Claude => vec!["--permission-mode".into(), "bypassPermissions".into()],
            AgentTool::Codex => vec!["--full-auto".into()],
        }
    }

    /// Resume-a-conversation base args (`finishTurn` hands the session id on).
    pub fn resume_flags(&self, session_id: &str) -> Vec<String> {
        match self {
            AgentTool::Claude => vec!["--resume".into(), session_id.into()],
            AgentTool::Codex => vec!["resume".into(), session_id.into()],
        }
    }

    /// Shown when the CLI isn't installed — actionable install guidance.
    pub fn install_hint(&self) -> &'static str {
        match self {
            AgentTool::Claude => "Install Claude Code (e.g. `npm i -g @anthropic-ai/claude-code`) and make sure `claude` is on your PATH.",
            AgentTool::Codex => "Install Codex (e.g. `npm i -g @openai/codex`) and make sure `codex` is on your PATH.",
        }
    }

    pub fn other(&self) -> AgentTool {
        match self {
            AgentTool::Claude => AgentTool::Codex,
            AgentTool::Codex => AgentTool::Claude,
        }
    }

    pub fn from_id(s: &str) -> Option<AgentTool> {
        match s.to_ascii_lowercase().as_str() {
            "claude" => Some(AgentTool::Claude),
            "codex" => Some(AgentTool::Codex),
            _ => None,
        }
    }
}
