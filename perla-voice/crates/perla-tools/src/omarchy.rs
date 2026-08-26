//! Fast Omarchy desktop tools: named Hyprland/Omarchy actions without grok.
//! Anything that needs to see or click inside an app still goes through
//! `run_task` (omarchy-harness). Destructive commands require `confirmed`.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::dispatcher::{ToolCallContext, ToolDispatcher};
use crate::types::{ToolDef, ToolResult};

pub const OMARCHY_TOOL_NAMES: &[&str] = &[
    "desktop_state",
    "launch_or_focus",
    "omarchy_run",
    "summon",
    "notify",
];

pub fn omarchy_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "desktop_state",
            description: "Read the Omarchy/Hyprland desktop: windows (class, title, workspace, address), workspaces, focused window, and cursor. Cheap and exact. Use this instead of a screenshot when the user asks what is open, which workspace they are on, or where a window is. Read-only.",
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "launch_or_focus",
            description: "Open an Omarchy app or focus it if it is already running. Use for 'open the browser/terminal/spotify/files/editor' and named apps. Prefer this over run_task for simple launch/focus.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "app": {
                        "type": "string",
                        "description": "Window class/title pattern or a well-known name: browser, terminal, spotify, editor, files, nautilus."
                    }
                },
                "required": ["app"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "omarchy_run",
            description: "Run one allowlisted Omarchy or hyprctl command. Use for theme, volume, brightness, workspace switch, window focus/close/fullscreen, plugin list, screenshot of the focused monitor. Destructive actions (shutdown, reboot, logout, close-all, package add/remove) require confirmed=true AFTER the user said yes by voice.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "A single command, e.g. 'omarchy-theme-set \"Tokyo Night\"', 'hyprctl dispatch workspace 3', 'omarchy-audio-output-volume 10 --limit-to 100'."
                    },
                    "confirmed": {
                        "type": "boolean",
                        "description": "Must be true for destructive commands. Only set after the user confirmed by voice."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "summon",
            description: "Open or close an Omarchy shell surface by plugin id: omarchy.menu, omarchy.emojis, omarchy.clipboard, omarchy.image-picker, omarchy.clock. Use for 'open the menu', 'emoji picker', 'clipboard history'.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Plugin id, e.g. omarchy.menu" },
                    "action": { "type": "string", "enum": ["summon", "hide", "toggle"], "description": "Default toggle." },
                    "payload": { "type": "string", "description": "Optional JSON payload for summon/toggle, e.g. {\"menu\":\"root\"}." }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "notify",
            description: "Show a desktop notification. Short title and body only.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string" },
                    "body": { "type": "string" }
                },
                "required": ["title"],
                "additionalProperties": false
            }),
        },
    ]
}

#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(&self, program: &str, args: &[String]) -> Result<CommandOutput, String>;
}

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

pub struct SystemRunner;

