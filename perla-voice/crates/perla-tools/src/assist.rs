//! Driving the desktop, and knowing what Omarchy is.
//!
//! `omarchy.rs` covers *named* actions — launch an app, set a theme, switch a
//! workspace. Two things were still missing, and both showed up the first time
//! someone actually talked to Perla on a real box:
//!
//! 1. **She could not type.** Anything that needed to put text or a keystroke
//!    into a focused window went through `run_task`, which is dispatched by
//!    `perla-hands` to the `grok` CLI. On a machine without grok installed —
//!    which is the normal case for a plain Omarchy install — that path is dead,
//!    so "type this in the terminal" simply failed. `omarchy-harness` was
//!    sitting right there, installed, with nothing able to reach it. These
//!    tools call it directly.
//!
//! 2. **She did not know how Omarchy works.** Asked "how does X work here" she
//!    answered from general training and got it wrong. The box ships the real
//!    answer: 429 commands carrying `# omarchy:` metadata, plus `docs/` and
//!    `manual/`. `omarchy_help` reads those instead of guessing.
//!
//! `see` is the exception to text-in/text-out: it returns a PNG, which reaches
//! the model as a separate image message (a function result cannot carry one).
//! Prefer `desktop_state` when the question is which windows exist — it is
//! cheaper and exact. Reach for `see` only when the answer is drawn in pixels.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use crate::dispatcher::{ToolCallContext, ToolDispatcher};
use crate::types::{ToolDef, ToolResult};

pub const ASSIST_TOOL_NAMES: &[&str] =
    &["see", "type_text", "press_key", "click_at", "omarchy_help"];

/// Bigger than this and the websocket frame is not worth it. A window capture
/// is normally well under; a 4K fullscreen one is not.
const MAX_SCREENSHOT_BYTES: u64 = 4 * 1024 * 1024;

/// The mental model, embedded so it is always available and always matches the
/// binary. Specifics (individual commands, current docs) come from the box via
/// `omarchy_help` — those drift, this does not.
const OMARCHY_OVERVIEW: &str = r#"Omarchy is an opinionated Arch Linux desktop built on Hyprland (a tiling Wayland compositor). Key facts:

- EVERYTHING is a command. ~429 `omarchy-*` scripts in $OMARCHY_PATH/bin form the API. `omarchy <group> <verb>` is a router over them: `omarchy theme set "Tokyo Night"` runs `omarchy-theme-set`. Every command carries metadata (summary, group, args, examples, whether it needs sudo). Ask `omarchy_help` for the real list before guessing a command name.
- The shell (top bar, menus, notifications, panels) is Quickshell/QML, not a normal panel. Talk to it with `omarchy-shell <target> <method>`, e.g. `omarchy-shell shell rescanPlugins`. Bar items are plugins with ids like `omarchy.clock`; user plugins live in ~/.config/omarchy/plugins/.
- Config is layered: packaged defaults in $OMARCHY_PATH/default/, user overrides in ~/.config/. Hyprland config is Lua (`o.bind("SUPER + K", "Label", "command")`), not the INI `bind =` syntax. Never edit files under $OMARCHY_PATH — they are package-owned and an update overwrites them.
- Themes are whole-system: one theme restyles terminal, editor, shell, borders and wallpaper together. `omarchy theme list` / `omarchy theme set`.
- Updates: `omarchy update` pulls new packages, runs migrations, and updates the checkout when running from a dev link.
- Keys: SUPER is the modifier for nearly everything. SUPER+K shows the hotkey cheatsheet, SUPER+SPACE opens the Omarchy menu, SUPER+RETURN a terminal.

When the user asks how something works, call `omarchy_help` and answer from what it returns. Do not invent command names."#;

