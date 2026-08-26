//! Builder-mode tool registry — port of `ToolDefs.swift`. Descriptions are
//! kept verbatim where possible: they are battle-tested prompt engineering.

use serde_json::json;

use crate::types::ToolDef;

/// The coding-orchestrator tool set registered with the realtime session.
pub fn builder_tools() -> Vec<ToolDef> {
    vec![
        run_claude_agent(),
        run_codex(),
        check_agent_session(),
        stop_agent(),
        steer_agent(),
        set_progress_updates(),
        get_usage(),
        switch_workspace(),
        review_with_other_agent(),
        read_file(),
        list_dir(),
        open_in_editor(),
    ]
}

/// The hands-mode tool set: Perla acts as ONE capable assistant whose hands
/// (a grok-build session) do files, shell, web search, and multi-step work.
/// Fewer tools, one verb for everything real.
pub fn hands_tools() -> Vec<ToolDef> {
    vec![
        run_task_tool(),
        check_task_tool(),
        stop_task_tool(),
        steer_task_tool(),
        set_progress_updates(),
        get_usage(),
        switch_workspace(),
        read_file(),
        list_dir(),
        open_in_editor(),
    ]
}

/// Board tools, added ON TOP of `hands_tools` when Perla lives inside Herdr:
/// visible agents and commands in tabs, plus whole-board awareness.
pub fn herdr_tools() -> Vec<ToolDef> {
    vec![
        start_agent_tool(),
        run_command_tool(),
        check_board_tool(),
        steer_agent_board_tool(),
        stop_agent_board_tool(),
        read_pane_tool(),
    ]
}

fn empty_params() -> serde_json::Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

pub fn start_agent_tool() -> ToolDef {
    ToolDef {
        name: "start_agent",
        description: "Start a coding agent (claude, codex, grok, …) in a NEW VISIBLE TAB the user can watch and type into. Use when the user asks for a specific agent by name, wants to watch the work happen, or wants a second agent alongside the current work. Returns as soon as the agent is ready (with the task submitted if given) — you'll get system notes when its state changes. For quick invisible work prefer run_task instead.",
        parameters: json!({
            "type": "object",
            "properties": {
                "kind": { "type": "string", "enum": ["claude", "codex", "grok"], "description": "Which agent CLI to run." },
                "task": { "type": "string", "description": "The first task to hand it. Omit to just open it." },
                "name": { "type": "string", "description": "Short memorable name (e.g. 'reviewer'). Auto-generated if omitted." }
            },
            "required": ["kind"],
            "additionalProperties": false
        }),
    }
}

pub fn run_command_tool() -> ToolDef {
    ToolDef {
        name: "run_command",
        description: "Run a plain shell command in a NEW VISIBLE TAB — dev servers, npm, tests, builds, logs. The tab stays open so the user can watch it, and you get a system note when the process exits (success or crash, with its exit code). Use read_pane for live output before then. For work that needs thinking, use run_task or start_agent instead.",
        parameters: json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to run." },
                "label": { "type": "string", "description": "Short tab label (e.g. 'dev-server')." }
            },
            "required": ["command"],
            "additionalProperties": false
        }),
    }
}

pub fn check_board_tool() -> ToolDef {
    ToolDef {
        name: "check_board",
        description: "See EVERYTHING running in the user's terminal session: every agent (including ones the user started by hand) with its live state — working, idle, or blocked — plus workspaces and task headlines. Call when the user asks what's running, what's happening across projects, or whether anything needs them. Read-only.",
        parameters: empty_params(),
    }
}

pub fn steer_agent_board_tool() -> ToolDef {
    ToolDef {
        name: "steer_agent",
        description: "Send a message or instruction to ANY agent on the board by its name — answer a question it's blocked on, add a requirement, redirect it. Works on agents the user started by hand too.",
        parameters: json!({
            "type": "object",
            "properties": {
                "agent": { "type": "string", "description": "The agent's name (from check_board or a system note)." },
                "message": { "type": "string", "description": "What to tell it." }
            },
            "required": ["agent", "message"],
            "additionalProperties": false
        }),
    }
}

pub fn stop_agent_board_tool() -> ToolDef {
    ToolDef {
        name: "stop_agent",
        description: "Interrupt an agent on the board (sends Escape into its pane). Its session stays open for a new direction. Use when the user says stop/pause/cancel about a named or visible agent.",
        parameters: json!({
            "type": "object",
            "properties": {
                "agent": { "type": "string", "description": "The agent's name or pane id." }
            },
            "required": ["agent"],
            "additionalProperties": false
        }),
    }
}