#[async_trait]
impl CommandRunner for SystemRunner {
    async fn run(&self, program: &str, args: &[String]) -> Result<CommandOutput, String> {
        let out = tokio::process::Command::new(program)
            .args(args)
            .output()
            .await
            .map_err(|e| format!("failed to spawn {program}: {e}"))?;
        Ok(CommandOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

pub struct OmarchyDispatcher {
    runner: Arc<dyn CommandRunner>,
    confirm_destructive: bool,
}

impl OmarchyDispatcher {
    pub fn system(confirm_destructive: bool) -> Self {
        Self {
            runner: Arc::new(SystemRunner),
            confirm_destructive,
        }
    }

    pub fn with_runner(runner: Arc<dyn CommandRunner>, confirm_destructive: bool) -> Self {
        Self {
            runner,
            confirm_destructive,
        }
    }
}

#[async_trait]
impl ToolDispatcher for OmarchyDispatcher {
    async fn dispatch(&self, name: &str, args: Value, _ctx: ToolCallContext) -> ToolResult {
        match name {
            "desktop_state" => self.desktop_state().await,
            "launch_or_focus" => self.launch_or_focus(&args).await,
            "omarchy_run" => self.omarchy_run(&args).await,
            "summon" => self.summon(&args).await,
            "notify" => self.notify(&args).await,
            other => ToolResult::error(format!("unknown omarchy tool '{other}'")),
        }
    }
}

impl OmarchyDispatcher {
    async fn desktop_state(&self) -> ToolResult {
        let clients = self.hypr_json("clients").await;
        let workspaces = self.hypr_json("workspaces").await;
        let active = self.hypr_json("activewindow").await;
        let cursor = self
            .runner
            .run("hyprctl", &["cursorpos".into()])
            .await
            .ok()
            .map(|o| o.stdout);
        let mut payload = json!({ "ok": true });
        match clients {
            Ok(v) => payload["windows"] = v,
            Err(e) => payload["windows_error"] = json!(e),
        }
        match workspaces {
            Ok(v) => payload["workspaces"] = v,
            Err(e) => payload["workspaces_error"] = json!(e),
        }
        match active {
            Ok(v) => payload["active"] = v,
            Err(e) => payload["active_error"] = json!(e),
        }
        if let Some(c) = cursor {
            payload["cursor"] = json!(c);
        }
        if payload.get("windows_error").is_some()
            && payload.get("workspaces_error").is_some()
            && payload.get("active_error").is_some()
        {
            return ToolResult::error(
                "hyprctl is not available — this tool only works inside an Omarchy/Hyprland session",
            );
        }
        ToolResult::success(payload)
    }

    async fn hypr_json(&self, sub: &str) -> Result<Value, String> {
        let out = self
            .runner
            .run("hyprctl", &["-j".into(), sub.into()])
            .await?;
        if out.status != 0 {
            return Err(nonempty(&out.stderr, &out.stdout, "hyprctl failed"));
        }
        serde_json::from_str(&out.stdout).map_err(|e| format!("hyprctl {sub} is not JSON: {e}"))
    }

    async fn launch_or_focus(&self, args: &Value) -> ToolResult {
        let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("").trim();
        if app.is_empty() {
            return ToolResult::error("missing app");
        }
        let (program, extra) = resolve_launch(app);
        let out = self.runner.run(program, &extra).await;
        match out {
            Ok(o) if o.status == 0 => ToolResult::success(json!({
                "status": "ok",
                "app": app,
                "command": program,
            })),
            Ok(o) => ToolResult::error(nonempty(&o.stderr, &o.stdout, "launch failed")),
            Err(e) => ToolResult::error(e),
        }
    }

    async fn omarchy_run(&self, args: &Value) -> ToolResult {
        let raw = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let confirmed = args
            .get("confirmed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        match parse_allowlisted(raw) {
            // A refusal is usually an invented name, not a forbidden one — the
            // model reached for `omarchy-close-all` when the box ships
            // `omarchy-hyprland-window-close-all`. Naming the real neighbours
            // turns a dead end into a retry.
            Err(e) => ToolResult::error(match suggest_commands(raw) {
                s if s.is_empty() => e,
                s => format!("{e}. Did you mean: {}", s.join(", ")),
            }),
            Ok(plan) => {
                let needs_ok = (plan.destructive || plan.unlisted) && self.confirm_destructive;
                if needs_ok && !confirmed {
                    let note = if plan.destructive {
                        "Ask the user by voice to confirm this destructive action, then call again with confirmed=true."
                    } else {
                        "This command is not on the vetted list. Tell the user plainly what it will do and ask for a spoken yes, then call again with confirmed=true."
                    };
                    return ToolResult::success(json!({
                        "status": "needs_confirmation",
                        "note": note,
                        "destructive": plan.destructive,
                        "command": raw,
                    }));
                }
                match self.runner.run(&plan.program, &plan.args).await {
                    Ok(o) if o.status == 0 => ToolResult::success(json!({
                        "status": "ok",
                        "stdout": truncate(&o.stdout, 4000),
                    })),
                    Ok(o) => ToolResult::error(nonempty(&o.stderr, &o.stdout, "command failed")),
                    Err(e) => ToolResult::error(e),
                }
            }
        }
    }

    async fn summon(&self, args: &Value) -> ToolResult {
        let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("").trim();
        if id.is_empty() {
            return ToolResult::error("missing id");
        }
        if id.contains('/') || id.contains(' ') || id.contains('\n') {
            return ToolResult::error("invalid plugin id");
        }
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("toggle");
        if !matches!(action, "summon" | "hide" | "toggle") {
            return ToolResult::error("action must be summon, hide, or toggle");
        }
        let payload = args
            .get("payload")
            .and_then(|v| v.as_str())
            .unwrap_or("{}");
        let mut argv = vec!["shell".into(), action.into(), id.into()];
        if action != "hide" {
            argv.push(payload.into());
        }
        match self.runner.run("omarchy-shell", &argv).await {
            Ok(o) if o.status == 0 => ToolResult::success(json!({
                "status": "ok",
                "id": id,
                "action": action,
                "stdout": o.stdout,
            })),
            Ok(o) => ToolResult::error(nonempty(&o.stderr, &o.stdout, "omarchy-shell failed")),
            Err(e) => ToolResult::error(e),
        }
    }

    async fn notify(&self, args: &Value) -> ToolResult {
        let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
        if title.trim().is_empty() {
            return ToolResult::error("missing title");
        }
        let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("");
        let message = if body.is_empty() {
            title.to_string()
        } else {
            format!("{title}\n{body}")
        };
        match self
            .runner
            .run("omarchy-notification-send", &[message])
            .await
        {
            Ok(o) if o.status == 0 => ToolResult::success(json!({ "status": "ok" })),
            Ok(o) => ToolResult::error(nonempty(&o.stderr, &o.stdout, "notify failed")),
            Err(e) => ToolResult::error(e),
        }
    }
}

/// Routes Omarchy fast tools to the desktop dispatcher; everything else
/// continues to the inner (hands / herdr) dispatcher.
pub struct LayeredDispatcher {
    pub omarchy: Arc<OmarchyDispatcher>,
    pub inner: Arc<dyn ToolDispatcher>,
}

#[async_trait]
impl ToolDispatcher for LayeredDispatcher {
    async fn dispatch(&self, name: &str, args: Value, ctx: ToolCallContext) -> ToolResult {
        if OMARCHY_TOOL_NAMES.contains(&name) {
            self.omarchy.dispatch(name, args, ctx).await
        } else {
            self.inner.dispatch(name, args, ctx).await
        }
    }
}

/// Real command names from `$OMARCHY_PATH/bin` that share a meaningful word
/// with what was attempted. Cheap substring scoring beats an edit distance
/// here: the model's guesses are usually right about the *words* and wrong
/// about the arrangement (`close-all` vs `hyprland-window-close-all`).
/// Where the running Omarchy tree lives (env, then `/etc/omarchy.conf`, then
/// the packaged default).
fn omarchy_root() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("OMARCHY_PATH") {
        if !p.trim().is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    if let Ok(text) = std::fs::read_to_string("/etc/omarchy.conf") {
        for line in text.lines() {
            if let Some(rest) = line.trim().strip_prefix("export OMARCHY_PATH=") {
                let val = rest.trim().trim_matches('"').trim_matches('\'');
                if !val.is_empty() {
                    return std::path::PathBuf::from(val);
                }
            }
        }
    }
    std::path::PathBuf::from("/usr/share/omarchy")
}

/// Does this box actually ship the command? `bin/` is the source of truth; a
/// dev-linked checkout and a packaged install both resolve through it.
fn omarchy_command_exists(program: &str) -> bool {
    if program.contains('/') {
        return false;
    }
    omarchy_root().join("bin").join(program).exists()
        || std::path::Path::new("/usr/bin").join(program).exists()
}

fn suggest_commands(raw: &str) -> Vec<String> {
    let attempted = raw.split_whitespace().next().unwrap_or("").to_lowercase();
    let words: Vec<&str> = attempted
        .trim_start_matches("omarchy-")
        .split(['-', '_'])
        .filter(|w| w.len() > 2)
        .collect();
    if words.is_empty() {
        return Vec::new();
    }
    let root = std::env::var("OMARCHY_PATH")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/omarchy.conf").ok().and_then(|t| {
                t.lines().find_map(|l| {
                    l.trim()
                        .strip_prefix("export OMARCHY_PATH=")
                        .map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string())
                })
            })
        })
        .unwrap_or_else(|| "/usr/share/omarchy".to_string());

    let Ok(entries) = std::fs::read_dir(std::path::Path::new(&root).join("bin")) else {
        return Vec::new();
    };
    let mut scored: Vec<(usize, String)> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|name| name.starts_with("omarchy-"))
        .filter_map(|name| {
            let hits = words.iter().filter(|w| name.contains(*w)).count();
            (hits > 0).then_some((hits, name))
        })
        .collect();
    // Most words matched first; a stable name order keeps output predictable.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().take(4).map(|(_, n)| n).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPlan {
    pub program: String,
    pub args: Vec<String>,
    pub destructive: bool,
    /// A real command on this box that no one has vetted. Not refused — the
    /// user is asked first. Omarchy has no permission model of its own (plugins
    /// run unsandboxed as the user), so consent is the only gate there is, and
    /// a flat refusal just pushed the model into an agent fallback instead.
    pub unlisted: bool,
}

