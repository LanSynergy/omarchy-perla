//! `perla-d` — headless host for the Perla engine.
//!
//! The Omarchy plugin is the face. This process is the brain: audio, the
//! realtime session, fast Omarchy tools, and hands. State is mirrored to
//! `$XDG_RUNTIME_DIR/perla/state.json`; commands arrive on `ctl.sock`.

use std::io::BufRead as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{info, warn};

use perla_core::events::{ConnectingPhase, Role, Speaker, Status};
use perla_core::{
    apply_settings_patch, user_config_path, Config, Engine, EngineCommand, EngineEvent,
    PublicSettings, SettingsPatch,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Transcript {
    role: String,
    text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Snapshot {
    status: String,
    speaker: String,
    muted: bool,
    reconnecting: bool,
    error: Option<String>,
    phase: Option<String>,
    activity: Option<String>,
    mic_level: f32,
    held_updates: usize,
    session_usd: f64,
    driving: bool,
    last_transcript: Option<Transcript>,
    pid: u32,
    provider: String,
    model: String,
    progress_mode: String,
    has_openai_key: bool,
    has_grok_key: bool,
    has_key: bool,
    start_muted: bool,
    voice: String,
    voice_language: Option<String>,
}

impl Snapshot {
    fn new() -> Self {
        Self {
            status: "disconnected".into(),
            speaker: "idle".into(),
            muted: false,
            reconnecting: false,
            error: None,
            phase: None,
            activity: None,
            mic_level: 0.0,
            held_updates: 0,
            session_usd: 0.0,
            driving: false,
            last_transcript: None,
            pid: std::process::id(),
            provider: "openai".into(),
            model: "gpt-realtime-2.1-mini".into(),
            progress_mode: "off".into(),
            has_openai_key: false,
            has_grok_key: false,
            has_key: false,
            start_muted: false,
            voice: "marin".into(),
            voice_language: None,
        }
    }

    fn apply_public(&mut self, public: &PublicSettings) {
        self.provider = public.provider.clone();
        self.model = public.model.clone();
        self.progress_mode = public.progress_mode.clone();
        self.has_openai_key = public.has_openai_key;
        self.has_grok_key = public.has_grok_key;
        self.has_key = public.has_key;
        self.start_muted = public.start_muted;
        self.voice = public.voice.clone();
        self.voice_language = public.voice_language.clone();
    }

    fn apply(&mut self, event: EngineEvent) -> bool {
        match event {
            EngineEvent::Status {
                status,
                error,
                reconnecting,
                phase,
            } => {
                self.status = status_name(status).into();
                self.error = error;
                self.reconnecting = reconnecting;
                self.phase = phase.map(phase_name).map(str::to_string);
                true
            }
            EngineEvent::Speaker(s) => {
                self.speaker = speaker_name(s).into();
                true
            }
            EngineEvent::Muted(m) => {
                self.muted = m;
                true
            }
            EngineEvent::Transcript(line) => {
                self.last_transcript = Some(Transcript {
                    role: role_name(line.role).into(),
                    text: line.text,
                });
                true
            }
            EngineEvent::AgentActivity(line) => {
                self.activity = line;
                true
            }
            EngineEvent::AgentRunning { running, .. } => {
                self.driving = running;
                true
            }
            EngineEvent::Cost { session_usd } => {
                self.session_usd = session_usd;
                true
            }
            EngineEvent::HeldUpdates(n) => {
                self.held_updates = n;
                true
            }
            EngineEvent::MicLevel(v) => {
                self.mic_level = v;
                false
            }
        }
    }
}

fn status_name(s: Status) -> &'static str {
    match s {
        Status::Disconnected => "disconnected",
        Status::Connecting => "connecting",
        Status::Connected => "connected",
        Status::ToolRunning => "tool_running",
        Status::Error => "error",
    }
}

fn speaker_name(s: Speaker) -> &'static str {
    match s {
        Speaker::Idle => "idle",
        Speaker::User => "user",
        Speaker::Model => "model",
    }
}

