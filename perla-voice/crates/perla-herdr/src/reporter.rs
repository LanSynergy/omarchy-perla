//! Perla's own presence in the Herdr sidebar.
//!
//! Herdr's custom-integration protocol (`pane report-agent`) lets any process
//! register itself as an agent for the pane it runs in. We use it so Perla
//! shows up next to claude/codex/grok in the sidebar with real states:
//!
//! - `idle`     — listening, nothing running
//! - `working`  — her hands (or a spawned agent) are mid-task
//! - `blocked`  — updates are held and waiting for the user to ask
//!
//! All calls are fire-and-forget with a strictly increasing `--seq`; Herdr
//! ignores stale sequence numbers, so out-of-order task completion is safe.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tracing::debug;

use crate::client::HerdrClient;

const SOURCE: &str = "custom:perla";
const AGENT: &str = "perla";

pub struct SelfReporter {
    client: HerdrClient,
    pane_id: String,
    seq: AtomicU64,
}

impl SelfReporter {
    /// Some only when running inside a herdr pane (HERDR_ENV=1 + pane id).
    pub fn detect() -> Option<Arc<Self>> {
        if !crate::inside_herdr() {
            return None;
        }
        let pane_id = std::env::var("HERDR_PANE_ID").ok().filter(|s| !s.is_empty())?;
        let client = HerdrClient::new()?;
        Some(Arc::new(Self {
            client,
            pane_id,
            seq: AtomicU64::new(1),
        }))
    }

    /// Report a semantic state ("idle" | "working" | "blocked"), with an
    /// optional short message shown for blocks. Non-blocking.
    pub fn report(self: &Arc<Self>, state: &str, message: Option<String>) {
        let this = self.clone();
        let state = state.to_string();
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            let seq_s = seq.to_string();
            let mut args: Vec<&str> = vec![
                "pane",
                "report-agent",
                &this.pane_id,
                "--source",
                SOURCE,
                "--agent",
                AGENT,
                "--state",
                &state,
                "--seq",
                &seq_s,
            ];
            if let Some(msg) = message.as_deref() {
                args.push("--message");
                args.push(msg);
            }
            if let Err(e) = this.client.call(&args).await {
                debug!("herdr self-report failed: {e:#}");
            }
        });
    }

    /// Update the pane's title in the sidebar ("Perla — reviewing auth…").
    /// Presentation-only; doesn't touch the semantic state. Non-blocking.
    pub fn set_title(self: &Arc<Self>, title: String) {
        let this = self.clone();
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            let seq_s = seq.to_string();
            let title: String = title.chars().take(80).collect();
            let args = [
                "pane",
                "report-metadata",
                &this.pane_id,
                "--source",
                SOURCE,
                "--agent",
                AGENT,
                "--title",
                &title,
                "--display-agent",
                "Perla",
                "--seq",
                &seq_s,
            ];
            if let Err(e) = this.client.call(&args).await {
                debug!("herdr title report failed: {e:#}");
            }
        });
    }

    /// Release lifecycle authority for this pane — call on clean exit so
    /// herdr doesn't show a ghost Perla.
    pub async fn release(&self) {
        let _ = self
            .client
            .call(&[
                "pane",
                "release-agent",
                &self.pane_id,
                "--source",
                SOURCE,
                "--agent",
                AGENT,
            ])
            .await;
    }
}
