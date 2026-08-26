//! Fixture tests for the parsers that read EXTERNAL formats we don't control
//! — the Claude Code and Codex JSONL transcript shapes, and the narration
//! built on top of them. Port of the macOS app's ParserTests.swift: when
//! either CLI changes its format, these are the canary — the failure shows up
//! here instead of as a silent 10-minute "lost track of the agent" timeout.

use perla_agents::digest::{parse_claude, parse_codex, AgentDigest, Todo};
use perla_agents::narration::Narration;
use perla_agents::transcripts::{
    extract_session_id, newest_codex_jsonl, parse_turn_end, parse_turn_interrupt,
};
use perla_agents::types::AgentTool;

// ── Turn-end detection ──────────────────────────────────────────────────────

#[test]
fn claude_end_turn_yields_summary() {
    let line = r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"All done — three files touched."}]}}"#;
    assert_eq!(
        parse_turn_end(AgentTool::Claude, line).as_deref(),
        Some("All done — three files touched.")
    );
}

#[test]
fn claude_end_turn_without_text_falls_back_to_done() {
    let line = r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[]}}"#;
    assert_eq!(
        parse_turn_end(AgentTool::Claude, line).as_deref(),
        Some("Done.")
    );
}

#[test]
fn claude_tool_use_is_not_a_turn_end() {
    let line = r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/tmp/Foo.swift"}}]}}"#;
    assert_eq!(parse_turn_end(AgentTool::Claude, line), None);
}

#[test]
fn claude_user_line_is_not_a_turn_end() {
    let line = r#"{"type":"user","message":{"content":"do the thing"}}"#;
    assert_eq!(parse_turn_end(AgentTool::Claude, line), None);
}

#[test]
fn codex_task_complete_yields_last_message() {
    let line = r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"Finished the refactor."}}"#;
    assert_eq!(
        parse_turn_end(AgentTool::Codex, line).as_deref(),
        Some("Finished the refactor.")
    );
}

#[test]
fn codex_agent_message_is_not_a_turn_end() {
    let line =
        r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Working on it."}}"#;
    assert_eq!(parse_turn_end(AgentTool::Codex, line), None);
}

#[test]
fn garbage_lines_are_ignored() {
    assert_eq!(parse_turn_end(AgentTool::Claude, "not json at all"), None);
    assert_eq!(parse_turn_end(AgentTool::Codex, "{\"half\":"), None);
}

// ── Interrupt detection (Esc in the TUI) ────────────────────────────────────

#[test]
fn claude_interrupt_array_content() {
    let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#;
    assert!(parse_turn_interrupt(AgentTool::Claude, line));
}

#[test]
fn claude_interrupt_for_tool_use_variant() {
    let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}"#;
    assert!(parse_turn_interrupt(AgentTool::Claude, line));
}

#[test]
fn claude_interrupt_string_content() {
    let line =
        r#"{"type":"user","message":{"role":"user","content":"[Request interrupted by user]"}}"#;
    assert!(parse_turn_interrupt(AgentTool::Claude, line));
}

#[test]
fn claude_real_user_prompt_is_not_an_interrupt() {
    let line = r#"{"type":"user","message":{"role":"user","content":"please fix the login bug"}}"#;
    assert!(!parse_turn_interrupt(AgentTool::Claude, line));
}

#[test]
fn claude_assistant_line_is_not_an_interrupt() {
    let line = r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#;
    assert!(!parse_turn_interrupt(AgentTool::Claude, line));
}

#[test]
fn codex_turn_aborted() {
    let line = r#"{"timestamp":"2025-11-20T07:14:02.192Z","type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}"#;
    assert!(parse_turn_interrupt(AgentTool::Codex, line));
}

#[test]
fn codex_task_complete_is_not_an_interrupt() {
    let line =
        r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"Done."}}"#;
    assert!(!parse_turn_interrupt(AgentTool::Codex, line));
}

// ── Session-ID extraction ───────────────────────────────────────────────────

#[test]
fn claude_session_id_comes_from_filename() {
    let path = std::path::Path::new("/tmp/whatever/8f3c2a1e-1234-4abc-9def-000011112222.jsonl");
    assert_eq!(
        extract_session_id(AgentTool::Claude, path).as_deref(),
        Some("8f3c2a1e-1234-4abc-9def-000011112222")
    );
}

#[test]
fn codex_session_id_comes_from_session_meta_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"c0dex-5e55-10n\",\"cwd\":\"/tmp/proj\"}}\n",
    )
    .unwrap();
    assert_eq!(
        extract_session_id(AgentTool::Codex, &path).as_deref(),
        Some("c0dex-5e55-10n")
    );
}

// ── Codex transcript selection (global day dir → cwd-scoped pick) ──────────