pub fn assist_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "see",
            description: "Look at a window: takes a screenshot and returns the picture, so you can read what is actually drawn — error text, page contents, a video title, which button is where. Use it when the answer is on the screen rather than in the window list, and before clicking anything you have not been given coordinates for. Returns the window geometry too, so you can turn what you see into click_at coordinates.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "app": { "type": "string", "description": "Window class/title to look at, e.g. 'chromium'. Omit for the focused window." }
                },
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "type_text",
            description: "Type text into the focused window as if the user typed it — terminal commands, a search box, a message. Set press_enter=true to submit it (that is what 'run this in the terminal' means). Give `app` to focus a window first, otherwise it goes wherever focus already is.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Exact text to type." },
                    "app": { "type": "string", "description": "Optional window class/title to focus first, e.g. 'Alacritty'." },
                    "press_enter": { "type": "boolean", "description": "Press Return after typing. Use for terminal commands." }
                },
                "required": ["text"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "press_key",
            description: "Press one key or chord in the focused window, e.g. 'Return', 'Escape', 'ctrl+c', 'super+k'. Use for shortcuts, cancelling, or submitting — not for typing words.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "combo": { "type": "string", "description": "Key or chord: Return, Escape, Tab, ctrl+c, super+k." },
                    "app": { "type": "string", "description": "Optional window class/title to focus first." }
                },
                "required": ["combo"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "click_at",
            description: "Move the pointer to absolute screen coordinates and click. Use only when there is no command or keyboard route — prefer omarchy_run, launch_or_focus or press_key. Get coordinates from desktop_state window geometry.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "x": { "type": "integer", "description": "Absolute X in screen pixels." },
                    "y": { "type": "integer", "description": "Absolute Y in screen pixels." },
                    "app": { "type": "string", "description": "Optional window class/title to focus first." },
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "description": "Default left." }
                },
                "required": ["x", "y"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "omarchy_help",
            description: "Look up how Omarchy actually works on THIS machine: matching commands with their real arguments and examples, plus excerpts from the shipped docs and user manual. Call this before answering any 'how do I / how does it work' question about Omarchy, and before guessing a command name for omarchy_run. Omit `query` for the overview.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Topic or command fragment: 'theme', 'update', 'screenshot', 'waybar', 'plugin'. Omit for a general overview." }
                },
                "additionalProperties": false
            }),
        },
    ]
}

/// Where the running Omarchy tree lives. The daemon is a systemd user unit and
/// does not inherit the login shell's environment, so an unset `OMARCHY_PATH`
/// is normal — `/etc/omarchy.conf` is the same file the shell sources, and it
/// is also what a dev-linked checkout rewrites.
fn omarchy_path() -> PathBuf {
    if let Ok(p) = std::env::var("OMARCHY_PATH") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Ok(text) = std::fs::read_to_string("/etc/omarchy.conf") {
        for line in text.lines() {
            if let Some(rest) = line.trim().strip_prefix("export OMARCHY_PATH=") {
                let val = rest.trim().trim_matches('"').trim_matches('\'');
                if !val.is_empty() {
                    return PathBuf::from(val);
                }
            }
        }
    }
    PathBuf::from("/usr/share/omarchy")
}

pub struct AssistDispatcher {
    harness_bin: String,
}

impl AssistDispatcher {
    pub fn system() -> Self {
        Self {
            harness_bin: "omarchy-harness".into(),
        }
    }

