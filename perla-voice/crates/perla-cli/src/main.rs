//! Demo TUI for the perla-voice engine.
//!
//! Proves the embedding contract: everything on screen renders from the
//! `EngineEvent` stream, everything the keys do goes through `EngineCommand`.
//! A host app (tray, web view, CMS) would wire the same two channels.
//!
//! Keys:
//!   s          start the voice session
//!   e          end it
//!   m / space  toggle mute (push-to-talk style)
//!   t          type a task (Enter sends, Esc cancels)
//!   u          hear updates held behind hold mode
//!   d          toggle detail-mode narration
//!   q          quit

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, Paragraph, Wrap};
use ratatui::Terminal;
use tokio::sync::mpsc;

use perla_core::events::{ConnectingPhase, Role, Speaker, Status};
use perla_core::{Config, Engine, EngineCommand, EngineEvent};
use perla_herdr::{HerdrClient, SelfReporter};

const TRANSCRIPT_KEEP: usize = 400;

/// `perla` run from a plain terminal, with herdr present: make sure the
/// "Perla" workspace exists, start perla inside its pinned (root) pane, and
/// replace this process with the herdr attach UI. The relaunched perla sees
/// HERDR_ENV=1 and runs the TUI normally — no recursion.
async fn bootstrap_into_herdr(
    config: &Config,
    config_path: Option<&std::path::Path>,
) -> Result<()> {
    use anyhow::{anyhow, Context};

    let client = HerdrClient::new().context("herdr binary not found")?;
    let workspaces = client.workspaces().await?;
    let existing = workspaces
        .iter()
        .find(|w| w.label.eq_ignore_ascii_case("perla"))
        .map(|w| w.workspace_id.clone());

    let (ws_id, pinned_pane) = match existing {
        Some(id) => {
            let panes = client.pane_ids(&id).await?;
            (id, panes.into_iter().next())
        }
        None => {
            let cwd = config.workspace.to_string_lossy().into_owned();
            let (id, pane) = client.workspace_create("Perla", &cwd).await?;
            (id, (!pane.is_empty()).then_some(pane))
        }
    };

    if let Some(pane) = pinned_pane {
        // Wait for the shell to reach its prompt (fresh panes need a beat).
        // If it never does, something is already running there (probably an
        // earlier perla) — just attach.
        if client
            .wait_for_prompt(&pane, std::time::Duration::from_secs(5))
            .await
            .is_ok()
        {
            let exe = std::env::current_exe().context("resolving own binary path")?;
            let mut command = shell_quote(&exe.to_string_lossy());
            if let Some(cfg) = config_path {
                command.push_str(" --config ");
                command.push_str(&shell_quote(&cfg.to_string_lossy()));
            }
            client.pane_run(&pane, &command).await?;
        }
    }
    let _ = client.workspace_focus(&ws_id).await;

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(client.binary()).exec();
        Err(anyhow!("couldn't attach herdr: {err}"))
    }
    #[cfg(not(unix))]
    {
        println!("Perla is set up in the 'Perla' herdr workspace — run `herdr` to attach.");
        Ok(())
    }
}

/// Single-quote a string for the pane's shell.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Mirrors Perla's state into the Herdr sidebar (working / idle / blocked +
/// a live title), so she reads like any other agent on the board. Dedupes —
/// only actual changes hit the herdr CLI.
struct Presence {
    reporter: Option<std::sync::Arc<SelfReporter>>,
    last: Option<(String, Option<String>)>,
    last_title: String,
}

impl Presence {
    fn new() -> Self {
        Self {
            reporter: SelfReporter::detect(),
            last: None,
            last_title: String::new(),
        }
    }