pub fn parse_allowlisted(raw: &str) -> Result<CommandPlan, String> {
    let tokens = tokenize(raw)?;
    if tokens.is_empty() {
        return Err("empty command".into());
    }
    let program = tokens[0].as_str();
    let rest = &tokens[1..];

    if is_destructive(program, rest) {
        return Ok(CommandPlan {
            program: program.into(),
            args: rest.to_vec(),
            destructive: true,
            unlisted: false,
        });
    }

    if program == "hyprctl" {
        validate_hyprctl(rest)?;
        return Ok(CommandPlan {
            program: program.into(),
            args: rest.to_vec(),
            destructive: false,
            unlisted: false,
        });
    }
    if program == "omarchy-shell" {
        validate_omarchy_shell(rest)?;
        return Ok(CommandPlan {
            program: program.into(),
            args: rest.to_vec(),
            destructive: false,
            unlisted: false,
        });
    }
    if allowed_set().contains(program) {
        return Ok(CommandPlan {
            program: program.into(),
            args: rest.to_vec(),
            destructive: false,
            unlisted: false,
        });
    }
    // Anything the box actually ships is offered to the user rather than
    // refused. Inventions still fall through to the error below.
    if program.starts_with("omarchy-") && omarchy_command_exists(program) {
        return Ok(CommandPlan {
            program: program.into(),
            args: rest.to_vec(),
            destructive: false,
            unlisted: true,
        });
    }

    Err(format!(
        "command '{program}' is not allowlisted or does not exist — call omarchy_help to find the real command name, or use launch_or_focus / summon / desktop_state"
    ))
}

