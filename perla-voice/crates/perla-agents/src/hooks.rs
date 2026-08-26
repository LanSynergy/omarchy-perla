//! Localhost listener for Claude Code hook callbacks — port of
//! `PerlaHookServer.swift`.
//!
//! Perla launches `claude` with `--settings <generated json>` whose Stop and
//! Notification hooks POST their stdin payload here via curl. That gives an
//! INSTANT, exact signal for "the turn just ended" and "Claude is waiting for
//! input" — the JSONL transcript tail remains the fallback (and the only
//! signal for Codex, which has no hooks). Everything degrades gracefully: if
//! the listener can't start, no `--settings` flag is added and polling
//! carries the whole load.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Fixed preferred port so globally-installed hooks keep working across
/// restarts. When taken we fall back to an ephemeral port: per-launch
/// `--settings` hooks still work; stale global hooks fail harmlessly
/// (`curl --max-time 3`).
pub const PREFERRED_PORT: u16 = 43823;

/// One parsed hook callback. `pid` comes from the hook shell's `$PPID` — the
/// hook command runs as a direct child of the claude process, so that pid is
/// the one identifier that tells two sessions in the SAME folder apart.
#[derive(Debug, Clone)]
pub struct HookEvent {
    pub kind: HookKind,
    pub cwd: String,
    pub pid: Option<u32>,
    pub session_id: Option<String>,
    pub transcript_path: Option<String>,
}

#[derive(Debug, Clone)]
pub enum HookKind {
    /// Claude's Stop hook fired: the turn in that session is over.
    Stop,
    /// Claude posted a Notification: it needs input/attention.
    Notification { message: String },
}

/// pid → its conversation, learned passively from hook traffic.
#[derive(Debug, Clone)]
pub struct HookBinding {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    pub seen_at: SystemTime,
}

pub struct HookServer {
    pub port: u16,
    token: String,
    bindings: Arc<Mutex<HashMap<u32, HookBinding>>>,
}

impl HookServer {
    /// Bind (preferred port first, ephemeral as fallback) and start serving.
    /// Events flow out `event_tx`. None when neither bind works.
    pub async fn start(event_tx: mpsc::UnboundedSender<HookEvent>) -> Option<Arc<HookServer>> {
        let token = auth_token();
        let listener = match TcpListener::bind(("127.0.0.1", PREFERRED_PORT)).await {
            Ok(l) => l,
            Err(_) => TcpListener::bind(("127.0.0.1", 0)).await.ok()?,
        };
        let port = listener.local_addr().ok()?.port();
        debug!(port, "hook server listening");
        let server = Arc::new(HookServer {
            port,
            token: token.clone(),
            bindings: Arc::new(Mutex::new(HashMap::new())),
        });

        let accept_server = server.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let server = accept_server.clone();
                let tx = event_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = server.handle(stream, tx).await {
                        warn!("hook connection error: {e}");
                    }
                });
            }
        });
        Some(server)
    }

    pub fn binding(&self, pid: u32) -> Option<HookBinding> {
        let mut map = self.bindings.lock().unwrap();
        let b = map.get(&pid).cloned()?;
        if !pid_alive(pid) {
            map.remove(&pid);
            return None;
        }
        Some(b)
    }

    /// Extra launch flags for `claude`: writes the settings file for the
    /// current port and returns `--settings <path>`. Empty when the user's
    /// GLOBAL hooks already cover every session on the fixed port — adding
    /// `--settings` too would fire each event twice.
    pub fn claude_launch_flags(&self) -> Vec<String> {
        if self.port == PREFERRED_PORT && global_hooks_installed() {
            return Vec::new();
        }
        match self.write_settings_file() {
            Some(path) => vec!["--settings".into(), path.to_string_lossy().to_string()],
            None => Vec::new(),
        }
    }

    /// Minimal HTTP handling: accumulate until Content-Length is satisfied,
    /// check the token, reply, dispatch.
    async fn handle(
        &self,
        mut stream: tokio::net::TcpStream,
        tx: mpsc::UnboundedSender<HookEvent>,
    ) -> std::io::Result<()> {
        let mut buffer: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 16384];
        let (head, body) = loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            buffer.extend_from_slice(&chunk[..n]);
            if buffer.len() > 512 * 1024 {
                return Ok(()); // runaway guard
            }
            if let Some(split) = find_header_end(&buffer) {
                let head = String::from_utf8_lossy(&buffer[..split]).to_string();
                let expected = content_length(&head);
                while buffer.len() - (split + 4) < expected {
                    let n = stream.read(&mut chunk).await?;
                    if n == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..n]);
                }
                let body = buffer[split + 4..].to_vec();
                break (head, body);
            }
        };

        let query = query_params(&head);
        // No token, no dispatch — 127.0.0.1 is reachable by every local
        // process and by no-preflight browser POSTs, and a spoofed
        // Notification would be spoken aloud. 403 so a curl repro tells the truth.
        let authorized = query
            .get("token")
            .map(|t| t == &self.token)
            .unwrap_or(false);
        let status = if authorized {
            "200 OK"
        } else {
            "403 Forbidden"
        };
        let response =
            format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;

        if !authorized {
            return Ok(());
        }
        let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
            return Ok(());
        };
        let pid = query.get("ppid").and_then(|p| p.parse::<u32>().ok());
        self.dispatch(payload, pid, &tx);
        Ok(())
    }

    fn dispatch(&self, payload: Value, pid: Option<u32>, tx: &mpsc::UnboundedSender<HookEvent>) {
        let cwd = payload
            .get("cwd")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let session_id = payload
            .get("session_id")
            .and_then(|s| s.as_str())
            .map(str::to_string);
        let transcript_path = payload
            .get("transcript_path")
            .and_then(|s| s.as_str())
            .map(str::to_string);

        // Every event refreshes the pid→conversation map. Sweep dead pids on
        // growth — liveness (not age) is the criterion.
        if let (Some(pid), Some(sid), Some(path)) = (pid, &session_id, &transcript_path) {
            let mut map = self.bindings.lock().unwrap();
            map.insert(
                pid,
                HookBinding {
                    session_id: sid.clone(),
                    transcript_path: path.clone(),
                    cwd: cwd.clone(),
                    seen_at: SystemTime::now(),
                },
            );
            if map.len() > 64 {
                map.retain(|p, _| pid_alive(*p));
            }
        }

        let kind = match payload.get("hook_event_name").and_then(|n| n.as_str()) {
            Some("Stop") => HookKind::Stop,
            Some("Notification") => {
                let message = payload
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string();
                if message.is_empty() {
                    return;
                }
                HookKind::Notification { message }
            }
            _ => return,
        };
        let _ = tx.send(HookEvent {
            kind,
            cwd,
            pid,
            session_id,
            transcript_path,
        });
    }

    /// Settings file rewritten each call so it always carries the current
    /// port. `--max-time 3` keeps a dead Perla from ever stalling Claude.
    fn write_settings_file(&self) -> Option<PathBuf> {
        let hook = json!({
            "hooks": [{ "type": "command", "command": hook_command(self.port, &self.token) }]
        });
        let settings = json!({ "hooks": { "Stop": [hook.clone()], "Notification": [hook] } });
        let dir = dirs::data_dir()?.join("perla-voice");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("claude-hooks.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&settings).ok()?).ok()?;
        Some(path)
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn content_length(head: &str) -> usize {
    head.lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// `POST /hook?token=…&ppid=12345 HTTP/1.1` → {"token": …, "ppid": "12345"}.
fn query_params(head: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(request_line) = head.lines().next() else {
        return out;
    };
    let Some(target) = request_line.split(' ').nth(1) else {
        return out;
    };
    let Some(q) = target.split_once('?').map(|(_, q)| q) else {
        return out;
    };
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            out.insert(k.to_string(), v.to_string());
        }
    }
    out
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Shared secret between this engine and the hook commands it writes.
/// Persistent (not per-run) because global hooks outlive restarts; anyone who
/// can read the token file is already running as the user.
fn auth_token() -> String {
    let path = dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("perla-voice/hook-token");
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return t;
        }
    }
    let t = uuid::Uuid::new_v4().to_string();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &t);
    t
}