fn phase_name(p: ConnectingPhase) -> &'static str {
    match p {
        ConnectingPhase::Handshake => "handshake",
        ConnectingPhase::Ready => "ready",
    }
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

#[derive(Debug, Deserialize)]
struct Request {
    id: Option<String>,
    cmd: String,
    muted: Option<bool>,
    down: Option<bool>,
    text: Option<String>,
}

fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("perla");
        }
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    std::env::temp_dir().join(format!("perla-{user}"))
}

fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn secure_private_file(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Where the debug trail lives. `state.json` sits in `$XDG_RUNTIME_DIR`, which
/// is tmpfs and vanishes on reboot; a trail you want to paste into a bug report
/// has to outlive that, so it goes in the XDG state dir instead.
fn log_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("perla/session.jsonl")
}

/// Keep the trail from growing without bound. Trimming to the newest half
/// (rather than deleting) means a long session still leaves recent history.
const LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// One transcript line appended as JSON. The engine already records all three
/// kinds — what the user said, what Perla said, and every `→ tool(args)` with
/// its `✓`/`✗` result — so this only has to persist what flows past.
async fn append_log(path: &Path, role: Role, text: &str) {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent).ok();
    }
    if let Ok(meta) = tokio::fs::metadata(path).await {
        if meta.len() > LOG_MAX_BYTES {
            if let Ok(body) = tokio::fs::read_to_string(path).await {
                let lines: Vec<&str> = body.lines().collect();
                let keep = lines[lines.len() / 2..].join("\n");
                tokio::fs::write(path, format!("{keep}\n")).await.ok();
            }
        }
    }
    let entry = serde_json::json!({
        "ts_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        "role": role_name(role),
        "text": text,
    });
    let mut line = entry.to_string();
    line.push('\n');
    use tokio::io::AsyncWriteExt as _;
    if let Ok(mut f) = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
    {
        secure_private_file(path).ok();
        f.write_all(line.as_bytes()).await.ok();
    }
}

/// `HH:MM:SS` UTC from epoch millis. No date crate in this tree, and a debug
/// trail only needs wall-clock-of-day plus ordering — the relative column does
/// the rest.
fn clock(ts_ms: u64) -> String {
    let secs = ts_ms / 1000;
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

/// Render the trail for a human: clock, offset from the first shown line, who
/// spoke, and the text. Tool lines already arrive marked `→`, `✓` or `✗`.
fn render_log(body: &str, tail: usize) -> String {
    let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(tail);
    let shown = &lines[start..];
    if shown.is_empty() {
        return "perla debug log is empty — start a session and talk to her first.\n".to_string();
    }
    let mut out = format!(
        "perla debug log — {} of {} lines (UTC)\n\n",
        shown.len(),
        lines.len()
    );
    let mut first_ts: Option<u64> = None;
    for raw in shown {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
            continue;
        };
        let ts = v.get("ts_ms").and_then(|x| x.as_u64()).unwrap_or(0);
        let base = *first_ts.get_or_insert(ts);
        let role = v.get("role").and_then(|x| x.as_str()).unwrap_or("?");
        let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("");
        let who = match role {
            "user" => "you  ",
            "assistant" => "perla",
            "tool" => "tool ",
            other => other,
        };
        let rel = (ts.saturating_sub(base)) as f64 / 1000.0;
        out.push_str(&format!(
            "{}  +{:>6.1}s  {}  {}\n",
            clock(ts),
            rel,
            who,
            text
        ));
    }
    out
}

fn paths() -> (PathBuf, PathBuf) {
    let dir = runtime_dir();
    (dir.join("state.json"), dir.join("ctl.sock"))
}

async fn write_state(path: &Path, snap: &Snapshot) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(snap)?;
    tokio::fs::write(&tmp, body).await?;
    secure_private_file(&tmp)?;
    tokio::fs::rename(&tmp, path).await?;
    secure_private_file(path)?;
    Ok(())
}