    /// Run a Python program through the harness. The harness binds `oma` and
    /// execs whatever arrives on stdin, so arguments are injected as JSON
    /// literals rather than interpolated into a shell string — text Perla types
    /// is arbitrary user content and must never become syntax.
    async fn harness(&self, program: &str) -> Result<String, String> {
        let mut child = tokio::process::Command::new(&self.harness_bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                format!("omarchy-harness could not start ({e}). Install it with: uv tool install --editable ~/Work/perla/omarchy-harness")
            })?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(program.as_bytes())
                .await
                .map_err(|e| format!("writing to omarchy-harness: {e}"))?;
        }
        let out = child
            .wait_with_output()
            .await
            .map_err(|e| format!("omarchy-harness failed: {e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() {
            return Ok(stdout);
        }
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        // The harness raises with a plain message; the traceback tail is the
        // only useful part for the model.
        let msg = stderr.lines().last().unwrap_or("").trim().to_string();
        Err(if msg.is_empty() {
            "omarchy-harness failed".into()
        } else {
            msg
        })
    }

    /// The harness crops to the window, which keeps the payload small and the
    /// model's attention on the thing that was asked about.
    async fn see(&self, args: &Value) -> ToolResult {
        let program = format!(
            "import json\nprint(json.dumps(oma.see({})))\n",
            json_arg(args, "app")
        );
        let raw = match self.harness(&program).await {
            Ok(v) => v,
            Err(e) => return ToolResult::error(e),
        };
        let Ok(info) = serde_json::from_str::<Value>(&raw) else {
            return ToolResult::error(format!("unexpected harness reply: {}", truncate(&raw, 200)));
        };
        let Some(path) = info.get("path").and_then(Value::as_str) else {
            return ToolResult::error("harness returned no screenshot path".to_string());
        };
        match tokio::fs::metadata(path).await {
            Ok(m) if m.len() > MAX_SCREENSHOT_BYTES => {
                return ToolResult::error(format!(
                    "screenshot is {} MB, too large to send — look at a single window instead of the whole screen",
                    m.len() / (1024 * 1024)
                ))
            }
            Err(e) => return ToolResult::error(format!("reading screenshot: {e}")),
            _ => {}
        }
        let bytes = match tokio::fs::read(path).await {
            Ok(b) => b,
            Err(e) => return ToolResult::error(format!("reading screenshot: {e}")),
        };
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let mime = match info.get("format").and_then(Value::as_str) {
            Some("jpeg") => "image/jpeg",
            _ => "image/png",
        };
        // The capture is scaled down, so a point in the image is not a point on
        // the screen. Handing over the factor and the arithmetic is what keeps
        // clicks landing.
        let cap = info
            .get("capture_scale")
            .and_then(Value::as_f64)
            .filter(|v| *v > 0.0)
            .unwrap_or(1.0);
        // Deleted straight after encoding: these accumulate fast and nothing
        // downstream needs the file once it is on the wire.
        tokio::fs::remove_file(path).await.ok();

        let app = info.get("app").and_then(Value::as_str).unwrap_or("");
        let title = info.get("title").and_then(Value::as_str).unwrap_or("");
        ToolResult::success(json!({
            "status": "ok",
            "app": app,
            "title": title,
            "geometry": info.get("geometry").cloned().unwrap_or(Value::Null),
            "scale": info.get("scale").cloned().unwrap_or(Value::Null),
            "capture_scale": cap,
            "note": format!(
                "The screenshot is attached as an image, captured at {cap:.2}x scale. To click something you see at image point (ix, iy), pass click_at x = {gx} + round(ix / {cap:.2}), y = {gy} + round(iy / {cap:.2}). Read the picture first, then act.",
                cap = cap,
                gx = info.get("geometry").and_then(|g| g.get("x")).and_then(Value::as_i64).unwrap_or(0),
                gy = info.get("geometry").and_then(|g| g.get("y")).and_then(Value::as_i64).unwrap_or(0),
            ),
            "__image": format!("data:{mime};base64,{b64}"),
            "__image_caption": format!("Screenshot of {app} — {title}"),
        }))
    }

    async fn type_text(&self, args: &Value) -> ToolResult {
        let text = args.get("text").and_then(Value::as_str).unwrap_or("");
        if text.is_empty() {
            return ToolResult::error("text is required".to_string());
        }
        let app = json_arg(args, "app");
        let enter = args
            .get("press_enter")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let program = format!(
            "import json\noma.type({}, {})\n{}print(json.dumps({{\"ok\": True}}))\n",
            json_lit(text),
            app,
            if enter {
                format!("oma.key(\"Return\", {app})\n")
            } else {
                String::new()
            }
        );
        match self.harness(&program).await {
            Ok(_) => ToolResult::success(json!({
                "status": "ok",
                "typed": text,
                "submitted": enter,
            })),
            Err(e) => ToolResult::error(e),
        }
    }

    async fn press_key(&self, args: &Value) -> ToolResult {
        let combo = args.get("combo").and_then(Value::as_str).unwrap_or("");
        if combo.is_empty() {
            return ToolResult::error("combo is required".to_string());
        }
        let program = format!(
            "import json\noma.key({}, {})\nprint(json.dumps({{\"ok\": True}}))\n",
            json_lit(combo),
            json_arg(args, "app")
        );
        match self.harness(&program).await {
            Ok(_) => ToolResult::success(json!({ "status": "ok", "pressed": combo })),
            Err(e) => ToolResult::error(e),
        }
    }

    async fn click_at(&self, args: &Value) -> ToolResult {
        let (x, y) = match (
            args.get("x").and_then(Value::as_i64),
            args.get("y").and_then(Value::as_i64),
        ) {
            (Some(x), Some(y)) => (x, y),
            _ => return ToolResult::error("x and y are required".to_string()),
        };
        let button = args
            .get("button")
            .and_then(Value::as_str)
            .filter(|b| matches!(*b, "left" | "right" | "middle"))
            .unwrap_or("left");
        let program = format!(
            "import json\noma.click({}, {}, {}, {})\nprint(json.dumps({{\"ok\": True}}))\n",
            x,
            y,
            json_arg(args, "app"),
            json_lit(button)
        );
        match self.harness(&program).await {
            Ok(_) => ToolResult::success(json!({ "status": "ok", "clicked": [x, y], "button": button })),
            Err(e) => ToolResult::error(e),
        }
    }

    /// Ground truth beats recall: the command list comes from the router's own
    /// JSON, the prose from the docs shipped alongside it.
    async fn omarchy_help(&self, args: &Value) -> ToolResult {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_lowercase();

        if query.is_empty() {
            return ToolResult::success(json!({
                "overview": OMARCHY_OVERVIEW,
                "note": "Call again with a query for exact commands and docs.",
            }));
        }

        let root = omarchy_path();
        let commands = find_commands(&root, &query).await;
        let docs = find_docs(&root, &query);

        if commands.is_empty() && docs.is_empty() {
            return ToolResult::success(json!({
                "query": query,
                "matches": 0,
                "overview": OMARCHY_OVERVIEW,
                "note": "Nothing matched that word. Answer from the overview, or try a different term.",
            }));
        }
        ToolResult::success(json!({
            "query": query,
            "commands": commands,
            "docs": docs,
        }))
    }
}