    fn sync(&mut self, app: &App) {
        let Some(reporter) = &self.reporter else { return };

        let agent_working = app.agents.iter().any(|(_, _, running)| *running);
        let (state, message) = if app.held_updates > 0 {
            (
                "blocked",
                Some(format!(
                    "{} update{} waiting — ask Perla",
                    app.held_updates,
                    if app.held_updates == 1 { "" } else { "s" }
                )),
            )
        } else if agent_working || app.status == Status::ToolRunning {
            ("working", None)
        } else {
            ("idle", None)
        };
        let snapshot = (state.to_string(), message.clone());
        if self.last.as_ref() != Some(&snapshot) {
            reporter.report(state, message);
            self.last = Some(snapshot);
        }

        let title = if let Some(activity) = &app.activity {
            format!("Perla — {activity}")
        } else {
            match app.status {
                Status::Connected | Status::ToolRunning => "Perla — listening".to_string(),
                Status::Connecting => "Perla — connecting…".to_string(),
                Status::Disconnected | Status::Error => "Perla — voice off (press s)".to_string(),
            }
        };
        if title != self.last_title {
            reporter.set_title(title.clone());
            self.last_title = title;
        }
    }

    async fn release(&self) {
        if let Some(reporter) = &self.reporter {
            reporter.release().await;
        }
    }
}

struct App {
    status: Status,
    error: Option<String>,
    reconnecting: bool,
    phase: Option<ConnectingPhase>,
    speaker: Speaker,
    muted: bool,
    mic_level: f32,
    cost_usd: f64,
    held_updates: usize,
    activity: Option<String>,
    agents: Vec<(String, String, bool)>, // (tool, cwd, running)
    transcript: VecDeque<(Role, String)>,
    detail_mode: bool,
    input: Option<String>, // Some = typing a task
    quitting: bool,
}

impl App {
    fn new(detail_mode: bool) -> Self {
        Self {
            status: Status::Disconnected,
            error: None,
            reconnecting: false,
            phase: None,
            speaker: Speaker::Idle,
            muted: false,
            mic_level: 0.0,
            cost_usd: 0.0,
            held_updates: 0,
            activity: None,
            agents: Vec::new(),
            transcript: VecDeque::new(),
            detail_mode,
            input: None,
            quitting: false,
        }
    }

    fn apply(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::Status {
                status,
                error,
                reconnecting,
                phase,
            } => {
                self.status = status;
                self.error = error;
                self.reconnecting = reconnecting;
                self.phase = phase;
            }
            EngineEvent::Speaker(s) => self.speaker = s,
            EngineEvent::Muted(m) => self.muted = m,
            EngineEvent::Transcript(line) => {
                self.transcript.push_back((line.role, line.text));
                while self.transcript.len() > TRANSCRIPT_KEEP {
                    self.transcript.pop_front();
                }
            }
            EngineEvent::AgentActivity(line) => self.activity = line,
            EngineEvent::AgentRunning { tool, cwd, running } => {
                self.agents.retain(|(t, c, _)| !(t == &tool && c == &cwd));
                self.agents.push((tool, cwd, running));
                self.agents.retain(|(_, _, r)| *r); // show live work only
            }
            EngineEvent::Cost { session_usd } => self.cost_usd = session_usd,
            EngineEvent::HeldUpdates(n) => self.held_updates = n,
            EngineEvent::MicLevel(v) => self.mic_level = v,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // The TUI owns stdout/stderr — route tracing to a file instead.
    if let Ok(filter) = std::env::var("PERLA_LOG") {
        if let Ok(file) = std::fs::File::create("perla-voice.log") {
            use tracing_subscriber::EnvFilter;
            let _ = tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::new(filter))
                .with_writer(std::sync::Mutex::new(file))
                .with_ansi(false)
                .try_init();
        }
    }

    let mut config_path: Option<std::path::PathBuf> = None;
    let mut no_herdr = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" | "-c" => config_path = args.next().map(Into::into),
            "--no-herdr" => no_herdr = true,
            "--help" | "-h" => {
                println!("perla-h — voice assistant\n\nUsage: perla-h [--config <path>] [--no-herdr]\n\nRun with herdr installed and `perla-h` moves itself into a pinned pane of the\n'Perla' herdr workspace and attaches. --no-herdr skips that and runs plain.\n\nAPI key: set PERLA_OPENAI_API_KEY (or OPENAI_API_KEY), or put it in perla-voice.toml.");
                return Ok(());
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }
    let config = Config::load(config_path.as_deref())?;