fn maybe_install_harness_skill() {
    let dest = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".grok/skills/omarchy-harness");
    if dest.join("SKILL.md").is_file() {
        return;
    }
    let candidates = [
        std::env::var_os("PERLA_OMARCHY_HARNESS_SKILL").map(PathBuf::from),
        std::env::current_exe().ok().and_then(|p| {
            p.parent()
                .map(|d| d.join("../../../omarchy-harness/skills/omarchy-harness"))
        }),
        std::env::current_dir()
            .ok()
            .map(|d| d.join("../omarchy-harness/skills/omarchy-harness")),
        std::env::current_dir()
            .ok()
            .map(|d| d.join("omarchy-harness/skills/omarchy-harness")),
    ];
    for src in candidates.into_iter().flatten() {
        let skill = src.join("SKILL.md");
        if skill.is_file() {
            if let Err(e) = copy_dir(&src, &dest) {
                warn!("could not install omarchy-harness skill: {e}");
            } else {
                info!("installed omarchy-harness skill at {}", dest.display());
            }
            return;
        }
    }
}

fn copy_dir(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_file() {
            std::fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}

async fn serve() -> Result<()> {
    maybe_install_harness_skill();
    let (state_path, sock_path) = paths();
    if let Some(parent) = sock_path.parent() {
        ensure_private_dir(parent)?;
    }
    let _ = tokio::fs::remove_file(&sock_path).await;
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("binding {}", sock_path.display()))?;
    secure_private_file(&sock_path)?;
    info!("perla-d listening on {}", sock_path.display());

    let snap = Arc::new(Mutex::new(Snapshot::new()));
    let restart = Arc::new(tokio::sync::Notify::new());
    let resume_after_restart = Arc::new(AtomicBool::new(false));
    let mut flush = tokio::time::interval(Duration::from_millis(120));

    loop {
        let config = Config::load(None)?;
        let public = PublicSettings::from_config(&config);
        {
            let mut s = snap.lock().await;
            s.apply_public(&public);
            s.muted = public.start_muted;
            s.pid = std::process::id();
            write_state(&state_path, &s).await.ok();
        }
        let (engine, mut events) = Engine::start(config);
        if resume_after_restart.swap(false, Ordering::Relaxed) {
            engine.send(EngineCommand::Start);
        }
        let log_file = log_path();
        let mut mic_dirty = false;
        loop {
            tokio::select! {
                Some(event) = events.recv() => {
                    // Persist before applying: `apply` consumes the event, and
                    // the snapshot only ever keeps the newest line.
                    if let EngineEvent::Transcript(line) = &event {
                        append_log(&log_file, line.role, &line.text).await;
                    }
                    let mut s = snap.lock().await;
                    let persist = s.apply(event);
                    if persist {
                        write_state(&state_path, &s).await.ok();
                    } else {
                        mic_dirty = true;
                    }
                }
                Ok((stream, _)) = listener.accept() => {
                    let engine = engine.clone();
                    let snap = snap.clone();
                    let state_path = state_path.clone();
                    let restart = restart.clone();
                    let resume_after_restart = resume_after_restart.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_client(
                            stream,
                            engine,
                            snap,
                            &state_path,
                            restart,
                            resume_after_restart,
                        ).await {
                            warn!("ctl client: {e:#}");
                        }
                    });
                }
                _ = flush.tick() => {
                    if mic_dirty {
                        mic_dirty = false;
                        let s = snap.lock().await;
                        write_state(&state_path, &s).await.ok();
                    }
                }
                _ = restart.notified() => {
                    info!("reloading engine after settings change");
                    let _ = engine.send(EngineCommand::Stop);
                    break;
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("perla-d stopping");
                    let _ = engine.send(EngineCommand::Stop);
                    let _ = tokio::fs::remove_file(&sock_path).await;
                    return Ok(());
                }
            }
        }
    }
}