/// `omarchy commands --json` is the router's own index — name, summary, group,
/// args and examples for every command on the box.
async fn find_commands(root: &std::path::Path, query: &str) -> Vec<Value> {
    let bin = root.join("bin/omarchy-commands");
    let out = tokio::process::Command::new(&bin)
        .arg("--json")
        .output()
        .await;
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    let Ok(parsed) = serde_json::from_slice::<Value>(&out.stdout) else {
        return Vec::new();
    };
    let Some(list) = parsed.as_array() else {
        return Vec::new();
    };
    let mut hits: Vec<Value> = Vec::new();
    for item in list {
        let hay = format!(
            "{} {} {}",
            item.get("name").and_then(Value::as_str).unwrap_or(""),
            item.get("summary").and_then(Value::as_str).unwrap_or(""),
            item.get("group").and_then(Value::as_str).unwrap_or(""),
        )
        .to_lowercase();
        if hay.contains(query) {
            hits.push(item.clone());
        }
        // A voice answer cannot read fifty commands aloud.
        if hits.len() >= 12 {
            break;
        }
    }
    hits
}

/// `docs/` explains the machinery, `manual/` explains it to a user. Both are
/// markdown, so a line-level grep with a little context is enough.
fn find_docs(root: &std::path::Path, query: &str) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for dir in ["docs", "manual"] {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
            .collect();
        files.sort();
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let title_hit = name.to_lowercase().contains(query);
            let mut excerpt: Vec<String> = Vec::new();
            for line in text.lines() {
                if line.to_lowercase().contains(query) {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        excerpt.push(truncate(trimmed, 240));
                    }
                }
                if excerpt.len() >= 6 {
                    break;
                }
            }
            if title_hit && excerpt.is_empty() {
                excerpt = text
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .take(6)
                    .map(|l| truncate(l.trim(), 240))
                    .collect();
            }
            if !excerpt.is_empty() {
                out.push(json!({ "file": format!("{dir}/{name}"), "lines": excerpt }));
            }
            if out.len() >= 5 {
                return out;
            }
        }
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