fn is_destructive(program: &str, args: &[String]) -> bool {
    matches!(
        program,
        "omarchy-system-shutdown"
            | "omarchy-system-reboot"
            | "omarchy-system-logout"
            | "omarchy-hyprland-window-close-all"
            | "omarchy-pkg-add"
            | "omarchy-pkg-drop"
            | "omarchy-pkg-remove"
            | "omarchy-system-factory-reset"
    ) || (program == "omarchy" && args.first().map(String::as_str) == Some("pkg"))
}

fn validate_hyprctl(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("hyprctl needs a subcommand".into());
    }
    let mut i = 0;
    if args[0] == "-j" {
        i = 1;
    }
    if i >= args.len() {
        return Err("hyprctl -j needs a subcommand".into());
    }
    match args[i].as_str() {
        "clients" | "workspaces" | "activewindow" | "monitors" | "cursorpos" | "layers"
        | "devices" | "version" | "binds" => Ok(()),
        "dispatch" => {
            let action = args.get(i + 1).map(String::as_str).unwrap_or("");
            const ACTIONS: &[&str] = &[
                "workspace",
                "focuswindow",
                "closewindow",
                "fullscreen",
                "movetoworkspace",
                "togglefloating",
                "movecursor",
                "cyclenext",
                "togglesplit",
            ];
            if ACTIONS.contains(&action) {
                Ok(())
            } else {
                Err(format!("hyprctl dispatch '{action}' is not allowlisted"))
            }
        }
        other => Err(format!("hyprctl '{other}' is not allowlisted")),
    }
}