async fn handle_client(
    stream: UnixStream,
    engine: Engine,
    snap: Arc<Mutex<Snapshot>>,
    state_path: &Path,
    restart: Arc<tokio::sync::Notify>,
    resume_after_restart: Arc<AtomicBool>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                write_json(
                    &mut writer,
                    &serde_json::json!({"ok": false, "error": e.to_string()}),
                )
                .await?;
                continue;
            }
        };
        let reply = dispatch_cmd(
            &req,
            &engine,
            &snap,
            state_path,
            &restart,
            &resume_after_restart,
        )
        .await;
        write_json(&mut writer, &reply).await?;
    }
    Ok(())
}

async fn dispatch_cmd(
    req: &Request,
    engine: &Engine,
    snap: &Arc<Mutex<Snapshot>>,
    state_path: &Path,
    restart: &tokio::sync::Notify,
    resume_after_restart: &AtomicBool,
) -> serde_json::Value {
    let id = req.id.clone().unwrap_or_default();
    let fail = |msg: &str| serde_json::json!({"id": id, "ok": false, "error": msg});
    let ok = || serde_json::json!({"id": id, "ok": true});
    match req.cmd.as_str() {
        "ping" => serde_json::json!({"id": id, "ok": true, "pong": true}),
        "status" => {
            let s = snap.lock().await;
            serde_json::json!({"id": id, "ok": true, "state": &*s})
        }
        "start" => {
            let _ = engine.send(EngineCommand::Start);
            ok()
        }
        "stop" => {
            let _ = engine.send(EngineCommand::Stop);
            ok()
        }
        "mute" | "toggle-listen" => {
            let status = snap.lock().await.status.clone();
            if req.cmd == "toggle-listen" && status == "disconnected" {
                let _ = engine.send(EngineCommand::Start);
            } else {
                let _ = engine.send(EngineCommand::ToggleMute);
            }
            ok()
        }
        "set-muted" => {
            let Some(muted) = req.muted else {
                return fail("set-muted needs muted");
            };
            let _ = engine.send(EngineCommand::SetMuted(muted));
            ok()
        }
        "ptt" => {
            let Some(down) = req.down else {
                return fail("ptt needs down");
            };
            let _ = engine.send(EngineCommand::PushToTalk(down));
            ok()
        }
        "send-text" => {
            let text = req.text.clone().unwrap_or_default();
            if text.trim().is_empty() {
                return fail("empty text");
            }
            let _ = engine.send(EngineCommand::SendText(text));
            ok()
        }
        "deliver-held" => {
            let _ = engine.send(EngineCommand::DeliverHeldUpdates);
            ok()
        }
        "reload-state" => {
            let s = snap.lock().await;
            let _ = write_state(state_path, &s).await;
            ok()
        }
        "config-get" => {
            let s = snap.lock().await;
            serde_json::json!({
                "id": id,
                "ok": true,
                "provider": s.provider,
                "model": s.model,
                "progress_mode": s.progress_mode,
                "has_openai_key": s.has_openai_key,
                "has_grok_key": s.has_grok_key,
                "has_key": s.has_key,
                "start_muted": s.start_muted,
                "voice": s.voice,
                "voice_language": s.voice_language,
                "muted": s.muted,
            })
        }
        "config-set" => {
            let raw = req.text.clone().unwrap_or_default();
            let patch: SettingsPatch = match serde_json::from_str(&raw) {
                Ok(p) => p,
                Err(e) => return fail(&format!("invalid settings JSON: {e}")),
            };
            match apply_settings_patch(&user_config_path(), &patch) {
                Ok(public) => {
                    {
                        let mut s = snap.lock().await;
                        let was_running = matches!(
                            s.status.as_str(),
                            "connecting" | "connected" | "tool_running"
                        );
                        resume_after_restart.store(was_running, Ordering::Relaxed);
                        s.apply_public(&public);
                        let _ = write_state(state_path, &s).await;
                    }
                    restart.notify_waiters();
                    serde_json::json!({
                        "id": id,
                        "ok": true,
                        "reloading": true,
                        "provider": public.provider,
                        "model": public.model,
                        "progress_mode": public.progress_mode,
                        "has_openai_key": public.has_openai_key,
                        "has_grok_key": public.has_grok_key,
                        "has_key": public.has_key,
                        "start_muted": public.start_muted,
                        "voice": public.voice,
                        "voice_language": public.voice_language,
                    })
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        other => fail(&format!("unknown cmd '{other}'")),
    }
}

async fn write_json<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    value: &serde_json::Value,
) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await?;
    Ok(())
}

async fn client(cmd: Request) -> Result<()> {
    let (_, sock) = paths();
    let mut stream = UnixStream::connect(&sock).await.with_context(|| {
        format!(
            "perla-d is not running (no {})\nStart it with: perla-d serve",
            sock.display()
        )
    })?;
    let mut line = serde_json::to_vec(&cmd)?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    stream.flush().await?;
    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply).await?;
    print!("{reply}");
    if reply.contains("\"ok\":false") {
        std::process::exit(1);
    }
    Ok(())
}