/// A JSON string literal is also a valid Python string literal, which is what
/// makes injecting arbitrary user text into the generated program safe.
fn json_lit(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// Optional window argument rendered as a Python literal — `None` when absent.
fn json_arg(args: &Value, key: &str) -> String {
    match args.get(key).and_then(Value::as_str) {
        Some(v) if !v.trim().is_empty() => json_lit(v),
        _ => "None".into(),
    }
}

#[async_trait]
impl ToolDispatcher for AssistDispatcher {
    async fn dispatch(&self, name: &str, args: Value, _ctx: ToolCallContext) -> ToolResult {
        match name {
            "see" => self.see(&args).await,
            "type_text" => self.type_text(&args).await,
            "press_key" => self.press_key(&args).await,
            "click_at" => self.click_at(&args).await,
            "omarchy_help" => self.omarchy_help(&args).await,
            other => ToolResult::error(format!("unknown assist tool: {other}")),
        }
    }
}

/// Same shape as `LayeredDispatcher`: claim our names, pass everything else on.
pub struct AssistLayer {
    pub assist: Arc<AssistDispatcher>,
    pub inner: Arc<dyn ToolDispatcher>,
}

#[async_trait]
impl ToolDispatcher for AssistLayer {
    async fn dispatch(&self, name: &str, args: Value, ctx: ToolCallContext) -> ToolResult {
        if ASSIST_TOOL_NAMES.contains(&name) {
            self.assist.dispatch(name, args, ctx).await
        } else {
            self.inner.dispatch(name, args, ctx).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_text_cannot_break_out_of_the_generated_program() {
        // Everything a quote could do to the surrounding Python is neutralised
        // by the escaping, so the literal must survive a round trip unchanged
        // and stay one single string.
        for nasty in [
            "\"); import os; os.system(\"rm -rf /\"); print(\"",
            "line one\nline two",
            "back\\slash",
            "unicode ✓ and \u{7}bell",
        ] {
            let lit = json_lit(nasty);
            assert!(lit.starts_with('"') && lit.ends_with('"'), "not a literal: {lit}");
            assert!(!lit.contains('\n'), "raw newline would end the statement: {lit}");
            let back: String = serde_json::from_str(&lit).expect("valid string literal");
            assert_eq!(back, nasty);
        }
    }

    #[test]
    fn absent_window_becomes_python_none() {
        assert_eq!(json_arg(&json!({}), "app"), "None");
        assert_eq!(json_arg(&json!({ "app": "  " }), "app"), "None");
        assert_eq!(json_arg(&json!({ "app": "Alacritty" }), "app"), "\"Alacritty\"");
    }

    #[test]
    fn overview_names_the_lua_binding_syntax() {
        assert!(OMARCHY_OVERVIEW.contains("o.bind("));
        assert!(OMARCHY_OVERVIEW.contains("$OMARCHY_PATH"));
    }
}