/// `$PPID` = the claude pid (the hook shell is claude's direct child) —
/// double-quoted so the shell expands it AND the `?` never globs.
fn hook_command(port: u16, token: &str) -> String {
    format!(
        "curl -s --max-time 3 -X POST \"http://127.0.0.1:{port}/hook?token={token}&ppid=$PPID\" \
         -H 'Content-Type: application/json' --data-binary @- >/dev/null 2>&1 || true"
    )
}

fn global_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude/settings.json"))
}

/// True when ~/.claude/settings.json already posts Stop events to our fixed port.
pub fn global_hooks_installed() -> bool {
    let Some(path) = global_settings_path() else {
        return false;
    };
    let Ok(data) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(obj) = serde_json::from_str::<Value>(&data) else {
        return false;
    };
    let marker = format!("127.0.0.1:{PREFERRED_PORT}/hook");
    obj.get("hooks")
        .and_then(|h| h.get("Stop"))
        .and_then(|s| s.as_array())
        .map(|matchers| {
            matchers.iter().any(|m| {
                m.get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|inner| {
                        inner.iter().any(|i| {
                            i.get("command")
                                .and_then(|c| c.as_str())
                                .map(|c| c.contains(&marker))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Merge Stop + Notification hooks into ~/.claude/settings.json so EVERY
/// claude session on this machine reports to Perla whenever she's running
/// (and no-ops within 3s when she isn't). Idempotent; upgrades our stale
/// entries in place; preserves the user's own hooks. Opt-in — never called
/// automatically.
pub fn install_global_hooks() -> anyhow::Result<()> {
    let path = global_settings_path().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let mut root: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    let command = hook_command(PREFERRED_PORT, &auth_token());
    let entry = json!({ "hooks": [{ "type": "command", "command": command }] });
    let marker = format!("127.0.0.1:{PREFERRED_PORT}/hook");

    let hooks = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings.json is not an object"))?
        .entry("hooks")
        .or_insert_with(|| json!({}));
    for event in ["Stop", "Notification"] {
        let matchers = hooks
            .as_object_mut()
            .unwrap()
            .entry(event)
            .or_insert_with(|| json!([]));
        let arr = matchers.as_array_mut().unwrap();
        let mut present = false;
        for m in arr.iter_mut() {
            if let Some(inner) = m.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                for i in inner.iter_mut() {
                    let is_ours = i
                        .get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains(&marker))
                        .unwrap_or(false);
                    if is_ours {
                        i["command"] = Value::String(command.clone());
                        present = true;
                    }
                }
            }
        }
        if !present {
            arr.push(entry.clone());
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&root)?)?;
    Ok(())
}
