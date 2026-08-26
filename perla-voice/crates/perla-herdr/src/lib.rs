//! Herdr integration — Perla's window onto the terminal board.
//!
//! Herdr (the agent multiplexer) owns the UI: workspaces, tabs, panes, a
//! sidebar with live agent states. Perla runs in a pinned pane and uses the
//! `herdr` CLI (JSON over stdout) to:
//!
//! - SEE everything: every pane and agent in the session — including ones
//!   the user started by hand — with `working / idle / blocked` states,
//! - SPAWN visible work: new tabs running claude / codex / grok / plain
//!   commands the user can watch and touch,
//! - NARRATE changes: the board watcher polls agent states and emits events
//!   the voice engine turns into speech ("codex is blocked, it's asking
//!   about the migration").
//!
//! Shapes were captured against herdr 0.8.0 (protocol 19); every parse is
//! defensive because the CLI is a moving target.

pub mod board;
pub mod client;
pub mod dispatcher;
pub mod reporter;

pub use board::{BoardWatcher, HerdrEvent, TrackedCommands};
pub use client::{HerdrAgent, HerdrClient, HerdrWorkspace};
pub use dispatcher::HerdrDispatcher;
pub use reporter::SelfReporter;

/// True when this process runs inside a herdr-managed pane.
pub fn inside_herdr() -> bool {
    std::env::var("HERDR_ENV").map(|v| v == "1").unwrap_or(false)
}

/// The herdr workspace Perla's own pane lives in, when inside herdr.
pub fn own_workspace_id() -> Option<String> {
    std::env::var("HERDR_WORKSPACE_ID").ok().filter(|s| !s.is_empty())
}

/// Herdr is usable: binary on PATH and the server socket present.
pub fn herdr_available() -> bool {
    let binary = which_herdr().is_some();
    let socket = dirs::config_dir()
        .map(|c| c.join("herdr/herdr.sock").exists())
        .unwrap_or(false)
        || dirs::home_dir()
            .map(|h| h.join(".config/herdr/herdr.sock").exists())
            .unwrap_or(false);
    binary && socket
}

pub fn which_herdr() -> Option<std::path::PathBuf> {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            let candidate = std::path::Path::new(dir).join("herdr");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let home = dirs::home_dir()?;
    let fallback = home.join(".local/bin/herdr");
    fallback.is_file().then_some(fallback)
}
