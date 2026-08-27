//! Engine configuration. Local API keys only — resolved from (in order)
//! explicit config file values, then environment variables.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    OpenAi,
    Grok,
}

impl ProviderKind {
    pub fn label(&self) -> &'static str {
        match self {
            ProviderKind::OpenAi => "openai",
            ProviderKind::Grok => "grok",
        }
    }

    pub fn parse_label(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "openai" | "open_ai" | "open-ai" => Some(Self::OpenAi),
            "grok" | "xai" | "x-ai" => Some(Self::Grok),
            _ => None,
        }
    }
}

/// Turn-taking tuning — port of `RealtimeTurnDetection`. `server_vad`,
/// deliberately not `semantic_vad`: one deterministic knob
/// (`silence_duration_ms`) instead of a model that cuts users off mid-thought.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VadConfig {
    pub silence_duration_ms: u32,
    pub prefix_padding_ms: u32,
    pub threshold: f64,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            silence_duration_ms: 1000,
            prefix_padding_ms: 300,
            threshold: 0.5,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OmarchyConfig {
    /// Hands should use the omarchy-harness skill for see/click/type.
    pub harness: bool,
    /// Register fast desktop tools (launch, theme, workspace, summon).
    pub fast_desktop_tools: bool,
    /// Destructive omarchy_run calls require confirmed=true.
    pub confirm_destructive: bool,
}

impl Default for OmarchyConfig {
    fn default() -> Self {
        Self {
            harness: cfg!(target_os = "linux"),
            fast_desktop_tools: cfg!(target_os = "linux"),
            confirm_destructive: true,
        }
    }
}

/// Speaker → mic feedback handling. On headphones none of this matters; on
/// speakers it is the difference between a conversation and Perla answering
/// herself in a loop.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Real acoustic echo cancellation (AEC3) when compiled in. Subtracts the
    /// speaker signal from the mic, so the user can talk over Perla at normal
    /// volume — full duplex. Falls back to `echo_guard` when unavailable.
    pub aec: bool,
    /// Half-duplex fallback: drop mic audio while Perla is audibly speaking.
    /// Cheap and dependency-free, but quiet interruptions get eaten.
    pub echo_guard: bool,
    /// Mic RMS (0..=1) that counts as deliberately talking over her. Perla's
    /// own echo sits well below this; a raised voice clears it.
    pub barge_rms: f32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            aec: true,
            echo_guard: true,
            barge_rms: 0.05,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub api_key: Option<String>,
    /// WebSocket endpoint override; each provider has a sensible default.
    pub url: Option<String>,
    /// Realtime model id, e.g. "gpt-realtime-2.1-mini".
    pub model: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub provider: ProviderKind,
    pub openai: ProviderConfig,
    pub grok: ProviderConfig,

    /// Voice preset name (provider-specific, e.g. "marin" for OpenAI).
    pub voice: String,
    /// Optional explicit reply-language pin, e.g. "ar" / "en". None = follow
    /// the user's speech.
    pub voice_language: Option<String>,

    /// Execution mode: "hands" (default — one grok-build session is Perla's
    /// hands for everything) or "agents" (Perla routes between the Claude
    /// Code / Codex CLIs like the macOS app).
    pub mode: String,
    /// Explicit path to the grok binary (hands mode). Auto-discovered from
    /// `~/.grok/bin/grok` and $PATH when unset.
    pub hands_binary: Option<PathBuf>,
    /// Model for the hands session (grok's `--model`), when the user picks one.
    pub hands_model: Option<String>,
    /// Herdr board integration (visible tabs, whole-board awareness).
    /// None = auto: enabled when the herdr binary + server are present.
    pub herdr: Option<bool>,

    /// Active workspace folder agent tasks run in.
    pub workspace: PathBuf,
    pub recent_workspaces: Vec<PathBuf>,
    /// Default agent runtime in agents mode: "claude" or "codex".
    pub runtime: String,
    /// Optional `--model` for the agent CLI.
    pub agent_model: Option<String>,
    /// Optional reasoning effort for the agent CLI.
    pub agent_effort: Option<String>,

    /// Live milestone narration while the agent works.
    pub detail_mode: bool,
    /// Only narrate step completions, not "now starting…".
    pub big_moments_only: bool,
    /// Queue finished-agent updates until the user asks for them.
    pub hold_announcements: bool,

    /// Start with the mic muted (push-to-talk style). Continuous mode = false.
    pub start_muted: bool,

    /// Omarchy desktop integration (fast tools + harness skill).
    pub omarchy: OmarchyConfig,

    pub vad: VadConfig,

    /// Speaker/mic feedback handling (echo cancellation, barge-in threshold).
    pub audio: AudioConfig,

    /// Rotate to a fresh provider session after this many seconds (the
    /// server caps sessions at ~60 min). Hard deadline is +8s.
    pub rotate_after_secs: u64,

    /// End the session after this many seconds with nothing from the user.
    /// An open session bills for the audio it hears whether or not anyone is
    /// talking to it, and ambient noise opens whole turns of its own — an idle
    /// session is pure cost. 0 disables the timeout.
    pub idle_stop_secs: u64,

    /// Upper bound for a single Realtime response. Keeping spoken replies
    /// short improves latency and prevents a bad turn from running up cost.
    pub max_output_tokens: u32,
    /// Conversation tokens retained after instructions. OpenAI's
    /// retention-ratio truncation keeps the prompt cache stable while putting
    /// a hard ceiling on the context replayed every turn.
    pub context_token_limit: u32,
    pub retention_ratio: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: ProviderKind::OpenAi,
            openai: ProviderConfig::default(),
            grok: ProviderConfig::default(),
            voice: "marin".into(),
            voice_language: None,
            mode: "hands".into(),
            hands_binary: None,
            hands_model: None,
            herdr: None,
            workspace: dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")),
            recent_workspaces: Vec::new(),
            runtime: "claude".into(),
            agent_model: None,
            agent_effort: None,
            detail_mode: false,
            big_moments_only: true,
            hold_announcements: false,
            start_muted: false,
            omarchy: OmarchyConfig::default(),
            vad: VadConfig::default(),
            audio: AudioConfig::default(),
            rotate_after_secs: 50 * 60,
            idle_stop_secs: 3 * 60,
            max_output_tokens: 768,
            context_token_limit: 8_000,
            retention_ratio: 0.8,
        }
    }
}