pub fn read_pane_tool() -> ToolDef {
    ToolDef {
        name: "read_pane",
        description: "Read the recent output of any agent or command pane on the board — what it printed, what it's asking, test results. Use to answer 'what is it saying / did the tests pass' for visible tabs.",
        parameters: json!({
            "type": "object",
            "properties": {
                "target": { "type": "string", "description": "Agent name or pane id." },
                "lines": { "type": "integer", "description": "How many recent lines (default 60)." }
            },
            "required": ["target"],
            "additionalProperties": false
        }),
    }
}

pub fn run_task_tool() -> ToolDef {
    ToolDef {
        name: "run_task",
            description: "Do real work with your hands: edit files, run commands, search the web, research, build, test, drive the desktop (see/click/type in any window via omarchy-harness on Omarchy/Hyprland or macos-harness on a Mac, drive the browser, read dialogs), or run other agents (claude, codex) — ANY multi-step or hands-on task. Prefer desktop_state / launch_or_focus / omarchy_run / summon for simple Omarchy actions. Returns IMMEDIATELY with status:\"submitted\" the moment the work is handed off — it does NOT wait for it to finish. You'll get a separate system note when it actually completes; relay that to the user then. NEVER re-send the SAME task — a repeat returns status:\"already_running\", which means your earlier request was NOT lost. A genuinely NEW task sent while one runs returns status:\"queued\" and starts automatically when the current one finishes (use steer_task to modify the running work, stop_task to halt it).",
        parameters: json!({
            "type": "object",
            "properties": {
                "task": { "type": "string", "description": "What to do, in natural language — include everything the user specified." }
            },
            "required": ["task"],
            "additionalProperties": false
        }),
    }
}

pub fn check_task_tool() -> ToolDef {
    ToolDef {
        name: "check_task",
        description: "Inspect A TASK YOU STARTED with run_task in the current workspace: whether it is still running or done, the to-do plan with progress, the latest message, and recent actions / file edits. Read-only — does NOT start or change any work. Call it when the user asks how THAT work is going: where it stands, whether it finished, how many steps are left. NOT for the desktop: questions about which windows or apps are open, what is on screen, or which workspace the user is on are desktop_state, never this.",
        parameters: empty_params(),
    }
}

pub fn stop_task_tool() -> ToolDef {
    ToolDef {
        name: "stop_task",
        description: "Interrupt the CURRENT work immediately. The session stays open so you can redirect right away. Use when the user says stop, hold on, wait, pause, cancel that, or wants to change course.",
        parameters: empty_params(),
    }
}

pub fn steer_task_tool() -> ToolDef {
    ToolDef {
        name: "steer_task",
        description: "Send a correction or extra instruction to work that is ALREADY running, without starting a new task — e.g. 'also handle the error case', 'use dark colors', 'skip the tests'. It is folded in right after the current step. If nothing is running, use run_task instead.",
        parameters: json!({
            "type": "object",
            "properties": {
                "message": { "type": "string", "description": "The instruction to fold into the running work." }
            },
            "required": ["message"],
            "additionalProperties": false
        }),
    }
}

pub fn run_claude_agent() -> ToolDef {
    ToolDef {
        name: "run_claude_agent",
        description: "Run a multi-step task with the local Claude Code CLI. Use for ANY work that touches the user's files, runs commands, edits code, or needs multi-step reasoning. Returns IMMEDIATELY with status:\"submitted\" the moment the task is handed off — it does NOT wait for the work to finish. You'll get a separate system note when the turn actually completes; relay that to the user then. NEVER re-send the SAME task — a repeat returns status:\"already_running\", which means your prompt was NOT lost. A genuinely NEW task sent while one is running returns status:\"queued\" and starts automatically the moment the current one finishes — tell the user it's queued (use steer_agent to modify the running task, stop_agent to halt it).",
        parameters: json!({
            "type": "object",
            "properties": {
                "task": { "type": "string", "description": "What the agent should do, in natural language." },
                "cwd": { "type": "string", "description": "Working directory. Defaults to the user's workspace." }
            },
            "required": ["task"],
            "additionalProperties": false
        }),
    }
}

pub fn run_codex() -> ToolDef {
    ToolDef {
        name: "run_codex",
        description: "Run a task with OpenAI's Codex CLI. Call this only when the user explicitly asks for Codex.",
        parameters: json!({
            "type": "object",
            "properties": {
                "task": { "type": "string" },
                "cwd": { "type": "string" }
            },
            "required": ["task"],
            "additionalProperties": false
        }),
    }
}