    // Outside herdr but herdr is here: install ourselves into the pinned
    // pane of the "Perla" workspace and attach the herdr UI instead.
    if !no_herdr && !perla_herdr::inside_herdr() && perla_herdr::herdr_available() {
        match bootstrap_into_herdr(&config, config_path.as_deref()).await {
            Ok(()) => return Ok(()), // exec'd into herdr (or printed guidance)
            Err(e) => {
                eprintln!("herdr bootstrap failed ({e:#}); running standalone.");
            }
        }
    }
    let detail = config.detail_mode;
    let (engine, mut events) = Engine::start(config);

    // Keyboard → channel (blocking crossterm poll off the async runtime).
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<TermEvent>();
    std::thread::spawn(move || loop {
        if crossterm::event::poll(Duration::from_millis(50)).unwrap_or(false) {
            if let Ok(ev) = crossterm::event::read() {
                if key_tx.send(ev).is_err() {
                    break;
                }
            }
        }
    });

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut app = App::new(detail);
    let mut presence = Presence::new();
    presence.sync(&app); // register in the herdr sidebar immediately
    let mut redraw = tokio::time::interval(Duration::from_millis(80));
    let result: Result<()> = loop {
        tokio::select! {
            Some(event) = events.recv() => {
                app.apply(event);
                presence.sync(&app);
            }
            Some(term) = key_rx.recv() => {
                if let TermEvent::Key(key) = term {
                    if key.kind == KeyEventKind::Press {
                        handle_key(&mut app, &engine, key.code, key.modifiers);
                    }
                }
            }
            _ = redraw.tick() => {
                if let Err(e) = terminal.draw(|f| draw(f, &app)) {
                    break Err(e.into());
                }
            }
        }
        if app.quitting {
            break Ok(());
        }
    };

    engine.send(EngineCommand::Stop);
    presence.release().await; // no ghost Perla in the sidebar
    tokio::time::sleep(Duration::from_millis(200)).await; // let recap persist
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    result
}

fn handle_key(app: &mut App, engine: &Engine, code: KeyCode, modifiers: KeyModifiers) {
    // Text-input mode captures everything except Esc/Enter.
    if let Some(buffer) = &mut app.input {
        match code {
            KeyCode::Esc => app.input = None,
            KeyCode::Enter => {
                let text = app.input.take().unwrap_or_default();
                if !text.trim().is_empty() {
                    engine.send(EngineCommand::SendText(text));
                }
            }
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char(c) => buffer.push(c),
            _ => {}
        }
        return;
    }
    match (code, modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Char('q'), _) => {
            app.quitting = true;
        }
        (KeyCode::Char('s'), _) => engine.send(EngineCommand::Start),
        (KeyCode::Char('e'), _) => engine.send(EngineCommand::Stop),
        (KeyCode::Char('m'), _) | (KeyCode::Char(' '), _) => engine.send(EngineCommand::ToggleMute),
        (KeyCode::Char('t'), _) => app.input = Some(String::new()),
        (KeyCode::Char('u'), _) => engine.send(EngineCommand::DeliverHeldUpdates),
        (KeyCode::Char('d'), _) => {
            app.detail_mode = !app.detail_mode;
            engine.send(EngineCommand::SetDetailMode {
                on: app.detail_mode,
                big_moments_only: false,
            });
        }
        _ => {}
    }
}

fn draw(frame: &mut ratatui::Frame, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // status bar
            Constraint::Min(5),    // transcript
            Constraint::Length(3), // agents / activity
            Constraint::Length(3), // input / help
        ])
        .split(frame.area());

    draw_status(frame, rows[0], app);
    draw_transcript(frame, rows[1], app);
    draw_agents(frame, rows[2], app);
    draw_footer(frame, rows[3], app);
}

