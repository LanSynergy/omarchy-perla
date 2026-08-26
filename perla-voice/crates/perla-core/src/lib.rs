//! perla-voice engine facade.
//!
//! Rust port of the macOS Perla voice-orchestrator architecture: a realtime
//! voice session that drives local coding agents (Claude Code / Codex),
//! watches their work, and narrates proactively.
//!
//! Embedding: build an [`engine::Engine`] with a [`config::Config`] (plus an
//! optional custom tool dispatcher), consume the [`events::EngineEvent`]
//! stream, and drive it with [`events::EngineCommand`]s.

pub mod config;
pub mod cost;
pub mod engine;
pub mod events;
pub mod language;
pub mod recap;
pub mod sidechannel;

pub use config::{
    apply_settings_patch, user_config_path, Config, OmarchyConfig, PublicSettings, SettingsPatch,
};
pub use engine::Engine;
pub use events::{EngineCommand, EngineEvent, Speaker, Status};

/// Re-exports so embedders only need perla-core.
pub use perla_agents as agents;
pub use perla_audio as audio;
pub use perla_provider as provider;
pub use perla_tools as tools;

/// Initialize tracing with env-filter (`PERLA_LOG` or `RUST_LOG`).
pub fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = std::env::var("PERLA_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| "info".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter))
        .with_writer(std::io::stderr)
        .try_init();
}