#[test]
fn picks_newest_transcript_matching_cwd_not_global_newest() {
    let dir = tempfile::tempdir().unwrap();
    let write_session = |name: &str, cwd: &str| {
        let path = dir.path().join(name);
        std::fs::write(
            &path,
            format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{name}\",\"cwd\":\"{cwd}\"}}}}\n"),
        )
        .unwrap();
        path
    };
    let mine = write_session("mine.jsonl", "/proj/a");
    std::thread::sleep(std::time::Duration::from_millis(50));
    let _other = write_session("other.jsonl", "/proj/b"); // globally newest

    // Naive newest-by-mtime would return other.jsonl (the cross-talk bug);
    // the cwd-aware pick must return ours.
    let picked = newest_codex_jsonl(dir.path(), "/proj/a").unwrap();
    assert_eq!(picked.path.file_name(), mine.file_name());

    // No match for this cwd → fall back to the globally newest.
    let fallback = newest_codex_jsonl(dir.path(), "/proj/zzz").unwrap();
    assert_eq!(
        fallback.path.file_name().unwrap().to_str(),
        Some("other.jsonl")
    );
}

// ── Digest parsing (Claude) ─────────────────────────────────────────────────

fn claude_digest(lines: &[&str]) -> AgentDigest {
    let mut d = AgentDigest::default();
    parse_claude(lines, &mut d);
    d
}

#[test]
fn todos_parsed_from_todo_write() {
    let d = claude_digest(&[
        r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","name":"TodoWrite","input":{"todos":[{"content":"Add tests","status":"completed"},{"content":"Fix bug","status":"in_progress"},{"content":"Ship it","status":"pending"}]}}]}}"#,
    ]);
    assert_eq!(d.todos.len(), 3);
    assert_eq!(d.todos[0].text, "Add tests");
    assert_eq!(d.todos[0].status, "completed");
    assert_eq!(d.todos[1].status, "in_progress");
}

#[test]
fn changed_files_from_edit_and_write() {
    let d = claude_digest(&[
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/proj/A.swift"}}]}}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Write","input":{"file_path":"/proj/B.swift"}}]}}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/proj/A.swift"}}]}}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/proj/C.swift"}}]}}"#,
    ]);
    // Deduped, most-recent-last; Read is not a mutation.
    assert_eq!(d.changed_files, vec!["/proj/B.swift", "/proj/A.swift"]);
}

#[test]
fn last_message_and_turn_state() {
    let d = claude_digest(&[
        r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"text","text":"Working…"}]}}"#,
        r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"Done!"}]}}"#,
    ]);
    assert!(d.turn_complete);
    assert_eq!(d.last_message.as_deref(), Some("Done!"));
}

#[test]
fn recent_actions_describe_tool_and_target() {
    let d = claude_digest(&[
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"xcodebuild -scheme Perla build"}}]}}"#,
    ]);
    assert_eq!(
        d.recent_actions,
        vec!["Bash xcodebuild -scheme Perla build"]
    );
}

#[test]
fn user_interrupt_ends_the_turn() {
    // Esc mid-turn: the last assistant line has a non-end stop_reason, then
    // the synthetic interrupt user line lands. The turn is OVER.
    let d = claude_digest(&[
        r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"text","text":"Working…"}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#,
    ]);
    assert!(d.turn_complete);
}

// ── Digest parsing (Codex) ──────────────────────────────────────────────────

fn codex_digest(lines: &[&str]) -> AgentDigest {
    let mut d = AgentDigest::default();
    parse_codex(lines, &mut d);
    d
}

#[test]
fn plan_becomes_todos() {
    // `arguments` is a JSON-ENCODED STRING, not a nested object.
    let d = codex_digest(&[
        r#"{"type":"response_item","payload":{"type":"function_call","name":"update_plan","arguments":"{\"plan\":[{\"step\":\"Scan files\",\"status\":\"completed\"},{\"step\":\"Apply fix\",\"status\":\"in_progress\"}]}"}}"#,
    ]);
    assert_eq!(d.todos.len(), 2);
    assert_eq!(d.todos[0].text, "Scan files");
    assert_eq!(d.todos[1].status, "in_progress");
    assert!(!d.turn_complete);
}

#[test]
fn task_complete_sets_message_and_state() {
    let d = codex_digest(&[
        r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"Refactor landed."}}"#,
    ]);
    assert!(d.turn_complete);
    assert_eq!(d.last_message.as_deref(), Some("Refactor landed."));
}

#[test]
fn turn_aborted_ends_the_turn() {
    let d = codex_digest(&[
        r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Working on it."}}"#,
        r#"{"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}"#,
    ]);
    assert!(d.turn_complete);
}