impl Config {
    /// Load from an explicit path, else `./perla-voice.toml`, else
    /// `~/.config/perla-voice/config.toml`, else defaults. Env vars fill in
    /// missing API keys afterwards.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let candidates: Vec<PathBuf> = match explicit {
            Some(p) => vec![p.to_path_buf()],
            None => {
                let mut v = vec![PathBuf::from("perla-voice.toml")];
                v.push(user_config_path());
                v
            }
        };
        let mut config = Config::default();
        for path in candidates {
            // Guarded rather than `exists()` + `read_to_string`: these paths are
            // predictable, so a planted FIFO would otherwise park the daemon here
            // forever and a planted huge file would read until memory ran out.
            if let Some(text) = crate::safeio::read_text_capped(&path)? {
                config =
                    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
                break;
            }
        }
        config.apply_env();
        config.workspace = expand_tilde(&config.workspace);
        Ok(config)
    }

    fn apply_env(&mut self) {
        if self.openai.api_key.is_none() {
            self.openai.api_key = std::env::var("PERLA_OPENAI_API_KEY")
                .or_else(|_| std::env::var("OPENAI_API_KEY"))
                .ok();
        }
        if self.grok.api_key.is_none() {
            self.grok.api_key = std::env::var("PERLA_XAI_API_KEY")
                .or_else(|_| std::env::var("XAI_API_KEY"))
                .ok();
        }
        if let Ok(ws) = std::env::var("PERLA_WORKSPACE") {
            self.workspace = PathBuf::from(ws);
        }
        if let Ok(mode) = std::env::var("PERLA_MODE") {
            if !mode.is_empty() {
                self.mode = mode;
            }
        }
        if let Ok(s) = std::env::var("PERLA_ROTATE_AFTER_SEC") {
            if let Ok(v) = s.parse::<u64>() {
                if v > 0 {
                    self.rotate_after_secs = v;
                }
            }
        }
    }

    /// The active provider's settings, with defaults resolved.
    pub fn active_provider(&self) -> ResolvedProvider {
        let (raw, default_url, default_model) = match self.provider {
            ProviderKind::OpenAi => (
                &self.openai,
                "wss://api.openai.com/v1/realtime",
                "gpt-realtime-2.1-mini",
            ),
            ProviderKind::Grok => (
                &self.grok,
                "wss://api.x.ai/v1/realtime",
                "grok-4-fast-realtime",
            ),
        };
        ResolvedProvider {
            kind: self.provider,
            api_key: raw.api_key.clone().unwrap_or_default(),
            url: raw.url.clone().unwrap_or_else(|| default_url.into()),
            model: raw.model.clone().unwrap_or_else(|| default_model.into()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub kind: ProviderKind,
    pub api_key: String,
    pub url: String,
    pub model: String,
}

pub fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

/// `~/.config/perla-voice/config.toml` — keys live here, not in shell.json.
pub fn user_config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("PERLA_CONFIG_PATH").filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("perla-voice/config.toml")
}

/// What the bar plugin may show. Never includes the raw API key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicSettings {
    pub provider: String,
    pub model: String,
    /// "off", "big", or "steps".
    pub progress_mode: String,
    pub has_openai_key: bool,
    pub has_grok_key: bool,
    pub has_key: bool,
    pub start_muted: bool,
    pub voice: String,
    /// Explicit reply-language pin ("en", "ar", …) or None for Auto, where the
    /// model follows whatever the user speaks.
    pub voice_language: Option<String>,
}

