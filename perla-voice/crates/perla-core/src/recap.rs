//! Cross-session memory: a compact recap of the last call per workspace, so
//! "pick up where we left off" works. Port of RealtimeSession's recap
//! persistence (UserDefaults → a JSON file under the platform data dir).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const MAX_AGE: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    text: String,
    at_epoch_secs: u64,
}

fn store_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("perla-voice/session-recaps.json"))
}

fn load() -> HashMap<String, Entry> {
    let Some(path) = store_path() else {
        return HashMap::new();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(map: &HashMap<String, Entry>) {
    let Some(path) = store_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(map) {
        let _ = std::fs::write(path, json);
    }
}

/// Persist a recap keyed by workspace when the call ends.
pub fn persist(workspace: &str, recap: &str) {
    if recap.is_empty() {
        return;
    }
    let mut map = load();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    map.insert(
        workspace.to_string(),
        Entry {
            text: recap.to_string(),
            at_epoch_secs: now,
        },
    );
    save(&map);
}

/// The stored recap for a workspace, if fresh enough (24h) to still help.
pub fn stored(workspace: &str) -> Option<String> {
    let map = load();
    let entry = map.get(workspace)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let age = now.saturating_sub(entry.at_epoch_secs);
    if age > MAX_AGE.as_secs() || entry.text.is_empty() {
        return None;
    }
    let hours = age / 3600;
    let when = if hours < 1 {
        "under an hour ago".to_string()
    } else {
        format!("about {hours}h ago")
    };
    Some(format!(
        "Your last voice session in this workspace ({when}) ended like this:\n{}",
        entry.text
    ))
}