#[test]
fn apply_patch_marks_changed_files() {
    // The \\n inside `arguments` survives the outer parse as a literal \n
    // escape inside the nested JSON string.
    let d = codex_digest(&[
        r#"{"type":"response_item","payload":{"type":"function_call","name":"apply_patch","arguments":"{\"input\":\"*** Begin Patch\\n*** Update File: /proj/Main.swift\\n@@\\n-a\\n+b\\n*** Add File: /proj/New.swift\\n+x\\n*** End Patch\"}"}}"#,
    ]);
    assert_eq!(d.changed_files, vec!["/proj/Main.swift", "/proj/New.swift"]);
}

#[test]
fn exec_command_with_heredoc_patch_marks_changed_files() {
    let d = codex_digest(&[
        r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"apply_patch <<'EOF'\\n*** Begin Patch\\n*** Update File: /proj/Thing.swift\\n*** End Patch\\nEOF\"}"}}"#,
    ]);
    assert_eq!(d.changed_files, vec!["/proj/Thing.swift"]);
    assert_eq!(d.recent_actions.len(), 1);
}

// ── Narration ───────────────────────────────────────────────────────────────

fn make_digest(todos: &[(&str, &str)], actions: &[&str]) -> AgentDigest {
    AgentDigest {
        todos: todos
            .iter()
            .map(|(t, s)| Todo {
                text: t.to_string(),
                status: s.to_string(),
            })
            .collect(),
        recent_actions: actions.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

#[test]
fn milestone_fires_once_per_completion() {
    let mut n = Narration::new();
    let first = make_digest(&[("Step A", "completed"), ("Step B", "in_progress")], &[]);
    assert!(n.ingest(&first, 5.0, true, false));
    assert!(n.drain().is_some());
    // Same snapshot again — nothing new, must stay silent.
    assert!(!n.ingest(&first, 8.0, true, false));
    assert!(n.drain().is_none());
}

#[test]
fn big_moments_only_skips_in_progress_changes() {
    let mut n = Narration::new();
    // Only the in-progress step changes — calm mode says nothing.
    assert!(!n.ingest(
        &make_digest(&[("A", "in_progress"), ("B", "pending")], &[]),
        5.0,
        true,
        true
    ));
    // A completion still speaks — while work remains.
    assert!(n.ingest(
        &make_digest(&[("A", "completed"), ("B", "in_progress")], &[]),
        10.0,
        true,
        true
    ));
}

/// The last box ticking means the turn is about to end and the completion
/// announcement will state the outcome — narrating here is the "it's done…
/// and now it's done again" double-announcement.
#[test]
fn all_todos_done_stays_silent() {
    let mut n = Narration::new();
    assert!(n.ingest(
        &make_digest(&[("A", "completed"), ("B", "in_progress")], &[]),
        5.0,
        true,
        false
    ));
    assert!(n.drain().is_some());
    // B finishes → every to-do complete → silent.
    assert!(!n.ingest(
        &make_digest(&[("A", "completed"), ("B", "completed")], &[]),
        9.0,
        true,
        false
    ));
    assert!(n.drain().is_none());
}

#[test]
fn milestone_carries_its_facts_for_the_completion() {
    let mut n = Narration::new();
    assert!(n.ingest(
        &make_digest(&[("A", "completed"), ("B", "in_progress")], &[]),
        5.0,
        true,
        false
    ));
    assert_eq!(n.drain().unwrap().facts, vec!["A"]);
    // All boxes ticked → suppressed → no pending, so no facts to log.
    assert!(!n.ingest(
        &make_digest(&[("A", "completed"), ("B", "completed")], &[]),
        9.0,
        true,
        false
    ));
    assert!(n.drain().is_none());
    // A fresh turn re-arms: an identically worded to-do must announce again.
    n.reset();
    assert!(n.ingest(
        &make_digest(&[("A", "completed"), ("C", "in_progress")], &[]),
        3.0,
        true,
        false
    ));
    assert_eq!(n.drain().unwrap().facts, vec!["A"]);
}

#[test]
fn disabled_stays_silent() {
    let mut n = Narration::new();
    assert!(!n.ingest(&make_digest(&[("A", "completed")], &[]), 5.0, false, false));
}

#[test]
fn completed_turn_stays_silent() {
    let mut n = Narration::new();
    let mut d = make_digest(&[("A", "completed")], &[]);
    d.turn_complete = true;
    assert!(!n.ingest(&d, 5.0, true, false));
}

#[test]
fn heartbeat_needs_actions_and_time() {
    let mut n = Narration::new();
    // No to-dos, no actions → silent.
    assert!(!n.ingest(&make_digest(&[], &[]), 30.0, true, false));
    // Actions but too early → silent.
    assert!(!n.ingest(&make_digest(&[], &["Bash ls"]), 5.0, true, false));
    // Settled in → heartbeat.
    assert!(n.ingest(&make_digest(&[], &["Bash ls"]), 30.0, true, false));
    // Immediately again → gated by the gap.
    assert!(!n.ingest(&make_digest(&[], &["Bash ls"]), 32.0, true, false));
}
