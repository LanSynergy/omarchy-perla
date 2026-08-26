//! Built-in fast tools — pure filesystem / OS helpers with no agent involved.
//! Port of the "Fast tools" section of `ToolDispatcher.swift`.

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::types::ToolResult;

const READ_CAP: usize = 4096;

/// Expand a leading `~` and normalize the path.
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs_home() {
            return home.join(rest);
        }
    }
    if path == "~" {
        if let Some(home) = dirs_home() {
            return home;
        }
    }
    PathBuf::from(path)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub async fn read_file(path: &str) -> ToolResult {
    let expanded = expand_tilde(path);
    let path_owned = path.to_string();
    tokio::task::spawn_blocking(move || {
        if !expanded.exists() {
            return ToolResult::failure(json!({ "path": path_owned, "error": "not found" }));
        }
        match std::fs::read(&expanded) {
            Ok(data) => {
                let slice = &data[..data.len().min(READ_CAP)];
                let content = String::from_utf8_lossy(slice).to_string();
                ToolResult::success(json!({
                    "path": expanded.to_string_lossy(),
                    "content": content,
                    "truncated": data.len() > READ_CAP,
                    "size": data.len(),
                }))
            }
            Err(_) => ToolResult::failure(json!({ "path": path_owned, "error": "unreadable" })),
        }
    })
    .await
    .unwrap_or_else(|_| ToolResult::error("read task panicked"))
}

pub async fn list_dir(path: &str) -> ToolResult {
    let expanded = expand_tilde(path);
    let path_owned = path.to_string();
    tokio::task::spawn_blocking(move || {
        if !expanded.exists() {
            return ToolResult::failure(json!({ "path": path_owned, "error": "not found" }));
        }
        match std::fs::read_dir(&expanded) {
            Ok(rd) => {
                let mut entries: Vec<String> = rd
                    .filter_map(|e| e.ok())
                    .map(|e| {
                        let kind = if e.path().is_dir() { "dir" } else { "file" };
                        format!("{kind}\t{}", e.file_name().to_string_lossy())
                    })
                    .collect();
                entries.sort();
                ToolResult::success(json!({
                    "path": expanded.to_string_lossy(),
                    "entries": entries,
                }))
            }
            Err(e) => ToolResult::failure(json!({
                "path": expanded.to_string_lossy(),
                "error": e.to_string(),
            })),
        }
    })
    .await
    .unwrap_or_else(|_| ToolResult::error("list task panicked"))
}

/// Open a file with the platform's default handler.
pub async fn open_in_editor(path: &str) -> ToolResult {
    let expanded = expand_tilde(path);
    if !expanded.exists() {
        return ToolResult::failure(json!({
            "path": expanded.to_string_lossy(),
            "error": "not found",
        }));
    }
    let opened = open_with_system(&expanded).await;
    if opened {
        ToolResult::success(json!({ "opened": expanded.to_string_lossy() }))
    } else {
        ToolResult::failure(json!({
            "path": expanded.to_string_lossy(),
            "error": "could not open with system handler",
        }))
    }
}

async fn open_with_system(target: &Path) -> bool {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = tokio::process::Command::new("/usr/bin/open");
        c.arg(target);
        c
    };
    #[cfg(target_os = "linux")]
    let mut cmd = {
        let mut c = tokio::process::Command::new("xdg-open");
        c.arg(target);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = tokio::process::Command::new("cmd");
        c.args(["/C", "start", ""]).arg(target);
        c
    };
    matches!(cmd.status().await, Ok(s) if s.success())
}
