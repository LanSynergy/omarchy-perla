use std::time::SystemTime;

use async_trait::async_trait;
use serde_json::Value;

use crate::types::ToolResult;

/// Per-call bookkeeping handed to the dispatcher. For slow agent tools the
/// engine pre-allocates a history id so the out-of-band completion (minutes
/// later) can be matched back to this call.
#[derive(Debug, Clone)]
pub struct ToolCallContext {
    pub call_id: String,
    pub history_id: Option<String>,
    pub started_at: SystemTime,
}

/// The seam hosts implement to answer the model's function calls.
///
/// The built-in `perla-agents` dispatcher handles the coding-agent tools and
/// fast file tools; an embedder can wrap it (or replace it) to add their own
/// tools — a CMS could register `publish_article`, a coding harness could
/// register its own `run_agent`.
#[async_trait]
pub trait ToolDispatcher: Send + Sync {
    async fn dispatch(&self, name: &str, args: Value, ctx: ToolCallContext) -> ToolResult;
}