fn usage() {
    eprintln!(
        "perla-d — Perla voice daemon for Omarchy

Usage:
  perla-d serve                 run the engine (systemd / foreground)
  perla-d start                 connect the voice session
  perla-d stop                  end the voice session
  perla-d toggle-listen         start if off, else mute/unmute
  perla-d mute                  toggle mute
  perla-d send <text>|--stdin   inject a typed task (--stdin is private)
  perla-d status                print state.json
  perla-d ping                  health check
  perla-d config                public settings (no raw keys)
  perla-d set-config --stdin    securely save keys/provider/model/progress
  perla-d ptt down|up           push-to-talk: mic live only while held
  perla-d log [--tail N]        what you said, what Perla said, every tool call
              [--json] [--copy]  --copy puts it on the clipboard

Keys are stored in ~/.config/perla-voice/config.toml, never in shell.json.

State:  $XDG_RUNTIME_DIR/perla/state.json
Socket: $XDG_RUNTIME_DIR/perla/ctl.sock
Log:    ~/.local/state/perla/session.jsonl
"
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    perla_core::init_logging();
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_else(|| "serve".into());
    match cmd.as_str() {
        "-h" | "--help" | "help" => {
            usage();
            Ok(())
        }
        "serve" => serve().await,
        "start" => {
            client(Request {
                id: Some("cli".into()),
                cmd: "start".into(),
                muted: None,
                down: None,
                text: None,
            })
            .await
        }
        "stop" => {
            client(Request {
                id: Some("cli".into()),
                cmd: "stop".into(),
                muted: None,
                down: None,
                text: None,
            })
            .await
        }
        "toggle-listen" => {
            client(Request {
                id: Some("cli".into()),
                cmd: "toggle-listen".into(),
                muted: None,
                down: None,
                text: None,
            })
            .await
        }
        "mute" => {
            client(Request {
                id: Some("cli".into()),
                cmd: "mute".into(),
                muted: None,
                down: None,
                text: None,
            })
            .await
        }
        "ping" => {
            client(Request {
                id: Some("cli".into()),
                cmd: "ping".into(),
                muted: None,
                down: None,
                text: None,
            })
            .await
        }
        "status" => {
            let (state_path, _) = paths();
            match tokio::fs::read_to_string(&state_path).await {
                Ok(text) => {
                    print!("{text}");
                    if !text.ends_with('\n') {
                        println!();
                    }
                    Ok(())
                }
                Err(_) => {
                    client(Request {
                        id: Some("cli".into()),
                        cmd: "status".into(),
                        muted: None,
                        down: None,
                        text: None,
                    })
                    .await
                }
            }
        }
        "send" => {
            let first = args.next();
            let text = if first.as_deref() == Some("--stdin") {
                let mut text = String::new();
                std::io::stdin()
                    .lock()
                    .read_line(&mut text)
                    .context("reading message from stdin")?;
                text
            } else {
                let mut parts = Vec::new();
                if let Some(first) = first {
                    parts.push(first);
                }
                parts.extend(args);
                parts.join(" ")
            };
            if text.trim().is_empty() {
                anyhow::bail!("usage: perla-d send <text>|--stdin");
            }
            client(Request {
                id: Some("cli".into()),
                cmd: "send-text".into(),
                muted: None,
                down: None,
                text: Some(text),
            })
            .await
        }
        "config" | "settings" => {
            client(Request {
                id: Some("cli".into()),
                cmd: "config-get".into(),
                muted: None,
                down: None,
                text: None,
            })
            .await
        }
        "set-config" => {
            let first = args.next();
            let text = if first.as_deref() == Some("--stdin") {
                let mut text = String::new();
                std::io::stdin()
                    .lock()
                    .read_line(&mut text)
                    .context("reading settings from stdin")?;
                text
            } else {
                let mut parts = Vec::new();
                if let Some(first) = first {
                    parts.push(first);
                }
                parts.extend(args);
                let text = parts.join(" ");
                let includes_key = serde_json::from_str::<SettingsPatch>(&text)
                    .map(|patch| patch.openai_key.is_some() || patch.grok_key.is_some())
                    .unwrap_or_else(|_| {
                        text.contains("\"openai_key\"") || text.contains("\"grok_key\"")
                    });
                if includes_key {
                    anyhow::bail!("API keys must use: perla-d set-config --stdin");
                }
                text
            };
            if text.trim().is_empty() {
                anyhow::bail!("usage: perla-d set-config --stdin");
            }
            client(Request {
                id: Some("cli".into()),
                cmd: "config-set".into(),
                muted: None,
                down: None,
                text: Some(text),
            })
            .await
        }
        "ptt" => {
            // The engine has had push-to-talk since the beginning; nothing
            // could reach it. Hold a key and the mic is live only then, which
            // is the cheapest way to run her.
            let arg = args.next().unwrap_or_default();
            let down = match arg.as_str() {
                "down" | "press" | "on" | "1" => true,
                "up" | "release" | "off" | "0" => false,
                _ => anyhow::bail!("usage: perla-d ptt down|up"),
            };
            client(Request {
                id: Some("cli".into()),
                cmd: "ptt".into(),
                muted: None,
                down: Some(down),
                text: None,
            })
            .await
        }
        "log" | "debug" => {
            let rest: Vec<String> = args.collect();
            let mut tail = 200usize;
            let mut as_json = false;
            let mut copy = false;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--json" => as_json = true,
                    "--copy" => copy = true,
                    "--tail" | "-n" => {
                        if let Some(v) = it.next() {
                            tail = v.parse().unwrap_or(tail);
                        }
                    }
                    other => anyhow::bail!(
                        "unknown flag '{other}' (usage: perla-d log [--tail N] [--json] [--copy])"
                    ),
                }
            }
            let path = log_path();
            let body = tokio::fs::read_to_string(&path).await.unwrap_or_default();
            let out = if as_json {
                let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
                let start = lines.len().saturating_sub(tail);
                format!("{}\n", lines[start..].join("\n"))
            } else {
                render_log(&body, tail)
            };
            if copy {
                // wl-copy is what every other Omarchy copy path uses.
                let mut child = tokio::process::Command::new("wl-copy")
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                    .context("wl-copy is needed for --copy (pacman -S wl-clipboard)")?;
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(out.as_bytes()).await.ok();
                }
                child.wait().await.ok();
                let n = out.lines().count().saturating_sub(2);
                println!("copied {n} lines to the clipboard ({})", path.display());
            } else {
                print!("{out}");
            }
            Ok(())
        }
        other => {
            usage();
            anyhow::bail!("unknown command '{other}'");
        }
    }
}

impl serde::Serialize for Request {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("Request", 5)?;
        s.serialize_field("id", &self.id)?;
        s.serialize_field("cmd", &self.cmd)?;
        s.serialize_field("muted", &self.muted)?;
        s.serialize_field("down", &self.down)?;
        s.serialize_field("text", &self.text)?;
        s.end()
    }
}
