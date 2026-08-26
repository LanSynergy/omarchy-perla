# perla-voice

An embeddable, cross-platform **voice-agent engine** in Rust — the port of the
macOS Perla app's battle-tested architecture. You talk; Perla does — never
talking over you, never re-sending a task, never losing a completion.

It is a **library first** (embed it in a CMS, a coding harness, a tray app, anything
that can consume an event stream) plus a thin demo TUI.

Two execution modes:

- **hands** (default) — Perla is one independent assistant whose *hands* are a
  persistent headless [grok-build](https://github.com/xai-org/grok-build) session,
  driven over ACP (JSON-RPC on stdio). Files, shell, web search, research,
  multi-step builds — and the hands can themselves launch Claude Code / Codex when
  asked. Because the integration is a *protocol*, the grok fork stays trivially
  in sync with upstream.
- **agents** — the macOS app's original shape: Perla routes between the Claude
  Code and Codex CLIs in hidden PTYs, reverse-engineering their state from JSONL
  transcripts.

Plus **the board**: when [Herdr](https://herdr.dev) (the agent terminal
multiplexer) is installed and running, Perla auto-gains board powers on top of
hands mode. Run `perla-h` from any terminal and it installs itself into a pinned
pane of a "Perla" herdr workspace and attaches the herdr UI. From there she can:

- `start_agent` — open claude / codex / grok in a **visible tab** you can watch
  and type into,
- `run_command` — dev servers, tests, npm, anything, in a visible tab; the
  process is watched, and Perla announces exits and crashes (with the likely
  cause from the output) unprompted,
- `check_board` — see *everything* running in the session, including agents you
  started by hand, with live working / idle / **blocked** states,
- `steer_agent` / `stop_agent` / `read_pane` — talk to, interrupt, or read any
  agent on the board by name.

A board watcher polls agent states and Perla speaks the news: *"claude in Clase
is blocked — it's asking whether to drop the migration"*. Quick invisible work
still goes through her own hands (headless grok).

Her hands can also control **the Mac itself** via the
[macos-harness](https://github.com/browser-use/macos-harness) skill installed
at `~/.grok/skills/macos-harness/` — screenshots of any app window, clicks and
typing without moving your cursor, accessibility-tree reading, AppleScript,
and your real logged-in Chrome. "Open Spotify and play my playlist" is just
another `run_task`. Requires granting Screen Recording + Accessibility to the
terminal app on first use (`macos-harness doctor` to check).

Perla is also a **first-class citizen of the sidebar**: via herdr's
custom-integration protocol (`pane report-agent`) she registers herself as an
agent, so her pane shows live `working` / `idle` / `blocked` states and a
title like *"Perla — Working on auth"*. Held updates surface as `blocked`
("2 updates waiting — ask Perla") so a glance at the sidebar tells you she has
news.

```
 you ──speech──▶ OpenAI Realtime (WebSocket) ──function calls──▶ tool dispatcher
                       ▲                                              │
                       │ side-channel narration            fast tools │ agent tools
                       │ (milestones, completions)                    ▼
                 side-channel queue ◀── narration engine ◀── AgentOrchestrator
                                                                  │ hidden PTYs
                                                                  ▼
                                                     claude / codex CLI sessions
                                                     (JSONL transcript tailing,
                                                      turn-end detection, digests)
```

## Quick start (demo TUI)

Prereqs: a microphone + speaker, an OpenAI API key, and optionally the `claude`
and/or `codex` CLIs on your PATH for agent tasks.

```bash
export PERLA_OPENAI_API_KEY=sk-...
cargo run -p perla-cli
```

Press `s` to start the session and just talk: *"have claude add a health-check
endpoint to this project"*. Perla fast-acks, the agent works in a hidden PTY, and
you get spoken milestone updates and a completion verdict. Interrupt her
mid-sentence — barge-in kills the audio instantly. Press `t` to type a task
instead of speaking it.

## Configuration

`perla-voice.toml` in the working directory (or `~/.config/perla-voice/config.toml`):

```toml
provider = "openai"          # or "grok"
voice = "marin"
# voice_language = "ar"      # pin every reply to one language; omit = follow the user

mode = "hands"               # "hands" (grok-build session) | "agents" (claude/codex CLIs)
# hands_binary = "~/.grok/bin/grok"   # auto-discovered when unset
# hands_model = "grok-build"
# herdr = true               # board integration; omit = auto when herdr is present

workspace = "~/code/my-project"
runtime = "claude"           # agents mode only: "claude" | "codex"
# agent_model = "opus"
# agent_effort = "high"

detail_mode = false          # completion-only is the low-cost default
big_moments_only = true      # when enabled, skip "now starting" chatter
hold_announcements = false   # queue finished-agent updates until you ask
start_muted = false          # true = push-to-talk style

rotate_after_secs = 3000     # pre-cap session rotation (server caps ~60 min)
idle_stop_secs = 180         # end an unused metered session
max_output_tokens = 768      # cap one spoken response
context_token_limit = 8000   # retention-ratio context ceiling
retention_ratio = 0.8        # keep prompt caching stable across truncation

# Omarchy (Linux/Hyprland). Defaults on on Linux, off elsewhere.
# [omarchy]
# harness = true                 # grok skill: omarchy-harness see/click/type
# fast_desktop_tools = true      # launch_or_focus, omarchy_run, summon, desktop_state
# confirm_destructive = true     # shutdown/reboot/pkg need a spoken yes

[vad]
silence_duration_ms = 1000   # raise it and Perla waits longer before replying

[openai]
# api_key = "sk-..."         # or PERLA_OPENAI_API_KEY / OPENAI_API_KEY
# model = "gpt-realtime-2.1-mini"  # low-cost production default

[grok]
# api_key = "xai-..."        # or PERLA_XAI_API_KEY / XAI_API_KEY
```

## Embedding

```rust
use perla_core::{Config, Engine, EngineCommand, EngineEvent};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::load(None)?;
    let (engine, mut events) = Engine::start(config);

    engine.send(EngineCommand::Start);
    while let Some(event) = events.recv().await {
        match event {
            EngineEvent::Transcript(line) => println!("{:?}: {}", line.role, line.text),
            EngineEvent::AgentRunning { tool, cwd, running } => { /* render a status strip */ }
            _ => {}
        }
    }
    Ok(())
}
```

Two extension seams:

- **`ToolDispatcher`** (`perla-tools`) — answer the model's function calls yourself.
  Wrap the built-in `AgentDispatcher` to add tools (`publish_article`, `deploy`,
  …) or replace it entirely, then boot with `Engine::start_with_dispatcher`.
- **`AgentBackend`** (`perla-agents`) — swap "the agent" itself: instead of the
  CLI-in-a-PTY backend, plug in your own coding harness or job runner while
  keeping the fast-ack / dedup / queue orchestration semantics.

## Crates

| crate | what it owns |
|---|---|
| `perla-core` | engine actor: session state machine, reconnect + pre-cap rotation with recap injection, side-channel queue, language lock, cost tracking |
| `perla-provider` | OpenAI Realtime / Grok over WebSocket (local API keys, no relay) |
| `perla-audio` | cpal capture/playback, 24 kHz PCM16 resampling, mic meter, source-level mute |
| `perla-tools` | tool schemas, `ToolDispatcher` trait, fast file tools, the orchestrator system prompt |
| `perla-agents` | hidden-PTY agent sessions, JSONL transcript parsers (fixture-tested), fast-ack submit + dedup + queue, Claude Stop/Notification hook server, narration milestone engine |
| `perla-hands` | the hands: a persistent headless grok-build session over ACP (JSON-RPC/stdio) |
| `perla-herdr` | the board: herdr CLI client, board watcher (state-change events), visible-tab agent/command spawning |
| `perla-cli` | ratatui demo (`perla-h`) and the Omarchy sidecar daemon (`perla-d`) |

## Omarchy daemon

On Omarchy, do not run the TUI as the desktop host. Run `perla-d`:

```sh
cargo install --path crates/perla-cli --bin perla-d
mkdir -p ~/.config/systemd/user
cp packaging/perla.service ~/.config/systemd/user/
systemctl --user enable --now perla.service
perla-d start
```

The bar plugin watches `$XDG_RUNTIME_DIR/perla/state.json` and sends `perla-d toggle-listen` / `mute` / `stop`. Fast tools (`desktop_state`, `launch_or_focus`, `omarchy_run`, `summon`, `notify`) drive Hyprland and the Omarchy CLI without grok. Seeing, clicking, and typing inside apps is `run_task` via [omarchy-harness](../omarchy-harness). Voxtype still owns dictation into the focused field.

## The hard-won behaviors (ported 1:1 from the macOS app)

- **Fast-ack agent tools** — `run_claude_agent` returns `submitted` the instant the
  prompt is typed; the result is narrated out-of-band minutes later. A re-send of
  the same task bounces as `already_running`; a new task while one runs is
  `queued` and auto-starts on a *clean* turn end only.
- **`finish_turn` idempotence** — JSONL tail, Claude Stop hook, interrupt, exit
  watch and End all race to one completion point; first signal wins.
- **Barge-in double-kill** — `response.cancel` stops generation server-side AND the
  local playback queue is cleared, so "stop" actually stops.
- **The gap guard** — from the moment you stop speaking until the model answers,
  proactive announcements hold their tongue (self-releasing after 3 s so a cough
  can't stall the queue).
- **Milestone truthfulness** — a narration fact only counts as "already told the
  user" when it was actually spoken, so the completion never repeats — and never
  omits — news.
- **Transparent transport swaps** — reconnects and the ~50-minute pre-cap rotation
  inject a conversation recap into the fresh session so Perla never re-greets you
  mid-call; agent sessions are untouched.
- **Bounded Realtime context** — responses are capped, old context is truncated
  with a cache-friendly retention ratio, and one-turn screenshots/progress audio
  are deleted after use instead of being billed again on every later turn.

## Tests

```bash
cargo test --workspace
```

The transcript parsers (turn-end, interrupt, session-id, digests for both Claude
and Codex JSONL) are covered by fixtures mirroring the macOS `ParserTests` — they
are the canary for CLI format changes.

## Platform notes

- Audio: cpal (CoreAudio on macOS, ALSA on Linux — `apt install libasound2-dev`,
  WASAPI on Windows).
- PTYs: `portable-pty`; no shell scripting or terminal automation required.
- Echo cancellation: none built in yet — use headphones, or enable your OS-level
  voice-processing input. The `aec` feature flag on `perla-audio` is the seam.