pub fn check_agent_session() -> ToolDef {
    ToolDef {
        name: "check_agent_session",
        description: "Inspect what the local coding agent (Claude Code / Codex) is doing in the current workspace: whether it's still working or done, its to-do list with progress, its last message, and recent file edits / commands. Read-only — does NOT start or change any work. Call it whenever the user asks what the agent is doing, where things stand, whether it's finished, how many items are left, or what's been built so far.",
        parameters: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    }
}

pub fn stop_agent() -> ToolDef {
    ToolDef {
        name: "stop_agent",
        description: "Interrupt the coding agent's CURRENT work — sends Escape to the live Claude Code / Codex session. The session stays open so you can immediately redirect it. Use when the user says stop, hold on, wait, pause, cancel that, or wants to change course. This does NOT shut down the session (that's the End button) — it just halts what the agent is doing right now.",
        parameters: empty_params(),
    }
}

pub fn steer_agent() -> ToolDef {
    ToolDef {
        name: "steer_agent",
        description: "Send an extra instruction to the coding agent while it is ALREADY working, without starting a new task — e.g. 'also handle the error case', 'use SwiftUI not AppKit', 'skip the tests'. Types the message into the live session and returns immediately. If nothing is running, start a task with run_claude_agent instead.",
        parameters: json!({
            "type": "object",
            "properties": {
                "message": { "type": "string", "description": "The instruction to inject into the running agent." }
            },
            "required": ["message"],
            "additionalProperties": false
        }),
    }
}

pub fn set_progress_updates() -> ToolDef {
    ToolDef {
        name: "set_progress_updates",
        description: "Control how much you proactively speak up about the coding agent's progress WHILE it works. Call this when the user asks you to change that — e.g. 'keep me posted' / 'give me updates' (mode: steps), 'just the big moments' / 'less chatty' (mode: big), 'quiet down' / 'stop narrating' / 'I'll ask' (mode: off). It only changes how often you volunteer updates; it does NOT start, stop, or check any work.",
        parameters: json!({
            "type": "object",
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["off", "steps", "big"],
                    "description": "off = don't volunteer updates (the user can still ask). steps = a short update on every step the agent finishes or starts. big = only when a step completes (calmer)."
                }
            },
            "required": ["mode"],
            "additionalProperties": false
        }),
    }
}

pub fn get_usage() -> ToolDef {
    ToolDef {
        name: "get_usage",
        description: "Report what this voice session has cost so far. Call when the user asks about cost, spend, or usage.",
        parameters: empty_params(),
    }
}

pub fn switch_workspace() -> ToolDef {
    ToolDef {
        name: "switch_workspace",
        description: "Switch the active workspace folder. Accepts a folder name or path and matches it against the user's recent workspaces. After switching, agent tasks run in the new folder (a separate session). Call when the user says things like 'switch to the marketing site' or 'work on the backend project'.",
        parameters: json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": "Folder name or path — matched against recent workspaces." }
            },
            "required": ["workspace"],
            "additionalProperties": false
        }),
    }
}

pub fn review_with_other_agent() -> ToolDef {
    ToolDef {
        name: "review_with_other_agent",
        description: "Have the OTHER coding agent review the current uncommitted changes in the workspace (Claude's work is reviewed by Codex and vice versa). Starts a review task in a separate session and reports back when it finishes, like run_claude_agent does. Call when the user asks for a second opinion, a cross-check, or a review of what was just built.",
        parameters: json!({
            "type": "object",
            "properties": {
                "focus": { "type": "string", "description": "Optional: what to focus the review on (e.g. 'the error handling')." }
            },
            "additionalProperties": false
        }),
    }
}

pub fn read_file() -> ToolDef {
    ToolDef {
        name: "read_file",
        description: "Read up to ~4KB of a single file at an absolute path.",
        parameters: json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
            "additionalProperties": false
        }),
    }
}

pub fn list_dir() -> ToolDef {
    ToolDef {
        name: "list_dir",
        description: "List entries of a directory at an absolute path.",
        parameters: json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
            "additionalProperties": false
        }),
    }
}

pub fn open_in_editor() -> ToolDef {
    ToolDef {
        name: "open_in_editor",
        description: "Open a file in the user's default editor / file handler.",
        parameters: json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
            "additionalProperties": false
        }),
    }
}