fn validate_omarchy_shell(args: &[String]) -> Result<(), String> {
    if args.first().map(String::as_str) != Some("shell") {
        return Err("omarchy-shell only allows the shell IPC target".into());
    }
    let method = args.get(1).map(String::as_str).unwrap_or("");
    const METHODS: &[&str] = &["ping", "summon", "hide", "toggle", "listPlugins"];
    if METHODS.contains(&method) {
        Ok(())
    } else {
        Err(format!("omarchy-shell shell '{method}' is not allowlisted"))
    }
}

fn resolve_launch(app: &str) -> (&'static str, Vec<String>) {
    let key = app.trim().to_lowercase();
    match key.as_str() {
        "browser" | "chromium" | "chrome" => ("omarchy-launch-browser", vec![]),
        "terminal" | "kitty" | "ghostty" | "alacritty" | "foot" => {
            ("omarchy-launch-terminal", vec![])
        }
        "spotify" => ("omarchy-launch-spotify", vec![]),
        "editor" => ("omarchy-launch-editor", vec![]),
        "files" | "nautilus" | "files app" => ("omarchy-launch-nautilus", vec![]),
        _ => (
            "omarchy-launch-or-focus",
            vec![app.to_string(), format!("uwsm-app -- {app}")],
        ),
    }
}