impl PublicSettings {
    pub fn from_config(config: &Config) -> Self {
        let has_openai_key = config
            .openai
            .api_key
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        let has_grok_key = config
            .grok
            .api_key
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        let has_key = match config.provider {
            ProviderKind::OpenAi => has_openai_key,
            ProviderKind::Grok => has_grok_key,
        };
        Self {
            provider: config.provider.label().into(),
            model: config.active_provider().model,
            progress_mode: if !config.detail_mode {
                "off"
            } else if config.big_moments_only {
                "big"
            } else {
                "steps"
            }
            .into(),
            has_openai_key,
            has_grok_key,
            has_key,
            start_muted: config.start_muted,
            voice: config.voice.clone(),
            voice_language: config.voice_language.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SettingsPatch {
    pub provider: Option<String>,
    /// Realtime model for the active (or simultaneously selected) provider.
    pub model: Option<String>,
    /// "off", "big", or "steps".
    pub progress_mode: Option<String>,
    pub openai_key: Option<String>,
    pub grok_key: Option<String>,
    pub start_muted: Option<bool>,
    pub voice: Option<String>,
    /// "en" / "ar" / … to pin the reply language; "auto" or "" clears the pin.
    pub voice_language: Option<String>,
}

/// Merge a settings patch into the user config file and return the public view.
pub fn apply_settings_patch(path: &Path, patch: &SettingsPatch) -> Result<PublicSettings> {
    let mut root = match crate::safeio::read_text_capped(path)? {
        Some(text) if !text.trim().is_empty() => {
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
        }
        _ => toml::Value::Table(toml::map::Map::new()),
    };

    let table = root
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("config root must be a table"))?;

    if let Some(provider) = patch.provider.as_deref() {
        let kind = ProviderKind::parse_label(provider)
            .ok_or_else(|| anyhow::anyhow!("provider must be openai or grok"))?;
        table.insert("provider".into(), toml::Value::String(kind.label().into()));
    }
    if let Some(model) = patch.model.as_deref() {
        let model = model.trim();
        if model.is_empty() || model.len() > 128 {
            anyhow::bail!("model must be between 1 and 128 characters");
        }
        let active = patch
            .provider
            .as_deref()
            .and_then(ProviderKind::parse_label)
            .or_else(|| {
                table
                    .get("provider")
                    .and_then(toml::Value::as_str)
                    .and_then(ProviderKind::parse_label)
            })
            .unwrap_or(ProviderKind::OpenAi);
        ensure_table(table, active.label())
            .insert("model".into(), toml::Value::String(model.into()));
    }
    if let Some(mode) = patch.progress_mode.as_deref() {
        match mode.trim().to_ascii_lowercase().as_str() {
            "off" => {
                table.insert("detail_mode".into(), toml::Value::Boolean(false));
                table.insert("big_moments_only".into(), toml::Value::Boolean(true));
            }
            "big" => {
                table.insert("detail_mode".into(), toml::Value::Boolean(true));
                table.insert("big_moments_only".into(), toml::Value::Boolean(true));
            }
            "steps" => {
                table.insert("detail_mode".into(), toml::Value::Boolean(true));
                table.insert("big_moments_only".into(), toml::Value::Boolean(false));
            }
            _ => anyhow::bail!("progress_mode must be off, big, or steps"),
        }
    }
    if let Some(voice) = patch.voice.as_deref() {
        let voice = voice.trim();
        if !voice.is_empty() {
            table.insert("voice".into(), toml::Value::String(voice.into()));
        }
    }
    if let Some(muted) = patch.start_muted {
        table.insert("start_muted".into(), toml::Value::Boolean(muted));
    }
    if let Some(lang) = patch.voice_language.as_deref() {
        // "auto" is how the UI says "no pin" — a dropdown cannot send null,
        // and an absent field already means "leave unchanged".
        let lang = lang.trim();
        if lang.is_empty() || lang.eq_ignore_ascii_case("auto") {
            table.remove("voice_language");
        } else {
            table.insert(
                "voice_language".into(),
                toml::Value::String(lang.to_ascii_lowercase()),
            );
        }
    }
    if let Some(key) = patch.openai_key.as_deref() {
        let key = key.trim();
        if !key.is_empty() {
            ensure_table(table, "openai").insert("api_key".into(), toml::Value::String(key.into()));
        }
    }
    if let Some(key) = patch.grok_key.as_deref() {
        let key = key.trim();
        if !key.is_empty() {
            ensure_table(table, "grok").insert("api_key".into(), toml::Value::String(key.into()));
        }
    }

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        crate::safeio::ensure_private_dir(parent)?;
    }
    let body = toml::to_string_pretty(&root).context("serializing config")?;
    // This body carries the user's API key. It must never land anywhere but
    // this exact path, and never half-written.
    crate::safeio::write_private(path, body.as_bytes())?;

    let config = Config::load(Some(path))?;
    Ok(PublicSettings::from_config(&config))
}

fn ensure_table<'a>(
    table: &'a mut toml::map::Map<String, toml::Value>,
    key: &str,
) -> &'a mut toml::map::Map<String, toml::Value> {
    let needs = !matches!(table.get(key), Some(toml::Value::Table(_)));
    if needs {
        table.insert(key.into(), toml::Value::Table(toml::map::Map::new()));
    }
    table
        .get_mut(key)
        .and_then(|v| v.as_table_mut())
        .expect("just inserted table")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_writes_provider_and_key() {
        let dir = std::env::temp_dir().join(format!("perla-cfg-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        let _ = std::fs::remove_file(&path);

        let public = apply_settings_patch(
            &path,
            &SettingsPatch {
                provider: Some("grok".into()),
                grok_key: Some("xai-test".into()),
                start_muted: Some(true),
                ..SettingsPatch::default()
            },
        )
        .unwrap();
        assert_eq!(public.provider, "grok");
        assert!(public.has_grok_key);
        assert!(public.has_key);
        assert!(public.start_muted);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("xai-test"));
        assert!(text.contains("start_muted = true"));
    }

    #[test]
    fn patch_writes_active_model_and_progress_mode() {
        let dir = std::env::temp_dir().join(format!("perla-cfg-model-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        let _ = std::fs::remove_file(&path);

        let public = apply_settings_patch(
            &path,
            &SettingsPatch {
                model: Some("gpt-realtime-2.1".into()),
                progress_mode: Some("big".into()),
                ..SettingsPatch::default()
            },
        )
        .unwrap();
        assert_eq!(public.model, "gpt-realtime-2.1");
        assert_eq!(public.progress_mode, "big");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("model = \"gpt-realtime-2.1\""));
        assert!(text.contains("detail_mode = true"));
        assert!(text.contains("big_moments_only = true"));
    }

    #[test]
    fn empty_key_does_not_wipe() {
        let dir = std::env::temp_dir().join(format!("perla-cfg-empty-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "provider = \"openai\"\n[openai]\napi_key = \"sk-keep\"\n",
        )
        .unwrap();

        apply_settings_patch(
            &path,
            &SettingsPatch {
                openai_key: Some("  ".into()),
                ..SettingsPatch::default()
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("sk-keep"));
    }
}