fn draw_status(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let (label, color) = match (app.status, app.reconnecting) {
        (Status::Disconnected, _) => ("disconnected", Color::DarkGray),
        (Status::Connecting, true) => ("reconnecting…", Color::Yellow),
        (Status::Connecting, false) => match app.phase {
            Some(ConnectingPhase::Ready) => ("connecting (ready)", Color::Yellow),
            _ => ("connecting…", Color::Yellow),
        },
        (Status::Connected, _) => ("connected", Color::Green),
        (Status::ToolRunning, _) => ("tool running", Color::Cyan),
        (Status::Error, _) => ("error", Color::Red),
    };
    let speaker = match app.speaker {
        Speaker::Idle => Span::styled("· idle", Style::default().fg(Color::DarkGray)),
        Speaker::User => Span::styled("● you", Style::default().fg(Color::Blue)),
        Speaker::Model => Span::styled("● perla", Style::default().fg(Color::Magenta)),
    };
    let mut spans = vec![
        Span::styled(
            format!(" {label} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" | "),
        speaker,
        Span::raw(" | "),
        Span::styled(
            if app.muted { "muted" } else { "mic live" },
            Style::default().fg(if app.muted { Color::Red } else { Color::Green }),
        ),
        Span::raw(" | "),
        Span::raw(format!("${:.2}", app.cost_usd)),
    ];
    if app.held_updates > 0 {
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(
            format!("{} update(s) held — press u", app.held_updates),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(error) = &app.error {
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(error.clone(), Style::default().fg(Color::Red)));
    }

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(22)])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" perla-voice "),
        ),
        cols[0],
    );
    frame.render_widget(
        Gauge::default()
            .block(Block::default().borders(Borders::ALL).title(" mic "))
            .gauge_style(Style::default().fg(if app.muted {
                Color::DarkGray
            } else {
                Color::Green
            }))
            .ratio(f64::from(app.mic_level.clamp(0.0, 1.0)))
            .label(""),
        cols[1],
    );
}

fn draw_transcript(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let visible = area.height.saturating_sub(2) as usize;
    let items: Vec<ListItem> = app
        .transcript
        .iter()
        .rev()
        .take(visible.max(1))
        .rev()
        .map(|(role, text)| {
            let (prefix, style) = match role {
                Role::User => ("you  ", Style::default().fg(Color::Blue)),
                Role::Assistant => ("perla", Style::default().fg(Color::Magenta)),
                Role::Tool => ("tool ", Style::default().fg(Color::DarkGray)),
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{prefix} "), style.add_modifier(Modifier::BOLD)),
                Span::raw(text.clone()),
            ]))
        })
        .collect();
    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title(" transcript ")),
        area,
    );
}

fn draw_agents(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let mut line: Vec<Span> = Vec::new();
    if let Some(activity) = &app.activity {
        line.push(Span::styled(
            format!("⚙ {activity}  "),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if app.agents.is_empty() && app.activity.is_none() {
        line.push(Span::styled(
            "no agents working",
            Style::default().fg(Color::DarkGray),
        ));
    }
    for (tool, cwd, _) in &app.agents {
        let folder = cwd.rsplit('/').next().unwrap_or(cwd);
        line.push(Span::styled(
            format!("▶ {tool} in {folder}  "),
            Style::default().fg(Color::Green),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(line))
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title(" agents ")),
        area,
    );
}

fn draw_footer(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let content = match &app.input {
        Some(buffer) => Line::from(vec![
            Span::styled("task> ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw(buffer.clone()),
            Span::styled("▏", Style::default().fg(Color::Yellow)),
        ]),
        None => Line::from(Span::styled(
            format!(
                " s start · e end · m/space mute · t type task · u held updates · d detail ({}) · q quit",
                if app.detail_mode { "on" } else { "off" }
            ),
            Style::default().fg(Color::DarkGray),
        )),
    };
    frame.render_widget(
        Paragraph::new(content).block(Block::default().borders(Borders::ALL)),
        area,
    );
}