fn tokenize(raw: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = raw.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, '\'') | (None, '"') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (_, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (_, c) => cur.push(c),
        }
    }
    if quote.is_some() {
        return Err("unclosed quote in command".into());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

fn nonempty(stderr: &str, stdout: &str, fallback: &str) -> String {
    if !stderr.is_empty() {
        stderr.to_string()
    } else if !stdout.is_empty() {
        stdout.to_string()
    } else {
        fallback.to_string()
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

fn allowed_binaries() -> HashSet<&'static str> {
    [
        "omarchy-launch-or-focus",
        "omarchy-launch-browser",
        "omarchy-launch-terminal",
        "omarchy-launch-spotify",
        "omarchy-launch-editor",
        "omarchy-launch-nautilus",
        "omarchy-theme-set",
        "omarchy-theme-list",
        "omarchy-theme-current",
        "omarchy-audio-output-volume",
        "omarchy-audio-output-switch",
        "omarchy-audio-input-mute",
        "omarchy-brightness-display",
        "omarchy-notification-send",
        "omarchy-capture-screenshot",
        "omarchy-plugin-list",
        "omarchy-menu",
        "omarchy-osd",
        "omarchy-hyprland-session-locked",
        "omarchy-hyprland-window-gaps-toggle",
        "omarchy-hyprland-window-pop",
        "omarchy-reminder",
    ]
    .into_iter()
    .collect()
}

fn allowed_set() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(allowed_binaries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Scripted {
        calls: Mutex<Vec<(String, Vec<String>)>>,
        result: CommandOutput,
    }

    #[async_trait]
    impl CommandRunner for Scripted {
        async fn run(&self, program: &str, args: &[String]) -> Result<CommandOutput, String> {
            self.calls
                .lock()
                .unwrap()
                .push((program.into(), args.to_vec()));
            Ok(self.result.clone())
        }
    }

    fn ctx() -> ToolCallContext {
        ToolCallContext {
            call_id: "t".into(),
            history_id: None,
            started_at: std::time::SystemTime::now(),
        }
    }

    #[test]
    fn allowlist_accepts_theme_and_workspace() {
        let p = parse_allowlisted("omarchy-theme-set \"Tokyo Night\"").unwrap();
        assert_eq!(p.program, "omarchy-theme-set");
        assert_eq!(p.args, vec!["Tokyo Night"]);
        assert!(!p.destructive);

        let p = parse_allowlisted("hyprctl dispatch workspace 3").unwrap();
        assert_eq!(p.args[0], "dispatch");
        assert!(!p.destructive);
    }

    #[test]
    fn allowlist_rejects_exec_and_shutdown_without_flag_path() {
        assert!(parse_allowlisted("bash -c 'rm -rf /'").is_err());
        assert!(parse_allowlisted("hyprctl dispatch exec kitty").is_err());
        let p = parse_allowlisted("omarchy-system-shutdown").unwrap();
        assert!(p.destructive);
    }

    #[test]
    fn an_invented_command_is_still_refused() {
        // Nothing on any box is called this, so it must not become a consent
        // prompt — that is the difference between "unvetted" and "imaginary".
        assert!(parse_allowlisted("omarchy-definitely-not-a-real-command-xyz").is_err());
        assert!(parse_allowlisted("bash -c 'rm -rf /'").is_err());
    }

    #[test]
    fn a_real_but_unvetted_command_asks_instead_of_refusing() {
        let dir = std::env::temp_dir().join(format!("perla-allowlist-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/omarchy-unvetted-example"), b"#!/bin/sh\n").unwrap();
        std::env::set_var("OMARCHY_PATH", &dir);

        let plan = parse_allowlisted("omarchy-unvetted-example").expect("should plan, not refuse");
        assert!(plan.unlisted, "a real command must be offered for consent");
        assert!(!plan.destructive, "unvetted is not the same as destructive");

        // A destructive one keeps its stronger label even though it is listed.
        let shutdown = parse_allowlisted("omarchy-system-shutdown").unwrap();
        assert!(shutdown.destructive);

        std::env::remove_var("OMARCHY_PATH");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tokenize_quotes() {
        assert_eq!(
            tokenize(r#"omarchy-theme-set "Tokyo Night""#).unwrap(),
            vec!["omarchy-theme-set", "Tokyo Night"]
        );
    }

    #[tokio::test]
    async fn destructive_needs_confirmation() {
        let runner = Arc::new(Scripted {
            calls: Mutex::new(vec![]),
            result: CommandOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        });
        let d = OmarchyDispatcher::with_runner(runner.clone(), true);
        let result = d
            .dispatch(
                "omarchy_run",
                json!({"command": "omarchy-system-reboot"}),
                ctx(),
            )
            .await;
        assert_eq!(result.status(), Some("needs_confirmation"));
        assert!(runner.calls.lock().unwrap().is_empty());

        let result = d
            .dispatch(
                "omarchy_run",
                json!({"command": "omarchy-system-reboot", "confirmed": true}),
                ctx(),
            )
            .await;
        assert!(result.ok);
        assert_eq!(runner.calls.lock().unwrap()[0].0, "omarchy-system-reboot");
    }

    #[tokio::test]
    async fn summon_builds_argv() {
        let runner = Arc::new(Scripted {
            calls: Mutex::new(vec![]),
            result: CommandOutput {
                status: 0,
                stdout: "ok".into(),
                stderr: String::new(),
            },
        });
        let d = OmarchyDispatcher::with_runner(runner.clone(), true);
        let result = d
            .dispatch(
                "summon",
                json!({"id": "omarchy.menu", "action": "summon", "payload": "{\"menu\":\"root\"}"}),
                ctx(),
            )
            .await;
        assert!(result.ok);
        let call = &runner.calls.lock().unwrap()[0];
        assert_eq!(call.0, "omarchy-shell");
        assert_eq!(
            call.1,
            vec!["shell", "summon", "omarchy.menu", r#"{"menu":"root"}"#]
        );
    }

    #[test]
    fn launch_aliases() {
        assert_eq!(resolve_launch("browser").0, "omarchy-launch-browser");
        assert_eq!(resolve_launch("Spotify").0, "omarchy-launch-spotify");
        assert_eq!(resolve_launch("signal").0, "omarchy-launch-or-focus");
    }
}
