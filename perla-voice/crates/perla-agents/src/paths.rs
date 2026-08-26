//! Binary discovery + spawn environment — port of `AgentPaths.swift`.
//!
//! A GUI-launched (or service-launched) process inherits a bare PATH, and the
//! agents shell out constantly (git, node, rg, the project toolchain). We
//! hand-search common install locations first, then the user's REAL
//! login-shell PATH (captures nvm/rbenv/asdf installs a static list can't).

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use crate::types::AgentTool;

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

pub fn candidate_paths() -> Vec<PathBuf> {
    let h = home();
    vec![
        h.join(".local/bin"),
        h.join(".claude/local"),
        h.join(".cargo/bin"),
        h.join(".bun/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ]
}

fn is_executable(path: &PathBuf) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

pub fn find(binary: &str) -> Option<PathBuf> {
    for dir in candidate_paths() {
        let full = dir.join(binary);
        if is_executable(&full) {
            return Some(full);
        }
    }
    for dir in login_shell_path().split(':') {
        if dir.is_empty() {
            continue;
        }
        let full = PathBuf::from(dir).join(binary);
        if is_executable(&full) {
            return Some(full);
        }
    }
    None
}

pub fn binary_for(tool: AgentTool) -> Option<PathBuf> {
    find(tool.binary_name())
}

/// PATH for a spawned agent: login-shell PATH, then the candidate dirs, then
/// whatever this process inherited. Deduped, first occurrence wins.
pub fn augmented_path() -> String {
    let mut seen = std::collections::HashSet::new();
    let mut parts: Vec<String> = Vec::new();
    let mut push = |p: &str| {
        if !p.is_empty() && seen.insert(p.to_string()) {
            parts.push(p.to_string());
        }
    };
    for p in login_shell_path().split(':') {
        push(p);
    }
    for p in candidate_paths() {
        push(&p.to_string_lossy());
    }
    push("/usr/sbin");
    push("/sbin");
    for p in std::env::var("PATH").unwrap_or_default().split(':') {
        push(p);
    }
    parts.join(":")
}

/// Environment for a spawned agent PTY: inherited env with PATH augmented and
/// the usual terminal vars filled in.
pub fn terminal_environment() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    let mut set = |key: &str, value: String, only_if_absent: bool| {
        if let Some(slot) = env.iter_mut().find(|(k, _)| k == key) {
            if !only_if_absent {
                slot.1 = value;
            }
        } else {
            env.push((key.to_string(), value));
        }
    };
    set("PATH", augmented_path(), false);
    set("TERM", "xterm-256color".into(), true);
    set("COLORTERM", "truecolor".into(), true);
    set("LANG", "en_US.UTF-8".into(), true);
    env
}

/// The user's real login-shell PATH, resolved once (blocking, ≤3s) and cached.
/// Empty string when resolution fails — callers just fall back.
fn login_shell_path() -> &'static str {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE.get_or_init(|| resolve_login_shell_path().unwrap_or_default())
}

fn resolve_login_shell_path() -> Option<String> {
    #[cfg(unix)]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        // Sentinel-wrapped so rc-file noise can't corrupt the value; hard
        // timeout so a hanging shell init can't wedge us.
        let (tx, rx) = std::sync::mpsc::channel();
        let child = std::process::Command::new(&shell)
            .args(["-ilc", "printf '__PERLA_PATH__%s__END__' \"$PATH\""])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .ok()?;
        let child_id = child.id();
        std::thread::spawn(move || {
            let out = child.wait_with_output().ok();
            let _ = tx.send(out);
        });
        let output = match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Some(out)) => out,
            _ => {
                #[allow(unsafe_code)]
                unsafe {
                    libc::kill(child_id as i32, libc::SIGKILL);
                }
                return None;
            }
        };
        let s = String::from_utf8_lossy(&output.stdout);
        let lo = s.find("__PERLA_PATH__")? + "__PERLA_PATH__".len();
        let hi = s.find("__END__")?;
        if hi < lo {
            return None;
        }
        let path = &s[lo..hi];
        path.contains('/').then(|| path.to_string())
    }
    #[cfg(not(unix))]
    {
        None
    }
}
